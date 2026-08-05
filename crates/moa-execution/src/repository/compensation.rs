//! Compensation registration, fencing, reverse-order claims, and finalization.

use super::*;
use super::{projection::budget_ledger, rows::*, sql::*};
use crate::interpreter::resolve_compensation_input;

async fn load_replan_stop_task(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
) -> Result<Option<ExecutionTaskRecord>> {
    sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
        .bind(run_uid)
        .bind(task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        .map(|row| task_from_row(&row))
        .transpose()
}

fn replan_stop_receipt_audit(receipt: &ReplanStopReceipt) -> Value {
    json!({
        "kind": "replan_stop_fenced",
        "accepted": true,
        "task_id": receipt.task_id,
        "task_generation": receipt.task_generation,
        "base_plan_revision": receipt.base_plan_revision,
        "amendment_hash": receipt.amendment_hash,
        "recorded_at": Utc::now(),
    })
}

fn task_has_replan_stop_receipt(task: &ExecutionTaskRecord, receipt: &ReplanStopReceipt) -> bool {
    let task_id = receipt.task_id.to_string();
    let amendment_hash = receipt.amendment_hash.to_string();
    task.task_id == receipt.task_id
        && task.outcome_audit.iter().any(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("replan_stop_fenced")
                && entry.get("accepted").and_then(Value::as_bool) == Some(true)
                && entry.get("task_id").and_then(Value::as_str) == Some(task_id.as_str())
                && entry.get("task_generation").and_then(Value::as_u64)
                    == Some(receipt.task_generation)
                && entry.get("base_plan_revision").and_then(Value::as_u64)
                    == Some(receipt.base_plan_revision)
                && entry.get("amendment_hash").and_then(Value::as_str)
                    == Some(amendment_hash.as_str())
        })
}

impl ExecutionRepository {
    /// Persists a terminal intent and fences all new forward admission before task settlement.
    pub async fn fence_run_for_terminal(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        expected_wake_epoch: u64,
        pending_terminal: PendingExecutionTerminal,
    ) -> Result<TerminalFenceOutcome> {
        self.fence_run_for_terminal_inner(
            scope,
            run_uid,
            expected_revision,
            expected_wake_epoch,
            pending_terminal,
            None,
        )
        .await
    }

    /// Persists an amendment-driven replan-stop fence and its exact replay receipt atomically.
    pub async fn fence_replan_stop(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        expected_wake_epoch: u64,
        pending_terminal: PendingExecutionTerminal,
        receipt: ReplanStopReceipt,
    ) -> Result<TerminalFenceOutcome> {
        if receipt.base_plan_revision != expected_revision
            || !matches!(
                pending_terminal.terminal_evidence.cause,
                ExecutionTerminalCause::ReplanStop { .. }
            )
        {
            return Err(Error::InvalidRepositoryInput {
                message: "replan-stop receipt must match the fenced revision and terminal cause"
                    .to_string(),
            });
        }
        self.fence_run_for_terminal_inner(
            scope,
            run_uid,
            expected_revision,
            expected_wake_epoch,
            pending_terminal,
            Some(receipt),
        )
        .await
    }

    async fn fence_run_for_terminal_inner(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        expected_wake_epoch: u64,
        pending_terminal: PendingExecutionTerminal,
        replan_stop_receipt: Option<ReplanStopReceipt>,
    ) -> Result<TerminalFenceOutcome> {
        pending_terminal.validate()?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TerminalFenceOutcome::NotFound);
        };
        let current = run_from_row(&run_row)?;
        if current.pending_terminal.as_ref() == Some(&pending_terminal) {
            if let Some(receipt) = replan_stop_receipt {
                let Some(task) = load_replan_stop_task(&mut conn, run_uid, receipt.task_id).await?
                else {
                    conn.commit().await.map_err(storage_error)?;
                    return Ok(TerminalFenceOutcome::NotFound);
                };
                if !task_has_replan_stop_receipt(&task, &receipt) {
                    conn.commit().await.map_err(storage_error)?;
                    return Ok(TerminalFenceOutcome::Conflict);
                }
            }
            let tasks_to_settle = load_nonterminal_tasks(&mut conn, run_uid).await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(TerminalFenceOutcome::Replayed(Box::new(
                TerminalFenceCommit {
                    run: current,
                    tasks_to_settle,
                },
            )));
        }
        if current.plan_revision != expected_revision
            || current.wake_epoch != expected_wake_epoch
            || current.status.is_terminal()
            || current.status == ExecutionRunStatus::Compensating
            || current.pending_terminal.is_some()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TerminalFenceOutcome::Conflict);
        }
        let replan_stop_task = if let Some(receipt) = replan_stop_receipt {
            let Some(task) = load_replan_stop_task(&mut conn, run_uid, receipt.task_id).await?
            else {
                conn.commit().await.map_err(storage_error)?;
                return Ok(TerminalFenceOutcome::NotFound);
            };
            if task.plan_revision != receipt.base_plan_revision
                || task.generation != receipt.task_generation
                || task.status != ExecutionTaskStatus::WaitingReplan
                || !matches!(
                    task.current_outcome.as_ref().map(|outcome| &outcome.result),
                    Some(ExecutionTaskResult::NeedsReplan { .. })
                )
            {
                conn.commit().await.map_err(storage_error)?;
                return Ok(TerminalFenceOutcome::Conflict);
            }
            Some((task, receipt))
        } else {
            None
        };
        let row = sqlx::query(FENCE_RUN_FOR_COMPENSATION_SQL)
            .bind(run_uid)
            .bind(to_i64(expected_revision, "expected plan revision")?)
            .bind(to_i64(expected_wake_epoch, "expected wake epoch")?)
            .bind(pending_terminal.status.as_str())
            .bind(pending_terminal.reason.as_str())
            .bind(serde_json::to_value(PendingTerminalEvidencePayload {
                terminal_evidence: pending_terminal.terminal_evidence.clone(),
                completion_check_results: pending_terminal.completion_check_results.clone(),
                terminal_gaps: pending_terminal.terminal_gaps.clone(),
            })?)
            .bind(&pending_terminal.output)
            .bind(&pending_terminal.cancellation_reason)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TerminalFenceOutcome::Conflict);
        };
        let run = run_from_row(&row)?;
        if let Some((task, receipt)) = replan_stop_task {
            sqlx::query(APPEND_TASK_OUTCOME_AUDIT_SQL)
                .bind(run_uid)
                .bind(task.task_id.as_uuid())
                .bind(replan_stop_receipt_audit(&receipt))
                .fetch_one(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        }
        let tasks_to_settle = load_nonterminal_tasks(&mut conn, run_uid).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TerminalFenceOutcome::Applied(Box::new(
            TerminalFenceCommit {
                run,
                tasks_to_settle,
            },
        )))
    }

    /// Enters `compensating` after every fenced forward task has durably settled.
    pub async fn begin_compensation(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        expected_wake_epoch: u64,
    ) -> Result<BeginCompensationOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(BeginCompensationOutcome::NotFound);
        };
        let current = run_from_row(&run_row)?;
        let pending_tasks = load_nonterminal_tasks(&mut conn, run_uid).await?;
        if !pending_tasks.is_empty() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(BeginCompensationOutcome::ForwardTasksPending(pending_tasks));
        }
        let registrations = load_compensations(&mut conn, run_uid).await?;
        let all_tasks = load_nonterminal_or_terminal_tasks(&mut conn, run_uid).await?;
        for task in all_tasks.iter().filter(|task| {
            task.compensation_contract.is_some() && task.status == ExecutionTaskStatus::Completed
        }) {
            if !registrations
                .iter()
                .any(|registration| registration.forward_task_id == task.task_id)
            {
                return Err(Error::InvalidRepositoryData {
                    message: format!(
                        "completed forward task {} is missing its atomic compensation registration",
                        task.task_id
                    ),
                });
            }
        }
        if current.status == ExecutionRunStatus::Compensating
            && current.plan_revision == expected_revision
            && current.pending_terminal.is_some()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(BeginCompensationOutcome::Replayed(Box::new(
                BeginCompensationCommit {
                    run: current,
                    registrations,
                },
            )));
        }
        if current.plan_revision != expected_revision
            || current.wake_epoch != expected_wake_epoch
            || current.pending_terminal.is_none()
            || current.status.is_terminal()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(BeginCompensationOutcome::Conflict);
        }
        if registrations.is_empty() && !current.manual_repair_required {
            conn.commit().await.map_err(storage_error)?;
            return Ok(BeginCompensationOutcome::NoCompensations(Box::new(current)));
        }
        let row = sqlx::query(BEGIN_COMPENSATION_SQL)
            .bind(run_uid)
            .bind(to_i64(expected_revision, "expected plan revision")?)
            .bind(to_i64(expected_wake_epoch, "expected wake epoch")?)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(BeginCompensationOutcome::Conflict);
        };
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(BeginCompensationOutcome::Applied(Box::new(
            BeginCompensationCommit { run, registrations },
        )))
    }

    /// Installs a held terminal intent after every fenced forward task has settled.
    pub async fn finalize_fenced_terminal(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_revision: u64,
        expected_wake_epoch: u64,
    ) -> Result<FencedTerminalFinalizationOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FencedTerminalFinalizationOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.status.is_terminal() && run.pending_terminal.is_none() {
            let manual = run.manual_repair_required
                && run.status == ExecutionRunStatus::Failed
                && run.terminal_reason == Some(ExecutionTerminalReason::CompensationFailed);
            conn.commit().await.map_err(storage_error)?;
            return Ok(if manual {
                FencedTerminalFinalizationOutcome::ManualRepairRequired(run)
            } else {
                FencedTerminalFinalizationOutcome::Replayed(run)
            });
        }
        let pending_tasks = load_nonterminal_tasks(&mut conn, run_uid).await?;
        if !pending_tasks.is_empty() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FencedTerminalFinalizationOutcome::ForwardTasksPending(
                pending_tasks,
            ));
        }
        if run.plan_revision != expected_revision
            || run.wake_epoch != expected_wake_epoch
            || run.pending_terminal.is_none()
            || run.status.is_terminal()
            || run.status == ExecutionRunStatus::Compensating
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(FencedTerminalFinalizationOutcome::Conflict);
        }
        let pending = run
            .pending_terminal
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "fenced run lost pending terminal intent".to_string(),
            })?;
        if run.manual_repair_required {
            let registrations = load_compensations(&mut conn, run_uid).await?;
            let (compensation_id, outcome) =
                compensation_failure_evidence(&mut conn, run_uid, &registrations).await?;
            let evidence = ExecutionTerminalEvidence {
                cause: ExecutionTerminalCause::CompensationFailure {
                    original_status: pending.status,
                    original_reason: pending.reason,
                    original_cause: Box::new(pending.terminal_evidence.cause.clone()),
                    compensation_id,
                    outcome,
                },
                satisfied_requirement_count: pending.terminal_evidence.satisfied_requirement_count,
                requirement_count: pending.terminal_evidence.requirement_count,
            };
            let row = finalize_compensation_run(
                &mut conn,
                run_uid,
                CompensationTerminalWrite {
                    status: ExecutionRunStatus::Failed,
                    reason: ExecutionTerminalReason::CompensationFailed,
                    evidence: &evidence,
                    completion_check_results: &pending.completion_check_results,
                    terminal_gaps: &pending.terminal_gaps,
                    output: pending.output,
                    manual_repair_required: true,
                },
            )
            .await?;
            let run = run_from_row(&row)?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(FencedTerminalFinalizationOutcome::ManualRepairRequired(run));
        }
        let row = finalize_compensation_run(
            &mut conn,
            run_uid,
            CompensationTerminalWrite {
                status: pending.status,
                reason: pending.reason,
                evidence: &pending.terminal_evidence,
                completion_check_results: &pending.completion_check_results,
                terminal_gaps: &pending.terminal_gaps,
                output: pending.output,
                manual_repair_required: false,
            },
        )
        .await?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(FencedTerminalFinalizationOutcome::Finalized(run))
    }

    /// Loads one complete compensation driver snapshot in strict reverse sequence order.
    pub async fn load_compensation_snapshot(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
    ) -> Result<Option<ExecutionCompensationSnapshot>> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let run = run_from_row(&run_row)?;
        let registrations = load_compensations(&mut conn, run_uid).await?;
        let nonterminal_forward_tasks = load_nonterminal_tasks(&mut conn, run_uid).await?;
        let manual_repair_required = run.manual_repair_required;
        conn.commit().await.map_err(storage_error)?;
        Ok(Some(ExecutionCompensationSnapshot {
            run,
            registrations,
            nonterminal_forward_tasks,
            manual_repair_required,
        }))
    }

    /// Claims exactly the highest unsettled compensation sequence under its generation fence.
    pub async fn claim_next_compensation(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        compensation_id: CompensationId,
        expected_generation: u64,
    ) -> Result<CompensationClaimOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationClaimOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.status != ExecutionRunStatus::Compensating
            || run.manual_repair_required
            || !load_nonterminal_tasks(&mut conn, run_uid).await?.is_empty()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationClaimOutcome::Conflict);
        }
        let Some(compensation_row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(compensation_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationClaimOutcome::NotFound);
        };
        let compensation = compensation_from_row(&compensation_row)?;
        let highest_unsettled: Option<Uuid> = sqlx::query_scalar(
            "SELECT compensation_id FROM moa.execution_compensation \
             WHERE run_uid = $1 AND status <> 'completed' \
             ORDER BY registered_sequence DESC LIMIT 1 FOR UPDATE",
        )
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if highest_unsettled != Some(compensation_id.as_uuid()) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationClaimOutcome::Conflict);
        }
        if compensation.status == CompensationStatus::Running
            && compensation.generation == expected_generation
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationClaimOutcome::Replayed(compensation));
        }
        if compensation.status != CompensationStatus::Pending
            || compensation.generation != expected_generation
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationClaimOutcome::Conflict);
        }
        if compensation.outcome.is_none() {
            let forward_task =
                load_forward_task(&mut conn, run_uid, compensation.forward_task_id).await?;
            let reservation =
                compensation_reservation(&run, &compensation, forward_task.retry.max_attempts)?;
            let mut ledger = budget_ledger(&run);
            if ledger.try_reserve(reservation).is_err() {
                let failed = terminalize_compensation_budget_rejection(
                    &mut conn,
                    &run,
                    &compensation,
                    reservation,
                )
                .await?;
                conn.commit().await.map_err(storage_error)?;
                return Ok(CompensationClaimOutcome::BudgetRejected(failed));
            }
            persist_run_budget(&mut conn, run_uid, &ledger, true).await?;
        }
        let row = sqlx::query(CLAIM_COMPENSATION_SQL)
            .bind(run_uid)
            .bind(compensation_id.as_uuid())
            .bind(to_i64(expected_generation, "compensation generation")?)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(CompensationClaimOutcome::Conflict);
        };
        let claimed = compensation_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationClaimOutcome::Claimed(claimed))
    }

    /// Records one compensation outcome and reconciles its separate bounded reservation.
    pub async fn record_compensation_outcome(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        compensation_id: CompensationId,
        generation: u64,
        outcome: ExecutionCompensationOutcome,
    ) -> Result<CompensationOutcomeWrite> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationOutcomeWrite::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(compensation_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationOutcomeWrite::NotFound);
        };
        let compensation = compensation_from_row(&row)?;
        let exact_settled_replay = compensation.generation == generation
            && compensation.status.is_settled()
            && compensation.outcome.as_ref() == Some(&outcome);
        let exact_requeue_replay = compensation.status == CompensationStatus::Pending
            && generation.checked_add(1) == Some(compensation.generation)
            && compensation.attempt == compensation.generation
            && compensation.outcome.as_ref() == Some(&outcome);
        if exact_settled_replay || exact_requeue_replay {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationOutcomeWrite::Replayed(compensation));
        }
        if run.status != ExecutionRunStatus::Compensating
            || compensation.status != CompensationStatus::Running
            || compensation.generation != generation
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationOutcomeWrite::Conflict);
        }
        let forward_task =
            load_forward_task(&mut conn, run_uid, compensation.forward_task_id).await?;
        let full_reservation =
            compensation_reservation(&run, &compensation, forward_task.retry.max_attempts)?;
        let previous_usage = compensation
            .outcome
            .as_ref()
            .map(ExecutionCompensationOutcome::usage)
            .cloned()
            .unwrap_or_else(zero_usage);
        let remaining = remaining_compensation_reservation(full_reservation, &previous_usage);
        let retryable = matches!(
            outcome,
            ExecutionCompensationOutcome::Failed {
                retryable: true,
                ..
            }
        ) && compensation.attempt < u64::from(forward_task.retry.max_attempts);
        let terminal = !retryable;
        let mut ledger = budget_ledger(&run);
        let reconciliation = ledger.reconcile_cumulative_with_ceiling(
            remaining,
            &previous_usage,
            outcome.usage(),
            terminal,
            i64::MAX as u64,
        )?;
        let (status, attempt, next_generation, manual_repair, error) = match &outcome {
            ExecutionCompensationOutcome::Completed { .. } => (
                CompensationStatus::Completed,
                compensation.attempt,
                compensation.generation,
                false,
                None,
            ),
            ExecutionCompensationOutcome::Failed { message, .. } if retryable => (
                CompensationStatus::Pending,
                compensation.attempt.checked_add(1).ok_or_else(|| {
                    Error::InvalidRepositoryData {
                        message: "compensation attempt overflow".to_string(),
                    }
                })?,
                compensation.generation.checked_add(1).ok_or_else(|| {
                    Error::InvalidRepositoryData {
                        message: "compensation generation overflow".to_string(),
                    }
                })?,
                false,
                Some(json!({"class":"retryable", "message":message})),
            ),
            ExecutionCompensationOutcome::Failed { message, .. } => (
                CompensationStatus::Failed,
                compensation.attempt,
                compensation.generation,
                true,
                Some(json!({"class":"terminal", "message":message})),
            ),
            ExecutionCompensationOutcome::UnknownOutcome { message, .. } => (
                CompensationStatus::UnknownOutcome,
                compensation.attempt,
                compensation.generation,
                true,
                Some(
                    json!({"class":"unknown_outcome", "message":message, "manual_repair_required":true}),
                ),
            ),
        };
        let persisted = persisted_compensation_outcome(&row, Some(outcome.clone()))?;
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET status = $3, attempt = $4, generation = $5, \
             outcome = $6, error = $7, updated_at = NOW(), \
             completed_at = CASE WHEN $8 THEN NOW() ELSE NULL END \
             WHERE run_uid = $1 AND compensation_id = $2 \
             RETURNING compensation_id, run_uid, forward_task_id, registered_sequence, \
             forward_generation, compensator, mapped_input, status, attempt, generation, \
             outcome, error, created_at, updated_at, started_at, completed_at",
        )
        .bind(run_uid)
        .bind(compensation_id.as_uuid())
        .bind(status.as_str())
        .bind(to_i64(attempt, "compensation attempt")?)
        .bind(to_i64(next_generation, "compensation generation")?)
        .bind(serde_json::to_value(persisted)?)
        .bind(error)
        .bind(status.is_settled())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        persist_run_budget_and_repair(&mut conn, run_uid, &reconciliation, manual_repair).await?;
        let updated = compensation_from_row(&updated)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(match status {
            CompensationStatus::Completed => CompensationOutcomeWrite::Completed(updated),
            CompensationStatus::Pending => CompensationOutcomeWrite::Requeued(updated),
            CompensationStatus::Failed => CompensationOutcomeWrite::Failed(updated),
            CompensationStatus::UnknownOutcome => CompensationOutcomeWrite::UnknownOutcome(updated),
            CompensationStatus::Running => CompensationOutcomeWrite::Conflict,
        })
    }

    /// Audits one action-review resolution under a compensation generation fence.
    pub async fn record_compensation_action_review_resolution(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        compensation_id: CompensationId,
        generation: u64,
        review_uid: Uuid,
        resolution: &ExecutionActionReviewResolution,
    ) -> Result<ActionReviewResolutionWrite> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
            .bind(run_uid)
            .bind(compensation_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ActionReviewResolutionWrite::NotFound);
        };
        let compensation = compensation_from_row(&row)?;
        let mut persisted = persisted_compensation_outcome(&row, compensation.outcome.clone())?;
        if let Some(existing) = persisted
            .review_audit
            .iter()
            .find(|entry| entry.review_uid == review_uid && entry.generation == generation)
        {
            if existing.resolution != *resolution {
                return Err(Error::InvalidRepositoryData {
                    message: "compensation review UID was replayed with a different resolution"
                        .to_string(),
                });
            }
            conn.commit().await.map_err(storage_error)?;
            return Ok(ActionReviewResolutionWrite::Replayed);
        }
        let accepted = compensation.status == CompensationStatus::Running
            && compensation.generation == generation;
        persisted.review_audit.push(CompensationReviewAuditEntry {
            review_uid,
            generation,
            accepted,
            resolution: resolution.clone(),
            recorded_at: Utc::now(),
        });
        sqlx::query(
            "UPDATE moa.execution_compensation SET outcome = $3, updated_at = NOW() \
             WHERE run_uid = $1 AND compensation_id = $2",
        )
        .bind(run_uid)
        .bind(compensation_id.as_uuid())
        .bind(serde_json::to_value(persisted)?)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(if accepted {
            ActionReviewResolutionWrite::Applied
        } else {
            ActionReviewResolutionWrite::AuditedStale
        })
    }

    /// Finalizes the original terminal intent or a typed compensation-failure outcome.
    pub async fn finalize_compensation(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        expected_wake_epoch: u64,
    ) -> Result<CompensationFinalizationOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationFinalizationOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.status.is_terminal() && run.pending_terminal.is_none() {
            let manual = run.manual_repair_required
                && run.status == ExecutionRunStatus::Failed
                && run.terminal_reason == Some(ExecutionTerminalReason::CompensationFailed);
            conn.commit().await.map_err(storage_error)?;
            return Ok(if manual {
                CompensationFinalizationOutcome::ManualRepairRequired(run)
            } else {
                CompensationFinalizationOutcome::Replayed(run)
            });
        }
        if run.status != ExecutionRunStatus::Compensating
            || run.wake_epoch != expected_wake_epoch
            || run.pending_terminal.is_none()
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationFinalizationOutcome::Conflict);
        }
        let registrations = load_compensations(&mut conn, run_uid).await?;
        if registrations
            .iter()
            .any(|registration| registration.status == CompensationStatus::Running)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationFinalizationOutcome::Conflict);
        }
        let pending = run
            .pending_terminal
            .clone()
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "compensating run lost pending terminal intent".to_string(),
            })?;
        if run.manual_repair_required
            || registrations.iter().any(|registration| {
                matches!(
                    registration.status,
                    CompensationStatus::Failed | CompensationStatus::UnknownOutcome
                )
            })
        {
            let (compensation_id, outcome) =
                compensation_failure_evidence(&mut conn, run_uid, &registrations).await?;
            let terminal_evidence = ExecutionTerminalEvidence {
                cause: ExecutionTerminalCause::CompensationFailure {
                    original_status: pending.status,
                    original_reason: pending.reason,
                    original_cause: Box::new(pending.terminal_evidence.cause.clone()),
                    compensation_id,
                    outcome,
                },
                satisfied_requirement_count: pending.terminal_evidence.satisfied_requirement_count,
                requirement_count: pending.terminal_evidence.requirement_count,
            };
            let row = finalize_compensation_run(
                &mut conn,
                run_uid,
                CompensationTerminalWrite {
                    status: ExecutionRunStatus::Failed,
                    reason: ExecutionTerminalReason::CompensationFailed,
                    evidence: &terminal_evidence,
                    completion_check_results: &pending.completion_check_results,
                    terminal_gaps: &pending.terminal_gaps,
                    output: pending.output,
                    manual_repair_required: true,
                },
            )
            .await?;
            let run = run_from_row(&row)?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationFinalizationOutcome::ManualRepairRequired(run));
        }
        if registrations
            .iter()
            .any(|registration| registration.status != CompensationStatus::Completed)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationFinalizationOutcome::Conflict);
        }
        let row = finalize_compensation_run(
            &mut conn,
            run_uid,
            CompensationTerminalWrite {
                status: pending.status,
                reason: pending.reason,
                evidence: &pending.terminal_evidence,
                completion_check_results: &pending.completion_check_results,
                terminal_gaps: &pending.terminal_gaps,
                output: pending.output,
                manual_repair_required: false,
            },
        )
        .await?;
        let run = run_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationFinalizationOutcome::Finalized(run))
    }
}

async fn load_nonterminal_tasks(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
) -> Result<Vec<ExecutionTaskRecord>> {
    let rows = sqlx::query(LIST_ALL_TASKS_SQL)
        .bind(run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    Ok(rows
        .iter()
        .map(task_from_row)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter(|task| !task.status.is_terminal())
        .collect())
}

async fn load_compensations(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
) -> Result<Vec<CompensationRegistrationProjection>> {
    sqlx::query(LIST_COMPENSATIONS_SQL)
        .bind(run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        .iter()
        .map(compensation_from_row)
        .collect()
}

async fn load_forward_task(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
) -> Result<ExecutionTaskRecord> {
    let row = sqlx::query(LOAD_TASK_SQL)
        .bind(run_uid)
        .bind(task_id.as_uuid())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    task_from_row(&row)
}

fn compensation_reservation(
    run: &ExecutionRunRecord,
    compensation: &CompensationRegistrationProjection,
    max_attempts: u32,
) -> Result<ExecutionEstimate> {
    let capability = run
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference == compensation.compensator.compensator)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: format!(
                "registered compensation {} has no pinned compensator capability",
                compensation.compensation_id
            ),
        })?;
    capability
        .estimate
        .checked_multiply_resources(u64::from(max_attempts), "compensation retry reservation")
}

fn remaining_compensation_reservation(
    full: ExecutionEstimate,
    used: &ExecutionUsage,
) -> ExecutionEstimate {
    ExecutionEstimate {
        cost_microusd: full.cost_microusd.saturating_sub(used.cost_microusd),
        tokens: full.tokens.saturating_sub(used.tokens),
        tool_calls: full.tool_calls.saturating_sub(used.tool_calls),
        retrieved_bytes: full.retrieved_bytes.saturating_sub(used.retrieved_bytes),
        tasks: 1,
    }
}

fn persisted_compensation_outcome(
    row: &PgRow,
    result: Option<ExecutionCompensationOutcome>,
) -> Result<CompensationPersistedOutcome> {
    let mut persisted: CompensationPersistedOutcome = row
        .try_get::<Option<Value>, _>("outcome")
        .map_err(row_error)?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    persisted.result = result;
    Ok(persisted)
}

async fn persist_run_budget(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    ledger: &BudgetLedger,
    wake: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE moa.execution_run SET reserved_cost_microusd=$2, reserved_tokens=$3, \
         reserved_tasks=$4, reserved_tool_calls=$5, reserved_retrieved_bytes=$6, \
         wake_epoch=wake_epoch+$7, updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run_uid)
    .bind(to_i64(ledger.reserved.cost_microusd, "run reserved cost")?)
    .bind(to_i64(ledger.reserved.tokens, "run reserved tokens")?)
    .bind(to_i64(ledger.reserved.tasks, "run reserved tasks")?)
    .bind(to_i64(
        ledger.reserved.tool_calls,
        "run reserved tool calls",
    )?)
    .bind(to_i64(
        ledger.reserved.retrieved_bytes,
        "run reserved bytes",
    )?)
    .bind(i64::from(wake))
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

async fn persist_run_budget_and_repair(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    reconciliation: &BudgetReconciliation,
    manual_repair: bool,
) -> Result<()> {
    sqlx::query(
        "UPDATE moa.execution_run SET reserved_cost_microusd=$2, reserved_tokens=$3, \
         reserved_tasks=$4, reserved_tool_calls=$5, reserved_retrieved_bytes=$6, \
         consumed_cost_microusd=$7, consumed_tokens=$8, consumed_tasks=$9, \
         consumed_tool_calls=$10, consumed_retrieved_bytes=$11, budget_overrun=$12, \
         manual_repair_required=manual_repair_required OR $13, wake_epoch=wake_epoch+1, \
         updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run_uid)
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
        "run reserved bytes",
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
        "run consumed bytes",
    )?)
    .bind(reconciliation.budget_overrun)
    .bind(manual_repair)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

async fn terminalize_compensation_budget_rejection(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    compensation: &CompensationRegistrationProjection,
    _reservation: ExecutionEstimate,
) -> Result<CompensationRegistrationProjection> {
    let outcome = ExecutionCompensationOutcome::Failed {
        message: "approved execution budget cannot reserve compensation".to_string(),
        retryable: false,
        usage: zero_usage(),
    };
    let row = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
        .bind(run.run_uid)
        .bind(compensation.compensation_id.as_uuid())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let persisted = persisted_compensation_outcome(&row, Some(outcome))?;
    let row = sqlx::query(
        "UPDATE moa.execution_compensation SET status='failed', outcome=$3, \
         error=jsonb_build_object('class','budget_exceeded','message','approved execution budget cannot reserve compensation'), \
         started_at=COALESCE(started_at,NOW()), completed_at=NOW(), updated_at=NOW() WHERE run_uid=$1 AND compensation_id=$2 \
         RETURNING compensation_id, run_uid, forward_task_id, registered_sequence, \
         forward_generation, compensator, mapped_input, status, attempt, generation, \
         outcome, error, created_at, updated_at, started_at, completed_at",
    )
    .bind(run.run_uid)
    .bind(compensation.compensation_id.as_uuid())
    .bind(serde_json::to_value(persisted)?)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    sqlx::query(
        "UPDATE moa.execution_run SET manual_repair_required=TRUE, wake_epoch=wake_epoch+1, updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    compensation_from_row(&row)
}

async fn compensation_failure_evidence(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    registrations: &[CompensationRegistrationProjection],
) -> Result<(CompensationId, ExecutionCompensationOutcome)> {
    if let Some(registration) = registrations.iter().find(|registration| {
        matches!(
            registration.status,
            CompensationStatus::Failed | CompensationStatus::UnknownOutcome
        )
    }) && let Some(outcome) = registration.outcome.clone()
    {
        return Ok((registration.compensation_id, outcome));
    }
    let tasks = load_nonterminal_or_terminal_tasks(conn, run_uid).await?;
    let task = tasks
        .into_iter()
        .find(|task| {
            matches!(
                task.current_outcome.as_ref().map(|outcome| &outcome.result),
                Some(ExecutionTaskResult::UnknownOutcome { .. })
            )
        })
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "manual repair fence has no failed compensation or ambiguous forward task"
                .to_string(),
        })?;
    let current = task
        .current_outcome
        .as_ref()
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "ambiguous forward task lost its outcome".to_string(),
        })?;
    let ExecutionTaskResult::UnknownOutcome { message } = &current.result else {
        return Err(Error::InvalidRepositoryData {
            message: "manual repair task is not an unknown outcome".to_string(),
        });
    };
    Ok((
        CompensationId::derive(task.task_id),
        ExecutionCompensationOutcome::UnknownOutcome {
            message: message.clone(),
            usage: current.usage.clone(),
        },
    ))
}

async fn load_nonterminal_or_terminal_tasks(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
) -> Result<Vec<ExecutionTaskRecord>> {
    sqlx::query(LIST_ALL_TASKS_SQL)
        .bind(run_uid)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        .iter()
        .map(task_from_row)
        .collect()
}

struct CompensationTerminalWrite<'a> {
    status: ExecutionRunStatus,
    reason: ExecutionTerminalReason,
    evidence: &'a ExecutionTerminalEvidence,
    completion_check_results: &'a [Value],
    terminal_gaps: &'a [String],
    output: Option<Value>,
    manual_repair_required: bool,
}

async fn finalize_compensation_run(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    write: CompensationTerminalWrite<'_>,
) -> Result<PgRow> {
    sqlx::query(
        "UPDATE moa.execution_run SET status=$2, terminal_reason=$3, terminal_cause=$4, \
         terminal_satisfied_requirement_count=$5, terminal_requirement_count=$6, \
         completion_check_results=$7, terminal_gaps=$8, output=$9, \
         pending_terminal_status=NULL, pending_terminal_reason=NULL, pending_terminal_cause=NULL, \
         pending_terminal_output=NULL, manual_repair_required=$10, waiting_reasons='[]'::JSONB, \
         wake_epoch=wake_epoch+1, completed_at=NOW(), updated_at=NOW() WHERE run_uid=$1 RETURNING *",
    )
    .bind(run_uid)
    .bind(write.status.as_str())
    .bind(write.reason.as_str())
    .bind(serde_json::to_value(&write.evidence.cause)?)
    .bind(to_i64(
        write.evidence.satisfied_requirement_count,
        "terminal satisfied requirements",
    )?)
    .bind(to_i64(
        write.evidence.requirement_count,
        "terminal requirements",
    )?)
    .bind(serde_json::to_value(write.completion_check_results)?)
    .bind(serde_json::to_value(write.terminal_gaps)?)
    .bind(write.output)
    .bind(write.manual_repair_required)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)
}

fn zero_usage() -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

pub(super) async fn register_compensation_for_completed_task(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    outcome: &ExecutionTaskOutcome,
    replay: bool,
) -> Result<Option<CompensationRegistrationProjection>> {
    let Some(contract) = task.compensation_contract.as_ref() else {
        return Ok(None);
    };
    let ExecutionTaskResult::Completed { output, .. } = &outcome.result else {
        return Ok(None);
    };
    let existing = sqlx::query(LOAD_COMPENSATION_BY_FORWARD_TASK_SQL)
        .bind(run.run_uid)
        .bind(task.task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let mapped_input = run
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference == contract.compensator)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "persisted compensation contract has no pinned compensator".to_string(),
        })
        .and_then(|compensator| {
            resolve_compensation_input(
                &contract.input_mapping,
                &task.input,
                output,
                &compensator.input_schema,
            )
        });
    let (
        mapped_input,
        status,
        expected_outcome,
        persisted_outcome,
        registration_error,
        manual_repair_required,
    ) = match mapped_input {
        Ok(mapped_input) => (
            mapped_input,
            CompensationStatus::Pending,
            None,
            None,
            None,
            false,
        ),
        Err(error) => {
            let message = error.to_string();
            let outcome = ExecutionCompensationOutcome::Failed {
                message: message.clone(),
                retryable: false,
                usage: zero_usage(),
            };
            let persisted = CompensationPersistedOutcome {
                result: Some(outcome.clone()),
                review_audit: Vec::new(),
            };
            (
                Value::Null,
                CompensationStatus::Failed,
                Some(outcome),
                Some(serde_json::to_value(persisted)?),
                Some(json!({
                    "class": "mapping_input_invalid",
                    "message": message,
                })),
                true,
            )
        }
    };
    if let Some(existing) = existing {
        let existing = compensation_from_row(&existing)?;
        // A successfully registered compensation may legitimately advance before the forward
        // task's post-commit replay arrives. Mapping rejection is terminal at registration, so
        // that fail-safe row must remain an exact replay instead of admitting lifecycle drift.
        let lifecycle_matches = match status {
            CompensationStatus::Pending => {
                existing
                    .error
                    .as_ref()
                    .and_then(|error| error.get("class").and_then(Value::as_str))
                    != Some("mapping_input_invalid")
            }
            CompensationStatus::Failed => {
                existing.status == CompensationStatus::Failed
                    && existing.outcome == expected_outcome
                    && existing.error == registration_error
            }
            CompensationStatus::Running
            | CompensationStatus::Completed
            | CompensationStatus::UnknownOutcome => false,
        };
        if existing.compensation_id != CompensationId::derive(task.task_id)
            || existing.run_uid != run.run_uid
            || existing.forward_task_id != task.task_id
            || existing.forward_generation != task.generation
            || existing.compensator != *contract
            || existing.mapped_input != mapped_input
            || !lifecycle_matches
        {
            return Err(Error::InvalidRepositoryData {
                message: "compensation registration replay differs from committed forward outcome"
                    .to_string(),
            });
        }
        return Ok(Some(existing));
    }
    if replay {
        return Err(Error::InvalidRepositoryData {
            message:
                "accepted forward outcome replay is missing its atomic compensation registration"
                    .to_string(),
        });
    }
    let compensation_id = CompensationId::derive(task.task_id);
    let inserted = sqlx::query(INSERT_COMPENSATION_SQL)
        .bind(compensation_id.as_uuid())
        .bind(run.run_uid)
        .bind(task.task_id.as_uuid())
        .bind(to_i64(
            run.next_compensation_sequence,
            "compensation sequence",
        )?)
        .bind(to_i64(task.generation, "forward generation")?)
        .bind(serde_json::to_value(contract)?)
        .bind(mapped_input)
        .bind(status.as_str())
        .bind(persisted_outcome)
        .bind(registration_error)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    if inserted.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: "compensation registration conflicted inside locked forward commit"
                .to_string(),
        });
    }
    sqlx::query(
        "UPDATE moa.execution_run SET next_compensation_sequence=next_compensation_sequence+1, \
         manual_repair_required=manual_repair_required OR $2, updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .bind(manual_repair_required)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let row = sqlx::query(LOAD_COMPENSATION_BY_FORWARD_TASK_SQL)
        .bind(run.run_uid)
        .bind(task.task_id.as_uuid())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    compensation_from_row(&row).map(Some)
}
