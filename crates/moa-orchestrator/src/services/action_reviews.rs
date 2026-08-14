//! Tenant-admin action review queue and decision service.

use chrono::{DateTime, Utc};
use moa_authz_schema::Relation;
use moa_core::{
    events::Event, types::action_policy::ActionClass, types::action_policy::ActionEnvelope,
    types::action_policy::ActionReviewOutcome, types::action_policy::ActionReviewOwner,
    types::action_policy::ActionReviewPreview, types::action_policy::ActionReviewReceipt,
    types::action_policy::ActionReviewRegistration, types::action_policy::ActionReviewStatus,
    types::action_policy::ToolResultSecurityMetadata, types::action_policy::ToolTerminalFact,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::identifiers::ToolCallId, types::security::SecurityCircuitStage,
    types::tools::SecuredToolOutput, types::tools::ToolCallRequest,
};
use moa_observability::propagation::ValidatedTraceContext;
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_observability::{
    record_action_review_decision, record_action_review_requested, record_approval_wait,
};
use moa_wire::session_store::AppendEventRequest;
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;

use crate::action_reviews::app as action_review_app;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::AuthzEnforcer;
use crate::objects::session::SessionClient;
use crate::objects::worker::WorkerClient;
use crate::services::action_review_dispatcher::{
    ActionReviewDispatcherClient, DispatchActionReviewsRequest,
};
use crate::services::durable_timeout::{DurableTimeoutRequest, schedule_durable_timeout};
use crate::services::session_store::RestateSessionStoreClient;
use crate::services::tool_executor::{
    ExecutionToolCallOrigin, ExecutionToolCallOutcome, ExecutionToolCallPhase,
    ExecutionToolCallRequest, ToolExecutorClient,
};
use crate::workflows::errors::moa_error_to_handler_error;
use moa_core::traits::SessionEventLookupStore;
use moa_execution::wire::ExecutionActionReviewResolution;

/// Summary returned for one tenant action review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReviewSummary {
    /// Review identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Original tool call identifier.
    pub tool_call_id: ToolCallId,
    /// Tool name.
    pub tool_name: String,
    /// Action class.
    pub action_class: ActionClass,
    /// Risk level.
    pub risk_level: moa_core::types::action_policy::RiskLevel,
    /// Concise input summary.
    pub input_summary: String,
    /// Durable action envelope.
    pub envelope: ActionEnvelope,
    /// Human-readable preview.
    pub preview: ActionReviewPreview,
    /// Review status.
    pub status: ActionReviewStatus,
    /// User that requested the action.
    pub requested_by: String,
    /// User that decided the action, when present.
    pub decided_by: Option<String>,
    /// Denial reason, when present.
    pub deny_reason: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Exact durable timeout persisted with the review.
    pub expires_at: DateTime<Utc>,
    /// Decision timestamp, when present.
    pub decided_at: Option<DateTime<Utc>>,
}

/// Request payload for `ActionReviews/request`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestActionReview {
    /// Durable policy-facing action envelope.
    pub envelope: ActionEnvelope,
    /// Human-readable preview rendered to admins.
    pub preview: ActionReviewPreview,
    /// Stored tool request to execute if the review is cleared.
    pub tool_request: ToolCallRequest,
}

/// Request payload for `ActionReviews/list_pending`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListActionReviewsRequest {
    /// Tenant whose pending action reviews should be listed.
    pub tenant_id: TenantId,
}

/// Request payload for `ActionReviews/decide`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecideActionReviewRequest {
    /// Tenant that owns the review.
    pub tenant_id: TenantId,
    /// Review identifier.
    pub review_id: Uuid,
    /// Decision kind.
    pub decision: ActionReviewDecisionKind,
    /// Optional denial reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Internal request to settle an execution-owned review before its owner terminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettleExecutionActionReviewRequest {
    /// Tenant that owns the review row.
    pub tenant_id: TenantId,
    /// Stable review identifier, equal to the governed execution tool-call id.
    pub review_id: Uuid,
    /// Exact task or compensation owner expected in the durable envelope.
    pub owner: ActionReviewOwner,
}

/// Exact execution-owned review whose bounded owner durably parked before acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcknowledgeExecutionActionReviewRequest {
    /// Tenant that owns the review row.
    pub tenant_id: TenantId,
    /// Stable review identifier returned by durable review admission.
    pub review_id: Uuid,
    /// Exact task or compensation owner parked under its generation fence.
    pub owner: ActionReviewOwner,
}

/// Durable settlement chosen while holding the action-review row lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionActionReviewSettlement {
    /// The review was still unclaimed and is now terminal, so no tool may dispatch.
    Revoked,
    /// A decision already claimed the reviewed effect, so the owner must join its resolution.
    JoinRequired,
}

/// Wire decision kind for `ActionReviews/decide`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionReviewDecisionKind {
    /// Clear the action for execution.
    Cleared,
    /// Deny the action.
    Denied,
}

/// Restate service surface for tenant-admin action reviews.
#[restate_sdk::service]
#[name = "ActionReviews"]
pub trait ActionReviews {
    /// Queue one action for tenant-admin review.
    async fn request(
        request: Json<RequestActionReview>,
    ) -> Result<Json<ActionReviewSummary>, HandlerError>;

    /// List pending tenant action reviews.
    async fn list_pending(
        request: Json<ListActionReviewsRequest>,
    ) -> Result<Json<Vec<ActionReviewSummary>>, HandlerError>;

    /// Decide one tenant action review.
    async fn decide(request: Json<DecideActionReviewRequest>) -> Result<(), HandlerError>;

    /// Revoke an unclaimed execution review or require its owner to join claimed work.
    async fn settle_execution_owner_review(
        request: Json<SettleExecutionActionReviewRequest>,
    ) -> Result<Json<ExecutionActionReviewSettlement>, HandlerError>;

    /// Makes an execution review decision-ready only after its owner park CAS committed.
    async fn acknowledge_execution_owner_review(
        request: Json<AcknowledgeExecutionActionReviewRequest>,
    ) -> Result<(), HandlerError>;
}

/// Settles an execution-owned action review against the durable row state.
///
/// This public adapter exists so DB-backed tests exercise the same transaction
/// used by the Restate handler rather than restating its SQL.
pub async fn settle_execution_action_review(
    pool: sqlx::PgPool,
    request: SettleExecutionActionReviewRequest,
) -> Result<ExecutionActionReviewSettlement, HandlerError> {
    action_review_app::settle_execution_owner_review(pool, request).await
}

/// Concrete action-review service implementation.
#[derive(Clone)]
pub struct ActionReviewsImpl {
    pool: sqlx::PgPool,
    session_events: Arc<dyn SessionEventLookupStore>,
    review_timeout_secs: i64,
    authz: AuthzEnforcer,
}

impl ActionReviewsImpl {
    /// Creates the action-review adapter with its persistence dependencies.
    ///
    /// `review_timeout_secs` sets how long a queued review may stay pending
    /// before the reaper fails it closed.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        session_events: Arc<dyn SessionEventLookupStore>,
        review_timeout_secs: i64,
        authz: AuthzEnforcer,
    ) -> Self {
        Self {
            pool,
            session_events,
            review_timeout_secs,
            authz,
        }
    }
}

impl ActionReviews for ActionReviewsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: request is an internal workflow call after the owning session or worker has already checked participant authorization before tool execution.
    async fn request(
        &self,
        ctx: Context<'_>,
        request: Json<RequestActionReview>,
    ) -> Result<Json<ActionReviewSummary>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionReviews", "request");
        let mut request = request.into_inner();
        let execution_task_trace_context = (request.envelope.owner.execution_origin().is_some()
            || request.envelope.owner.compensation_origin().is_some())
        .then(|| incoming_trace_context(&ctx))
        .flatten();
        action_review_app::prepare_request(&mut request)?;
        let event = action_review_app::requested_event(&request);
        let pool = self.pool.clone();
        let owner = request.envelope.owner.clone();
        let session_id = owner.session_id();
        let action_class = request.envelope.action_class;
        let review_timeout_secs = self.review_timeout_secs;

        let stored = ctx
            .run(|| async move {
                action_review_app::request_review(
                    pool,
                    request,
                    review_timeout_secs,
                    execution_task_trace_context,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_request")
            .await?
            .into_inner();
        let owner_needs_registration = !stored.owner_registered
            && stored.summary.status == ActionReviewStatus::Pending
            && requires_conversational_registration(&owner);
        if owner_needs_registration {
            // The database row is deliberately not decision-ready until the typed
            // owner has durably acknowledged registration. A crash between these
            // two steps safely retries the idempotent owner call.
            register_conversational_review(&ctx, &owner, stored.summary.id).await?;
        }
        crate::restate_identity::replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id,
                    event,
                    dedupe_key: Some(
                        moa_core::types::action_policy::action_review_requested_dedupe_key(
                            stored.summary.id,
                        ),
                    ),
                })),
        )
        .call()
        .await?;
        if owner_needs_registration {
            let pool = self.pool.clone();
            let storage_partition_id = storage_partition_id(stored.summary.tenant_id);
            let review_id = stored.summary.id;
            ctx.run(|| async move {
                action_review_app::mark_owner_registered(
                    pool,
                    storage_partition_id,
                    review_id,
                    None,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_mark_owner_registered")
            .await?;
        }
        if stored.summary.status == ActionReviewStatus::Pending {
            let timeout_secs = u64::try_from(review_timeout_secs).map_err(|error| {
                TerminalError::new(format!("action review timeout is invalid: {error}"))
            })?;
            schedule_durable_timeout(
                &ctx,
                DurableTimeoutRequest::action_review(
                    stored.summary.tenant_id,
                    stored.summary.id,
                    owner,
                ),
                Duration::from_secs(timeout_secs),
            );
        }
        if stored.newly_inserted {
            record_action_review_requested(
                moa_core::types::action_policy::ActionPolicyEffect::AdminReview,
                action_class,
            );
        }
        Ok(Json::from(stored.summary))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_pending(
        &self,
        ctx: Context<'_>,
        request: Json<ListActionReviewsRequest>,
    ) -> Result<Json<Vec<ActionReviewSummary>>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionReviews", "list_pending");
        let request = request.into_inner();
        self.authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Admin)
            .await?;
        let pool = self.pool.clone();
        let storage_partition_id = storage_partition_id(request.tenant_id);

        Ok(ctx
            .run(|| async move {
                action_review_app::list_pending_reviews(pool, storage_partition_id)
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_list_pending")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn decide(
        &self,
        ctx: Context<'_>,
        request: Json<DecideActionReviewRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionReviews", "decide");
        let resolution_trace_context = current_trace_context();
        let request = request.into_inner();
        let tenant_id = request.tenant_id;
        let review_id = request.review_id;
        let identity = self
            .authz
            .authorize_tenant(&ctx, request.tenant_id, Relation::Admin)
            .await?;
        let pool = self.pool.clone();
        let decision_trace_context = resolution_trace_context.clone();
        let mut decided = ctx
            .run(|| async move {
                action_review_app::decide_review(
                    pool,
                    request,
                    identity.id.to_string(),
                    decision_trace_context,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_decide")
            .await?
            .into_inner();
        let owns_execution_attempt = !decided.owner.is_conversational();

        if let Some(execution_request) = execution_review_request(
            &decided.owner,
            decided.review_id,
            decided.tool_request.as_ref(),
        ) {
            let execution = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ToolExecutorClient>()
                    .execute_execution(Json::from(execution_request)),
            )
            .call()
            .await;
            let resolution = match execution {
                Ok(Json(outcome)) => execution_review_resolution(outcome)?,
                Err(error) => return Err(error.into()),
            };
            let pool = self.pool.clone();
            decided = ctx
                .run(|| async move {
                    action_review_app::finalize_execution_review(
                        pool,
                        tenant_id,
                        review_id,
                        resolution,
                        resolution_trace_context,
                    )
                    .await
                    .map(Json::from)
                })
                .name("action_reviews_finalize_execution_review")
                .await?
                .into_inner();
        }

        crate::restate_identity::replay_safe_request(
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest {
                    session_id: decided.owner.session_id(),
                    event: Event::ActionReviewDecided {
                        review_id: decided.review_id,
                        decision: decided.decision.clone(),
                        decided_by: decided.decided_by.clone(),
                        decided_at: decided.decided_at,
                    },
                    dedupe_key: Some(
                        moa_core::types::action_policy::action_review_decided_dedupe_key(
                            decided.review_id,
                        ),
                    ),
                })),
        )
        .call()
        .await?;
        if decided.newly_decided {
            record_action_review_decision(decided.status, decided.action_class);
            let wait = (decided.decided_at - decided.created_at)
                .to_std()
                .unwrap_or_default();
            record_approval_wait(decided.action_class, wait);
        }

        // Executed exactly once per cleared review. `None` means either a denial or a
        // replay of an already-dispatched clear; both fall through to receipt
        // reconstruction from durable facts below.
        let mut executed_output: Option<SecuredToolOutput> = None;
        if let Some(tool_request) = decided.tool_request.as_ref() {
            if let Some(execution_request) =
                execution_review_request(&decided.owner, decided.review_id, Some(tool_request))
            {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute_execution(Json::from(execution_request)),
                )
                .call()
                .await?;
            } else if prior_tool_terminal_fact(
                &ctx,
                &decided,
                tool_request.tool_call_id,
                &self.session_events,
            )
            .await?
            .is_none()
            {
                let execution = crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute(Json::from(tool_request.clone())),
                )
                .call()
                .await;
                match execution {
                    Ok(output) => executed_output = Some(output.into_inner()),
                    // A pre-durability infrastructure failure leaves nothing to report:
                    // the invocation retries and no owner callback is sent. A failure
                    // that already recorded its terminal tool event is recovered from
                    // that durable fact instead of re-running a reviewed side effect.
                    Err(error) => {
                        if prior_tool_terminal_fact(
                            &ctx,
                            &decided,
                            tool_request.tool_call_id,
                            &self.session_events,
                        )
                        .await?
                        .is_none()
                        {
                            return Err(error.into());
                        }
                    }
                }
            }
        }

        // Execution-task and compensation owners keep their workflow outbox/ack
        // paths and receive zero conversational callbacks.
        if decided.owner.is_conversational()
            && let Some(receipt) = conversational_receipt(
                &ctx,
                &decided,
                executed_output.as_ref(),
                &self.session_events,
            )
            .await?
        {
            let security_stage =
                apply_receipt_security_assessment(&ctx, tenant_id, &receipt).await?;
            if matches!(
                security_stage,
                Some(SecurityCircuitStage::SuspendedForInput | SecurityCircuitStage::Halted)
            ) {
                release_conversational_review(
                    &ctx,
                    moa_core::types::action_policy::ActionReviewRelease {
                        review_id: receipt.review_id,
                        owner: receipt.owner,
                        resume_queued: false,
                    },
                )
                .await?;
            } else {
                deliver_conversational_resolution(&ctx, receipt).await?;
            }
        }
        if owns_execution_attempt {
            let handle = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ActionReviewDispatcherClient>()
                    .dispatch(Json::from(DispatchActionReviewsRequest::default()))
                    .idempotency_key(format!("execution-action-review-decision:{review_id}")),
            )
            .send();
            let _invocation_id = handle.invocation_id().await?;
        }
        Ok(())
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: invoked by the exact keyed execution task or compensation workflow before terminal settlement.
    async fn settle_execution_owner_review(
        &self,
        ctx: Context<'_>,
        request: Json<SettleExecutionActionReviewRequest>,
    ) -> Result<Json<ExecutionActionReviewSettlement>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionReviews", "settle_execution_owner_review");
        let pool = self.pool.clone();
        Ok(ctx
            .run(|| async move {
                settle_execution_action_review(pool, request.into_inner())
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_settle_execution_owner_review")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: invoked only after an exact execution attempt generation durably parks its review.
    async fn acknowledge_execution_owner_review(
        &self,
        ctx: Context<'_>,
        request: Json<AcknowledgeExecutionActionReviewRequest>,
    ) -> Result<(), HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("ActionReviews", "acknowledge_execution_owner_review");
        let request = request.into_inner();
        if request.owner.execution_origin().is_none()
            && request.owner.compensation_origin().is_none()
        {
            return Err(TerminalError::new(
                "execution review acknowledgement requires a task or compensation owner",
            )
            .into());
        }
        let pool = self.pool.clone();
        let storage_partition_id = storage_partition_id(request.tenant_id);
        ctx.run(|| async move {
            action_review_app::mark_owner_registered(
                pool,
                storage_partition_id,
                request.review_id,
                Some(&request.owner),
            )
            .await
        })
        .name("action_reviews_acknowledge_execution_owner")
        .await
        .map_err(HandlerError::from)
    }
}

fn requires_conversational_registration(owner: &ActionReviewOwner) -> bool {
    matches!(
        owner,
        ActionReviewOwner::Coordinator { .. } | ActionReviewOwner::Worker { .. }
    )
}

/// Registers one pending conversational review on its typed owner.
///
/// Execution owners are routed nowhere: they are resumed by the durable
/// execution outbox, and sending either to a Session or Worker handler would
/// resume a conversation that never issued the action.
async fn register_conversational_review(
    ctx: &Context<'_>,
    owner: &ActionReviewOwner,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    let registration = ActionReviewRegistration {
        review_id,
        owner: owner.clone(),
    };
    match owner {
        ActionReviewOwner::Coordinator { session_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .register_action_review(Json::from(registration)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::Worker { worker_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<WorkerClient>(worker_id.clone())
                    .register_action_review(Json::from(registration)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::ExecutionTask { .. }
        | ActionReviewOwner::ExecutionCompensation { .. } => {}
    }
    Ok(())
}

/// Delivers one typed resolution receipt to its conversational owner.
async fn deliver_conversational_resolution(
    ctx: &Context<'_>,
    receipt: ActionReviewReceipt,
) -> Result<(), HandlerError> {
    match receipt.owner.clone() {
        ActionReviewOwner::Coordinator { session_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .action_review_resolved(Json::from(receipt)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::Worker { worker_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<WorkerClient>(worker_id)
                    .action_review_resolved(Json::from(receipt)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::ExecutionTask { .. }
        | ActionReviewOwner::ExecutionCompensation { .. } => {}
    }
    Ok(())
}

/// Applies one reviewed `ToolResult` assessment before any model continuation.
async fn apply_receipt_security_assessment(
    ctx: &Context<'_>,
    tenant_id: TenantId,
    receipt: &ActionReviewReceipt,
) -> Result<Option<SecurityCircuitStage>, HandlerError> {
    let metadata = match &receipt.outcome {
        ActionReviewOutcome::Cleared(ToolTerminalFact::Result(metadata)) => metadata,
        ActionReviewOutcome::Cleared(ToolTerminalFact::Error) | ActionReviewOutcome::Denied => {
            return Ok(None);
        }
    };
    let tool_call_id = receipt.executed_tool_call_id.ok_or_else(|| {
        TerminalError::new("reviewed ToolResult receipt has no executed tool call id")
    })?;
    crate::services::security_events::apply_reviewed_conversational_assessment(
        ctx,
        tenant_id,
        &receipt.owner,
        tool_call_id,
        &metadata.capability,
        &metadata.assessment,
    )
    .await
    .map(Some)
}

/// Releases a review registration without scheduling a model continuation.
async fn release_conversational_review(
    ctx: &Context<'_>,
    release: moa_core::types::action_policy::ActionReviewRelease,
) -> Result<(), HandlerError> {
    match release.owner.clone() {
        ActionReviewOwner::Coordinator { session_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .release_action_review(Json::from(release)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::Worker { worker_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<WorkerClient>(worker_id)
                    .release_action_review(Json::from(release)),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::ExecutionTask { .. }
        | ActionReviewOwner::ExecutionCompensation { .. } => {}
    }
    Ok(())
}

/// Releases a timed-out conversational review without scheduling continuation.
pub(crate) async fn release_timed_out_conversational_review(
    ctx: &Context<'_>,
    release: moa_core::types::action_policy::ActionReviewRelease,
) -> Result<(), HandlerError> {
    let idempotency_key = release.review_id.to_string();
    match release.owner.clone() {
        ActionReviewOwner::Coordinator { session_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<SessionClient>(session_id.to_string())
                    .release_action_review(Json::from(release))
                    .idempotency_key(idempotency_key),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::Worker { worker_id, .. } => {
            crate::restate_identity::replay_safe_request(
                ctx.object_client::<WorkerClient>(worker_id)
                    .release_action_review(Json::from(release))
                    .idempotency_key(idempotency_key),
            )
            .call()
            .await?;
        }
        ActionReviewOwner::ExecutionTask { .. }
        | ActionReviewOwner::ExecutionCompensation { .. } => {}
    }
    Ok(())
}

/// Builds the typed receipt for a conversational owner, or `None` when the
/// terminal facts a callback depends on are not durable yet.
///
/// A denial resolves on the decision alone. A clear additionally requires the
/// executed tool's terminal `ToolResult`/`ToolError`: either the one this
/// invocation just produced, or — on replay — the one already in the durable log.
async fn conversational_receipt(
    ctx: &Context<'_>,
    decided: &action_review_app::DecidedReview,
    executed_output: Option<&SecuredToolOutput>,
    session_events: &Arc<dyn SessionEventLookupStore>,
) -> Result<Option<ActionReviewReceipt>, HandlerError> {
    let outcome = match decided.status {
        ActionReviewStatus::Denied => ActionReviewOutcome::Denied,
        ActionReviewStatus::Cleared => {
            let Some(executed_tool_call_id) = decided.executed_tool_call_id else {
                return Ok(None);
            };
            match executed_output {
                Some(output) => ActionReviewOutcome::Cleared(executed_terminal_fact(output)),
                None => {
                    let Some(terminal) = prior_tool_terminal_fact(
                        ctx,
                        decided,
                        executed_tool_call_id,
                        session_events,
                    )
                    .await?
                    else {
                        return Ok(None);
                    };
                    ActionReviewOutcome::Cleared(terminal)
                }
            }
        }
        ActionReviewStatus::Pending | ActionReviewStatus::Timeout | ActionReviewStatus::Revoked => {
            return Ok(None);
        }
    };

    Ok(Some(ActionReviewReceipt {
        review_id: decided.review_id,
        owner: decided.owner.clone(),
        tool_name: decided.tool_name.clone(),
        executed_tool_call_id: decided.executed_tool_call_id,
        outcome,
    }))
}

/// Classifies the output this invocation just produced.
///
/// A tool that returned an error output still produced a durable `ToolResult`; the
/// distinction from an `ExecutionError` is what tells the continuation whether the
/// action ran at all.
fn executed_terminal_fact(secured: &SecuredToolOutput) -> ToolTerminalFact {
    ToolTerminalFact::Result(ToolResultSecurityMetadata {
        success: !secured.is_error(),
        assessment: secured.assessment.clone(),
        capability: secured.capability.clone(),
    })
}

fn execution_review_resolution(
    outcome: ExecutionToolCallOutcome,
) -> Result<ExecutionActionReviewResolution, HandlerError> {
    match outcome {
        ExecutionToolCallOutcome::Completed { output } => {
            Ok(ExecutionActionReviewResolution::Completed {
                tool_output: serde_json::to_value(output).map_err(|error| {
                    TerminalError::new(format!("serialize execution review tool output: {error}"))
                })?,
            })
        }
        ExecutionToolCallOutcome::ExternalJob {
            external_job_uid,
            job,
        } => Ok(ExecutionActionReviewResolution::ExternalJob {
            external_job_uid,
            job,
        }),
        ExecutionToolCallOutcome::UnknownOutcome { message } => {
            Ok(ExecutionActionReviewResolution::UnknownOutcome { message })
        }
        ExecutionToolCallOutcome::NotDispatched { reason } => {
            Ok(ExecutionActionReviewResolution::NotDispatched { reason })
        }
    }
}

fn execution_review_request(
    owner: &ActionReviewOwner,
    review_uid: Uuid,
    tool_request: Option<&moa_core::types::tools::ToolCallRequest>,
) -> Option<ExecutionToolCallRequest> {
    let call = tool_request?.clone();
    let origin = match owner {
        ActionReviewOwner::ExecutionTask { origin, .. } => ExecutionToolCallOrigin::Task(*origin),
        ActionReviewOwner::ExecutionCompensation { origin, .. } => {
            ExecutionToolCallOrigin::Compensation(*origin)
        }
        ActionReviewOwner::Coordinator { .. } | ActionReviewOwner::Worker { .. } => return None,
    };
    Some(ExecutionToolCallRequest {
        call,
        origin,
        phase: ExecutionToolCallPhase::Reviewed { review_uid },
    })
}

/// Loads the terminal fact already durable for one reviewed call.
async fn prior_tool_terminal_fact(
    ctx: &Context<'_>,
    decided: &action_review_app::DecidedReview,
    tool_call_id: ToolCallId,
    session_events: &Arc<dyn SessionEventLookupStore>,
) -> Result<Option<ToolTerminalFact>, HandlerError> {
    let store = session_events.clone();
    let storage_partition_id = decided.storage_partition_id.clone();
    let session_id = decided.owner.session_id();
    Ok(ctx
        .run(|| async move {
            store
                .tool_terminal_fact(&storage_partition_id, session_id, tool_call_id)
                .await
                .map(Json::from)
                .map_err(moa_error_to_handler_error)
        })
        .name("action_reviews_tool_terminal_fact")
        .await?
        .into_inner())
}

fn storage_partition_id(tenant_id: TenantId) -> StoragePartitionId {
    StoragePartitionId::for_tenant(tenant_id)
}

fn incoming_trace_context(ctx: &impl RequestHeaders) -> Option<ValidatedTraceContext> {
    let headers = ctx.request_headers();
    ValidatedTraceContext::from_headers(|name| headers.get(name).cloned())
}

fn current_trace_context() -> Option<ValidatedTraceContext> {
    let headers = moa_observability::current_trace_headers();
    ValidatedTraceContext::from_headers(|name| headers.get(name).cloned())
}

#[cfg(test)]
mod tests {
    use moa_core::traits::{Identity, IdentityType};
    use moa_core::types::{
        action_policy::{ActionReviewOwner, ExecutionCompensationOrigin, ExecutionTaskOrigin},
        identifiers::{SessionId, TenantId, ToolCallId},
        tools::ToolCallRequest,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        executed_terminal_fact, execution_review_request, execution_review_resolution,
        requires_conversational_registration,
    };
    use crate::services::tool_executor::{
        ExecutionToolCallOrigin, ExecutionToolCallOutcome, ExecutionToolCallPhase,
    };
    use moa_core::types::action_policy::{ToolResultSecurityMetadata, ToolTerminalFact};
    use moa_execution::wire::ExecutionActionReviewResolution;

    #[test]
    fn executed_terminal_fact_preserves_tool_result_success_bit() {
        // Pins: a tool that ran and returned an error output still produced a
        // durable ToolResult, but its terminal metadata must retain `success=false`.
        let success =
            executed_terminal_fact(&moa_core::types::tools::SecuredToolOutput::assessed_safe(
                moa_core::types::tools::ToolOutput::text("ok", std::time::Duration::from_millis(1)),
                moa_core::types::security::ToolCapabilityId::builtin("noop"),
            ));
        assert!(matches!(
            success,
            ToolTerminalFact::Result(ToolResultSecurityMetadata { success: true, .. })
        ));

        let tool_error =
            executed_terminal_fact(&moa_core::types::tools::SecuredToolOutput::assessed_safe(
                moa_core::types::tools::ToolOutput::error(
                    "exit 1",
                    std::time::Duration::from_millis(1),
                ),
                moa_core::types::security::ToolCapabilityId::builtin("noop"),
            ));
        assert!(matches!(
            tool_error,
            ToolTerminalFact::Result(ToolResultSecurityMetadata { success: false, .. })
        ));
    }

    #[test]
    fn action_policy_clear_preserves_execution_task_review_provenance() {
        // Pins: approval replay cannot downgrade an execution task to root-session dispatch.
        let origin = ExecutionTaskOrigin {
            run_uid: Uuid::from_u128(10),
            task_uid: Uuid::from_u128(20),
            generation: 3,
            attempt_generation: 4,
        };
        let call = ToolCallRequest {
            tool_call_id: ToolCallId::new(),
            caller_identity: Identity {
                identity_type: IdentityType::Operator,
                id: Uuid::from_u128(2),
                tenant_id: TenantId::from(Uuid::from_u128(1)),
                api_key_id: None,
                acting_on_behalf_of: None,
            },
            provider_tool_use_id: None,
            tool_name: "bash".to_string(),
            expected_tool_contract_revision: "contract-v1".to_string(),
            input: json!({"cmd": "printf reviewed"}),
            active_canary: None,
            session_id: moa_core::types::identifiers::SessionId::new(),
            trusted_sandbox_manifest: None,
            worker_id: None,
            resource_budget: Default::default(),
        };

        let owner = ActionReviewOwner::ExecutionTask {
            session_id: call.session_id,
            origin,
        };
        let review_uid = Uuid::from_u128(21);
        let request = execution_review_request(&owner, review_uid, Some(&call))
            .expect("execution provenance should select execution-task dispatch");

        assert_eq!(request.call, call);
        assert_eq!(request.origin, ExecutionToolCallOrigin::Task(origin));
        assert_eq!(
            request.phase,
            ExecutionToolCallPhase::Reviewed { review_uid }
        );
        assert!(
            execution_review_request(
                &ActionReviewOwner::Coordinator {
                    session_id: SessionId::new(),
                    turn_id: "turn-1".to_string(),
                    generation: 1,
                },
                Uuid::from_u128(22),
                Some(&call),
            )
            .is_none()
        );
    }

    #[test]
    fn action_policy_clear_preserves_execution_compensation_review_provenance() {
        // Pins: a cleared rollback review carries its exact compensation id and
        // generation to ToolExecutor instead of masquerading as a forward task.
        let call = ToolCallRequest {
            tool_call_id: ToolCallId::new(),
            caller_identity: Identity {
                identity_type: IdentityType::Operator,
                id: Uuid::from_u128(2),
                tenant_id: TenantId::from(Uuid::from_u128(1)),
                api_key_id: None,
                acting_on_behalf_of: None,
            },
            provider_tool_use_id: None,
            tool_name: "bash".to_string(),
            expected_tool_contract_revision: "contract-v1".to_string(),
            input: json!({"cmd": "printf undo"}),
            active_canary: None,
            session_id: SessionId::new(),
            trusted_sandbox_manifest: None,
            worker_id: None,
            resource_budget: Default::default(),
        };
        let origin = ExecutionCompensationOrigin {
            run_uid: Uuid::from_u128(10),
            compensation_id: Uuid::from_u128(30),
            generation: 7,
            attempt_generation: 8,
        };
        let owner = ActionReviewOwner::ExecutionCompensation {
            session_id: call.session_id,
            origin,
        };

        let review_uid = Uuid::from_u128(31);
        let request = execution_review_request(&owner, review_uid, Some(&call))
            .expect("compensation provenance should select execution dispatch");

        assert_eq!(request.call, call);
        assert_eq!(
            request.origin,
            ExecutionToolCallOrigin::Compensation(origin)
        );
        assert_eq!(
            request.phase,
            ExecutionToolCallPhase::Reviewed { review_uid }
        );
    }

    #[test]
    fn execution_review_registration_waits_for_the_attempt_park_offline() {
        // Pins: execution-owned reviews cannot become decision-ready in the
        // ActionReviews request handler before the attempt's Postgres park CAS.
        let session_id = SessionId::new();
        assert!(requires_conversational_registration(
            &ActionReviewOwner::Coordinator {
                session_id,
                turn_id: "turn-1".to_string(),
                generation: 1,
            }
        ));
        assert!(requires_conversational_registration(
            &ActionReviewOwner::Worker {
                session_id,
                worker_id: "worker-1".to_string(),
                turn_id: "turn-1".to_string(),
                generation: 1,
            }
        ));
        assert!(!requires_conversational_registration(
            &ActionReviewOwner::ExecutionTask {
                session_id,
                origin: ExecutionTaskOrigin {
                    run_uid: Uuid::from_u128(10),
                    task_uid: Uuid::from_u128(20),
                    generation: 3,
                    attempt_generation: 4,
                },
            }
        ));
        assert!(!requires_conversational_registration(
            &ActionReviewOwner::ExecutionCompensation {
                session_id,
                origin: ExecutionCompensationOrigin {
                    run_uid: Uuid::from_u128(10),
                    compensation_id: Uuid::from_u128(30),
                    generation: 7,
                    attempt_generation: 8,
                },
            }
        ));
    }

    #[test]
    fn execution_review_preserves_classified_error_for_owner_specific_terminalization() {
        // Pins: ActionReviews persists the valid classified envelope even when it
        // is an error, so the task can use its pinned idempotency contract while a
        // compensation can apply its own definitive-failure rule.
        let output = moa_core::types::tools::SecuredToolOutput::assessed_safe(
            moa_core::types::tools::ToolOutput::error(
                "upstream rejected request",
                std::time::Duration::ZERO,
            ),
            moa_core::types::security::ToolCapabilityId::builtin("fixture"),
        );
        let resolution = execution_review_resolution(ExecutionToolCallOutcome::Completed {
            output: Box::new(output.clone()),
        })
        .expect("classified execution output should serialize");

        let ExecutionActionReviewResolution::Completed { tool_output } = resolution else {
            panic!("a classified tool error must remain a completed execution envelope");
        };
        let decoded: moa_core::types::tools::SecuredToolOutput =
            serde_json::from_value(tool_output)
                .expect("stored execution review output should decode");
        assert_eq!(decoded, output);
    }

    #[test]
    fn execution_review_persists_definitive_not_dispatched_resolution() {
        // Pins: once an admin has claimed a review, a stale owner resolves
        // definitively instead of leaving the review pending or implying execution.
        let reason = moa_execution::wire::ExecutionToolDispatchRejection::OperationNotRunning;
        let resolution =
            execution_review_resolution(ExecutionToolCallOutcome::NotDispatched { reason })
                .expect("typed non-dispatch should be a valid review resolution");

        assert_eq!(
            resolution,
            ExecutionActionReviewResolution::NotDispatched { reason }
        );
    }

    #[test]
    fn execution_review_preserves_pre_admitted_external_job_identity_offline() {
        // Pins: review settlement must route the exact MOA-owned job reserved before
        // provider dispatch; reconstructing a new UID would orphan capacity and callbacks.
        let external_job_uid = Uuid::from_u128(41);
        let job = moa_core::types::tools::AsyncToolJob {
            provider: "fixture".to_string(),
            provider_job_id: "provider-job-41".to_string(),
            idempotency_key: "review-job-41".to_string(),
            callback_auth_reference: "callback-41".to_string(),
            progress_phase: "queued".to_string(),
            cancel_supported: true,
            next_reconcile_at: chrono::DateTime::UNIX_EPOCH,
        };

        let resolution = execution_review_resolution(ExecutionToolCallOutcome::ExternalJob {
            external_job_uid,
            job: job.clone(),
        })
        .expect("bound execution external job should remain a valid review resolution");

        assert_eq!(
            resolution,
            ExecutionActionReviewResolution::ExternalJob {
                external_job_uid,
                job,
            }
        );
    }
}
