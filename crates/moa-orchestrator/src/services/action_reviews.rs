//! Workspace-admin action review queue and decision service.

use chrono::{DateTime, Utc};
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::restate_observability::annotate_restate_handler_span;
use moa_core::traits::Identity;
use moa_core::wire::AppendEventRequest;
use moa_core::{
    ActionClass, ActionEnvelope, ActionReviewDecision, ActionReviewPreview, ActionReviewStatus,
    Event, EventType, ToolCallId, ToolCallRequest, WorkspaceId, record_action_review_decision,
    record_action_review_requested,
};
use moa_security::{ToolInputCanaryScreening, screen_tool_input_for_canary};
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
        screen_review_tool_input(&request.tool_request)?;
        request.tool_request.active_canary = None;
        let pool = OrchestratorCtx::current().graph_pool.clone();
        let event = Event::ActionReviewRequested {
            review_id: request.envelope.review_id,
            envelope: request.envelope.clone(),
            preview: request.preview.clone(),
        };
        let session_id = request.envelope.session_id;
        let action_class = request.envelope.action_class;

        let stored = ctx
            .run(|| async move { insert_review(pool, request).await.map(Json::from) })
            .name("action_reviews_request")
            .await?
            .into_inner();
        if stored.requested_event_recorded_at.is_none() {
            if let Some(session_id) = session_id {
                let event_exists = prior_action_review_event_exists(
                    &ctx,
                    &stored.summary.workspace_id,
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
            let pool = OrchestratorCtx::current().graph_pool.clone();
            let workspace_id = stored.summary.workspace_id.clone();
            let review_id = stored.summary.id;
            ctx.run(|| async move {
                mark_requested_event_recorded(pool, workspace_id, review_id)
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

        if decided.decision_event_recorded_at.is_none() {
            if let Some(session_id) = decided.session_id {
                let event_exists = prior_action_review_event_exists(
                    &ctx,
                    &decided.workspace_id,
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
            let pool = OrchestratorCtx::current().graph_pool.clone();
            let workspace_id = decided.workspace_id.clone();
            let review_id = decided.review_id;
            ctx.run(|| async move {
                mark_decision_event_recorded(pool, workspace_id, review_id)
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
            let pool = OrchestratorCtx::current().graph_pool.clone();
            let workspace_id = decided.workspace_id.clone();
            let review_id = decided.review_id;
            ctx.run(|| async move {
                mark_execution_requested(pool, workspace_id, review_id)
                    .await
                    .map(Json::from)
            })
            .name("action_reviews_mark_execution_requested")
            .await?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DecidedReview {
    review_id: Uuid,
    workspace_id: WorkspaceId,
    session_id: Option<moa_core::SessionId>,
    decision: ActionReviewDecision,
    status: ActionReviewStatus,
    action_class: ActionClass,
    decided_by: String,
    decided_at: DateTime<Utc>,
    decision_event_recorded_at: Option<DateTime<Utc>>,
    newly_decided: bool,
    tool_request: Option<ToolCallRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredReview {
    summary: ActionReviewSummary,
    requested_event_recorded_at: Option<DateTime<Utc>>,
    newly_inserted: bool,
}

async fn insert_review(
    pool: sqlx::PgPool,
    request: RequestActionReview,
) -> Result<StoredReview, HandlerError> {
    let tool_request = serde_json::to_value(&request.tool_request)
        .map_err(|error| TerminalError::new(format!("serialize tool request: {error}")))?;
    let envelope = serde_json::to_value(&request.envelope)
        .map_err(|error| TerminalError::new(format!("serialize envelope: {error}")))?;
    let preview = serde_json::to_value(&request.preview)
        .map_err(|error| TerminalError::new(format!("serialize preview: {error}")))?;
    let insert = sqlx::query(
        r#"
        INSERT INTO workspace_action_reviews (
            id, workspace_id, user_id, session_id, sub_agent_id, tool_call_id, tool_name,
            action_class, risk_level, input_summary, normalized_input, envelope,
            preview, tool_request, requested_by
        )
        VALUES ($1, $2, NULL, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
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

    let mut stored = load_review_state(
        pool,
        request.envelope.workspace_id,
        request.envelope.review_id,
    )
    .await?;
    stored.newly_inserted = insert.rows_affected() > 0;
    Ok(stored)
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

async fn load_review_state(
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    review_id: Uuid,
) -> Result<StoredReview, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, workspace_id, session_id, sub_agent_id, tool_call_id, tool_name,
               action_class, risk_level, input_summary, envelope, preview, status,
               requested_by, requested_event_recorded_at, decided_by, deny_reason,
               created_at, expires_at, decided_at
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

    Ok(StoredReview {
        summary: summary_from_row(&row)?,
        requested_event_recorded_at: row
            .try_get("requested_event_recorded_at")
            .map_err(db_error)?,
        newly_inserted: false,
    })
}

async fn decide_review(
    pool: sqlx::PgPool,
    request: DecideActionReviewRequest,
    decided_by: String,
) -> Result<DecidedReview, HandlerError> {
    let decision = match request.decision {
        ActionReviewDecisionKind::Cleared => ActionReviewDecision::Cleared,
        ActionReviewDecisionKind::Denied => ActionReviewDecision::Denied {
            reason: request.reason.clone(),
        },
    };
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| TerminalError::new(format!("db begin: {error}")))?;
    let row = sqlx::query(
        r#"
        SELECT id, workspace_id, session_id, action_class, status, tool_request,
               decided_by, deny_reason, decided_at, decision_event_recorded_at,
               execution_tool_call_id, execution_requested_at
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
    let stored_status: ActionReviewStatus = parse_db_enum(
        "status",
        row.try_get::<String, _>("status").map_err(db_error)?,
    )?;
    let desired_status = match &decision {
        ActionReviewDecision::Cleared => ActionReviewStatus::Cleared,
        ActionReviewDecision::Denied { .. } => ActionReviewStatus::Denied,
    };
    if stored_status != ActionReviewStatus::Pending && stored_status != desired_status {
        return Err(TerminalError::new_with_code(
            409,
            format!("action review already {}", stored_status.as_str()),
        )
        .into());
    }

    let newly_decided = stored_status == ActionReviewStatus::Pending;
    let existing_decided_at = row
        .try_get::<Option<DateTime<Utc>>, _>("decided_at")
        .map_err(db_error)?;
    let existing_decided_by = row
        .try_get::<Option<String>, _>("decided_by")
        .map_err(db_error)?;
    let existing_deny_reason = row
        .try_get::<Option<String>, _>("deny_reason")
        .map_err(db_error)?;
    let existing_execution_tool_call_id = row
        .try_get::<Option<Uuid>, _>("execution_tool_call_id")
        .map_err(db_error)?;
    let decided_at = existing_decided_at.unwrap_or_else(Utc::now);
    let decided_by = existing_decided_by.unwrap_or(decided_by);
    let deny_reason = match &decision {
        ActionReviewDecision::Denied { reason } => existing_deny_reason.or_else(|| reason.clone()),
        ActionReviewDecision::Cleared => None,
    };
    let execution_tool_call_id = if matches!(decision, ActionReviewDecision::Cleared) {
        Some(existing_execution_tool_call_id.unwrap_or_else(Uuid::now_v7))
    } else {
        None
    };

    if newly_decided || existing_execution_tool_call_id != execution_tool_call_id {
        sqlx::query(
            r#"
            UPDATE workspace_action_reviews
            SET status = $3,
                decided_by = $4,
                deny_reason = $5,
                decided_at = $6,
                execution_tool_call_id = $7
            WHERE workspace_id = $1 AND id = $2
            "#,
        )
        .bind(request.workspace_id.to_string())
        .bind(request.review_id)
        .bind(desired_status.as_str())
        .bind(&decided_by)
        .bind(deny_reason.as_deref())
        .bind(decided_at)
        .bind(execution_tool_call_id)
        .execute(&mut *tx)
        .await
        .map_err(db_error)?;
    }
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
    let decision_event_recorded_at = row
        .try_get::<Option<DateTime<Utc>>, _>("decision_event_recorded_at")
        .map_err(db_error)?;
    let execution_requested_at = row
        .try_get::<Option<DateTime<Utc>>, _>("execution_requested_at")
        .map_err(db_error)?;
    let tool_request =
        if matches!(decision, ActionReviewDecision::Cleared) && execution_requested_at.is_none() {
            let mut tool_request = serde_json::from_value::<ToolCallRequest>(
                row.try_get::<serde_json::Value, _>("tool_request")
                    .map_err(db_error)?,
            )
            .map_err(|error| TerminalError::new(format!("decode stored tool request: {error}")))?;
            let execution_tool_call_id = execution_tool_call_id.ok_or_else(|| {
                TerminalError::new("cleared action review did not have an execution tool id")
            })?;
            tool_request.tool_call_id = ToolCallId(execution_tool_call_id);
            tool_request.provider_tool_use_id = None;
            tool_request.active_canary = None;
            Some(tool_request)
        } else {
            None
        };

    Ok(DecidedReview {
        review_id: request.review_id,
        workspace_id: request.workspace_id,
        session_id,
        decision,
        status: desired_status,
        action_class,
        decided_by,
        decided_at,
        decision_event_recorded_at,
        newly_decided,
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

async fn mark_requested_event_recorded(
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        UPDATE workspace_action_reviews
        SET requested_event_recorded_at = COALESCE(requested_event_recorded_at, NOW())
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(review_id)
    .execute(&pool)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn mark_decision_event_recorded(
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        UPDATE workspace_action_reviews
        SET decision_event_recorded_at = COALESCE(decision_event_recorded_at, NOW())
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(review_id)
    .execute(&pool)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn mark_execution_requested(
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        UPDATE workspace_action_reviews
        SET execution_requested_at = COALESCE(execution_requested_at, NOW())
        WHERE workspace_id = $1 AND id = $2
        "#,
    )
    .bind(workspace_id.to_string())
    .bind(review_id)
    .execute(&pool)
    .await
    .map_err(db_error)?;
    Ok(())
}

async fn prior_tool_result_exists(
    ctx: &Context<'_>,
    decided: &DecidedReview,
    tool_call_id: ToolCallId,
) -> Result<bool, HandlerError> {
    let Some(session_id) = decided.session_id else {
        return Ok(false);
    };
    let store = OrchestratorCtx::current().session_store.clone();
    let workspace_id = decided.workspace_id.clone();
    Ok(ctx
        .run(|| async move {
            store
                .tool_event_exists(
                    &workspace_id,
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
    workspace_id: &WorkspaceId,
    session_id: moa_core::SessionId,
    event_type: EventType,
    review_id: Uuid,
) -> Result<bool, HandlerError> {
    let store = OrchestratorCtx::current().session_store.clone();
    let workspace_id = workspace_id.clone();
    Ok(ctx
        .run(|| async move {
            store
                .action_review_event_exists(&workspace_id, session_id, event_type, review_id)
                .await
                .map(Json::from)
                .map_err(HandlerError::from)
        })
        .name("action_reviews_event_exists")
        .await?
        .into_inner())
}

fn screen_review_tool_input(request: &ToolCallRequest) -> Result<(), HandlerError> {
    let serialized_input = serde_json::to_string(&request.input)
        .map_err(|error| TerminalError::new(format!("serialize tool input: {error}")))?;
    if matches!(
        screen_tool_input_for_canary(request.active_canary.as_deref(), &serialized_input),
        ToolInputCanaryScreening::Blocked(_)
    ) {
        return Err(TerminalError::new_with_code(
            400,
            format!(
                "tool {} blocked because it leaked a protected canary token",
                request.tool_name
            ),
        )
        .into());
    }
    Ok(())
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
