//! Row-locked external-effect admission for forward tasks and compensations.

use super::*;
use super::{rows::*, sql::*};

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
            return Ok(ExecutionEffectAdmissionOutcome::Rejected(
                ExecutionToolDispatchRejection::OriginNotFound,
            ));
        };
        let run = run_from_row(&run_row)?;
        if run.session_id != session_id {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionEffectAdmissionOutcome::Rejected(
                ExecutionToolDispatchRejection::OriginNotFound,
            ));
        }
        let rejection = match owner {
            ExecutionEffectOwner::Task {
                task_id,
                generation,
            } => {
                let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
                    .bind(run_uid)
                    .bind(task_id.as_uuid())
                    .fetch_optional(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?
                else {
                    conn.commit().await.map_err(storage_error)?;
                    return Ok(ExecutionEffectAdmissionOutcome::Rejected(
                        ExecutionToolDispatchRejection::OriginNotFound,
                    ));
                };
                let task = task_from_row(&task_row)?;
                if run.status.is_terminal()
                    || run.status == ExecutionRunStatus::Compensating
                    || run.pending_terminal.is_some()
                    || run.manual_repair_required
                {
                    Some(ExecutionToolDispatchRejection::RunNotDispatchable)
                } else if task.generation != generation {
                    Some(ExecutionToolDispatchRejection::StaleGeneration)
                } else if task.status != ExecutionTaskStatus::Running {
                    Some(ExecutionToolDispatchRejection::OperationNotRunning)
                } else {
                    None
                }
            }
            ExecutionEffectOwner::Compensation {
                compensation_id,
                generation,
            } => {
                let Some(compensation_row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
                    .bind(run_uid)
                    .bind(compensation_id.as_uuid())
                    .fetch_optional(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?
                else {
                    conn.commit().await.map_err(storage_error)?;
                    return Ok(ExecutionEffectAdmissionOutcome::Rejected(
                        ExecutionToolDispatchRejection::OriginNotFound,
                    ));
                };
                let compensation = compensation_from_row(&compensation_row)?;
                if run.status != ExecutionRunStatus::Compensating
                    || run.pending_terminal.is_none()
                    || run.manual_repair_required
                {
                    Some(ExecutionToolDispatchRejection::RunNotDispatchable)
                } else if compensation.generation != generation {
                    Some(ExecutionToolDispatchRejection::StaleGeneration)
                } else if compensation.status != CompensationStatus::Running {
                    Some(ExecutionToolDispatchRejection::OperationNotRunning)
                } else {
                    None
                }
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(
            rejection.map_or(ExecutionEffectAdmissionOutcome::Admitted, |reason| {
                ExecutionEffectAdmissionOutcome::Rejected(reason)
            }),
        )
    }
}
