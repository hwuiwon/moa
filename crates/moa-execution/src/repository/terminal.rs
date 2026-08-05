//! Successful run finalization and shared state-projection helpers.

use super::*;
use super::{projection::*, rows::*, sql::*};

impl ExecutionRepository {
    /// Atomically finalizes one successfully completed revision with deterministic evidence.
    pub async fn finalize_run(
        &self,
        scope: ExecutionScope,
        request: RunFinalizationRequest,
    ) -> Result<FinalizationOutcome> {
        let RunFinalizationRequest {
            run_uid,
            expected_revision,
            expected_wake_epoch,
            terminal_projection,
            completion_evaluation,
            terminal_evidence,
            terminal_reason,
        } = request;
        let expected_status = run_status_from_completion(completion_evaluation.status);
        if expected_status != ExecutionRunStatus::Completed {
            return Err(Error::InvalidRepositoryInput {
                message: "ordinary finalization only accepts successful completion; non-success terminal intents must use the compensation fence"
                    .to_string(),
            });
        }
        if run_status_from_terminal_projection(&terminal_projection) != expected_status {
            return Err(Error::InvalidRepositoryInput {
                message: "terminal projection and completion evaluation disagree".to_string(),
            });
        }
        let selected_reason = execution_terminal_reason(
            &terminal_evidence.cause,
            &terminal_projection,
            &completion_evaluation,
        )?;
        if terminal_reason != selected_reason {
            return Err(Error::InvalidRepositoryInput {
                message: "selected terminal reason disagrees with typed terminal evidence"
                    .to_string(),
            });
        }
        let output = terminal_projection_output(&terminal_projection);
        let checks = serde_json::to_value(&completion_evaluation.checks)?;
        let gaps = serde_json::to_value(&completion_evaluation.gaps)?;
        let terminal_cause = serde_json::to_value(&terminal_evidence.cause)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::NotFound);
        };
        let current = run_from_row(&row)?;
        if current.status.is_terminal() {
            let replay = current.plan_revision == expected_revision
                && current.status == expected_status
                && current.output == output
                && serde_json::to_value(&current.completion_check_results)? == checks
                && serde_json::to_value(&current.terminal_gaps)? == gaps
                && current.terminal_evidence.as_ref() == Some(&terminal_evidence)
                && current.terminal_reason == Some(terminal_reason);
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                FinalizationOutcome::Replayed(current)
            } else {
                FinalizationOutcome::Conflict
            });
        }
        if current.plan_revision != expected_revision || current.wake_epoch != expected_wake_epoch {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        let nonterminal_tasks = sqlx::query(LOAD_NONTERMINAL_TASKS_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        if current.pending_terminal.is_some()
            || current.manual_repair_required
            || !nonterminal_tasks.is_empty()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        let expected_terminal_evidence = terminal_evidence_from_evaluation(
            terminal_evidence.cause.clone(),
            &completion_evaluation,
        )?;
        if terminal_evidence != expected_terminal_evidence {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FinalizationOutcome::Conflict);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_run \
             SET status = $3, output = $4, completion_check_results = $5, \
                 terminal_gaps = $6, terminal_cause = $7, \
                 terminal_satisfied_requirement_count = $8, \
                 terminal_requirement_count = $9, terminal_reason = $10, \
                 waiting_reasons = '[]'::JSONB, \
                 wake_epoch = wake_epoch + 1, completed_at = NOW(), updated_at = NOW() \
             WHERE run_uid = $1 AND plan_revision = $2 \
             RETURNING *",
        )
        .bind(run_uid)
        .bind(to_i64(expected_revision, "expected plan revision")?)
        .bind(expected_status.as_str())
        .bind(output)
        .bind(checks)
        .bind(gaps)
        .bind(terminal_cause)
        .bind(to_i64(
            terminal_evidence.satisfied_requirement_count,
            "terminal satisfied requirement count",
        )?)
        .bind(to_i64(
            terminal_evidence.requirement_count,
            "terminal requirement count",
        )?)
        .bind(terminal_reason.as_str())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(FinalizationOutcome::Finalized(run))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn terminal_repository_exposes_no_direct_non_success_finalizers() {
        // Pins: every non-success terminal intent must enter through the
        // compensation fence; restoring either legacy transaction bypasses
        // forward-task join and rollback settlement.
        let source = include_str!("terminal.rs");
        let direct_cancel = ["pub async fn ", "cancel_run("].concat();
        let direct_replan = ["pub async fn ", "finalize_replan_stop("].concat();

        assert!(
            !source.contains(&direct_cancel),
            "legacy direct cancellation remains publicly dispatchable"
        );
        assert!(
            !source.contains(&direct_replan),
            "legacy direct replan-stop finalization remains publicly dispatchable"
        );
    }
}
