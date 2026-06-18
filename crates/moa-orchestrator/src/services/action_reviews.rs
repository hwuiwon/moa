//! Workspace-admin action review queue and decision service.

use chrono::{DateTime, Utc};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::wire::AppendEventRequest;
use moa_core::{
    ActionClass, ActionEnvelope, ActionReviewDecision, ActionReviewPreview, ActionReviewStatus,
    Event, ToolCallId, ToolCallRequest, WorkspaceId, record_action_review_decision,
    record_action_review_requested,
};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

use crate::OrchestratorCtx;
use crate::ctx::RequestHeaders;
use crate::handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error};
use crate::services::session_store::RestateSessionStoreClient;
use crate::services::tool_executor::ToolExecutorClient;

/// Summary returned for one workspace action review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionReviewSummary {
    /// Review identifier.
    pub id: Uuid,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
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
    /// Expiration timestamp, when configured.
    pub expires_at: Option<DateTime<Utc>>,
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
    /// Workspace whose pending action reviews should be listed.
    pub workspace_id: WorkspaceId,
}

/// Request payload for `ActionReviews/decide`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecideActionReviewRequest {
    /// Workspace that owns the review.
    pub workspace_id: WorkspaceId,
    /// Review identifier.
    pub review_id: Uuid,
    /// Decision string: `cleared` or `denied`.
    pub decision: String,
    /// Optional denial reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Restate service surface for workspace-admin action reviews.
#[restate_sdk::service]
#[name = "ActionReviews"]
pub trait ActionReviews {
    /// Queue one action for workspace-admin review.
    async fn request(
        request: Json<RequestActionReview>,
    ) -> Result<Json<ActionReviewSummary>, HandlerError>;

    /// List pending workspace action reviews.
    async fn list_pending(
        request: Json<ListActionReviewsRequest>,
    ) -> Result<Json<Vec<ActionReviewSummary>>, HandlerError>;

    /// Decide one workspace action review.
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
        request.tool_request.active_canary = None;
        let pool = OrchestratorCtx::current().graph_pool.clone();
        let event = Event::ActionReviewRequested {
            review_id: request.envelope.review_id,
            envelope: request.envelope.clone(),
            preview: request.preview.clone(),
        };
        let session_id = request.envelope.session_id;
        let action_class = request.envelope.action_class;

        let summary = ctx
            .run(|| async move { insert_review(pool, request).await.map(Json::from) })
            .name("action_reviews_request")
            .await?
            .into_inner();
        if let Some(session_id) = session_id {
            ctx.service_client::<RestateSessionStoreClient>()
                .append_event(Json(AppendEventRequest { session_id, event }))
                .call()
                .await?;
        }
        record_action_review_requested(moa_core::ActionPolicyEffect::AdminReview, action_class);
        Ok(Json::from(summary))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_pending(
        &self,
        ctx: Context<'_>,
        request: Json<ListActionReviewsRequest>,
    ) -> Result<Json<Vec<ActionReviewSummary>>, HandlerError> {
        annotate_restate_handler_span("ActionReviews", "list_pending");
        let request = request.into_inner();
        authorize_workspace(&ctx, &request.workspace_id, Relation::Admin).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();

        Ok(ctx
            .run(|| async move {
                list_pending_reviews(pool, request.workspace_id)
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
        let identity = authorize_workspace(&ctx, &request.workspace_id, Relation::Admin).await?;
        let pool = OrchestratorCtx::current().graph_pool.clone();
        let decided = ctx
            .run(|| async move {
                decide_review(pool, request, identity.id.to_string())
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_decide")
            .await?
            .into_inner();

        if let Some(session_id) = decided.session_id {
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
        record_action_review_decision(decided.status, decided.action_class);

        if let Some(tool_request) = decided.tool_request {
            ctx.service_client::<ToolExecutorClient>()
                .execute(Json::from(tool_request))
                .call()
                .await?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DecidedReview {
    review_id: Uuid,
    session_id: Option<moa_core::SessionId>,
    decision: ActionReviewDecision,
    status: ActionReviewStatus,
    action_class: ActionClass,
    decided_by: String,
    decided_at: DateTime<Utc>,
    tool_request: Option<ToolCallRequest>,
}

async fn insert_review(
    pool: sqlx::PgPool,
    request: RequestActionReview,
) -> Result<ActionReviewSummary, HandlerError> {
    let tool_request = serde_json::to_value(&request.tool_request)
        .map_err(|error| TerminalError::new(format!("serialize tool request: {error}")))?;
    let envelope = serde_json::to_value(&request.envelope)
        .map_err(|error| TerminalError::new(format!("serialize envelope: {error}")))?;
    let preview = serde_json::to_value(&request.preview)
        .map_err(|error| TerminalError::new(format!("serialize preview: {error}")))?;
    sqlx::query(
        r#"
        INSERT INTO workspace_action_reviews (
            id, workspace_id, session_id, sub_agent_id, tool_call_id, tool_name,
            action_class, risk_level, input_summary, normalized_input, envelope,
            preview, tool_request, requested_by
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(request.envelope.review_id)
    .bind(request.envelope.workspace_id.to_string())
    .bind(request.envelope.session_id.map(|id| id.0))
    .bind(request.envelope.sub_agent_id.clone())
    .bind(request.envelope.tool_call_id.0)
    .bind(&request.envelope.tool_name)
    .bind(request.envelope.action_class.as_str())
    .bind(request.envelope.risk_level.as_str())
    .bind(&request.envelope.input_summary)
    .bind(&request.envelope.normalized_input)
    .bind(envelope)
    .bind(preview)
    .bind(tool_request)
    .bind(request.envelope.user_id.to_string())
    .execute(&pool)
    .await
    .map_err(db_error)?;

    load_review(
        pool,
        request.envelope.workspace_id,
        request.envelope.review_id,
    )
    .await
}

async fn list_pending_reviews(
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<ActionReviewSummary>, HandlerError> {
    let rows = sqlx::query(
        r#"
        SELECT id, workspace_id, session_id, sub_agent_id, tool_call_id, tool_name,
               action_class, risk_level, input_summary, envelope, preview, status,
               requested_by, decided_by, deny_reason, created_at, expires_at, decided_at
        FROM workspace_action_reviews
        WHERE workspace_id = $1 AND status = 'pending'
        ORDER BY created_at DESC
        LIMIT 100
        "#,
    )
    .bind(workspace_id.to_string())
    .fetch_all(&pool)
    .await
    .map_err(db_error)?;

    rows.iter().map(summary_from_row).collect()
}

async fn load_review(
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    review_id: Uuid,
) -> Result<ActionReviewSummary, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, workspace_id, session_id, sub_agent_id, tool_call_id, tool_name,
               action_class, risk_level, input_summary, envelope, preview, status,
               requested_by, decided_by, deny_reason, created_at, expires_at, decided_at
        FROM workspace_action_reviews
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(review_id)
    .fetch_optional(&pool)
    .await
    .map_err(db_error)?
    .ok_or_else(|| TerminalError::new_with_code(404, "action review not found"))?;

    summary_from_row(&row)
}

async fn decide_review(
    pool: sqlx::PgPool,
    request: DecideActionReviewRequest,
    decided_by: String,
) -> Result<DecidedReview, HandlerError> {
    let decision = match request.decision.as_str() {
        "cleared" => ActionReviewDecision::Cleared,
        "denied" => ActionReviewDecision::Denied {
            reason: request.reason.clone(),
        },
        other => {
            return Err(TerminalError::new_with_code(
                400,
                format!("bad action review decision: {other}"),
            )
            .into());
        }
    };
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let row = sqlx::query(
        r#"
        SELECT id, workspace_id, session_id, action_class, status, tool_request
        FROM workspace_action_reviews
        WHERE workspace_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(request.workspace_id.to_string())
    .bind(request.review_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_error)?
    .ok_or_else(|| TerminalError::new_with_code(404, "action review not found"))?;
    let status: String = row.try_get("status").map_err(db_error)?;
    if status != "pending" {
        return Err(
            TerminalError::new_with_code(409, format!("action review already {status}")).into(),
        );
    }

    let decided_at = Utc::now();
    let status = match &decision {
        ActionReviewDecision::Cleared => ActionReviewStatus::Cleared,
        ActionReviewDecision::Denied { .. } => ActionReviewStatus::Denied,
    };
    sqlx::query(
        r#"
        UPDATE workspace_action_reviews
        SET status = $3, decided_by = $4, deny_reason = $5, decided_at = $6
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(request.workspace_id.to_string())
    .bind(request.review_id)
    .bind(status.as_str())
    .bind(&decided_by)
    .bind(request.reason.as_deref())
    .bind(decided_at)
    .execute(&mut *tx)
    .await
    .map_err(db_error)?;
    tx.commit()
        .await
        .map_err(|error| TerminalError::new(format!("db commit: {error}")))?;

    let session_id = row
        .try_get::<Option<Uuid>, _>("session_id")
        .map_err(db_error)?
        .map(moa_core::SessionId);
    let action_class = parse_db_enum::<ActionClass>(
        "action_class",
        row.try_get::<String, _>("action_class").map_err(db_error)?,
    )?;
    let tool_request = if matches!(decision, ActionReviewDecision::Cleared) {
        let mut tool_request = serde_json::from_value::<ToolCallRequest>(
            row.try_get::<serde_json::Value, _>("tool_request")
                .map_err(db_error)?,
        )
        .map_err(|error| TerminalError::new(format!("decode stored tool request: {error}")))?;
        tool_request.tool_call_id = ToolCallId(Uuid::now_v7());
        tool_request.provider_tool_use_id = None;
        tool_request.active_canary = None;
        Some(tool_request)
    } else {
        None
    };

    Ok(DecidedReview {
        review_id: request.review_id,
        session_id,
        decision,
        status,
        action_class,
        decided_by,
        decided_at,
        tool_request,
    })
}

fn summary_from_row(row: &sqlx::postgres::PgRow) -> Result<ActionReviewSummary, HandlerError> {
    Ok(ActionReviewSummary {
        id: row.try_get("id").map_err(db_error)?,
        workspace_id: WorkspaceId::new(row.try_get::<String, _>("workspace_id").map_err(db_error)?),
        session_id: row
            .try_get::<Option<Uuid>, _>("session_id")
            .map_err(db_error)?
            .map(moa_core::SessionId),
        sub_agent_id: row.try_get("sub_agent_id").map_err(db_error)?,
        tool_call_id: ToolCallId(row.try_get("tool_call_id").map_err(db_error)?),
        tool_name: row.try_get("tool_name").map_err(db_error)?,
        action_class: parse_db_enum(
            "action_class",
            row.try_get::<String, _>("action_class").map_err(db_error)?,
        )?,
        risk_level: parse_db_enum(
            "risk_level",
            row.try_get::<String, _>("risk_level").map_err(db_error)?,
        )?,
        input_summary: row.try_get("input_summary").map_err(db_error)?,
        envelope: serde_json::from_value(
            row.try_get::<serde_json::Value, _>("envelope")
                .map_err(db_error)?,
        )
        .map_err(|error| TerminalError::new(format!("decode envelope: {error}")))?,
        preview: serde_json::from_value(
            row.try_get::<serde_json::Value, _>("preview")
                .map_err(db_error)?,
        )
        .map_err(|error| TerminalError::new(format!("decode preview: {error}")))?,
        status: parse_db_enum(
            "status",
            row.try_get::<String, _>("status").map_err(db_error)?,
        )?,
        requested_by: row.try_get("requested_by").map_err(db_error)?,
        decided_by: row.try_get("decided_by").map_err(db_error)?,
        deny_reason: row.try_get("deny_reason").map_err(db_error)?,
        created_at: row.try_get("created_at").map_err(db_error)?,
        expires_at: row.try_get("expires_at").map_err(db_error)?,
        decided_at: row.try_get("decided_at").map_err(db_error)?,
    })
}

async fn authorize_workspace(
    ctx: &impl RequestHeaders,
    workspace_id: &WorkspaceId,
    relation: Relation,
) -> Result<Identity, HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Workspace,
        workspace_id,
        relation,
    )
    .await
    .map_err(translate_authz_error)?;
    Ok(identity)
}

fn parse_db_enum<E>(kind: &str, value: String) -> Result<E, HandlerError>
where
    E: std::str::FromStr,
{
    value
        .parse::<E>()
        .map_err(|_| TerminalError::new(format!("unknown {kind} value `{value}`")).into())
}

fn db_error(error: sqlx::Error) -> HandlerError {
    TerminalError::new(format!("action review db error: {error}")).into()
}
