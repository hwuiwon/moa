//! Tenant-admin action review queue and decision service.

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::ExecutionFailureClass;
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
use std::sync::Arc;
use uuid::Uuid;

use crate::action_reviews::app as action_review_app;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::authorize_tenant;
use crate::objects::session::SessionClient;
use crate::objects::worker::WorkerClient;
use crate::services::session_store::RestateSessionStoreClient;
use crate::services::tool_executor::{ExecutionTaskToolCallRequest, ToolExecutorClient};
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
}

/// Concrete action-review service implementation.
#[derive(Clone)]
pub struct ActionReviewsImpl {
    pool: sqlx::PgPool,
    session_events: Arc<dyn SessionEventLookupStore>,
    review_timeout_secs: i64,
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
    ) -> Self {
        Self {
            pool,
            session_events,
            review_timeout_secs,
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
        let execution_task_trace_context = request
            .envelope
            .owner
            .execution_origin()
            .is_some()
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
        let owner_needs_registration =
            !stored.owner_registered && stored.summary.status == ActionReviewStatus::Pending;
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
                action_review_app::mark_owner_registered(pool, storage_partition_id, review_id)
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_mark_owner_registered")
            .await?;
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
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
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
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
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

        if let (Some(tool_request), Some(origin)) = (
            decided.tool_request.clone(),
            decided.owner.execution_origin(),
        ) {
            let execution = crate::restate_identity::replay_safe_request(
                ctx.service_client::<ToolExecutorClient>()
                    .execute_execution_task(Json::from(ExecutionTaskToolCallRequest {
                        call: tool_request,
                        origin: Some(origin),
                    })),
            )
            .call()
            .await;
            let resolution = match execution {
                Ok(secured) => {
                    // The reviewed tool's output was classified inside the
                    // executor's `ctx.run`. This path consumes that envelope
                    // whole — it never re-derives a summary from raw bytes and
                    // never reclassifies.
                    let secured = secured.into_inner();
                    if secured.is_error() {
                        ExecutionActionReviewResolution::Failed {
                            class: ExecutionFailureClass::Terminal,
                            message: "reviewed tool returned a classified error".to_string(),
                        }
                    } else {
                        ExecutionActionReviewResolution::Completed {
                            tool_output: serde_json::to_value(&secured).map_err(|error| {
                                TerminalError::new(format!(
                                    "serialize execution review tool output: {error}"
                                ))
                            })?,
                        }
                    }
                }
                Err(_) => ExecutionActionReviewResolution::Failed {
                    class: ExecutionFailureClass::Terminal,
                    message: "reviewed tool execution failed before producing a classified result"
                        .to_string(),
                },
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
                execution_task_review_request(decided.owner.execution_origin(), tool_request)
            {
                crate::restate_identity::replay_safe_request(
                    ctx.service_client::<ToolExecutorClient>()
                        .execute_execution_task(Json::from(execution_request)),
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

        // An execution-task owner keeps its existing run/task outbox and ack path and
        // receives zero conversational callbacks.
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
        Ok(())
    }
}

/// Registers one pending conversational review on its typed owner.
///
/// An `ExecutionTask` owner is routed nowhere: it is resumed by the durable
/// execution outbox, and sending it to a Session or Worker handler would resume a
/// conversation that never issued the action.
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
        ActionReviewOwner::ExecutionTask { .. } => {}
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
        ActionReviewOwner::ExecutionTask { .. } => {}
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
        ActionReviewOwner::ExecutionTask { .. } => {}
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
        ActionReviewStatus::Pending | ActionReviewStatus::Timeout => return Ok(None),
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

fn execution_task_review_request(
    origin: Option<moa_core::types::action_policy::ExecutionTaskOrigin>,
    tool_request: &moa_core::types::tools::ToolCallRequest,
) -> Option<ExecutionTaskToolCallRequest> {
    origin.map(|origin| ExecutionTaskToolCallRequest {
        call: tool_request.clone(),
        origin: Some(origin),
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
                .map_err(HandlerError::from)
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
        action_policy::ExecutionTaskOrigin,
        identifiers::{TenantId, ToolCallId},
        tools::ToolCallRequest,
    };
    use serde_json::json;
    use uuid::Uuid;

    use super::{executed_terminal_fact, execution_task_review_request};
    use moa_core::types::action_policy::{ToolResultSecurityMetadata, ToolTerminalFact};

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

        let request = execution_task_review_request(Some(origin), &call)
            .expect("execution provenance should select execution-task dispatch");

        assert_eq!(request.call, call);
        assert_eq!(request.origin, Some(origin));
        assert!(execution_task_review_request(None, &call).is_none());
    }
}
