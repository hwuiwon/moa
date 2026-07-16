//! W3C trace-context propagation across MOA service boundaries.
//!
//! MOA services are separate processes joined by Restate invocations and plain
//! HTTP calls. `tracing` spans do not cross those boundaries on their own, so a
//! single user turn would otherwise fragment into one disconnected trace per
//! hop. These helpers validate, inject, adopt, and link the W3C context used at
//! those boundaries.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use opentelemetry::global;
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use reqwest::RequestBuilder;
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// W3C `traceparent` header name.
pub const TRACEPARENT_HEADER: &str = "traceparent";
/// W3C `tracestate` header name.
pub const TRACESTATE_HEADER: &str = "tracestate";
/// MOA header carrying a separately linked W3C `traceparent` context.
pub const TRACE_LINK_TRACEPARENT_HEADER: &str = "x-moa-trace-link-traceparent";
/// MOA header carrying the `tracestate` paired with the linked context.
pub const TRACE_LINK_TRACESTATE_HEADER: &str = "x-moa-trace-link-tracestate";
/// Maximum stored byte length for one combined `tracestate` value.
pub const TRACESTATE_MAX_BYTES: usize = 512;
const TRACEPARENT_LEN: usize = 55;
const TRACESTATE_MAX_MEMBERS: usize = 32;
const TRACESTATE_KEY_MAX_BYTES: usize = 256;
const TRACESTATE_VALUE_MAX_BYTES: usize = 256;

/// A validated W3C Level 2 trace context safe for persistence and reinjection.
///
/// Invalid `traceparent` values discard the complete pair. A valid parent with
/// invalid `tracestate` retains the parent and drops only the state. Accepted
/// non-empty state is kept byte-for-byte; empty-member placement and OWS are
/// not normalized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedTraceContext {
    traceparent: String,
    tracestate: Option<String>,
}

impl ValidatedTraceContext {
    /// Validates a trace-context pair.
    ///
    /// Returns `None` when `traceparent` is absent or invalid. Invalid state is
    /// represented as `None` on an otherwise valid returned context.
    #[must_use]
    pub fn new(traceparent: Option<&str>, tracestate: Option<&str>) -> Option<Self> {
        let traceparent = traceparent.filter(|value| valid_traceparent(value))?;
        Some(Self {
            traceparent: traceparent.to_string(),
            tracestate: tracestate.and_then(validated_tracestate),
        })
    }

    /// Reads and validates a trace context from a header lookup function.
    #[must_use]
    pub fn from_headers<F>(get: F) -> Option<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let traceparent = get(TRACEPARENT_HEADER);
        let tracestate = get(TRACESTATE_HEADER);
        Self::new(traceparent.as_deref(), tracestate.as_deref())
    }

    /// Returns the byte-exact validated `traceparent`.
    #[must_use]
    pub fn traceparent(&self) -> &str {
        &self.traceparent
    }

    /// Returns the byte-exact validated `tracestate`, when non-empty and valid.
    #[must_use]
    pub fn tracestate(&self) -> Option<&str> {
        self.tracestate.as_deref()
    }

    /// Builds the remote OpenTelemetry span context represented by this pair.
    #[must_use]
    pub fn remote_span_context(&self) -> SpanContext {
        let Ok(trace_id) = TraceId::from_hex(&self.traceparent[3..35]) else {
            return SpanContext::NONE;
        };
        let Ok(span_id) = SpanId::from_hex(&self.traceparent[36..52]) else {
            return SpanContext::NONE;
        };
        let Ok(flags) = u8::from_str_radix(&self.traceparent[53..55], 16) else {
            return SpanContext::NONE;
        };
        let trace_state = self
            .tracestate
            .as_deref()
            .and_then(|value| TraceState::from_str(value).ok())
            .unwrap_or_default();
        SpanContext::new(trace_id, span_id, TraceFlags::new(flags), true, trace_state)
    }

    fn headers(&self) -> impl Iterator<Item = (&'static str, &str)> {
        self.named_headers(TRACEPARENT_HEADER, TRACESTATE_HEADER)
    }

    fn named_headers(
        &self,
        traceparent_header: &'static str,
        tracestate_header: &'static str,
    ) -> impl Iterator<Item = (&'static str, &str)> {
        std::iter::once((traceparent_header, self.traceparent.as_str())).chain(
            self.tracestate
                .as_deref()
                .map(move |state| (tracestate_header, state)),
        )
    }
}

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
/// Returns an empty map when no valid OTel context is active, so callers may
/// inject unconditionally without branching on whether tracing is enabled.
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

/// Adds the current span's trace headers to an arbitrary request builder.
///
/// The callback keeps this helper independent of a concrete transport while
/// still giving generated Restate requests one common approved wrapper.
pub fn with_trace_headers<T, F>(request: T, mut add_header: F) -> T
where
    F: FnMut(T, String, String) -> T,
{
    current_trace_headers()
        .into_iter()
        .fold(request, |request, (name, value)| {
            add_header(request, name, value)
        })
}

/// Adds a separately validated trace context to an arbitrary request builder.
///
/// This is used when a durable outbox reinjects the first stored causal pair
/// rather than the reaper's current span.
pub fn with_validated_trace_headers<T, F>(
    request: T,
    context: Option<&ValidatedTraceContext>,
    mut add_header: F,
) -> T
where
    F: FnMut(T, String, String) -> T,
{
    match context {
        Some(context) => context.headers().fold(request, |request, (name, value)| {
            add_header(request, name.to_string(), value.to_string())
        }),
        None => request,
    }
}

/// Adds the current span's W3C context to a raw HTTP request.
pub fn with_reqwest_trace_headers(request: RequestBuilder) -> RequestBuilder {
    with_trace_headers(request, |request, name, value| request.header(name, value))
}

/// Adds a separately validated W3C context to a raw HTTP request.
pub fn with_reqwest_validated_trace_headers(
    request: RequestBuilder,
    context: Option<&ValidatedTraceContext>,
) -> RequestBuilder {
    with_validated_trace_headers(request, context, |request, name, value| {
        request.header(name, value)
    })
}

/// Adds a separately validated W3C context as MOA span-link headers.
pub fn with_reqwest_validated_trace_link_headers(
    request: RequestBuilder,
    context: Option<&ValidatedTraceContext>,
) -> RequestBuilder {
    match context {
        Some(context) => context
            .named_headers(TRACE_LINK_TRACEPARENT_HEADER, TRACE_LINK_TRACESTATE_HEADER)
            .fold(request, |request, (name, value)| {
                request.header(name, value)
            }),
        None => request,
    }
}

/// Adopts a remote W3C trace context as the parent of `span`.
///
/// Missing or malformed parent context is a no-op. Invalid state is discarded
/// while a separately valid parent is retained.
pub fn adopt_remote_parent<F>(span: &Span, get: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    ValidatedTraceContext::from_headers(get)
        .is_some_and(|context| adopt_validated_parent(span, &context))
}

/// Adopts a validated remote context as the parent of `span`.
pub fn adopt_validated_parent(span: &Span, context: &ValidatedTraceContext) -> bool {
    let parent = context.remote_span_context();
    if !parent.is_valid() {
        return false;
    }
    span.set_parent(opentelemetry::Context::new().with_remote_span_context(parent))
        .is_ok()
}

/// Adds a remote W3C trace context as an explicit link on `span`.
///
/// This is the fan-out/fan-in primitive for causal relationships that are not
/// represented by a direct parent edge.
pub fn link_remote_context<F>(span: &Span, get: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    ValidatedTraceContext::from_headers(get)
        .is_some_and(|context| link_validated_context(span, &context))
}

/// Adds the validated MOA span-link headers as an explicit link on `span`.
pub fn link_remote_context_from_link_headers<F>(span: &Span, get: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    ValidatedTraceContext::new(
        get(TRACE_LINK_TRACEPARENT_HEADER).as_deref(),
        get(TRACE_LINK_TRACESTATE_HEADER).as_deref(),
    )
    .is_some_and(|context| link_validated_context(span, &context))
}

/// Adds a validated remote context as an explicit link on `span`.
pub fn link_validated_context(span: &Span, context: &ValidatedTraceContext) -> bool {
    let linked = context.remote_span_context();
    if !linked.is_valid() {
        return false;
    }
    span.add_link(linked);
    true
}

fn valid_traceparent(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != TRACEPARENT_LEN
        || bytes[0..3] != *b"00-"
        || bytes[35] != b'-'
        || bytes[52] != b'-'
        || !bytes[3..35].iter().copied().all(is_lower_hex)
        || !bytes[36..52].iter().copied().all(is_lower_hex)
        || !bytes[53..55].iter().copied().all(is_lower_hex)
        || bytes[3..35].iter().all(|byte| *byte == b'0')
        || bytes[36..52].iter().all(|byte| *byte == b'0')
    {
        return false;
    }

    let Some(flags) = parse_hex_byte(bytes[53], bytes[54]) else {
        return false;
    };
    flags & 0xfc == 0
}

fn validated_tracestate(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() > TRACESTATE_MAX_BYTES
        || !bytes
            .iter()
            .all(|byte| byte.is_ascii() && (*byte == b'\t' || !byte.is_ascii_control()))
    {
        return None;
    }

    let members = value.split(',').collect::<Vec<_>>();
    if members.len() > TRACESTATE_MAX_MEMBERS {
        return None;
    }

    let mut keys = HashSet::new();
    let mut non_empty_members = 0;
    for member in members {
        let member = member.trim_matches([' ', '\t']);
        if member.is_empty() {
            continue;
        }
        non_empty_members += 1;
        if member
            .as_bytes()
            .iter()
            .filter(|byte| **byte == b'=')
            .count()
            != 1
        {
            return None;
        }
        let (key, state_value) = member.split_once('=')?;
        if !valid_tracestate_key(key) || !valid_tracestate_value(state_value) || !keys.insert(key) {
            return None;
        }
    }

    (non_empty_members > 0).then(|| value.to_string())
}

fn valid_tracestate_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > TRACESTATE_KEY_MAX_BYTES {
        return false;
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase()
            || byte.is_ascii_digit()
            || matches!(byte, b'_' | b'-' | b'*' | b'/' | b'@')
    })
}

fn valid_tracestate_value(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= TRACESTATE_VALUE_MAX_BYTES
        && bytes.last().is_some_and(|byte| *byte != b' ')
        && bytes
            .iter()
            .all(|byte| matches!(byte, 0x20..=0x7e) && !matches!(byte, b',' | b'='))
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || matches!(byte, b'a'..=b'f')
}

fn parse_hex_byte(high: u8, low: u8) -> Option<u8> {
    Some((hex_nibble(high)? << 4) | hex_nibble(low)?)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use opentelemetry::trace::SpanContext;

    use super::*;
    use crate::test_capture::{capture_spans, find_span, single_link_context};

    const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn validated_trace_context_pins_level_two_parent_and_state_grammar() {
        // Pins: the persisted parser accepts exactly Task 11's W3C Level 2
        // traceparent/tracestate contract and preserves valid state bytes.
        for parent in [
            PARENT,
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-02",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-03",
        ] {
            assert!(
                ValidatedTraceContext::new(Some(parent), None).is_some(),
                "valid parent rejected: {parent}"
            );
        }
        for parent in [
            "",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7",
            "01-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-00000000000000000000000000000000-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-0000000000000000-01",
            "00-4BF92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e473g-00f067aa0ba902b7-01",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-04",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-ff",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\r",
        ] {
            assert_eq!(
                ValidatedTraceContext::new(Some(parent), Some("vendor=value")),
                None,
                "invalid parent accepted: {parent:?}"
            );
        }

        for state in [
            "vendor=value",
            "1vendor=value",
            "vendor@one@two=value",
            " vendor=  leading spaces\t",
            "a=value ",
            ",vendor=value",
            "vendor=value,",
            "vendor=value,,other=two",
        ] {
            let context = ValidatedTraceContext::new(Some(PARENT), Some(state))
                .expect("valid parent should survive");
            assert_eq!(
                context.tracestate(),
                Some(state),
                "valid state bytes changed: {state:?}"
            );
        }

        for state in ["", " ", "\t", ",", " , \t"] {
            let context = ValidatedTraceContext::new(Some(PARENT), Some(state))
                .expect("valid parent should survive empty state");
            assert_eq!(
                context.tracestate(),
                None,
                "{state:?} should normalize null"
            );
        }
    }

    #[test]
    fn validated_trace_context_enforces_state_boundaries_and_drops_only_bad_state() {
        // Pins: invalid state never rejects a valid parent, including member,
        // key/value, byte-safety, uniqueness, and local storage-cap failures.
        let exact_key = format!("{}=v", "a".repeat(256));
        let exact_value = format!("a={}", "v".repeat(256));
        let exact_cap = format!("{}={}", "a".repeat(255), "v".repeat(256));
        for state in [&exact_key, &exact_value, &exact_cap] {
            let context = ValidatedTraceContext::new(Some(PARENT), Some(state))
                .expect("valid parent should survive");
            assert_eq!(context.tracestate(), Some(state.as_str()));
        }

        let thirty_two = (0..32)
            .map(|index| format!("k{index}=v"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            ValidatedTraceContext::new(Some(PARENT), Some(&thirty_two))
                .expect("valid parent")
                .tracestate(),
            Some(thirty_two.as_str())
        );

        let thirty_three = (0..33)
            .map(|index| format!("k{index}=v"))
            .collect::<Vec<_>>()
            .join(",");
        let thirty_three_with_empties = format!("{},,", "a=v,".repeat(31));
        for state in [
            format!("{}=v", "a".repeat(257)),
            format!("a={}", "v".repeat(257)),
            format!("{}={}", "a".repeat(256), "v".repeat(256)),
            thirty_three,
            thirty_three_with_empties,
            "a=v,a=other".to_string(),
            "_a=v".to_string(),
            "@a=v".to_string(),
            "A=v".to_string(),
            "a=".to_string(),
            "a=value=other".to_string(),
            "a=value\r\ninjected".to_string(),
            "a=v\u{7f}".to_string(),
            "a=vé".to_string(),
            "a=\tvalue".to_string(),
        ] {
            let context = ValidatedTraceContext::new(Some(PARENT), Some(&state))
                .expect("valid parent should survive invalid state");
            assert_eq!(
                context.traceparent(),
                PARENT,
                "valid parent changed for {state:?}"
            );
            assert_eq!(
                context.tracestate(),
                None,
                "invalid state accepted: {state:?}"
            );
        }
    }

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
    fn request_helpers_inject_current_and_durable_contexts() {
        // Pins: generated-client wrappers and raw HTTP reinjection share the same
        // exact header insertion primitive.
        init_trace_propagation();
        let mut injected = Vec::new();
        capture_spans(|| {
            let span = tracing::info_span!("request_parent");
            span.in_scope(|| {
                injected = with_trace_headers(Vec::new(), |mut request, name, value| {
                    request.push((name, value));
                    request
                });
            });
        });
        assert_eq!(
            injected
                .iter()
                .filter(|(name, _)| name == TRACEPARENT_HEADER)
                .count(),
            1
        );

        let durable =
            ValidatedTraceContext::new(Some(PARENT), Some(" vendor=value,")).expect("context");
        let injected =
            with_validated_trace_headers(Vec::new(), Some(&durable), |mut request, name, value| {
                request.push((name, value));
                request
            });
        assert_eq!(
            injected,
            vec![
                (TRACEPARENT_HEADER.to_string(), PARENT.to_string()),
                (TRACESTATE_HEADER.to_string(), " vendor=value,".to_string()),
            ]
        );

        let linked = durable
            .named_headers(TRACE_LINK_TRACEPARENT_HEADER, TRACE_LINK_TRACESTATE_HEADER)
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<Vec<_>>();
        assert_eq!(
            linked,
            vec![
                (
                    TRACE_LINK_TRACEPARENT_HEADER.to_string(),
                    PARENT.to_string(),
                ),
                (
                    TRACE_LINK_TRACESTATE_HEADER.to_string(),
                    " vendor=value,".to_string(),
                ),
            ]
        );
    }

    #[test]
    fn adopt_remote_parent_and_link_capture_real_context_ids() {
        // Pins: adopted parents and asynchronous links export their real trace,
        // parent, and linked span IDs through the production OTel layer.
        init_trace_propagation();
        let mut headers = HashMap::new();
        let spans = capture_spans(|| {
            let parent = tracing::info_span!("remote_parent");
            parent.in_scope(|| {
                headers = current_trace_headers();
            });

            let child = tracing::info_span!("local_child");
            assert!(
                adopt_remote_parent(&child, |key| headers.get(key).cloned()),
                "valid traceparent should be adopted"
            );
            child.in_scope(|| {});

            let linked = tracing::info_span!("fan_in");
            assert!(
                link_remote_context(&linked, |key| headers.get(key).cloned()),
                "valid traceparent should be linked"
            );
            linked.in_scope(|| {});

            let durable_linked = tracing::info_span!("durable_fan_in");
            assert!(
                link_remote_context_from_link_headers(&durable_linked, |key| {
                    match key {
                        TRACE_LINK_TRACEPARENT_HEADER => headers.get(TRACEPARENT_HEADER).cloned(),
                        TRACE_LINK_TRACESTATE_HEADER => headers.get(TRACESTATE_HEADER).cloned(),
                        _ => None,
                    }
                }),
                "valid durable link headers should be linked"
            );
            durable_linked.in_scope(|| {});
        });

        let parent = find_span(&spans, "remote_parent");
        let child = find_span(&spans, "local_child");
        let linked = find_span(&spans, "fan_in");
        let durable_linked = find_span(&spans, "durable_fan_in");
        assert_eq!(
            child.span_context.trace_id(),
            parent.span_context.trace_id()
        );
        assert_eq!(child.parent_span_id, parent.span_context.span_id());
        assert!(child.parent_span_is_remote);
        let linked_context = single_link_context(linked);
        assert_eq!(linked_context.trace_id(), parent.span_context.trace_id());
        assert_eq!(linked_context.span_id(), parent.span_context.span_id());
        assert!(linked_context.is_remote());
        let durable_linked_context = single_link_context(durable_linked);
        assert_eq!(
            durable_linked_context.trace_id(),
            parent.span_context.trace_id()
        );
        assert_eq!(
            durable_linked_context.span_id(),
            parent.span_context.span_id()
        );
        assert!(durable_linked_context.is_remote());
    }

    #[test]
    fn missing_or_invalid_remote_context_is_a_noop() {
        // Pins: uninstrumented and malformed callers cannot create an invalid
        // parent or link.
        init_trace_propagation();
        capture_spans(|| {
            let span = tracing::info_span!("no_parent");
            assert!(!adopt_remote_parent(&span, |_| None));
            assert!(!link_remote_context(&span, |name| {
                (name == TRACEPARENT_HEADER).then(|| "malformed".to_string())
            }));
            span.in_scope(|| {});
        });
    }

    #[test]
    fn validated_remote_span_context_is_nonzero_and_remote() {
        // Pins: persistence reload constructs the exact trace and span IDs used
        // for adoption/linking without synthesizing placeholders.
        let context = ValidatedTraceContext::new(Some(PARENT), None).expect("valid context");
        let span_context: SpanContext = context.remote_span_context();
        assert_eq!(
            span_context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(span_context.span_id().to_string(), "00f067aa0ba902b7");
        assert!(span_context.is_remote());
        assert!(span_context.is_sampled());
    }
}
