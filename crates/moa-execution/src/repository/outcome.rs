//! Task outcome recording and compiler-validated amendment persistence.

use super::*;
use super::{
    materialize::reconcile_outcome_usage, outcome_support::*, rows::*, sql::*,
    transition::task_outcome_is_exact_replay,
};

impl ExecutionRepository {
    /// Records one cumulative task outcome under the current generation fence.
    ///
    /// Stale, terminal-task, terminal-run, and invalid cumulative messages are
    /// retained in append-only audit history without changing current state.
    pub async fn record_task_outcome(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        outcome: ExecutionTaskOutcome,
    ) -> Result<TaskOutcomeWrite> {
        let validation = moa_artifacts::validation::validate_execution_task_outcome(&outcome);
        if let Some(error) = validation.errors.first() {
            return Err(Error::InvalidRepositoryInput {
                message: format!("{}: {}", error.path, error.message),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::NotFound);
        };
        let task = task_from_row(&task_row)?;

        if task_outcome_is_exact_replay(&task, generation, &outcome) {
            let budget_overrun = run.budget_overrun;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::Replayed {
                run,
                task,
                budget_overrun,
            });
        }

        let rejection = if outcome.schema_version != 1 {
            Some(TaskOutcomeRejection::UnsupportedSchemaVersion)
        } else if run.status.is_terminal() {
            Some(TaskOutcomeRejection::TerminalRun)
        } else if task.status.is_terminal() {
            Some(TaskOutcomeRejection::TerminalTask)
        } else if task.generation != generation {
            Some(TaskOutcomeRejection::StaleGeneration)
        } else if task.status != ExecutionTaskStatus::Running {
            Some(TaskOutcomeRejection::InvalidTaskStatus)
        } else if !usage_is_cumulative(&task.actual, &outcome.usage) {
            Some(TaskOutcomeRejection::NonCumulativeUsage)
        } else {
            None
        };
        if let Some(reason) = rejection {
            let task =
                append_outcome_audit(&mut conn, &task, generation, &outcome, false, Some(reason))
                    .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::Rejected { task, reason });
        }

        let terminal = task_outcome_is_terminal(&outcome);
        let Some(reconciliation) = reconcile_outcome_usage(&run, &task, &outcome, terminal) else {
            let reason = TaskOutcomeRejection::NonCumulativeUsage;
            let task =
                append_outcome_audit(&mut conn, &task, generation, &outcome, false, Some(reason))
                    .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::Rejected { task, reason });
        };
        let task_status = task_status_from_outcome(&outcome, true);
        let run_status = run_status_after_task_outcome(run.status, &outcome);
        let (output, error, citations) = outcome_projection_fields(&outcome)?;
        let audit = outcome_audit_entry(&task, generation, &outcome, true, None);
        let current_outcome = serde_json::to_value(&outcome)?;
        let citations = serde_json::to_value(citations)?;
        let completed_increment = u64::from(task_status == ExecutionTaskStatus::Completed);
        let failed_increment = u64::from(task_status == ExecutionTaskStatus::Failed);
        let cancelled_increment = u64::from(task_status == ExecutionTaskStatus::Cancelled);

        let run_row = sqlx::query(RECONCILE_RUN_OUTCOME_SQL)
            .bind(run_uid)
            .bind(run_status.as_str())
            .bind(to_i64(
                reconciliation.run_reserved.cost_microusd,
                "run reserved cost",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tokens,
                "run reserved tokens",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tasks,
                "run reserved tasks",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tool_calls,
                "run reserved tool calls",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.retrieved_bytes,
                "run reserved retrieved bytes",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.cost_microusd,
                "run consumed cost",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.tokens,
                "run consumed tokens",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.tasks,
                "run consumed tasks",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.tool_calls,
                "run consumed tool calls",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.retrieved_bytes,
                "run consumed retrieved bytes",
            )?)
            .bind(reconciliation.budget_overrun)
            .bind(to_i64(completed_increment, "completed task increment")?)
            .bind(to_i64(failed_increment, "failed task increment")?)
            .bind(to_i64(cancelled_increment, "cancelled task increment")?)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&run_row)?;

        let row = sqlx::query(RECORD_TASK_OUTCOME_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(to_i64(generation, "task generation")?)
            .bind(task_status.as_str())
            .bind(to_i64(
                reconciliation.remaining_task_reservation.cost_microusd,
                "remaining task cost reservation",
            )?)
            .bind(to_i64(
                reconciliation.remaining_task_reservation.tokens,
                "remaining task token reservation",
            )?)
            .bind(to_i64(
                reconciliation.remaining_task_reservation.tasks,
                "remaining task logical reservation",
            )?)
            .bind(to_i64(
                reconciliation.remaining_task_reservation.tool_calls,
                "remaining task tool-call reservation",
            )?)
            .bind(to_i64(
                reconciliation.remaining_task_reservation.retrieved_bytes,
                "remaining task byte reservation",
            )?)
            .bind(to_i64(outcome.usage.cost_microusd, "actual task cost")?)
            .bind(to_i64(outcome.usage.tokens, "actual task tokens")?)
            .bind(to_i64(
                u64::from(reconciliation.terminal),
                "actual logical task",
            )?)
            .bind(to_i64(outcome.usage.tool_calls, "actual task tool calls")?)
            .bind(to_i64(
                outcome.usage.retrieved_bytes,
                "actual task retrieved bytes",
            )?)
            .bind(current_outcome)
            .bind(output)
            .bind(error)
            .bind(citations)
            .bind(audit)
            .bind(reconciliation.terminal)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskOutcomeWrite::Applied {
            run,
            task,
            budget_overrun: reconciliation.budget_overrun,
        })
    }

    /// Recovers an exact committed amendment handoff before current-revision validation.
    pub async fn recover_amendment_handoff(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        amendment_hash: &ExecutionHash,
    ) -> Result<AmendmentReplayOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentReplayOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let amendment_hash_text = amendment_hash.to_string();
        let exact_history = run.plan_history.iter().rev().find(|entry| {
            entry.get("base_plan_revision").and_then(Value::as_u64) == Some(expected_revision)
                && entry.get("amendment_hash").and_then(Value::as_str)
                    == Some(amendment_hash_text.as_str())
        });
        let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
            .bind(run_uid)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let tasks = task_rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>>>()?;
        let audited_task_ids = tasks
            .iter()
            .filter(|task| {
                task.outcome_audit.iter().any(|entry| {
                    entry.get("accepted").and_then(Value::as_bool) == Some(true)
                        && entry.get("amendment_hash").and_then(Value::as_str)
                            == Some(amendment_hash_text.as_str())
                        && entry.get("base_plan_revision").and_then(Value::as_u64)
                            == Some(expected_revision)
                })
            })
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        let task_ids_to_release = match exact_history {
            Some(history) => history
                .get("task_ids_to_release")
                .cloned()
                .and_then(|value| serde_json::from_value::<Vec<ExecutionTaskId>>(value).ok())
                .filter(|task_ids| !task_ids.is_empty())
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "committed amendment history is missing its handoff task IDs"
                        .to_string(),
                })?,
            None if run.plan_revision == expected_revision && !audited_task_ids.is_empty() => {
                audited_task_ids
            }
            None => {
                let outcome = if run.plan_revision == expected_revision && !run.status.is_terminal()
                {
                    AmendmentReplayOutcome::NotApplied
                } else {
                    AmendmentReplayOutcome::Conflict
                };
                conn.commit().await.map_err(storage_error)?;
                return Ok(outcome);
            }
        };
        let audit_matches = task_ids_to_release.iter().all(|task_id| {
            tasks.iter().any(|task| {
                task.task_id == *task_id
                    && task.outcome_audit.iter().any(|entry| {
                        entry.get("accepted").and_then(Value::as_bool) == Some(true)
                            && entry.get("amendment_hash").and_then(Value::as_str)
                                == Some(amendment_hash_text.as_str())
                            && entry.get("base_plan_revision").and_then(Value::as_u64)
                                == Some(expected_revision)
                    })
            })
        });
        let outcome = if audit_matches {
            AmendmentReplayOutcome::Replayed(Box::new(AmendmentCommit {
                run,
                task_ids_to_release,
            }))
        } else {
            AmendmentReplayOutcome::Conflict
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Appends one compiler-validated amendment under the expected revision fence.
    pub async fn append_amendment(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        validated: ValidatedAmendment,
    ) -> Result<AmendmentWrite> {
        if amendment_hash(&validated.amendment)? != validated.amendment_hash {
            return Err(Error::InvalidRepositoryInput {
                message: "validated amendment hash is inconsistent".to_string(),
            });
        }
        if validated.amendment.base_plan_revision != expected_revision {
            return Ok(AmendmentWrite::Conflict);
        }
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentWrite::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.plan_revision != expected_revision
            || run.status != ExecutionRunStatus::WaitingReplan
            || run.active_plan_hash == validated.active_plan.plan_hash
        {
            let amendment_hash_text = validated.amendment_hash.to_string();
            let exact_history = run.plan_history.iter().rev().any(|entry| {
                entry.get("base_plan_revision").and_then(Value::as_u64) == Some(expected_revision)
                    && entry.get("amendment_hash").and_then(Value::as_str)
                        == Some(amendment_hash_text.as_str())
                    && entry.get("task_ids_to_release")
                        == Some(&json!([validated.superseded_task_id]))
            });
            let exact_task = if exact_history {
                sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
                    .bind(run_uid)
                    .bind(validated.superseded_task_id.as_uuid())
                    .fetch_optional(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?
                    .map(|row| task_from_row(&row))
                    .transpose()?
                    .is_some_and(|task| {
                        task.outcome_audit.iter().any(|entry| {
                            entry.get("accepted").and_then(Value::as_bool) == Some(true)
                                && entry.get("base_plan_revision").and_then(Value::as_u64)
                                    == Some(expected_revision)
                                && entry.get("amendment_hash").and_then(Value::as_str)
                                    == Some(amendment_hash_text.as_str())
                        })
                    })
            } else {
                false
            };
            conn.commit().await.map_err(storage_error)?;
            return Ok(if exact_task {
                AmendmentWrite::Replayed(Box::new(AmendmentCommit {
                    run,
                    task_ids_to_release: vec![validated.superseded_task_id],
                }))
            } else {
                AmendmentWrite::Conflict
            });
        }
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(validated.superseded_task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentWrite::Conflict);
        };
        let task = task_from_row(&task_row)?;
        if task.status != ExecutionTaskStatus::WaitingReplan {
            conn.commit().await.map_err(storage_error)?;
            return Ok(AmendmentWrite::Conflict);
        }
        let Some(previous_outcome) = task.current_outcome.as_ref() else {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryData {
                message: "waiting-replan task has no persisted outcome".to_string(),
            });
        };
        if !matches!(
            &previous_outcome.result,
            ExecutionTaskResult::NeedsReplan { .. }
        ) {
            conn.rollback().await.map_err(storage_error)?;
            return Err(Error::InvalidRepositoryData {
                message: "waiting-replan task does not have a needs-replan outcome".to_string(),
            });
        }
        let triggering_failure =
            task_failure_fingerprint_input(&task).ok_or_else(|| Error::InvalidRepositoryData {
                message: "waiting-replan task has no fingerprintable triggering failure"
                    .to_string(),
            })?;
        let triggering_failure_fingerprint = failure_fingerprint(&triggering_failure)?;
        let triggering_failure_fingerprint_text = triggering_failure_fingerprint.to_string();
        let triggering_failure_count = run
            .plan_history
            .iter()
            .filter(|entry| {
                entry.get("failure_fingerprint").and_then(Value::as_str)
                    == Some(triggering_failure_fingerprint_text.as_str())
            })
            .filter_map(|entry| {
                entry
                    .get("failure_fingerprint_count")
                    .and_then(Value::as_u64)
            })
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let superseded_outcome = ExecutionTaskOutcome {
            schema_version: 1,
            usage: previous_outcome.usage.clone(),
            result: ExecutionTaskResult::Cancelled {
                reason: "superseded_by_plan_revision".to_string(),
            },
        };
        let reconciliation = reconcile_outcome_usage(&run, &task, &superseded_outcome, true)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "superseded task reservation could not be reconciled".to_string(),
            })?;
        let task_audit = json!({
            "kind": "superseded_by_plan_revision",
            "attempt": task.attempt,
            "generation": task.generation,
            "base_plan_revision": expected_revision,
            "amendment_hash": validated.amendment_hash,
            "accepted": true,
            "recorded_at": Utc::now(),
        });
        let superseded_outcome = serde_json::to_value(superseded_outcome)?;
        let next_revision =
            expected_revision
                .checked_add(1)
                .ok_or_else(|| Error::InvalidRepositoryInput {
                    message: "execution plan revision overflow".to_string(),
                })?;
        let history = json!({
            "base_plan_revision": expected_revision,
            "plan_revision": next_revision,
            "amendment": validated.amendment,
            "amendment_hash": validated.amendment_hash,
            "outcome": "applied",
            "task_ids_to_release": [task.task_id],
            "active_plan_hash": validated.active_plan.plan_hash,
            "reason": validated.amendment.reason,
            "requirement_mapping": validated.requirement_mapping,
            "failure_fingerprint": triggering_failure_fingerprint,
            "failure_fingerprint_count": triggering_failure_count,
            "recorded_at": Utc::now(),
        });
        let active_plan = serde_json::to_value(&validated.active_plan)?;
        let row = sqlx::query(APPEND_AMENDMENT_SQL)
            .bind(run_uid)
            .bind(to_i64(expected_revision, "expected plan revision")?)
            .bind(to_i64(next_revision, "next plan revision")?)
            .bind(active_plan)
            .bind(validated.active_plan.plan_hash.to_string())
            .bind(history)
            .bind(to_i64(
                reconciliation.run_reserved.cost_microusd,
                "run reserved cost",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tokens,
                "run reserved tokens",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tasks,
                "run reserved tasks",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.tool_calls,
                "run reserved tool calls",
            )?)
            .bind(to_i64(
                reconciliation.run_reserved.retrieved_bytes,
                "run reserved retrieved bytes",
            )?)
            .bind(to_i64(
                reconciliation.run_consumed.tasks,
                "run consumed tasks",
            )?)
            .bind(reconciliation.budget_overrun)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(AmendmentWrite::Conflict);
        };
        let run = run_from_row(&row)?;
        sqlx::query(SUPERSEDE_REPLAN_TASK_SQL)
            .bind(run_uid)
            .bind(task.task_id.as_uuid())
            .bind(task_audit)
            .bind(superseded_outcome)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(AmendmentWrite::Applied(Box::new(AmendmentCommit {
            run,
            task_ids_to_release: vec![task.task_id],
        })))
    }
}
