//! Postgres storage for tenant action reviews.

use chrono::{DateTime, Utc};
use moa_core::{
    types::action_policy::ActionClass, types::action_policy::ActionEnvelope,
    types::action_policy::ActionReviewOwner, types::action_policy::ActionReviewRelease,
    types::action_policy::ActionReviewStatus, types::contact::SessionActorRef,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::identifiers::ToolCallId, types::tools::ToolCallRequest,
};
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::services::action_reviews::{ActionReviewSummary, RequestActionReview};
use moa_execution::{
    state::{CompensationId, ExecutionTaskId},
    wire::{
        ExecutionActionReviewResolution, ExecutionActionReviewResolutionRequest,
        ExecutionCompensationReviewResolutionRequest,
    },
};
use moa_observability::propagation::ValidatedTraceContext;

const EXECUTION_REVIEW_OUTBOX_CLAIM_TIMEOUT_SECS: i64 = 300;

/// Stored state returned after a request insert or idempotent lookup.
pub(crate) struct StoredReview {
    /// Review DTO rendered by the service.
    pub(crate) summary: ActionReviewSummary,
    /// Whether this call inserted the row rather than observing an existing row.
    pub(crate) newly_inserted: bool,
    /// Timestamp proving the owner acknowledged registration.
    pub(crate) owner_registered_at: Option<DateTime<Utc>>,
    /// Canonical persisted identity of the first request using this review id.
    request_identity: Option<Value>,
}

/// Durable row state needed to apply an action-review decision.
pub(crate) struct ReviewDecisionRow {
    /// Exact owner resumed when this review resolves.
    pub(crate) owner: ActionReviewOwner,
    /// Reviewed tool name, carried into the typed resolution receipt.
    pub(crate) tool_name: String,
    /// Action class used for decision metrics.
    pub(crate) action_class: ActionClass,
    /// Timestamp the review was created, used for approval-wait metrics.
    pub(crate) created_at: DateTime<Utc>,
    /// Current review status.
    pub(crate) status: ActionReviewStatus,
    /// Timestamp proving the owner acknowledged registration.
    pub(crate) owner_registered_at: Option<DateTime<Utc>>,
    /// Stored tool request to execute after a clear decision.
    pub(crate) tool_request: ToolCallRequest,
    /// Original execution-operation context stored when the review was created.
    pub(crate) execution_task_trace_context: Option<ValidatedTraceContext>,
    /// User that already decided the review, if any.
    pub(crate) decided_by: Option<String>,
    /// Existing denial reason, if any.
    pub(crate) deny_reason: Option<String>,
    /// Existing decision timestamp, if any.
    pub(crate) decided_at: Option<DateTime<Utc>>,
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
    pub(crate) request: ClaimedExecutionReviewRequest,
    /// Persisted attempt generation used to fence acknowledgement updates.
    pub(crate) attempt_count: i32,
    /// Immutable resolution context reinjected on every callback retry.
    pub(crate) resolution_trace_context: Option<ValidatedTraceContext>,
    /// Immutable execution-operation context linked on every callback retry.
    pub(crate) task_trace_context: Option<ValidatedTraceContext>,
}

/// Exact workflow request targeted by one claimed execution-review outbox row.
pub(crate) enum ClaimedExecutionReviewRequest {
    /// Forward execution-task review resolution.
    Task(ExecutionActionReviewResolutionRequest),
    /// Compensation review resolution.
    Compensation(ExecutionCompensationReviewResolutionRequest),
}

/// One timed-out review awaiting release from its conversational owner.
pub(crate) struct PendingActionReviewRelease {
    /// Durable timestamp recorded when the review timed out.
    pub(crate) timed_out_at: DateTime<Utc>,
    /// Typed release request sent to the exact owner.
    pub(crate) release: ActionReviewRelease,
}

/// Exact tenant action-review row requested by a durable timeout delivery.
pub(crate) struct ActionReviewTimeoutLookup {
    /// Tenant that owns the review and supplies the control-plane isolation fence.
    pub(crate) tenant_id: TenantId,
    /// Stable review identifier carried by the delayed trigger.
    pub(crate) review_id: Uuid,
}

/// Persisted action-review state used to fence one durable timeout delivery.
pub(crate) struct ActionReviewTimeoutSnapshot {
    /// Exact owner incarnation stored when the review was created.
    pub(crate) owner: ActionReviewOwner,
    /// Current typed review status.
    pub(crate) status: ActionReviewStatus,
    /// Whether the persisted expiry is due at the database clock.
    pub(crate) is_due: bool,
    /// Whether the owner durably acknowledged review registration.
    pub(crate) owner_registered: bool,
    /// Timestamp proving durable execution already claimed a clear decision.
    pub(crate) execution_requested_at: Option<DateTime<Utc>>,
    /// Persisted terminal decision timestamp.
    pub(crate) decided_at: Option<DateTime<Utc>>,
    /// Timestamp proving conversational owner release delivery completed.
    pub(crate) owner_release_delivered_at: Option<DateTime<Utc>>,
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
    let execution_task_trace_context = (request.envelope.owner.execution_origin().is_some()
        || request.envelope.owner.compensation_origin().is_some())
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
    .bind(request.envelope.owner.session_id().0)
    .bind(request.envelope.owner.worker_id().cloned())
    .bind(request.envelope.tool_call_id.0)
    .bind(&request.envelope.tool_name)
    .bind(request.envelope.action_class.as_str())
    .bind(request.envelope.risk_level.as_str())
    .bind(&request.envelope.input_summary)
    .bind(&request.envelope.normalized_input)
    .bind(&envelope)
    .bind(&preview)
    .bind(&tool_request)
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

    let newly_inserted = insert.rows_affected() > 0;
    let mut stored = load_review_state(
        pool,
        storage_partition_id,
        request.envelope.review_id,
        !newly_inserted,
    )
    .await?;
    stored.newly_inserted = newly_inserted;
    if !newly_inserted {
        let request_identity = serde_json::json!({
            "envelope": envelope,
            "preview": preview,
            "tool_request": tool_request,
        });
        if stored.request_identity.as_ref() != Some(&request_identity) {
            return Err(TerminalError::new_with_code(
                409,
                "action review id conflicts with the first canonical request",
            )
            .into());
        }
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
               requested_by, decided_by, deny_reason, created_at, expires_at, decided_at
        FROM tenant_action_reviews
        WHERE storage_partition_id = $1
          AND status = 'pending'
          AND owner_registered_at IS NOT NULL
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
        SELECT id, tenant_id, storage_partition_id, action_class, status, tool_request, envelope,
               decided_by, deny_reason, created_at, decided_at,
               execution_tool_call_id, execution_requested_at,
               execution_task_traceparent, execution_task_tracestate, owner_registered_at
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
        owner: envelope.owner.clone(),
        tool_name: envelope.tool_name.clone(),
        action_class: parse_db_enum(
            "action_class",
            row.try_get::<String, _>("action_class").map_err(db_error)?,
        )?,
        created_at: row.try_get("created_at").map_err(db_error)?,
        status: parse_db_enum(
            "status",
            row.try_get::<String, _>("status").map_err(db_error)?,
        )?,
        owner_registered_at: row.try_get("owner_registered_at").map_err(db_error)?,
        tool_request: serde_json::from_value(
            row.try_get::<serde_json::Value, _>("tool_request")
                .map_err(db_error)?,
        )
        .map_err(|error| TerminalError::new(format!("decode stored tool request: {error}")))?,
        execution_task_trace_context: trace_context_from_columns(
            row.try_get("execution_task_traceparent")
                .map_err(db_error)?,
            row.try_get("execution_task_tracestate").map_err(db_error)?,
        ),
        decided_by: row.try_get("decided_by").map_err(db_error)?,
        deny_reason: row.try_get("deny_reason").map_err(db_error)?,
        decided_at: row.try_get("decided_at").map_err(db_error)?,
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

/// Atomically clears and claims one conversational review for execution.
pub(crate) async fn claim_conversational_review(
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
        SET status = 'cleared',
            decided_by = $3,
            decided_at = $4,
            deny_reason = NULL,
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
            "action review cleared execution is already claimed",
        )
        .into());
    }
    Ok(())
}

/// Atomically inserts the execution-operation resolution owned by a terminal review.
pub(crate) async fn insert_execution_review_resolution(
    tx: &mut Transaction<'_, Postgres>,
    tenant_id: TenantId,
    review_id: Uuid,
    owner: &ActionReviewOwner,
    resolution: &ExecutionActionReviewResolution,
    resolution_trace_context: Option<&ValidatedTraceContext>,
    task_trace_context: Option<&ValidatedTraceContext>,
) -> Result<(), HandlerError> {
    install_control_plane_scope(tx).await.map_err(db_error)?;
    let inserted = insert_execution_review_resolution_sql(
        tx,
        tenant_id,
        review_id,
        owner,
        resolution,
        resolution_trace_context,
        task_trace_context,
    )
    .await
    .map_err(db_error)?;
    if !inserted {
        return Err(TerminalError::new(
            "execution-origin action review does not match its persisted operation",
        )
        .into());
    }
    Ok(())
}

/// Marks the review ready only after its owner acknowledged registration.
pub(crate) async fn mark_owner_registered(
    pool: sqlx::PgPool,
    storage_partition_id: StoragePartitionId,
    review_id: Uuid,
    expected_owner: Option<&ActionReviewOwner>,
) -> Result<(), HandlerError> {
    let expected_owner = expected_owner
        .map(serde_json::to_value)
        .transpose()
        .map_err(|error| TerminalError::new(format!("serialize action review owner: {error}")))?;
    let result = sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET owner_registered_at = COALESCE(owner_registered_at, NOW())
        WHERE storage_partition_id = $1
          AND id = $2
          AND status = 'pending'
          AND ($3::JSONB IS NULL OR envelope -> 'owner' = $3)
        "#,
    )
    .bind(storage_partition_id.to_string())
    .bind(review_id)
    .bind(expected_owner)
    .execute(&pool)
    .await
    .map_err(db_error)?;
    if result.rows_affected() != 1 {
        return Err(TerminalError::new_with_code(
            409,
            "action review is no longer pending during owner registration",
        )
        .into());
    }
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
          AND owner_registered_at IS NOT NULL
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
        if envelope.owner.execution_origin().is_some()
            || envelope.owner.compensation_origin().is_some()
        {
            let reason = "review expired without a decision".to_string();
            let task_trace_context = trace_context_from_columns(
                row.try_get("execution_task_traceparent")?,
                row.try_get("execution_task_tracestate")?,
            );
            let inserted = insert_execution_review_resolution_sql(
                &mut tx,
                TenantId::from(row.try_get::<Uuid, _>("tenant_id")?),
                row.try_get("id")?,
                &envelope.owner,
                &ExecutionActionReviewResolution::TimedOut { reason },
                resolution_trace_context,
                task_trace_context.as_ref(),
            )
            .await?;
            if !inserted {
                return Err(sqlx::Error::Protocol(
                    "timed-out execution review has no matching execution operation".to_string(),
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

/// Loads one exact action-review timeout snapshot under control-plane scope.
///
/// Both tenant and review identity are matched before the stored owner is
/// returned, so the caller can compare the complete owner-generation fence
/// without crossing the action-review storage boundary.
pub(crate) async fn load_action_review_timeout_snapshot(
    pool: &sqlx::PgPool,
    request: ActionReviewTimeoutLookup,
) -> Result<Option<ActionReviewTimeoutSnapshot>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    install_control_plane_scope(&mut tx).await?;
    let row = sqlx::query(
        r#"
        SELECT envelope, status, expires_at <= NOW() AS is_due,
               owner_registered_at IS NOT NULL AS owner_registered,
               execution_requested_at, decided_at, owner_release_delivered_at
        FROM tenant_action_reviews
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(request.review_id)
    .bind(request.tenant_id.0)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    row.map(|row| {
        let envelope: ActionEnvelope = serde_json::from_value(row.try_get("envelope")?)
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
        let status = row
            .try_get::<String, _>("status")?
            .parse::<ActionReviewStatus>()
            .map_err(|_| sqlx::Error::Decode("unknown action review status".into()))?;
        Ok(ActionReviewTimeoutSnapshot {
            owner: envelope.owner,
            status,
            is_due: row.try_get("is_due")?,
            owner_registered: row.try_get("owner_registered")?,
            execution_requested_at: row.try_get("execution_requested_at")?,
            decided_at: row.try_get("decided_at")?,
            owner_release_delivered_at: row.try_get("owner_release_delivered_at")?,
        })
    })
    .transpose()
}

/// Loads one bounded batch of timed-out conversational owner releases.
pub(crate) async fn pending_action_review_releases(
    pool: &sqlx::PgPool,
    limit: i64,
) -> Result<Vec<PendingActionReviewRelease>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    install_control_plane_scope(&mut tx).await?;
    let rows = sqlx::query(
        r#"
        SELECT id, envelope, decided_at
        FROM tenant_action_reviews
        WHERE status = 'timeout'
          AND owner_registered_at IS NOT NULL
          AND owner_release_delivered_at IS NULL
          AND envelope -> 'owner' ->> 'owner' IN ('coordinator', 'worker')
        ORDER BY created_at, id
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    let pending = rows
        .iter()
        .map(|row| {
            let review_id = row.try_get("id")?;
            let envelope: ActionEnvelope = serde_json::from_value(row.try_get("envelope")?)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            Ok(PendingActionReviewRelease {
                timed_out_at: row.try_get("decided_at")?,
                release: ActionReviewRelease {
                    review_id,
                    owner: envelope.owner,
                    resume_queued: true,
                },
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()?;
    tx.commit().await?;
    Ok(pending)
}

/// Marks one timeout release delivered.
pub(crate) async fn mark_action_review_release_delivered(
    pool: &sqlx::PgPool,
    review_id: Uuid,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    install_control_plane_scope(&mut tx).await?;
    sqlx::query(
        r#"
        UPDATE tenant_action_reviews
        SET owner_release_delivered_at = COALESCE(owner_release_delivered_at, NOW())
        WHERE id = $1
          AND status = 'timeout'
        "#,
    )
    .bind(review_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
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
        RETURNING outbox.review_uid, outbox.owner_kind, outbox.run_uid,
                  outbox.operation_id, outbox.generation, outbox.resolution,
                  outbox.attempt_count,
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
            let resolution = serde_json::from_value::<ExecutionActionReviewResolution>(
                row.try_get::<Value, _>("resolution")?,
            )
            .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            let review_uid = row.try_get("review_uid")?;
            let run_uid = row.try_get("run_uid")?;
            let operation_id = row.try_get("operation_id")?;
            let generation = u64::try_from(row.try_get::<i64, _>("generation")?)
                .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
            let request = match row.try_get::<String, _>("owner_kind")?.as_str() {
                "task" => {
                    ClaimedExecutionReviewRequest::Task(ExecutionActionReviewResolutionRequest {
                        run_uid,
                        task_id: ExecutionTaskId::from_uuid(operation_id),
                        generation,
                        review_uid,
                        resolution,
                    })
                }
                "compensation" => ClaimedExecutionReviewRequest::Compensation(
                    ExecutionCompensationReviewResolutionRequest {
                        run_uid,
                        compensation_id: CompensationId::from_uuid(operation_id),
                        generation,
                        review_uid,
                        resolution,
                    },
                ),
                owner_kind => {
                    return Err(sqlx::Error::Decode(
                        format!("unknown execution review owner_kind: {owner_kind}").into(),
                    ));
                }
            };
            Ok(ClaimedExecutionReviewResolution {
                review_uid,
                request,
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
    owner: &ActionReviewOwner,
    resolution: &ExecutionActionReviewResolution,
    resolution_trace_context: Option<&ValidatedTraceContext>,
    task_trace_context: Option<&ValidatedTraceContext>,
) -> Result<bool, sqlx::Error> {
    let resolution =
        serde_json::to_value(resolution).map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
    let (owner_kind, run_uid, operation_id, generation, statement) = match owner {
        ActionReviewOwner::ExecutionTask { origin, .. } => (
            "task",
            origin.run_uid,
            origin.task_uid,
            origin.generation,
            r#"
        INSERT INTO moa.execution_action_review_outbox (
            review_uid, tenant_id, contact_id, owner_kind, run_uid, operation_id,
            generation, resolution, traceparent, tracestate,
            task_traceparent, task_tracestate
        )
        SELECT $1, task.tenant_id, task.contact_id, $2, task.run_uid, task.task_id,
               $6, $7, $8, $9, $10, $11
        FROM moa.execution_task AS task
        WHERE task.run_uid = $3
          AND task.task_id = $4
          AND task.tenant_id = $5
        ON CONFLICT (review_uid) DO UPDATE
        SET next_attempt_at = NOW(),
            updated_at = NOW()
        WHERE moa.execution_action_review_outbox.owner_kind = EXCLUDED.owner_kind
          AND moa.execution_action_review_outbox.run_uid = EXCLUDED.run_uid
          AND moa.execution_action_review_outbox.operation_id = EXCLUDED.operation_id
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
        ),
        ActionReviewOwner::ExecutionCompensation { origin, .. } => (
            "compensation",
            origin.run_uid,
            origin.compensation_id,
            origin.generation,
            r#"
        INSERT INTO moa.execution_action_review_outbox (
            review_uid, tenant_id, contact_id, owner_kind, run_uid, operation_id,
            generation, resolution, traceparent, tracestate,
            task_traceparent, task_tracestate
        )
        SELECT $1, run.tenant_id, run.contact_id, $2, compensation.run_uid,
               compensation.compensation_id, $6, $7, $8, $9, $10, $11
        FROM moa.execution_compensation AS compensation
        JOIN moa.execution_run AS run ON run.run_uid = compensation.run_uid
        WHERE compensation.run_uid = $3
          AND compensation.compensation_id = $4
          AND run.tenant_id = $5
        ON CONFLICT (review_uid) DO UPDATE
        SET next_attempt_at = NOW(),
            updated_at = NOW()
        WHERE moa.execution_action_review_outbox.owner_kind = EXCLUDED.owner_kind
          AND moa.execution_action_review_outbox.run_uid = EXCLUDED.run_uid
          AND moa.execution_action_review_outbox.operation_id = EXCLUDED.operation_id
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
        ),
        ActionReviewOwner::Coordinator { .. } | ActionReviewOwner::Worker { .. } => {
            return Err(sqlx::Error::Protocol(
                "conversational review cannot enter the execution outbox".to_string(),
            ));
        }
    };
    let result = sqlx::query(statement)
        .bind(review_id)
        .bind(owner_kind)
        .bind(run_uid)
        .bind(operation_id)
        .bind(tenant_id.0)
        .bind(i64::try_from(generation).map_err(|error| sqlx::Error::Encode(Box::new(error)))?)
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
          AND owner_registered_at IS NOT NULL
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
          AND owner_registered_at IS NOT NULL
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
    include_request_identity: bool,
) -> Result<StoredReview, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, tenant_id, storage_partition_id, session_id, worker_id, tool_call_id, tool_name,
               action_class, risk_level, input_summary, envelope, preview, status,
               requested_by, decided_by, deny_reason, created_at, expires_at, decided_at,
               owner_registered_at, tool_request
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
    let request_identity = if include_request_identity {
        Some(serde_json::json!({
            "envelope": row.try_get::<serde_json::Value, _>("envelope").map_err(db_error)?,
            "preview": row.try_get::<serde_json::Value, _>("preview").map_err(db_error)?,
            "tool_request": row.try_get::<serde_json::Value, _>("tool_request").map_err(db_error)?,
        }))
    } else {
        None
    };

    Ok(StoredReview {
        summary: summary_from_row(&row)?,
        newly_inserted: false,
        owner_registered_at: row.try_get("owner_registered_at").map_err(db_error)?,
        request_identity,
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
