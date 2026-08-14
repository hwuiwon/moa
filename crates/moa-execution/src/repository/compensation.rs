//! Compensation registration, fencing, reverse-order claims, and finalization.

mod pending_terminal;

use super::*;
use super::{
    capacity::{
        CapacityReleaseOutcome, CapacityReserveOutcome, ExecutionCapacityDimension,
        advance_tenant_fairness, capacity_bucket_has_room, compensation_attempt_capacity_request,
        prelock_capacity_dimensions_in_tx, prelock_existing_capacity_dimensions_in_tx,
        release_capacity_in_tx, release_owned_run_capacity_in_tx, reserve_capacity_in_tx,
    },
    external_job::{
        ExecutionExternalJobCancellationRequestOutcome, ExecutionExternalJobIntentReleaseOutcome,
        ExecutionExternalJobOwner, ExecutionExternalJobRecord, ExecutionExternalJobState,
        NewExecutionExternalJobIntent, load_external_job_for_update_in_conn,
        release_external_job_intent_in_conn, request_external_job_cancellation_in_conn,
    },
    outbox::{
        ExecutionDispatchKind, ExecutionDispatchRecord, NewExecutionDispatch,
        enqueue_dispatch_in_conn,
    },
    outcome::record_task_outcome_in_conn,
    projection::budget_ledger,
    ready::{transition_node_counters_in_tx, transition_node_counters_with_input_audience_in_tx},
    rows::*,
    run::{enqueue_run_activation_in_conn, load_current_terminal_task_cancellation_dispatches},
    sql::*,
    task::settle_external_job_terminal_in_conn as settle_task_external_job_terminal_in_conn,
    terminal::{
        PendingTerminalAdvanceCommit, PendingTerminalAdvanceOutcome, PendingTerminalAdvanceStage,
        ReplanStopReceipt, drain_run_triggers_page_in_conn,
    },
    transition::refresh_run_after_wait_settlement_in_conn,
    trigger::{
        ExecutionTriggerKind, ExecutionTriggerWrite, NewExecutionTrigger,
        create_trigger_with_dispatch_in_conn, release_trigger_capacity_in_conn, trigger_from_row,
    },
};
use crate::{
    interpreter::resolve_compensation_input,
    state::{
        ExecutionLimitStop, ExecutionTerminalCause, ExecutionTerminalEvidence,
        cancelled_task_outcome,
    },
    wire::{
        ExecutionAttemptCancelReason, ExecutionCompensationAttemptCancelRequest,
        ExecutionCompensationAttemptRequest, ExecutionCompensationReleaseIntent,
        ExecutionTaskAttemptCancelRequest,
    },
};
use chrono::Duration;
use moa_artifacts::execution_plan::ExecutionCancelPolicy;
use moa_config::ExecutionConfig;
use moa_core::types::sandbox_workspace::{ExecutionHandReleaseOwner, ExecutionHandReleaseReceipt};

/// Durable lifecycle of one bounded compensation-attempt slice.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum CompensationAttemptState {
    /// No slice currently owns capacity or a dispatch.
    Idle,
    /// The immutable slice dispatch is pending delivery.
    Dispatching,
    /// The slice is actively executing.
    Running,
    /// Provider teardown was requested while capacity remains owned.
    Cancelling,
    /// The slice is parked on an exact action-policy review.
    WaitingReview,
    /// The slice is parked on asynchronous provider-owned work.
    WaitingExternal,
    /// The logical compensation settled definitively.
    Terminal,
    /// The compensating effect may have committed without an authoritative result.
    UnknownOutcome,
}

impl CompensationAttemptState {
    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Dispatching => "dispatching",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::WaitingReview => "waiting_review",
            Self::WaitingExternal => "waiting_external",
            Self::Terminal => "terminal",
            Self::UnknownOutcome => "unknown_outcome",
        }
    }
}

impl FromStr for CompensationAttemptState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "idle" => Ok(Self::Idle),
            "dispatching" => Ok(Self::Dispatching),
            "running" => Ok(Self::Running),
            "cancelling" => Ok(Self::Cancelling),
            "waiting_review" => Ok(Self::WaitingReview),
            "waiting_external" => Ok(Self::WaitingExternal),
            "terminal" => Ok(Self::Terminal),
            "unknown_outcome" => Ok(Self::UnknownOutcome),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown compensation attempt state `{value}`"),
            }),
        }
    }
}

/// Current durable compensation slice plus its admitted principal.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompensationAttemptRecord {
    /// Immutable logical compensation registration.
    pub registration: CompensationRegistrationProjection,
    /// Authoritative locked run, including admitted identity, session, catalog, and scope.
    pub run: ExecutionRunRecord,
    /// Run-controller generation fencing this slice.
    pub controller_generation: u64,
    /// Bounded slice generation, separate from logical effect generation.
    pub attempt_generation: u64,
    /// Current bounded slice state.
    pub attempt_state: CompensationAttemptState,
    /// First active timestamp for the current slice.
    pub attempt_started_at: Option<DateTime<Utc>>,
    /// Latest monotonic progress timestamp.
    pub last_progress_at: DateTime<Utc>,
    /// Absolute watchdog deadline for an active slice.
    pub attempt_deadline_at: Option<DateTime<Utc>>,
    /// Time at which an external review wait began.
    pub waiting_since: Option<DateTime<Utc>>,
    /// Immutable dispatch identity for the current slice.
    pub active_dispatch_uid: Option<Uuid>,
    /// Exact asynchronous provider job owned by the parked compensation.
    pub external_job_uid: Option<Uuid>,
    /// Truthful ownership-transfer intent while the slice is cancelling.
    pub release_intent: Option<ExecutionCompensationReleaseIntent>,
    /// Monotonic count of dispatches created for this registration.
    pub dispatch_sequence: u64,
}

/// Exact identity fence carried by a bounded compensation workflow.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompensationAttemptFence {
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable logical compensation registration.
    pub compensation_id: CompensationId,
    /// Exact run-controller generation.
    pub controller_generation: u64,
    /// Exact logical effect generation.
    pub compensation_generation: u64,
    /// Exact bounded slice generation.
    pub attempt_generation: u64,
    /// Immutable dispatch identity of the bounded slice.
    pub dispatch_uid: Uuid,
}

/// One admitted compensation slice and all durable delivery receipts.
#[derive(Clone, Debug, PartialEq)]
pub struct CompensationAttemptAdmission {
    /// Current compensation attempt projection.
    pub attempt: CompensationAttemptRecord,
    /// Exact shared-capacity reservation released by settlement.
    pub capacity_reservation_uid: Uuid,
    /// Immutable compensation-attempt dispatch.
    pub dispatch: ExecutionDispatchRecord,
    /// Immutable watchdog trigger.
    pub watchdog: ExecutionTriggerWrite,
}

/// Result of atomically selecting and admitting the next reverse-order compensation.
#[derive(Clone, Debug, PartialEq)]
pub enum CompensationAttemptAdmissionOutcome {
    /// The highest unsettled registration entered Dispatching with capacity.
    Admitted(Box<CompensationAttemptAdmission>),
    /// The exact active slice was already admitted.
    Replayed(Box<CompensationAttemptAdmission>),
    /// Shared fleet or tenant capacity is currently exhausted.
    CapacityUnavailable {
        /// Earliest useful sparse admission retry.
        retry_at: DateTime<Utc>,
    },
    /// Every registered compensation is settled.
    Complete,
    /// No visible run exists.
    NotFound,
    /// The run, reverse-order registration, or state is not dispatchable.
    Conflict,
}

/// Result shared by exact compensation-attempt transitions.
#[derive(Clone, Debug, PartialEq)]
pub enum CompensationAttemptWriteOutcome {
    /// The transition changed canonical state.
    Applied(CompensationAttemptRecord),
    /// The exact transition had already been applied.
    Replayed(CompensationAttemptRecord),
    /// No visible run or registration exists.
    NotFound,
    /// A generation, dispatch, review, deadline, or state fence rejected the write.
    Conflict,
}

/// Result of claiming exact compensation teardown ownership before provider I/O.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum CompensationAttemptReleaseClaimOutcome {
    /// The active slice entered the non-dispatchable cancelling phase.
    Applied(CompensationAttemptRecord),
    /// The exact cancellation request already owns the cancelling phase.
    Replayed(CompensationAttemptRecord),
    /// No exact run or compensation registration exists.
    NotFound,
    /// An immutable controller, generation, dispatch, capacity, or watchdog fence differed.
    Stale,
    /// The compensation slice is not currently eligible to relinquish ownership.
    InvalidState,
}

/// Result of fencing a compensation after provider recovery proved no job started.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum CompensationExternalNotStartedReleaseClaimOutcome {
    /// The active slice entered Cancelling with a truthful retry release intent.
    Applied {
        /// Exact request carried through verified sandbox teardown and finalization.
        request: ExecutionCompensationAttemptCancelRequest,
        /// Current compensation projection after the phase-one fence.
        attempt: CompensationAttemptRecord,
    },
    /// The exact phase-one fence was already persisted.
    Replayed {
        /// Exact request carried through verified sandbox teardown and finalization.
        request: ExecutionCompensationAttemptCancelRequest,
        /// Current compensation projection after the phase-one fence.
        attempt: CompensationAttemptRecord,
    },
    /// A prior exact finalizer already returned the slice to Idle.
    AlreadySettled,
    /// No exact run or compensation registration exists.
    NotFound,
    /// An immutable owner, generation, capacity, or watchdog coordinate differed.
    Stale,
    /// The compensation cannot enter recovery teardown from its current state.
    InvalidState,
}

/// Result of adopting a recovered, already-started provider job into compensation teardown.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum CompensationRecoveredExternalReleaseClaimOutcome {
    /// The exact provider job and compensation teardown fence were attached atomically.
    Applied {
        /// Exact request carried through verified sandbox teardown and finalization.
        request: ExecutionCompensationAttemptCancelRequest,
        /// Current compensation projection after the phase-one fence.
        attempt: CompensationAttemptRecord,
    },
    /// The same exact recovered job was already attached to the cancelling slice.
    Replayed {
        /// Exact request carried through verified sandbox teardown and finalization.
        request: ExecutionCompensationAttemptCancelRequest,
        /// Current compensation projection after the phase-one fence.
        attempt: CompensationAttemptRecord,
    },
    /// The recovered job is already owned by a storage-only or terminal compensation state.
    AlreadySettled,
    /// No exact run, compensation, or provider job exists.
    NotFound,
    /// An immutable owner, generation, capacity, or watchdog coordinate differed.
    Stale,
    /// The compensation cannot enter recovery teardown from its current state.
    InvalidState,
}

/// Result of consuming one storage-only compensation action-review resolution.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum CompensationReviewResolutionOutcome {
    /// The exact current review resolution settled or requeued the compensation.
    Applied(CompensationAttemptRecord),
    /// The same semantic resolution had already been consumed.
    Replayed(CompensationAttemptRecord),
    /// The decision arrived before the active attempt completed its durable park.
    NotReady,
    /// The run or compensation registration does not exist.
    NotFound,
    /// The logical generation or review identity is obsolete.
    Stale,
}

/// Result of parking a released compensation attempt on asynchronous provider work.
#[derive(Clone, Debug, PartialEq)]
pub enum CompensationAttemptExternalOutcome {
    /// The exact provider job and storage-only wait committed atomically.
    Applied {
        /// Waiting or immediately settled compensation projection.
        attempt: CompensationAttemptRecord,
        /// Durable provider job.
        external_job: ExecutionExternalJobRecord,
    },
    /// The same exact provider job had already been attached.
    Replayed {
        /// Current compensation projection.
        attempt: CompensationAttemptRecord,
        /// Existing durable provider job.
        external_job: ExecutionExternalJobRecord,
    },
    /// No exact run, compensation, or capacity receipt exists.
    NotFound,
    /// An immutable attempt or external-job coordinate differed.
    Stale,
    /// The compensation cannot yield from its current state.
    InvalidState,
}

/// Result of consuming one terminal provider job into its exact compensation wait.
#[derive(Clone, Debug, PartialEq)]
pub enum CompensationExternalJobSettlementOutcome {
    /// The terminal provider result settled or requeued the compensation.
    Applied(CompensationAttemptRecord),
    /// The same terminal provider result had already been consumed.
    Replayed(CompensationAttemptRecord),
    /// The callback is durable, but active sandbox ownership must be released first.
    DeferredRelease(CompensationAttemptRecord),
    /// The provider job no longer owns the current compensation attempt.
    Stale,
    /// No owning compensation exists.
    NotFound,
}

impl ExecutionRepository {
    /// Admits the highest unsettled compensation into one bounded durable slice.
    pub async fn admit_next_compensation_attempt(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        run_uid: Uuid,
        now: DateTime<Utc>,
    ) -> Result<CompensationAttemptAdmissionOutcome> {
        let deadline = checked_attempt_deadline(config, now)?;
        let retry_at = checked_retry_at(config, now)?;
        let mut conn = scope.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::NotFound);
        };
        let visible_run = run_from_row(&run_row)?;
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            visible_run.tenant_id,
            &[
                ExecutionCapacityDimension::ActiveTasks,
                ExecutionCapacityDimension::ScheduledTriggers,
            ],
        )
        .await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        if run.tenant_id != visible_run.tenant_id
            || run.status != ExecutionRunStatus::Compensating
            || run.manual_repair_required
            || run.pending_terminal.is_none()
            || nonterminal_task_exists(&mut conn, run_uid).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::Conflict);
        }
        let Some(row) = sqlx::query(
            "SELECT * FROM moa.execution_compensation WHERE run_uid = $1 \
             AND status <> 'completed' ORDER BY registered_sequence DESC \
             LIMIT 1 FOR UPDATE",
        )
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::Complete);
        };
        let registration = compensation_from_row(&row)?;
        let attempt_state = compensation_attempt_state_from_row(&row)?;
        if matches!(
            registration.status,
            CompensationStatus::Failed | CompensationStatus::UnknownOutcome
        ) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::Conflict);
        }
        if attempt_state == CompensationAttemptState::Dispatching {
            let admission =
                load_existing_compensation_admission(&mut conn, config, &run, &row, &registration)
                    .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::Replayed(Box::new(
                admission,
            )));
        }
        if !compensation_capacity_available(&mut conn, visible_run.tenant_id).await? {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::CapacityUnavailable { retry_at });
        }
        if attempt_state != CompensationAttemptState::Idle
            || !matches!(
                registration.status,
                CompensationStatus::Pending | CompensationStatus::Running
            )
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::Conflict);
        }
        if registration.status == CompensationStatus::Pending && registration.outcome.is_none() {
            let forward_task =
                load_forward_task(&mut conn, run_uid, registration.forward_task_id).await?;
            let reservation =
                compensation_reservation(&run, &registration, forward_task.retry.max_attempts)?;
            let mut ledger = budget_ledger(&run);
            if ledger.try_reserve(reservation).is_err() {
                terminalize_compensation_budget_rejection(
                    &mut conn,
                    &run,
                    &registration,
                    reservation,
                )
                .await?;
                enqueue_current_compensation_controller_wake(
                    &mut conn,
                    &run,
                    json!({"reason": "compensation_budget_rejected"}),
                    now,
                )
                .await?;
                conn.commit().await.map_err(storage_error)?;
                return Ok(CompensationAttemptAdmissionOutcome::Conflict);
            }
            persist_run_budget(&mut conn, run_uid, &ledger, false).await?;
        }
        let attempt_generation = required_u64(&row, "attempt_generation")?;
        let dispatch_uid = Uuid::now_v7();
        let watchdog_uid = Uuid::now_v7();
        let reservation_uid = reserve_compensation_attempt_capacity(
            &mut conn,
            config,
            &run,
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
                &run,
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
        let dispatch_request = compensation_dispatch(&run, &attempt_request, now)?;
        let dispatch = enqueue_dispatch_in_conn(conn.as_mut(), &dispatch_request).await?;
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET status = 'running', \
             attempt_state = 'dispatching', attempt_started_at = $6, \
             last_progress_at = GREATEST(last_progress_at, $6), \
             attempt_deadline_at = $7, waiting_since = NULL, \
             active_dispatch_uid = $8, dispatch_sequence = dispatch_sequence + 1, \
             started_at = COALESCE(started_at, $6), updated_at = NOW() \
             WHERE run_uid = $1 AND compensation_id = $2 AND generation = $3 \
             AND attempt_generation = $4 AND attempt_state = 'idle' \
             AND status IN ('pending', 'running') AND EXISTS ( \
                 SELECT 1 FROM moa.execution_run AS run \
                 WHERE run.run_uid=$1 AND run.controller_generation=$5) \
             RETURNING *",
        )
        .bind(run_uid)
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
        .map_err(sqlx_error)?;
        let Some(updated) = updated else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(CompensationAttemptAdmissionOutcome::Conflict);
        };
        let record = compensation_attempt_from_row(&updated, &run)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationAttemptAdmissionOutcome::Admitted(Box::new(
            CompensationAttemptAdmission {
                attempt: record,
                capacity_reservation_uid: reservation_uid,
                dispatch,
                watchdog,
            },
        )))
    }

    /// Starts one exact immutable compensation slice dispatch.
    pub async fn start_compensation_attempt(
        &self,
        scope: ExecutionScope,
        fence: CompensationAttemptFence,
        now: DateTime<Utc>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        self.transition_active_compensation_attempt(scope, fence, now, true)
            .await
    }

    /// Records monotonic progress for one exact active compensation slice.
    pub async fn record_compensation_attempt_progress(
        &self,
        scope: ExecutionScope,
        fence: CompensationAttemptFence,
        observed_at: DateTime<Utc>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        self.transition_active_compensation_attempt(scope, fence, observed_at, false)
            .await
    }

    /// Claims one exact active compensation slice before sandbox checkpoint and release I/O.
    pub async fn begin_compensation_attempt_release(
        &self,
        request: &ExecutionCompensationAttemptCancelRequest,
        claimed_at: DateTime<Utc>,
    ) -> Result<CompensationAttemptReleaseClaimOutcome> {
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some((run, row)) = load_fenced_compensation_for_cancel(&mut conn, request).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptReleaseClaimOutcome::NotFound);
        };
        let current = compensation_attempt_from_row(&row, &run)?;
        if run.tenant_id != request.tenant_id
            || !compensation_attempt_resources_match(&mut conn, request).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptReleaseClaimOutcome::Stale);
        }
        if current.attempt_state == CompensationAttemptState::Cancelling {
            conn.commit().await.map_err(storage_error)?;
            return Ok(if current.release_intent == Some(request.intent) {
                CompensationAttemptReleaseClaimOutcome::Replayed(current)
            } else {
                CompensationAttemptReleaseClaimOutcome::Stale
            });
        }
        if !matches!(
            current.attempt_state,
            CompensationAttemptState::Dispatching | CompensationAttemptState::Running
        ) || current.registration.status != CompensationStatus::Running
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptReleaseClaimOutcome::InvalidState);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_compensation SET attempt_state='cancelling', \
                 release_intent=$7, last_progress_at=GREATEST(last_progress_at,$6), \
                 updated_at=NOW() \
             WHERE run_uid=$1 AND compensation_id=$2 \
                 AND generation=$3 AND attempt_generation=$4 AND active_dispatch_uid=$5 \
                AND attempt_state IN ('dispatching','running') RETURNING *",
        )
        .bind(request.run_uid)
        .bind(request.compensation_id.as_uuid())
        .bind(to_i64(
            request.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            request.compensation_attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(request.active_dispatch_uid)
        .bind(claimed_at)
        .bind(compensation_release_intent_label(request.intent))
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(CompensationAttemptReleaseClaimOutcome::Stale);
        };
        let record = compensation_attempt_from_row(&row, &run)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationAttemptReleaseClaimOutcome::Applied(record))
    }

    async fn transition_active_compensation_attempt(
        &self,
        scope: ExecutionScope,
        fence: CompensationAttemptFence,
        observed_at: DateTime<Utc>,
        start: bool,
    ) -> Result<CompensationAttemptWriteOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some((run, row)) = load_fenced_compensation(&mut conn, fence).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        let current = compensation_attempt_from_row(&row, &run)?;
        let expected = if start {
            CompensationAttemptState::Dispatching
        } else {
            CompensationAttemptState::Running
        };
        if current.attempt_state == CompensationAttemptState::Running
            && start
            && current.last_progress_at >= observed_at
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Replayed(current));
        }
        if current.attempt_state != expected
            || observed_at < current.last_progress_at
            || run.status != ExecutionRunStatus::Compensating
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET attempt_state = $7, \
             attempt_started_at = COALESCE(attempt_started_at, $6), \
             last_progress_at = $6, updated_at = $6 \
             WHERE run_uid = $1 AND compensation_id = $2 AND generation = $3 \
             AND attempt_generation = $4 AND active_dispatch_uid = $5 RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(fence.dispatch_uid)
        .bind(observed_at)
        .bind(CompensationAttemptState::Running.as_str())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let record = compensation_attempt_from_row(&updated, &run)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationAttemptWriteOutcome::Applied(record))
    }

    /// Finalizes one cancelling compensation only after exact sandbox release proof.
    pub async fn settle_released_compensation_attempt(
        &self,
        request: &ExecutionCompensationAttemptCancelRequest,
        outcome: ExecutionCompensationOutcome,
        now: DateTime<Utc>,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        validate_compensation_settlement_intent(request.intent, &outcome)?;
        if !compensation_release_receipt_matches(request, workspace_release_receipt.as_ref()) {
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        self.settle_compensation_attempt_from_state(
            ExecutionScope::ControlPlane,
            compensation_cancel_request_fence(request),
            outcome,
            now,
            CompensationAttemptState::Cancelling,
            Some(request),
            workspace_release_receipt.as_ref(),
        )
        .await
    }

    /// Releases one paused compensation slice back to idle after exact sandbox teardown proof.
    pub async fn yield_released_compensation_attempt(
        &self,
        request: &ExecutionCompensationAttemptCancelRequest,
        now: DateTime<Utc>,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        if request.intent != ExecutionCompensationReleaseIntent::Pause {
            return Err(Error::InvalidRepositoryInput {
                message: "released compensation yield requires the pause intent".to_string(),
            });
        }
        if !compensation_release_receipt_matches(request, workspace_release_receipt.as_ref()) {
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let fence = compensation_cancel_request_fence(request);
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some(tenant_id) = load_compensation_tenant(&mut conn, fence.run_uid).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            tenant_id,
            &[ExecutionCapacityDimension::ActiveTasks],
        )
        .await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(request.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
            .bind(request.run_uid)
            .bind(request.compensation_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        let current = compensation_attempt_from_row(&row, &run)?;
        if !persisted_compensation_release_receipt_matches(
            &mut conn,
            request,
            workspace_release_receipt.as_ref(),
        )
        .await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        if current.attempt_state == CompensationAttemptState::Idle
            && current.attempt_generation == fence.attempt_generation.saturating_add(1)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Replayed(current));
        }
        if run.tenant_id != request.tenant_id
            || current.attempt_state != CompensationAttemptState::Cancelling
            || current.release_intent != Some(ExecutionCompensationReleaseIntent::Pause)
            || !compensation_attempt_resources_match(&mut conn, request).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        release_unbound_compensation_external_intent_in_conn(&mut conn, &run, &current).await?;
        let resource_fence = compensation_cancel_resource_fence(request);
        release_compensation_attempt_capacity(&mut conn, &run, resource_fence).await?;
        supersede_compensation_triggers(&mut conn, resource_fence, None).await?;
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET attempt_state='idle', \
                 attempt_generation=attempt_generation+1, attempt_started_at=NULL, \
                 attempt_deadline_at=NULL, waiting_since=NULL, active_dispatch_uid=NULL, \
                 external_job_uid=NULL, release_intent=NULL, \
                 last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
             WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
               AND attempt_generation=$4 AND active_dispatch_uid=$5 \
               AND attempt_state='cancelling' AND release_intent='pause' RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(fence.dispatch_uid)
        .bind(now)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(updated) = updated else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        };
        let run = reconcile_run_after_compensation_capacity_release(&mut conn, &run, now).await?;
        if !matches!(
            run.status,
            ExecutionRunStatus::PauseRequested
                | ExecutionRunStatus::Pausing
                | ExecutionRunStatus::Paused
        ) {
            enqueue_current_compensation_controller_wake(
                &mut conn,
                &run,
                json!({"reason": "compensation_pause_released"}),
                now,
            )
            .await?;
        }
        let record = compensation_attempt_from_row(&updated, &run)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationAttemptWriteOutcome::Applied(record))
    }

    /// Requeues one compensation after recovery proved its provider job never started.
    pub async fn yield_released_compensation_attempt_after_external_not_started(
        &self,
        request: &ExecutionCompensationAttemptCancelRequest,
        now: DateTime<Utc>,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        if request.intent != ExecutionCompensationReleaseIntent::Retry {
            return Err(Error::InvalidRepositoryInput {
                message: "external NotStarted recovery requires the retry release intent"
                    .to_string(),
            });
        }
        if !compensation_release_receipt_matches(request, workspace_release_receipt.as_ref()) {
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let fence = compensation_cancel_request_fence(request);
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some(tenant_id) = load_compensation_tenant(&mut conn, fence.run_uid).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            tenant_id,
            &[ExecutionCapacityDimension::ActiveTasks],
        )
        .await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(request.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
            .bind(request.run_uid)
            .bind(request.compensation_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        let current = compensation_attempt_from_row(&row, &run)?;
        if !persisted_compensation_release_receipt_matches(
            &mut conn,
            request,
            workspace_release_receipt.as_ref(),
        )
        .await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        if current.attempt_state == CompensationAttemptState::Idle
            && current.attempt_generation == fence.attempt_generation.saturating_add(1)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Replayed(current));
        }
        if run.tenant_id != request.tenant_id
            || run.controller_generation != request.controller_generation
            || current.registration.generation != request.compensation_generation
            || current.attempt_generation != request.compensation_attempt_generation
            || current.active_dispatch_uid != Some(request.active_dispatch_uid)
            || current.attempt_state != CompensationAttemptState::Cancelling
            || current.release_intent != Some(ExecutionCompensationReleaseIntent::Retry)
            || current.external_job_uid.is_some()
            || !compensation_attempt_resources_match(&mut conn, request).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let resource_fence = compensation_cancel_resource_fence(request);
        release_compensation_attempt_capacity(&mut conn, &run, resource_fence).await?;
        supersede_compensation_triggers(&mut conn, resource_fence, None).await?;
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET attempt_state='idle', \
                 attempt_generation=attempt_generation+1, attempt_started_at=NULL, \
                 attempt_deadline_at=NULL, waiting_since=NULL, active_dispatch_uid=NULL, \
                 external_job_uid=NULL, release_intent=NULL, \
                 last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
             WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
               AND attempt_generation=$4 AND active_dispatch_uid=$5 \
               AND attempt_state='cancelling' AND release_intent='retry' \
               AND external_job_uid IS NULL RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(fence.dispatch_uid)
        .bind(now)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(updated) = updated else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        };
        enqueue_current_compensation_controller_wake(
            &mut conn,
            &run,
            json!({"reason": "compensation_external_not_started_released"}),
            now,
        )
        .await?;
        let record = compensation_attempt_from_row(&updated, &run)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationAttemptWriteOutcome::Applied(record))
    }

    /// Parks one cancelling slice after its sandbox ownership was durably released.
    pub async fn park_released_compensation_review(
        &self,
        request: &ExecutionCompensationAttemptCancelRequest,
        review_uid: Uuid,
        expires_at: DateTime<Utc>,
        now: DateTime<Utc>,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        if request.intent != ExecutionCompensationReleaseIntent::Review {
            return Err(Error::InvalidRepositoryInput {
                message: "compensation review park requires the review release intent".to_string(),
            });
        }
        if review_uid.is_nil() || expires_at <= now {
            return Err(Error::InvalidRepositoryInput {
                message: "compensation review requires a non-nil UID and future expiry".to_string(),
            });
        }
        if !compensation_release_receipt_matches(request, workspace_release_receipt.as_ref()) {
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let fence = compensation_cancel_request_fence(request);
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some(tenant_id) = load_compensation_tenant(&mut conn, fence.run_uid).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            tenant_id,
            &[ExecutionCapacityDimension::ActiveTasks],
        )
        .await?;
        let Some((run, row)) = load_fenced_compensation_for_cancel(&mut conn, request).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        let current = compensation_attempt_from_row(&row, &run)?;
        if !persisted_compensation_release_receipt_matches(
            &mut conn,
            request,
            workspace_release_receipt.as_ref(),
        )
        .await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        if run.tenant_id != request.tenant_id {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let mut persisted =
            persisted_compensation_outcome(&row, current.registration.outcome.clone())?;
        if current.attempt_state == CompensationAttemptState::WaitingReview {
            let exact_review = persisted.review_audit.iter().any(|entry| {
                entry.review_uid == review_uid
                    && entry.generation == fence.compensation_generation
                    && !entry.accepted
                    && entry.expires_at == Some(expires_at)
            });
            conn.commit().await.map_err(storage_error)?;
            return Ok(if exact_review {
                CompensationAttemptWriteOutcome::Replayed(current)
            } else {
                CompensationAttemptWriteOutcome::Conflict
            });
        }
        if current.attempt_state != CompensationAttemptState::Cancelling {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        if current.release_intent != Some(ExecutionCompensationReleaseIntent::Review)
            || !compensation_attempt_resources_match(&mut conn, request).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        release_unbound_compensation_external_intent_in_conn(&mut conn, &run, &current).await?;
        let resource_fence = compensation_cancel_resource_fence(request);
        release_compensation_attempt_capacity(&mut conn, &run, resource_fence).await?;
        supersede_compensation_triggers(&mut conn, resource_fence, None).await?;
        persisted.review_audit.push(CompensationReviewAuditEntry {
            review_uid,
            generation: fence.compensation_generation,
            accepted: false,
            resolution: None,
            expires_at: Some(expires_at),
            recorded_at: now,
        });
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET attempt_state = 'waiting_review', \
             attempt_deadline_at = NULL, waiting_since = $6, active_dispatch_uid = NULL, \
             release_intent=NULL, outcome=$7, \
             last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
             WHERE run_uid = $1 AND compensation_id = $2 AND generation = $3 \
             AND attempt_generation = $4 AND active_dispatch_uid = $5 RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(fence.dispatch_uid)
        .bind(now)
        .bind(serde_json::to_value(persisted)?)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let run = reconcile_run_after_compensation_capacity_release(&mut conn, &run, now).await?;
        let record = compensation_attempt_from_row(&updated, &run)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationAttemptWriteOutcome::Applied(record))
    }

    /// Commits one provider job before parking a released compensation attempt.
    pub async fn begin_compensation_external_release(
        &self,
        request: &ExecutionCompensationAttemptCancelRequest,
        external_job_uid: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<CompensationAttemptExternalOutcome> {
        if request.intent != ExecutionCompensationReleaseIntent::ExternalJob {
            return Err(Error::InvalidRepositoryInput {
                message: "external-job release requires the exact external-job intent".to_string(),
            });
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let outcome = begin_compensation_external_release_in_conn(
            &mut conn,
            request,
            external_job_uid,
            claimed_at,
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Finalizes a released compensation attempt into its already-durable provider wait.
    pub async fn yield_released_compensation_attempt_to_external_job(
        &self,
        request: &ExecutionCompensationAttemptCancelRequest,
        external_job_uid: Uuid,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
        yielded_at: DateTime<Utc>,
    ) -> Result<CompensationAttemptExternalOutcome> {
        if request.intent != ExecutionCompensationReleaseIntent::ExternalJob {
            return Err(Error::InvalidRepositoryInput {
                message: "external-job yield requires the exact external-job release intent"
                    .to_string(),
            });
        }
        if !compensation_release_receipt_matches(request, workspace_release_receipt.as_ref()) {
            return Ok(CompensationAttemptExternalOutcome::Stale);
        }
        let fence = compensation_cancel_request_fence(request);
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some(tenant_id) = load_compensation_tenant(&mut conn, fence.run_uid).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::NotFound);
        };
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            tenant_id,
            &[ExecutionCapacityDimension::ActiveTasks],
        )
        .await?;
        let Some((run, row)) = load_fenced_compensation_for_cancel(&mut conn, request).await?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::NotFound);
        };
        let current = compensation_attempt_from_row(&row, &run)?;
        if !persisted_compensation_release_receipt_matches(
            &mut conn,
            request,
            workspace_release_receipt.as_ref(),
        )
        .await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::Stale);
        }
        let Some(persisted_job) =
            load_external_job_for_update_in_conn(conn.as_mut(), external_job_uid).await?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::NotFound);
        };
        let expected_owner = ExecutionExternalJobOwner::Compensation {
            compensation_id: request.compensation_id.as_uuid(),
            compensation_generation: request.compensation_generation,
            compensation_attempt_generation: request.compensation_attempt_generation,
        };
        if run.tenant_id != request.tenant_id
            || persisted_job.tenant_id != request.tenant_id
            || persisted_job.run_uid != request.run_uid
            || persisted_job.owner != expected_owner
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::Stale);
        }
        if matches!(
            current.attempt_state,
            CompensationAttemptState::WaitingExternal
                | CompensationAttemptState::Terminal
                | CompensationAttemptState::UnknownOutcome
        ) && current.external_job_uid == Some(external_job_uid)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::Replayed {
                attempt: current,
                external_job: persisted_job,
            });
        }
        if current.attempt_state != CompensationAttemptState::Cancelling {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::InvalidState);
        }
        if !compensation_attempt_resources_match(&mut conn, request).await? {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::Stale);
        }
        let resource_fence = compensation_cancel_resource_fence(request);
        release_compensation_attempt_capacity(&mut conn, &run, resource_fence).await?;
        supersede_compensation_triggers(&mut conn, resource_fence, None).await?;
        let row = sqlx::query(
            "UPDATE moa.execution_compensation SET attempt_state='waiting_external', \
                 waiting_since=$6, external_job_uid=$7, active_dispatch_uid=NULL, \
                 attempt_deadline_at=NULL, release_intent=NULL, \
                 last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
             WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
               AND attempt_generation=$4 AND active_dispatch_uid=$5 \
               AND attempt_state='cancelling' RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(fence.dispatch_uid)
        .bind(yielded_at)
        .bind(external_job_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(CompensationAttemptExternalOutcome::Stale);
        };
        let mut attempt = compensation_attempt_from_row(&row, &run)?;
        if persisted_job.state.is_terminal() {
            attempt =
                match settle_external_job_terminal_in_conn(&mut conn, &persisted_job, yielded_at)
                    .await?
                {
                    CompensationExternalJobSettlementOutcome::Applied(attempt)
                    | CompensationExternalJobSettlementOutcome::Replayed(attempt) => attempt,
                    CompensationExternalJobSettlementOutcome::DeferredRelease(_) => {
                        conn.rollback().await.map_err(storage_error)?;
                        return Ok(CompensationAttemptExternalOutcome::Stale);
                    }
                    CompensationExternalJobSettlementOutcome::Stale => {
                        conn.rollback().await.map_err(storage_error)?;
                        return Ok(CompensationAttemptExternalOutcome::Stale);
                    }
                    CompensationExternalJobSettlementOutcome::NotFound => {
                        conn.rollback().await.map_err(storage_error)?;
                        return Ok(CompensationAttemptExternalOutcome::NotFound);
                    }
                };
        }
        if !persisted_job.state.is_terminal() {
            enqueue_run_activation_in_conn(
                conn.as_mut(),
                run.tenant_id,
                run.run_uid,
                run.controller_generation,
                yielded_at,
                json!({
                    "source": "compensation_external_job_started",
                    "compensation_id": fence.compensation_id,
                    "external_job_uid": external_job_uid,
                }),
            )
            .await?;
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationAttemptExternalOutcome::Applied {
            attempt,
            external_job: persisted_job,
        })
    }

    /// Resolves the current parked compensation review from its stable action-review owner.
    #[allow(clippy::too_many_arguments)]
    pub async fn resolve_current_compensation_review(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        compensation_id: CompensationId,
        logical_generation: u64,
        review_uid: Uuid,
        resolution: &ExecutionActionReviewResolution,
        now: DateTime<Utc>,
    ) -> Result<CompensationReviewResolutionOutcome> {
        if !matches!(scope, ExecutionScope::ControlPlane)
            || logical_generation == 0
            || review_uid.is_nil()
        {
            return Err(Error::InvalidRepositoryInput {
                message: "compensation review resolution requires control-plane scope and exact identities"
                    .to_string(),
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
            return Ok(CompensationReviewResolutionOutcome::NotFound);
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
            return Ok(CompensationReviewResolutionOutcome::NotFound);
        };
        let current = compensation_attempt_from_row(&row, &run)?;
        let persisted = persisted_compensation_outcome(&row, current.registration.outcome.clone())?;
        if let Some(existing) = persisted
            .review_audit
            .iter()
            .find(|entry| entry.review_uid == review_uid && entry.generation == logical_generation)
        {
            if existing.accepted && existing.resolution.as_ref() != Some(resolution) {
                return Err(Error::InvalidRepositoryData {
                    message: "compensation review UID replayed with different semantics"
                        .to_string(),
                });
            }
            if existing.accepted {
                conn.commit().await.map_err(storage_error)?;
                return Ok(CompensationReviewResolutionOutcome::Replayed(current));
            }
        }
        if current.registration.generation != logical_generation {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationReviewResolutionOutcome::Stale);
        }
        if matches!(
            current.attempt_state,
            CompensationAttemptState::Dispatching | CompensationAttemptState::Running
        ) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationReviewResolutionOutcome::NotReady);
        }
        if current.attempt_state != CompensationAttemptState::WaitingReview {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationReviewResolutionOutcome::Stale);
        }
        let parked_review = persisted.review_audit.iter().any(|entry| {
            entry.review_uid == review_uid
                && entry.generation == logical_generation
                && !entry.accepted
        });
        if !parked_review {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationReviewResolutionOutcome::Stale);
        }
        // Pausing advances the run controller generation, but the storage-only review remains
        // owned by the immutable attempt dispatch that originally entered the wait.
        let dispatch_owner: Option<(Uuid, i64)> = sqlx::query_as(
            "SELECT dispatch_uid,controller_generation FROM moa.execution_dispatch_outbox \
             WHERE run_uid=$1 AND compensation_id=$2 AND compensation_generation=$3 \
             AND compensation_attempt_generation=$4 AND dispatch_kind='compensation_attempt' \
             ORDER BY created_at DESC,dispatch_uid DESC LIMIT 1",
        )
        .bind(run_uid)
        .bind(compensation_id.as_uuid())
        .bind(to_i64(logical_generation, "compensation generation")?)
        .bind(to_i64(
            current.attempt_generation,
            "compensation attempt generation",
        )?)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some((dispatch_uid, attempt_controller_generation)) = dispatch_owner else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(CompensationReviewResolutionOutcome::Stale);
        };
        let fence = CompensationAttemptFence {
            run_uid,
            compensation_id,
            controller_generation: to_u64(
                attempt_controller_generation,
                "attempt controller generation",
            )?,
            compensation_generation: logical_generation,
            attempt_generation: current.attempt_generation,
            dispatch_uid,
        };
        if let ExecutionActionReviewResolution::ExternalJob {
            external_job_uid,
            job,
        } = resolution
        {
            let write = Self::settle_reviewed_compensation_external_job_in_conn(
                &mut conn,
                &run,
                &row,
                fence,
                review_uid,
                resolution,
                *external_job_uid,
                job,
                now,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return match write {
                CompensationAttemptWriteOutcome::Applied(record) => {
                    Ok(CompensationReviewResolutionOutcome::Applied(record))
                }
                CompensationAttemptWriteOutcome::Replayed(record) => {
                    Ok(CompensationReviewResolutionOutcome::Replayed(record))
                }
                CompensationAttemptWriteOutcome::NotFound => {
                    Ok(CompensationReviewResolutionOutcome::NotFound)
                }
                CompensationAttemptWriteOutcome::Conflict => {
                    Ok(CompensationReviewResolutionOutcome::Stale)
                }
            };
        }
        let outcome = compensation_outcome_from_review_resolution(resolution)?;
        let write = Self::settle_reviewed_compensation_in_conn(
            &mut conn, &run, &row, fence, review_uid, resolution, outcome, now,
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        match write {
            CompensationAttemptWriteOutcome::Applied(record) => {
                Ok(CompensationReviewResolutionOutcome::Applied(record))
            }
            CompensationAttemptWriteOutcome::Replayed(record) => {
                Ok(CompensationReviewResolutionOutcome::Replayed(record))
            }
            CompensationAttemptWriteOutcome::NotFound => {
                Ok(CompensationReviewResolutionOutcome::NotFound)
            }
            CompensationAttemptWriteOutcome::Conflict => {
                Ok(CompensationReviewResolutionOutcome::Stale)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle_compensation_attempt_from_state(
        &self,
        scope: ExecutionScope,
        fence: CompensationAttemptFence,
        outcome: ExecutionCompensationOutcome,
        now: DateTime<Utc>,
        expected_state: CompensationAttemptState,
        cancellation_request: Option<&ExecutionCompensationAttemptCancelRequest>,
        workspace_release_receipt: Option<&ExecutionHandReleaseReceipt>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let Some(tenant_id) = load_compensation_tenant(&mut conn, fence.run_uid).await? else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            tenant_id,
            &[ExecutionCapacityDimension::ActiveTasks],
        )
        .await?;
        let loaded = if let Some(request) = cancellation_request {
            load_fenced_compensation_for_cancel(&mut conn, request).await?
        } else {
            load_fenced_compensation(&mut conn, fence).await?
        };
        let Some((run, row)) = loaded else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::NotFound);
        };
        let current = compensation_attempt_from_row(&row, &run)?;
        if current.attempt_state != expected_state
            || cancellation_request.is_some_and(|request| run.tenant_id != request.tenant_id)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        if let Some(request) = cancellation_request
            && (current.release_intent != Some(request.intent)
                || !compensation_attempt_resources_match(&mut conn, request).await?
                || !persisted_compensation_release_receipt_matches(
                    &mut conn,
                    request,
                    workspace_release_receipt,
                )
                .await?)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let forward_task = load_forward_task(
            &mut conn,
            fence.run_uid,
            current.registration.forward_task_id,
        )
        .await?;
        let full_reservation =
            compensation_reservation(&run, &current.registration, forward_task.retry.max_attempts)?;
        let previous_usage = current
            .registration
            .outcome
            .as_ref()
            .map(ExecutionCompensationOutcome::usage)
            .cloned()
            .unwrap_or_else(zero_usage);
        let remaining = remaining_compensation_reservation(full_reservation, &previous_usage);
        let retry = matches!(
            outcome,
            ExecutionCompensationOutcome::Failed {
                retryable: true,
                ..
            }
        ) && current.registration.attempt < u64::from(forward_task.retry.max_attempts);
        let accepted_outcome = if retry {
            outcome
        } else {
            force_terminal_failure_if_exhausted(outcome)
        };
        let mut ledger = budget_ledger(&run);
        let reconciliation = ledger.reconcile_cumulative_with_ceiling(
            remaining,
            &previous_usage,
            accepted_outcome.usage(),
            !retry,
            i64::MAX as u64,
        )?;
        let (status, attempt_state, attempt, generation, next_attempt_generation, repair, error) =
            compensation_settlement_fields(&current, &accepted_outcome, retry)?;
        let persisted = persisted_compensation_outcome(&row, Some(accepted_outcome))?;
        release_unbound_compensation_external_intent_in_conn(&mut conn, &run, &current).await?;
        let resource_fence = cancellation_request
            .map(compensation_cancel_resource_fence)
            .unwrap_or(fence);
        release_compensation_attempt_capacity(&mut conn, &run, resource_fence).await?;
        supersede_compensation_triggers(&mut conn, resource_fence, None).await?;
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET status=$6, attempt_state=$7, attempt=$8, \
             generation=$9, attempt_generation=$10, outcome=$11, error=$12, \
             attempt_started_at=NULL, attempt_deadline_at=NULL, waiting_since=NULL, \
             active_dispatch_uid=NULL, release_intent=NULL, \
             last_progress_at=GREATEST(last_progress_at,$13), updated_at=NOW(), \
             completed_at=CASE WHEN $14 THEN $13 ELSE NULL END \
             WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
             AND attempt_generation=$4 AND active_dispatch_uid=$5 \
             AND attempt_state=$15 RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(fence.dispatch_uid)
        .bind(status.as_str())
        .bind(attempt_state.as_str())
        .bind(to_i64(attempt, "compensation attempt")?)
        .bind(to_i64(generation, "compensation generation")?)
        .bind(to_i64(
            next_attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(serde_json::to_value(persisted)?)
        .bind(error)
        .bind(now)
        .bind(status.is_settled())
        .bind(expected_state.as_str())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        persist_run_budget_and_repair(&mut conn, fence.run_uid, &reconciliation, repair).await?;
        enqueue_current_compensation_controller_wake(
            &mut conn,
            &run,
            json!({"reason": "compensation_settled"}),
            now,
        )
        .await?;
        let record = compensation_attempt_from_row(&updated, &run)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(CompensationAttemptWriteOutcome::Applied(record))
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle_reviewed_compensation_in_conn(
        conn: &mut ScopedConn<'_>,
        run: &ExecutionRunRecord,
        row: &PgRow,
        fence: CompensationAttemptFence,
        review_uid: Uuid,
        resolution: &ExecutionActionReviewResolution,
        outcome: ExecutionCompensationOutcome,
        now: DateTime<Utc>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        let current = compensation_attempt_from_row(row, run)?;
        let replay = persisted_compensation_outcome(row, current.registration.outcome.clone())?;
        if let Some(existing) = replay.review_audit.iter().find(|entry| {
            entry.review_uid == review_uid && entry.generation == fence.compensation_generation
        }) {
            if existing.accepted {
                if existing.resolution.as_ref() != Some(resolution)
                    || replay.result.as_ref() != Some(&outcome)
                {
                    return Err(Error::InvalidRepositoryData {
                        message: "compensation review UID replayed with different semantics"
                            .to_string(),
                    });
                }
                return Ok(CompensationAttemptWriteOutcome::Replayed(current));
            }
        } else {
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        if current.attempt_state != CompensationAttemptState::WaitingReview {
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let mut persisted = persisted_compensation_outcome(row, Some(outcome.clone()))?;
        let entry = persisted
            .review_audit
            .iter_mut()
            .find(|entry| {
                entry.review_uid == review_uid
                    && entry.generation == fence.compensation_generation
                    && !entry.accepted
            })
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "parked compensation review lost its persisted audit entry".to_string(),
            })?;
        entry.accepted = true;
        entry.resolution = Some(resolution.clone());
        entry.recorded_at = now;
        let forward_task =
            load_forward_task(conn, fence.run_uid, current.registration.forward_task_id).await?;
        let full_reservation =
            compensation_reservation(run, &current.registration, forward_task.retry.max_attempts)?;
        let previous_usage = current
            .registration
            .outcome
            .as_ref()
            .map(ExecutionCompensationOutcome::usage)
            .cloned()
            .unwrap_or_else(zero_usage);
        let remaining = remaining_compensation_reservation(full_reservation, &previous_usage);
        let retry = matches!(
            outcome,
            ExecutionCompensationOutcome::Failed {
                retryable: true,
                ..
            }
        ) && current.registration.attempt < u64::from(forward_task.retry.max_attempts);
        let accepted_outcome = if retry {
            outcome
        } else {
            force_terminal_failure_if_exhausted(outcome)
        };
        persisted.result = Some(accepted_outcome.clone());
        let mut ledger = budget_ledger(run);
        let reconciliation = ledger.reconcile_cumulative_with_ceiling(
            remaining,
            &previous_usage,
            accepted_outcome.usage(),
            !retry,
            i64::MAX as u64,
        )?;
        let (status, attempt_state, attempt, generation, next_attempt_generation, repair, error) =
            compensation_settlement_fields(&current, &accepted_outcome, retry)?;
        release_unbound_compensation_external_intent_in_conn(conn, run, &current).await?;
        supersede_compensation_triggers(conn, fence, None).await?;
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET status=$5, attempt_state=$6, attempt=$7, \
             generation=$8, attempt_generation=$9, outcome=$10, error=$11, \
             attempt_started_at=NULL, attempt_deadline_at=NULL, waiting_since=NULL, \
             active_dispatch_uid=NULL, \
             last_progress_at=GREATEST(last_progress_at,$12), updated_at=NOW(), \
             completed_at=CASE WHEN $13 THEN $12 ELSE NULL END \
             WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
             AND attempt_generation=$4 AND attempt_state='waiting_review' RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(status.as_str())
        .bind(attempt_state.as_str())
        .bind(to_i64(attempt, "compensation attempt")?)
        .bind(to_i64(generation, "compensation generation")?)
        .bind(to_i64(
            next_attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(serde_json::to_value(persisted)?)
        .bind(error)
        .bind(now)
        .bind(status.is_settled())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        persist_run_budget_and_repair(conn, fence.run_uid, &reconciliation, repair).await?;
        if !matches!(
            run.status,
            ExecutionRunStatus::PauseRequested
                | ExecutionRunStatus::Pausing
                | ExecutionRunStatus::Paused
        ) {
            enqueue_current_compensation_controller_wake(
                conn,
                run,
                json!({"reason": "compensation_review_resolved", "review_uid": review_uid}),
                now,
            )
            .await?;
        }
        let record = compensation_attempt_from_row(&updated, run)?;
        Ok(CompensationAttemptWriteOutcome::Applied(record))
    }

    #[allow(clippy::too_many_arguments)]
    async fn settle_reviewed_compensation_external_job_in_conn(
        conn: &mut ScopedConn<'_>,
        run: &ExecutionRunRecord,
        row: &PgRow,
        fence: CompensationAttemptFence,
        review_uid: Uuid,
        resolution: &ExecutionActionReviewResolution,
        external_job_uid: Uuid,
        job: &moa_core::types::tools::AsyncToolJob,
        now: DateTime<Utc>,
    ) -> Result<CompensationAttemptWriteOutcome> {
        let current = compensation_attempt_from_row(row, run)?;
        if current.attempt_state != CompensationAttemptState::WaitingReview {
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        }
        let mut persisted =
            persisted_compensation_outcome(row, current.registration.outcome.clone())?;
        let entry = persisted
            .review_audit
            .iter_mut()
            .find(|entry| {
                entry.review_uid == review_uid
                    && entry.generation == fence.compensation_generation
                    && !entry.accepted
            })
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "parked compensation review lost its persisted audit entry".to_string(),
            })?;
        entry.accepted = true;
        entry.resolution = Some(resolution.clone());
        entry.recorded_at = now;
        let bound_job_exists = sqlx::query_scalar::<_, bool>(
            "SELECT TRUE FROM moa.execution_external_job \
             WHERE external_job_uid=$1 AND tenant_id=$2 AND run_uid=$3 \
               AND compensation_id=$4 AND compensation_generation=$5 \
               AND compensation_attempt_generation=$6 AND idempotency_key=$7 \
               AND provider=$8 AND provider_job_id=$9 AND state <> 'unbound' FOR UPDATE",
        )
        .bind(external_job_uid)
        .bind(run.tenant_id.0)
        .bind(run.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(&job.idempotency_key)
        .bind(&job.provider)
        .bind(&job.provider_job_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        .unwrap_or(false);
        if !bound_job_exists {
            return Err(Error::InvalidRepositoryData {
                message:
                    "reviewed compensation external result requires its exact bound provider job"
                        .to_string(),
            });
        }
        let updated = sqlx::query(
            "UPDATE moa.execution_compensation SET attempt_state='waiting_external', \
                 external_job_uid=$5, waiting_since=$6, outcome=$7, \
                 last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
             WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
               AND attempt_generation=$4 AND attempt_state='waiting_review' \
               AND active_dispatch_uid IS NULL RETURNING *",
        )
        .bind(run.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .bind(to_i64(
            fence.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(external_job_uid)
        .bind(now)
        .bind(serde_json::to_value(persisted)?)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(updated) = updated else {
            return Ok(CompensationAttemptWriteOutcome::Conflict);
        };
        if !matches!(
            run.status,
            ExecutionRunStatus::PauseRequested
                | ExecutionRunStatus::Pausing
                | ExecutionRunStatus::Paused
        ) {
            enqueue_current_compensation_controller_wake(
                conn,
                run,
                json!({
                    "reason": "compensation_review_external_job_started",
                    "review_uid": review_uid,
                    "external_job_uid": external_job_uid,
                }),
                now,
            )
            .await?;
        }
        Ok(CompensationAttemptWriteOutcome::Applied(
            compensation_attempt_from_row(&updated, run)?,
        ))
    }
}

fn checked_attempt_deadline(config: &ExecutionConfig, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let seconds = i64::try_from(config.active_attempt_timeout_seconds).map_err(|_| {
        Error::InvalidRepositoryInput {
            message: "active compensation attempt timeout exceeds chrono duration".to_string(),
        }
    })?;
    now.checked_add_signed(Duration::seconds(seconds))
        .ok_or_else(|| Error::InvalidRepositoryInput {
            message: "active compensation attempt deadline is not representable".to_string(),
        })
}

fn checked_retry_at(config: &ExecutionConfig, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let seconds = i64::try_from(config.trigger_reconciliation_cadence_seconds).map_err(|_| {
        Error::InvalidRepositoryInput {
            message: "compensation admission retry cadence exceeds chrono duration".to_string(),
        }
    })?;
    now.checked_add_signed(Duration::seconds(seconds))
        .ok_or_else(|| Error::InvalidRepositoryInput {
            message: "compensation admission retry time is not representable".to_string(),
        })
}

/// Reports whether the locked `active_tasks` buckets can admit one compensation attempt.
async fn compensation_capacity_available(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
) -> Result<bool> {
    let dimension = ExecutionCapacityDimension::ActiveTasks.as_str();
    let fleet = capacity_bucket_has_room(conn.as_mut(), "fleet", None, dimension).await?;
    let tenant =
        capacity_bucket_has_room(conn.as_mut(), "tenant", Some(tenant_id.0), dimension).await?;
    Ok(fleet && tenant)
}

/// Reserves the exact active-task receipt for one compensation attempt and charges fairness.
///
/// Compensation shares the `active_tasks` dimension with forward attempts, so it also shares the
/// weighted-fair accounting: an admitted rollback advances the tenant's virtual finish exactly
/// like a forward dispatch instead of consuming fleet capacity outside the scheduler.
async fn reserve_compensation_attempt_capacity(
    conn: &mut ScopedConn<'_>,
    config: &ExecutionConfig,
    run: &ExecutionRunRecord,
    registration: &CompensationRegistrationProjection,
    attempt_generation: u64,
    deadline: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<Uuid> {
    let request = compensation_attempt_capacity_request(
        run.tenant_id,
        run.run_uid,
        run.controller_generation,
        registration.compensation_id.as_uuid(),
        registration.generation,
        attempt_generation,
        Some(deadline),
    );
    match reserve_capacity_in_tx(conn.as_mut(), config, request).await? {
        CapacityReserveOutcome::Reserved | CapacityReserveOutcome::Replayed => {}
        CapacityReserveOutcome::Saturated => {
            return Err(Error::InvalidRepositoryData {
                message: "locked active-task capacity rejected an admitted compensation attempt"
                    .to_string(),
            });
        }
    }
    advance_tenant_fairness(conn, run.tenant_id.0, now).await?;
    Ok(request.reservation_uid)
}

/// Releases the exact active-task receipt owned by one settled compensation attempt.
async fn release_compensation_attempt_capacity(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    fence: CompensationAttemptFence,
) -> Result<()> {
    let request = compensation_attempt_capacity_request(
        run.tenant_id,
        fence.run_uid,
        fence.controller_generation,
        fence.compensation_id.as_uuid(),
        fence.compensation_generation,
        fence.attempt_generation,
        None,
    );
    match release_capacity_in_tx(conn.as_mut(), request).await? {
        // A replayed settlement legitimately observes its own already-released receipt.
        CapacityReleaseOutcome::Released | CapacityReleaseOutcome::AlreadyReleased => Ok(()),
        CapacityReleaseOutcome::NotFound | CapacityReleaseOutcome::Stale => {
            Err(Error::InvalidRepositoryData {
                message: "active compensation slice lost its capacity reservation".to_string(),
            })
        }
    }
}

fn compensation_dispatch(
    run: &ExecutionRunRecord,
    request: &ExecutionCompensationAttemptRequest,
    now: DateTime<Utc>,
) -> Result<NewExecutionDispatch> {
    Ok(NewExecutionDispatch {
        dispatch_uid: request.dispatch_uid,
        tenant_id: run.tenant_id,
        run_uid: Some(run.run_uid),
        task_id: None,
        compensation_id: Some(request.compensation_id.as_uuid()),
        trigger_uid: None,
        external_job_uid: None,
        kind: ExecutionDispatchKind::CompensationAttempt,
        controller_generation: Some(run.controller_generation),
        wake_epoch: None,
        attempt_generation: None,
        compensation_generation: Some(request.compensation_generation),
        compensation_attempt_generation: Some(request.compensation_attempt_generation),
        not_before_at: now,
        payload: serde_json::to_value(request)?,
    })
}

fn compensation_trigger(
    run: &ExecutionRunRecord,
    registration: &CompensationRegistrationProjection,
    attempt_generation: u64,
    trigger_uid: Uuid,
    kind: ExecutionTriggerKind,
    due_at: DateTime<Utc>,
    payload: Value,
) -> NewExecutionTrigger {
    NewExecutionTrigger {
        trigger_uid,
        tenant_id: run.tenant_id,
        run_uid: Some(run.run_uid),
        task_id: None,
        compensation_id: Some(registration.compensation_id.as_uuid()),
        schedule_uid: None,
        kind,
        controller_generation: Some(run.controller_generation),
        attempt_generation: None,
        compensation_generation: Some(registration.generation),
        compensation_attempt_generation: Some(attempt_generation),
        occurrence_sequence: None,
        schedule_incarnation: None,
        due_at,
        payload,
    }
}

async fn load_existing_compensation_admission(
    conn: &mut ScopedConn<'_>,
    config: &ExecutionConfig,
    run: &ExecutionRunRecord,
    row: &PgRow,
    registration: &CompensationRegistrationProjection,
) -> Result<CompensationAttemptAdmission> {
    let attempt_generation = required_u64(row, "attempt_generation")?;
    let dispatch_uid: Uuid = row
        .try_get::<Option<Uuid>, _>("active_dispatch_uid")
        .map_err(row_error)?
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "dispatching compensation has no active dispatch UID".to_string(),
        })?;
    let reservation_uid: Uuid = sqlx::query_scalar(
        "SELECT reservation_uid FROM moa.execution_capacity_reservation \
         WHERE run_uid=$1 AND compensation_id=$2 AND controller_generation=$3 \
         AND compensation_generation=$4 AND compensation_attempt_generation=$5 \
         AND resource_dimension='active_tasks' AND state='reserved'",
    )
    .bind(run.run_uid)
    .bind(registration.compensation_id.as_uuid())
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(to_i64(registration.generation, "compensation generation")?)
    .bind(to_i64(
        attempt_generation,
        "compensation attempt generation",
    )?)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let deadline: DateTime<Utc> = row.try_get("attempt_deadline_at").map_err(row_error)?;
    let watchdog_row = sqlx::query(
        "SELECT trigger_uid, due_at, payload FROM moa.execution_trigger \
         WHERE run_uid=$1 AND compensation_id=$2 AND trigger_kind='compensation_watchdog' \
         AND controller_generation=$3 AND compensation_generation=$4 \
         AND compensation_attempt_generation=$5 AND state = 'pending'",
    )
    .bind(run.run_uid)
    .bind(registration.compensation_id.as_uuid())
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(to_i64(registration.generation, "compensation generation")?)
    .bind(to_i64(
        attempt_generation,
        "compensation attempt generation",
    )?)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let watchdog_uid: Uuid = watchdog_row.try_get("trigger_uid").map_err(row_error)?;
    let payload: Value = watchdog_row.try_get("payload").map_err(row_error)?;
    let dispatch_row = sqlx::query(
        "SELECT not_before_at, payload FROM moa.execution_dispatch_outbox \
         WHERE dispatch_uid=$1",
    )
    .bind(dispatch_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let dispatch_payload: Value = dispatch_row.try_get("payload").map_err(row_error)?;
    let attempt_request: ExecutionCompensationAttemptRequest =
        serde_json::from_value(dispatch_payload)?;
    let dispatch_request = compensation_dispatch(
        run,
        &attempt_request,
        dispatch_row.try_get("not_before_at").map_err(row_error)?,
    )?;
    let dispatch = enqueue_dispatch_in_conn(conn.as_mut(), &dispatch_request).await?;
    let watchdog = create_trigger_with_dispatch_in_conn(
        conn.as_mut(),
        config,
        &compensation_trigger(
            run,
            registration,
            attempt_generation,
            watchdog_uid,
            ExecutionTriggerKind::CompensationWatchdog,
            deadline,
            payload,
        ),
    )
    .await?;
    Ok(CompensationAttemptAdmission {
        attempt: compensation_attempt_from_row(row, run)?,
        capacity_reservation_uid: reservation_uid,
        dispatch,
        watchdog,
    })
}

fn compensation_attempt_state_from_row(row: &PgRow) -> Result<CompensationAttemptState> {
    row.try_get::<String, _>("attempt_state")
        .map_err(row_error)?
        .parse()
}

fn compensation_release_intent_from_row(
    row: &PgRow,
) -> Result<Option<ExecutionCompensationReleaseIntent>> {
    row.try_get::<Option<String>, _>("release_intent")
        .map_err(row_error)?
        .map(|label| match label.as_str() {
            "outcome" => Ok(ExecutionCompensationReleaseIntent::Outcome),
            "retry" => Ok(ExecutionCompensationReleaseIntent::Retry),
            "review" => Ok(ExecutionCompensationReleaseIntent::Review),
            "external_job" => Ok(ExecutionCompensationReleaseIntent::ExternalJob),
            "pause" => Ok(ExecutionCompensationReleaseIntent::Pause),
            "watchdog" => Ok(ExecutionCompensationReleaseIntent::Watchdog),
            "deadline" => Ok(ExecutionCompensationReleaseIntent::Deadline),
            "run_terminal" => Ok(ExecutionCompensationReleaseIntent::RunTerminal),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown compensation release intent `{label}`"),
            }),
        })
        .transpose()
}

fn compensation_cancel_request_fence(
    request: &ExecutionCompensationAttemptCancelRequest,
) -> CompensationAttemptFence {
    CompensationAttemptFence {
        run_uid: request.run_uid,
        compensation_id: request.compensation_id,
        controller_generation: request.controller_generation,
        compensation_generation: request.compensation_generation,
        attempt_generation: request.compensation_attempt_generation,
        dispatch_uid: request.active_dispatch_uid,
    }
}

fn compensation_cancel_resource_fence(
    request: &ExecutionCompensationAttemptCancelRequest,
) -> CompensationAttemptFence {
    CompensationAttemptFence {
        run_uid: request.run_uid,
        compensation_id: request.compensation_id,
        controller_generation: request.attempt_controller_generation,
        compensation_generation: request.compensation_generation,
        attempt_generation: request.compensation_attempt_generation,
        dispatch_uid: request.active_dispatch_uid,
    }
}

fn compensation_release_receipt_matches(
    request: &ExecutionCompensationAttemptCancelRequest,
    receipt: Option<&ExecutionHandReleaseReceipt>,
) -> bool {
    receipt.is_some_and(|receipt| {
        receipt.tenant_id == request.tenant_id
            && receipt.run_id.0 == request.run_uid
            && matches!(
                receipt.owner,
                ExecutionHandReleaseOwner::Compensation {
                    compensation_id,
                    logical_generation,
                } if compensation_id.0 == request.compensation_id.as_uuid()
                    && logical_generation == request.compensation_generation
            )
            && receipt.attempt_generation == request.compensation_attempt_generation
    })
}

async fn persisted_compensation_release_receipt_matches(
    conn: &mut ScopedConn<'_>,
    request: &ExecutionCompensationAttemptCancelRequest,
    receipt: Option<&ExecutionHandReleaseReceipt>,
) -> Result<bool> {
    let Some(receipt) = receipt else {
        return Ok(false);
    };
    if !compensation_release_receipt_matches(request, Some(receipt)) {
        return Ok(false);
    }
    let matches: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.sandbox_execution_hand_release_receipts \
         WHERE receipt_id=$1 AND tenant_id=$2 AND run_uid=$3 \
           AND owner_kind='compensation' AND task_id IS NULL AND compensation_id=$4 \
           AND logical_generation=$5 AND attempt_generation=$6 \
           AND receipt_state='released' AND destroy_outcome='verified_absent' \
           AND released_at IS NOT NULL \
           AND hand_provisioning_operation_id IS NOT DISTINCT FROM $7 \
           AND hand_lease_generation IS NOT DISTINCT FROM $8)",
    )
    .bind(receipt.receipt_id)
    .bind(request.tenant_id.0)
    .bind(request.run_uid)
    .bind(request.compensation_id.as_uuid())
    .bind(to_i64(
        request.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        request.compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(
        receipt
            .hand_provisioning_operation_id
            .map(|operation_id| operation_id.0),
    )
    .bind(
        receipt
            .hand_lease_generation
            .map(|generation| to_i64(generation, "hand lease generation"))
            .transpose()?,
    )
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(matches)
}

async fn compensation_attempt_resources_match(
    conn: &mut ScopedConn<'_>,
    request: &ExecutionCompensationAttemptCancelRequest,
) -> Result<bool> {
    let matches: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation AS reservation \
         WHERE reservation.reservation_uid=$1 AND reservation.tenant_id=$2 \
           AND reservation.run_uid=$3 AND reservation.compensation_id=$4 \
           AND reservation.controller_generation=$5 \
           AND reservation.compensation_generation=$6 \
           AND reservation.compensation_attempt_generation=$7 \
           AND reservation.resource_dimension='active_tasks' \
           AND reservation.state IN ('reserved','reconciling')) \
         AND EXISTS (SELECT 1 FROM moa.execution_trigger AS trigger \
         WHERE trigger.trigger_uid=$8 AND trigger.tenant_id=$2 AND trigger.run_uid=$3 \
           AND trigger.compensation_id=$4 AND trigger.controller_generation=$5 \
           AND trigger.compensation_generation=$6 \
           AND trigger.compensation_attempt_generation=$7 \
           AND trigger.trigger_kind='compensation_watchdog' \
           AND trigger.state = 'pending')",
    )
    .bind(request.capacity_reservation_uid)
    .bind(request.tenant_id.0)
    .bind(request.run_uid)
    .bind(request.compensation_id.as_uuid())
    .bind(to_i64(
        request.attempt_controller_generation,
        "attempt controller generation",
    )?)
    .bind(to_i64(
        request.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        request.compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(request.watchdog_trigger_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(matches)
}

async fn canonical_active_compensation_release_request(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    attempt: &CompensationAttemptRecord,
    intent: ExecutionCompensationReleaseIntent,
) -> Result<Option<ExecutionCompensationAttemptCancelRequest>> {
    let Some(active_dispatch_uid) = attempt.active_dispatch_uid else {
        return Ok(None);
    };
    let resource = sqlx::query(
        "SELECT reservation.reservation_uid, trigger.trigger_uid \
         FROM moa.execution_capacity_reservation AS reservation \
         JOIN moa.execution_trigger AS trigger ON trigger.run_uid=reservation.run_uid \
          AND trigger.compensation_id=reservation.compensation_id \
          AND trigger.controller_generation=reservation.controller_generation \
          AND trigger.compensation_generation=reservation.compensation_generation \
          AND trigger.compensation_attempt_generation=reservation.compensation_attempt_generation \
          AND trigger.trigger_kind='compensation_watchdog' \
          AND trigger.state = 'pending' \
         WHERE reservation.tenant_id=$1 AND reservation.run_uid=$2 \
          AND reservation.compensation_id=$3 AND reservation.controller_generation=$4 \
          AND reservation.compensation_generation=$5 \
          AND reservation.compensation_attempt_generation=$6 \
          AND reservation.resource_dimension='active_tasks' \
          AND reservation.state IN ('reserved','reconciling') \
         FOR UPDATE OF reservation, trigger",
    )
    .bind(run.tenant_id.0)
    .bind(run.run_uid)
    .bind(attempt.registration.compensation_id.as_uuid())
    .bind(to_i64(
        attempt.controller_generation,
        "attempt controller generation",
    )?)
    .bind(to_i64(
        attempt.registration.generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        attempt.attempt_generation,
        "compensation attempt generation",
    )?)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(resource) = resource else {
        return Ok(None);
    };
    Ok(Some(ExecutionCompensationAttemptCancelRequest {
        cancellation_dispatch_uid: Uuid::new_v5(
            &active_dispatch_uid,
            b"compensation-attempt-release-v1",
        ),
        tenant_id: run.tenant_id,
        run_uid: run.run_uid,
        compensation_id: attempt.registration.compensation_id,
        controller_generation: run.controller_generation,
        attempt_controller_generation: attempt.controller_generation,
        compensation_generation: attempt.registration.generation,
        compensation_attempt_generation: attempt.attempt_generation,
        active_dispatch_uid,
        capacity_reservation_uid: resource.try_get("reservation_uid").map_err(row_error)?,
        watchdog_trigger_uid: resource.try_get("trigger_uid").map_err(row_error)?,
        intent,
    }))
}

async fn release_unbound_compensation_external_intent_in_conn(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    attempt: &CompensationAttemptRecord,
) -> Result<()> {
    let rows = sqlx::query(
        "SELECT job.external_job_uid, job.job_generation, job.provider, job.idempotency_key, \
                capacity.expires_at \
         FROM moa.execution_external_job AS job \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.tenant_id=job.tenant_id \
          AND capacity.external_job_uid=job.external_job_uid \
          AND capacity.resource_dimension='external_jobs' \
          AND capacity.state IN ('reserved','reconciling') \
         WHERE job.tenant_id=$1 AND job.run_uid=$2 AND job.compensation_id=$3 \
           AND job.compensation_generation=$4 \
           AND job.compensation_attempt_generation=$5 AND job.state='unbound' \
         ORDER BY job.external_job_uid LIMIT 2 FOR UPDATE OF job, capacity",
    )
    .bind(run.tenant_id.0)
    .bind(run.run_uid)
    .bind(attempt.registration.compensation_id.as_uuid())
    .bind(to_i64(
        attempt.registration.generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        attempt.attempt_generation,
        "compensation attempt generation",
    )?)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if rows.len() > 1 {
        return Err(Error::InvalidRepositoryData {
            message: "compensation attempt owns multiple unbound external-job intents".to_string(),
        });
    }
    let Some(row) = rows.first() else {
        return Ok(());
    };
    let intent = NewExecutionExternalJobIntent {
        external_job_uid: row.try_get("external_job_uid").map_err(row_error)?,
        tenant_id: run.tenant_id,
        run_uid: run.run_uid,
        owner: ExecutionExternalJobOwner::Compensation {
            compensation_id: attempt.registration.compensation_id.as_uuid(),
            compensation_generation: attempt.registration.generation,
            compensation_attempt_generation: attempt.attempt_generation,
        },
        job_generation: required_u64(row, "job_generation")?,
        provider: row.try_get("provider").map_err(row_error)?,
        idempotency_key: row.try_get("idempotency_key").map_err(row_error)?,
        expires_at: row.try_get("expires_at").map_err(row_error)?,
    };
    match release_external_job_intent_in_conn(conn.as_mut(), &intent).await? {
        ExecutionExternalJobIntentReleaseOutcome::Released
        | ExecutionExternalJobIntentReleaseOutcome::AlreadyReleased => Ok(()),
        ExecutionExternalJobIntentReleaseOutcome::Stale
        | ExecutionExternalJobIntentReleaseOutcome::AlreadyBound => {
            Err(Error::InvalidRepositoryData {
                message:
                    "compensation finalizer found a stale or already-bound external-job intent"
                        .to_string(),
            })
        }
    }
}

fn compensation_attempt_from_row(
    row: &PgRow,
    run: &ExecutionRunRecord,
) -> Result<CompensationAttemptRecord> {
    Ok(CompensationAttemptRecord {
        registration: compensation_from_row(row)?,
        run: run.clone(),
        controller_generation: run.controller_generation,
        attempt_generation: required_u64(row, "attempt_generation")?,
        attempt_state: compensation_attempt_state_from_row(row)?,
        attempt_started_at: row.try_get("attempt_started_at").map_err(row_error)?,
        last_progress_at: row.try_get("last_progress_at").map_err(row_error)?,
        attempt_deadline_at: row.try_get("attempt_deadline_at").map_err(row_error)?,
        waiting_since: row.try_get("waiting_since").map_err(row_error)?,
        active_dispatch_uid: row.try_get("active_dispatch_uid").map_err(row_error)?,
        external_job_uid: row.try_get("external_job_uid").map_err(row_error)?,
        release_intent: compensation_release_intent_from_row(row)?,
        dispatch_sequence: required_u64(row, "dispatch_sequence")?,
    })
}

async fn load_compensation_tenant(
    conn: &mut ScopedConn<'_>,
    run_uid: Uuid,
) -> Result<Option<TenantId>> {
    sqlx::query_scalar::<_, Uuid>("SELECT tenant_id FROM moa.execution_run WHERE run_uid=$1")
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)
        .map(|tenant| tenant.map(TenantId))
}

async fn load_fenced_compensation(
    conn: &mut ScopedConn<'_>,
    fence: CompensationAttemptFence,
) -> Result<Option<(ExecutionRunRecord, PgRow)>> {
    load_fenced_compensation_with_attempt_controller(conn, fence, fence.controller_generation).await
}

async fn load_fenced_compensation_for_cancel(
    conn: &mut ScopedConn<'_>,
    request: &ExecutionCompensationAttemptCancelRequest,
) -> Result<Option<(ExecutionRunRecord, PgRow)>> {
    load_fenced_compensation_with_attempt_controller(
        conn,
        compensation_cancel_request_fence(request),
        request.attempt_controller_generation,
    )
    .await
}

/// Atomically adopts one already-bound provider job into exact compensation teardown.
pub(super) async fn begin_compensation_external_release_in_conn(
    conn: &mut ScopedConn<'_>,
    request: &ExecutionCompensationAttemptCancelRequest,
    external_job_uid: Uuid,
    claimed_at: DateTime<Utc>,
) -> Result<CompensationAttemptExternalOutcome> {
    if request.intent != ExecutionCompensationReleaseIntent::ExternalJob {
        return Err(Error::InvalidRepositoryInput {
            message: "external-job release requires the exact external-job intent".to_string(),
        });
    }
    let Some((run, row)) = load_fenced_compensation_for_cancel(conn, request).await? else {
        return Ok(CompensationAttemptExternalOutcome::NotFound);
    };
    let current = compensation_attempt_from_row(&row, &run)?;
    let Some(persisted_job) =
        load_external_job_for_update_in_conn(conn.as_mut(), external_job_uid).await?
    else {
        return Ok(CompensationAttemptExternalOutcome::NotFound);
    };
    let expected_owner = ExecutionExternalJobOwner::Compensation {
        compensation_id: request.compensation_id.as_uuid(),
        compensation_generation: request.compensation_generation,
        compensation_attempt_generation: request.compensation_attempt_generation,
    };
    if run.tenant_id != request.tenant_id
        || persisted_job.tenant_id != request.tenant_id
        || persisted_job.run_uid != request.run_uid
        || persisted_job.owner != expected_owner
        || persisted_job.state == ExecutionExternalJobState::Unbound
    {
        return Ok(CompensationAttemptExternalOutcome::Stale);
    }
    if matches!(
        current.attempt_state,
        CompensationAttemptState::Cancelling
            | CompensationAttemptState::WaitingExternal
            | CompensationAttemptState::Terminal
            | CompensationAttemptState::UnknownOutcome
    ) && current.external_job_uid == Some(external_job_uid)
        && (current.attempt_state != CompensationAttemptState::Cancelling
            || current.release_intent == Some(ExecutionCompensationReleaseIntent::ExternalJob))
    {
        return Ok(CompensationAttemptExternalOutcome::Replayed {
            attempt: current,
            external_job: persisted_job,
        });
    }
    if !compensation_attempt_resources_match(conn, request).await? {
        return Ok(CompensationAttemptExternalOutcome::Stale);
    }
    if current.attempt_state != CompensationAttemptState::Running
        || current.registration.status != CompensationStatus::Running
    {
        return Ok(CompensationAttemptExternalOutcome::InvalidState);
    }
    let row = sqlx::query(
        "UPDATE moa.execution_compensation SET attempt_state='cancelling', \
             external_job_uid=$6, release_intent=$8, \
             last_progress_at=GREATEST(last_progress_at,$7), updated_at=NOW() \
         WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
           AND attempt_generation=$4 AND active_dispatch_uid=$5 \
           AND attempt_state='running' RETURNING *",
    )
    .bind(request.run_uid)
    .bind(request.compensation_id.as_uuid())
    .bind(to_i64(
        request.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        request.compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(request.active_dispatch_uid)
    .bind(external_job_uid)
    .bind(claimed_at)
    .bind(compensation_release_intent_label(request.intent))
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(CompensationAttemptExternalOutcome::Stale);
    };
    Ok(CompensationAttemptExternalOutcome::Applied {
        attempt: compensation_attempt_from_row(&row, &run)?,
        external_job: persisted_job,
    })
}

/// Adopts one recovered, already-bound provider job using canonical attempt resources.
pub(super) async fn begin_recovered_compensation_external_release_in_conn(
    conn: &mut ScopedConn<'_>,
    job: &ExecutionExternalJobRecord,
    claimed_at: DateTime<Utc>,
) -> Result<CompensationRecoveredExternalReleaseClaimOutcome> {
    let Some(persisted_job) =
        load_external_job_for_update_in_conn(conn.as_mut(), job.external_job_uid).await?
    else {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::NotFound);
    };
    if persisted_job != *job {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::Stale);
    }
    let ExecutionExternalJobOwner::Compensation {
        compensation_id,
        compensation_generation,
        compensation_attempt_generation,
    } = persisted_job.owner
    else {
        return Err(Error::InvalidRepositoryInput {
            message: "recovered compensation external release requires a compensation owner"
                .to_string(),
        });
    };
    if persisted_job.state == ExecutionExternalJobState::Unbound {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::InvalidState);
    }
    let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(persisted_job.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::NotFound);
    };
    let run = run_from_row(&run_row)?;
    let Some(row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
        .bind(persisted_job.run_uid)
        .bind(compensation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::NotFound);
    };
    let current = compensation_attempt_from_row(&row, &run)?;
    if run.tenant_id != persisted_job.tenant_id
        || current.registration.generation != compensation_generation
        || current.attempt_generation != compensation_attempt_generation
    {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::Stale);
    }
    if matches!(
        current.attempt_state,
        CompensationAttemptState::WaitingExternal
            | CompensationAttemptState::Terminal
            | CompensationAttemptState::UnknownOutcome
    ) && current.external_job_uid == Some(persisted_job.external_job_uid)
    {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::AlreadySettled);
    }
    let Some(request) = canonical_active_compensation_release_request(
        conn,
        &run,
        &current,
        ExecutionCompensationReleaseIntent::ExternalJob,
    )
    .await?
    else {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::Stale);
    };
    if current.attempt_state == CompensationAttemptState::Cancelling {
        return Ok(
            if current.release_intent == Some(ExecutionCompensationReleaseIntent::ExternalJob)
                && current.external_job_uid == Some(persisted_job.external_job_uid)
            {
                CompensationRecoveredExternalReleaseClaimOutcome::Replayed {
                    request,
                    attempt: current,
                }
            } else {
                CompensationRecoveredExternalReleaseClaimOutcome::Stale
            },
        );
    }
    if current.attempt_state != CompensationAttemptState::Running
        || current.registration.status != CompensationStatus::Running
        || current.external_job_uid.is_some()
    {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::InvalidState);
    }
    let updated = sqlx::query(
        "UPDATE moa.execution_compensation SET attempt_state='cancelling', \
             external_job_uid=$6, release_intent='external_job', \
             last_progress_at=GREATEST(last_progress_at,$7), updated_at=NOW() \
         WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
           AND attempt_generation=$4 AND active_dispatch_uid=$5 \
           AND attempt_state='running' AND external_job_uid IS NULL RETURNING *",
    )
    .bind(persisted_job.run_uid)
    .bind(compensation_id)
    .bind(to_i64(compensation_generation, "compensation generation")?)
    .bind(to_i64(
        compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(request.active_dispatch_uid)
    .bind(persisted_job.external_job_uid)
    .bind(claimed_at)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(updated) = updated else {
        return Ok(CompensationRecoveredExternalReleaseClaimOutcome::Stale);
    };
    Ok(CompensationRecoveredExternalReleaseClaimOutcome::Applied {
        request,
        attempt: compensation_attempt_from_row(&updated, &run)?,
    })
}

/// Fences one compensation for verified teardown after start recovery proved NotStarted.
///
/// The caller must release the exact unbound external-job intent in the same transaction
/// before invoking this helper. That ordering proves there is no bound provider owner while
/// keeping intent release and compensation teardown fencing atomic.
pub(super) async fn begin_compensation_external_not_started_release_in_conn(
    conn: &mut ScopedConn<'_>,
    intent: &NewExecutionExternalJobIntent,
    claimed_at: DateTime<Utc>,
) -> Result<CompensationExternalNotStartedReleaseClaimOutcome> {
    let ExecutionExternalJobOwner::Compensation {
        compensation_id,
        compensation_generation,
        compensation_attempt_generation,
    } = intent.owner
    else {
        return Err(Error::InvalidRepositoryInput {
            message: "compensation NotStarted recovery requires a compensation owner".to_string(),
        });
    };
    let provider_owner_remains: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_external_job WHERE external_job_uid=$1) \
         OR EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
           WHERE external_job_uid=$1 AND resource_dimension='external_jobs' \
             AND state IN ('reserved','reconciling'))",
    )
    .bind(intent.external_job_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if provider_owner_remains {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::Stale);
    }
    let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(intent.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::NotFound);
    };
    let run = run_from_row(&run_row)?;
    let Some(row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
        .bind(intent.run_uid)
        .bind(compensation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::NotFound);
    };
    let current = compensation_attempt_from_row(&row, &run)?;
    if run.tenant_id != intent.tenant_id
        || current.registration.generation != compensation_generation
    {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::Stale);
    }
    if current.attempt_state == CompensationAttemptState::Idle
        && current.attempt_generation == compensation_attempt_generation.saturating_add(1)
    {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::AlreadySettled);
    }
    if current.attempt_generation != compensation_attempt_generation {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::Stale);
    }
    let Some(active_dispatch_uid) = current.active_dispatch_uid else {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::InvalidState);
    };
    let resource = sqlx::query(
        "SELECT reservation.reservation_uid, trigger.trigger_uid \
         FROM moa.execution_capacity_reservation AS reservation \
         JOIN moa.execution_trigger AS trigger ON trigger.run_uid=reservation.run_uid \
          AND trigger.compensation_id=reservation.compensation_id \
          AND trigger.controller_generation=reservation.controller_generation \
          AND trigger.compensation_generation=reservation.compensation_generation \
          AND trigger.compensation_attempt_generation=reservation.compensation_attempt_generation \
          AND trigger.trigger_kind='compensation_watchdog' \
          AND trigger.state = 'pending' \
         WHERE reservation.tenant_id=$1 AND reservation.run_uid=$2 \
          AND reservation.compensation_id=$3 AND reservation.controller_generation=$4 \
          AND reservation.compensation_generation=$5 \
          AND reservation.compensation_attempt_generation=$6 \
          AND reservation.resource_dimension='active_tasks' \
          AND reservation.state IN ('reserved','reconciling') \
         FOR UPDATE OF reservation, trigger",
    )
    .bind(intent.tenant_id.0)
    .bind(intent.run_uid)
    .bind(compensation_id)
    .bind(to_i64(
        current.controller_generation,
        "attempt controller generation",
    )?)
    .bind(to_i64(compensation_generation, "compensation generation")?)
    .bind(to_i64(
        compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(resource) = resource else {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::Stale);
    };
    let request = ExecutionCompensationAttemptCancelRequest {
        cancellation_dispatch_uid: Uuid::new_v5(
            &active_dispatch_uid,
            b"compensation-attempt-release-v1",
        ),
        tenant_id: intent.tenant_id,
        run_uid: intent.run_uid,
        compensation_id: CompensationId::from_uuid(compensation_id),
        controller_generation: run.controller_generation,
        attempt_controller_generation: current.controller_generation,
        compensation_generation,
        compensation_attempt_generation,
        active_dispatch_uid,
        capacity_reservation_uid: resource.try_get("reservation_uid").map_err(row_error)?,
        watchdog_trigger_uid: resource.try_get("trigger_uid").map_err(row_error)?,
        intent: ExecutionCompensationReleaseIntent::Retry,
    };
    if current.attempt_state == CompensationAttemptState::Cancelling {
        return Ok(
            if current.release_intent == Some(ExecutionCompensationReleaseIntent::Retry)
                && current.external_job_uid.is_none()
            {
                CompensationExternalNotStartedReleaseClaimOutcome::Replayed {
                    request,
                    attempt: current,
                }
            } else {
                CompensationExternalNotStartedReleaseClaimOutcome::Stale
            },
        );
    }
    if current.attempt_state != CompensationAttemptState::Running
        || current.registration.status != CompensationStatus::Running
        || current.external_job_uid.is_some()
    {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::InvalidState);
    }
    let updated = sqlx::query(
        "UPDATE moa.execution_compensation SET attempt_state='cancelling', \
             release_intent='retry', last_progress_at=GREATEST(last_progress_at,$6), \
             updated_at=NOW() \
         WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
           AND attempt_generation=$4 AND active_dispatch_uid=$5 \
           AND attempt_state='running' AND external_job_uid IS NULL RETURNING *",
    )
    .bind(intent.run_uid)
    .bind(compensation_id)
    .bind(to_i64(compensation_generation, "compensation generation")?)
    .bind(to_i64(
        compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(active_dispatch_uid)
    .bind(claimed_at)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(updated) = updated else {
        return Ok(CompensationExternalNotStartedReleaseClaimOutcome::Stale);
    };
    Ok(CompensationExternalNotStartedReleaseClaimOutcome::Applied {
        request,
        attempt: compensation_attempt_from_row(&updated, &run)?,
    })
}

async fn load_fenced_compensation_with_attempt_controller(
    conn: &mut ScopedConn<'_>,
    fence: CompensationAttemptFence,
    attempt_controller_generation: u64,
) -> Result<Option<(ExecutionRunRecord, PgRow)>> {
    let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(fence.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(None);
    };
    let run = run_from_row(&run_row)?;
    if run.controller_generation != fence.controller_generation {
        return Ok(None);
    }
    let Some(row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
        .bind(fence.run_uid)
        .bind(fence.compensation_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(None);
    };
    if required_u64(&row, "generation")? != fence.compensation_generation
        || required_u64(&row, "attempt_generation")? != fence.attempt_generation
    {
        return Ok(None);
    }
    let dispatch_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_dispatch_outbox WHERE dispatch_uid=$1 \
         AND run_uid=$2 AND compensation_id=$3 AND controller_generation=$4 \
         AND compensation_generation=$5 AND compensation_attempt_generation=$6 \
         AND dispatch_kind='compensation_attempt')",
    )
    .bind(fence.dispatch_uid)
    .bind(fence.run_uid)
    .bind(fence.compensation_id.as_uuid())
    .bind(to_i64(
        attempt_controller_generation,
        "attempt controller generation",
    )?)
    .bind(to_i64(
        fence.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        fence.attempt_generation,
        "compensation attempt generation",
    )?)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(dispatch_exists.then_some((run, row)))
}

/// Repairs one exact compensation dispatch that dead-lettered before provider start.
pub(super) async fn settle_unstarted_compensation_attempt_in_conn(
    conn: &mut ScopedConn<'_>,
    request: &ExecutionCompensationAttemptRequest,
    settled_at: DateTime<Utc>,
) -> Result<CompensationAttemptWriteOutcome> {
    let Some(tenant_id) = load_compensation_tenant(conn, request.run_uid).await? else {
        return Ok(CompensationAttemptWriteOutcome::NotFound);
    };
    if tenant_id != request.tenant_id {
        return Ok(CompensationAttemptWriteOutcome::Conflict);
    }
    prelock_existing_capacity_dimensions_in_tx(
        conn.as_mut(),
        tenant_id,
        &[ExecutionCapacityDimension::ActiveTasks],
    )
    .await?;
    let run_row = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(request.run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let run = run_from_row(&run_row)?;
    let row = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
        .bind(request.run_uid)
        .bind(request.compensation_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(CompensationAttemptWriteOutcome::NotFound);
    };
    let current = compensation_attempt_from_row(&row, &run)?;
    if current.registration.generation == request.compensation_generation
        && current.attempt_generation == request.compensation_attempt_generation.saturating_add(1)
        && current.attempt_state == CompensationAttemptState::Idle
        && current.active_dispatch_uid.is_none()
    {
        return Ok(CompensationAttemptWriteOutcome::Replayed(current));
    }
    let exact_resources: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
         WHERE reservation_uid=$1 AND tenant_id=$2 AND run_uid=$3 AND compensation_id=$4 \
           AND controller_generation=$5 AND compensation_generation=$6 \
           AND compensation_attempt_generation=$7 AND resource_dimension='active_tasks' \
           AND state IN ('reserved','reconciling')) \
         AND EXISTS (SELECT 1 FROM moa.execution_trigger WHERE trigger_uid=$8 AND run_uid=$3 \
           AND compensation_id=$4 AND controller_generation=$5 AND compensation_generation=$6 \
           AND compensation_attempt_generation=$7 AND trigger_kind='compensation_watchdog' \
           AND state = 'pending')",
    )
    .bind(request.capacity_reservation_uid)
    .bind(request.tenant_id.0)
    .bind(request.run_uid)
    .bind(request.compensation_id.as_uuid())
    .bind(to_i64(
        request.controller_generation,
        "controller generation",
    )?)
    .bind(to_i64(
        request.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        request.compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(request.watchdog_trigger_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if run.controller_generation != request.controller_generation
        || current.registration.generation != request.compensation_generation
        || current.attempt_generation != request.compensation_attempt_generation
        || current.active_dispatch_uid != Some(request.dispatch_uid)
        || current.attempt_state != CompensationAttemptState::Dispatching
        || !exact_resources
    {
        return Ok(CompensationAttemptWriteOutcome::Conflict);
    }
    let fence = CompensationAttemptFence {
        run_uid: request.run_uid,
        compensation_id: request.compensation_id,
        controller_generation: request.controller_generation,
        compensation_generation: request.compensation_generation,
        attempt_generation: request.compensation_attempt_generation,
        dispatch_uid: request.dispatch_uid,
    };
    release_unbound_compensation_external_intent_in_conn(conn, &run, &current).await?;
    release_compensation_attempt_capacity(conn, &run, fence).await?;
    supersede_compensation_triggers(conn, fence, None).await?;
    let updated = sqlx::query(
        "UPDATE moa.execution_compensation SET attempt_state='idle', \
         attempt_generation=attempt_generation+1, attempt_started_at=NULL, \
         attempt_deadline_at=NULL, waiting_since=NULL, active_dispatch_uid=NULL, \
         external_job_uid=NULL, release_intent=NULL, \
         last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
         WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
           AND attempt_generation=$4 AND active_dispatch_uid=$5 \
           AND attempt_state='dispatching' RETURNING *",
    )
    .bind(request.run_uid)
    .bind(request.compensation_id.as_uuid())
    .bind(to_i64(
        request.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        request.compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(request.dispatch_uid)
    .bind(settled_at)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    enqueue_current_compensation_controller_wake(
        conn,
        &run,
        json!({"reason":"compensation_dispatch_delivery_lost"}),
        settled_at,
    )
    .await?;
    Ok(CompensationAttemptWriteOutcome::Applied(
        compensation_attempt_from_row(&updated, &run)?,
    ))
}

async fn supersede_compensation_triggers(
    conn: &mut ScopedConn<'_>,
    fence: CompensationAttemptFence,
    except: Option<ExecutionTriggerKind>,
) -> Result<()> {
    let except = except.map(ExecutionTriggerKind::as_str);
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox SET state='superseded', claim_owner=NULL, \
         claimed_at=NULL, claim_expires_at=NULL, updated_at=NOW() WHERE trigger_uid IN ( \
           SELECT trigger_uid FROM moa.execution_trigger WHERE run_uid=$1 \
           AND compensation_id=$2 AND controller_generation=$3 \
           AND compensation_generation=$4 AND compensation_attempt_generation=$5 \
           AND ($6::TEXT IS NULL OR trigger_kind <> $6) \
         ) AND state IN ('pending','dispatching')",
    )
    .bind(fence.run_uid)
    .bind(fence.compensation_id.as_uuid())
    .bind(to_i64(
        fence.controller_generation,
        "controller generation",
    )?)
    .bind(to_i64(
        fence.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        fence.attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(except)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    // `RETURNING *` rather than a bare UPDATE: every superseded trigger still owns a
    // `scheduled_triggers` capacity receipt, and superseding the row does not release it.
    // Leaking it wedges the run permanently — both terminal-drain branches probe for any
    // held `active_tasks`/`scheduled_triggers`/`external_jobs` reservation and hard-error
    // ("failed compensation retained non-lifetime capacity" / "completed compensation
    // retained non-lifetime capacity"), so a compensating run could never finalize whether
    // its undo failed OR succeeded. `trigger.rs::supersede_trigger_in_conn` releases on
    // every arm; this bulk path must do the same.
    let superseded = sqlx::query(
        "UPDATE moa.execution_trigger SET state='superseded', updated_at=NOW() \
         WHERE run_uid=$1 AND compensation_id=$2 \
         AND controller_generation=$3 AND compensation_generation=$4 \
         AND compensation_attempt_generation=$5 AND ($6::TEXT IS NULL OR trigger_kind <> $6) \
         AND state = 'pending' RETURNING *",
    )
    .bind(fence.run_uid)
    .bind(fence.compensation_id.as_uuid())
    .bind(to_i64(
        fence.controller_generation,
        "controller generation",
    )?)
    .bind(to_i64(
        fence.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_i64(
        fence.attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(except)
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    for row in &superseded {
        let trigger = trigger_from_row(row)?;
        release_trigger_capacity_in_conn(conn.as_mut(), &trigger).await?;
    }
    Ok(())
}

async fn enqueue_current_compensation_controller_wake(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    payload: Value,
    now: DateTime<Utc>,
) -> Result<()> {
    enqueue_run_activation_in_conn(
        conn.as_mut(),
        run.tenant_id,
        run.run_uid,
        run.controller_generation,
        now,
        payload,
    )
    .await?;
    Ok(())
}

type CompensationSettlementFields = (
    CompensationStatus,
    CompensationAttemptState,
    u64,
    u64,
    u64,
    bool,
    Option<Value>,
);

fn compensation_settlement_fields(
    current: &CompensationAttemptRecord,
    outcome: &ExecutionCompensationOutcome,
    retry: bool,
) -> Result<CompensationSettlementFields> {
    if retry {
        let message = match outcome {
            ExecutionCompensationOutcome::Failed { message, .. } => message,
            ExecutionCompensationOutcome::Completed { .. }
            | ExecutionCompensationOutcome::UnknownOutcome { .. } => {
                return Err(Error::InvalidRepositoryInput {
                    message: "only failed compensation outcomes may retry".to_string(),
                });
            }
        };
        return Ok((
            CompensationStatus::Pending,
            CompensationAttemptState::Idle,
            current.registration.attempt.checked_add(1).ok_or_else(|| {
                Error::InvalidRepositoryData {
                    message: "compensation attempt overflow".to_string(),
                }
            })?,
            current
                .registration
                .generation
                .checked_add(1)
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "compensation generation overflow".to_string(),
                })?,
            current.attempt_generation.checked_add(1).ok_or_else(|| {
                Error::InvalidRepositoryData {
                    message: "compensation attempt generation overflow".to_string(),
                }
            })?,
            false,
            Some(json!({"class":"retryable", "message":message})),
        ));
    }
    match outcome {
        ExecutionCompensationOutcome::Completed { .. } => Ok((
            CompensationStatus::Completed,
            CompensationAttemptState::Terminal,
            current.registration.attempt,
            current.registration.generation,
            current.attempt_generation,
            false,
            None,
        )),
        ExecutionCompensationOutcome::Failed { message, .. } => Ok((
            CompensationStatus::Failed,
            CompensationAttemptState::Terminal,
            current.registration.attempt,
            current.registration.generation,
            current.attempt_generation,
            true,
            Some(json!({"class":"terminal", "message":message})),
        )),
        ExecutionCompensationOutcome::UnknownOutcome { message, .. } => Ok((
            CompensationStatus::UnknownOutcome,
            CompensationAttemptState::UnknownOutcome,
            current.registration.attempt,
            current.registration.generation,
            current.attempt_generation,
            true,
            Some(json!({
                "class":"unknown_outcome",
                "message":message,
                "manual_repair_required":true
            })),
        )),
    }
}

fn force_terminal_failure_if_exhausted(
    outcome: ExecutionCompensationOutcome,
) -> ExecutionCompensationOutcome {
    match outcome {
        ExecutionCompensationOutcome::Failed {
            message,
            retryable: true,
            usage,
        } => ExecutionCompensationOutcome::Failed {
            message,
            retryable: false,
            usage,
        },
        other => other,
    }
}

/// Settles one terminal provider job into its exact waiting compensation attempt.
pub(super) async fn settle_external_job_terminal_in_conn(
    conn: &mut ScopedConn<'_>,
    job: &ExecutionExternalJobRecord,
    settled_at: DateTime<Utc>,
) -> Result<CompensationExternalJobSettlementOutcome> {
    if !job.state.is_terminal() {
        return Err(Error::InvalidRepositoryInput {
            message: "compensation external-job settlement requires a terminal job".to_string(),
        });
    }
    let ExecutionExternalJobOwner::Compensation {
        compensation_id,
        compensation_generation,
        compensation_attempt_generation,
    } = job.owner
    else {
        return Ok(CompensationExternalJobSettlementOutcome::Stale);
    };
    let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(job.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(CompensationExternalJobSettlementOutcome::NotFound);
    };
    let run = run_from_row(&run_row)?;
    let Some(row) = sqlx::query(LOAD_COMPENSATION_FOR_UPDATE_SQL)
        .bind(job.run_uid)
        .bind(compensation_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(CompensationExternalJobSettlementOutcome::NotFound);
    };
    let current = compensation_attempt_from_row(&row, &run)?;
    if current.registration.status.is_settled() {
        return Ok(if current.external_job_uid == Some(job.external_job_uid) {
            CompensationExternalJobSettlementOutcome::Replayed(current)
        } else {
            CompensationExternalJobSettlementOutcome::Stale
        });
    }
    if run.tenant_id == job.tenant_id
        && current.registration.generation == compensation_generation
        && current.attempt_generation == compensation_attempt_generation
        && current.attempt_state == CompensationAttemptState::Cancelling
        && current.release_intent == Some(ExecutionCompensationReleaseIntent::ExternalJob)
        && current.external_job_uid == Some(job.external_job_uid)
    {
        return Ok(CompensationExternalJobSettlementOutcome::DeferredRelease(
            current,
        ));
    }
    if run.tenant_id != job.tenant_id
        || current.registration.generation != compensation_generation
        || current.attempt_generation != compensation_attempt_generation
        || current.attempt_state != CompensationAttemptState::WaitingExternal
        || current.external_job_uid != Some(job.external_job_uid)
    {
        return Ok(CompensationExternalJobSettlementOutcome::Stale);
    }
    let previous_usage = current
        .registration
        .outcome
        .as_ref()
        .map(ExecutionCompensationOutcome::usage)
        .cloned()
        .unwrap_or_else(zero_usage);
    let outcome = match job.state {
        ExecutionExternalJobState::Completed => ExecutionCompensationOutcome::Completed {
            output: job.output.clone().unwrap_or(Value::Null),
            usage: previous_usage.clone(),
        },
        ExecutionExternalJobState::Failed => ExecutionCompensationOutcome::Failed {
            message: job
                .error
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "asynchronous compensation job failed".to_string()),
            retryable: true,
            usage: previous_usage.clone(),
        },
        ExecutionExternalJobState::Cancelled => ExecutionCompensationOutcome::Failed {
            message: "asynchronous compensation job was cancelled".to_string(),
            retryable: false,
            usage: previous_usage.clone(),
        },
        ExecutionExternalJobState::UnknownOutcome => ExecutionCompensationOutcome::UnknownOutcome {
            message: job.error.as_ref().map(Value::to_string).unwrap_or_else(|| {
                "asynchronous compensation job has an unknown outcome".to_string()
            }),
            usage: previous_usage.clone(),
        },
        ExecutionExternalJobState::Unbound
        | ExecutionExternalJobState::Starting
        | ExecutionExternalJobState::Running
        | ExecutionExternalJobState::WaitingReconcile
        | ExecutionExternalJobState::CancelRequested => {
            return Err(Error::InvalidRepositoryInput {
                message: "compensation external-job settlement observed nonterminal state"
                    .to_string(),
            });
        }
    };
    let forward_task =
        load_forward_task(conn, job.run_uid, current.registration.forward_task_id).await?;
    let full_reservation =
        compensation_reservation(&run, &current.registration, forward_task.retry.max_attempts)?;
    let remaining = remaining_compensation_reservation(full_reservation, &previous_usage);
    let retry = matches!(
        outcome,
        ExecutionCompensationOutcome::Failed {
            retryable: true,
            ..
        }
    ) && run.pending_terminal.is_none()
        && current.registration.attempt < u64::from(forward_task.retry.max_attempts);
    let accepted_outcome = if retry {
        outcome
    } else {
        force_terminal_failure_if_exhausted(outcome)
    };
    let mut ledger = budget_ledger(&run);
    let reconciliation = ledger.reconcile_cumulative_with_ceiling(
        remaining,
        &previous_usage,
        accepted_outcome.usage(),
        !retry,
        i64::MAX as u64,
    )?;
    let (status, attempt_state, attempt, generation, next_attempt_generation, repair, error) =
        compensation_settlement_fields(&current, &accepted_outcome, retry)?;
    let persisted = persisted_compensation_outcome(&row, Some(accepted_outcome))?;
    let updated = sqlx::query(
        "UPDATE moa.execution_compensation SET status=$6, attempt_state=$7, attempt=$8, \
         generation=$9, attempt_generation=$10, outcome=$11, error=$12, \
         attempt_started_at=NULL, attempt_deadline_at=NULL, waiting_since=NULL, \
         active_dispatch_uid=NULL, \
         last_progress_at=GREATEST(last_progress_at,$13), updated_at=NOW(), \
         completed_at=CASE WHEN $14 THEN $13 ELSE NULL END \
         WHERE run_uid=$1 AND compensation_id=$2 AND generation=$3 \
           AND attempt_generation=$4 AND external_job_uid=$5 \
           AND attempt_state='waiting_external' RETURNING *",
    )
    .bind(job.run_uid)
    .bind(compensation_id)
    .bind(to_i64(compensation_generation, "compensation generation")?)
    .bind(to_i64(
        compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(job.external_job_uid)
    .bind(status.as_str())
    .bind(attempt_state.as_str())
    .bind(to_i64(attempt, "compensation attempt")?)
    .bind(to_i64(generation, "compensation generation")?)
    .bind(to_i64(
        next_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(serde_json::to_value(persisted)?)
    .bind(error)
    .bind(settled_at)
    .bind(status.is_settled())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(updated) = updated else {
        return Ok(CompensationExternalJobSettlementOutcome::Stale);
    };
    persist_run_budget_and_repair(conn, job.run_uid, &reconciliation, repair).await?;
    if !matches!(
        run.status,
        ExecutionRunStatus::PauseRequested
            | ExecutionRunStatus::Pausing
            | ExecutionRunStatus::Paused
    ) {
        enqueue_current_compensation_controller_wake(
            conn,
            &run,
            json!({
                "reason": "compensation_external_job_settled",
                "external_job_uid": job.external_job_uid,
            }),
            settled_at,
        )
        .await?;
    }
    Ok(CompensationExternalJobSettlementOutcome::Applied(
        compensation_attempt_from_row(&updated, &run)?,
    ))
}

fn compensation_release_intent_label(intent: ExecutionCompensationReleaseIntent) -> &'static str {
    match intent {
        ExecutionCompensationReleaseIntent::Outcome => "outcome",
        ExecutionCompensationReleaseIntent::Retry => "retry",
        ExecutionCompensationReleaseIntent::Review => "review",
        ExecutionCompensationReleaseIntent::ExternalJob => "external_job",
        ExecutionCompensationReleaseIntent::Pause => "pause",
        ExecutionCompensationReleaseIntent::Watchdog => "watchdog",
        ExecutionCompensationReleaseIntent::Deadline => "deadline",
        ExecutionCompensationReleaseIntent::RunTerminal => "run_terminal",
    }
}

fn validate_compensation_settlement_intent(
    intent: ExecutionCompensationReleaseIntent,
    outcome: &ExecutionCompensationOutcome,
) -> Result<()> {
    let retryable_failure = matches!(
        outcome,
        ExecutionCompensationOutcome::Failed {
            retryable: true,
            ..
        }
    );
    let valid = match intent {
        ExecutionCompensationReleaseIntent::Outcome => !retryable_failure,
        ExecutionCompensationReleaseIntent::Retry
        | ExecutionCompensationReleaseIntent::Watchdog => retryable_failure,
        ExecutionCompensationReleaseIntent::Deadline
        | ExecutionCompensationReleaseIntent::RunTerminal => !retryable_failure,
        ExecutionCompensationReleaseIntent::Review
        | ExecutionCompensationReleaseIntent::ExternalJob
        | ExecutionCompensationReleaseIntent::Pause => false,
    };
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidRepositoryInput {
            message: format!(
                "compensation release intent `{}` does not match its settlement path",
                compensation_release_intent_label(intent)
            ),
        })
    }
}

fn compensation_outcome_from_review_resolution(
    resolution: &ExecutionActionReviewResolution,
) -> Result<ExecutionCompensationOutcome> {
    Ok(match resolution {
        ExecutionActionReviewResolution::Completed { tool_output } => {
            ExecutionCompensationOutcome::Completed {
                output: tool_output.clone(),
                usage: zero_usage(),
            }
        }
        ExecutionActionReviewResolution::UnknownOutcome { message } => {
            ExecutionCompensationOutcome::UnknownOutcome {
                message: message.clone(),
                usage: zero_usage(),
            }
        }
        ExecutionActionReviewResolution::ExternalJob { .. } => {
            return Err(Error::InvalidRepositoryInput {
                message: "compensation external-job review requires the durable external-job owner handoff"
                    .to_string(),
            });
        }
        ExecutionActionReviewResolution::Failed { message, .. } => {
            ExecutionCompensationOutcome::Failed {
                message: message.clone(),
                retryable: false,
                usage: zero_usage(),
            }
        }
        ExecutionActionReviewResolution::NotDispatched { reason } => {
            ExecutionCompensationOutcome::Failed {
                message: format!("compensation was not dispatched: {reason:?}"),
                retryable: true,
                usage: zero_usage(),
            }
        }
        ExecutionActionReviewResolution::Denied { reason }
        | ExecutionActionReviewResolution::TimedOut { reason } => {
            ExecutionCompensationOutcome::Failed {
                message: reason.clone(),
                retryable: false,
                usage: zero_usage(),
            }
        }
    })
}

async fn active_attempt_capacity_count(conn: &mut ScopedConn<'_>, run_uid: Uuid) -> Result<u64> {
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_capacity_reservation WHERE run_uid=$1 \
         AND resource_dimension='active_tasks' AND state IN ('reserved','reconciling')",
    )
    .bind(run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    u64::try_from(count).map_err(|_| Error::InvalidRepositoryData {
        message: "active-attempt capacity count is negative".to_string(),
    })
}

async fn reconcile_run_after_compensation_capacity_release(
    conn: &mut ScopedConn<'_>,
    run: &ExecutionRunRecord,
    now: DateTime<Utc>,
) -> Result<ExecutionRunRecord> {
    let active_count = active_attempt_capacity_count(conn, run.run_uid).await?;
    let row = sqlx::query(
        "UPDATE moa.execution_run SET active_task_count=$3, \
         status=CASE WHEN status IN ('pause_requested','pausing') AND $3=0 \
                     THEN 'paused' ELSE status END, \
         activation_state=CASE WHEN status IN ('pause_requested','pausing') AND $3=0 \
                               THEN 'paused' ELSE activation_state END, \
         paused_at=CASE WHEN status IN ('pause_requested','pausing') AND $3=0 \
                        THEN COALESCE(paused_at,GREATEST($4,pause_requested_at,clock_timestamp())) \
                        ELSE paused_at END, \
         last_progress_at=GREATEST(last_progress_at,$4,clock_timestamp()),updated_at=NOW() \
         WHERE run_uid=$1 AND controller_generation=$2 RETURNING *",
    )
    .bind(run.run_uid)
    .bind(to_i64(run.controller_generation, "controller generation")?)
    .bind(to_i64(active_count, "active task count")?)
    .bind(now)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: "compensation release lost its current run generation".to_string(),
    })?;
    run_from_row(&row)
}

#[allow(clippy::too_many_arguments)]
async fn nonterminal_task_exists(conn: &mut ScopedConn<'_>, run_uid: Uuid) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_task WHERE run_uid=$1 \
         AND status NOT IN ('completed','skipped','failed','cancelled','unknown_outcome'))",
    )
    .bind(run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)
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
        "UPDATE moa.execution_compensation SET status='failed', attempt_state='terminal', outcome=$3, \
         error=jsonb_build_object('class','budget_exceeded','message','approved execution budget cannot reserve compensation'), \
         started_at=COALESCE(started_at,NOW()), attempt_started_at=COALESCE(attempt_started_at,NOW()), \
         last_progress_at=GREATEST(last_progress_at,NOW()), attempt_deadline_at=NULL, \
         waiting_since=NULL, active_dispatch_uid=NULL, \
         completed_at=NOW(), updated_at=NOW() WHERE run_uid=$1 AND compensation_id=$2 \
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
