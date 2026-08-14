//! Shared session-event helpers for the turn-execution workflows.
//!
//! Root session turns (`turn_execution`) and worker turns
//! (`worker_turn_execution`) persist the same durable events through identical
//! wiring. These helpers are the single definition of that wiring so both
//! workflows emit bit-identical events, fields, and tracing.
//!
//! Every append needs the session-store backend and the append-strategy flag.
//! Both arrive as an explicitly injected [`TurnEventAppender`]: the composition
//! root builds one and hands it to each workflow implementation, which owns it
//! and passes it down. Nothing here reads global runtime state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use moa_core::{
    events::Event, events::TurnFailureActor, events::TurnFailureClass, traits::SessionStore as _,
    types::completion::ToolCallContent, types::completion::ToolInvocation,
    types::events_stream::EventRecord, types::identifiers::SessionId,
    types::identifiers::ToolCallId, types::provider::ModelTier, types::session::SessionMeta,
    types::tools::SecuredToolOutput,
};
use moa_observability::restate_observability::event_persist_span;
use moa_observability::{record_session_error, record_turn_event_persist_duration};
use moa_session::PostgresSessionStore;
use moa_wire::session_store::{
    AppendEventRequest, RecordSegmentSkillUseRequest, RecordSegmentToolUseRequest,
};
use moa_wire::turn::TurnOutcomeKind;
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::services::session_store::RestateSessionStoreClient;
use crate::workflows::turn_responsiveness::ToolBudgetExhausted;

/// Durable turn-event append dependency owned by a workflow implementation.
///
/// Carries the two things an append needs — the concrete session-store backend
/// and whether appends bypass the `SessionStore` service — so neither is read
/// from global runtime state at append time. Cloning is cheap: the backend is
/// shared behind an [`Arc`].
#[derive(Clone)]
pub struct TurnEventAppender {
    session_store: Arc<PostgresSessionStore>,
    direct_append: bool,
}

impl TurnEventAppender {
    /// Builds the appender from the session-store backend and append strategy.
    ///
    /// `direct_append` mirrors `session.direct_turn_event_append`: when set,
    /// appends run as a durable action against `session_store` instead of an
    /// RPC to the `SessionStore` service.
    #[must_use]
    pub fn new(session_store: Arc<PostgresSessionStore>, direct_append: bool) -> Self {
        Self {
            session_store,
            direct_append,
        }
    }
}

/// Appends one durable session event and returns its stored record.
///
/// Wraps the append in the standard persistence span and latency counters so
/// every turn-event write is measured identically across both workflows.
pub(super) async fn append_session_event(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
) -> Result<EventRecord, HandlerError> {
    let dedupe = next_turn_event_identity(ctx).await?;
    append_with_identity(appender, ctx, session_id, event, dedupe).await
}

/// Appends one durable session event under a caller-supplied dedupe key.
///
/// The ordinary path derives its dedupe key from a per-workflow append sequence,
/// which makes two logically distinct facts collide only if they occupy the same
/// sequence slot. A fact whose identity is defined by its own domain — the
/// canonical failed-turn event, keyed by actor plus turn — must not depend on how
/// many appends happened to precede it, so it names its key explicitly. The key
/// alone determines the durable action identity, so a replayed workflow re-derives
/// the same append and the event materializes exactly once.
pub(super) async fn append_session_event_with_dedupe_key(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
    dedupe_key: String,
) -> Result<EventRecord, HandlerError> {
    let dedupe = keyed_turn_event_identity(dedupe_key);
    append_with_identity(appender, ctx, session_id, event, dedupe).await
}

async fn append_with_identity(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    event: Event,
    dedupe: TurnEventIdentity,
) -> Result<EventRecord, HandlerError> {
    let persist_span = event_persist_span(1);
    let persist_started = Instant::now();
    moa_core::coordination_counters::record_durable_append();
    let record = if appender.direct_append {
        let action_started = Instant::now();
        let store = appender.session_store.clone();
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
    store: &PostgresSessionStore,
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

/// Builds the durable append identity for a caller-supplied dedupe key.
///
/// The action name and retry jitter are derived from the key itself, so the
/// identity is a pure function of the fact being recorded and stays stable across
/// replay without consuming the workflow's append sequence.
fn keyed_turn_event_identity(dedupe_key: String) -> TurnEventIdentity {
    let digest = blake3::hash(dedupe_key.as_bytes());
    let short = digest.to_hex()[..12].to_string();
    TurnEventIdentity {
        key: dedupe_key,
        action_name: format!("turn_event_append_keyed_{short}"),
        jitter_ms: 50 + u64::from(digest.as_bytes()[0]) % 101,
    }
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
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    tool_call: &ToolCallContent,
) -> Result<(), HandlerError> {
    let invocation = tool_call.invocation.clone();
    append_session_event(
        appender,
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
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    tool_id: ToolCallId,
    invocation: &ToolInvocation,
    secured: &SecuredToolOutput,
) -> Result<(), HandlerError> {
    append_session_event(
        appender,
        ctx,
        session_id,
        Event::tool_result(tool_id, invocation.id.clone(), secured.clone()),
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
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    exhaustion: &ToolBudgetExhausted,
) -> Result<(), HandlerError> {
    record_session_error("tool_budget");
    append_session_event(
        appender,
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

/// Appends the canonical failed-turn fact and returns its safe summary.
///
/// This is the single writer of [`Event::TurnFailed`]. Both turn workflows call
/// it at their catch-all failure boundary, before telling their owning object the
/// outcome, so the failure is durably recorded even if the callback, attention
/// signal, or notification that follows is lost. The returned summary is the same
/// bounded, secret-free sentence that was persisted, and is what the caller must
/// put in `TurnOutcome.message`: the underlying error is logged for operators and
/// never persisted.
///
/// The append is keyed `turn_failed:{actor_key}:{turn_id}`, so a workflow replay
/// re-derives one identical append and the fact materializes exactly once per
/// actor and turn.
/// Derives the canonical failed-turn dedupe key for one actor and turn.
///
/// The key is a persisted durability contract: the event store deduplicates on
/// it, so every path that records the same actor's failure for the same turn —
/// catch-all boundary, body-reported failure, or an independent re-invocation —
/// must derive the identical string. It is a pure function of the fact alone,
/// never of append order, clocks, or randomness.
pub(super) fn turn_failed_dedupe_key(actor: &TurnFailureActor, turn_id: &str) -> String {
    format!("turn_failed:{}:{turn_id}", actor.actor_key())
}

pub(super) async fn append_turn_failed(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    actor: TurnFailureActor,
    turn_id: &str,
    class: TurnFailureClass,
) -> Result<String, HandlerError> {
    let summary = class.summary().to_string();
    record_session_error("turn_failed");
    append_session_event_with_dedupe_key(
        appender,
        ctx,
        session_id,
        Event::TurnFailed {
            actor: actor.clone(),
            turn_id: turn_id.to_string(),
            class,
            summary: summary.clone(),
        },
        turn_failed_dedupe_key(&actor, turn_id),
    )
    .await?;
    Ok(summary)
}

/// Stable, hand-authored terminal rejection codes that may surface verbatim in
/// a failed `TurnOutcome.message`.
///
/// The catch-all boundaries replace arbitrary workflow error text with the
/// fixed class sentence because those errors can carry provider, tool, and
/// prompt material. Deliberate policy rejections are different: their text is
/// authored in this repository as a stable code that callers match on, so
/// erasing it would collapse a typed contract into an indistinguishable
/// generic failure. Only codes on this closed list survive sanitization;
/// everything else keeps the fixed class sentence.
/// Repository-authored failure text that may survive catch-all sanitization.
pub(super) const SAFE_TERMINAL_REJECTION_CODES: &[&str] = &[
    "durable_execution_requires_user_message_origin",
    "run_requires_user_message_origin",
];

/// Returns the stable rejection code carried by a hand-authored terminal
/// error, or `None` for every other failure.
///
/// Matches on the debug rendering because `HandlerError` exposes no `Display`;
/// the closed allowlist means only exact repository-authored constants can
/// ever pass through, whatever the rendering carries around them.
pub(super) fn safe_terminal_rejection_code(error: &impl std::fmt::Debug) -> Option<&'static str> {
    let rendered = format!("{error:?}");
    SAFE_TERMINAL_REJECTION_CODES
        .iter()
        .copied()
        .find(|code| rendered.contains(code))
}

/// Persists a zero-cost auxiliary assistant response and returns its text.
///
/// Used for canned replies (clarifications, budget-stop notices) that never
/// call the model, so all token and cost fields are zero.
pub(super) async fn append_zero_cost_assistant_response(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    text: String,
) -> Result<String, HandlerError> {
    append_zero_cost_assistant_response_with_sequence(appender, ctx, session_id, meta, text)
        .await
        .map(|(text, _sequence_num)| text)
}

/// Persists a zero-cost auxiliary assistant response and returns its text plus sequence number.
///
/// Root turn execution uses the sequence number to bound post-outcome segment assessment.
pub(super) async fn append_zero_cost_assistant_response_with_sequence(
    appender: &TurnEventAppender,
    ctx: &WorkflowContext<'_>,
    session_id: SessionId,
    meta: &SessionMeta,
    text: String,
) -> Result<(String, u64), HandlerError> {
    let record = append_session_event(
        appender,
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
    fn turn_failed_dedupe_key_is_a_pure_function_of_actor_and_turn() {
        // Pins: the canonical failed-turn dedupe key is derived from the actor
        // and turn alone. The event store deduplicates on this exact string,
        // so a sequence-, clock-, or randomness-derived key would let a
        // re-executed append materialize the same failure fact twice, and an
        // operator counting failures would double-count every failed turn.
        assert_eq!(
            turn_failed_dedupe_key(&TurnFailureActor::Coordinator, "turn-1"),
            "turn_failed:coordinator:turn-1",
        );
        assert_eq!(
            turn_failed_dedupe_key(
                &TurnFailureActor::Worker {
                    worker_id: "worker-9".to_string(),
                },
                "turn-2",
            ),
            "turn_failed:worker:worker-9:turn-2",
        );
        // Re-derivation is stable: the same fact always yields the same key.
        assert_eq!(
            turn_failed_dedupe_key(&TurnFailureActor::Coordinator, "turn-1"),
            turn_failed_dedupe_key(&TurnFailureActor::Coordinator, "turn-1"),
        );
    }

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
