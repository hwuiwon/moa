//! Per-fixture OTLP/HTTP protobuf collector for traces, metrics, and logs.
//!
//! The collector serves the three signal paths a real OTLP/HTTP receiver serves,
//! `/v1/traces`, `/v1/metrics`, and `/v1/logs`, and [`OtlpCapture::endpoint`] hands out the
//! collector BASE URL rather than either of them. That mirrors production
//! exactly: `MOA_OBSERVABILITY_OTLP_ENDPOINT` names a collector, and each captured
//! signal derives its own
//! path from it, and a value that already names one signal is refused at config
//! load. A fixture that handed out `.../v1/traces` would be configuring the
//! child with a value production rejects, and could never observe a metric at
//! all.
//!
//! Every request to any other path is recorded instead of being silently
//! dropped, because the failure this fixture exists to catch — an exporter
//! posting a signal to the wrong path — is otherwise indistinguishable from an
//! exporter that never ran.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opentelemetry_proto::tonic::collector::logs::v1::{
    ExportLogsServiceRequest, ExportLogsServiceResponse,
};
use opentelemetry_proto::tonic::collector::metrics::v1::{
    ExportMetricsServiceRequest, ExportMetricsServiceResponse,
};
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::metrics::v1::{ResourceMetrics, metric, number_data_point};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, Span};
use prost::Message;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const DEFAULT_SIGNAL_CAPACITY: usize = 8_192;
const RUN_UID_ATTRIBUTE: &str = "moa.execution.run_uid";

/// One exported OTLP span link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedOtlpLink {
    trace_id: String,
    span_id: String,
}

impl CapturedOtlpLink {
    /// Returns the linked trace ID as 32 lowercase hexadecimal characters.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Returns the linked span ID as 16 lowercase hexadecimal characters.
    #[must_use]
    pub fn span_id(&self) -> &str {
        &self.span_id
    }
}

/// One span decoded from an OTLP/HTTP protobuf export batch.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedOtlpSpan {
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    name: String,
    attributes: BTreeMap<String, String>,
    resource_attributes: BTreeMap<String, String>,
    scope_name: String,
    scope_version: String,
    links: Vec<CapturedOtlpLink>,
}

impl CapturedOtlpSpan {
    /// Returns the span trace ID as 32 lowercase hexadecimal characters.
    #[must_use]
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Returns the span ID as 16 lowercase hexadecimal characters.
    #[must_use]
    pub fn span_id(&self) -> &str {
        &self.span_id
    }

    /// Returns the parent span ID, or `None` for a root span.
    #[must_use]
    pub fn parent_span_id(&self) -> Option<&str> {
        self.parent_span_id.as_deref()
    }

    /// Returns the exported span name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns one string-rendered span attribute.
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    /// Returns one string-rendered resource attribute.
    #[must_use]
    pub fn resource_attribute(&self, key: &str) -> Option<&str> {
        self.resource_attributes.get(key).map(String::as_str)
    }

    /// Returns every string-rendered resource attribute.
    #[must_use]
    pub fn resource_attributes(&self) -> &BTreeMap<String, String> {
        &self.resource_attributes
    }

    /// Returns the instrumentation scope name.
    #[must_use]
    pub fn scope_name(&self) -> &str {
        &self.scope_name
    }

    /// Returns the instrumentation scope version.
    #[must_use]
    pub fn scope_version(&self) -> &str {
        &self.scope_version
    }

    /// Returns every exported causal link.
    #[must_use]
    pub fn links(&self) -> &[CapturedOtlpLink] {
        &self.links
    }
}

/// One data point decoded from an exported OTLP metric.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedOtlpDataPoint {
    attributes: BTreeMap<String, String>,
    value: f64,
    count: u64,
    explicit_bounds: Vec<f64>,
}

impl CapturedOtlpDataPoint {
    /// Returns one string-rendered data-point attribute.
    #[must_use]
    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.attributes.get(key).map(String::as_str)
    }

    /// Returns the point value: the number for sums and gauges, the sum for histograms.
    #[must_use]
    pub fn value(&self) -> f64 {
        self.value
    }

    /// Returns the recorded observation count: always 1 for sums and gauges.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Returns the exported histogram bucket boundaries, empty for sums and gauges.
    ///
    /// Boundaries are exported data, not an implementation detail: the OTLP
    /// bridge installs explicit views so latency histograms keep their sub-10ms
    /// buckets, and an exporter that silently fell back to the SDK default
    /// layout would still produce a metric with a plausible value.
    #[must_use]
    pub fn explicit_bounds(&self) -> &[f64] {
        &self.explicit_bounds
    }
}

/// One metric decoded from an OTLP/HTTP protobuf export batch.
#[derive(Clone, Debug, PartialEq)]
pub struct CapturedOtlpMetric {
    name: String,
    unit: String,
    resource_attributes: BTreeMap<String, String>,
    scope_name: String,
    data_points: Vec<CapturedOtlpDataPoint>,
}

impl CapturedOtlpMetric {
    /// Returns the exported metric name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the exported metric unit, empty when the instrument declares none.
    #[must_use]
    pub fn unit(&self) -> &str {
        &self.unit
    }

    /// Returns one string-rendered resource attribute.
    #[must_use]
    pub fn resource_attribute(&self, key: &str) -> Option<&str> {
        self.resource_attributes.get(key).map(String::as_str)
    }

    /// Returns every string-rendered resource attribute.
    #[must_use]
    pub fn resource_attributes(&self) -> &BTreeMap<String, String> {
        &self.resource_attributes
    }

    /// Returns the instrumentation scope name.
    #[must_use]
    pub fn scope_name(&self) -> &str {
        &self.scope_name
    }

    /// Returns every decoded data point.
    #[must_use]
    pub fn data_points(&self) -> &[CapturedOtlpDataPoint] {
        &self.data_points
    }
}

#[derive(Default)]
struct CaptureStore {
    spans: Mutex<VecDeque<CapturedOtlpSpan>>,
    metrics: Mutex<VecDeque<CapturedOtlpMetric>>,
    unexpected_requests: Mutex<Vec<String>>,
    notify: Notify,
    capacity: usize,
}

impl CaptureStore {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            spans: Mutex::new(VecDeque::with_capacity(capacity)),
            metrics: Mutex::new(VecDeque::with_capacity(capacity)),
            unexpected_requests: Mutex::new(Vec::new()),
            notify: Notify::new(),
            capacity,
        }
    }

    async fn push_spans(&self, spans: impl IntoIterator<Item = CapturedOtlpSpan>) {
        let mut stored = self.spans.lock().await;
        for span in spans {
            if stored.len() == self.capacity {
                stored.pop_front();
            }
            stored.push_back(span);
        }
        drop(stored);
        self.notify.notify_waiters();
    }

    async fn push_metrics(&self, metrics: impl IntoIterator<Item = CapturedOtlpMetric>) {
        let mut stored = self.metrics.lock().await;
        for metric in metrics {
            if stored.len() == self.capacity {
                stored.pop_front();
            }
            stored.push_back(metric);
        }
        drop(stored);
        self.notify.notify_waiters();
    }

    async fn push_unexpected_request(&self, description: String) {
        self.unexpected_requests.lock().await.push(description);
        self.notify.notify_waiters();
    }

    /// Renders what the collector has actually received, for failure messages.
    ///
    /// A timeout waiting for a span or a metric has several causes that look
    /// identical from the outside — nothing exported, the wrong signal exported,
    /// or an export posted to a path the collector does not serve. Printing the
    /// observed names and the misdirected paths separates them in one run.
    async fn observed_summary(&self) -> String {
        let span_names = self
            .spans
            .lock()
            .await
            .iter()
            .map(|span| span.name.clone())
            .collect::<Vec<_>>();
        let metric_names = self
            .metrics
            .lock()
            .await
            .iter()
            .map(|metric| metric.name.clone())
            .collect::<Vec<_>>();
        let unexpected = self.unexpected_requests.lock().await.clone();
        format!(
            "observed {} span(s) {span_names:?}, {} metric(s) {metric_names:?}, \
             {} request(s) to unserved paths {unexpected:?}",
            span_names.len(),
            metric_names.len(),
            unexpected.len(),
        )
    }
}

/// Fixture-owned OTLP collector and bounded query surface.
pub struct OtlpCapture {
    endpoint: String,
    resource_name: String,
    store: Arc<CaptureStore>,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
}

impl OtlpCapture {
    /// Starts a collector on one ephemeral loopback port.
    pub async fn start(resource_name: String) -> Result<Self> {
        Self::start_with_capacity(resource_name, DEFAULT_SIGNAL_CAPACITY).await
    }

    async fn start_with_capacity(resource_name: String, capacity: usize) -> Result<Self> {
        if capacity == 0 {
            bail!("OTLP capture signal capacity must be positive");
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind fixture OTLP collector")?;
        let address = listener
            .local_addr()
            .context("read fixture OTLP collector address")?;
        let endpoint = format!("http://{address}");
        let store = Arc::new(CaptureStore::with_capacity(capacity));
        let app = Router::new()
            .route("/v1/traces", post(export_traces))
            .route("/v1/metrics", post(export_metrics))
            .route("/v1/logs", post(export_logs))
            .fallback(record_unserved_request)
            .with_state(store.clone());
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });
        Ok(Self {
            endpoint,
            resource_name,
            store,
            shutdown: Some(shutdown_tx),
            task: Some(task),
        })
    }

    /// Returns the collector BASE URL injected into fixture children.
    ///
    /// This is deliberately not a signal endpoint. `MOA_OBSERVABILITY_OTLP_ENDPOINT`
    /// is the collector base and config load refuses a value already naming
    /// `/v1/traces`, `/v1/metrics` or `/v1/logs`, so handing out a signal path
    /// here would configure the child with a value production rejects.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the exact URL this collector serves OTLP traces on.
    #[must_use]
    pub fn traces_endpoint(&self) -> String {
        format!("{}/v1/traces", self.endpoint)
    }

    /// Returns the exact URL this collector serves OTLP metrics on.
    #[must_use]
    pub fn metrics_endpoint(&self) -> String {
        format!("{}/v1/metrics", self.endpoint)
    }

    /// Returns the exact URL this collector serves OTLP logs on.
    #[must_use]
    pub fn logs_endpoint(&self) -> String {
        format!("{}/v1/logs", self.endpoint)
    }

    /// Returns the unique fixture service-resource name.
    #[must_use]
    pub fn resource_name(&self) -> &str {
        &self.resource_name
    }

    /// Clears every captured signal without replacing the collector.
    pub async fn clear(&self) {
        self.store.spans.lock().await.clear();
        self.store.metrics.lock().await.clear();
        self.store.unexpected_requests.lock().await.clear();
    }

    /// Returns captured spans for one exact trace ID in export order.
    pub async fn spans_for_trace(&self, trace_id: &str) -> Vec<CapturedOtlpSpan> {
        self.store
            .spans
            .lock()
            .await
            .iter()
            .filter(|span| span.trace_id == trace_id)
            .cloned()
            .collect()
    }

    /// Returns the newest captured span carrying one execution run UID.
    pub async fn span_by_run_uid(&self, run_uid: &str) -> Option<CapturedOtlpSpan> {
        self.store
            .spans
            .lock()
            .await
            .iter()
            .rev()
            .find(|span| span.attribute(RUN_UID_ATTRIBUTE) == Some(run_uid))
            .cloned()
    }

    /// Returns every captured export of one exact metric name, in export order.
    pub async fn metrics_named(&self, name: &str) -> Vec<CapturedOtlpMetric> {
        self.store
            .metrics
            .lock()
            .await
            .iter()
            .filter(|metric| metric.name == name)
            .cloned()
            .collect()
    }

    /// Returns every request this collector received on a path it does not serve.
    pub async fn unexpected_requests(&self) -> Vec<String> {
        self.store.unexpected_requests.lock().await.clone()
    }

    /// Waits until one captured span matches `predicate`.
    pub async fn wait_for_span<F>(
        &self,
        timeout: Duration,
        predicate: F,
    ) -> Result<CapturedOtlpSpan>
    where
        F: Fn(&CapturedOtlpSpan) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.store.notify.notified();
            if let Some(span) = self
                .store
                .spans
                .lock()
                .await
                .iter()
                .rev()
                .find(|span| predicate(span))
                .cloned()
            {
                return Ok(span);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for matching OTLP span; {}",
                    self.store.observed_summary().await
                );
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                bail!(
                    "timed out waiting for matching OTLP span; {}",
                    self.store.observed_summary().await
                );
            }
        }
    }

    /// Waits until one captured metric matches `predicate`.
    pub async fn wait_for_metric<F>(
        &self,
        timeout: Duration,
        predicate: F,
    ) -> Result<CapturedOtlpMetric>
    where
        F: Fn(&CapturedOtlpMetric) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            let notified = self.store.notify.notified();
            if let Some(metric) = self
                .store
                .metrics
                .lock()
                .await
                .iter()
                .rev()
                .find(|metric| predicate(metric))
                .cloned()
            {
                return Ok(metric);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "timed out waiting for matching OTLP metric; {}",
                    self.store.observed_summary().await
                );
            }
            if tokio::time::timeout(remaining, notified).await.is_err() {
                bail!(
                    "timed out waiting for matching OTLP metric; {}",
                    self.store.observed_summary().await
                );
            }
        }
    }

    /// Gracefully stops the collector and waits for its listener task.
    pub async fn shutdown(&mut self) -> Result<()> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.await
                .context("join fixture OTLP collector")?
                .context("serve fixture OTLP collector")?;
        }
        Ok(())
    }

    pub(super) fn stop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for OtlpCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn export_traces(State(store): State<Arc<CaptureStore>>, body: Bytes) -> Response {
    let request = match ExportTraceServiceRequest::decode(body) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid OTLP trace protobuf: {error}"),
            )
                .into_response();
        }
    };
    let spans = request
        .resource_spans
        .into_iter()
        .flat_map(captured_resource_spans)
        .collect::<Vec<_>>();
    store.push_spans(spans).await;

    protobuf_response(
        ExportTraceServiceResponse {
            partial_success: None,
        }
        .encode_to_vec(),
    )
}

async fn export_metrics(State(store): State<Arc<CaptureStore>>, body: Bytes) -> Response {
    let request = match ExportMetricsServiceRequest::decode(body) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("invalid OTLP metric protobuf: {error}"),
            )
                .into_response();
        }
    };
    let metrics = request
        .resource_metrics
        .into_iter()
        .flat_map(captured_resource_metrics)
        .collect::<Vec<_>>();
    store.push_metrics(metrics).await;

    protobuf_response(
        ExportMetricsServiceResponse {
            partial_success: None,
        }
        .encode_to_vec(),
    )
}

async fn export_logs(body: Bytes) -> Response {
    if let Err(error) = ExportLogsServiceRequest::decode(body) {
        return (
            StatusCode::BAD_REQUEST,
            format!("invalid OTLP log protobuf: {error}"),
        )
            .into_response();
    }

    protobuf_response(
        ExportLogsServiceResponse {
            partial_success: None,
        }
        .encode_to_vec(),
    )
}

/// Records, rather than discards, an export posted to an unserved path.
async fn record_unserved_request(
    State(store): State<Arc<CaptureStore>>,
    request: Request,
) -> Response {
    let description = format!("{} {}", request.method(), request.uri().path());
    store.push_unexpected_request(description).await;
    (
        StatusCode::NOT_FOUND,
        "fixture OTLP collector serves only /v1/traces, /v1/metrics, and /v1/logs",
    )
        .into_response()
}

fn protobuf_response(body: Vec<u8>) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-protobuf"),
    );
    response
}

fn captured_resource_spans(resource_spans: ResourceSpans) -> Vec<CapturedOtlpSpan> {
    let resource_attributes = resource_spans
        .resource
        .map(|resource| captured_attributes(&resource.attributes))
        .unwrap_or_default();
    resource_spans
        .scope_spans
        .into_iter()
        .flat_map(|scope_spans| {
            let (scope_name, scope_version) = scope_spans
                .scope
                .map(|scope| (scope.name, scope.version))
                .unwrap_or_default();
            let resource_attributes = resource_attributes.clone();
            scope_spans.spans.into_iter().map(move |span| {
                captured_span(
                    span,
                    resource_attributes.clone(),
                    scope_name.clone(),
                    scope_version.clone(),
                )
            })
        })
        .collect()
}

fn captured_resource_metrics(resource_metrics: ResourceMetrics) -> Vec<CapturedOtlpMetric> {
    let resource_attributes = resource_metrics
        .resource
        .map(|resource| captured_attributes(&resource.attributes))
        .unwrap_or_default();
    resource_metrics
        .scope_metrics
        .into_iter()
        .flat_map(|scope_metrics| {
            let scope_name = scope_metrics
                .scope
                .map(|scope| scope.name)
                .unwrap_or_default();
            let resource_attributes = resource_attributes.clone();
            scope_metrics
                .metrics
                .into_iter()
                .map(move |metric| CapturedOtlpMetric {
                    name: metric.name,
                    unit: metric.unit,
                    resource_attributes: resource_attributes.clone(),
                    scope_name: scope_name.clone(),
                    data_points: captured_data_points(metric.data.as_ref()),
                })
        })
        .collect()
}

fn captured_data_points(data: Option<&metric::Data>) -> Vec<CapturedOtlpDataPoint> {
    match data {
        Some(metric::Data::Sum(sum)) => sum.data_points.iter().map(captured_number_point).collect(),
        Some(metric::Data::Gauge(gauge)) => gauge
            .data_points
            .iter()
            .map(captured_number_point)
            .collect(),
        Some(metric::Data::Histogram(histogram)) => histogram
            .data_points
            .iter()
            .map(|point| CapturedOtlpDataPoint {
                attributes: captured_attributes(&point.attributes),
                value: point.sum.unwrap_or_default(),
                count: point.count,
                explicit_bounds: point.explicit_bounds.clone(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn captured_number_point(
    point: &opentelemetry_proto::tonic::metrics::v1::NumberDataPoint,
) -> CapturedOtlpDataPoint {
    let value = match point.value {
        Some(number_data_point::Value::AsDouble(value)) => value,
        #[allow(clippy::cast_precision_loss)]
        Some(number_data_point::Value::AsInt(value)) => value as f64,
        None => 0.0,
    };
    CapturedOtlpDataPoint {
        attributes: captured_attributes(&point.attributes),
        value,
        count: 1,
        explicit_bounds: Vec::new(),
    }
}

fn captured_span(
    span: Span,
    resource_attributes: BTreeMap<String, String>,
    scope_name: String,
    scope_version: String,
) -> CapturedOtlpSpan {
    CapturedOtlpSpan {
        trace_id: lowercase_hex(&span.trace_id),
        span_id: lowercase_hex(&span.span_id),
        parent_span_id: (!span.parent_span_id.is_empty())
            .then(|| lowercase_hex(&span.parent_span_id)),
        name: span.name,
        attributes: captured_attributes(&span.attributes),
        resource_attributes,
        scope_name,
        scope_version,
        links: span
            .links
            .into_iter()
            .map(|link| CapturedOtlpLink {
                trace_id: lowercase_hex(&link.trace_id),
                span_id: lowercase_hex(&link.span_id),
            })
            .collect(),
    }
}

fn captured_attributes(attributes: &[KeyValue]) -> BTreeMap<String, String> {
    attributes
        .iter()
        .filter_map(|attribute| {
            attribute
                .value
                .as_ref()
                .map(|value| (attribute.key.clone(), render_any_value(value)))
        })
        .collect()
}

fn render_any_value(value: &AnyValue) -> String {
    match value.value.as_ref() {
        Some(any_value::Value::StringValue(value)) => value.clone(),
        Some(any_value::Value::BoolValue(value)) => value.to_string(),
        Some(any_value::Value::IntValue(value)) => value.to_string(),
        Some(any_value::Value::DoubleValue(value)) => value.to_string(),
        Some(any_value::Value::BytesValue(value)) => lowercase_hex(value),
        Some(any_value::Value::ArrayValue(value)) => value
            .values
            .iter()
            .map(render_any_value)
            .collect::<Vec<_>>()
            .join(","),
        Some(any_value::Value::KvlistValue(value)) => value
            .values
            .iter()
            .filter_map(|entry| {
                entry
                    .value
                    .as_ref()
                    .map(|value| format!("{}={}", entry.key, render_any_value(value)))
            })
            .collect::<Vec<_>>()
            .join(","),
        None => String::new(),
    }
}

fn lowercase_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut rendered = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        rendered.push(char::from(HEX[usize::from(byte >> 4)]));
        rendered.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    rendered
}

#[cfg(test)]
mod tests {
    use opentelemetry_proto::tonic::common::v1::{InstrumentationScope, KeyValue};
    use opentelemetry_proto::tonic::metrics::v1::{
        AggregationTemporality, Histogram, HistogramDataPoint, Metric, NumberDataPoint,
        ScopeMetrics, Sum,
    };
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{ScopeSpans, span};

    use super::*;

    const TRACE_ID: [u8; 16] = [0x11; 16];
    const PARENT_SPAN_ID: [u8; 8] = [0x22; 8];
    const CHILD_SPAN_ID: [u8; 8] = [0x33; 8];
    const LINK_SPAN_ID: [u8; 8] = [0x44; 8];
    const RESOURCE_NAME: &str = "fixture-resource";

    #[tokio::test]
    async fn otlp_capture_survives_producer_restart_queries_parent_and_links_and_is_bounded() {
        // Pins: one fixture collector accepts real protobuf from two child-shaped
        // producer lifetimes, retains one query surface, exposes parent/link IDs,
        // bounds storage, clears explicitly, and closes its listener on teardown.
        let mut capture = OtlpCapture::start_with_capacity(RESOURCE_NAME.to_string(), 2)
            .await
            .expect("start OTLP capture");
        let traces_endpoint = capture.traces_endpoint();

        post_traces(
            &traces_endpoint,
            trace_batch(span_with_ids(
                "producer-before-restart",
                PARENT_SPAN_ID,
                None,
                Vec::new(),
            )),
        )
        .await;
        drop(reqwest::Client::new());
        post_traces(
            &traces_endpoint,
            trace_batch(span_with_ids(
                "producer-after-restart",
                CHILD_SPAN_ID,
                Some(PARENT_SPAN_ID),
                vec![span::Link {
                    trace_id: TRACE_ID.to_vec(),
                    span_id: LINK_SPAN_ID.to_vec(),
                    trace_state: String::new(),
                    attributes: Vec::new(),
                    dropped_attributes_count: 0,
                    flags: 0,
                }],
            )),
        )
        .await;

        let trace_id = lowercase_hex(&TRACE_ID);
        let spans = capture.spans_for_trace(&trace_id).await;
        assert_eq!(
            spans.iter().map(CapturedOtlpSpan::name).collect::<Vec<_>>(),
            vec!["producer-before-restart", "producer-after-restart"]
        );
        let after = capture
            .wait_for_span(Duration::from_secs(1), |span| {
                span.name() == "producer-after-restart"
            })
            .await
            .expect("wait for restarted producer span");
        assert_eq!(
            capture
                .span_by_run_uid("run-1")
                .await
                .expect("query span by execution run UID")
                .span_id(),
            after.span_id()
        );
        let parent_span_id = lowercase_hex(&PARENT_SPAN_ID);
        assert_eq!(after.parent_span_id(), Some(parent_span_id.as_str()));
        assert_eq!(after.links().len(), 1);
        assert_eq!(after.links()[0].trace_id(), trace_id);
        let link_span_id = lowercase_hex(&LINK_SPAN_ID);
        assert_eq!(after.links()[0].span_id(), link_span_id);
        assert_eq!(
            after.resource_attribute("service.name"),
            Some(RESOURCE_NAME)
        );
        assert_eq!(after.scope_name(), "fixture-producer");
        assert_eq!(after.scope_version(), "1.0");

        post_traces(
            &traces_endpoint,
            trace_batch(span_with_ids(
                "bounded-newest",
                [0x55; 8],
                Some(CHILD_SPAN_ID),
                Vec::new(),
            )),
        )
        .await;
        let spans = capture.spans_for_trace(&trace_id).await;
        assert_eq!(spans.len(), 2, "store must evict its oldest span");
        assert_eq!(spans[0].name(), "producer-after-restart");
        assert_eq!(spans[1].name(), "bounded-newest");

        capture.clear().await;
        assert!(capture.spans_for_trace(&trace_id).await.is_empty());
        capture.shutdown().await.expect("shutdown OTLP capture");
        let error = reqwest::Client::new()
            .post(&traces_endpoint)
            .body(Vec::new())
            .send()
            .await
            .expect_err("collector listener should close after teardown");
        assert!(
            error.is_connect(),
            "unexpected teardown request error: {error}"
        );
    }

    #[tokio::test]
    async fn otlp_capture_decodes_runtime_metrics_alongside_traces_from_one_base_endpoint() {
        // Pins: the collector base URL carries BOTH signals. A child configured
        // with `endpoint()` exports traces to /v1/traces and metrics to
        // /v1/metrics, and both decode into queryable values sharing one resource
        // identity. Trace-only capture is what this replaces: with metrics
        // undecodable, an exporter that never sent a metric and one whose metrics
        // were dropped on the floor were the same observation.
        let capture = OtlpCapture::start(RESOURCE_NAME.to_string())
            .await
            .expect("start OTLP capture");

        // `endpoint()` must be the collector base, exactly as production config
        // requires; deriving the signal paths from it is the child's job.
        let base = capture.endpoint().to_string();
        for signal in ["/v1/traces", "/v1/metrics", "/v1/logs"] {
            assert!(
                !base.ends_with(signal),
                "fixture endpoint `{base}` names the {signal} signal; config load refuses \
                 signal-specific OTLP endpoints, so a child given this value cannot start"
            );
        }
        assert_eq!(capture.traces_endpoint(), format!("{base}/v1/traces"));
        assert_eq!(capture.metrics_endpoint(), format!("{base}/v1/metrics"));

        post_traces(
            &capture.traces_endpoint(),
            trace_batch(span_with_ids(
                "turn.execute",
                CHILD_SPAN_ID,
                None,
                Vec::new(),
            )),
        )
        .await;
        post_metrics(&capture.metrics_endpoint(), metric_batch()).await;

        let span = capture
            .wait_for_span(Duration::from_secs(5), |span| span.name() == "turn.execute")
            .await
            .expect("wait for exported span");
        let counter = capture
            .wait_for_metric(Duration::from_secs(5), |metric| {
                metric.name() == "moa_turns_total"
            })
            .await
            .expect("wait for exported runtime counter");

        assert_eq!(counter.unit(), "1");
        assert_eq!(counter.scope_name(), "moa");
        assert_eq!(counter.data_points().len(), 1);
        assert!(
            (counter.data_points()[0].value() - 7.0).abs() < f64::EPSILON,
            "unexpected counter value: {:?}",
            counter.data_points()[0]
        );
        assert_eq!(counter.data_points()[0].attribute("outcome"), Some("ok"));

        // The resource identity must be byte-identical across signals or no query
        // can join a trace to the metrics describing the same process.
        assert_eq!(
            span.resource_attributes(),
            counter.resource_attributes(),
            "trace and metric resources disagree: {:?} vs {:?}",
            span.resource_attributes(),
            counter.resource_attributes()
        );
        assert_eq!(span.resource_attribute("service.name"), Some(RESOURCE_NAME));

        // Histogram boundaries survive the round trip: the OTLP bridge installs
        // explicit views, and a silent fall back to the SDK default layout still
        // produces a plausible-looking metric.
        let latency = capture.metrics_named("moa_turn_latency_seconds").await;
        assert_eq!(
            latency.len(),
            1,
            "unexpected histogram exports: {latency:?}"
        );
        assert_eq!(latency[0].data_points()[0].count(), 3);
        assert_eq!(
            latency[0].data_points()[0].explicit_bounds(),
            &[0.005, 0.01, 0.025]
        );

        assert!(
            capture.unexpected_requests().await.is_empty(),
            "collector received requests on unserved paths: {:?}",
            capture.unexpected_requests().await
        );

        // Clearing must reset both captured signals, or a fixture reusing the collector
        // across phases reads a previous phase's metric as its own.
        capture.clear().await;
        assert!(capture.metrics_named("moa_turns_total").await.is_empty());
    }

    #[tokio::test]
    async fn otlp_capture_accepts_logs_from_the_shared_base_endpoint() {
        // Pins: the production collector base URL carries logs alongside traces
        // and metrics, so the fixture must accept the derived /v1/logs endpoint.
        let capture = OtlpCapture::start(RESOURCE_NAME.to_string())
            .await
            .expect("start OTLP capture");

        post_logs(
            &capture.logs_endpoint(),
            ExportLogsServiceRequest {
                resource_logs: Vec::new(),
            },
        )
        .await;

        assert!(
            capture.unexpected_requests().await.is_empty(),
            "valid OTLP logs must not be reported as an unserved request"
        );
    }

    #[tokio::test]
    async fn otlp_capture_records_an_export_posted_to_an_unserved_path() {
        // Pins: a misdirected export is recorded and reported, not silently
        // dropped. An exporter that posts traces to the collector root produces
        // exactly the same "no matching span" timeout as an exporter that never
        // ran, and this is the observation that separates them.
        let capture = OtlpCapture::start(RESOURCE_NAME.to_string())
            .await
            .expect("start OTLP capture");

        let response = reqwest::Client::new()
            .post(capture.endpoint())
            .header(header::CONTENT_TYPE, "application/x-protobuf")
            .body(
                trace_batch(span_with_ids(
                    "misdirected",
                    CHILD_SPAN_ID,
                    None,
                    Vec::new(),
                ))
                .encode_to_vec(),
            )
            .send()
            .await
            .expect("post to the collector root");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        assert_eq!(capture.unexpected_requests().await, vec!["POST /"]);
        let error = capture
            .wait_for_span(Duration::from_millis(200), |span| {
                span.name() == "misdirected"
            })
            .await
            .expect_err("a span posted to an unserved path must not be captured");
        let rendered = format!("{error}");
        assert!(
            rendered.contains("unserved paths [\"POST /\"]"),
            "timeout message must name the misdirected request; got: {rendered}"
        );
    }

    fn trace_batch(span: Span) -> ExportTraceServiceRequest {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(fixture_resource()),
                scope_spans: vec![ScopeSpans {
                    scope: Some(InstrumentationScope {
                        name: "fixture-producer".to_string(),
                        version: "1.0".to_string(),
                        attributes: Vec::new(),
                        dropped_attributes_count: 0,
                    }),
                    spans: vec![span],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
    }

    fn metric_batch() -> ExportMetricsServiceRequest {
        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(fixture_resource()),
                scope_metrics: vec![ScopeMetrics {
                    scope: Some(InstrumentationScope {
                        name: "moa".to_string(),
                        version: String::new(),
                        attributes: Vec::new(),
                        dropped_attributes_count: 0,
                    }),
                    metrics: vec![
                        Metric {
                            name: "moa_turns_total".to_string(),
                            description: "turns started".to_string(),
                            unit: "1".to_string(),
                            metadata: Vec::new(),
                            data: Some(metric::Data::Sum(Sum {
                                data_points: vec![NumberDataPoint {
                                    attributes: vec![string_attribute("outcome", "ok")],
                                    start_time_unix_nano: 1,
                                    time_unix_nano: 2,
                                    exemplars: Vec::new(),
                                    flags: 0,
                                    value: Some(number_data_point::Value::AsInt(7)),
                                }],
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                                is_monotonic: true,
                            })),
                        },
                        Metric {
                            name: "moa_turn_latency_seconds".to_string(),
                            description: "turn latency".to_string(),
                            unit: "s".to_string(),
                            metadata: Vec::new(),
                            data: Some(metric::Data::Histogram(Histogram {
                                data_points: vec![HistogramDataPoint {
                                    attributes: Vec::new(),
                                    start_time_unix_nano: 1,
                                    time_unix_nano: 2,
                                    count: 3,
                                    sum: Some(0.06),
                                    bucket_counts: vec![1, 1, 1, 0],
                                    explicit_bounds: vec![0.005, 0.01, 0.025],
                                    exemplars: Vec::new(),
                                    flags: 0,
                                    min: Some(0.001),
                                    max: Some(0.05),
                                }],
                                aggregation_temporality: AggregationTemporality::Cumulative as i32,
                            })),
                        },
                    ],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        }
    }

    fn fixture_resource() -> Resource {
        Resource {
            attributes: vec![
                string_attribute("service.name", RESOURCE_NAME),
                string_attribute("deployment.environment", "fixture"),
                string_attribute("service.version", "0.0.0-fixture"),
            ],
            dropped_attributes_count: 0,
            entity_refs: Vec::new(),
        }
    }

    fn span_with_ids(
        name: &str,
        span_id: [u8; 8],
        parent_span_id: Option<[u8; 8]>,
        links: Vec<span::Link>,
    ) -> Span {
        Span {
            trace_id: TRACE_ID.to_vec(),
            span_id: span_id.to_vec(),
            trace_state: String::new(),
            parent_span_id: parent_span_id.map_or_else(Vec::new, |id| id.to_vec()),
            flags: 1,
            name: name.to_string(),
            kind: 1,
            start_time_unix_nano: 1,
            end_time_unix_nano: 2,
            attributes: vec![string_attribute(RUN_UID_ATTRIBUTE, "run-1")],
            dropped_attributes_count: 0,
            events: Vec::new(),
            dropped_events_count: 0,
            links,
            dropped_links_count: 0,
            status: None,
        }
    }

    fn string_attribute(key: &str, value: &str) -> KeyValue {
        KeyValue {
            key: key.to_string(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue(value.to_string())),
            }),
        }
    }

    async fn post_traces(endpoint: &str, request: ExportTraceServiceRequest) {
        let body = post_protobuf(endpoint, request.encode_to_vec()).await;
        let response =
            ExportTraceServiceResponse::decode(body).expect("decode OTLP trace response");
        assert_eq!(response.partial_success, None);
    }

    async fn post_metrics(endpoint: &str, request: ExportMetricsServiceRequest) {
        let body = post_protobuf(endpoint, request.encode_to_vec()).await;
        let response =
            ExportMetricsServiceResponse::decode(body).expect("decode OTLP metric response");
        assert_eq!(response.partial_success, None);
    }

    async fn post_logs(endpoint: &str, request: ExportLogsServiceRequest) {
        let body = post_protobuf(endpoint, request.encode_to_vec()).await;
        let response = ExportLogsServiceResponse::decode(body).expect("decode OTLP log response");
        assert_eq!(response.partial_success, None);
    }

    async fn post_protobuf(endpoint: &str, body: Vec<u8>) -> Bytes {
        let response = reqwest::Client::new()
            .post(endpoint)
            .header(header::CONTENT_TYPE, "application/x-protobuf")
            .body(body)
            .send()
            .await
            .expect("post OTLP protobuf batch");
        assert_eq!(response.status(), StatusCode::OK, "endpoint {endpoint}");
        response.bytes().await.expect("read OTLP response")
    }
}
