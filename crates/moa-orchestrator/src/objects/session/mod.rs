//! Restate virtual object that owns one durable MOA session key.

use std::collections::VecDeque;
use std::time::Instant;

use chrono::{DateTime, Utc};
use moa_core::wire::session_store::{AppendEventRequest, UpdateStatusRequest};
use moa_core::wire::turn::{
    CancelResponse, PendingMessage, QueueMessageRequest, QueueMessageResponse, RunTurnRequest,
    SessionProgress, SessionProgressRequest, SessionSnapshot, StartTurnRequest, StartTurnResponse,
    TurnOutcome as ExecutionTurnOutcome, TurnOutcomeKind as ExecutionTurnOutcomeKind, TurnProgress,
    TurnTrigger,
};
use moa_core::{
    ActiveSegment, CancelScope, ChildSignalKind, ConsumeWorkerChildResultInput,
    ConsumeWorkerChildResultOutput, ContactRef, Event, EventRange, EventRecord, InputAudience,
    MarkWorkerChildTerminalInput, MoaError, ParentResumePolicy, Result as MoaResult, SessionId,
    SessionMeta, SessionStatus, SignalSeverity, UnreadChildSignal, UserMessage, WorkerChildRef,
    WorkerId, WorkerMessage, WorkerProgressSummary, WorkerSignal, WorkerTerminalResult,
};
use moa_observability::record_turn_event_persist_duration;
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::OrchestratorCtx;
use crate::objects::durable_utc_now;
use crate::objects::worker::WorkerClient;
use crate::restate_identity::with_identity_headers;
use crate::services::session_store::RestateSessionStoreClient;
use crate::vo::{VoReader, VoState, set_or_clear_opt, set_or_clear_scalar, set_or_clear_vec};
use crate::worker_dispatch::MAX_WORKER_FAN_OUT;
use crate::workflows::turn_execution::TurnExecutionClient;
use moa_observability::restate_observability::{annotate_restate_handler_span, event_persist_span};

mod handlers;
mod liveness;
mod narration;
mod persistence;
mod state;

use crate::workflows::errors::moa_error_to_handler_error;
use persistence::{parse_session_key, sync_status};
pub use state::{ChildLivenessState, ResumeBudget, ResumeTurnContext, SessionVoState};

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

/// Input for registering a deterministic auto-delegation run owned by the session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegisterAutoDelegationRunInput {
    /// User-message sequence that caused the worker DAG to be scheduled.
    pub user_sequence_num: u64,
    /// Worker ids in deterministic scheduled order.
    pub worker_ids: Vec<WorkerId>,
}

/// Internal payload for a generation-guarded progress-narration tick self-call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NarrationTickRequest {
    /// Scheduling generation observed when this tick was scheduled. A tick whose
    /// generation no longer matches the VO's current generation is stale and is
    /// ignored without rescheduling, because a newer generation now owns scheduling.
    pub generation: u64,
}

/// Internal payload for a generation-guarded per-child liveness-watchdog self-call.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckChildLivenessRequest {
    /// Active child whose heartbeat liveness this check evaluates.
    pub worker_id: WorkerId,
    /// Per-child scheduling generation observed when this check was scheduled. A check
    /// whose generation no longer matches the child's outstanding entry is stale and is
    /// ignored, because a newer arming (or a clear) now owns scheduling for the child.
    pub expected_generation: u64,
    /// When this check was scheduled to fire (journaled at schedule time, informational).
    pub scheduled_at: DateTime<Utc>,
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

    /// Registers a root-owned child worker for later turns and cancellation.
    async fn register_child(child: Json<WorkerChildRef>) -> Result<(), HandlerError>;

    /// Registers the worker set for deterministic auto-delegation completion fan-in.
    async fn register_auto_delegation_run(
        input: Json<RegisterAutoDelegationRunInput>,
    ) -> Result<(), HandlerError>;

    /// Removes a root-owned child worker from the active registry.
    async fn remove_child(worker_id: String) -> Result<(), HandlerError>;

    /// Caches a root child terminal result until a wait consumes it.
    async fn mark_child_terminal(
        input: Json<MarkWorkerChildTerminalInput>,
    ) -> Result<(), HandlerError>;

    /// Consumes a cached root child terminal result.
    async fn consume_child_result(
        input: Json<ConsumeWorkerChildResultInput>,
    ) -> Result<Json<ConsumeWorkerChildResultOutput>, HandlerError>;

    /// Lists root-owned active child workers.
    #[shared]
    async fn child_refs() -> Result<Json<Vec<WorkerChildRef>>, HandlerError>;

    /// Records a control-plane attention signal raised by an owned child worker.
    ///
    /// Idempotent: a retried delivery (same `signal_id`) appends no second event and
    /// records no duplicate unread entry. May arm a guarded coordinator auto-resume
    /// when the parent is idle (decision only in this increment; dispatch is Task 6).
    async fn record_child_signal(signal: Json<WorkerSignal>) -> Result<(), HandlerError>;

    /// Clears all persisted VO state for this session key.
    async fn destroy() -> Result<(), HandlerError>;

    /// Internal generation-guarded tick that drives per-session progress narration.
    async fn narration_tick(req: Json<NarrationTickRequest>) -> Result<(), HandlerError>;

    /// Internal generation-guarded per-child heartbeat-liveness watchdog tick.
    async fn check_child_liveness(req: Json<CheckChildLivenessRequest>)
    -> Result<(), HandlerError>;
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
/// active children are fetched on demand via `Worker::progress_summary`, capped
/// by the existing fan-out limit so the fan-in never walks an unbounded tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChildProgressFetch {
    /// Summary already synthesized from a cached terminal child ref.
    Ready(WorkerProgressSummary),
    /// Active child id whose compact summary must be fetched live.
    Fetch(WorkerId),
}

/// Synthesizes a compact summary for a terminal child from its cached parent ref,
/// avoiding a live call to the (possibly self-cleaned) child VO.
#[must_use]
pub fn terminal_child_summary(
    child: &WorkerChildRef,
    terminal: &WorkerTerminalResult,
) -> WorkerProgressSummary {
    terminal_result_summary(child.id.clone(), terminal)
}

/// Synthesizes a compact summary directly from a terminal result and child id.
///
/// Shared by the cached-ref fan-in (`terminal_child_summary`) and the
/// `wait_worker` terminal path, which has the terminal result in hand but no
/// child ref. Avoids a live call to the (possibly self-cleaned) child VO.
#[must_use]
pub fn terminal_result_summary(
    worker_id: WorkerId,
    terminal: &WorkerTerminalResult,
) -> WorkerProgressSummary {
    WorkerProgressSummary {
        worker_id,
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
        // A terminal child is no longer running and cannot be blocked on input.
        awaiting_input: false,
    }
}

/// Appends one durable session event with an idempotency dedupe key.
///
/// A retried call with the same `(session_id, dedupe_key)` returns the first persisted
/// sequence and inserts no second row (Task 2), so control-plane signal and watchdog
/// event recording is safe to retry after partial delivery.
async fn append_session_event_deduped(
    ctx: &ObjectContext<'_>,
    session_id: SessionId,
    event: Event,
    dedupe_key: String,
) -> Result<(), HandlerError> {
    ctx.service_client::<RestateSessionStoreClient>()
        .append_event(Json(AppendEventRequest {
            session_id,
            event,
            dedupe_key: Some(dedupe_key),
        }))
        .call()
        .await?;
    Ok(())
}

/// Plans the on-demand, bounded child-progress fan-in for `Session/progress`.
///
/// This is bounded on-demand fan-in (not pushed through the single-writer VO):
/// terminal children are synthesized from cached refs in place, and at most
/// `max_live` active children are scheduled for a live `progress_summary` read.
#[must_use]
pub fn plan_child_progress_fan_in(
    children: &[WorkerChildRef],
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
