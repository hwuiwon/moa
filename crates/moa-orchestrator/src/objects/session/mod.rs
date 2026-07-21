//! Restate virtual object that owns one durable MOA session key.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use chrono::{DateTime, Utc};
use moa_config::SessionLimitsConfig;
use moa_core::traits::SessionRepository;
use moa_core::{
    error::MoaError, error::Result as MoaResult, events::Event, events::ExecutionInputRequired,
    events::ExecutionProgress, events::ExecutionRunEvidenceRef,
    events::ExecutionSynthesisRequested, types::contact::ContactRef,
    types::events_stream::ClaimCheck, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::execution_planning::ExecutionRunStarted,
    types::identifiers::SessionId, types::segments::ActiveSegment, types::session::CancelScope,
    types::session::SessionMeta, types::session::SessionStatus, types::session::UserMessage,
    types::worker::commands::ConsumeWorkerChildResultInput,
    types::worker::commands::ConsumeWorkerChildResultOutput,
    types::worker::commands::MarkWorkerChildTerminalInput,
    types::worker::commands::UserReplyDeliveryAck, types::worker::state::ChildSignalKind,
    types::worker::state::InputAudience, types::worker::state::ParentResumePolicy,
    types::worker::state::SignalSeverity, types::worker::state::UnreadChildSignal,
    types::worker::state::WorkerChildRef, types::worker::state::WorkerId,
    types::worker::state::WorkerProgressSummary, types::worker::state::WorkerSignal,
    types::worker::state::WorkerTerminalResult,
};
use moa_observability::record_turn_event_persist_duration;
use moa_session::PostgresSessionStore;
use moa_wire::session_store::{AppendEventRequest, GetEventsRequest, UpdateStatusRequest};
use moa_wire::turn::{
    CancelResponse, PendingMessage, QueueMessageRequest, QueueMessageResponse, RunTurnRequest,
    SessionProgress, SessionProgressRequest, SessionSnapshot, StartTurnRequest, StartTurnResponse,
    TurnOutcome as ExecutionTurnOutcome, TurnOutcomeKind as ExecutionTurnOutcomeKind, TurnProgress,
    TurnTrigger,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::objects::durable_utc_now;
use crate::objects::worker::WorkerClient;
use crate::objects::worker::WorkerProvideInputRequest;
use crate::restate_identity::with_identity_headers;
use crate::services::execution::ExecutionClient;
use crate::services::session_store::RestateSessionStoreClient;
use crate::vo::{
    Tracked, VoReader, VoState, set_changed_opt, set_changed_scalar, set_changed_vec,
    set_or_clear_opt, set_or_clear_scalar, set_or_clear_vec,
};
use crate::worker_dispatch::MAX_WORKER_FAN_OUT;
use crate::workflows::turn_execution::TurnExecutionClient;
use moa_observability::restate_observability::{annotate_restate_handler_span, event_persist_span};

mod execution_runs;
mod handlers;
mod liveness;
mod narration;
mod persistence;
mod state;

use crate::workflows::errors::moa_error_to_handler_error;
use persistence::{parse_session_key, sync_status};
pub use state::{
    ActiveExecutionRunState, ExecutionProgressSignature, ExecutionSynthesisDedupe,
    ExecutionTemplateAdmissionReplayState, ExecutionTemplateAdmissionResume,
    PendingUserReplyTarget,
};
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

/// Session delivery payload for one committed execution-run admission.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRunStartedDelivery {
    /// Exact durable session event payload from Task 7.
    pub started: ExecutionRunStarted,
    /// Exact approved run budget needed for confirmation replay.
    pub approved_budget: moa_artifacts::execution_plan::ExecutionBudgetLimit,
}

/// Stable result of delivering one terminal execution into its linked synthesis turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSynthesisDispatch {
    /// Durable execution-run identifier.
    pub run_uid: uuid::Uuid,
    /// Exact persisted user event that originated the run.
    pub originating_user_sequence_num: u64,
    /// Stable linked synthesis workflow key.
    pub turn_id: String,
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

    /// Commits and activates one detached execution-run admission marker.
    async fn execution_run_started(
        delivery: Json<ExecutionRunStartedDelivery>,
    ) -> Result<(), HandlerError>;

    /// Admits one exact pinned execution template through this existing Session.
    async fn admit_execution_template(
        request: Json<moa_execution::wire::ExecutionTemplateAdmissionRequest>,
    ) -> Result<Json<moa_execution::wire::ExecutionTemplateAdmissionResponse>, HandlerError>;

    /// Publishes one cadence- and delta-gated aggregate execution progress event.
    async fn execution_progress(progress: Json<ExecutionProgress>) -> Result<(), HandlerError>;

    /// Publishes and activates one exact user-addressed execution input request.
    async fn execution_input_required(
        input: Json<ExecutionInputRequired>,
    ) -> Result<(), HandlerError>;

    /// Publishes terminal evidence and durably dispatches its one linked synthesis turn.
    async fn execution_terminal(
        delivery: Json<moa_execution::wire::ExecutionTerminalDelivery>,
    ) -> Result<Json<ExecutionSynthesisDispatch>, HandlerError>;

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
pub struct SessionImpl {
    session_store: Arc<dyn SessionRepository>,
    session_store_backend: Arc<PostgresSessionStore>,
    session_limits: SessionLimitsConfig,
}

impl SessionImpl {
    /// Creates a session object with its persistence and scheduling dependencies.
    #[must_use]
    pub fn new(
        session_store: Arc<PostgresSessionStore>,
        session_limits: SessionLimitsConfig,
    ) -> Self {
        Self {
            session_store: session_store.clone(),
            session_store_backend: session_store,
            session_limits,
        }
    }
}

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
    crate::restate_identity::replay_safe_request(
        ctx.service_client::<RestateSessionStoreClient>()
            .append_event(Json(AppendEventRequest {
                session_id,
                event,
                dedupe_key: Some(dedupe_key),
            })),
    )
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

/// Restores child-progress summaries to plan order after concurrent live reads.
///
/// Missing slots represent failed live reads and are omitted without disturbing
/// the relative order of the remaining cached and live summaries.
#[must_use]
pub fn child_progress_in_plan_order(
    summaries: Vec<Option<WorkerProgressSummary>>,
) -> Vec<WorkerProgressSummary> {
    summaries.into_iter().flatten().collect()
}
