//! W3C trace-context propagation across MOA service boundaries.
//!
//! MOA services are separate processes joined by Restate invocations and plain
//! HTTP calls. `tracing` spans do not cross those boundaries on their own, so a
//! single user turn would otherwise fragment into one disconnected trace per
//! hop (edge, Session VO, TurnExecution, brain, providers). These helpers
//! serialize the active span's OpenTelemetry context into `traceparent` /
//! `tracestate` request headers on the sending side and re-adopt it as the
//! parent of the receiving span, yielding one end-to-end trace per turn.

use std::collections::HashMap;

use opentelemetry::global;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// W3C `traceparent` header name.
pub const TRACEPARENT_HEADER: &str = "traceparent";
/// W3C `tracestate` header name.
pub const TRACESTATE_HEADER: &str = "tracestate";

/// Installs the process-global W3C trace-context propagator.
///
/// The global propagator is a single process-wide slot, so this is effectively
/// idempotent (last writer wins with the same type). Called once during
/// telemetry bootstrap before any inject/extract helper is used.
pub fn init_trace_propagation() {
    global::set_text_map_propagator(TraceContextPropagator::new());
}

/// Serializes a span's OpenTelemetry context into W3C trace-context headers.
///
/// Returns an empty map when no sampled OTel context is active (for example when
/// observability is disabled or the span is unsampled), so callers may inject
/// unconditionally without branching on whether tracing is enabled.
#[must_use]
pub fn trace_headers_for_span(span: &Span) -> HashMap<String, String> {
    let context = span.context();
    let mut carrier = HashMap::new();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&context, &mut carrier);
    });
    carrier
}

/// Serializes the current span's context into W3C trace-context headers.
#[must_use]
pub fn current_trace_headers() -> HashMap<String, String> {
    trace_headers_for_span(&Span::current())
}

/// Adopts a remote W3C trace context as the parent of `span`.
///
/// `get` reads a header value by name from the caller's inbound headers (Restate
/// `HeaderMap`, axum `HeaderMap`, etc.). When `traceparent` resolves to a valid
/// remote span context, `span` is linked beneath it so the local subtree
/// continues the sender's trace. Returns `true` when a parent was adopted; a
/// missing or malformed `traceparent` is a no-op that leaves `span` as its own
/// root.
pub fn adopt_remote_parent<F>(span: &Span, get: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    let Some(traceparent) = get(TRACEPARENT_HEADER) else {
        return false;
    };
    let mut carrier = HashMap::new();
    carrier.insert(TRACEPARENT_HEADER.to_string(), traceparent);
    if let Some(tracestate) = get(TRACESTATE_HEADER) {
        carrier.insert(TRACESTATE_HEADER.to_string(), tracestate);
    }

    let parent = global::get_text_map_propagator(|propagator| propagator.extract(&carrier));
    if parent.span().span_context().is_valid() {
        // set_parent only fails when the OTel layer is absent; nothing to do then.
        let _ = span.set_parent(parent);
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::test_capture::{capture_spans, find_span};

    #[test]
    fn injected_headers_carry_a_traceparent() {
        // Pins: a live span serializes into a W3C `traceparent` header so outbound
        // requests can carry the trace across a service boundary.
        init_trace_propagation();
        let mut headers = HashMap::new();
        capture_spans(|| {
            let span = tracing::info_span!("outbound");
            span.in_scope(|| {
                headers = current_trace_headers();
            });
        });
        assert!(
            headers.contains_key(TRACEPARENT_HEADER),
            "expected a traceparent header, got keys {:?}",
            headers.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn adopt_remote_parent_continues_the_injected_trace() {
        // Pins: headers injected from a parent span, then adopted onto an
        // otherwise-rootless child span, place the child in the SAME trace and
        // directly under the parent span — the core end-to-end continuity claim.
        init_trace_propagation();
        let mut headers = HashMap::new();
        let spans = capture_spans(|| {
            let parent = tracing::info_span!("remote_parent");
            parent.in_scope(|| {
                headers = current_trace_headers();
            });
            // Created at top level so it has no ambient tracing parent; the only
            // parent it can get is the adopted remote context.
            let child = tracing::info_span!("local_child");
            let adopted = adopt_remote_parent(&child, |key| headers.get(key).cloned());
            assert!(adopted, "valid traceparent should be adopted");
            child.in_scope(|| {});
        });

        let parent = find_span(&spans, "remote_parent");
        let child = find_span(&spans, "local_child");
        assert_eq!(
            child.span_context.trace_id(),
            parent.span_context.trace_id(),
            "child must share the parent's trace id"
        );
        assert_eq!(
            child.parent_span_id,
            parent.span_context.span_id(),
            "child must be parented directly to the remote span"
        );
    }

    #[test]
    fn adopt_remote_parent_without_traceparent_is_a_noop() {
        // Pins: absent trace-context headers leave the span as its own root and
        // report no adoption, so uninstrumented callers never crash or mislink.
        init_trace_propagation();
        capture_spans(|| {
            let span = tracing::info_span!("no_parent");
            let adopted = adopt_remote_parent(&span, |_| None);
            assert!(!adopted);
            span.in_scope(|| {});
        });
    }
}
