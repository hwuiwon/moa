//! Terminal run transitions, cancellation, and shared state-projection helpers.

use super::*;
use super::{projection::*, rows::*, sql::*, transition::*};

impl ExecutionRepository {
    /// Atomically finalizes one active revision with deterministic completion evidence.
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

    /// Atomically cancels one fenced waiting-replan task and finalizes its run.
    pub async fn finalize_replan_stop(
        &self,
        scope: ExecutionScope,
        request: ReplanStopRequest,
    ) -> Result<ReplanStopOutcome> {
        let ReplanStopRequest {
            run_uid,
            expected_revision,
            expected_wake_epoch,
            task_id,
            expected_generation,
            amendment_hash,
            cancellation_reason,
            terminal_projection,
            completion_evaluation,
            terminal_evidence,
            terminal_reason,
        } = request;
        let terminal_status = run_status_from_completion(completion_evaluation.status);
        if !matches!(
            terminal_status,
            ExecutionRunStatus::Partial | ExecutionRunStatus::Blocked
        ) || run_status_from_terminal_projection(&terminal_projection) != terminal_status
        {
            return Err(Error::InvalidRepositoryInput {
                message: "replan stop must finalize a matching partial or blocked run".to_string(),
            });
        }
        let selected_reason = execution_terminal_reason(
            &terminal_evidence.cause,
            &terminal_projection,
            &completion_evaluation,
        )?;
        if terminal_reason != selected_reason {
            return Err(Error::InvalidRepositoryInput {
                message: "selected replan-stop terminal reason disagrees with typed evidence"
                    .to_string(),
            });
        }
        let output = terminal_projection_output(&terminal_projection);
        let checks = serde_json::to_value(&completion_evaluation.checks)?;
        let gaps = serde_json::to_value(&completion_evaluation.gaps)?;
        let terminal_cause = serde_json::to_value(&terminal_evidence.cause)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.status.is_terminal() {
            let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
                .bind(run_uid)
                .fetch_all(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let tasks = task_rows
                .iter()
                .map(task_from_row)
                .collect::<Result<Vec<_>>>()?;
            let Some(task) = tasks.iter().find(|task| task.task_id == task_id).cloned() else {
                conn.commit().await.map_err(storage_error)?;
                return Ok(ReplanStopOutcome::NotFound);
            };
            let cancelled_outcome =
                cancelled_task_outcome(cancellation_reason.clone(), task.actual.clone());
            let amendment_hash_text = amendment_hash.as_ref().map(ToString::to_string);
            let task_ids_to_release = tasks
                .iter()
                .filter(|task| {
                    task.outcome_audit.iter().any(|entry| {
                        entry.get("kind").and_then(Value::as_str) == Some("replan_stopped")
                            && entry.get("accepted").and_then(Value::as_bool) == Some(true)
                            && entry.get("base_plan_revision").and_then(Value::as_u64)
                                == Some(expected_revision)
                            && entry.get("amendment_hash").and_then(Value::as_str)
                                == amendment_hash_text.as_deref()
                    })
                })
                .map(|task| task.task_id)
                .collect::<Vec<_>>();
            let replay = run.plan_revision == expected_revision
                && run.status == terminal_status
                && run.output == output
                && serde_json::to_value(&run.completion_check_results)? == checks
                && serde_json::to_value(&run.terminal_gaps)? == gaps
                && run.terminal_evidence.as_ref() == Some(&terminal_evidence)
                && run.terminal_reason == Some(terminal_reason)
                && task.plan_revision == expected_revision
                && task.generation == expected_generation
                && task.status == ExecutionTaskStatus::Cancelled
                && task.current_outcome.as_ref() == Some(&cancelled_outcome)
                && !task_ids_to_release.is_empty();
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                ReplanStopOutcome::Replayed(Box::new(ReplanStopFinalization {
                    run,
                    task,
                    task_ids_to_release,
                }))
            } else {
                ReplanStopOutcome::Conflict
            });
        }
        let task_rows = sqlx::query(LOAD_NONTERMINAL_TASKS_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let tasks = task_rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>>>()?;
        let Some(task) = tasks.iter().find(|task| task.task_id == task_id) else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::NotFound);
        };
        if run.plan_revision != expected_revision
            || run.wake_epoch != expected_wake_epoch
            || run.status != ExecutionRunStatus::WaitingReplan
            || task.plan_revision != expected_revision
            || task.generation != expected_generation
            || task.status != ExecutionTaskStatus::WaitingReplan
            || !matches!(
                task.current_outcome.as_ref().map(|outcome| &outcome.result),
                Some(ExecutionTaskResult::NeedsReplan { .. })
            )
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::Conflict);
        }
        let expected_terminal_evidence = terminal_evidence_from_evaluation(
            terminal_evidence.cause.clone(),
            &completion_evaluation,
        )?;
        if terminal_evidence != expected_terminal_evidence
            || !matches!(
                terminal_evidence.cause,
                ExecutionTerminalCause::ReplanStop { .. }
            )
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::Conflict);
        }
        let cancellation = terminalize_nonterminal_tasks(
            &mut conn,
            &run,
            &tasks,
            TaskCancellationEvidence {
                kind: "replan_stopped",
                reason: &cancellation_reason,
                base_plan_revision: Some(expected_revision),
                amendment_hash: amendment_hash.as_ref(),
                terminal_status: Some(terminal_status),
                terminal_projection: Some(&terminal_projection),
                completion_evaluation: Some(&completion_evaluation),
            },
        )
        .await?;
        let task_ids_to_release = cancellation
            .tasks
            .iter()
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        let run_row = sqlx::query(FINALIZE_REPLAN_STOP_RUN_SQL)
            .bind(run_uid)
            .bind(to_i64(expected_revision, "expected plan revision")?)
            .bind(terminal_status.as_str())
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
            .bind(to_i64(
                cancellation.run_reserved.cost_microusd,
                "run reserved cost",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tokens,
                "run reserved tokens",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tasks,
                "run reserved tasks",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tool_calls,
                "run reserved tool calls",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.retrieved_bytes,
                "run reserved retrieved bytes",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.cost_microusd,
                "run consumed cost",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tokens,
                "run consumed tokens",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tasks,
                "run consumed tasks",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tool_calls,
                "run consumed tool calls",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.retrieved_bytes,
                "run consumed retrieved bytes",
            )?)
            .bind(cancellation.budget_overrun)
            .bind(to_i64(
                tasks.len() as u64,
                "replan-stop cancelled task count",
            )?)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(run_row) = run_row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReplanStopOutcome::Conflict);
        };
        let finalized = ReplanStopFinalization {
            run: run_from_row(&run_row)?,
            task_ids_to_release,
            task: cancellation
                .tasks
                .into_iter()
                .find(|task| task.task_id == task_id)
                .ok_or_else(|| Error::Storage {
                    message: "replan-stop transaction lost its originating task".to_string(),
                })?,
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(ReplanStopOutcome::Finalized(Box::new(finalized)))
    }

    /// Atomically cancels a run, all nonterminal work, and all unused reservations.
    pub async fn cancel_run(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        request: CancellationRequest,
    ) -> Result<CancellationOutcome> {
        let CancellationRequest {
            reason,
            terminal_evidence,
        } = request;
        if terminal_evidence.cause != ExecutionTerminalCause::Cancellation {
            return Err(Error::InvalidRepositoryInput {
                message: "run cancellation requires the cancellation terminal cause".to_string(),
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
            return Ok(CancellationOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.status == ExecutionRunStatus::Cancelled {
            if run.cancellation_reason.as_deref() != Some(reason.as_str())
                || run.terminal_evidence.as_ref() != Some(&terminal_evidence)
                || run.terminal_reason != Some(ExecutionTerminalReason::Cancelled)
            {
                conn.commit().await.map_err(storage_error)?;
                return Ok(CancellationOutcome::Conflict);
            }
            let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
                .bind(run_uid)
                .fetch_all(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let mut task_ids_to_release = task_rows
                .iter()
                .map(task_from_row)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter(|task| task_has_accepted_audit_kind(task, "run_cancelled"))
                .map(|task| task.task_id)
                .collect::<Vec<_>>();
            task_ids_to_release.sort();
            conn.commit().await.map_err(storage_error)?;
            return Ok(CancellationOutcome::Replayed(Box::new(
                CancellationCommit {
                    run,
                    task_ids_to_release,
                },
            )));
        }
        if run.status.is_terminal() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CancellationOutcome::Conflict);
        }

        sqlx::query(
            "SELECT task_id FROM moa.execution_task WHERE run_uid = $1 ORDER BY task_id FOR UPDATE",
        )
        .bind(run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let task_rows = sqlx::query(LIST_ALL_TASKS_SQL)
            .bind(run_uid)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let all_tasks = task_rows
            .iter()
            .map(task_from_row)
            .collect::<Result<Vec<_>>>()?;
        let expected_terminal_evidence = cancellation_terminal_evidence(
            &run.goal,
            &run.active_plan,
            &scheduling_projection(&run, &all_tasks),
        )?;
        if terminal_evidence != expected_terminal_evidence {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CancellationOutcome::Conflict);
        }
        let tasks = all_tasks
            .into_iter()
            .filter(|task| !task.status.is_terminal())
            .collect::<Vec<_>>();
        let cancellation = terminalize_nonterminal_tasks(
            &mut conn,
            &run,
            &tasks,
            TaskCancellationEvidence {
                kind: "run_cancelled",
                reason: &reason,
                base_plan_revision: None,
                amendment_hash: None,
                terminal_status: Some(ExecutionRunStatus::Cancelled),
                terminal_projection: None,
                completion_evaluation: None,
            },
        )
        .await?;
        let mut task_ids_to_release = cancellation
            .tasks
            .iter()
            .map(|task| task.task_id)
            .collect::<Vec<_>>();
        task_ids_to_release.sort();
        let prior_run_status = run.status;
        let row = sqlx::query(CANCEL_RUN_SQL)
            .bind(run_uid)
            .bind(&reason)
            .bind(serde_json::to_value(&terminal_evidence.cause)?)
            .bind(to_i64(
                terminal_evidence.satisfied_requirement_count,
                "terminal satisfied requirement count",
            )?)
            .bind(to_i64(
                terminal_evidence.requirement_count,
                "terminal requirement count",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.cost_microusd,
                "run reserved cost",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tokens,
                "run reserved tokens",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tasks,
                "run reserved tasks",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.tool_calls,
                "run reserved tool calls",
            )?)
            .bind(to_i64(
                cancellation.run_reserved.retrieved_bytes,
                "run reserved retrieved bytes",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.cost_microusd,
                "run consumed cost",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tokens,
                "run consumed tokens",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tasks,
                "run consumed tasks",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.tool_calls,
                "run consumed tool calls",
            )?)
            .bind(to_i64(
                cancellation.run_consumed.retrieved_bytes,
                "run consumed retrieved bytes",
            )?)
            .bind(cancellation.budget_overrun)
            .bind(to_i64(tasks.len() as u64, "cancelled task count")?)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        let metrics = ExecutionMutationMetricEvidence {
            run: run_transition_evidence(prior_run_status, &run),
            tasks: tasks
                .iter()
                .zip(&cancellation.tasks)
                .map(|(prior, task)| task_transition_evidence(prior.status, task))
                .collect(),
        };
        Ok(CancellationOutcome::Cancelled {
            commit: Box::new(CancellationCommit {
                run,
                task_ids_to_release,
            }),
            metrics: Box::new(metrics),
        })
    }
}
