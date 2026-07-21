//! Shared session-event helpers for the turn-execution workflows.
//!
//! Root session turns (`turn_execution`) and worker turns
//! (`worker_turn_execution`) persist the same durable events through identical
//! wiring. These helpers are the single definition of that wiring so both
//! workflows emit bit-identical events, fields, and tracing.

use std::time::Instant;

use moa_core::{
    events::Event, types::completion::ToolCallContent, types::completion::ToolInvocation,
    types::events_stream::EventRecord, types::identifiers::SessionId,
    types::identifiers::ToolCallId, types::provider::ModelTier, types::session::SessionMeta,
    types::tools::ToolOutput,
};
use moa_observability::restate_observability::event_persist_span;
use moa_observability::{record_session_error, record_turn_event_persist_duration};
use moa_wire::session_store::{
    AppendEventRequest, RecordSegmentSkillUseRequest, RecordSegmentToolUseRequest,
};
use moa_wire::turn::TurnOutcomeKind;
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::services::session_store::RestateSessionStoreClient;
use crate::workflows::turn_responsiveness::ToolBudgetExhausted;

/// Appends one durable session event and returns its stored record.
///
/// Wraps the append in the standard persistence span and latency counters so
/// every turn-event write is measured identically across both workflows.
pub(super) async fn append_session_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<EventRecord, HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    moa_core::coordination_counters::record_durable_append();
    let record = crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event,
                dedupe_key: None,
            })),
    )
    .call()
    .instrument(persist_span)
    .await?
    .into_inner();
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(record)
}

/// Persists a `ToolCall` event for a model-issued tool invocation.
pub(super) async fn append_tool_call_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_session_event(
        ctx,
        session_id,
        Event::ToolCall {
            tool_id,
            provider_tool_use_id: invocation.id,
            provider_thought_signature: tool_call
                .provider_metadata
                .as_ref()
                .and_then(|metadata| metadata.thought_signature())
                .map(str::to_string),
            tool_name: invocation.name,
            input: invocation.input,
            hand_id: None,
        },
    )
    .await
    .map(|_| ())
}

/// Persists a `ToolResult` event for a completed tool invocation.
pub(super) async fn append_tool_result_event(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    output: &ToolOutput,
) -> Result<(), HandlerError> {
    append_session_event(
        ctx,
        session_id,
        Event::ToolResult {
            tool_id,
            provider_tool_use_id: invocation.id.clone(),
            output: output.clone(),
            original_output_tokens: output.original_output_tokens,
            success: !output.is_error,
            duration_ms: 0,
        },
    )
    .await
    .map(|_| ())
}

/// Records that the current segment used a tool, without blocking the turn.
pub(super) async fn record_segment_tool_use(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_name: &str,
) -> Result<(), HandlerError> {
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .record_segment_tool_use(Json(RecordSegmentToolUseRequest {
                session_id,
                tool_name: tool_name.to_string(),
            })),
    )
    .send();
    Ok(())
}

/// Records the skills a tool call engaged on the current segment, without blocking the turn.
///
/// Detection is a deterministic match of the tool call's input against the turn's
/// selected skill names ([`moa_core::types::skill_use::skills_used_in_tool_call`]),
/// so only skills the model actually engaged are credited, distinct from the full
/// set of injected (activated) skills.
pub(super) async fn record_segment_skill_use_for_tool_call(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_name: &str,
    input: &serde_json::Value,
    selected_skills: &[String],
) -> Result<(), HandlerError> {
    let engaged =
        moa_core::types::skill_use::skills_used_in_tool_call(tool_name, input, selected_skills);
    for skill_name in engaged {
        crate::restate_identity::replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .record_segment_skill_use(Json(RecordSegmentSkillUseRequest {
                    session_id,
                    skill_name,
                })),
        )
        .send();
    }
    Ok(())
}

/// Emits the recoverable error event that records a tool-budget exhaustion stop.
pub(super) async fn emit_tool_budget_exceeded(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    exhaustion: &ToolBudgetExhausted,
) -> Result<(), HandlerError> {
    record_session_error("tool_budget");
    append_session_event(
        ctx,
        session_id,
        Event::Error {
            message: exhaustion.audit_message(),
            recoverable: true,
        },
    )
    .await
    .map(|_| ())
}

/// Persists a zero-cost auxiliary assistant response and returns its text.
///
/// Used for canned replies (clarifications, budget-stop notices) that never
/// call the model, so all token and cost fields are zero.
pub(super) async fn append_zero_cost_assistant_response(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    text: String,
) -> Result<String, HandlerError> {
    append_zero_cost_assistant_response_with_sequence(ctx, session_id, meta, text)
        .await
        .map(|(text, _sequence_num)| text)
}

/// Persists a zero-cost auxiliary assistant response and returns its text plus sequence number.
///
/// Root turn execution uses the sequence number to bound post-outcome segment assessment.
pub(super) async fn append_zero_cost_assistant_response_with_sequence(
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    text: String,
) -> Result<(String, u64), HandlerError> {
    let record = append_session_event(
        ctx,
        session_id,
        Event::BrainResponse {
            text: text.clone(),
            thought_signature: None,
            model: meta.model.clone(),
            model_tier: ModelTier::Auxiliary,
            input_tokens_uncached: 0,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 0,
            output_tokens: 0,
            cost_cents: 0,
            duration_ms: 0,
            llm_ttft_ms: None,
        },
    )
    .await?;
    Ok((text, record.sequence_num))
}

/// Maps a turn outcome kind to its stable label for tracing and metrics.
pub(super) fn turn_outcome_kind_label(kind: &TurnOutcomeKind) -> &'static str {
    match kind {
        TurnOutcomeKind::Completed => "completed",
        TurnOutcomeKind::Accepted { .. } => "accepted",
        TurnOutcomeKind::Cancelled => "cancelled",
        TurnOutcomeKind::Failed => "failed",
    }
}
