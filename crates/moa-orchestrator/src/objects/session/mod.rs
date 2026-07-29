//! Restate virtual object that owns one durable MOA session key.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use moa_config::{MoaConfig, SessionLimitsConfig};
use moa_core::traits::SessionStore;
use moa_core::{
    error::MoaError, error::Result as MoaResult, events::Event, events::ExecutionInputRequired,
    events::ExecutionProgress, events::ExecutionRunEvidenceRef,
    events::ExecutionSynthesisRequested, events::QueuedMessageRejection,
    types::contact::ContactRef, types::events_stream::ClaimCheck, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::execution_planning::ExecutionRunStarted,
    types::identifiers::SessionId, types::segments::ActiveSegment, types::session::CancelScope,
    types::session::SessionMeta, types::session::SessionStatus,
    types::worker::commands::ClearWorkerInputTargetsInput,
    types::worker::commands::ConsumeWorkerChildResultInput,
    types::worker::commands::ConsumeWorkerChildResultOutput,
    types::worker::commands::MarkWorkerChildTerminalInput,
    types::worker::commands::UserReplyDeliveryAck, types::worker::state::ChildSignalKind,
    types::worker::state::InputAudience, types::worker::state::ParentResumePolicy,
    types::worker::state::SignalSeverity, types::worker::state::UnreadChildSignal,
    types::worker::state::WorkerChildRef, types::worker::state::WorkerId,
    types::worker::state::WorkerInputTarget, types::worker::state::WorkerProgressSummary,
    types::worker::state::WorkerSignal, types::worker::state::WorkerTerminalResult,
};
use moa_observability::record_turn_event_persist_duration;
use moa_session::PostgresSessionStore;
use moa_wire::session_store::{AppendEventRequest, GetEventsRequest, UpdateStatusRequest};
use moa_wire::turn::{
    ApplySecurityAssessmentRequest, ApplySecurityAssessmentResponse, CancelResponse,
    PendingMessage, QueueMessageRequest, QueueMessageResponse, RegisterCoordinatorInputRequest,
    RunTurnRequest, SessionProgress, SessionProgressRequest, SessionSnapshot, StartTurnRequest,
    StartTurnResponse, TurnOutcome as ExecutionTurnOutcome,
    TurnOutcomeKind as ExecutionTurnOutcomeKind, TurnProgress, TurnTrigger,
};
use restate_sdk::prelude::*;
use tracing::Instrument;

use crate::action_reviews::scheduling::{ActionReviewSchedule, QueuedActionReviewContinuation};
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

mod admission;
mod execution_runs;
mod handlers;
mod liveness;
mod message_admission;
mod narration;
mod persistence;
mod state;

use crate::workflows::errors::moa_error_to_handler_error;
use message_admission::{
    AdmissionLookup, K_MESSAGE_ADMISSIONS, MessageAdmissionState, MessageRouting,
    SessionMessageAdmissions, record_admission_decision, resolve_message_routing,
};
use persistence::{parse_session_key, sync_status};
pub use state::{
    ActiveExecutionRunState, ExecutionProgressSignature, ExecutionSynthesisDedupe,
    ExecutionTemplateAdmissionReplayState, ExecutionTemplateAdmissionResume,
    PendingUserReplyTarget,
};
pub use state::{
    ChildLivenessState, CoordinatorPendingInput, ResumeBudget, ResumeTurnContext, SessionVoState,
};

const K_PENDING_STATE: &str = "pending_state";

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct SessionPendingState {
    active_turn_id: Option<String>,
    pending_messages: VecDeque<PendingMessage>,
    last_outcome: Option<ExecutionTurnOutcome>,
    #[serde(default)]
    turn_waiters: Vec<SessionTurnWaiter>,
    #[serde(default)]
    admission_heartbeat_generation: u64,
    /// Cancellation requested for a turn that has not yet reported its outcome.
    ///
    /// The scope decides queue disposition when the matching `Cancelled` callback
    /// arrives, so it must be remembered between the request and the callback.
    #[serde(default)]
    pending_cancellation: Option<PendingCancellation>,
    /// Monotonic turn-admission generation for this session.
    ///
    /// Advanced by every admitted user message, whether it starts a turn
    /// immediately or is queued. An action review registered under an older
    /// generation has been superseded by newer user work and never continues.
    #[serde(default)]
    turn_generation: u64,
    /// Derived scheduling index for this session's conversational action reviews.
    #[serde(default)]
    action_reviews: ActionReviewSchedule,
}

/// Cancellation requested for one turn, awaiting that turn's outcome callback.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct PendingCancellation {
    /// Turn whose outcome clears this cancellation.
    turn_id: String,
    /// Requested scope, which decides what happens to queued work.
    scope: CancelScope,
}

impl SessionPendingState {
    /// Returns whether a new turn may be admitted right now.
    ///
    /// A whole-task-tree cancellation fences admission for the window between the
    /// request and the cancelled turn's callback. Without the fence a message that
    /// raced the cancellation would start a turn inside a tree that is being torn
    /// down. `CoordinatorOnly` deliberately does not fence: it cancels one turn and
    /// the next queued message is expected to run.
    fn task_tree_cancellation_fenced(&self) -> bool {
        self.pending_cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.scope.cancels_task_tree())
    }

    /// Returns whether this terminal outcome should dispatch the next queued message.
    ///
    /// The queue continues for every outcome that leaves the session able to work:
    /// completed, accepted, failed, and a cancellation that only stopped this
    /// coordinator turn. Stopping on failure would strand acknowledged messages
    /// behind a turn that is never coming back.
    ///
    /// A cancellation continues the queue only when this exact turn was cancelled
    /// with coordinator-only scope. A task-tree cancellation already drained the
    /// queue, and a `Cancelled` outcome with no recorded request — an externally
    /// cancelled invocation — dispatches nothing, because nothing asked for the
    /// queue to continue.
    /// Advances the admission generation for one accepted user message.
    ///
    /// Returns the generation the admitted message runs under. Every action review
    /// registered under an older generation is discarded: the user has since asked
    /// for something newer, and a late approval must not preempt it.
    fn advance_turn_generation(&mut self) -> u64 {
        self.turn_generation = self.turn_generation.saturating_add(1);
        let discarded = self.action_reviews.discard_below(self.turn_generation);
        if discarded > 0 {
            tracing::debug!(
                generation = self.turn_generation,
                discarded,
                "discarded superseded session action reviews on new admission"
            );
        }
        self.turn_generation
    }

    fn dispatches_next_after(&self, outcome: &ExecutionTurnOutcome) -> bool {
        match outcome.kind {
            ExecutionTurnOutcomeKind::Completed
            | ExecutionTurnOutcomeKind::Accepted { .. }
            | ExecutionTurnOutcomeKind::Failed => true,
            ExecutionTurnOutcomeKind::Cancelled => {
                self.pending_cancellation
                    .as_ref()
                    .is_some_and(|cancellation| {
                        cancellation.turn_id == outcome.turn_id
                            && !cancellation.scope.cancels_task_tree()
                    })
            }
        }
    }
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

/// Internal payload for a generation-guarded shared-admission lease heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnAdmissionHeartbeatRequest {
    /// Scheduling generation owned by the currently active turn chain.
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

    /// Registers one coordinator input request so a plain user reply can resolve it.
    async fn register_coordinator_input(
        request: Json<RegisterCoordinatorInputRequest>,
    ) -> Result<(), HandlerError>;

    /// Atomically applies one classified tool output to the coordinator's circuit.
    ///
    /// Single-writer by virtue of being a virtual-object handler, which is what
    /// makes the read-score-write step atomic against concurrent tool results in
    /// the same turn.
    async fn apply_security_assessment(
        request: Json<ApplySecurityAssessmentRequest>,
    ) -> Result<Json<ApplySecurityAssessmentResponse>, HandlerError>;

    /// Publishes and activates one exact user-addressed execution input request.
    async fn execution_input_required(
        input: Json<ExecutionInputRequired>,
    ) -> Result<(), HandlerError>;

    /// Publishes terminal evidence and durably dispatches its one linked synthesis turn.
    async fn execution_terminal(
        delivery: Json<moa_execution::wire::ExecutionTerminalDelivery>,
    ) -> Result<Json<ExecutionSynthesisDispatch>, HandlerError>;

    /// Registers one pending action review this session's coordinator raised.
    ///
    /// Called synchronously by `ActionReviews/request` before the reviewing turn
    /// learns the action is pending. Registering the same review id twice is a
    /// no-op.
    async fn register_action_review(
        registration: Json<moa_core::types::action_policy::ActionReviewRegistration>,
    ) -> Result<(), HandlerError>;

    /// Applies one resolved action review and schedules the coordinator continuation.
    ///
    /// A receipt for an unknown or already-resolved review is a no-op, and a receipt
    /// whose generation was superseded produces no continuation.
    async fn action_review_resolved(
        receipt: Json<moa_core::types::action_policy::ActionReviewReceipt>,
    ) -> Result<(), HandlerError>;

    /// Releases one timed-out or security-stopped review without a model continuation.
    async fn release_action_review(
        release: Json<moa_core::types::action_policy::ActionReviewRelease>,
    ) -> Result<(), HandlerError>;

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

    /// Retracts the reply targets advertised for input requests a child has cleared.
    ///
    /// Called by the child whenever an in-flight `request_input` round-trip dies — wait
    /// timeout, cancellation, terminal turn outcome, or an answered request — so a plain
    /// user reply is never delivered to an awakeable nothing is parked on. Idempotent:
    /// retracting an unknown or already-retracted target is a no-op.
    async fn clear_worker_input_targets(
        input: Json<ClearWorkerInputTargetsInput>,
    ) -> Result<(), HandlerError>;

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

    /// Internal generation-guarded tick that renews the active shared admission lease.
    async fn turn_admission_heartbeat(
        req: Json<TurnAdmissionHeartbeatRequest>,
    ) -> Result<(), HandlerError>;

    /// Internal generation-guarded per-child heartbeat-liveness watchdog tick.
    async fn check_child_liveness(req: Json<CheckChildLivenessRequest>)
    -> Result<(), HandlerError>;
}

/// Concrete `Session` virtual object implementation.
pub struct SessionImpl {
    session_store: Arc<dyn SessionStore>,
    session_store_backend: Arc<PostgresSessionStore>,
    /// Control-plane pool the keyed execution-template admission replays against.
    admission_pool: sqlx::PgPool,
    /// Runtime configuration Session-owned template planning compiles against.
    config: Arc<MoaConfig>,
    session_limits: SessionLimitsConfig,
    turn_admission: admission::TurnAdmission,
}

impl SessionImpl {
    /// Creates a session object with its persistence and scheduling dependencies.
    ///
    /// `admission_pool` and `config` are injected rather than read from the
    /// installed process context: the admission replay transactions and the
    /// template planner are the only reasons this object needed the whole
    /// dependency graph, and a constructor parameter is what makes those two
    /// needs visible at the composition root instead of at the call site.
    #[must_use]
    pub fn new(
        session_store: Arc<PostgresSessionStore>,
        admission_pool: sqlx::PgPool,
        config: Arc<MoaConfig>,
        session_limits: SessionLimitsConfig,
        runtime_cache: Arc<dyn moa_core::traits::RuntimeCacheStore>,
    ) -> Self {
        let turn_admission = admission::TurnAdmission::new(runtime_cache, &session_limits);
        Self {
            session_store: session_store.clone(),
            session_store_backend: session_store,
            admission_pool,
            config,
            session_limits,
            turn_admission,
        }
    }
}

async fn load_pending_state<R: VoReader>(reader: &R) -> Result<SessionPendingState, HandlerError> {
    Ok(reader.get_json(K_PENDING_STATE).await?.unwrap_or_default())
}

fn persist_pending_state(ctx: &ObjectContext<'_>, state: &SessionPendingState) {
    ctx.set(K_PENDING_STATE, Json::from(state.clone()));
}

/// Loads the session's admission projection, the fence every message passes through.
async fn load_message_admissions<R: VoReader>(
    reader: &R,
) -> Result<SessionMessageAdmissions, HandlerError> {
    Ok(reader
        .get_json(K_MESSAGE_ADMISSIONS)
        .await?
        .unwrap_or_default())
}

/// Persists the admission projection and publishes its bounded-cache observability.
fn persist_message_admissions(
    ctx: &ObjectContext<'_>,
    admissions: &SessionMessageAdmissions,
    evicted: usize,
) {
    ctx.set(K_MESSAGE_ADMISSIONS, Json::from(admissions.clone()));
    message_admission::record_admission_evictions(evicted, admissions.len());
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

fn arm_turn_admission_heartbeat(
    ctx: &ObjectContext<'_>,
    pending_state: &mut SessionPendingState,
    turn_admission: &admission::TurnAdmission,
) {
    pending_state.admission_heartbeat_generation = pending_state
        .admission_heartbeat_generation
        .saturating_add(1);
    schedule_turn_admission_heartbeat(
        ctx,
        pending_state.admission_heartbeat_generation,
        turn_admission,
    );
}

fn schedule_turn_admission_heartbeat(
    ctx: &ObjectContext<'_>,
    generation: u64,
    turn_admission: &admission::TurnAdmission,
) {
    let request = ctx
        .object_client::<SessionClient>(ctx.key().to_string())
        .turn_admission_heartbeat(Json::from(TurnAdmissionHeartbeatRequest { generation }))
        .idempotency_key(format!(
            "turn-admission-heartbeat:{generation}:{}",
            ctx.invocation_id()
        ));
    crate::restate_identity::replay_safe_request(request).send_after(Duration::from_millis(
        turn_admission.heartbeat_interval_ms(),
    ));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(turn_id: &str, kind: ExecutionTurnOutcomeKind) -> ExecutionTurnOutcome {
        ExecutionTurnOutcome {
            turn_id: turn_id.to_string(),
            kind,
            message: "outcome".to_string(),
        }
    }

    fn cancelled_state(turn_id: &str, scope: CancelScope) -> SessionPendingState {
        SessionPendingState {
            active_turn_id: Some(turn_id.to_string()),
            pending_cancellation: Some(PendingCancellation {
                turn_id: turn_id.to_string(),
                scope,
            }),
            ..SessionPendingState::default()
        }
    }

    #[test]
    fn failed_turn_still_dispatches_the_next_queued_message() {
        // Pins: acknowledged work stays reachable after a turn dies. Restricting the
        // queue pop to Completed/Accepted strands every message queued behind a
        // failed turn forever, because nothing else ever pops the queue.
        let state = SessionPendingState::default();

        assert!(state.dispatches_next_after(&outcome("turn-1", ExecutionTurnOutcomeKind::Failed)));
        assert!(
            state.dispatches_next_after(&outcome("turn-1", ExecutionTurnOutcomeKind::Completed))
        );
        assert!(state.dispatches_next_after(&outcome(
            "turn-1",
            ExecutionTurnOutcomeKind::Accepted {
                execution_run_uid: uuid::Uuid::from_u128(7),
            }
        )));
    }

    #[test]
    fn only_coordinator_only_cancellation_continues_the_queue() {
        // Pins: cancellation scope decides queue disposition. A coordinator-only
        // cancel stops one turn and the queue continues; a task-tree cancel tore the
        // tree down and already drained the queue, so it must dispatch nothing.
        let coordinator_only = cancelled_state("turn-1", CancelScope::CoordinatorOnly);
        let task_tree = cancelled_state("turn-1", CancelScope::TaskTree);
        let cancelled = outcome("turn-1", ExecutionTurnOutcomeKind::Cancelled);

        assert!(coordinator_only.dispatches_next_after(&cancelled));
        assert!(!task_tree.dispatches_next_after(&cancelled));
    }

    #[test]
    fn cancellation_without_a_matching_request_dispatches_nothing() {
        // Pins: an externally cancelled invocation, or a cancellation recorded for a
        // different turn, is not evidence that the queue should continue. Treating an
        // unexplained Cancelled outcome as continuable would start queued work inside
        // a tree that may already be torn down.
        let cancelled = outcome("turn-1", ExecutionTurnOutcomeKind::Cancelled);
        let unrequested = SessionPendingState::default();
        let other_turn = cancelled_state("turn-2", CancelScope::CoordinatorOnly);

        assert!(!unrequested.dispatches_next_after(&cancelled));
        assert!(!other_turn.dispatches_next_after(&cancelled));
    }

    #[test]
    fn a_resolved_continuation_runs_before_the_ordinary_queue_at_its_own_generation() {
        // Pins: a same-generation continuation is the tail of work the session already
        // acknowledged, so it goes ahead of ordinary FIFO. It stays eligible only while
        // it is current: the moment a newer user message is admitted, the older review's
        // continuation is stranded and the queued user message runs instead.
        use moa_core::types::action_policy::{
            ActionReviewContinuation, ActionReviewOutcome, ActionReviewOwner, ActionReviewReceipt,
        };
        use moa_core::types::identifiers::{SessionId, ToolCallId};

        let session_id = SessionId::new();
        let review_id = uuid::Uuid::from_u128(0x13_2001);
        let mut state = SessionPendingState::default();
        let generation = state.advance_turn_generation();
        assert_eq!(generation, 1);
        assert!(
            state
                .action_reviews
                .register(review_id, "turn-origin".to_string(), generation)
        );

        let registered = state
            .action_reviews
            .resolve(review_id)
            .expect("registered review resolves once");
        assert!(
            state
                .action_reviews
                .enqueue(QueuedActionReviewContinuation {
                    continuation: ActionReviewContinuation {
                        receipt: ActionReviewReceipt {
                            review_id,
                            owner: ActionReviewOwner::Coordinator {
                                session_id,
                                turn_id: "turn-origin".to_string(),
                                generation,
                            },
                            tool_name: "bash".to_string(),
                            executed_tool_call_id: Some(ToolCallId::new()),
                            outcome: ActionReviewOutcome::Cleared(
                                moa_core::types::action_policy::ToolTerminalFact::Result(
                                    moa_core::types::action_policy::ToolResultSecurityMetadata {
                                        success: true,
                                        assessment:
                                            moa_core::types::security::ToolOutputAssessment::safe(),
                                        capability:
                                            moa_core::types::security::ToolCapabilityId::builtin(
                                                "bash",
                                            ),
                                    },
                                ),
                            ),
                        },
                    },
                    turn_id: "turn-continuation".to_string(),
                    generation: registered.generation,
                    ordinal: registered.ordinal,
                })
        );
        assert!(state.action_reviews.has_queued(generation));

        let newer = state.advance_turn_generation();
        assert_eq!(newer, 2);
        assert!(
            state.action_reviews.take_next(newer).is_none(),
            "a superseded continuation must not run ahead of the newer user message"
        );
        assert!(!state.action_reviews.has_queued(generation));
    }

    #[test]
    fn only_task_tree_cancellation_fences_admission() {
        // Pins: the admission fence is scoped to a task-tree teardown. Fencing on a
        // coordinator-only cancel would refuse the very next queued message, and not
        // fencing on a task-tree cancel would admit a turn into a tree whose children
        // and execution runs are already being cancelled.
        assert!(cancelled_state("turn-1", CancelScope::TaskTree).task_tree_cancellation_fenced());
        assert!(
            !cancelled_state("turn-1", CancelScope::CoordinatorOnly)
                .task_tree_cancellation_fenced()
        );
        assert!(!SessionPendingState::default().task_tree_cancellation_fenced());
    }
}
