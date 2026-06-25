//! Tenant-admin action review queue and decision service.

use chrono::{DateTime, Utc};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::traits::Identity;
use moa_core::wire::AppendEventRequest;
use moa_core::{
    ActionClass, ActionEnvelope, ActionReviewPreview, ActionReviewStatus, Event, EventType,
    StoragePartitionId, TenantId, ToolCallId, ToolCallRequest,
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_observability::{record_action_review_decision, record_action_review_requested};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::action_reviews::app as action_review_app;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::services::session_store::RestateSessionStoreClient;
use crate::services::tool_executor::ToolExecutorClient;

/// Summary returned for one tenant action review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReviewSummary {
    /// Review identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning session, when the action came from a session turn.
    pub session_id: Option<moa_core::SessionId>,
    /// Sub-agent that requested the action, when present.
    pub sub_agent_id: Option<String>,
    /// Original tool call identifier.
    pub tool_call_id: ToolCallId,
    /// Tool name.
    pub tool_name: String,
    /// Action class.
    pub action_class: ActionClass,
    /// Risk level.
    pub risk_level: moa_core::RiskLevel,
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
#[derive(Clone, Default)]
pub struct ActionReviewsImpl;

impl ActionReviews for ActionReviewsImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: request is an internal workflow call after the owning session or sub-agent has already checked participant authorization before tool execution.
    async fn request(
        &self,
        ctx: Context<'_>,
        request: Json<RequestActionReview>,
    ) -> Result<Json<ActionReviewSummary>, HandlerError> {
        annotate_restate_handler_span("ActionReviews", "request");
        let mut request = request.into_inner();
        action_review_app::prepare_request(&mut request)?;
        let event = action_review_app::requested_event(&request);
        let pool = OrchestratorCtx::current_graph_pool();
        let session_id = request.envelope.session_id;
        let action_class = request.envelope.action_class;

        let stored = ctx
            .run(|| async move {
                action_review_app::request_review(pool, request)
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_request")
            .await?
            .into_inner();
        if stored.record_requested_event {
            if let Some(session_id) = session_id {
                let event_exists = prior_action_review_event_exists(
                    &ctx,
                    &storage_partition_id(stored.summary.tenant_id),
                    session_id,
                    EventType::ActionReviewRequested,
                    stored.summary.id,
                )
                .await?;
                if !event_exists {
                    ctx.service_client::<RestateSessionStoreClient>()
                        .append_event(Json(AppendEventRequest { session_id, event }))
                        .call()
                        .await?;
                }
            }
            if session_id.is_none() {
                tracing::warn!(
                    action_review.id = %stored.summary.id,
                    tenant_id = %stored.summary.tenant_id,
                    sub_agent_id = ?stored.summary.sub_agent_id,
                    "action review has no session id; skipping session event append"
                );
            }
            let pool = OrchestratorCtx::current_graph_pool();
            let storage_partition_id = storage_partition_id(stored.summary.tenant_id);
            let review_id = stored.summary.id;
            ctx.run(|| async move {
                action_review_app::mark_requested_event_recorded(
                    pool,
                    storage_partition_id,
                    review_id,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_mark_requested_event_recorded")
            .await?;
        }
        if stored.newly_inserted {
            record_action_review_requested(moa_core::ActionPolicyEffect::AdminReview, action_class);
        }
        Ok(Json::from(stored.summary))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_pending(
        &self,
        ctx: Context<'_>,
        request: Json<ListActionReviewsRequest>,
    ) -> Result<Json<Vec<ActionReviewSummary>>, HandlerError> {
        annotate_restate_handler_span("ActionReviews", "list_pending");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = OrchestratorCtx::current_graph_pool();
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
        annotate_restate_handler_span("ActionReviews", "decide");
        let request = request.into_inner();
        let identity = authorize_tenant(&ctx, request.tenant_id, Relation::Admin).await?;
        let pool = OrchestratorCtx::current_graph_pool();
        let decided = ctx
            .run(|| async move {
                action_review_app::decide_review(pool, request, identity.id.to_string())
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_decide")
            .await?
            .into_inner();

        if decided.record_decision_event {
            if let Some(session_id) = decided.session_id {
                let event_exists = prior_action_review_event_exists(
                    &ctx,
                    &decided.storage_partition_id,
                    session_id,
                    EventType::ActionReviewDecided,
                    decided.review_id,
                )
                .await?;
                if !event_exists {
                    ctx.service_client::<RestateSessionStoreClient>()
                        .append_event(Json(AppendEventRequest {
                            session_id,
                            event: Event::ActionReviewDecided {
                                review_id: decided.review_id,
                                decision: decided.decision.clone(),
                                decided_by: decided.decided_by.clone(),
                                decided_at: decided.decided_at,
                            },
                        }))
                        .call()
                        .await?;
                }
            }
            let pool = OrchestratorCtx::current_graph_pool();
            let storage_partition_id = decided.storage_partition_id.clone();
            let review_id = decided.review_id;
            ctx.run(|| async move {
                action_review_app::mark_decision_event_recorded(
                    pool,
                    storage_partition_id,
                    review_id,
                )
                .await
                .map(Json::from)
            })
            .name("action_reviews_mark_decision_event_recorded")
            .await?;
        }
        if decided.newly_decided {
            record_action_review_decision(decided.status, decided.action_class);
        }

        if let Some(tool_request) = decided.tool_request.as_ref() {
            if !prior_tool_result_exists(&ctx, &decided, tool_request.tool_call_id).await? {
                let execution = ctx
                    .service_client::<ToolExecutorClient>()
                    .execute(Json::from(tool_request.clone()))
                    .call()
                    .await;
                if let Err(error) = execution
                    && !prior_tool_result_exists(&ctx, &decided, tool_request.tool_call_id).await?
                {
                    return Err(error.into());
                }
            }
            let pool = OrchestratorCtx::current_graph_pool();
            let storage_partition_id = decided.storage_partition_id.clone();
            let review_id = decided.review_id;
            ctx.run(|| async move {
                action_review_app::mark_execution_requested(pool, storage_partition_id, review_id)
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_mark_execution_requested")
            .await?;
        }
        Ok(())
    }
}

async fn prior_tool_result_exists(
    ctx: &Context<'_>,
    decided: &action_review_app::DecidedReview,
    tool_call_id: ToolCallId,
) -> Result<bool, HandlerError> {
    let Some(session_id) = decided.session_id else {
        return Ok(false);
    };
    let store = OrchestratorCtx::current_session_store();
    let storage_partition_id = decided.storage_partition_id.clone();
    Ok(ctx
        .run(|| async move {
            store
                .tool_event_exists(
                    &storage_partition_id,
                    session_id,
                    EventType::ToolResult,
                    tool_call_id,
                )
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("action_reviews_tool_result_exists")
        .await?
        .into_inner())
}

async fn prior_action_review_event_exists(
    ctx: &Context<'_>,
    storage_partition_id: &StoragePartitionId,
    session_id: moa_core::SessionId,
    event_type: EventType,
    review_id: Uuid,
) -> Result<bool, HandlerError> {
    let store = OrchestratorCtx::current_session_store();
    let storage_partition_id = storage_partition_id.clone();
    Ok(ctx
        .run(|| async move {
            store
                .action_review_event_exists(
                    &storage_partition_id,
                    session_id,
                    event_type,
                    review_id,
                )
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("action_reviews_event_exists")
        .await?
        .into_inner())
}

async fn authorize_tenant(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(&fga, &identity, ObjectType::Tenant, tenant_id, relation)
        .await
        .map_err(translate_authz_error)?;
    Ok(identity)
}

fn storage_partition_id(tenant_id: TenantId) -> StoragePartitionId {
    StoragePartitionId::new(tenant_id.to_string())
}
