//! Postgres storage for tenant action reviews.

use chrono::{DateTime, Utc};
use moa_core::{
    types::action_policy::ActionClass, types::action_policy::ActionEnvelope,
    types::action_policy::ActionReviewStatus, types::action_policy::ExecutionTaskOrigin,
    types::contact::SessionActorRef, types::identifiers::StoragePartitionId,
    types::identifiers::TenantId, types::identifiers::ToolCallId, types::tools::ToolCallRequest,
};
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::services::action_reviews::{ActionReviewSummary, RequestActionReview};
use moa_execution::{
    state::ExecutionTaskId,
    wire::{ExecutionActionReviewResolution, ExecutionActionReviewResolutionRequest},
};
use moa_observability::propagation::ValidatedTraceContext;

const EXECUTION_REVIEW_OUTBOX_CLAIM_TIMEOUT_SECS: i64 = 300;

/// Stored state returned after a request insert or idempotent lookup.
pub(crate) struct StoredReview {
    /// Review DTO rendered by the service.
    pub(crate) summary: ActionReviewSummary,
    /// Timestamp proving the requested event was appended.
    pub(crate) requested_event_recorded_at: Option<DateTime<Utc>>,
    /// Whether this call inserted the row rather than observing an existing row.
    pub(crate) newly_inserted: bool,
    /// First validated execution-task context stored with the review.
    execution_task_trace_context: Option<ValidatedTraceContext>,
}

/// Durable row state needed to apply an action-review decision.
pub(crate) struct ReviewDecisionRow {
    /// Owning session, when the action came from a session turn.
    pub(crate) session_id: Option<moa_core::types::identifiers::SessionId>,
    /// Action class used for decision metrics.
    pub(crate) action_class: ActionClass,
    /// Timestamp the review was created, used for approval-wait metrics.
    pub(crate) created_at: DateTime<Utc>,
    /// Current review status.
    pub(crate) status: ActionReviewStatus,
    /// Stored tool request to execute after a clear decision.
    pub(crate) tool_request: ToolCallRequest,
    /// Dynamic execution task that owns cleared dispatch, when present.
    pub(crate) execution_origin: Option<ExecutionTaskOrigin>,
    /// Original execution-task context stored when the review was created.
    pub(crate) execution_task_trace_context: Option<ValidatedTraceContext>,
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
    /// Storage partition that owns the review.
    pub(crate) storage_partition_id: StoragePartitionId,
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

/// One claimed execution-origin review resolution awaiting Restate delivery.
pub(crate) struct ClaimedExecutionReviewResolution {
    /// Stable outbox and review identifier.
    pub(crate) review_uid: Uuid,
    /// Exact keyed task request.
    pub(crate) request: ExecutionActionReviewResolutionRequest,
    /// Persisted attempt generation used to fence acknowledgement updates.
    pub(crate) attempt_count: i32,
    /// Immutable resolution context reinjected on every callback retry.
    pub(crate) resolution_trace_context: Option<ValidatedTraceContext>,
    /// Immutable execution-task context linked on every callback retry.
    pub(crate) task_trace_context: Option<ValidatedTraceContext>,
}

/// Insert a pending tenant action review, or load the existing idempotent row.
///
/// `review_timeout_secs` sets the row's `expires_at` relative to insertion so
/// the action-review reaper can fail an undecided review closed.
pub(crate) async fn insert_review(
    pool: sqlx::PgPool,
    request: RequestActionReview,
    review_timeout_secs: i64,
    execution_task_trace_context: Option<ValidatedTraceContext>,
) -> Result<StoredReview, HandlerError> {
    let tool_request = serde_json::to_value(&request.tool_request)
        .map_err(|error| TerminalError::new(format!("serialize tool request: {error}")))?;
    let envelope = serde_json::to_value(&request.envelope)
        .map_err(|error| TerminalError::new(format!("serialize envelope: {error}")))?;
    let preview = serde_json::to_value(&request.preview)
        .map_err(|error| TerminalError::new(format!("serialize preview: {error}")))?;
    let tenant_id = request.envelope.tenant_id;
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let requested_by = session_actor_ref_to_storage(&request.envelope.requested_by);
    let execution_task_trace_context = request
        .envelope
        .execution_origin
        .is_some()
        .then_some(execution_task_trace_context)
        .flatten();
    let insert = sqlx::query(
        r#"
        INSERT INTO tenant_action_reviews (
            id, tenant_id, storage_partition_id, user_id, session_id, worker_id, tool_call_id, tool_name,
            action_class, risk_level, input_summary, normalized_input, envelope,
            preview, tool_request, requested_by, execution_task_traceparent,
            execution_task_tracestate, expires_at
        )
        VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                $16, $17, NOW() + ($18 || ' seconds')::INTERVAL)
        ON CONFLICT (id) DO NOTHING
        "#,
    )
    .bind(request.envelope.review_id)
    .bind(tenant_id.0)
    .bind(storage_partition_id.to_string())
    .bind(request.envelope.session_id.map(|id| id.0))
    .bind(request.envelope.worker_id.clone())
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
    .bind(
        execution_task_trace_context
            .as_ref()
            .map(ValidatedTraceContext::traceparent),
    )
    .bind(
        execution_task_trace_context
            .as_ref()
            .and_then(ValidatedTraceContext::tracestate),
    )
    .bind(review_timeout_secs.to_string())
    .execute(&pool)
    .await
    .map_err(db_error)?;

    let mut stored =
        load_review_state(pool, storage_partition_id, request.envelope.review_id).await?;
    stored.newly_inserted = insert.rows_affected() > 0;
    if !stored.newly_inserted && stored.execution_task_trace_context != execution_task_trace_context
    {
        return Err(TerminalError::new_with_code(
            409,
            "action review replay conflicts with the first execution-task trace context",
        )
        .into());
    }
    Ok(stored)
}

fn session_actor_ref_to_storage(actor: &SessionActorRef) -> String {
    match actor {
        SessionActorRef::Identity { id } => format!("identity:{id}"),
        SessionActorRef::Contact { id } => format!("contact:{id}"),
        SessionActorRef::Anonymous => "anonymous".to_string(),
    }
}

/// List pending reviews for one tenant storage partition.
pub(crate) async fn list_pending_reviews(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
) -> Result<Vec<ActionReviewSummary>, HandlerError> {
    let rows = sqlx::query(
        r#"
        SELECT id, tenant_id, storage_partition_id, session_id, worker_id, tool_call_id, tool_name,
               action_class, risk_level, input_summary, envelope, preview, status,
               requested_by, decided_by, deny_reason, created_at, decided_at
        FROM tenant_action_reviews
        WHERE storage_partition_id = $1 AND status = 'pending'
        ORDER BY created_at DESC
        LIMIT 100
        "#,
    )
    .bind(storage_partition_id.to_string())
    .fetch_all(&pool)
    .await
    .map_err(db_error)?;

    rows.iter().map(summary_from_row).collect()
}

/// Lock and load a review row for decision processing.
pub(crate) async fn load_review_for_update(
    tx: &mut Transaction<'_, Postgres>,
    storage_partition_id: &StoragePartitionId,
    review_id: Uuid,
) -> Result<ReviewDecisionRow, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, tenant_id, storage_partition_id, session_id, action_class, status, tool_request, envelope,
               decided_by, deny_reason, created_at, decided_at, decision_event_recorded_at,
               execution_tool_call_id, execution_requested_at,
               execution_task_traceparent, execution_task_tracestate
        FROM tenant_action_reviews
        WHERE storage_partition_id = $1 AND id = $2
        FOR UPDATE
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(review_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_error)?
    .ok_or_else(|| TerminalError::new_with_code(404, "action review not found"))?;

    let envelope: ActionEnvelope = serde_json::from_value(
        row.try_get::<serde_json::Value, _>("envelope")
            .map_err(db_error)?,
    )
    .map_err(|error| TerminalError::new(format!("decode stored action envelope: {error}")))?;
    Ok(ReviewDecisionRow {
        session_id: row
            .try_get::<Option<Uuid>, _>("session_id")
            .map_err(db_error)?
            .map(moa_core::types::identifiers::SessionId),
        action_class: parse_db_enum(
            "action_class",
            row.try_get::<String, _>("action_class").map_err(db_error)?,
        )?,
        created_at: row.try_get("created_at").map_err(db_error)?,
        status: parse_db_enum(
            "status",
            row.try_get::<String, _>("status").map_err(db_error)?,
        )?,
        tool_request: serde_json::from_value(
            row.try_get::<serde_json::Value, _>("tool_request")
                .map_err(db_error)?,
        )
        .map_err(|error| TerminalError::new(format!("decode stored tool request: {error}")))?,
        execution_origin: envelope.execution_origin,
        execution_task_trace_context: trace_context_from_columns(
            row.try_get("execution_task_traceparent")
                .map_err(db_error)?,
            row.try_get("execution_task_tracestate").map_err(db_error)?,
        ),
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
        UPDATE tenant_action_reviews
        SET status = $3,
            decided_by = $4,
            deny_reason = $5,
            decided_at = $6,
            execution_tool_call_id = $7
        WHERE storage_partition_id = $1 AND id = $2
        "#,
    )
    .bind(update.storage_partition_id.to_string())
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

/// Claims one cleared execution-origin review while leaving it nonterminal until tool completion.
pub(crate) async fn claim_execution_review(
    tx: &mut Transaction<'_, Postgres>,
    storage_partition_id: &StoragePartitionId,
    review_id: Uuid,
    decided_by: &str,
    decided_at: DateTime<Utc>,
    execution_tool_call_id: Uuid,
) -> Result<(), HandlerError> {
    let result = sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET decided_by = $3,
            decided_at = $4,
            execution_tool_call_id = $5,
            execution_requested_at = NOW()
        WHERE storage_partition_id = $1
          AND id = $2
          AND status = 'pending'
          AND execution_requested_at IS NULL
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(review_id)
    .bind(decided_by)
    .bind(decided_at)
    .bind(execution_tool_call_id)
    .execute(&mut **tx)
    .await
    .map_err(db_error)?;
    if result.rows_affected() != 1 {
        return Err(TerminalError::new_with_code(
            409,
            "action review cleared execution is already in progress",
        )
        .into());
    }
    Ok(())
}

/// Atomically inserts the execution-task resolution owned by a terminal review.
pub(crate) async fn insert_execution_review_resolution(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: TenantId,
    review_id: Uuid,
    origin: ExecutionTaskOrigin,
    resolution: &ExecutionActionReviewResolution,
    resolution_trace_context: Option<&ValidatedTraceContext>,
    task_trace_context: Option<&ValidatedTraceContext>,
) -> Result<(), HandlerError> {
    install_control_plane_scope(tx).await.map_err(db_error)?;
    let inserted = insert_execution_review_resolution_sql(
        tx,
        tenant_id,
        review_id,
        origin,
        resolution,
        resolution_trace_context,
        task_trace_context,
    )
    .await
    .map_err(db_error)?;
    if !inserted {
        return Err(TerminalError::new(
            "execution-origin action review does not match a persisted execution task",
        )
        .into());
    }
    Ok(())
}

/// Mark the request event as durably recorded.
pub(crate) async fn mark_requested_event_recorded(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET requested_event_recorded_at = COALESCE(requested_event_recorded_at, NOW())
        WHERE storage_partition_id = $1 AND id = $2
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(review_id)
    .execute(&pool)
    .await
    .map_err(db_error)?;
    Ok(())
}

/// Mark the decision event as durably recorded.
pub(crate) async fn mark_decision_event_recorded(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET decision_event_recorded_at = COALESCE(decision_event_recorded_at, NOW())
        WHERE storage_partition_id = $1 AND id = $2
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(review_id)
    .execute(&pool)
    .await
    .map_err(db_error)?;
    Ok(())
}

/// Mark a cleared review execution as requested.
pub(crate) async fn mark_execution_requested(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
    review_id: Uuid,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET execution_requested_at = COALESCE(execution_requested_at, NOW())
        WHERE storage_partition_id = $1 AND id = $2
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(review_id)
    .execute(&pool)
    .await
    .map_err(db_error)?;
    Ok(())
}

/// One review the reaper transitioned from `pending` to `timeout`.
pub(crate) struct TimedOutReview {
    /// Action class used for the timeout decision metric.
    pub(crate) action_class: ActionClass,
    /// Creation timestamp, used for the approval-wait metric.
    pub(crate) created_at: DateTime<Utc>,
    /// Timeout timestamp, recorded as the decision time.
    pub(crate) decided_at: DateTime<Utc>,
}

/// Fail every expired pending review closed in one statement.
///
/// A `timeout` row is terminal: [`super::app::decide_review`] rejects any later
/// clear because the status is no longer `pending`, so the gated tool never
/// executes. Runs unscoped across storage partitions because the reaper is a
/// deployment-global background job, not a tenant request.
pub(crate) async fn timeout_expired_reviews(
    pool: &sqlx::PgPool,
    resolution_trace_context: Option<&ValidatedTraceContext>,
) -> Result<Vec<TimedOutReview>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    install_control_plane_scope(&mut tx).await?;
    let rows = sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET status = 'timeout',
            decided_at = NOW(),
            deny_reason = COALESCE(deny_reason, 'review expired without a decision')
        WHERE status = 'pending'
          AND execution_requested_at IS NULL
          AND expires_at <= NOW()
        RETURNING id, tenant_id, action_class, created_at, decided_at, envelope,
                  execution_task_traceparent, execution_task_tracestate
        "#,
    )
    .fetch_all(&mut *tx)
    .await?;
    for row in &rows {
        let envelope: ActionEnvelope = serde_json::from_value(row.try_get("envelope")?)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        if let Some(origin) = envelope.execution_origin {
            let reason = "review expired without a decision".to_string();
            let task_trace_context = trace_context_from_columns(
                row.try_get("execution_task_traceparent")?,
                row.try_get("execution_task_tracestate")?,
            );
            let inserted = insert_execution_review_resolution_sql(
                &mut tx,
                TenantId::from(row.try_get::<Uuid, _>("tenant_id")?),
                row.try_get("id")?,
                origin,
                &ExecutionActionReviewResolution::TimedOut { reason },
                resolution_trace_context,
                task_trace_context.as_ref(),
            )
            .await?;
            if !inserted {
                return Err(sqlx::Error::Protocol(
                    "timed-out execution review has no matching execution task".to_string(),
                ));
            }
        }
    }
    let timed_out = rows
        .iter()
        .map(|row| {
            let action_class = row
                .try_get::<String, _>("action_class")?
                .parse::<ActionClass>()
                .map_err(|_| sqlx::Error::Decode("unknown action_class".into()))?;
            Ok(TimedOutReview {
                action_class,
                created_at: row.try_get("created_at")?,
                decided_at: row.try_get("decided_at")?,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    tx.commit().await?;
    Ok(timed_out)
}

/// Claims one bounded outbox batch with a persisted attempt count and stale-lease recovery.
pub(crate) async fn claim_execution_review_resolutions(
    pool: &sqlx::PgPool,
    limit: i64,
) -> Result<Vec<ClaimedExecutionReviewResolution>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    install_control_plane_scope(&mut tx).await?;
    let rows = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT review_uid
            FROM moa.execution_action_review_outbox
            WHERE delivered_at IS NULL
              AND next_attempt_at <= NOW()
              AND (
                  claimed_at IS NULL
                  OR claimed_at <= NOW() - ($2 || ' seconds')::INTERVAL
              )
            ORDER BY next_attempt_at, created_at, review_uid
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        UPDATE moa.execution_action_review_outbox AS outbox
        SET attempt_count = outbox.attempt_count + 1,
            claimed_at = NOW(),
            updated_at = NOW()
        FROM candidates
        WHERE outbox.review_uid = candidates.review_uid
        RETURNING outbox.review_uid, outbox.run_uid, outbox.task_id,
                  outbox.generation, outbox.resolution, outbox.attempt_count,
                  outbox.traceparent, outbox.tracestate,
                  outbox.task_traceparent, outbox.task_tracestate
        "#,
    )
    .bind(limit)
    .bind(EXECUTION_REVIEW_OUTBOX_CLAIM_TIMEOUT_SECS.to_string())
    .fetch_all(&mut *tx)
    .await?;
    let claimed = rows
        .iter()
        .map(|row| {
            let task_id = ExecutionTaskId::from_uuid(row.try_get("task_id")?);
            let resolution = serde_json::from_value::<ExecutionActionReviewResolution>(
                row.try_get::<Value, _>("resolution")?,
            )
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            let review_uid = row.try_get("review_uid")?;
            Ok(ClaimedExecutionReviewResolution {
                review_uid,
                request: ExecutionActionReviewResolutionRequest {
                    run_uid: row.try_get("run_uid")?,
                    task_id,
                    generation: u64::try_from(row.try_get::<i64, _>("generation")?)
                        .map_err(|error| sqlx::Error::Decode(Box::new(error)))?,
                    review_uid,
                    resolution,
                },
                attempt_count: row.try_get("attempt_count")?,
                resolution_trace_context: trace_context_from_columns(
                    row.try_get("traceparent")?,
                    row.try_get("tracestate")?,
                ),
                task_trace_context: trace_context_from_columns(
                    row.try_get("task_traceparent")?,
                    row.try_get("task_tracestate")?,
                ),
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    tx.commit().await?;
    Ok(claimed)
}

/// Marks one exact claimed attempt delivered after a generation-fenced acknowledgement.
pub(crate) async fn mark_execution_review_delivered(
    pool: &sqlx::PgPool,
    review_uid: Uuid,
    attempt_count: i32,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    install_control_plane_scope(&mut tx).await?;
    let result = sqlx::query(
        r#"
        UPDATE moa.execution_action_review_outbox
        SET delivered_at = NOW(), claimed_at = NULL, last_error = NULL, updated_at = NOW()
        WHERE review_uid = $1 AND attempt_count = $2 AND delivered_at IS NULL
        "#,
    )
    .bind(review_uid)
    .bind(attempt_count)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

/// Releases one failed claim with persisted bounded exponential backoff.
pub(crate) async fn mark_execution_review_failed(
    pool: &sqlx::PgPool,
    review_uid: Uuid,
    attempt_count: i32,
    error: &str,
) -> Result<bool, sqlx::Error> {
    let backoff_seconds =
        1_i64 << u32::try_from(attempt_count.saturating_sub(1).min(6)).unwrap_or_default();
    let mut tx = pool.begin().await?;
    install_control_plane_scope(&mut tx).await?;
    let result = sqlx::query(
        r#"
        UPDATE moa.execution_action_review_outbox
        SET claimed_at = NULL,
            next_attempt_at = NOW() + ($3 || ' seconds')::INTERVAL,
            last_error = LEFT($4, 2000),
            updated_at = NOW()
        WHERE review_uid = $1 AND attempt_count = $2 AND delivered_at IS NULL
        "#,
    )
    .bind(review_uid)
    .bind(attempt_count)
    .bind(backoff_seconds.to_string())
    .bind(error)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

async fn insert_execution_review_resolution_sql(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: TenantId,
    review_id: Uuid,
    origin: ExecutionTaskOrigin,
    resolution: &ExecutionActionReviewResolution,
    resolution_trace_context: Option<&ValidatedTraceContext>,
    task_trace_context: Option<&ValidatedTraceContext>,
) -> Result<bool, sqlx::Error> {
    let resolution =
        serde_json::to_value(resolution).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let result = sqlx::query(
        r#"
        INSERT INTO moa.execution_action_review_outbox (
            review_uid, tenant_id, contact_id, run_uid, task_id, generation, resolution,
            traceparent, tracestate, task_traceparent, task_tracestate
        )
        SELECT $1, task.tenant_id, task.contact_id, task.run_uid, task.task_id, $5, $6,
               $7, $8, $9, $10
        FROM moa.execution_task AS task
        WHERE task.run_uid = $2
          AND task.task_id = $3
          AND task.tenant_id = $4
        ON CONFLICT (review_uid) DO UPDATE
        SET next_attempt_at = NOW(),
            updated_at = NOW()
        WHERE moa.execution_action_review_outbox.run_uid = EXCLUDED.run_uid
          AND moa.execution_action_review_outbox.task_id = EXCLUDED.task_id
          AND moa.execution_action_review_outbox.generation = EXCLUDED.generation
          AND moa.execution_action_review_outbox.resolution = EXCLUDED.resolution
          AND moa.execution_action_review_outbox.traceparent
                IS NOT DISTINCT FROM EXCLUDED.traceparent
          AND moa.execution_action_review_outbox.tracestate
                IS NOT DISTINCT FROM EXCLUDED.tracestate
          AND moa.execution_action_review_outbox.task_traceparent
                IS NOT DISTINCT FROM EXCLUDED.task_traceparent
          AND moa.execution_action_review_outbox.task_tracestate
                IS NOT DISTINCT FROM EXCLUDED.task_tracestate
        "#,
    )
    .bind(review_id)
    .bind(origin.run_uid)
    .bind(origin.task_uid)
    .bind(tenant_id.0)
    .bind(i64::try_from(origin.generation).map_err(|error| sqlx::Error::Encode(Box::new(error)))?)
    .bind(resolution)
    .bind(resolution_trace_context.map(ValidatedTraceContext::traceparent))
    .bind(resolution_trace_context.and_then(ValidatedTraceContext::tracestate))
    .bind(task_trace_context.map(ValidatedTraceContext::traceparent))
    .bind(task_trace_context.and_then(ValidatedTraceContext::tracestate))
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn install_control_plane_scope(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_catalog.set_config('moa.control_plane', 'true', true)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Pending-review queue snapshot used to publish operator gauges.
pub(crate) struct PendingReviewStats {
    /// Pending review count per bounded risk level (`low`/`medium`/`high`).
    pub(crate) depth_by_risk: Vec<(String, i64)>,
    /// Age in seconds of the oldest pending review, or `0.0` when the queue is empty.
    pub(crate) oldest_pending_age_seconds: f64,
}

/// Sample the pending-review queue for gauge emission.
pub(crate) async fn pending_review_stats(
    pool: &sqlx::PgPool,
) -> Result<PendingReviewStats, sqlx::Error> {
    let depth_rows = sqlx::query(
        r#"
        SELECT risk_level, COUNT(*) AS depth
        FROM tenant_action_reviews
        WHERE status = 'pending'
        GROUP BY risk_level
        "#,
    )
    .fetch_all(pool)
    .await?;
    let depth_by_risk = depth_rows
        .iter()
        .map(|row| {
            Ok((
                row.try_get::<String, _>("risk_level")?,
                row.try_get::<i64, _>("depth")?,
            ))
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;

    let oldest_age: Option<f64> = sqlx::query_scalar(
        r#"
        SELECT EXTRACT(EPOCH FROM (NOW() - MIN(created_at)))::DOUBLE PRECISION
        FROM tenant_action_reviews
        WHERE status = 'pending'
        "#,
    )
    .fetch_one(pool)
    .await?;

    Ok(PendingReviewStats {
        depth_by_risk,
        oldest_pending_age_seconds: oldest_age.unwrap_or(0.0).max(0.0),
    })
}

async fn load_review_state(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
    review_id: Uuid,
) -> Result<StoredReview, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, tenant_id, storage_partition_id, session_id, worker_id, tool_call_id, tool_name,
               action_class, risk_level, input_summary, envelope, preview, status,
               requested_by, requested_event_recorded_at, decided_by, deny_reason,
               created_at, decided_at, execution_task_traceparent, execution_task_tracestate
        FROM tenant_action_reviews
        WHERE storage_partition_id = $1 AND id = $2
        "#,
    )
    .bind(storage_partition_id.to_string())
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
        execution_task_trace_context: trace_context_from_columns(
            row.try_get("execution_task_traceparent")
                .map_err(db_error)?,
            row.try_get("execution_task_tracestate").map_err(db_error)?,
        ),
    })
}

fn trace_context_from_columns(
    traceparent: Option<String>,
    tracestate: Option<String>,
) -> Option<ValidatedTraceContext> {
    ValidatedTraceContext::new(traceparent.as_deref(), tracestate.as_deref())
}

fn summary_from_row(row: &sqlx::postgres::PgRow) -> Result<ActionReviewSummary, HandlerError> {
    Ok(ActionReviewSummary {
        id: row.try_get("id").map_err(db_error)?,
        tenant_id: TenantId::from(row.try_get::<Uuid, _>("tenant_id").map_err(db_error)?),
        session_id: row
            .try_get::<Option<Uuid>, _>("session_id")
            .map_err(db_error)?
            .map(moa_core::types::identifiers::SessionId),
        worker_id: row.try_get("worker_id").map_err(db_error)?,
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

#[cfg(test)]
mod tests {
    use super::trace_context_from_columns;

    const VALID_PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn invalid_persisted_parent_fails_closed_to_no_context() {
        // Pins: corrupted legacy trace storage can never be reinjected as an outbox parent.
        let context = trace_context_from_columns(
            Some("00-invalid".to_string()),
            Some("vendor=value".to_string()),
        );

        assert_eq!(context, None);
    }

    #[test]
    fn invalid_persisted_state_drops_only_state() {
        // Pins: a valid parent remains causal when unsafe tracestate is discarded.
        let context = trace_context_from_columns(
            Some(VALID_PARENT.to_string()),
            Some("vendor=unsafe\nstate".to_string()),
        )
        .expect("valid parent should survive invalid state");

        assert_eq!(context.traceparent(), VALID_PARENT);
        assert_eq!(context.tracestate(), None);
    }
}
