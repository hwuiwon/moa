//! Per-fixture OTLP/HTTP protobuf trace collector.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use opentelemetry_proto::tonic::collector::trace::v1::{
    ExportTraceServiceRequest, ExportTraceServiceResponse,
};
use opentelemetry_proto::tonic::common::v1::{AnyValue, KeyValue, any_value};
use opentelemetry_proto::tonic::trace::v1::{ResourceSpans, Span};
use prost::Message;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

const DEFAULT_SPAN_CAPACITY: usize = 8_192;
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

#[derive(Default)]
struct CaptureStore {
    spans: Mutex<VecDeque<CapturedOtlpSpan>>,
    notify: Notify,
    capacity: usize,
}

impl CaptureStore {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            spans: Mutex::new(VecDeque::with_capacity(capacity)),
            notify: Notify::new(),
            capacity,
        }
    }

    async fn push_batch(&self, spans: impl IntoIterator<Item = CapturedOtlpSpan>) {
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
        Self::start_with_capacity(resource_name, DEFAULT_SPAN_CAPACITY).await
    }

    async fn start_with_capacity(resource_name: String, capacity: usize) -> Result<Self> {
        if capacity == 0 {
            bail!("OTLP capture span capacity must be positive");
        }
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind fixture OTLP collector")?;
        let address = listener
            .local_addr()
            .context("read fixture OTLP collector address")?;
        let endpoint = format!("http://{address}/v1/traces");
        let store = Arc::new(CaptureStore::with_capacity(capacity));
        let app = Router::new()
            .route("/v1/traces", post(export_traces))
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

    /// Returns the exact OTLP traces endpoint injected into fixture children.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the unique fixture service-resource name.
    #[must_use]
    pub fn resource_name(&self) -> &str {
        &self.resource_name
    }

    /// Clears every currently captured span without replacing the collector.
    pub async fn clear(&self) {
        self.store.spans.lock().await.clear();
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
                bail!("timed out waiting for matching OTLP span");
            }
            tokio::time::timeout(remaining, notified)
                .await
                .map_err(|_| anyhow!("timed out waiting for matching OTLP span"))?;
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
    store.push_batch(spans).await;

    let body = ExportTraceServiceResponse {
        partial_success: None,
    }
    .encode_to_vec();
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
    use opentelemetry_proto::tonic::resource::v1::Resource;
    use opentelemetry_proto::tonic::trace::v1::{ScopeSpans, span};

    use super::*;

    const TRACE_ID: [u8; 16] = [0x11; 16];
    const PARENT_SPAN_ID: [u8; 8] = [0x22; 8];
    const CHILD_SPAN_ID: [u8; 8] = [0x33; 8];
    const LINK_SPAN_ID: [u8; 8] = [0x44; 8];

    #[tokio::test]
    async fn otlp_capture_survives_producer_restart_queries_parent_and_links_and_is_bounded() {
        // Pins: one fixture collector accepts real protobuf from two child-shaped
        // producer lifetimes, retains one query surface, exposes parent/link IDs,
        // bounds storage, clears explicitly, and closes its listener on teardown.
        let mut capture = OtlpCapture::start_with_capacity("fixture-resource".to_string(), 2)
            .await
            .expect("start OTLP capture");
        let endpoint = capture.endpoint().to_string();

        post_batch(
            &endpoint,
            batch(span_with_ids(
                "producer-before-restart",
                PARENT_SPAN_ID,
                None,
                Vec::new(),
            )),
        )
        .await;
        drop(reqwest::Client::new());
        post_batch(
            &endpoint,
            batch(span_with_ids(
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
            Some("fixture-resource")
        );
        assert_eq!(after.scope_name(), "fixture-producer");
        assert_eq!(after.scope_version(), "1.0");

        post_batch(
            &endpoint,
            batch(span_with_ids(
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
            .post(&endpoint)
            .body(Vec::new())
            .send()
            .await
            .expect_err("collector listener should close after teardown");
        assert!(
            error.is_connect(),
            "unexpected teardown request error: {error}"
        );
    }

    fn batch(span: Span) -> ExportTraceServiceRequest {
        ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![string_attribute("service.name", "fixture-resource")],
                    dropped_attributes_count: 0,
                    entity_refs: Vec::new(),
                }),
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

    async fn post_batch(endpoint: &str, request: ExportTraceServiceRequest) {
        let response = reqwest::Client::new()
            .post(endpoint)
            .header(header::CONTENT_TYPE, "application/x-protobuf")
            .body(request.encode_to_vec())
            .send()
            .await
            .expect("post OTLP protobuf batch");
        assert_eq!(response.status(), StatusCode::OK);
        let response =
            ExportTraceServiceResponse::decode(response.bytes().await.expect("read OTLP response"))
                .expect("decode OTLP response");
        assert_eq!(response.partial_success, None);
    }
}
