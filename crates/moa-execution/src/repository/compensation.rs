//! Compensation registration, fencing, reverse-order claims, and finalization.

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
    ready::transition_node_counters_in_tx,
    rows::*,
    run::enqueue_run_activation_in_conn,
    sql::*,
    task::settle_external_job_terminal_in_conn as settle_task_external_job_terminal_in_conn,
    terminal::{
        PendingTerminalAdvanceCommit, PendingTerminalAdvanceOutcome, PendingTerminalAdvanceStage,
        ReplanStopReceipt, drain_run_triggers_page_in_conn,
    },
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

const PENDING_TERMINAL_CANCEL_NAMESPACE: Uuid =
    Uuid::from_u128(0xd3d4_9744_5c24_58cc_8be8_4806_faba_1837);
const MAX_PENDING_TERMINAL_PAGE_SIZE: u32 = 1_000;

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
            &[ExecutionCapacityDimension::ActiveTasks],
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
        cancellation_dispatches: Vec::new(),
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
                     waiting_reasons='[]'::JSONB, next_wake_at=NULL, waiting_since=NULL, \
                     waiting_task_count=0, waiting_input_task_count=0, \
                     waiting_review_task_count=0, waiting_signal_task_count=0, \
                     waiting_timer_task_count=0, waiting_external_task_count=0, \
                     waiting_replan_task_count=0, waiting_input_user_task_count=0, \
                     waiting_input_tenant_admin_task_count=0, \
                     waiting_input_external_task_count=0, waiting_reasons_truncated=FALSE, \
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
           AND task.attempt_state <> 'cancelling' \
         ORDER BY CASE WHEN task.attempt_state IN ('dispatching','running') THEN 0 ELSE 1 END, \
                  task.task_id LIMIT $2 FOR UPDATE",
    )
    .bind(run.run_uid)
    .bind(i64::from(page_limit))
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let processed_task_count =
        u32::try_from(task_rows.len()).map_err(|_| Error::InvalidRepositoryData {
            message: "terminal drain task page exceeds u32".to_string(),
        })?;
    let mut settled_task_count = 0_u64;
    let mut cancellation_dispatches = Vec::with_capacity(task_rows.len());
    for row in task_rows {
        let task = task_from_row(&row)?;
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
            ExecutionAttemptState::Dispatching | ExecutionAttemptState::Running
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
        supersede_storage_task_waits(&mut conn, &task).await?;
        let original_status = task.status;
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
        "SELECT reservation.reservation_uid, trigger.trigger_uid \
         FROM moa.execution_capacity_reservation AS reservation \
         JOIN moa.execution_trigger AS trigger ON trigger.run_uid=reservation.run_uid \
          AND trigger.task_id=reservation.task_id \
          AND trigger.controller_generation=reservation.controller_generation \
          AND trigger.attempt_generation=reservation.attempt_generation \
          AND trigger.trigger_kind='task_watchdog' \
          AND trigger.state = 'pending' \
         WHERE reservation.run_uid=$1 AND reservation.task_id=$2 \
          AND reservation.controller_generation=$3 AND reservation.attempt_generation=$4 \
          AND reservation.resource_dimension='active_tasks' \
          AND reservation.state IN ('reserved','reconciling') FOR UPDATE OF reservation, trigger",
    )
    .bind(run.run_uid)
    .bind(task.task_id.as_uuid())
    .bind(to_i64(run.controller_generation, "controller generation")?)
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
             AND attempt_state IN ('dispatching','running')",
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
        attempt_controller_generation: run.controller_generation,
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

async fn supersede_storage_task_waits(
    conn: &mut ScopedConn<'_>,
    task: &ExecutionTaskRecord,
) -> Result<()> {
    let trigger_uids = sqlx::query_scalar::<_, Uuid>(
        "UPDATE moa.execution_trigger SET state='superseded', updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 \
             AND trigger_kind <> 'task_watchdog' AND state = 'pending' \
             RETURNING trigger_uid",
    )
    .bind(task.run_uid)
    .bind(task.task_id.as_uuid())
    .fetch_all(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if !trigger_uids.is_empty() {
        sqlx::query(
            "UPDATE moa.execution_dispatch_outbox SET state='superseded', claim_owner=NULL, \
             claimed_at=NULL, claim_expires_at=NULL, updated_at=NOW() \
             WHERE trigger_uid=ANY($1::UUID[]) \
             AND state IN ('pending','dispatching')",
        )
        .bind(&trigger_uids)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    }
    Ok(())
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
                        THEN COALESCE(paused_at,$4) ELSE paused_at END, \
         last_progress_at=GREATEST(last_progress_at,$4),updated_at=NOW() \
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
             active_task_count=$6, processed_wake_epoch=$3, \
             last_progress_at=GREATEST(last_progress_at,$7), updated_at=NOW() \
         WHERE run_uid=$1 AND controller_generation=$2 AND wake_epoch >= $3 \
           AND processed_wake_epoch < $3 \
           AND activation_state IN ('queued','advancing','paused') RETURNING *",
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
             waiting_task_count=0, waiting_input_task_count=0, waiting_review_task_count=0, \
             waiting_signal_task_count=0, waiting_timer_task_count=0, \
             waiting_external_task_count=0, waiting_replan_task_count=0, \
             waiting_input_user_task_count=0, waiting_input_tenant_admin_task_count=0, \
             waiting_input_external_task_count=0, waiting_reasons_truncated=FALSE, \
             waiting_since=NULL, ready_task_count=0, active_task_count=0, \
             processed_wake_epoch=$3, completed_at=$12, \
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
