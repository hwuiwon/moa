//! In-memory OpenTelemetry span capture helpers shared by crate tests.
//!
//! Tests run span-emitting code under a thread-local `tracing` subscriber backed
//! by a real OpenTelemetry tracer whose finished spans are buffered in memory.
//! This exercises the production export path, so assertions can inspect the exact
//! span name, kind, and attributes a collector would receive — including
//! attributes written through `OpenTelemetrySpanExt::set_attribute` and
//! `tracing::Span::record`, which a plain `tracing` layer never observes.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::{
    InMemorySpanExporter, Sampler, SdkTracerProvider, SimpleSpanProcessor, SpanData,
};
use tracing_subscriber::layer::SubscriberExt;

/// Runs `emit` under an in-memory OpenTelemetry subscriber and returns every finished span.
///
/// Spans created inside `emit` must be entered (e.g. via [`tracing::Span::in_scope`])
/// and dropped before the closure returns so the layer starts and ends them; the
/// returned [`SpanData`] then carries their exported name, kind, and attributes.
pub(crate) fn capture_spans<F>(emit: F) -> Vec<SpanData>
where
    F: FnOnce(),
{
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_sampler(Sampler::AlwaysOn)
        .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
        .build();
    let tracer = provider.tracer("moa-observability-test");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = tracing_subscriber::registry().with(layer);

    tracing::subscriber::with_default(subscriber, emit);

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
