//! Restate-side observability helpers shared by orchestrator handlers.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use moa_core::{SessionId, SessionMeta, TraceContext, TurnLatencySnapshot, TurnReplaySnapshot};
use opentelemetry::trace::{SpanContext, SpanId, TraceFlags, TraceId, TraceState};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Annotates the current tracing span with the Restate service and handler names.
pub(crate) fn annotate_restate_handler_span(service: &str, handler: &str) {
    let span = tracing::Span::current();
    span.set_attribute("restate.service", service.to_string());
    span.set_attribute("restate.handler", handler.to_string());
}

/// Applies stable session/user/workspace tracing attributes to the provided span.
pub(crate) fn apply_session_trace(
    span: &tracing::Span,
    meta: &SessionMeta,
    prompt: Option<&str>,
    environment: Option<&str>,
) {
    TraceContext::from_session_meta(meta, prompt)
        .with_environment(environment.map(str::to_string))
        .apply_to_span(span);
}

/// Adds a deterministic session-root link so all turns can be grouped by session in Tempo.
pub(crate) fn add_session_trace_link(span: &tracing::Span, session_id: SessionId) {
    span.add_link(synthetic_session_span_context(session_id));
}

/// Emits the shared per-turn replay summary event and mirrors the values onto the turn span.
pub(crate) fn emit_turn_replay_summary(
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

/// Emits the shared per-turn latency summary event and mirrors the values onto the turn span.
pub(crate) fn emit_turn_latency_summary(
    turn_root_span: &tracing::Span,
    turn_number: i64,
    snapshot: &TurnLatencySnapshot,
) {
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
