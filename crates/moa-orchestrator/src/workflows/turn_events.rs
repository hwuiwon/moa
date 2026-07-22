//! Shared session-event helpers for the turn-execution workflows.
//!
//! Root session turns (`turn_execution`) and worker turns
//! (`worker_turn_execution`) persist the same durable events through identical
//! wiring. These helpers are the single definition of that wiring so both
//! workflows emit bit-identical events, fields, and tracing.

use std::time::{Duration, Instant};

use moa_core::{
    events::Event, traits::SessionStore as _, types::completion::ToolCallContent,
    types::completion::ToolInvocation, types::events_stream::EventRecord,
    types::identifiers::SessionId, types::identifiers::ToolCallId, types::provider::ModelTier,
    types::session::SessionMeta, types::tools::ToolOutput,
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
    let dedupe = next_turn_event_identity(ctx).await?;
    let orchestrator = crate::ctx::OrchestratorCtx::current();
    let record = if orchestrator.config().session.direct_turn_event_append {
        let action_started = Instant::now();
        let store = orchestrator.session_store_backend();
        let event_is_error = matches!(&event, Event::Error { .. });
        let event_for_action = event.clone();
        let dedupe_for_action = dedupe.key.clone();
        let action = ctx
            .run(move || {
                let store = store.clone();
                let event = event_for_action.clone();
                let dedupe_key = dedupe_for_action.clone();
                async move {
                    emit_direct_event(&store, session_id, event, dedupe_key)
                        .await
                        .map(Json::from)
                        .map_err(crate::workflows::errors::moa_error_to_handler_error)
                }
            })
            .name(dedupe.action_name)
            .retry_policy(turn_event_append_retry_policy(dedupe.jitter_ms))
            .instrument(persist_span)
            .await?
            .into_inner();
        moa_observability::record_session_event_append_phase_duration(
            moa_observability::SessionEventAppendPhase::DirectAction,
            action_started.elapsed(),
        );
        if event_is_error {
            record_session_error("event_log");
        }
        action
    } else {
        crate::restate_identity::replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id,
                    event,
                    dedupe_key: Some(dedupe.key),
                })),
        )
        .call()
        .instrument(persist_span)
        .await?
        .into_inner()
    };
    record_turn_event_persist_duration(persist_started.elapsed(), 1);
    Ok(record)
}

async fn emit_direct_event(
    store: &moa_session::PostgresSessionStore,
    session_id: SessionId,
    event: Event,
    dedupe_key: String,
) -> moa_core::error::Result<EventRecord> {
    store
        .emit_event_record(session_id, event, Some(dedupe_key))
        .await
}

struct TurnEventIdentity {
    key: String,
    action_name: String,
    jitter_ms: u64,
}

const K_EVENT_APPEND_SEQUENCE: &str = "turn_event_append_sequence";

async fn next_turn_event_identity(
    ctx: &WorkflowContext<'_>,
) -> Result<TurnEventIdentity, HandlerError> {
    let sequence = ctx
        .get::<Json<u64>>(K_EVENT_APPEND_SEQUENCE)
        .await?
        .map(Json::into_inner)
        .unwrap_or_default();
    ctx.set(
        K_EVENT_APPEND_SEQUENCE,
        Json::from(sequence.saturating_add(1)),
    );
    Ok(turn_event_identity(ctx.key(), sequence))
}

fn turn_event_identity(turn_id: &str, sequence: u64) -> TurnEventIdentity {
    let turn_digest = blake3::hash(turn_id.as_bytes());
    let short = turn_digest.to_hex()[..12].to_string();
    let jitter_ms = 50 + (u64::from(turn_digest.as_bytes()[0]) + sequence.wrapping_mul(31)) % 101;
    TurnEventIdentity {
        key: format!("turn_event:{turn_id}:{sequence}"),
        action_name: format!("turn_event_append_{short}_{sequence}"),
        jitter_ms,
    }
}

fn turn_event_append_retry_policy(jitter_ms: u64) -> RunRetryPolicy {
    RunRetryPolicy::new()
        .initial_delay(Duration::from_millis(jitter_ms))
        .exponentiation_factor(2.0)
        .max_delay(Duration::from_secs(1))
        .max_attempts(5)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_event_identity_is_unique_per_append_and_stable_per_sequence() {
        // Pins: a replay of one logical append reuses its database dedupe key
        // and action name, while two identical event bodies remain distinct.
        let first = turn_event_identity("turn-123", 7);
        let replay = turn_event_identity("turn-123", 7);
        let next = turn_event_identity("turn-123", 8);

        assert_eq!(first.key, replay.key);
        assert_eq!(first.action_name, replay.action_name);
        assert_eq!(first.jitter_ms, replay.jitter_ms);
        assert_ne!(first.key, next.key);
        assert_ne!(first.action_name, next.action_name);
        assert!(first.action_name.len() <= 64);
        assert!((50..=150).contains(&first.jitter_ms));
    }

    #[test]
    fn turn_event_identity_is_namespaced_by_workflow() {
        // Pins: root and worker workflows may share the same append sequence
        // without sharing a database idempotency key.
        let root = turn_event_identity("root-turn", 0);
        let worker = turn_event_identity("worker-turn", 0);

        assert_ne!(root.key, worker.key);
        assert_ne!(root.action_name, worker.action_name);
    }

    #[cfg(feature = "execution-planning-failpoints")]
    #[tokio::test]
    async fn direct_append_lost_ack_retry_materializes_one_event_db() {
        // Pins: the direct action passes its replay-stable identity into the
        // store, so an error returned after commit cannot duplicate the event.
        use moa_core::types::contact::SessionActorRef;
        use moa_core::types::identifiers::{ModelId, TenantId};
        use moa_session::failpoints;
        use moa_test_support::postgres::bootstrap_test_db;

        let test_db = bootstrap_test_db().await.expect("bootstrap test database");
        let session_id = test_db
            .store()
            .create_session(SessionMeta {
                tenant_id: TenantId::new(),
                created_by: Some(SessionActorRef::Identity {
                    id: uuid::Uuid::from_u128(42),
                }),
                model: ModelId::new("test-model"),
                ..SessionMeta::default()
            })
            .await
            .expect("create direct-append session");
        let event = Event::UserMessage {
            text: "post-commit direct append".to_string(),
            attachments: Vec::new(),
        };
        let identity = turn_event_identity("turn-direct-failpoint", 0);
        failpoints::arm("event_append_post_commit", 1);

        let first = emit_direct_event(
            test_db.store(),
            session_id,
            event.clone(),
            identity.key.clone(),
        )
        .await;
        assert!(first.is_err(), "post-commit ack failpoint must surface");

        let retried = emit_direct_event(test_db.store(), session_id, event, identity.key)
            .await
            .expect("direct append retry should resolve the committed event");
        assert_eq!(retried.sequence_num, 0);
        let events = test_db
            .store()
            .get_events(
                session_id,
                moa_core::types::events_stream::EventRange::all(),
            )
            .await
            .expect("load direct append events");
        assert_eq!(events.len(), 1, "direct retry must not duplicate the event");
        failpoints::reset("event_append_post_commit");
    }
}
