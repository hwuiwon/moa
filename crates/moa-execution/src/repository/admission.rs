//! Row-locked external-effect admission for forward tasks and compensations.

use moa_core::types::action_policy::{
    ActionReviewOwner, ExecutionCompensationOrigin, ExecutionTaskOrigin,
};

use super::*;
use super::{
    compensation::CompensationAttemptState,
    rows::*,
    sql::*,
    task::{checkpoint_review_uid, task_checkpoint_from_row},
};

impl ExecutionRepository {
    /// Linearizes one external-effect admission against the run's terminal fence.
    ///
    /// Callers journal this decision before starting the effect. A terminal fence takes the same
    /// run-row lock, so the admitted call is durably ordered either before or after that fence.
    pub async fn admit_execution_effect(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        session_id: SessionId,
        owner: ExecutionEffectOwner,
    ) -> Result<ExecutionEffectAdmissionOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(rejected(ExecutionToolDispatchRejection::OriginNotFound));
        };
        let run = run_from_row(&run_row)?;
        if run.session_id != session_id {
            conn.commit().await.map_err(storage_error)?;
            return Ok(rejected(ExecutionToolDispatchRejection::OriginNotFound));
        }

        let rejection = match owner {
            ExecutionEffectOwner::Task {
                task_id,
                generation,
                attempt_generation,
                phase,
            } => {
                let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
                    .bind(run_uid)
                    .bind(task_id.as_uuid())
                    .fetch_optional(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?
                else {
                    conn.commit().await.map_err(storage_error)?;
                    return Ok(rejected(ExecutionToolDispatchRejection::OriginNotFound));
                };
                let task = task_from_row(&task_row)?;
                if run.status.is_terminal()
                    || run.status == ExecutionRunStatus::Compensating
                    || run.pending_terminal.is_some()
                    || run.manual_repair_required
                {
                    Some(ExecutionToolDispatchRejection::RunNotDispatchable)
                } else if task.generation != generation
                    || task.attempt_generation != attempt_generation
                {
                    Some(ExecutionToolDispatchRejection::StaleGeneration)
                } else if !task_effect_phase_is_current(&mut conn, &run, &task, phase, session_id)
                    .await?
                {
                    Some(ExecutionToolDispatchRejection::OperationNotRunning)
                } else {
                    None
                }
            }
            ExecutionEffectOwner::Compensation {
                compensation_id,
                generation,
                attempt_generation,
                phase,
            } => {
                let Some(compensation_row) = sqlx::query(
                    "SELECT * FROM moa.execution_compensation \
                     WHERE run_uid = $1 AND compensation_id = $2 FOR UPDATE",
                )
                .bind(run_uid)
                .bind(compensation_id.as_uuid())
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?
                else {
                    conn.commit().await.map_err(storage_error)?;
                    return Ok(rejected(ExecutionToolDispatchRejection::OriginNotFound));
                };
                let compensation = compensation_from_row(&compensation_row)?;
                let current_attempt_generation =
                    required_u64(&compensation_row, "attempt_generation")?;
                if run.status != ExecutionRunStatus::Compensating
                    || run.pending_terminal.is_none()
                    || run.manual_repair_required
                {
                    Some(ExecutionToolDispatchRejection::RunNotDispatchable)
                } else if compensation.generation != generation
                    || current_attempt_generation != attempt_generation
                {
                    Some(ExecutionToolDispatchRejection::StaleGeneration)
                } else if !compensation_effect_phase_is_current(
                    &mut conn,
                    &run,
                    &compensation,
                    &compensation_row,
                    attempt_generation,
                    phase,
                    session_id,
                )
                .await?
                {
                    Some(ExecutionToolDispatchRejection::OperationNotRunning)
                } else {
                    None
                }
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(rejection.map_or(ExecutionEffectAdmissionOutcome::Admitted, rejected))
    }
}

const fn rejected(reason: ExecutionToolDispatchRejection) -> ExecutionEffectAdmissionOutcome {
    ExecutionEffectAdmissionOutcome::Rejected(reason)
}

async fn task_effect_phase_is_current(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    phase: ExecutionEffectPhase,
    session_id: SessionId,
) -> Result<bool> {
    match phase {
        ExecutionEffectPhase::Direct => Ok(task.status == ExecutionTaskStatus::Running
            && task.attempt_state == ExecutionAttemptState::Running),
        ExecutionEffectPhase::Reviewed { review_uid } => {
            if review_uid.is_nil()
                || task.status != ExecutionTaskStatus::WaitingReview
                || task.attempt_state != ExecutionAttemptState::Waiting
            {
                return Ok(false);
            }
            let checkpoint = sqlx::query(
                "SELECT * FROM moa.execution_task_checkpoint \
                 WHERE tenant_id=$1 AND run_uid=$2 AND task_id=$3 \
                   AND superseded_at IS NULL FOR UPDATE",
            )
            .bind(run.tenant_id.0)
            .bind(run.run_uid)
            .bind(task.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let Some(checkpoint) = checkpoint else {
                return Err(Error::InvalidRepositoryData {
                    message: "waiting-review task has no current attempt checkpoint".to_string(),
                });
            };
            let checkpoint = task_checkpoint_from_row(&checkpoint)?;
            if checkpoint.controller_generation != run.controller_generation
                || checkpoint.task_generation != task.generation
                || checkpoint.attempt_generation != task.attempt_generation
                || checkpoint_review_uid(&checkpoint.payload) != Some(review_uid)
            {
                return Ok(false);
            }
            let expected_owner = ActionReviewOwner::ExecutionTask {
                session_id,
                origin: ExecutionTaskOrigin {
                    run_uid: run.run_uid,
                    task_uid: task.task_id.as_uuid(),
                    generation: task.generation,
                    attempt_generation: task.attempt_generation,
                },
            };
            current_claimed_review_matches(
                conn,
                run.tenant_id,
                session_id,
                review_uid,
                &expected_owner,
            )
            .await
        }
    }
}

async fn compensation_effect_phase_is_current(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    compensation: &CompensationRegistrationProjection,
    row: &PgRow,
    attempt_generation: u64,
    phase: ExecutionEffectPhase,
    session_id: SessionId,
) -> Result<bool> {
    let attempt_state = CompensationAttemptState::from_str(
        &row.try_get::<String, _>("attempt_state")
            .map_err(row_error)?,
    )?;
    match phase {
        ExecutionEffectPhase::Direct => Ok(compensation.status == CompensationStatus::Running
            && attempt_state == CompensationAttemptState::Running),
        ExecutionEffectPhase::Reviewed { review_uid } => {
            if review_uid.is_nil()
                || compensation.status != CompensationStatus::Running
                || attempt_state != CompensationAttemptState::WaitingReview
            {
                return Ok(false);
            }
            let persisted = row
                .try_get::<Option<Value>, _>("outcome")
                .map_err(row_error)?
                .map(serde_json::from_value::<CompensationPersistedOutcome>)
                .transpose()?
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "waiting-review compensation has no persisted review audit"
                        .to_string(),
                })?;
            if !persisted.review_audit.iter().any(|entry| {
                entry.review_uid == review_uid
                    && entry.generation == compensation.generation
                    && !entry.accepted
            }) {
                return Ok(false);
            }
            let expected_owner = ActionReviewOwner::ExecutionCompensation {
                session_id,
                origin: ExecutionCompensationOrigin {
                    run_uid: run.run_uid,
                    compensation_id: compensation.compensation_id.as_uuid(),
                    generation: compensation.generation,
                    attempt_generation,
                },
            };
            current_claimed_review_matches(
                conn,
                run.tenant_id,
                session_id,
                review_uid,
                &expected_owner,
            )
            .await
        }
    }
}

async fn current_claimed_review_matches(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    session_id: SessionId,
    review_uid: Uuid,
    expected_owner: &ActionReviewOwner,
) -> Result<bool> {
    let expected_owner = serde_json::to_value(expected_owner)?;
    let row = sqlx::query_scalar::<_, bool>(
        "SELECT TRUE FROM public.tenant_action_reviews \
         WHERE tenant_id=$1 AND session_id=$2 AND id=$3 \
           AND status='pending' AND owner_registered_at IS NOT NULL \
           AND execution_requested_at IS NOT NULL AND execution_tool_call_id IS NOT NULL \
           AND owner_release_delivered_at IS NULL \
           AND envelope ->> 'review_id' = $3::TEXT \
           AND envelope -> 'owner' = $4 \
         FOR UPDATE",
    )
    .bind(tenant_id.0)
    .bind(session_id.0)
    .bind(review_uid)
    .bind(expected_owner)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(row.is_some())
}
