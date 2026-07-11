//! Restate-side observability helpers shared by orchestrator handlers.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use moa_core::{
    coordination_counters::CoordinationSnapshot, session_replay::TurnReplaySnapshot,
    types::identifiers::SessionId, types::observability::TraceContext, types::session::SessionMeta,
};
use opentelemetry::trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::runtime_metrics::{TurnLatencyStep, record_turn_step_duration};
use crate::trace_context::apply_trace_context_to_span;
use crate::turn_latency::{TurnLatencySnapshot, current_turn_root_span};

/// Annotates the current tracing span with the Restate service and handler names.
pub fn annotate_restate_handler_span(service: &str, handler: &str) {
    let span = tracing::Span::current();
    span.set_attribute("restate.service", service.to_string());
    span.set_attribute("restate.handler", handler.to_string());
}

/// Returns the OpenTelemetry trace ID for the current tracing span when one is active.
#[must_use]
pub fn current_trace_id() -> Option<String> {
    trace_id_for_span(&tracing::Span::current())
}

/// Returns the OpenTelemetry trace ID for a tracing span when it has a sampled context.
#[must_use]
pub fn trace_id_for_span(span: &tracing::Span) -> Option<String> {
    let trace_id = span.context().span().span_context().trace_id();
    let value = trace_id.to_string();
    if value.chars().all(|character| character == '0') {
        None
    } else {
        Some(value)
    }
}

/// Applies stable session, tenant, and contact tracing attributes to the provided span.
pub fn apply_session_trace(
    span: &tracing::Span,
    meta: &SessionMeta,
    prompt: Option<&str>,
    environment: Option<&str>,
) {
    let trace_context = TraceContext::from_session_meta(meta, prompt)
        .with_environment(environment.map(str::to_string));
    apply_trace_context_to_span(&trace_context, span);
}

/// Adds a deterministic session-root link so all turns can be grouped by session in Tempo.
pub fn add_session_trace_link(span: &tracing::Span, session_id: SessionId) {
    span.add_link(synthetic_session_span_context(session_id));
}

/// Returns whether raw/hashed prompt tracing is enabled for turn spans.
///
/// Disabled by default so turn spans carry neither the raw prompt as their name (unbounded
/// cardinality) nor prompt text as an attribute (PII). Set `MOA_TRACE_PROMPT_SAMPLE=1` (or
/// `true`) to opt in for local debugging; the value is read once per process.
fn prompt_trace_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("MOA_TRACE_PROMPT_SAMPLE")
            .map(|value| matches!(value.trim(), "1" | "true" | "TRUE"))
            .unwrap_or(false)
    })
}

/// Returns a bounded, non-reversible hash of a prompt for span correlation.
fn prompt_hash(prompt: &str) -> String {
    let mut hasher = DefaultHasher::new();
    prompt.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Root span for one brain turn with the standard per-turn trace attributes.
pub fn session_turn_span(
    meta: &SessionMeta,
    prompt: Option<&str>,
    turn_number: i64,
    environment: Option<&str>,
) -> tracing::Span {
    session_turn_span_inner(
        meta,
        prompt,
        turn_number,
        environment,
        prompt_trace_debug_enabled(),
    )
}

fn session_turn_span_inner(
    meta: &SessionMeta,
    prompt: Option<&str>,
    turn_number: i64,
    environment: Option<&str>,
    prompt_debug: bool,
) -> tracing::Span {
    let span = tracing::info_span!(
        "session_turn",
        // Static, low-cardinality span name. The prompt is never used as the span name (PII +
        // unbounded cardinality); a bounded hash is attached below only when prompt debug is on.
        otel.name = %format!("MOA turn {turn_number}"),
        moa.session.id = %meta.id,
        moa.worker.id = tracing::field::Empty,
        moa.tenant.id = %meta.tenant_id,
        moa.contact.id = tracing::field::Empty,
        moa.model = %meta.model,
        moa.turn.number = turn_number,
        moa.turn.prompt_hash = tracing::field::Empty,
        moa.turn.get_events_calls = tracing::field::Empty,
        moa.turn.events_replayed = tracing::field::Empty,
        moa.turn.events_bytes = tracing::field::Empty,
        moa.turn.get_events_total_ms = tracing::field::Empty,
        moa.turn.vo_session_calls = tracing::field::Empty,
        moa.turn.vo_worker_calls = tracing::field::Empty,
        moa.turn.vo_sends = tracing::field::Empty,
        moa.turn.durable_appends = tracing::field::Empty,
        moa.turn.snapshot_load_ms = tracing::field::Empty,
        moa.turn.snapshot_hit = tracing::field::Empty,
        moa.turn.snapshot_write_ms = tracing::field::Empty,
        moa.turn.pipeline_compile_ms = tracing::field::Empty,
        moa.turn.llm_call_ms = tracing::field::Empty,
        moa.turn.tool_dispatch_ms = tracing::field::Empty,
        moa.turn.event_persist_ms = tracing::field::Empty,
        moa.turn.llm_ttft_ms = tracing::field::Empty,
        moa.turn.compaction_tier1 = tracing::field::Empty,
        moa.turn.compaction_tier2 = tracing::field::Empty,
        moa.turn.compaction_tier3 = tracing::field::Empty,
        moa.turn.compaction_tokens_reclaimed = tracing::field::Empty,
        moa.turn.compaction_messages_elided = tracing::field::Empty,
    );
    // Keep the raw prompt out of trace metadata by default; only forward it (via the bounded
    // `moa.trace.name` attribute) and attach the prompt hash when the operator opts in.
    let traced_prompt = if prompt_debug { prompt } else { None };
    apply_session_trace(&span, meta, traced_prompt, environment);
    add_session_trace_link(&span, meta.id);
    if prompt_debug && let Some(prompt) = prompt {
        span.record("moa.turn.prompt_hash", prompt_hash(prompt));
    }
    span
}

/// Root span for one worker turn iteration.
pub fn worker_turn_span(
    meta: &SessionMeta,
    worker_id: &str,
    turn_id: &str,
    turn_number: i64,
    environment: Option<&str>,
) -> tracing::Span {
    let span = session_turn_span(meta, None, turn_number, environment);
    span.record("moa.worker.id", worker_id);
    span.set_attribute(
        "otel.name",
        format!("MOA worker {worker_id} turn {turn_number}"),
    );
    span.set_attribute("moa.turn.scope", "worker");
    span.set_attribute("moa.turn.id", turn_id.to_string());
    span
}

/// Child span around one provider completion call.
pub fn llm_call_span(meta: &SessionMeta) -> tracing::Span {
    match current_turn_root_span() {
        Some(parent) => tracing::info_span!(
            parent: &parent,
            "llm_call",
            otel.kind = "client",
            gen_ai.operation.name = "chat",
            gen_ai.request.model = %meta.model,
            moa.session.id = %meta.id,
            moa.tenant.id = %meta.tenant_id,
        ),
        None => tracing::info_span!(
            "llm_call",
            otel.kind = "client",
            gen_ai.operation.name = "chat",
            gen_ai.request.model = %meta.model,
            moa.session.id = %meta.id,
            moa.tenant.id = %meta.tenant_id,
        ),
    }
}

/// Child span around one tool execution or worker dispatch.
pub fn tool_dispatch_span(tool_name: &str) -> tracing::Span {
    match current_turn_root_span() {
        Some(parent) => tracing::info_span!(
            parent: &parent,
            "tool_dispatch",
            moa.tool.name = tool_name,
        ),
        None => tracing::info_span!("tool_dispatch", moa.tool.name = tool_name),
    }
}

/// Child span around one event persistence batch.
pub fn event_persist_span(events_written: usize) -> tracing::Span {
    match current_turn_root_span() {
        Some(parent) => tracing::info_span!(
            parent: &parent,
            "event_persist",
            moa.persist.events_written = events_written as i64,
        ),
        None => tracing::info_span!(
            "event_persist",
            moa.persist.events_written = events_written as i64,
        ),
    }
}

/// Emits the shared per-turn replay summary event and mirrors the values onto the turn span.
pub fn emit_turn_replay_summary(
    turn_root_span: &tracing::Span,
    turn_number: i64,
    snapshot: &TurnReplaySnapshot,
) {
    turn_root_span.record(
        "moa.turn.get_events_calls",
        snapshot.get_events_calls as i64,
    );
    turn_root_span.record("moa.turn.events_replayed", snapshot.events_replayed as i64);
    turn_root_span.record("moa.turn.events_bytes", snapshot.events_bytes as i64);
    turn_root_span.record(
        "moa.turn.get_events_total_ms",
        snapshot.get_events_total_ms() as i64,
    );
    turn_root_span.record(
        "moa.turn.pipeline_compile_ms",
        snapshot.pipeline_compile_ms() as i64,
    );

    tracing::info!(
        parent: turn_root_span,
        turn_number,
        get_events_calls = snapshot.get_events_calls,
        events_replayed = snapshot.events_replayed,
        events_bytes = snapshot.events_bytes,
        get_events_total_ms = snapshot.get_events_total_ms(),
        pipeline_compile_ms = snapshot.pipeline_compile_ms(),
        "turn event replay summary"
    );
}

/// Emits the shared per-turn coordination summary event and mirrors the values onto the turn span.
///
/// Counts the durable virtual-object round-trips (Session/Worker `.call()`s, fire-and-forget
/// `.send()`s, and durable event appends) made during one turn so fan-in and delegation
/// optimizations have a directly observable target.
pub fn emit_turn_coordination_summary(
    turn_root_span: &tracing::Span,
    snapshot: &CoordinationSnapshot,
) {
    turn_root_span.record(
        "moa.turn.vo_session_calls",
        snapshot.session_vo_calls as i64,
    );
    turn_root_span.record("moa.turn.vo_worker_calls", snapshot.worker_vo_calls as i64);
    turn_root_span.record("moa.turn.vo_sends", snapshot.vo_sends as i64);
    turn_root_span.record("moa.turn.durable_appends", snapshot.durable_appends as i64);

    tracing::info!(
        parent: turn_root_span,
        vo_session_calls = snapshot.session_vo_calls,
        vo_worker_calls = snapshot.worker_vo_calls,
        vo_sends = snapshot.vo_sends,
        durable_appends = snapshot.durable_appends,
        total_vo_calls = snapshot.total_vo_calls(),
        "turn coordination summary"
    );
}

/// Emits the shared per-turn latency summary event and mirrors the values onto the turn span.
pub fn emit_turn_latency_summary(
    turn_root_span: &tracing::Span,
    turn_number: i64,
    snapshot: &TurnLatencySnapshot,
) {
    record_turn_step_duration(
        TurnLatencyStep::SnapshotLoad,
        snapshot.snapshot_load_duration,
    );
    record_turn_step_duration(
        TurnLatencyStep::SnapshotWrite,
        snapshot.snapshot_write_duration,
    );
    record_turn_step_duration(
        TurnLatencyStep::PipelineCompile,
        snapshot.pipeline_compile_duration,
    );
    record_turn_step_duration(TurnLatencyStep::LlmCall, snapshot.llm_call_duration);
    record_turn_step_duration(
        TurnLatencyStep::ToolDispatch,
        snapshot.tool_dispatch_duration,
    );
    record_turn_step_duration(
        TurnLatencyStep::EventPersist,
        snapshot.event_persist_duration,
    );
    if let Some(ttft) = snapshot.llm_ttft {
        record_turn_step_duration(TurnLatencyStep::LlmTtft, ttft);
    }

    turn_root_span.record(
        "moa.turn.snapshot_load_ms",
        snapshot.snapshot_load_ms() as i64,
    );
    turn_root_span.record("moa.turn.snapshot_hit", snapshot.snapshot_hit);
    turn_root_span.record(
        "moa.turn.snapshot_write_ms",
        snapshot.snapshot_write_ms() as i64,
    );
    turn_root_span.record(
        "moa.turn.pipeline_compile_ms",
        snapshot.pipeline_compile_ms() as i64,
    );
    turn_root_span.record("moa.turn.llm_call_ms", snapshot.llm_call_ms() as i64);
    turn_root_span.record(
        "moa.turn.tool_dispatch_ms",
        snapshot.tool_dispatch_ms() as i64,
    );
    turn_root_span.record(
        "moa.turn.event_persist_ms",
        snapshot.event_persist_ms() as i64,
    );
    turn_root_span.record("moa.turn.compaction_tier1", snapshot.compaction_tier1);
    turn_root_span.record("moa.turn.compaction_tier2", snapshot.compaction_tier2);
    turn_root_span.record("moa.turn.compaction_tier3", snapshot.compaction_tier3);
    turn_root_span.record(
        "moa.turn.compaction_tokens_reclaimed",
        snapshot.compaction_tokens_reclaimed as i64,
    );
    turn_root_span.record(
        "moa.turn.compaction_messages_elided",
        snapshot.compaction_messages_elided as i64,
    );
    if let Some(ttft_ms) = snapshot.llm_ttft_ms() {
        turn_root_span.record("moa.turn.llm_ttft_ms", ttft_ms as i64);
    }

    tracing::info!(
        parent: turn_root_span,
        turn_number,
        snapshot_load_ms = snapshot.snapshot_load_ms(),
        snapshot_hit = snapshot.snapshot_hit,
        snapshot_write_ms = snapshot.snapshot_write_ms(),
        pipeline_compile_ms = snapshot.pipeline_compile_ms(),
        llm_call_ms = snapshot.llm_call_ms(),
        tool_dispatch_ms = snapshot.tool_dispatch_ms(),
        event_persist_ms = snapshot.event_persist_ms(),
        compaction_tier1 = snapshot.compaction_tier1,
        compaction_tier2 = snapshot.compaction_tier2,
        compaction_tier3 = snapshot.compaction_tier3,
        compaction_tokens_reclaimed = snapshot.compaction_tokens_reclaimed,
        compaction_messages_elided = snapshot.compaction_messages_elided,
        llm_ttft_ms = snapshot.llm_ttft_ms().unwrap_or_default(),
        "turn latency breakdown"
    );
}

fn synthetic_session_span_context(session_id: SessionId) -> SpanContext {
    let mut left = DefaultHasher::new();
    "moa.session.synthetic_trace.left".hash(&mut left);
    session_id.hash(&mut left);
    let left = left.finish();

    let mut right = DefaultHasher::new();
    "moa.session.synthetic_trace.right".hash(&mut right);
    session_id.hash(&mut right);
    let right = right.finish();

    let mut trace_id_bytes = [0_u8; 16];
    trace_id_bytes[..8].copy_from_slice(&left.to_be_bytes());
    trace_id_bytes[8..].copy_from_slice(&right.to_be_bytes());
    SpanContext::new(
        TraceId::from_bytes(trace_id_bytes),
        SpanId::INVALID,
        TraceFlags::SAMPLED,
        false,
        TraceState::default(),
    )
}

#[cfg(test)]
mod tests {
    use moa_core::{
        types::channel::Channel, types::identifiers::ModelId, types::identifiers::SessionId,
        types::identifiers::TenantId, types::session::SessionMeta,
    };
    use opentelemetry::Value;
    use opentelemetry::trace::SpanKind;

    use super::*;
    use crate::test_capture::{attr_string, capture_spans, find_span};

    fn test_meta() -> SessionMeta {
        SessionMeta {
            id: SessionId::new(),
            tenant_id: TenantId::new(),
            channel: Channel::Slack,
            model: ModelId::new("claude-sonnet-4-20250514"),
            ..SessionMeta::default()
        }
    }

    #[test]
    fn session_turn_span_uses_static_name_and_keeps_prompt_out_by_default() {
        // Pins: the per-turn root span name is static (never the prompt), the raw prompt is not
        // exported as an attribute by default, and the session/tenant/model/turn attributes the
        // orchestrator dashboards key on are still present.
        let meta = test_meta();
        let spans = capture_spans(|| {
            let span = session_turn_span_inner(
                &meta,
                Some("Fix the OAuth bug"),
                7,
                Some("production"),
                false,
            );
            span.in_scope(|| {});
        });

        let span = find_span(&spans, "MOA turn 7");
        assert_eq!(
            attr_string(span, "moa.trace.name"),
            None,
            "raw prompt must not be exported by default"
        );
        assert_eq!(attr_string(span, "moa.turn.prompt_hash"), None);
        let session_id = meta.id.to_string();
        let tenant_id = meta.tenant_id.to_string();
        assert_eq!(
            attr_string(span, "moa.session.id").as_deref(),
            Some(session_id.as_str())
        );
        assert_eq!(
            attr_string(span, "moa.tenant.id").as_deref(),
            Some(tenant_id.as_str())
        );
        assert_eq!(
            attr_string(span, "moa.model").as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert_eq!(attr_string(span, "moa.turn.number").as_deref(), Some("7"));
        assert_eq!(attr_string(span, "moa.channel").as_deref(), Some("slack"));
        assert_eq!(
            attr_string(span, "moa.environment").as_deref(),
            Some("production")
        );
    }

    #[test]
    fn session_turn_span_prompt_debug_attaches_bounded_hash_not_span_name() {
        // Pins: with prompt debug enabled the span name stays static and the prompt surfaces only
        // as a bounded hash attribute plus the moa.trace.name sample, never as the span name.
        let meta = test_meta();
        let spans = capture_spans(|| {
            let span = session_turn_span_inner(&meta, Some("Fix the OAuth bug"), 7, None, true);
            span.in_scope(|| {});
        });

        let span = find_span(&spans, "MOA turn 7");
        let hash = attr_string(span, "moa.turn.prompt_hash").expect("hash attribute present");
        assert_eq!(
            hash.len(),
            16,
            "prompt hash is a bounded 16-hex-char digest"
        );
        assert_ne!(hash, "Fix the OAuth bug");
        assert_eq!(
            attr_string(span, "moa.trace.name").as_deref(),
            Some("Fix the OAuth bug")
        );
    }

    #[test]
    fn llm_call_span_is_client_kind_with_genai_attributes() {
        // Pins: provider completion spans carry the GenAI semantic-convention attributes
        // and the OTel client kind so backends classify them as outbound model calls.
        let meta = test_meta();
        let spans = capture_spans(|| {
            let span = llm_call_span(&meta);
            span.in_scope(|| {});
        });

        let span = find_span(&spans, "llm_call");
        assert_eq!(span.span_kind, SpanKind::Client);
        assert_eq!(
            attr_string(span, "gen_ai.operation.name").as_deref(),
            Some("chat")
        );
        let model = meta.model.to_string();
        assert_eq!(
            attr_string(span, "gen_ai.request.model").as_deref(),
            Some(model.as_str())
        );
        let session_id = meta.id.to_string();
        assert_eq!(
            attr_string(span, "moa.session.id").as_deref(),
            Some(session_id.as_str())
        );
    }

    #[test]
    fn tool_dispatch_span_records_tool_name() {
        // Pins: tool dispatch spans carry the dispatched tool name.
        let spans = capture_spans(|| {
            let span = tool_dispatch_span("bash");
            span.in_scope(|| {});
        });

        let span = find_span(&spans, "tool_dispatch");
        assert_eq!(attr_string(span, "moa.tool.name").as_deref(), Some("bash"));
    }

    #[test]
    fn event_persist_span_records_events_written_as_integer() {
        // Pins: event persistence spans carry the written-event count as an i64 attribute.
        let spans = capture_spans(|| {
            let span = event_persist_span(3);
            span.in_scope(|| {});
        });

        let span = find_span(&spans, "event_persist");
        let value = span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "moa.persist.events_written")
            .map(|kv| kv.value.clone());
        assert_eq!(value, Some(Value::I64(3)));
    }

    #[test]
    fn worker_turn_span_tags_scope_and_identifiers() {
        // Pins: worker turns reuse the session-turn root but tag the worker id, turn
        // id, and `worker` scope. `set_attribute("otel.name", ..)` adds an attribute and
        // does NOT rename the span, so the exported name stays the session-turn name.
        let meta = test_meta();
        let spans = capture_spans(|| {
            let span = worker_turn_span(&meta, "agent-7", "turn-42", 2, Some("staging"));
            span.in_scope(|| {});
        });

        let span = find_span(&spans, "MOA turn 2");
        assert_eq!(
            attr_string(span, "moa.worker.id").as_deref(),
            Some("agent-7")
        );
        assert_eq!(
            attr_string(span, "moa.turn.scope").as_deref(),
            Some("worker")
        );
        assert_eq!(attr_string(span, "moa.turn.id").as_deref(), Some("turn-42"));
        assert_eq!(
            attr_string(span, "otel.name").as_deref(),
            Some("MOA worker agent-7 turn 2")
        );
    }
}
