//! Restate handlers for the Session VO.

use super::execution_runs::{
    accept_execution_input_required, accept_execution_progress, accept_execution_run_started,
    accept_execution_terminal, admit_execution_template, dispatch_execution_run,
};
use super::state::resume::signal_kind_is_resume_eligible;
use super::*;
use crate::handlers::authz_shim::require_identity;
use crate::services::tool_executor::{ReleaseSessionHandsRequest, ToolExecutorClient};

mod authz;
mod execution_bridge;
mod lifecycle;
mod progress;
mod reviews;
mod turns;
mod workers;

use turns::*;

use authz::{require_session_participant, require_shared_session_participant};

impl Session for SessionImpl {
    #[tracing::instrument(skip(self, ctx, meta))]
    // SAFETY: internal SessionStore initialization only; mirrors persisted session metadata into VO hot state.
    async fn set_meta(
        &self,
        ctx: ObjectContext<'_>,
        meta: Json<SessionMeta>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_set_meta(ctx, meta).await
    }

    #[tracing::instrument(skip(self, ctx, scope))]
    async fn cancel(
        &self,
        ctx: ObjectContext<'_>,
        scope: Json<CancelScope>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_cancel(ctx, scope).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionStatus>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_status(ctx).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn start_turn(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<StartTurnRequest>,
    ) -> Result<Json<StartTurnResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_start_turn(ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn record_turn_outcome(
        &self,
        ctx: ObjectContext<'_>,
        outcome: Json<ExecutionTurnOutcome>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_record_turn_outcome(ctx, outcome).await
    }

    #[tracing::instrument(skip(self, ctx, registration))]
    // SAFETY: internal control-plane write from `ActionReviews/request`, which runs
    // only after this session admitted the caller and its own coordinator turn issued
    // the reviewed tool call. It records the review id on this session's own VO state
    // and returns no caller-owned data.
    async fn register_action_review(
        &self,
        ctx: ObjectContext<'_>,
        registration: Json<moa_core::types::action_policy::ActionReviewRegistration>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_register_action_review(ctx, registration).await
    }

    #[tracing::instrument(skip(self, ctx, receipt))]
    // SAFETY: internal control-plane write from `ActionReviews/decide`, which
    // authorizes the deciding tenant admin before resolving. It writes only this
    // session's own VO state and event log.
    async fn action_review_resolved(
        &self,
        ctx: ObjectContext<'_>,
        receipt: Json<moa_core::types::action_policy::ActionReviewReceipt>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_action_review_resolved(ctx, receipt).await
    }

    #[tracing::instrument(skip(self, ctx, release))]
    // SAFETY: internal control-plane release from the action-review reaper or
    // security circuit. It removes only the matching review registration owned by
    // this session and does not create a continuation for that review.
    async fn release_action_review(
        &self,
        ctx: ObjectContext<'_>,
        release: Json<moa_core::types::action_policy::ActionReviewRelease>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_release_action_review(ctx, release).await
    }

    #[tracing::instrument(skip(self, ctx, delivery))]
    // SAFETY: internal TurnExecution delivery after Execution/start has committed the run.
    async fn execution_run_started(
        &self,
        ctx: ObjectContext<'_>,
        delivery: Json<ExecutionRunStartedDelivery>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_execution_run_started(ctx, delivery).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn admit_execution_template(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<moa_execution::wire::ExecutionTemplateAdmissionRequest>,
    ) -> Result<Json<moa_execution::wire::ExecutionTemplateAdmissionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_admit_execution_template(ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx, progress))]
    // SAFETY: internal ExecutionRun delivery after execution persistence committed the aggregate.
    async fn execution_progress(
        &self,
        ctx: ObjectContext<'_>,
        progress: Json<ExecutionProgress>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_execution_progress(ctx, progress).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal registration by this session's own running turn; it stores an
    // awakeable id and a pending reply target and reads no caller-owned data back.
    async fn register_coordinator_input(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<RegisterCoordinatorInputRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_register_coordinator_input(ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal cleanup by the workflow that owns this exact registration;
    // the generation and workflow fences prevent it from retracting newer work.
    async fn clear_coordinator_input(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<ClearCoordinatorInputRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_clear_coordinator_input(ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: internal workflow delivery of an assessment the router already produced;
    // it reads no caller-owned data back and returns only closed-vocabulary state.
    async fn apply_security_assessment(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<ApplySecurityAssessmentRequest>,
    ) -> Result<Json<ApplySecurityAssessmentResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_apply_security_assessment(ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: internal ExecutionRun delivery after a task persisted exact user-audience input.
    async fn execution_input_required(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ExecutionInputRequired>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_execution_input_required(ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx, delivery))]
    // SAFETY: internal ExecutionRun delivery after the terminal run and task projection are durable.
    async fn execution_terminal(
        &self,
        ctx: ObjectContext<'_>,
        delivery: Json<moa_execution::wire::ExecutionTerminalDelivery>,
    ) -> Result<Json<ExecutionSynthesisDispatch>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_execution_terminal(ctx, delivery).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only by authorized workflows after the turn has been admitted by Session.
    async fn attach_turn_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<AttachSessionTurnWaiterInput>,
    ) -> Result<Json<AttachSessionTurnWaiterOutput>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_attach_turn_waiter(ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only by authorized workflows after the turn wait deadline expires.
    async fn remove_turn_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<RemoveSessionTurnWaiterInput>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_remove_turn_waiter(ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn request_cancel(
        &self,
        ctx: ObjectContext<'_>,
        reason: Json<String>,
    ) -> Result<Json<CancelResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_request_cancel(ctx, reason).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn snapshot(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<SessionSnapshot>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_snapshot(ctx).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn progress(
        &self,
        ctx: SharedObjectContext<'_>,
        request: Json<SessionProgressRequest>,
    ) -> Result<Json<SessionProgress>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_progress(ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx, child))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn register_child(
        &self,
        ctx: ObjectContext<'_>,
        child: Json<WorkerChildRef>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_register_child(ctx, child).await
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn remove_child(
        &self,
        ctx: ObjectContext<'_>,
        worker_id: String,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_remove_child(ctx, worker_id).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: internal retraction from this session's own child; it only removes reply
    // targets this session advertised for that child and reads no caller-owned data.
    async fn clear_worker_input_targets(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ClearWorkerInputTargetsInput>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_clear_worker_input_targets(ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only from Worker terminal delivery after parent dispatch authz has already checked.
    async fn mark_child_terminal(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<MarkWorkerChildTerminalInput>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_mark_child_terminal(ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn consume_child_result(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<ConsumeWorkerChildResultInput>,
    ) -> Result<Json<ConsumeWorkerChildResultOutput>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_consume_child_result(ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx))]
    // SAFETY: called only from TurnExecution after session participant authz has already checked.
    async fn child_refs(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Vec<WorkerChildRef>>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_child_refs(ctx).await
    }

    #[tracing::instrument(skip(self, ctx, signal))]
    // SAFETY: internal child→parent control-plane write. The signaling Worker VO is
    // part of this session's task tree — it was reserved/spawned under the owning
    // session's participant authz, exactly like register_child/mark_child_terminal. The
    // handler only appends idempotently to this session's own event log and updates the
    // session's compact VO state; it reads no caller-owned data back to the caller. This
    // mirrors the established internal VO→VO write pattern on Session and adds no broad
    // authz bypass.
    async fn record_child_signal(
        &self,
        ctx: ObjectContext<'_>,
        signal: Json<WorkerSignal>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_record_child_signal(ctx, signal).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_destroy(ctx).await
    }

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-tick scheduled by this Session VO; it
    // reads only its own VO state and the bounded child/turn fan-in, and forwards the
    // session's own owning-actor identity to the detached narration job, which re-checks
    // Session participant authz on its gated progress read.
    async fn narration_tick(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<NarrationTickRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_narration_tick(ctx, req).await
    }

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-call; it only renews the shared
    // admission lease for this Session while a coordinator turn remains active.
    async fn turn_admission_heartbeat(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<TurnAdmissionHeartbeatRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_turn_admission_heartbeat(ctx, req).await
    }

    #[tracing::instrument(skip(self, ctx, req))]
    // SAFETY: internal generation-guarded self-tick scheduled by this Session VO for its
    // own active children. It reads only its own VO state plus the child's compact
    // progress summary (the same informational fan-in `progress` already performs), and
    // any stale signal it raises is recorded through `record_child_signal`, which carries
    // the established internal child→parent control-plane authz justification.
    async fn check_child_liveness(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<CheckChildLivenessRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        self.handle_check_child_liveness(ctx, req).await
    }
}

/// Installs the coordinator circuit owner as part of admitting a turn.
fn activate_coordinator_security_owner(state: &mut SessionVoState, turn_id: &str, generation: u64) {
    state.security_circuit.adopt_owner(
        &moa_core::types::security::SecurityCircuitOwner::Coordinator {
            turn_id: turn_id.to_string(),
            generation,
        },
    );
}

#[cfg(test)]
mod tests;
