//! In-memory OpenTelemetry span capture helpers shared by `moa-providers`
//! observability tests.
//!
//! Tests run span-emitting code under a thread-local `tracing` subscriber
//! backed by a real OpenTelemetry tracer whose finished spans are buffered in
//! memory. This exercises the production export path, so assertions can
//! inspect the exact span name, kind, and attributes a collector would
//! receive — including attributes written through
//! `OpenTelemetrySpanExt::set_attribute`, which a plain `tracing` layer never
//! observes.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, Sampler, SdkTracerProvider, SimpleSpanProcessor, SpanData,
};
use tracing_subscriber::layer::SubscriberExt;

/// Runs `emit` under an in-memory OpenTelemetry subscriber and returns every finished span.
///
/// Spans created inside `emit` must be entered and dropped before the closure
/// returns so the layer starts and ends them; the returned [`SpanData`] then
/// carries their exported name, kind, and attributes.
pub(crate) fn capture_spans<F>(emit: F) -> Vec<SpanData>
where
    F: FnOnce(),
{
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let tracer = provider.tracer("moa-providers-test");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(layer);

    tracing::subscriber::with_default(subscriber, emit);

    let _ = provider.force_flush();
    exporter
        .get_finished_spans()
        .expect("in-memory exporter should return finished spans")
}

/// Runs `emit` under an in-memory OpenTelemetry subscriber and returns every finished span.
///
/// Async counterpart to [`capture_spans`] for tests that drive a real provider
/// HTTP round trip: the subscriber guard is held across `emit`'s `.await`
/// points, which only compiles for a non-`Send` future, i.e. a `#[tokio::test]`
/// using the default current-thread runtime (never `tokio::spawn`, which
/// requires `Send`).
pub(crate) async fn capture_spans_async<Fut>(emit: Fut) -> Vec<SpanData>
where
    Fut: std::future::Future<Output = ()>,
{
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let tracer = provider.tracer("moa-providers-test");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(layer);

    {
        let _guard = tracing::subscriber::set_default(subscriber);
        emit.await;
    }

    let _ = provider.force_flush();
    exporter
        .get_finished_spans()
        .expect("in-memory exporter should return finished spans")
}

/// Returns the single captured span whose exported name equals `name`.
///
/// Panics when zero or more than one span matches, which keeps each assertion
/// pinned to one unambiguous span.
pub(crate) fn find_span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    let matches = spans
        .iter()
        .filter(|span| span.name.as_ref() == name)
        .collect::<Vec<_>>();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one captured span named `{name}`, found {} (names: {:?})",
        matches.len(),
        spans
            .iter()
            .map(|span| span.name.as_ref())
            .collect::<Vec<_>>(),
    );
    matches[0]
}

/// Returns the string form of the first attribute on `span` keyed by `key`, if present.
pub(crate) fn attr_string(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

/// Returns the `i64` form of the first attribute on `span` keyed by `key`, if present.
pub(crate) fn attr_i64(span: &SpanData, key: &str) -> Option<i64> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .and_then(|kv| match &kv.value {
            opentelemetry::Value::I64(value) => Some(*value),
            _ => None,
        })
}

/// Returns the `f64` form of the first attribute on `span` keyed by `key`, if present.
pub(crate) fn attr_f64(span: &SpanData, key: &str) -> Option<f64> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .and_then(|kv| match &kv.value {
            opentelemetry::Value::F64(value) => Some(*value),
            _ => None,
        })
}
