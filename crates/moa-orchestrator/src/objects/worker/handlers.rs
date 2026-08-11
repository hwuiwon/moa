//! Thin Restate handler binding for the Worker VO.

use super::state::{ClaimedHistoryEntry, MAX_CLEANUP_RELEASE_ATTEMPTS, WorkerHistoryEntry};
use super::*;
use crate::action_reviews::scheduling::QueuedActionReviewContinuation;
use crate::handlers::authz_shim::require_identity;
use crate::objects::session::SessionClient;
use crate::services::tool_executor::{ReleaseWorkerHandsRequest, ToolExecutorClient};
use crate::workflows::worker_turn_execution::WorkerTurnExecutionClient;
use moa_core::types::worker::commands::ClearWorkerInputTargetsInput;
use moa_security::{canary_system_message, new_canary_token};
use moa_wire::turn::{RunWorkerTurnRequest, TurnOutcomeKind};

mod admission;
mod cleanup;
mod coordination;
mod turn;

use admission::{
    WorkerTurnDispatch, activate_worker_security_owner, generate_turn_id, required_parent_session,
    start_worker_turn_execution,
};
use cleanup::{retract_session_input_targets, schedule_cleanup_self_call};

impl Worker for WorkerImpl {
    #[tracing::instrument(skip(self, ctx, msg))]
    async fn post_message(
        &self,
        ctx: ObjectContext<'_>,
        msg: Json<WorkerMessage>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::post_message(self, ctx, msg).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn provide_input(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<WorkerProvideInputRequest>,
    ) -> Result<Json<UserReplyDeliveryAck>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::provide_input(self, ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn status(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkerStatus>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::status(self, ctx).await
    }

    // SAFETY: informational fan-in read; mirrors `status` which exposes the same
    // VO projection without additional authz (the calling coordinator is already
    // authorized for the owning session before it fans in).
    #[tracing::instrument(skip(self, ctx))]
    async fn progress_summary(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<WorkerProgressSummary>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::progress_summary(self, ctx).await
    }

    // SAFETY: internal liveness write invoked only by the child's own turn workflow at
    // the progress cadence; it updates this Worker's state and owns its self-deadline.
    #[tracing::instrument(skip(self, ctx, at))]
    async fn record_heartbeat(
        &self,
        ctx: ObjectContext<'_>,
        at: Json<DateTime<Utc>>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::record_heartbeat(self, ctx, at).await
    }

    // SAFETY: internal generation-guarded self-call scheduled by this Worker VO. It
    // reads only Worker-owned state and sends one joined control signal to its already
    // established parent Session when the exact latest heartbeat becomes stale.
    #[tracing::instrument(skip(self, ctx, request))]
    async fn liveness_deadline(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<WorkerLivenessDeadlineRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::liveness_deadline(self, ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn result(
        &self,
        ctx: SharedObjectContext<'_>,
    ) -> Result<Json<Option<WorkerResult>>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::result(self, ctx).await
    }

    #[tracing::instrument(skip(self, ctx, reason))]
    async fn cancel(&self, ctx: ObjectContext<'_>, reason: String) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::cancel(self, ctx, reason).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn prepare_turn(
        &self,
        ctx: ObjectContext<'_>,
    ) -> Result<Json<WorkerTurnPreparation>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::prepare_turn(self, ctx).await
    }

    #[tracing::instrument(skip(self, ctx, response))]
    async fn record_response(
        &self,
        ctx: ObjectContext<'_>,
        response: Json<WorkerTurnResponseRecord>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::record_response(self, ctx, response).await
    }

    #[tracing::instrument(skip(self, ctx, record))]
    async fn record_tool_result(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<WorkerToolRecord>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::record_tool_result(self, ctx, record).await
    }

    #[tracing::instrument(skip(self, ctx, record))]
    async fn record_denied_tool(
        &self,
        ctx: ObjectContext<'_>,
        record: Json<WorkerToolRecord>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::record_denied_tool(self, ctx, record).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn apply_security_assessment(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<moa_wire::turn::ApplySecurityAssessmentRequest>,
    ) -> Result<Json<moa_wire::turn::ApplySecurityAssessmentResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::apply_security_assessment(self, ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn apply_turn_outcome(
        &self,
        ctx: ObjectContext<'_>,
        outcome: Json<WorkerTurnOutcomeRecord>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::apply_turn_outcome(self, ctx, outcome).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn attach_result_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<AttachWorkerResultWaiterInput>,
    ) -> Result<Json<AttachWorkerResultWaiterOutput>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::attach_result_waiter(self, ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn remove_result_waiter(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<RemoveWorkerResultWaiterInput>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::remove_result_waiter(self, ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx, input))]
    async fn register_input_request(
        &self,
        ctx: ObjectContext<'_>,
        input: Json<WorkerPendingInput>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::register_input_request(self, ctx, input).await
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn clear_input_request(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<WorkerClearInputRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::clear_input_request(self, ctx, request).await
    }

    #[tracing::instrument(skip(self, ctx, outcome))]
    async fn record_turn_outcome(
        &self,
        ctx: ObjectContext<'_>,
        outcome: Json<moa_wire::turn::TurnOutcome>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::record_turn_outcome(self, ctx, outcome).await
    }

    #[tracing::instrument(skip(self, ctx, registration))]
    async fn register_action_review(
        &self,
        ctx: ObjectContext<'_>,
        registration: Json<ActionReviewRegistration>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::register_action_review(self, ctx, registration).await
    }

    #[tracing::instrument(skip(self, ctx, receipt))]
    async fn action_review_resolved(
        &self,
        ctx: ObjectContext<'_>,
        receipt: Json<ActionReviewReceipt>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::action_review_resolved(self, ctx, receipt).await
    }

    #[tracing::instrument(skip(self, ctx, release))]
    async fn release_action_review(
        &self,
        ctx: ObjectContext<'_>,
        release: Json<moa_core::types::action_policy::ActionReviewRelease>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::release_action_review(self, ctx, release).await
    }

    #[tracing::instrument(skip(self, ctx))]
    async fn destroy(&self, ctx: ObjectContext<'_>) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::destroy(self, ctx).await
    }

    // SAFETY: internal generation-guarded self-call scheduled by this Worker VO's own
    // terminal-delivery path. It touches only this child and its established parent fan-out.
    #[tracing::instrument(skip(self, ctx, req))]
    async fn cleanup(
        &self,
        ctx: ObjectContext<'_>,
        req: Json<CleanupRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        WorkerImpl::cleanup(self, ctx, req).await
    }
}

#[cfg(test)]
use cleanup::{CleanupDecision, decide_cleanup, release_worker_hands_request};
#[cfg(test)]
use turn::JournaledWorkerToolCatalog;

#[cfg(test)]
mod tests;
