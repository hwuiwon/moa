//! Logical-task reservation, redispatch, waiting, and review-resolution persistence.

use super::*;
use super::{materialize::DbEstimate, outcome_support::*, rows::*, sql::*};

impl ExecutionRepository {
    /// Atomically reserves all five resource dimensions for one pending task.
    pub async fn reserve_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
    ) -> Result<ReservationOutcome> {
        let generation_db = to_i64(generation, "task generation")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::NotFound);
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
            return Ok(ReservationOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if task.generation != generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Rejected(
                ReservationRejection::GenerationMismatch,
            ));
        }
        if let Some(rejection) = terminal_reservation_rejection(&task) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::AlreadyTerminalized(Box::new(
                ReservationTerminalization {
                    run,
                    task,
                    rejection,
                },
            )));
        }
        if task.status == ExecutionTaskStatus::Reserved {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::AlreadyReserved(task));
        }
        if task.status != ExecutionTaskStatus::Pending {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Rejected(
                ReservationRejection::InvalidTaskStatus,
            ));
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        ) || run.pending_terminal.is_some()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Rejected(
                ReservationRejection::InvalidRunStatus,
            ));
        }
        if run
            .approved_budget
            .deadline_at
            .is_some_and(|deadline| Utc::now() > deadline)
        {
            let terminalized = terminalize_reservation_rejection(
                &mut conn,
                &run,
                &task,
                ReservationRejection::DeadlineElapsed,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Terminalized(Box::new(terminalized)));
        }
        let estimate = DbEstimate::try_from(task.estimate)?;
        let run_updated = sqlx::query(RESERVE_RUN_BUDGET_SQL)
            .bind(run_uid)
            .bind(estimate.cost_microusd)
            .bind(estimate.tokens)
            .bind(estimate.tasks)
            .bind(estimate.tool_calls)
            .bind(estimate.retrieved_bytes)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        if run_updated.rows_affected() != 1 {
            let terminalized = terminalize_reservation_rejection(
                &mut conn,
                &run,
                &task,
                ReservationRejection::BudgetExceeded,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ReservationOutcome::Terminalized(Box::new(terminalized)));
        }

        let row = sqlx::query(RESERVE_TASK_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(generation_db)
            .bind(estimate.cost_microusd)
            .bind(estimate.tokens)
            .bind(estimate.tasks)
            .bind(estimate.tool_calls)
            .bind(estimate.retrieved_bytes)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ReservationOutcome::Reserved(task))
    }

    /// Marks one reserved task running under its current generation fence.
    pub async fn mark_task_running(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
    ) -> Result<TransitionOutcome> {
        let generation_db = to_i64(generation, "task generation")?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
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
            return Ok(TransitionOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if task.generation != generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::GenerationMismatch,
            ));
        }
        if task.status == ExecutionTaskStatus::Running {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::AlreadyApplied(task));
        }
        if task.status != ExecutionTaskStatus::Reserved {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidTaskStatus,
            ));
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        ) || run.pending_terminal.is_some()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }

        sqlx::query(
            "UPDATE moa.execution_run \
             SET status = CASE WHEN status = 'queued' THEN 'running' ELSE status END, \
                 started_at = COALESCE(started_at, NOW()), \
                 wake_epoch = wake_epoch + 1, updated_at = NOW() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let row = sqlx::query(MARK_TASK_RUNNING_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(generation_db)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TransitionOutcome::Applied(task))
    }

    /// Resumes input-waiting work or dispatches a retry under a new generation.
    pub async fn resume_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        kind: ResumeKind,
    ) -> Result<TransitionOutcome> {
        self.resume_task_inner(scope, run_uid, task_id, generation, kind, None)
            .await
    }

    /// Resumes one waiting-input task and atomically appends the exact supplied payload.
    pub async fn resume_task_with_input(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        input: Value,
    ) -> Result<TransitionOutcome> {
        self.resume_task_inner(
            scope,
            run_uid,
            task_id,
            generation,
            ResumeKind::Input,
            Some(input),
        )
        .await
    }

    /// Dispatches one retry when the persisted retry policy has attempts remaining.
    pub async fn retry_task(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
    ) -> Result<TransitionOutcome> {
        self.resume_task_inner(scope, run_uid, task_id, generation, ResumeKind::Retry, None)
            .await
    }

    async fn resume_task_inner(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        kind: ResumeKind,
        resume_input: Option<Value>,
    ) -> Result<TransitionOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
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
            return Ok(TransitionOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        let history_kind = match kind {
            ResumeKind::Input => "input_resume",
            ResumeKind::Retry => "retry",
        };
        if redispatch_is_exact_replay(&task, history_kind, generation, resume_input.as_ref()) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::AlreadyApplied(task));
        }
        if task.generation != generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::GenerationMismatch,
            ));
        }
        if kind == ResumeKind::Retry && task.attempt >= task.retry.max_attempts {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidTaskStatus,
            ));
        }
        let retry_attempt = task.attempt.checked_add(1);
        let (expected_status, allowed_run_status, next_attempt) = match kind {
            ResumeKind::Input => (
                ExecutionTaskStatus::WaitingInput,
                matches!(
                    run.status,
                    ExecutionRunStatus::WaitingInput | ExecutionRunStatus::Running
                ),
                task.attempt,
            ),
            ResumeKind::Retry => (
                ExecutionTaskStatus::Running,
                run.status == ExecutionRunStatus::Running,
                match retry_attempt {
                    Some(attempt) => attempt,
                    None => {
                        conn.commit().await.map_err(storage_error)?;
                        return Ok(TransitionOutcome::Rejected(
                            TransitionRejection::CounterOverflow,
                        ));
                    }
                },
            ),
        };
        if task.status != expected_status {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidTaskStatus,
            ));
        }
        if !allowed_run_status || run.pending_terminal.is_some() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }
        let admission_rejection = if run
            .approved_budget
            .deadline_at
            .is_some_and(|deadline| Utc::now() > deadline)
        {
            Some((
                moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
                TransitionRejection::DeadlineElapsed,
            ))
        } else if resume_budget_exhausted(&run, &task) {
            Some((
                moa_artifacts::execution_plan::ExecutionFailureClass::BudgetExceeded,
                TransitionRejection::BudgetExceeded,
            ))
        } else {
            None
        };
        let Some(next_generation) = generation.checked_add(1) else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::CounterOverflow,
            ));
        };
        let history = json!({
            "kind": history_kind,
            "requested_generation": generation,
            "attempt": next_attempt,
            "generation": next_generation,
            "admission_rejection": admission_rejection
                .as_ref()
                .map(|(_, reason)| format!("{reason:?}")),
        });

        let row = sqlx::query(RESUME_TASK_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(task.status.as_str())
            .bind(to_i64(generation, "task generation")?)
            .bind(
                i32::try_from(next_attempt).map_err(|_| Error::InvalidRepositoryInput {
                    message: "task attempt exceeds PostgreSQL INTEGER".to_string(),
                })?,
            )
            .bind(to_i64(next_generation, "next task generation")?)
            .bind(history)
            .bind(resume_input)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        if let Some((class, reason)) = admission_rejection {
            let task = terminalize_redispatch_rejection(
                &mut conn,
                &run,
                &task,
                history_kind,
                class,
                reason,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Applied(task));
        }
        sqlx::query(
            "UPDATE moa.execution_run \
             SET status = CASE WHEN status IN ('waiting_input', 'waiting_replan') \
                               THEN 'running' ELSE status END, \
                 waiting_reasons = '[]'::JSONB, wake_epoch = wake_epoch + 1, \
                 updated_at = NOW() \
             WHERE run_uid = $1",
        )
        .bind(run_uid)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TransitionOutcome::Applied(task))
    }

    /// Lists one bounded, stable page of visible tasks for a run.
    pub async fn list_tasks(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        page: ExecutionTaskPageRequest,
    ) -> Result<ExecutionTaskPage> {
        let limit = if page.limit == 0 {
            DEFAULT_TASK_PAGE_LIMIT
        } else {
            page.limit.min(MAX_TASK_PAGE_LIMIT)
        };
        let fetch_limit = i64::from(limit) + 1;
        let mut conn = scope.begin(&self.pool).await?;
        let cursor_node_id = page.cursor.as_ref().map(|cursor| cursor.node_id.as_str());
        let cursor_item_key = page.cursor.as_ref().map(|cursor| cursor.item_key.as_str());
        let cursor_task_id = page.cursor.as_ref().map(|cursor| cursor.task_id.as_uuid());
        let rows = sqlx::query(LIST_TASKS_SQL)
            .bind(run_uid)
            .bind(cursor_node_id)
            .bind(cursor_item_key)
            .bind(cursor_task_id)
            .bind(fetch_limit)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let mut tasks = rows.iter().map(task_from_row).collect::<Result<Vec<_>>>()?;
        let has_more = tasks.len() > limit as usize;
        if has_more {
            let _ = tasks.pop();
        }
        let next_cursor = if has_more {
            tasks.last().map(|task| ExecutionTaskCursor {
                node_id: task.node_id.clone(),
                item_key: task.item_key.clone(),
                task_id: task.task_id,
            })
        } else {
            None
        };
        Ok(ExecutionTaskPage { tasks, next_cursor })
    }

    /// Transitions a run into one scheduler wait state under a source-status fence.
    pub async fn transition_run_wait(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_status: ExecutionRunStatus,
        waiting_status: ExecutionRunStatus,
    ) -> Result<TransitionOutcome> {
        self.transition_run_wait_with_reasons(
            scope,
            run_uid,
            expected_status,
            waiting_status,
            Vec::new(),
        )
        .await
    }

    /// Transitions a run and persists the exact scheduler wait reasons atomically.
    pub async fn transition_run_wait_with_reasons(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_status: ExecutionRunStatus,
        waiting_status: ExecutionRunStatus,
        waiting_reasons: Vec<WaitingReason>,
    ) -> Result<TransitionOutcome> {
        if !matches!(
            waiting_status,
            ExecutionRunStatus::WaitingInput
                | ExecutionRunStatus::WaitingReview
                | ExecutionRunStatus::WaitingReplan
                | ExecutionRunStatus::Running
        ) {
            return Err(Error::InvalidRepositoryInput {
                message: "run wait target must be running or one waiting status".to_string(),
            });
        }
        let waiting_value = serde_json::to_value(&waiting_reasons)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::NotFound);
        };
        let current = run_from_row(&row)?;
        if current.status == waiting_status && current.waiting_reasons == waiting_reasons {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::RunAlreadyApplied(current));
        }
        if current.status != expected_status
            || current.status.is_terminal()
            || current.pending_terminal.is_some()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::InvalidRunStatus,
            ));
        }
        if current.status == ExecutionRunStatus::Queued
            && waiting_status != ExecutionRunStatus::Running
        {
            sqlx::query(
                "UPDATE moa.execution_run SET status = 'running', \
                 started_at = COALESCE(started_at, NOW()), updated_at = NOW() \
                 WHERE run_uid = $1 AND status = 'queued'",
            )
            .bind(run_uid)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        }
        let row = sqlx::query(
            "UPDATE moa.execution_run \
             SET status = $2, waiting_reasons = $3, wake_epoch = wake_epoch + 1, \
                 started_at = CASE WHEN $2 = 'running' THEN COALESCE(started_at, NOW()) \
                                   ELSE started_at END, \
                 updated_at = NOW() \
             WHERE run_uid = $1 \
             RETURNING *",
        )
        .bind(run_uid)
        .bind(waiting_status.as_str())
        .bind(waiting_value)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TransitionOutcome::RunApplied(run))
    }

    /// Records a generation-fenced zero- or nonzero-usage outcome for a parked external wait.
    pub async fn complete_external_wait(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        outcome: ExecutionTaskOutcome,
    ) -> Result<TaskOutcomeWrite> {
        self.record_task_outcome(scope, run_uid, task_id, generation, outcome)
            .await
    }

    /// Idempotently audits one action-review resolution under its task generation fence.
    pub async fn record_action_review_resolution(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        review_uid: Uuid,
        resolution: &ExecutionActionReviewResolution,
    ) -> Result<ActionReviewResolutionWrite> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ActionReviewResolutionWrite::NotFound);
        };
        let task = task_from_row(&row)?;
        let review_uid_text = review_uid.to_string();
        if let Some(existing) = task.outcome_audit.iter().find(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("execution_action_review_resolution")
                && entry.get("review_uid").and_then(Value::as_str) == Some(review_uid_text.as_str())
                && entry.get("generation").and_then(Value::as_u64) == Some(generation)
        }) {
            let existing_resolution: ExecutionActionReviewResolution =
                serde_json::from_value(existing.get("resolution").cloned().ok_or_else(|| {
                    Error::InvalidRepositoryData {
                        message: "persisted task review audit is missing its resolution"
                            .to_string(),
                    }
                })?)?;
            if existing_resolution != *resolution {
                return Err(Error::InvalidRepositoryData {
                    message: "task review UID was replayed with a different resolution".to_string(),
                });
            }
            conn.commit().await.map_err(storage_error)?;
            return Ok(ActionReviewResolutionWrite::Replayed);
        }
        let accepted = task.generation == generation && task.status == ExecutionTaskStatus::Running;
        let audit = json!({
            "kind": "execution_action_review_resolution",
            "review_uid": review_uid,
            "generation": generation,
            "accepted": accepted,
            "resolution": resolution,
            "recorded_at": Utc::now(),
        });
        sqlx::query(APPEND_TASK_OUTCOME_AUDIT_SQL)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .bind(audit)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(if accepted {
            ActionReviewResolutionWrite::Applied
        } else {
            ActionReviewResolutionWrite::AuditedStale
        })
    }
}
