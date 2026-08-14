//! Pending-terminal fencing, bounded drain, compensation coordination, and finalization.

use super::*;

const PENDING_TERMINAL_CANCEL_NAMESPACE: Uuid =
    Uuid::from_u128(0xd3d4_9744_5c24_58cc_8be8_4806_faba_1837);
const MAX_PENDING_TERMINAL_PAGE_SIZE: u32 = 1_000;

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

fn replan_stop_receipt_audit(receipt: &ReplanStopReceipt, recorded_at: DateTime<Utc>) -> Value {
    json!({
        "kind": "replan_stop_fenced",
        "accepted": true,
        "task_id": receipt.task_id,
        "task_generation": receipt.task_generation,
        "base_plan_revision": receipt.base_plan_revision,
        "amendment_hash": receipt.amendment_hash,
        "recorded_at": recorded_at,
    })
}

impl ExecutionRepository {
    /// Fences one due approved deadline and advances one bounded terminal-drain page.
    #[allow(clippy::too_many_arguments)]
    pub async fn fence_deadline_and_enqueue_settlement(
        &self,
        config: &ExecutionConfig,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        expected_wake_epoch: u64,
        now: DateTime<Utc>,
        page_limit: u32,
    ) -> Result<PendingTerminalAdvanceOutcome> {
        validate_pending_terminal_page_limit(page_limit)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run) = load_and_lock_pending_terminal_run(&mut conn, config, run_uid).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::NotFound);
        };
        if run.controller_generation != controller_generation
            || run.wake_epoch != expected_wake_epoch
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        if run.budget_deadline_suspended_at.is_some() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        if expected_wake_epoch <= run.processed_wake_epoch {
            let commit = replayed_pending_terminal_commit(&mut conn, config, run).await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Replayed(Box::new(commit)));
        }
        let Some(deadline_at) = run.approved_budget.deadline_at else {
            return Err(Error::InvalidRepositoryData {
                message: "durable execution run is missing its approved deadline".to_string(),
            });
        };
        if deadline_at > now || run.status.is_terminal() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        let requirement_count = u64::try_from(run.goal.requirements.len()).map_err(|_| {
            Error::InvalidRepositoryData {
                message: "execution requirement count exceeds u64".to_string(),
            }
        })?;
        let pending = PendingExecutionTerminal {
            status: ExecutionRunStatus::Failed,
            reason: ExecutionTerminalReason::DeadlineExceeded,
            terminal_evidence: ExecutionTerminalEvidence {
                cause: ExecutionTerminalCause::LimitStop {
                    reason: ExecutionLimitStop::DeadlineExceeded,
                },
                satisfied_requirement_count: 0,
                requirement_count,
            },
            completion_check_results: Vec::new(),
            terminal_gaps: vec!["approved execution deadline elapsed".to_string()],
            output: run.output.clone(),
            cancellation_reason: None,
        };
        pending.validate()?;
        let new_pending = run.pending_terminal.is_none().then_some(pending);
        advance_pending_terminal_page_in_conn(
            conn,
            config,
            run,
            controller_generation,
            expected_wake_epoch,
            new_pending,
            now,
            page_limit,
        )
        .await
    }

    /// Persists one completion-derived terminal intent and advances its first bounded drain page.
    #[allow(clippy::too_many_arguments)]
    pub async fn fence_completion_terminal_and_enqueue_settlement(
        &self,
        config: &ExecutionConfig,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        expected_wake_epoch: u64,
        pending: PendingExecutionTerminal,
        now: DateTime<Utc>,
        page_limit: u32,
    ) -> Result<PendingTerminalAdvanceOutcome> {
        validate_pending_terminal_page_limit(page_limit)?;
        pending.validate()?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run) = load_and_lock_pending_terminal_run(&mut conn, config, run_uid).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::NotFound);
        };
        if run.controller_generation != controller_generation
            || run.wake_epoch != expected_wake_epoch
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        if expected_wake_epoch <= run.processed_wake_epoch {
            let commit = replayed_pending_terminal_commit(&mut conn, config, run).await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Replayed(Box::new(commit)));
        }
        if run.status.is_terminal()
            || run
                .pending_terminal
                .as_ref()
                .is_some_and(|current| current != &pending)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        advance_pending_terminal_page_in_conn(
            conn,
            config,
            run,
            controller_generation,
            expected_wake_epoch,
            Some(pending),
            now,
            page_limit,
        )
        .await
    }

    /// Persists one externally requested cancellation and advances its first bounded drain page.
    ///
    /// Unlike completion-derived terminalization, a public cancellation may arrive after a
    /// storage-only run has already acknowledged its current controller wake. In that case this
    /// transition claims a fresh wake inside the same transaction without dispatching a controller
    /// activation or moving the parked run into active capacity.
    #[allow(clippy::too_many_arguments)]
    pub async fn fence_cancellation_terminal_and_enqueue_settlement(
        &self,
        config: &ExecutionConfig,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        expected_wake_epoch: u64,
        pending: PendingExecutionTerminal,
        now: DateTime<Utc>,
        page_limit: u32,
    ) -> Result<PendingTerminalAdvanceOutcome> {
        validate_pending_terminal_page_limit(page_limit)?;
        pending.validate()?;
        if pending.status != ExecutionRunStatus::Cancelled
            || pending.reason != ExecutionTerminalReason::Cancelled
            || pending.terminal_evidence.cause != ExecutionTerminalCause::Cancellation
        {
            return Err(Error::InvalidRepositoryInput {
                message:
                    "external cancellation fence requires an exact cancellation terminal intent"
                        .to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let Some(mut run) = load_and_lock_pending_terminal_run(&mut conn, config, run_uid).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::NotFound);
        };
        if run.status == ExecutionRunStatus::Cancelled {
            let commit = replayed_pending_terminal_commit(&mut conn, config, run).await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Replayed(Box::new(commit)));
        }
        if run.status.is_terminal() || run.status == ExecutionRunStatus::Compensating {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        if let Some(current) = &run.pending_terminal {
            if current != &pending {
                conn.commit().await.map_err(storage_error)?;
                return Ok(PendingTerminalAdvanceOutcome::Conflict);
            }
            let commit = replayed_pending_terminal_commit(&mut conn, config, run).await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Replayed(Box::new(commit)));
        }
        if run.controller_generation != controller_generation
            || run.wake_epoch != expected_wake_epoch
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        let terminal_wake_epoch = if run.processed_wake_epoch == expected_wake_epoch {
            run = claim_fresh_terminal_mutation_wake_in_conn(&mut conn, &run, now).await?;
            run.wake_epoch
        } else if run.processed_wake_epoch < expected_wake_epoch {
            expected_wake_epoch
        } else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        };
        advance_pending_terminal_page_in_conn(
            conn,
            config,
            run,
            controller_generation,
            terminal_wake_epoch,
            Some(pending),
            now,
            page_limit,
        )
        .await
    }

    /// Persists an exact replan-stop receipt and advances its first bounded terminal-drain page.
    #[allow(clippy::too_many_arguments)]
    pub async fn fence_replan_stop_and_enqueue_settlement(
        &self,
        config: &ExecutionConfig,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        expected_revision: u64,
        expected_wake_epoch: u64,
        pending: PendingExecutionTerminal,
        receipt: ReplanStopReceipt,
        now: DateTime<Utc>,
        page_limit: u32,
    ) -> Result<PendingTerminalAdvanceOutcome> {
        validate_pending_terminal_page_limit(page_limit)?;
        pending.validate()?;
        if receipt.base_plan_revision != expected_revision
            || !matches!(
                pending.terminal_evidence.cause,
                ExecutionTerminalCause::ReplanStop { .. }
            )
        {
            return Err(Error::InvalidRepositoryInput {
                message: "replan-stop receipt must match the fenced revision and terminal cause"
                    .to_string(),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run) = load_and_lock_pending_terminal_run(&mut conn, config, run_uid).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::NotFound);
        };
        if run.controller_generation != controller_generation
            || run.plan_revision != expected_revision
            || run.wake_epoch != expected_wake_epoch
            || run.status.is_terminal()
            || run.status == ExecutionRunStatus::Compensating
            || run
                .pending_terminal
                .as_ref()
                .is_some_and(|current| current != &pending)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        let Some(task) = load_replan_stop_task(&mut conn, run_uid, receipt.task_id).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::NotFound);
        };
        let receipt_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_amendment_receipt \
             WHERE tenant_id=$1 AND run_uid=$2 AND base_plan_revision=$3 \
               AND amendment_hash=$4 AND receipt_kind='replan_stop' \
               AND superseded_task_id=$5 AND task_generation=$6 \
               AND cardinality(task_ids_to_release)=0)",
        )
        .bind(run.tenant_id.0)
        .bind(run.run_uid)
        .bind(to_i64(
            receipt.base_plan_revision,
            "replan-stop plan revision",
        )?)
        .bind(receipt.amendment_hash.to_string())
        .bind(receipt.task_id.as_uuid())
        .bind(to_i64(
            receipt.task_generation,
            "replan-stop task generation",
        )?)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let intent = sqlx::query(
            "SELECT tenant_id,controller_generation,wake_epoch,origin_task_id,task_generation, \
                    base_plan_revision,stop_reason,amendment_hash \
             FROM moa.execution_replan_stop_intent WHERE run_uid=$1 FOR UPDATE",
        )
        .bind(run.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if run.pending_terminal.is_some() {
            if !receipt_exists || intent.is_some() {
                conn.commit().await.map_err(storage_error)?;
                return Ok(PendingTerminalAdvanceOutcome::Conflict);
            }
            if expected_wake_epoch <= run.processed_wake_epoch {
                let commit = replayed_pending_terminal_commit(&mut conn, config, run).await?;
                conn.commit().await.map_err(storage_error)?;
                return Ok(PendingTerminalAdvanceOutcome::Replayed(Box::new(commit)));
            }
            return advance_pending_terminal_page_in_conn(
                conn,
                config,
                run,
                controller_generation,
                expected_wake_epoch,
                None,
                now,
                page_limit,
            )
            .await;
        }
        let Some(intent) = intent else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        };
        let ExecutionTerminalCause::ReplanStop {
            reason: expected_stop_reason,
        } = &pending.terminal_evidence.cause
        else {
            return Err(Error::InvalidRepositoryData {
                message: "replan-stop fence lost its validated terminal cause".to_string(),
            });
        };
        let expected_stop_reason = expected_stop_reason.as_str();
        let intent_exact = intent.try_get::<Uuid, _>("tenant_id").map_err(row_error)?
            == run.tenant_id.0
            && required_u64(&intent, "controller_generation")? == controller_generation
            && required_u64(&intent, "wake_epoch")? == expected_wake_epoch
            && intent
                .try_get::<Uuid, _>("origin_task_id")
                .map_err(row_error)?
                == receipt.task_id.as_uuid()
            && required_u64(&intent, "task_generation")? == receipt.task_generation
            && required_u64(&intent, "base_plan_revision")? == receipt.base_plan_revision
            && intent
                .try_get::<String, _>("stop_reason")
                .map_err(row_error)?
                == expected_stop_reason
            && intent
                .try_get::<String, _>("amendment_hash")
                .map_err(row_error)?
                == receipt.amendment_hash.to_string();
        if receipt_exists
            || !intent_exact
            || task.plan_revision != receipt.base_plan_revision
            || task.generation != receipt.task_generation
            || task.status != ExecutionTaskStatus::WaitingReplan
            || !matches!(
                task.current_outcome.as_ref().map(|outcome| &outcome.result),
                Some(ExecutionTaskResult::NeedsReplan { .. })
            )
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        sqlx::query(
            "INSERT INTO moa.execution_amendment_receipt \
             (tenant_id,run_uid,base_plan_revision,amendment_hash,receipt_kind, \
              superseded_task_id,task_generation,task_ids_to_release,created_at) \
             VALUES ($1,$2,$3,$4,'replan_stop',$5,$6,'{}'::UUID[],$7)",
        )
        .bind(run.tenant_id.0)
        .bind(run.run_uid)
        .bind(to_i64(
            receipt.base_plan_revision,
            "replan-stop plan revision",
        )?)
        .bind(receipt.amendment_hash.to_string())
        .bind(receipt.task_id.as_uuid())
        .bind(to_i64(
            receipt.task_generation,
            "replan-stop task generation",
        )?)
        .bind(now)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let deleted = sqlx::query(
            "DELETE FROM moa.execution_replan_stop_intent WHERE tenant_id=$1 AND run_uid=$2 \
             AND controller_generation=$3 AND wake_epoch=$4",
        )
        .bind(run.tenant_id.0)
        .bind(run.run_uid)
        .bind(to_i64(controller_generation, "controller generation")?)
        .bind(to_i64(expected_wake_epoch, "expected wake epoch")?)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if deleted.rows_affected() != 1 {
            return Err(Error::InvalidRepositoryData {
                message: "replan-stop fence lost its exact durable intent".to_string(),
            });
        }
        sqlx::query(APPEND_TASK_OUTCOME_AUDIT_SQL)
            .bind(run_uid)
            .bind(task.task_id.as_uuid())
            .bind(replan_stop_receipt_audit(&receipt, now))
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        advance_pending_terminal_page_in_conn(
            conn,
            config,
            run,
            controller_generation,
            expected_wake_epoch,
            Some(pending),
            now,
            page_limit,
        )
        .await
    }

    /// Advances one bounded page of an already-fenced pending-terminal drain.
    #[allow(clippy::too_many_arguments)]
    pub async fn advance_pending_terminal_settlement(
        &self,
        config: &ExecutionConfig,
        scope: ExecutionScope,
        run_uid: Uuid,
        controller_generation: u64,
        expected_wake_epoch: u64,
        now: DateTime<Utc>,
        page_limit: u32,
    ) -> Result<PendingTerminalAdvanceOutcome> {
        validate_pending_terminal_page_limit(page_limit)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run) = load_and_lock_pending_terminal_run(&mut conn, config, run_uid).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::NotFound);
        };
        if run.controller_generation != controller_generation
            || run.wake_epoch != expected_wake_epoch
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        if expected_wake_epoch <= run.processed_wake_epoch {
            let commit = replayed_pending_terminal_commit(&mut conn, config, run).await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Replayed(Box::new(commit)));
        }
        if run.pending_terminal.is_none() || run.status.is_terminal() {
            conn.commit().await.map_err(storage_error)?;
            return Ok(PendingTerminalAdvanceOutcome::Conflict);
        }
        advance_pending_terminal_page_in_conn(
            conn,
            config,
            run,
            controller_generation,
            expected_wake_epoch,
            None,
            now,
            page_limit,
        )
        .await
    }
}

enum PendingCompensationDrive {
    Admitted(Box<CompensationAttemptAdmission>),
    Replayed(Box<CompensationAttemptAdmission>),
    CapacityUnavailable { retry_at: DateTime<Utc> },
    ExternalCancellation(ExecutionDispatchRecord),
    Parked,
    Complete,
    ManualRepair(CompensationRegistrationProjection),
}

async fn drive_pending_terminal_compensation_in_conn(
    conn: &mut ScopedConn<'_>,
    config: &ExecutionConfig,
    run: &ExecutionRunRecord,
    now: DateTime<Utc>,
) -> Result<PendingCompensationDrive> {
    let nonterminal_forward_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid=$1 \
         AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome'))",
    )
    .bind(run.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    // `manual_repair_required` is deliberately NOT rejected here. Settling a compensation
    // attempt with a non-retryable failure sets that flag on the run, so rejecting it made
    // the very next controller activation return a terminal repository error and the run
    // sat in `compensating` forever instead of terminalizing `Failed`/`CompensationFailed`.
    // The flag means "stop driving automatically and hand this to an operator", which is a
    // `ManualRepair` outcome, not an invalid state — see the check below the registration
    // load, which needs the row to report which compensation is stuck.
    if run.status != ExecutionRunStatus::Compensating
        || run.pending_terminal.is_none()
        || nonterminal_forward_exists
    {
        return Err(Error::InvalidRepositoryData {
            message: "bounded compensation driver entered from an invalid run state".to_string(),
        });
    }
    let Some(row) = sqlx::query(
        "SELECT * FROM moa.execution_compensation WHERE run_uid=$1 \
         AND status <> 'completed' ORDER BY registered_sequence DESC \
         LIMIT 1 FOR UPDATE",
    )
    .bind(run.run_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    else {
        return Ok(PendingCompensationDrive::Complete);
    };
    let registration = compensation_from_row(&row)?;
    let attempt_state = compensation_attempt_state_from_row(&row)?;
    if run.manual_repair_required
        || matches!(
            registration.status,
            CompensationStatus::Failed | CompensationStatus::UnknownOutcome
        )
    {
        return Ok(PendingCompensationDrive::ManualRepair(registration));
    }
    if attempt_state == CompensationAttemptState::Dispatching {
        return Ok(PendingCompensationDrive::Replayed(Box::new(
            load_existing_compensation_admission(conn, config, run, &row, &registration).await?,
        )));
    }
    if attempt_state == CompensationAttemptState::WaitingExternal {
        let external_job_uid = row
            .try_get::<Option<Uuid>, _>("external_job_uid")
            .map_err(row_error)?
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "waiting-external compensation lost its exact external job UID"
                    .to_string(),
            })?;
        let owner = ExecutionExternalJobOwner::Compensation {
            compensation_id: registration.compensation_id.as_uuid(),
            compensation_generation: registration.generation,
            compensation_attempt_generation: required_u64(&row, "attempt_generation")?,
        };
        return match request_external_job_cancellation_in_conn(
            conn,
            config,
            external_job_uid,
            owner,
            now,
        )
        .await?
        {
            ExecutionExternalJobCancellationRequestOutcome::Applied(dispatch)
            | ExecutionExternalJobCancellationRequestOutcome::Replayed(dispatch) => {
                Ok(PendingCompensationDrive::ExternalCancellation(dispatch))
            }
            ExecutionExternalJobCancellationRequestOutcome::UnboundPendingRecovery => {
                Ok(PendingCompensationDrive::Parked)
            }
            ExecutionExternalJobCancellationRequestOutcome::AlreadyTerminal => {
                let job = load_external_job_for_update_in_conn(conn.as_mut(), external_job_uid)
                    .await?
                    .ok_or_else(|| Error::InvalidRepositoryData {
                        message: "terminal compensation external job disappeared".to_string(),
                    })?;
                settle_external_job_terminal_in_conn(conn, &job, now).await?;
                Ok(PendingCompensationDrive::Parked)
            }
            ExecutionExternalJobCancellationRequestOutcome::NotFound
            | ExecutionExternalJobCancellationRequestOutcome::Stale => {
                Err(Error::InvalidRepositoryData {
                    message: "waiting-external compensation has a stale external job owner"
                        .to_string(),
                })
            }
        };
    }
    if matches!(
        attempt_state,
        CompensationAttemptState::Running
            | CompensationAttemptState::Cancelling
            | CompensationAttemptState::WaitingReview
    ) {
        return Ok(PendingCompensationDrive::Parked);
    }
    if attempt_state != CompensationAttemptState::Idle
        || !matches!(
            registration.status,
            CompensationStatus::Pending | CompensationStatus::Running
        )
    {
        return Err(Error::InvalidRepositoryData {
            message: "highest reverse-order compensation is not dispatchable or settled"
                .to_string(),
        });
    }
    let retry_at = checked_retry_at(config, now)?;
    if !compensation_capacity_available(conn, run.tenant_id).await? {
        return Ok(PendingCompensationDrive::CapacityUnavailable { retry_at });
    }
    if registration.status == CompensationStatus::Pending && registration.outcome.is_none() {
        let forward_task =
            load_forward_task(conn, run.run_uid, registration.forward_task_id).await?;
        let reservation =
            compensation_reservation(run, &registration, forward_task.retry.max_attempts)?;
        let mut ledger = budget_ledger(run);
        if ledger.try_reserve(reservation).is_err() {
            let failed =
                terminalize_compensation_budget_rejection(conn, run, &registration, reservation)
                    .await?;
            return Ok(PendingCompensationDrive::ManualRepair(failed));
        }
        persist_run_budget(conn, run.run_uid, &ledger, false).await?;
    }
    let deadline = checked_attempt_deadline(config, now)?;
    let attempt_generation = required_u64(&row, "attempt_generation")?;
    let dispatch_uid = Uuid::now_v7();
    let watchdog_uid = Uuid::now_v7();
    let reservation_uid = reserve_compensation_attempt_capacity(
        conn,
        config,
        run,
        &registration,
        attempt_generation,
        deadline,
        now,
    )
    .await?;
    let watchdog = create_trigger_with_dispatch_in_conn(
        conn.as_mut(),
        config,
        &compensation_trigger(
            run,
            &registration,
            attempt_generation,
            watchdog_uid,
            ExecutionTriggerKind::CompensationWatchdog,
            deadline,
            json!({}),
        ),
    )
    .await?;
    let attempt_request = ExecutionCompensationAttemptRequest {
        dispatch_uid,
        capacity_reservation_uid: reservation_uid,
        watchdog_trigger_uid: watchdog.trigger.trigger_uid,
        watchdog_dispatch_uid: watchdog.dispatch.dispatch_uid,
        run_uid: run.run_uid,
        compensation_id: registration.compensation_id,
        compensation_generation: registration.generation,
        compensation_attempt_generation: attempt_generation,
        controller_generation: run.controller_generation,
        attempt_deadline_at: deadline,
        tenant_id: run.tenant_id,
    };
    let dispatch = enqueue_dispatch_in_conn(
        conn.as_mut(),
        &compensation_dispatch(run, &attempt_request, now)?,
    )
    .await?;
    let updated = sqlx::query(
        "UPDATE moa.execution_compensation SET status='running', \
         attempt_state='dispatching', attempt_started_at=$6, \
         last_progress_at=GREATEST(last_progress_at,$6), \
         attempt_deadline_at=$7, waiting_since=NULL, active_dispatch_uid=$8, \
         dispatch_sequence=dispatch_sequence+1, started_at=COALESCE(started_at,$6), \
         updated_at=NOW() WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
         AND attempt_generation=$4 AND attempt_state='idle' \
         AND status IN ('pending','running') AND EXISTS ( \
             SELECT 1 FROM moa.execution_run AS current_run \
             WHERE current_run.run_uid=$1 AND current_run.controller_generation=$5) \
         RETURNING *",
    )
    .bind(run.run_uid)
    .bind(registration.compensation_id.as_uuid())
    .bind(to_i64(registration.generation, "compensation generation")?)
    .bind(to_i64(
        attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(now)
    .bind(deadline)
    .bind(dispatch_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: "bounded compensation admission lost its exact row lock".to_string(),
    })?;
    Ok(PendingCompensationDrive::Admitted(Box::new(
        CompensationAttemptAdmission {
            attempt: compensation_attempt_from_row(&updated, run)?,
            capacity_reservation_uid: reservation_uid,
            dispatch,
            watchdog,
        },
    )))
}

fn validate_pending_terminal_page_limit(page_limit: u32) -> Result<()> {
    if page_limit == 0 || page_limit > MAX_PENDING_TERMINAL_PAGE_SIZE {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "pending-terminal page limit must be between 1 and {MAX_PENDING_TERMINAL_PAGE_SIZE}"
            ),
        });
    }
    Ok(())
}

async fn load_and_lock_pending_terminal_run(
    conn: &mut ScopedConn<'_>,
    config: &ExecutionConfig,
    run_uid: Uuid,
) -> Result<Option<ExecutionRunRecord>> {
    let Some(visible_row) = sqlx::query(LOAD_RUN_SQL)
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(None);
    };
    let visible = run_from_row(&visible_row)?;
    prelock_capacity_dimensions_in_tx(
        conn.as_mut(),
        config,
        visible.tenant_id,
        &[
            ExecutionCapacityDimension::ActiveRuns,
            ExecutionCapacityDimension::ActiveTasks,
            ExecutionCapacityDimension::ParkedRuns,
            ExecutionCapacityDimension::ScheduledTriggers,
            ExecutionCapacityDimension::ExternalJobs,
        ],
    )
    .await?;
    let row = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let run = run_from_row(&row)?;
    if run.tenant_id != visible.tenant_id {
        return Err(Error::InvalidRepositoryData {
            message: "execution run tenant changed while acquiring compensation capacity locks"
                .to_string(),
        });
    }
    Ok(Some(run))
}

async fn claim_fresh_terminal_mutation_wake_in_conn(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    now: DateTime<Utc>,
) -> Result<ExecutionRunRecord> {
    let row = sqlx::query(
        "UPDATE moa.execution_run SET wake_epoch=wake_epoch+1, updated_at=$4 \
         WHERE run_uid=$1 AND controller_generation=$2 AND wake_epoch=$3 \
           AND processed_wake_epoch=wake_epoch AND pending_terminal_status IS NULL \
           AND status NOT IN ('completed','partial','blocked','unsupported','failed','cancelled', \
                              'compensating') RETURNING *",
    )
    .bind(run.run_uid)
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(to_i64(run.wake_epoch, "wake epoch")?)
    .bind(now)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::Storage {
        message: "external cancellation lost its fresh terminal-wake fence".to_string(),
    })?;
    run_from_row(&row)
}

async fn replayed_pending_terminal_commit(
    conn: &mut ScopedConn<'_>,
    config: &ExecutionConfig,
    run: ExecutionRunRecord,
) -> Result<PendingTerminalAdvanceCommit> {
    let work_remaining: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid=$1 \
             AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome')) \
         OR EXISTS (SELECT 1 FROM moa.execution_compensation WHERE run_uid=$1 \
             AND status <> 'completed') \
         OR EXISTS (SELECT 1 FROM moa.execution_capacity_reservation WHERE run_uid=$1 \
             AND resource_dimension IN ('active_tasks','scheduled_triggers','external_jobs') \
             AND state IN ('reserved','reconciling'))",
    )
    .bind(run.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let compensation_admission = if run.status == ExecutionRunStatus::Compensating {
        let row = sqlx::query(
            "SELECT * FROM moa.execution_compensation WHERE run_uid=$1 \
             AND status <> 'completed' ORDER BY registered_sequence DESC LIMIT 1 FOR UPDATE",
        )
        .bind(run.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if let Some(row) = row {
            if compensation_attempt_state_from_row(&row)? == CompensationAttemptState::Dispatching {
                let registration = compensation_from_row(&row)?;
                Some(Box::new(
                    load_existing_compensation_admission(conn, config, &run, &row, &registration)
                        .await?,
                ))
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };
    let cancellation_dispatches =
        load_current_terminal_task_cancellation_dispatches(conn, &run, config.max_in_flight_tasks)
            .await?;
    let continuation = load_pending_terminal_continuation(conn, &run).await?;
    let stage = if run.status.is_terminal() {
        if run.manual_repair_required {
            PendingTerminalAdvanceStage::ManualRepairRequired
        } else {
            PendingTerminalAdvanceStage::Finalized
        }
    } else if compensation_admission.is_some() {
        PendingTerminalAdvanceStage::CompensationQueued
    } else if work_remaining {
        PendingTerminalAdvanceStage::Draining
    } else {
        PendingTerminalAdvanceStage::EnqueuedPage
    };
    Ok(PendingTerminalAdvanceCommit {
        run,
        stage,
        settled_task_count: 0,
        drained_trigger_count: 0,
        cancellation_dispatches,
        compensation_admission,
        continuation: continuation.map(Box::new),
        work_remaining,
    })
}

async fn load_pending_terminal_continuation(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
) -> Result<Option<ExecutionDispatchRecord>> {
    let row = sqlx::query(
        "SELECT dispatch_uid, not_before_at, payload, wake_epoch \
         FROM moa.execution_dispatch_outbox WHERE run_uid=$1 \
           AND dispatch_kind='run_activation' AND controller_generation=$2 \
           AND payload->>'source_wake_epoch'=$3 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(run.run_uid)
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(run.processed_wake_epoch.to_string())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let wake_epoch = required_u64(&row, "wake_epoch")?;
    let request = NewExecutionDispatch {
        dispatch_uid: row.try_get("dispatch_uid").map_err(row_error)?,
        tenant_id: run.tenant_id,
        run_uid: Some(run.run_uid),
        task_id: None,
        compensation_id: None,
        trigger_uid: None,
        external_job_uid: None,
        kind: ExecutionDispatchKind::RunActivation,
        controller_generation: Some(run.controller_generation),
        wake_epoch: Some(wake_epoch),
        attempt_generation: None,
        compensation_generation: None,
        compensation_attempt_generation: None,
        not_before_at: row.try_get("not_before_at").map_err(row_error)?,
        payload: row.try_get("payload").map_err(row_error)?,
    };
    enqueue_dispatch_in_conn(conn.as_mut(), &request)
        .await
        .map(Some)
}

#[allow(clippy::too_many_arguments)]
async fn advance_pending_terminal_page_in_conn(
    mut conn: ScopedConn<'_>,
    config: &ExecutionConfig,
    mut run: ExecutionRunRecord,
    controller_generation: u64,
    expected_wake_epoch: u64,
    new_pending: Option<PendingExecutionTerminal>,
    now: DateTime<Utc>,
    page_limit: u32,
) -> Result<PendingTerminalAdvanceOutcome> {
    if let Some(pending) = new_pending {
        if let Some(current) = &run.pending_terminal {
            if current != &pending {
                conn.commit().await.map_err(storage_error)?;
                return Ok(PendingTerminalAdvanceOutcome::Conflict);
            }
        } else {
            let row = sqlx::query(
                "UPDATE moa.execution_run SET pending_terminal_status=$4, \
                     pending_terminal_reason=$5, pending_terminal_cause=$6, \
                     pending_terminal_output=$7, cancellation_reason=$8, \
                     next_wake_at=NULL, \
                     updated_at=$9 WHERE run_uid=$1 AND controller_generation=$2 \
                     AND wake_epoch=$3 AND pending_terminal_status IS NULL \
                     AND status NOT IN ('completed','partial','blocked','unsupported', \
                                        'failed','cancelled','compensating') RETURNING *",
            )
            .bind(run.run_uid)
            .bind(to_i64(controller_generation, "controller generation")?)
            .bind(to_i64(expected_wake_epoch, "expected wake epoch")?)
            .bind(pending.status.as_str())
            .bind(pending.reason.as_str())
            .bind(serde_json::to_value(PendingTerminalEvidencePayload {
                terminal_evidence: pending.terminal_evidence.clone(),
                completion_check_results: pending.completion_check_results.clone(),
                terminal_gaps: pending.terminal_gaps.clone(),
            })?)
            .bind(&pending.output)
            .bind(&pending.cancellation_reason)
            .bind(now)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let Some(row) = row else {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(PendingTerminalAdvanceOutcome::Conflict);
            };
            run = run_from_row(&row)?;
        }
    }
    let pending = run
        .pending_terminal
        .clone()
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "terminal drain lost its pending terminal intent".to_string(),
        })?;
    let cancel_reason = if pending.reason == ExecutionTerminalReason::DeadlineExceeded {
        ExecutionAttemptCancelReason::DeadlineExceeded
    } else {
        ExecutionAttemptCancelReason::RunTerminal
    };
    let task_rows = sqlx::query(
        "SELECT task.* FROM moa.execution_task AS task WHERE task.run_uid=$1 \
           AND task.status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome') \
           AND (task.attempt_state <> 'cancelling' OR (task.external_job_uid IS NULL AND EXISTS ( \
               SELECT 1 FROM moa.execution_capacity_reservation AS active \
               WHERE active.run_uid=task.run_uid AND active.task_id=task.task_id \
                 AND active.attempt_generation=task.attempt_generation \
                 AND active.resource_dimension='active_tasks' \
                 AND active.state IN ('reserved','reconciling')))) \
         ORDER BY CASE WHEN task.attempt_state IN ('dispatching','running','cancelling') \
                       THEN 0 ELSE 1 END, \
                  task.task_id LIMIT $2 FOR UPDATE",
    )
    .bind(run.run_uid)
    .bind(i64::from(page_limit))
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let tasks = task_rows
        .iter()
        .map(task_from_row)
        .collect::<Result<Vec<_>>>()?;
    let processed_task_count =
        u32::try_from(tasks.len()).map_err(|_| Error::InvalidRepositoryData {
            message: "terminal drain task page exceeds u32".to_string(),
        })?;
    let storage_task_ids = tasks
        .iter()
        .filter(|task| {
            task.status != ExecutionTaskStatus::WaitingExternal
                && !matches!(
                    task.attempt_state,
                    ExecutionAttemptState::Dispatching | ExecutionAttemptState::Running
                )
        })
        .map(|task| task.task_id.as_uuid())
        .collect::<Vec<_>>();
    supersede_storage_task_waits(&mut conn, run.run_uid, run.tenant_id.0, &storage_task_ids)
        .await?;
    let mut settled_task_count = 0_u64;
    let mut cancellation_dispatches = Vec::with_capacity(tasks.len());
    for task in tasks {
        if task.status == ExecutionTaskStatus::WaitingExternal {
            let external_job_uid =
                task.external_job_uid
                    .ok_or_else(|| Error::InvalidRepositoryData {
                        message: "waiting-external task lost its exact external job UID"
                            .to_string(),
                    })?;
            let owner = ExecutionExternalJobOwner::Task {
                task_id: task.task_id.as_uuid(),
                attempt_generation: task.attempt_generation,
            };
            match request_external_job_cancellation_in_conn(
                &mut conn,
                config,
                external_job_uid,
                owner,
                now,
            )
            .await?
            {
                ExecutionExternalJobCancellationRequestOutcome::Applied(dispatch)
                | ExecutionExternalJobCancellationRequestOutcome::Replayed(dispatch) => {
                    cancellation_dispatches.push(dispatch);
                }
                ExecutionExternalJobCancellationRequestOutcome::UnboundPendingRecovery => {}
                ExecutionExternalJobCancellationRequestOutcome::AlreadyTerminal => {
                    let job = load_external_job_for_update_in_conn(conn.as_mut(), external_job_uid)
                        .await?
                        .ok_or_else(|| Error::InvalidRepositoryData {
                            message: "terminal external job disappeared under its owner fence"
                                .to_string(),
                        })?;
                    settle_task_external_job_terminal_in_conn(&mut conn, &job, now).await?;
                }
                ExecutionExternalJobCancellationRequestOutcome::NotFound
                | ExecutionExternalJobCancellationRequestOutcome::Stale => {
                    return Err(Error::InvalidRepositoryData {
                        message: "waiting-external task has a stale external job owner".to_string(),
                    });
                }
            }
            continue;
        }
        if matches!(
            task.attempt_state,
            ExecutionAttemptState::Dispatching
                | ExecutionAttemptState::Running
                | ExecutionAttemptState::Cancelling
        ) {
            cancellation_dispatches.push(
                enqueue_pending_terminal_task_cancellation(
                    &mut conn,
                    &run,
                    &task,
                    cancel_reason,
                    pending.reason,
                    now,
                )
                .await?,
            );
            continue;
        }
        let original_status = task.status;
        let input_audience = if original_status == ExecutionTaskStatus::WaitingInput {
            Some(
                task.current_outcome
                    .as_ref()
                    .and_then(|outcome| match &outcome.result {
                        ExecutionTaskResult::NeedsInput { audience, .. } => Some(audience.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| Error::InvalidRepositoryData {
                        message: "terminal drain lost the waiting-input audience".to_string(),
                    })?,
            )
        } else {
            None
        };
        match record_task_outcome_in_conn(
            &mut conn,
            run.run_uid,
            task.task_id,
            task.generation,
            cancelled_task_outcome(
                format!("run terminal fence: {}", pending.reason.as_str()),
                task.actual.clone(),
            ),
        )
        .await?
        {
            TaskOutcomeWrite::Applied { task, .. } | TaskOutcomeWrite::Replayed { task, .. } => {
                if let Some(audience) = input_audience.as_ref() {
                    transition_node_counters_with_input_audience_in_tx(
                        &mut conn,
                        run.run_uid,
                        &task.node_id,
                        &task.item_key,
                        original_status,
                        ExecutionTaskStatus::Cancelled,
                        audience,
                    )
                    .await?;
                    refresh_run_after_wait_settlement_in_conn(
                        &mut conn,
                        run.run_uid,
                        task.task_id,
                        now,
                    )
                    .await?;
                } else {
                    transition_node_counters_in_tx(
                        &mut conn,
                        run.run_uid,
                        &task.node_id,
                        &task.item_key,
                        original_status,
                        ExecutionTaskStatus::Cancelled,
                    )
                    .await?;
                }
            }
            TaskOutcomeWrite::Rejected { reason, .. } => {
                return Err(Error::InvalidRepositoryData {
                    message: format!("terminal drain task settlement was rejected: {reason:?}"),
                });
            }
            TaskOutcomeWrite::NotFound => {
                return Err(Error::InvalidRepositoryData {
                    message: "terminal drain lost a row-locked task".to_string(),
                });
            }
        }
        settled_task_count =
            settled_task_count
                .checked_add(1)
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "terminal drain settled-task count overflow".to_string(),
                })?;
    }

    let task_dispatch_count = cancellation_dispatches.len();
    let remaining_slots = page_limit.saturating_sub(processed_task_count);
    if remaining_slots > 0 && run.status != ExecutionRunStatus::Compensating {
        let compensation_rows = sqlx::query(
            "SELECT compensation.* FROM moa.execution_compensation AS compensation \
             WHERE compensation.run_uid=$1 \
               AND compensation.attempt_state IN ('dispatching','running') \
             ORDER BY compensation.registered_sequence DESC LIMIT $2 FOR UPDATE",
        )
        .bind(run.run_uid)
        .bind(i64::from(remaining_slots))
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        for row in compensation_rows {
            cancellation_dispatches.push(
                enqueue_pending_terminal_compensation_cancellation(
                    &mut conn,
                    &run,
                    &row,
                    cancel_reason,
                    pending.reason,
                    now,
                )
                .await?,
            );
        }
    }
    let compensation_cancellation_count = cancellation_dispatches
        .len()
        .checked_sub(task_dispatch_count)
        .and_then(|count| u32::try_from(count).ok())
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "terminal drain compensation cancellation count overflow".to_string(),
        })?;
    let charged_after_cancellations = processed_task_count
        .checked_add(compensation_cancellation_count)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "terminal drain page accounting overflow after cancellation".to_string(),
        })?;
    let trigger_slots = page_limit.saturating_sub(charged_after_cancellations);

    let nonterminal_forward_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_task WHERE run_uid=$1 \
         AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome')",
    )
    .bind(run.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let actionable_forward_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid=$1 \
         AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome') \
         AND attempt_state <> 'cancelling' AND status <> 'waiting_external')",
    )
    .bind(run.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let active_count = active_attempt_capacity_count(&mut conn, run.run_uid).await?;
    let has_registrations: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_compensation WHERE run_uid=$1)",
    )
    .bind(run.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let retain_cancelled_effects = pending.status == ExecutionRunStatus::Cancelled
        && run.active_plan.definition.cancel_policy == ExecutionCancelPolicy::RetainEffects;
    let should_compensate = has_registrations && !retain_cancelled_effects;
    let active_trigger_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_trigger WHERE run_uid=$1 \
         AND state = 'pending')",
    )
    .bind(run.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let cleanup_triggers_now = nonterminal_forward_count == 0
        && active_count == 0
        && !should_compensate
        && run.status != ExecutionRunStatus::Compensating;
    let (mut drained_trigger_count, mut trigger_work_remaining) =
        if cleanup_triggers_now && trigger_slots > 0 {
            let page = drain_run_triggers_page_in_conn(&mut conn, &run, trigger_slots).await?;
            (page.drained_trigger_count, page.work_remaining)
        } else if cleanup_triggers_now {
            (0, active_trigger_exists)
        } else {
            (0, false)
        };
    let ready_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_task WHERE run_uid=$1 AND status='ready'",
    )
    .bind(run.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;

    let mut stage = PendingTerminalAdvanceStage::Draining;
    let mut work_remaining = nonterminal_forward_count > 0 || active_count > 0;
    let mut continuation_payload = None;
    let mut continuation_not_before = now;
    let mut checkpoint_status = run.status;
    let mut checkpoint_active_count = active_count;
    let mut compensation_admission = None;
    if actionable_forward_exists || trigger_work_remaining {
        stage = PendingTerminalAdvanceStage::EnqueuedPage;
        work_remaining = true;
        continuation_payload = Some(json!({
            "reason":"pending_terminal_page",
            "source_wake_epoch": expected_wake_epoch,
        }));
    } else if nonterminal_forward_count == 0 && active_count == 0 {
        if should_compensate && run.status != ExecutionRunStatus::Compensating {
            stage = PendingTerminalAdvanceStage::EnqueuedPage;
            checkpoint_status = ExecutionRunStatus::Compensating;
            work_remaining = true;
            continuation_payload = Some(json!({
                "reason":"pending_terminal_compensation",
                "source_wake_epoch": expected_wake_epoch,
            }));
        } else if run.status == ExecutionRunStatus::Compensating {
            match drive_pending_terminal_compensation_in_conn(&mut conn, config, &run, now).await? {
                PendingCompensationDrive::Admitted(admission)
                | PendingCompensationDrive::Replayed(admission) => {
                    stage = PendingTerminalAdvanceStage::CompensationQueued;
                    work_remaining = true;
                    compensation_admission = Some(admission);
                    checkpoint_active_count =
                        active_attempt_capacity_count(&mut conn, run.run_uid).await?;
                }
                PendingCompensationDrive::CapacityUnavailable { retry_at } => {
                    stage = PendingTerminalAdvanceStage::EnqueuedPage;
                    work_remaining = true;
                    continuation_not_before = retry_at;
                    continuation_payload = Some(json!({
                        "reason":"pending_terminal_compensation_capacity",
                        "source_wake_epoch": expected_wake_epoch,
                    }));
                }
                PendingCompensationDrive::ExternalCancellation(dispatch) => {
                    cancellation_dispatches.push(dispatch);
                    work_remaining = true;
                }
                PendingCompensationDrive::Parked => {
                    work_remaining = true;
                }
                PendingCompensationDrive::ManualRepair(registration) => {
                    if trigger_slots > 0 {
                        let page =
                            drain_run_triggers_page_in_conn(&mut conn, &run, trigger_slots).await?;
                        drained_trigger_count = page.drained_trigger_count;
                        trigger_work_remaining = page.work_remaining;
                    } else {
                        trigger_work_remaining = active_trigger_exists;
                    }
                    if trigger_work_remaining {
                        stage = PendingTerminalAdvanceStage::EnqueuedPage;
                        work_remaining = true;
                        continuation_payload = Some(json!({
                            "reason":"pending_terminal_manual_repair_cleanup",
                            "source_wake_epoch": expected_wake_epoch,
                        }));
                    } else {
                        let non_lifetime_capacity_exists: bool = sqlx::query_scalar(
                            "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
                             WHERE run_uid=$1 AND resource_dimension IN \
                             ('active_tasks','scheduled_triggers','external_jobs') \
                             AND state IN ('reserved','reconciling'))",
                        )
                        .bind(run.run_uid)
                        .fetch_one(conn.as_mut())
                        .await
                        .map_err(sqlx_error)?;
                        if non_lifetime_capacity_exists {
                            return Err(Error::InvalidRepositoryData {
                                message: "failed compensation retained non-lifetime capacity"
                                    .to_string(),
                            });
                        }
                        let failure = compensation_failure_pending(&pending, &registration)?;
                        replace_pending_terminal_exact(
                            &mut conn,
                            &run,
                            &pending,
                            &failure,
                            controller_generation,
                            expected_wake_epoch,
                            now,
                        )
                        .await?;
                        let finalized = finalize_pending_terminal_exact(
                            &mut conn,
                            &run,
                            &failure,
                            controller_generation,
                            expected_wake_epoch,
                            now,
                        )
                        .await?;
                        conn.commit().await.map_err(storage_error)?;
                        return Ok(PendingTerminalAdvanceOutcome::Applied(Box::new(
                            PendingTerminalAdvanceCommit {
                                run: finalized,
                                stage: PendingTerminalAdvanceStage::ManualRepairRequired,
                                settled_task_count,
                                drained_trigger_count,
                                cancellation_dispatches,
                                compensation_admission: None,
                                continuation: None,
                                work_remaining: false,
                            },
                        )));
                    }
                }
                PendingCompensationDrive::Complete => {
                    if trigger_slots > 0 {
                        let page =
                            drain_run_triggers_page_in_conn(&mut conn, &run, trigger_slots).await?;
                        drained_trigger_count = page.drained_trigger_count;
                        trigger_work_remaining = page.work_remaining;
                    } else {
                        trigger_work_remaining = active_trigger_exists;
                    }
                    if trigger_work_remaining {
                        stage = PendingTerminalAdvanceStage::EnqueuedPage;
                        work_remaining = true;
                        continuation_payload = Some(json!({
                            "reason":"pending_terminal_trigger_cleanup",
                            "source_wake_epoch": expected_wake_epoch,
                        }));
                    } else {
                        let non_lifetime_capacity_exists: bool = sqlx::query_scalar(
                            "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
                             WHERE run_uid=$1 AND resource_dimension IN \
                             ('active_tasks','scheduled_triggers','external_jobs') \
                             AND state IN ('reserved','reconciling'))",
                        )
                        .bind(run.run_uid)
                        .fetch_one(conn.as_mut())
                        .await
                        .map_err(sqlx_error)?;
                        if non_lifetime_capacity_exists {
                            return Err(Error::InvalidRepositoryData {
                                message: "completed compensation retained non-lifetime capacity"
                                    .to_string(),
                            });
                        }
                        let finalized = finalize_pending_terminal_exact(
                            &mut conn,
                            &run,
                            &pending,
                            controller_generation,
                            expected_wake_epoch,
                            now,
                        )
                        .await?;
                        conn.commit().await.map_err(storage_error)?;
                        return Ok(PendingTerminalAdvanceOutcome::Applied(Box::new(
                            PendingTerminalAdvanceCommit {
                                run: finalized,
                                stage: PendingTerminalAdvanceStage::Finalized,
                                settled_task_count,
                                drained_trigger_count,
                                cancellation_dispatches,
                                compensation_admission: None,
                                continuation: None,
                                work_remaining: false,
                            },
                        )));
                    }
                }
            }
        } else {
            let non_lifetime_capacity_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation WHERE run_uid=$1 \
                 AND resource_dimension IN ('active_tasks','scheduled_triggers','external_jobs') \
                 AND state IN ('reserved','reconciling'))",
            )
            .bind(run.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if non_lifetime_capacity_exists {
                work_remaining = true;
            } else {
                let finalized = finalize_pending_terminal_exact(
                    &mut conn,
                    &run,
                    &pending,
                    controller_generation,
                    expected_wake_epoch,
                    now,
                )
                .await?;
                stage = if finalized.manual_repair_required {
                    PendingTerminalAdvanceStage::ManualRepairRequired
                } else {
                    PendingTerminalAdvanceStage::Finalized
                };
                conn.commit().await.map_err(storage_error)?;
                return Ok(PendingTerminalAdvanceOutcome::Applied(Box::new(
                    PendingTerminalAdvanceCommit {
                        run: finalized,
                        stage,
                        settled_task_count,
                        drained_trigger_count,
                        cancellation_dispatches,
                        compensation_admission: None,
                        continuation: None,
                        work_remaining: false,
                    },
                )));
            }
        }
    }

    let checkpointed = checkpoint_pending_terminal_wake(
        &mut conn,
        run.run_uid,
        controller_generation,
        expected_wake_epoch,
        checkpoint_status,
        u64::try_from(ready_count).map_err(|_| Error::InvalidRepositoryData {
            message: "terminal drain ready-task count is negative".to_string(),
        })?,
        checkpoint_active_count,
        now,
    )
    .await?;
    let continuation = if let Some(payload) = continuation_payload {
        Some(Box::new(
            enqueue_run_activation_in_conn(
                conn.as_mut(),
                checkpointed.tenant_id,
                checkpointed.run_uid,
                checkpointed.controller_generation,
                continuation_not_before,
                payload,
            )
            .await?,
        ))
    } else {
        None
    };
    let row = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(run.run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    run = run_from_row(&row)?;
    conn.commit().await.map_err(storage_error)?;
    Ok(PendingTerminalAdvanceOutcome::Applied(Box::new(
        PendingTerminalAdvanceCommit {
            run,
            stage,
            settled_task_count,
            drained_trigger_count,
            cancellation_dispatches,
            compensation_admission,
            continuation,
            work_remaining,
        },
    )))
}

async fn enqueue_pending_terminal_task_cancellation(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    reason: ExecutionAttemptCancelReason,
    terminal_reason: ExecutionTerminalReason,
    now: DateTime<Utc>,
) -> Result<ExecutionDispatchRecord> {
    let row = sqlx::query(
        "SELECT reservation.reservation_uid, reservation.controller_generation, \
                trigger.trigger_uid \
         FROM moa.execution_capacity_reservation AS reservation \
         JOIN moa.execution_trigger AS trigger ON trigger.run_uid=reservation.run_uid \
          AND trigger.task_id=reservation.task_id \
          AND trigger.controller_generation=reservation.controller_generation \
          AND trigger.attempt_generation=reservation.attempt_generation \
          AND trigger.trigger_kind='task_watchdog' \
          AND trigger.state = 'pending' \
         WHERE reservation.run_uid=$1 AND reservation.task_id=$2 \
          AND reservation.attempt_generation=$3 \
          AND reservation.resource_dimension='active_tasks' \
          AND reservation.state IN ('reserved','reconciling') FOR UPDATE OF reservation, trigger",
    )
    .bind(run.run_uid)
    .bind(task.task_id.as_uuid())
    .bind(to_i64(task.attempt_generation, "task attempt generation")?)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: format!(
            "active task {} is missing its exact capacity or watchdog receipt",
            task.task_id
        ),
    })?;
    let active_dispatch_uid =
        task.active_dispatch_uid
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: format!(
                    "active task {} is missing its dispatch identity",
                    task.task_id
                ),
            })?;
    let capacity_reservation_uid: Uuid = row.try_get("reservation_uid").map_err(row_error)?;
    let attempt_controller_generation = required_u64(&row, "controller_generation")?;
    let watchdog_trigger_uid: Uuid = row.try_get("trigger_uid").map_err(row_error)?;
    let cancellation_dispatch_uid = pending_terminal_cancel_dispatch_uid(
        active_dispatch_uid,
        run.controller_generation,
        terminal_reason,
    );
    let cancelling = sqlx::query(
        "UPDATE moa.execution_task SET attempt_state='cancelling', \
             last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 \
             AND generation=$3 AND attempt_generation=$4 AND active_dispatch_uid=$5 \
             AND attempt_state IN ('dispatching','running','cancelling')",
    )
    .bind(run.run_uid)
    .bind(task.task_id.as_uuid())
    .bind(to_i64(task.generation, "task generation")?)
    .bind(to_i64(task.attempt_generation, "task attempt generation")?)
    .bind(active_dispatch_uid)
    .bind(now)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if cancelling.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: format!("task {} lost its terminal cancellation fence", task.task_id),
        });
    }
    let reconciling = sqlx::query(
        "UPDATE moa.execution_capacity_reservation SET state='reconciling', updated_at=$2 \
         WHERE reservation_uid=$1 AND state IN ('reserved','reconciling')",
    )
    .bind(capacity_reservation_uid)
    .bind(now)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if reconciling.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: format!("task {} lost its active-capacity receipt", task.task_id),
        });
    }
    let payload = serde_json::to_value(ExecutionTaskAttemptCancelRequest {
        cancellation_dispatch_uid,
        tenant_id: run.tenant_id,
        run_uid: run.run_uid,
        task_id: task.task_id,
        controller_generation: run.controller_generation,
        attempt_controller_generation,
        task_generation: task.generation,
        attempt_generation: task.attempt_generation,
        active_dispatch_uid,
        capacity_reservation_uid,
        watchdog_trigger_uid,
        reason,
    })?;
    enqueue_dispatch_in_conn(
        conn.as_mut(),
        &NewExecutionDispatch {
            dispatch_uid: cancellation_dispatch_uid,
            tenant_id: run.tenant_id,
            run_uid: Some(run.run_uid),
            task_id: Some(task.task_id.as_uuid()),
            compensation_id: None,
            trigger_uid: None,
            external_job_uid: task.external_job_uid,
            kind: ExecutionDispatchKind::TaskAttemptCancel,
            controller_generation: Some(run.controller_generation),
            wake_epoch: None,
            attempt_generation: Some(task.attempt_generation),
            compensation_generation: None,
            compensation_attempt_generation: None,
            not_before_at: now,
            payload,
        },
    )
    .await
}

async fn enqueue_pending_terminal_compensation_cancellation(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    compensation_row: &PgRow,
    reason: ExecutionAttemptCancelReason,
    terminal_reason: ExecutionTerminalReason,
    now: DateTime<Utc>,
) -> Result<ExecutionDispatchRecord> {
    let registration = compensation_from_row(compensation_row)?;
    let attempt_generation = required_u64(compensation_row, "attempt_generation")?;
    let active_dispatch_uid: Uuid = compensation_row
        .try_get("active_dispatch_uid")
        .map_err(row_error)?;
    let receipt = sqlx::query(
        "SELECT reservation.reservation_uid, trigger.trigger_uid \
         FROM moa.execution_capacity_reservation AS reservation \
         JOIN moa.execution_trigger AS trigger ON trigger.run_uid=reservation.run_uid \
          AND trigger.compensation_id=reservation.compensation_id \
          AND trigger.controller_generation=reservation.controller_generation \
          AND trigger.compensation_generation=reservation.compensation_generation \
          AND trigger.compensation_attempt_generation=reservation.compensation_attempt_generation \
          AND trigger.trigger_kind='compensation_watchdog' \
          AND trigger.state = 'pending' \
         WHERE reservation.run_uid=$1 AND reservation.compensation_id=$2 \
          AND reservation.controller_generation=$3 AND reservation.compensation_generation=$4 \
          AND reservation.compensation_attempt_generation=$5 \
          AND reservation.resource_dimension='active_tasks' \
          AND reservation.state IN ('reserved','reconciling') FOR UPDATE OF reservation, trigger",
    )
    .bind(run.run_uid)
    .bind(registration.compensation_id.as_uuid())
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(to_i64(registration.generation, "compensation generation")?)
    .bind(to_i64(
        attempt_generation,
        "compensation attempt generation",
    )?)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: format!(
            "active compensation {} is missing its exact capacity or watchdog receipt",
            registration.compensation_id
        ),
    })?;
    let capacity_reservation_uid: Uuid = receipt.try_get("reservation_uid").map_err(row_error)?;
    let watchdog_trigger_uid: Uuid = receipt.try_get("trigger_uid").map_err(row_error)?;
    let cancellation_dispatch_uid = pending_terminal_cancel_dispatch_uid(
        active_dispatch_uid,
        run.controller_generation,
        terminal_reason,
    );
    let intent = compensation_release_intent(reason);
    let cancelling = sqlx::query(
        "UPDATE moa.execution_compensation SET attempt_state='cancelling', \
             release_intent=$7, last_progress_at=GREATEST(last_progress_at,$6), \
             updated_at=NOW() \
         WHERE run_uid=$1 AND compensation_id=$2 \
             AND generation=$3 AND attempt_generation=$4 AND active_dispatch_uid=$5 \
             AND attempt_state IN ('dispatching','running')",
    )
    .bind(run.run_uid)
    .bind(registration.compensation_id.as_uuid())
    .bind(to_i64(registration.generation, "compensation generation")?)
    .bind(to_i64(
        attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(active_dispatch_uid)
    .bind(now)
    .bind(compensation_release_intent_label(intent))
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if cancelling.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "compensation {} lost its terminal cancellation fence",
                registration.compensation_id
            ),
        });
    }
    let reconciling = sqlx::query(
        "UPDATE moa.execution_capacity_reservation SET state='reconciling', updated_at=$2 \
         WHERE reservation_uid=$1 AND state IN ('reserved','reconciling')",
    )
    .bind(capacity_reservation_uid)
    .bind(now)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if reconciling.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: format!(
                "compensation {} lost its active-capacity receipt",
                registration.compensation_id
            ),
        });
    }
    let payload = serde_json::to_value(ExecutionCompensationAttemptCancelRequest {
        cancellation_dispatch_uid,
        tenant_id: run.tenant_id,
        run_uid: run.run_uid,
        compensation_id: registration.compensation_id,
        controller_generation: run.controller_generation,
        attempt_controller_generation: run.controller_generation,
        compensation_generation: registration.generation,
        compensation_attempt_generation: attempt_generation,
        active_dispatch_uid,
        capacity_reservation_uid,
        watchdog_trigger_uid,
        intent,
    })?;
    enqueue_dispatch_in_conn(
        conn.as_mut(),
        &NewExecutionDispatch {
            dispatch_uid: cancellation_dispatch_uid,
            tenant_id: run.tenant_id,
            run_uid: Some(run.run_uid),
            task_id: None,
            compensation_id: Some(registration.compensation_id.as_uuid()),
            trigger_uid: None,
            external_job_uid: None,
            kind: ExecutionDispatchKind::CompensationAttemptCancel,
            controller_generation: Some(run.controller_generation),
            wake_epoch: None,
            attempt_generation: None,
            compensation_generation: Some(registration.generation),
            compensation_attempt_generation: Some(attempt_generation),
            not_before_at: now,
            payload,
        },
    )
    .await
}

fn pending_terminal_cancel_dispatch_uid(
    active_dispatch_uid: Uuid,
    controller_generation: u64,
    terminal_reason: ExecutionTerminalReason,
) -> Uuid {
    let name = format!(
        "{active_dispatch_uid}:{controller_generation}:{}",
        terminal_reason.as_str()
    );
    Uuid::new_v5(&PENDING_TERMINAL_CANCEL_NAMESPACE, name.as_bytes())
}

fn compensation_release_intent(
    reason: ExecutionAttemptCancelReason,
) -> ExecutionCompensationReleaseIntent {
    match reason {
        ExecutionAttemptCancelReason::DeadlineExceeded => {
            ExecutionCompensationReleaseIntent::Deadline
        }
        ExecutionAttemptCancelReason::RunTerminal => {
            ExecutionCompensationReleaseIntent::RunTerminal
        }
        ExecutionAttemptCancelReason::PauseRequested => ExecutionCompensationReleaseIntent::Pause,
        ExecutionAttemptCancelReason::ExternalJobStarted => {
            ExecutionCompensationReleaseIntent::ExternalJob
        }
    }
}

async fn supersede_storage_task_waits(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    tenant_id: Uuid,
    task_ids: &[Uuid],
) -> Result<()> {
    if task_ids.is_empty() {
        return Ok(());
    }
    let trigger_uids = sqlx::query_scalar::<_, Uuid>(
        "UPDATE moa.execution_trigger SET state='superseded', updated_at=NOW() \
             WHERE run_uid=$1 AND task_id = ANY($2::UUID[]) \
             AND trigger_kind <> 'task_watchdog' AND state = 'pending' \
             RETURNING trigger_uid",
    )
    .bind(run_uid)
    .bind(task_ids)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if trigger_uids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET state='cancelled', claim_owner=NULL, claimed_at=NULL, claim_expires_at=NULL, \
             updated_at=NOW() \
         WHERE trigger_uid = ANY($1::UUID[]) AND state IN ('pending','dispatching')",
    )
    .bind(&trigger_uids)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let receipt_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_capacity_reservation AS reservation \
         JOIN moa.execution_trigger AS trigger \
           ON trigger.trigger_uid=reservation.trigger_uid \
          AND trigger.tenant_id=reservation.tenant_id \
          AND trigger.run_uid IS NOT DISTINCT FROM reservation.run_uid \
          AND trigger.controller_generation IS NOT DISTINCT FROM reservation.controller_generation \
         WHERE reservation.trigger_uid = ANY($1::UUID[]) \
           AND reservation.tenant_id=$2 AND reservation.run_uid=$3 \
           AND reservation.resource_dimension='scheduled_triggers'",
    )
    .bind(&trigger_uids)
    .bind(tenant_id)
    .bind(run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if usize::try_from(receipt_count).ok() != Some(trigger_uids.len()) {
        return Err(Error::InvalidRepositoryData {
            message: "storage-wait trigger capacity receipts do not match their exact owners"
                .to_string(),
        });
    }
    let released_quantities = sqlx::query_scalar::<_, i64>(
        "UPDATE moa.execution_capacity_reservation \
         SET state='released', released_at=NOW(), updated_at=NOW() \
         WHERE trigger_uid = ANY($1::UUID[]) AND tenant_id=$2 AND run_uid=$3 \
           AND resource_dimension='scheduled_triggers' \
           AND state IN ('reserved','reconciling') AND released_at IS NULL \
         RETURNING quantity",
    )
    .bind(&trigger_uids)
    .bind(tenant_id)
    .bind(run_uid)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let released_quantity = released_quantities
        .into_iter()
        .try_fold(0_i64, i64::checked_add)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "storage-wait trigger capacity quantity overflowed PostgreSQL BIGINT"
                .to_string(),
        })?;
    if released_quantity == 0 {
        return Ok(());
    }
    let buckets = sqlx::query(
        "UPDATE moa.execution_capacity_bucket \
         SET reserved_quantity=reserved_quantity-$2, version=version+1, updated_at=NOW() \
         WHERE resource_dimension='scheduled_triggers' AND reserved_quantity >= $2 \
           AND ((scope_kind='fleet' AND tenant_id IS NULL) \
                OR (scope_kind='tenant' AND tenant_id=$1))",
    )
    .bind(tenant_id)
    .bind(released_quantity)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if buckets.rows_affected() != 2 {
        return Err(Error::InvalidRepositoryData {
            message: "storage-wait trigger release did not decrement both capacity buckets"
                .to_string(),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn checkpoint_pending_terminal_wake(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
    controller_generation: u64,
    expected_wake_epoch: u64,
    status: ExecutionRunStatus,
    ready_task_count: u64,
    active_task_count: u64,
    now: DateTime<Utc>,
) -> Result<ExecutionRunRecord> {
    let row = sqlx::query(
        "UPDATE moa.execution_run SET status=$4, activation_state='idle', \
             next_wake_at=NULL, waiting_since=NULL, ready_task_count=$5, \
             active_task_count=$6, processed_wake_epoch=$3, activation_failure_count=0, \
             last_progress_at=GREATEST(last_progress_at,$7), updated_at=NOW() \
         WHERE run_uid=$1 AND controller_generation=$2 AND wake_epoch >= $3 \
           AND processed_wake_epoch < $3 \
           AND activation_state IN ('idle','queued','advancing','paused') RETURNING *",
    )
    .bind(run_uid)
    .bind(to_i64(controller_generation, "controller generation")?)
    .bind(to_i64(expected_wake_epoch, "expected wake epoch")?)
    .bind(status.as_str())
    .bind(to_i64(ready_task_count, "ready task count")?)
    .bind(to_i64(active_task_count, "active task count")?)
    .bind(now)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: "terminal drain lost its controller wake checkpoint fence".to_string(),
    })?;
    run_from_row(&row)
}

fn compensation_failure_pending(
    original: &PendingExecutionTerminal,
    registration: &CompensationRegistrationProjection,
) -> Result<PendingExecutionTerminal> {
    let outcome = registration
        .outcome
        .clone()
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "failed compensation is missing its terminal outcome".to_string(),
        })?;
    let pending = PendingExecutionTerminal {
        status: ExecutionRunStatus::Failed,
        reason: ExecutionTerminalReason::CompensationFailed,
        terminal_evidence: ExecutionTerminalEvidence {
            cause: ExecutionTerminalCause::CompensationFailure {
                original_status: original.status,
                original_reason: original.reason,
                original_cause: Box::new(original.terminal_evidence.cause.clone()),
                compensation_id: registration.compensation_id,
                outcome,
            },
            satisfied_requirement_count: original.terminal_evidence.satisfied_requirement_count,
            requirement_count: original.terminal_evidence.requirement_count,
        },
        completion_check_results: original.completion_check_results.clone(),
        terminal_gaps: original.terminal_gaps.clone(),
        output: original.output.clone(),
        cancellation_reason: None,
    };
    pending.validate()?;
    Ok(pending)
}

async fn replace_pending_terminal_exact(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    expected: &PendingExecutionTerminal,
    replacement: &PendingExecutionTerminal,
    controller_generation: u64,
    expected_wake_epoch: u64,
    now: DateTime<Utc>,
) -> Result<()> {
    let expected_payload = serde_json::to_value(PendingTerminalEvidencePayload {
        terminal_evidence: expected.terminal_evidence.clone(),
        completion_check_results: expected.completion_check_results.clone(),
        terminal_gaps: expected.terminal_gaps.clone(),
    })?;
    let replacement_payload = serde_json::to_value(PendingTerminalEvidencePayload {
        terminal_evidence: replacement.terminal_evidence.clone(),
        completion_check_results: replacement.completion_check_results.clone(),
        terminal_gaps: replacement.terminal_gaps.clone(),
    })?;
    let updated = sqlx::query(
        "UPDATE moa.execution_run SET pending_terminal_status=$5, \
         pending_terminal_reason=$6, pending_terminal_cause=$7, pending_terminal_output=$8, \
         cancellation_reason=NULL, manual_repair_required=TRUE, updated_at=$9 \
         WHERE run_uid=$1 AND controller_generation=$2 AND wake_epoch >= $3 \
           AND pending_terminal_cause=$4 AND status='compensating'",
    )
    .bind(run.run_uid)
    .bind(to_i64(controller_generation, "controller generation")?)
    .bind(to_i64(expected_wake_epoch, "expected wake epoch")?)
    .bind(expected_payload)
    .bind(replacement.status.as_str())
    .bind(replacement.reason.as_str())
    .bind(replacement_payload)
    .bind(&replacement.output)
    .bind(now)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if updated.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: "compensation failure lost its exact pending-terminal replacement fence"
                .to_string(),
        });
    }
    Ok(())
}

async fn finalize_pending_terminal_exact(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    pending: &PendingExecutionTerminal,
    controller_generation: u64,
    expected_wake_epoch: u64,
    now: DateTime<Utc>,
) -> Result<ExecutionRunRecord> {
    release_owned_run_capacity_in_tx(
        conn.as_mut(),
        run.tenant_id,
        run.run_uid,
        run.controller_generation,
    )
    .await?;
    let evidence_payload = serde_json::to_value(PendingTerminalEvidencePayload {
        terminal_evidence: pending.terminal_evidence.clone(),
        completion_check_results: pending.completion_check_results.clone(),
        terminal_gaps: pending.terminal_gaps.clone(),
    })?;
    let row = sqlx::query(
        "UPDATE moa.execution_run SET status=$4, terminal_reason=$5, terminal_cause=$6, \
             terminal_satisfied_requirement_count=$7, terminal_requirement_count=$8, \
             completion_check_results=$9, terminal_gaps=$10, output=$11, \
             pending_terminal_status=NULL, pending_terminal_reason=NULL, \
             pending_terminal_cause=NULL, pending_terminal_output=NULL, \
             reserved_cost_microusd=0, reserved_tokens=0, reserved_tasks=0, \
             reserved_tool_calls=0, reserved_retrieved_bytes=0, \
             activation_state='terminal', waiting_reasons='[]'::JSONB, next_wake_at=NULL, \
             budget_deadline_suspended_at=NULL, \
             waiting_task_count=0, waiting_input_task_count=0, waiting_review_task_count=0, \
             waiting_signal_task_count=0, waiting_timer_task_count=0, \
             waiting_external_task_count=0, waiting_replan_task_count=0, \
             waiting_input_user_task_count=0, waiting_input_tenant_admin_task_count=0, \
             waiting_input_external_task_count=0, waiting_reasons_truncated=FALSE, \
             waiting_since=NULL, ready_task_count=0, active_task_count=0, \
             processed_wake_epoch=$3, activation_failure_count=0, completed_at=$12, \
             last_progress_at=GREATEST(last_progress_at,$12), updated_at=NOW() \
         WHERE run_uid=$1 AND controller_generation=$2 AND wake_epoch >= $3 \
           AND processed_wake_epoch < $3 AND pending_terminal_cause=$13 \
           AND NOT EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid=$1 \
             AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome')) \
           AND NOT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation WHERE run_uid=$1 \
             AND resource_dimension IN ('active_tasks','scheduled_triggers','external_jobs') \
             AND state IN ('reserved','reconciling')) \
         RETURNING *",
    )
    .bind(run.run_uid)
    .bind(to_i64(controller_generation, "controller generation")?)
    .bind(to_i64(expected_wake_epoch, "expected wake epoch")?)
    .bind(pending.status.as_str())
    .bind(pending.reason.as_str())
    .bind(serde_json::to_value(&pending.terminal_evidence.cause)?)
    .bind(to_i64(
        pending.terminal_evidence.satisfied_requirement_count,
        "terminal satisfied requirement count",
    )?)
    .bind(to_i64(
        pending.terminal_evidence.requirement_count,
        "terminal requirement count",
    )?)
    .bind(serde_json::to_value(&pending.completion_check_results)?)
    .bind(serde_json::to_value(&pending.terminal_gaps)?)
    .bind(&pending.output)
    .bind(now)
    .bind(evidence_payload)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: "terminal drain lost its final exact fence".to_string(),
    })?;
    run_from_row(&row)
}
