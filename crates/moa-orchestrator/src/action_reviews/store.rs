//! Postgres storage for workspace action reviews.

use chrono::{DateTime, Utc};
use moa_core::{
    ActionClass, ActionReviewStatus, SessionActorRef, TenantId, ToolCallId, ToolCallRequest,
    WorkspaceId,
};
use restate_sdk::prelude::{HandlerError, TerminalError};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::services::action_reviews::{ActionReviewSummary, RequestActionReview};

/// Stored state returned after a request insert or idempotent lookup.
pub(crate) struct StoredReview {
    /// Review DTO rendered by the service.
    pub(crate) summary: ActionReviewSummary,
    /// Timestamp proving the requested event was appended.
    pub(crate) requested_event_recorded_at: Option<DateTime<Utc>>,
    /// Whether this call inserted the row rather than observing an existing row.
    pub(crate) newly_inserted: bool,
}

/// Durable row state needed to apply an action-review decision.
pub(crate) struct ReviewDecisionRow {
    /// Owning session, when the action came from a session turn.
    pub(crate) session_id: Option<moa_core::SessionId>,
    /// Action class used for decision metrics.
    pub(crate) action_class: ActionClass,
    /// Current review status.
    pub(crate) status: ActionReviewStatus,
    /// Stored tool request to execute after a clear decision.
    pub(crate) tool_request: ToolCallRequest,
    /// User that already decided the review, if any.
    pub(crate) decided_by: Option<String>,
    /// Existing denial reason, if any.
    pub(crate) deny_reason: Option<String>,
    /// Existing decision timestamp, if any.
    pub(crate) decided_at: Option<DateTime<Utc>>,
    /// Timestamp proving the decision event was appended.
    pub(crate) decision_event_recorded_at: Option<DateTime<Utc>>,
    /// Tool-call id assigned to a cleared execution.
    pub(crate) execution_tool_call_id: Option<Uuid>,
    /// Timestamp proving cleared execution was requested.
    pub(crate) execution_requested_at: Option<DateTime<Utc>>,
}

/// Decision update to persist for a review row.
pub(crate) struct ReviewDecisionUpdate {
    /// Workspace that owns the review.
    pub(crate) workspace_id: WorkspaceId,
    /// Review identifier.
    pub(crate) review_id: Uuid,
    /// New terminal status.
    pub(crate) status: ActionReviewStatus,
    /// User that decided the review.
    pub(crate) decided_by: String,
    /// Denial reason for denied reviews.
    pub(crate) deny_reason: Option<String>,
    /// Decision timestamp.
    pub(crate) decided_at: DateTime<Utc>,
    /// Tool-call id assigned to a cleared execution.
    pub(crate) execution_tool_call_id: Option<Uuid>,
}

/// Insert a pending workspace action review, or load the existing idempotent row.
pub(crate) async fn insert_review(
    pool: sqlx::PgPool,
    request: RequestActionReview,
) -> Result<StoredReview, HandlerError> {
    let tool_request = serde_json::to_value(&request.tool_request)
        .map_err(|error| TerminalError::new(format!("serialize tool request: {error}")))?;
    let envelope = serde_json::to_value(&request.envelope)
        .map_err(|error| TerminalError::new(format!("serialize envelope: {error}")))?;
    let preview = serde_json::to_value(&request.preview)
        .map_err(|error| TerminalError::new(format!("serialize preview: {error}")))?;
    let storage_workspace_id = WorkspaceId::new(request.envelope.tenant_id.to_string());
    let requested_by = session_actor_ref_to_storage(&request.envelope.requested_by);
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
    .bind(storage_workspace_id.to_string())
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
    .bind(&requested_by)
    .execute(&pool)
    .await
    .map_err(db_error)?;

    let mut stored =
        load_review_state(pool, storage_workspace_id, request.envelope.review_id).await?;
    stored.newly_inserted = insert.rows_affected() > 0;
    Ok(stored)
}

fn session_actor_ref_to_storage(actor: &SessionActorRef) -> String {
    match actor {
        SessionActorRef::Identity { id } => format!("identity:{id}"),
        SessionActorRef::Contact { id } => format!("contact:{id}"),
        SessionActorRef::Anonymous => "anonymous".to_string(),
    }
}

/// List pending reviews for one workspace.
pub(crate) async fn list_pending_reviews(
    pool: sqlx::PgPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<ActionReviewSummary>, HandlerError> {
    let rows = sqlx::query(
        r#"
        SELECT id, workspace_id, session_id, sub_agent_id, tool_call_id, tool_name,
               action_class, risk_level, input_summary, envelope, preview, status,
               requested_by, decided_by, deny_reason, created_at, decided_at
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

/// Lock and load a review row for decision processing.
pub(crate) async fn load_review_for_update(
    tx: &mut Transaction<'_, Postgres>,
    workspace_id: &WorkspaceId,
    review_id: Uuid,
) -> Result<ReviewDecisionRow, HandlerError> {
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
    .bind(workspace_id.to_string())
    .bind(review_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error)?
    .ok_or_else(|| TerminalError::new_with_code(404, "action review not found"))?;

    Ok(ReviewDecisionRow {
        session_id: row
            .try_get::<Option<Uuid>, _>("session_id")
            .map_err(db_error)?
            .map(moa_core::SessionId),
        action_class: parse_db_enum(
            "action_class",
            row.try_get::<String, _>("action_class").map_err(db_error)?,
        )?,
        status: parse_db_enum(
            "status",
            row.try_get::<String, _>("status").map_err(db_error)?,
        )?,
        tool_request: serde_json::from_value(
            row.try_get::<serde_json::Value, _>("tool_request")
                .map_err(db_error)?,
        )
        .map_err(|error| TerminalError::new(format!("decode stored tool request: {error}")))?,
        decided_by: row.try_get("decided_by").map_err(db_error)?,
        deny_reason: row.try_get("deny_reason").map_err(db_error)?,
        decided_at: row.try_get("decided_at").map_err(db_error)?,
        decision_event_recorded_at: row
            .try_get("decision_event_recorded_at")
            .map_err(db_error)?,
        execution_tool_call_id: row.try_get("execution_tool_call_id").map_err(db_error)?,
        execution_requested_at: row.try_get("execution_requested_at").map_err(db_error)?,
    })
}

/// Persist a terminal review decision.
pub(crate) async fn update_review_decision(
    tx: &mut Transaction<'_, Postgres>,
    update: ReviewDecisionUpdate,
) -> Result<(), HandlerError> {
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
    .bind(update.workspace_id.to_string())
    .bind(update.review_id)
    .bind(update.status.as_str())
    .bind(&update.decided_by)
    .bind(update.deny_reason.as_deref())
    .bind(update.decided_at)
    .bind(update.execution_tool_call_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    Ok(())
}

/// Mark the request event as durably recorded.
pub(crate) async fn mark_requested_event_recorded(
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

/// Mark the decision event as durably recorded.
pub(crate) async fn mark_decision_event_recorded(
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

/// Mark a cleared review execution as requested.
pub(crate) async fn mark_execution_requested(
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
               created_at, decided_at
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

fn summary_from_row(row: &sqlx::postgres::PgRow) -> Result<ActionReviewSummary, HandlerError> {
    Ok(ActionReviewSummary {
        id: row.try_get("id").map_err(db_error)?,
        tenant_id: TenantId::from(
            Uuid::parse_str(&row.try_get::<String, _>("workspace_id").map_err(db_error)?)
                .map_err(|error| TerminalError::new(format!("decode review tenant id: {error}")))?,
        ),
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
        decided_at: row.try_get("decided_at").map_err(db_error)?,
    })
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
