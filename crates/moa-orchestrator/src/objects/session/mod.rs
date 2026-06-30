//! Restate virtual object that owns one durable MOA session key.

use std::collections::VecDeque;
use std::time::Instant;

use chrono::{DateTime, Utc};
use moa_core::wire::session_store::{AppendEventRequest, UpdateStatusRequest};
use moa_core::wire::turn::{
    CancelResponse, PendingMessage, QueueMessageRequest, QueueMessageResponse, RunTurnRequest,
    SessionProgress, SessionProgressRequest, SessionSnapshot, StartTurnRequest, StartTurnResponse,
    TurnOutcome as ExecutionTurnOutcome, TurnOutcomeKind as ExecutionTurnOutcomeKind, TurnProgress,
};
use moa_core::{
    ActiveSegment, CancelScope, ConsumeSubAgentChildResultInput, ConsumeSubAgentChildResultOutput,
    ContactRef, Event, EventRange, EventRecord, MarkSubAgentChildTerminalInput, MoaError,
    Result as MoaResult, SessionId, SessionMeta, SessionStatus, SubAgentChildRef, SubAgentId,
    SubAgentProgressSummary, SubAgentSignal, SubAgentTerminalResult, UnreadChildSignal,
    UserMessage,
};
use moa_observability::record_turn_event_persist_duration;
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::objects::durable_utc_now;
use crate::objects::sub_agent::SubAgentClient;
use crate::restate_identity::with_identity_headers;
use crate::services::session_store::RestateSessionStoreClient;
use crate::sub_agent_dispatch::MAX_SUB_AGENT_FAN_OUT;
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_scalar, set_or_clear_vec};
use crate::workflows::turn_execution::TurnExecutionClient;
use moa_observability::restate_observability::{annotate_restate_handler_span, event_persist_span};

mod handlers;
mod narration;
mod persistence;
mod state;

use crate::workflows::errors::moa_error_to_handler_error;
use persistence::{parse_session_key, sync_status};
pub use state::{ResumeBudget, SessionVoState};

const K_PENDING_STATE: &str = "pending_state";

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SessionPendingState {
    active_turn_id: Option<String>,
    pending_messages: VecDeque<PendingMessage>,
    last_outcome: Option<ExecutionTurnOutcome>,
    #[serde(default)]
    turn_waiters: Vec<SessionTurnWaiter>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SessionTurnWaiter {
    turn_id: String,
    awakeable_id: String,
}

/// Input for registering a workflow awakeable that resolves when a session turn completes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachSessionTurnWaiterInput {
    /// Stable turn id returned when the turn was admitted.
    pub turn_id: String,
    /// Awakeable id owned by the waiting workflow.
    pub awakeable_id: String,
}

/// Output returned after registering a session turn waiter.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachSessionTurnWaiterOutput {
    /// Already available terminal outcome, if the turn completed before registration.
    pub outcome: Option<ExecutionTurnOutcome>,
}

/// Input for removing a workflow awakeable after a bounded turn wait times out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemoveSessionTurnWaiterInput {
    /// Stable turn id the waiter was attached to.
    pub turn_id: String,
    /// Awakeable id that should no longer be resolved by the session.
    pub awakeable_id: String,
}

/// Internal payload for a generation-guarded progress-narration tick self-call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NarrationTickRequest {
    /// Scheduling generation observed when this tick was scheduled. A tick whose
    /// generation no longer matches the VO's current generation is stale and is
    /// ignored without rescheduling, because a newer generation now owns scheduling.
    pub generation: u64,
}

/// Restate virtual object surface for one durable session key.
#[restate_sdk::object]
pub trait Session {
    /// Initializes VO state after `SessionStore/create_session` persists metadata in Postgres.
    async fn set_meta(meta: Json<SessionMeta>) -> Result<(), HandlerError>;

    /// Appends a user message and drives turns until the session becomes idle or blocked.
    async fn post_message(msg: Json<UserMessage>) -> Result<(), HandlerError>;

    /// Requests cancellation at the given scope: only the coordinator turn, or the whole task tree.
    async fn cancel(scope: Json<CancelScope>) -> Result<(), HandlerError>;

    /// Returns the current durable lifecycle status without entering the single-writer queue.
    #[shared]
    async fn status() -> Result<Json<SessionStatus>, HandlerError>;

    /// Starts a new turn through the additive `TurnExecution` workflow path.
    async fn start_turn(
        req: Json<StartTurnRequest>,
    ) -> Result<Json<StartTurnResponse>, HandlerError>;

    /// Records the terminal outcome delivered by a `TurnExecution` workflow.
    async fn record_turn_outcome(outcome: Json<ExecutionTurnOutcome>) -> Result<(), HandlerError>;

    /// Registers a workflow awakeable that should resolve when the turn completes.
    async fn attach_turn_waiter(
        input: Json<AttachSessionTurnWaiterInput>,
    ) -> Result<Json<AttachSessionTurnWaiterOutput>, HandlerError>;

    /// Removes a workflow awakeable after a bounded turn wait times out.
    async fn remove_turn_waiter(
        input: Json<RemoveSessionTurnWaiterInput>,
    ) -> Result<(), HandlerError>;

    /// Forwards a cancellation request to the active `TurnExecution` workflow.
    async fn request_cancel(reason: Json<String>) -> Result<Json<CancelResponse>, HandlerError>;

    /// Queues a user message or starts a turn immediately when no turn is active.
    async fn queue_message(
        req: Json<QueueMessageRequest>,
    ) -> Result<Json<QueueMessageResponse>, HandlerError>;

    /// Returns a read-only snapshot of the additive `TurnExecution` lifecycle state.
    #[shared]
    async fn snapshot() -> Result<Json<SessionSnapshot>, HandlerError>;

    /// Returns session snapshot, active-turn progress, and recent durable events in one call.
    #[shared]
    async fn progress(
        req: Json<SessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError>;

    /// Registers a root-owned child sub-agent for later turns and cancellation.
    async fn register_child(child: Json<SubAgentChildRef>) -> Result<(), HandlerError>;

    /// Removes a root-owned child sub-agent from the active registry.
    async fn remove_child(sub_agent_id: String) -> Result<(), HandlerError>;

    /// Caches a root child terminal result until a wait consumes it.
    async fn mark_child_terminal(
        input: Json<MarkSubAgentChildTerminalInput>,
    ) -> Result<(), HandlerError>;

    /// Consumes a cached root child terminal result.
    async fn consume_child_result(
        input: Json<ConsumeSubAgentChildResultInput>,
    ) -> Result<Json<ConsumeSubAgentChildResultOutput>, HandlerError>;

    /// Lists root-owned active child sub-agents.
    #[shared]
    async fn child_refs() -> Result<Json<Vec<SubAgentChildRef>>, HandlerError>;

    /// Records a control-plane attention signal raised by an owned child sub-agent.
    ///
    /// Idempotent: a retried delivery (same `signal_id`) appends no second event and
    /// records no duplicate unread entry. May arm a guarded coordinator auto-resume
    /// when the parent is idle (decision only in this increment; dispatch is Task 6).
    async fn record_child_signal(signal: Json<SubAgentSignal>) -> Result<(), HandlerError>;

    /// Clears all persisted VO state for this session key.
    async fn destroy() -> Result<(), HandlerError>;

    /// Internal generation-guarded tick that drives per-session progress narration.
    async fn narration_tick(req: Json<NarrationTickRequest>) -> Result<(), HandlerError>;
}

/// Concrete `Session` virtual object implementation.
pub struct SessionImpl;

async fn load_pending_state<R: VoReader>(reader: &R) -> Result<SessionPendingState, HandlerError> {
    Ok(reader.get_json(K_PENDING_STATE).await?.unwrap_or_default())
}

fn persist_pending_state(ctx: &ObjectContext<'_>, state: &SessionPendingState) {
    ctx.set(K_PENDING_STATE, Json::from(state.clone()));
}

fn generate_turn_id(ctx: &mut ObjectContext<'_>) -> String {
    ctx.rand_uuid().to_string()
}

fn dispatch_turn_execution(ctx: &ObjectContext<'_>, request: RunTurnRequest) {
    let turn_id = request.turn_id.clone();
    let identity = request.identity.clone();
    let request = ctx
        .workflow_client::<TurnExecutionClient>(turn_id.clone())
        .run(Json::from(request));
    with_identity_headers(request, &identity).send();
}

/// One planned step for the bounded `Session/progress` child fan-in.
///
/// Terminal children are synthesized from the cached parent ref with no live call;
/// active children are fetched on demand via `SubAgent::progress_summary`, capped
/// by the existing fan-out limit so the fan-in never walks an unbounded tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildProgressFetch {
    /// Summary already synthesized from a cached terminal child ref.
    Ready(SubAgentProgressSummary),
    /// Active child id whose compact summary must be fetched live.
    Fetch(SubAgentId),
}

/// Synthesizes a compact summary for a terminal child from its cached parent ref,
/// avoiding a live call to the (possibly self-cleaned) child VO.
#[must_use]
pub fn terminal_child_summary(
    child: &SubAgentChildRef,
    terminal: &SubAgentTerminalResult,
) -> SubAgentProgressSummary {
    SubAgentProgressSummary {
        sub_agent_id: child.id.clone(),
        state: terminal.state,
        active_turn_id: None,
        last_summary: Some(
            terminal
                .result
                .error
                .clone()
                .unwrap_or_else(|| terminal.result.output.clone()),
        ),
        tokens_used: terminal.result.tokens_used,
        budget_remaining: 0,
        last_heartbeat_at: None,
        stale: false,
    }
}

/// Plans the on-demand, bounded child-progress fan-in for `Session/progress`.
///
/// This is bounded on-demand fan-in (not pushed through the single-writer VO):
/// terminal children are synthesized from cached refs in place, and at most
/// `max_live` active children are scheduled for a live `progress_summary` read.
#[must_use]
pub fn plan_child_progress_fan_in(
    children: &[SubAgentChildRef],
    max_live: usize,
) -> Vec<ChildProgressFetch> {
    let mut plan = Vec::with_capacity(children.len());
    let mut live = 0usize;
    for child in children {
        match &child.terminal {
            Some(terminal) => plan.push(ChildProgressFetch::Ready(terminal_child_summary(
                child, terminal,
            ))),
            None if live < max_live => {
                live += 1;
                plan.push(ChildProgressFetch::Fetch(child.id.clone()));
            }
            None => {}
        }
    }
    plan
}
