//! Transactional dispatch outbox persistence for bounded execution activations.

use std::{collections::HashSet, str::FromStr, time::Duration};

use crate::wire::{
    ExecutionCompensationAttemptCancelRequest, ExecutionCompensationAttemptRequest,
    ExecutionExternalJobCancelRequest, ExecutionTaskAttemptCancelRequest,
    ExecutionTaskAttemptRequest,
};
use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::{
    Error, ExecutionRepository, ExecutionScope, Result, sqlx_error, storage_error,
    task::{TaskAttemptFence, TaskAttemptSettlementOutcome},
    to_optional_i64, to_u32,
};

const MAX_CLAIM_BATCH_SIZE: u32 = 1_000;
const MAX_HEALTH_SAMPLE_SIZE: u32 = 100_000;
/// Per-phase census of the live nonterminal fleet.
///
/// The status list is spelled out rather than bound as a parameter so it matches the
/// `execution_run_nonterminal_idx` predicate literally: the planner cannot prove a bound
/// array implies that predicate, and a parameterized form loses the index-only scan. The
/// list is pinned against [`ExecutionRunPhaseDimension::ALL`] by an offline test, and the
/// index leads on `status` so the grouped aggregate needs no sort. The scan is proportional
/// to the live nonterminal set, not to run history, because the index is partial.
const RUN_PHASE_CENSUS_SQL: &str = r#"
SELECT status, count(*)::BIGINT AS run_count
FROM moa.execution_run
WHERE status IN (
    'awaiting_confirmation', 'queued', 'running', 'waiting_input',
    'waiting_review', 'waiting_signal', 'waiting_timer', 'waiting_external',
    'waiting_replan', 'pause_requested', 'pausing', 'paused', 'compensating'
)
GROUP BY status
"#;
const MAX_ERROR_CHARS: usize = 4_096;
const MAX_MAINTENANCE_ERROR_BYTES: usize = 4_096;

/// Durable dispatch target selected by the execution controller.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionDispatchKind {
    /// Wake one bounded run-controller activation.
    RunActivation,
    /// Start one bounded task-attempt generation.
    TaskAttempt,
    /// Start one bounded compensation-attempt generation.
    CompensationAttempt,
    /// Cancel one exact active task-attempt generation.
    TaskAttemptCancel,
    /// Cancel one exact active compensation-attempt generation.
    CompensationAttemptCancel,
    /// Deliver one immutable temporal trigger.
    TriggerDelivery,
    /// Request cancellation of one asynchronous provider job.
    ExternalCancel,
}

impl ExecutionDispatchKind {
    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunActivation => "run_activation",
            Self::TaskAttempt => "task_attempt",
            Self::CompensationAttempt => "compensation_attempt",
            Self::TaskAttemptCancel => "task_attempt_cancel",
            Self::CompensationAttemptCancel => "compensation_attempt_cancel",
            Self::TriggerDelivery => "trigger_delivery",
            Self::ExternalCancel => "external_cancel",
        }
    }
}

impl FromStr for ExecutionDispatchKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "run_activation" => Ok(Self::RunActivation),
            "task_attempt" => Ok(Self::TaskAttempt),
            "compensation_attempt" => Ok(Self::CompensationAttempt),
            "task_attempt_cancel" => Ok(Self::TaskAttemptCancel),
            "compensation_attempt_cancel" => Ok(Self::CompensationAttemptCancel),
            "trigger_delivery" => Ok(Self::TriggerDelivery),
            "external_cancel" => Ok(Self::ExternalCancel),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution dispatch kind `{value}`"),
            }),
        }
    }
}

/// Lifecycle state shared by execution trigger and dispatch queues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionDeliveryState {
    /// Durable work has not been claimed.
    Pending,
    /// One replica owns a time-bounded delivery claim.
    Dispatching,
    /// Delivery was acknowledged successfully.
    Delivered,
    /// A newer generation replaced this work.
    Superseded,
    /// Cancellation fenced this work before delivery.
    Cancelled,
    /// Delivery exhausted its bounded retry policy.
    DeadLetter,
}

impl ExecutionDeliveryState {
    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Dispatching => "dispatching",
            Self::Delivered => "delivered",
            Self::Superseded => "superseded",
            Self::Cancelled => "cancelled",
            Self::DeadLetter => "dead_letter",
        }
    }
}

impl FromStr for ExecutionDeliveryState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "dispatching" => Ok(Self::Dispatching),
            "delivered" => Ok(Self::Delivered),
            "superseded" => Ok(Self::Superseded),
            "cancelled" => Ok(Self::Cancelled),
            "dead_letter" => Ok(Self::DeadLetter),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution delivery state `{value}`"),
            }),
        }
    }
}

/// Immutable dispatch intent inserted in the same transaction as its state mutation.
#[derive(Clone, Debug, PartialEq)]
pub struct NewExecutionDispatch {
    /// Stable dispatch identity used by replaying callers.
    pub dispatch_uid: Uuid,
    /// Tenant that owns every referenced row.
    pub tenant_id: TenantId,
    /// Owning execution run, when this target is run-scoped.
    pub run_uid: Option<Uuid>,
    /// Owning logical task, for task-attempt and external-cancel dispatches.
    pub task_id: Option<Uuid>,
    /// Owning compensation registration for compensation-attempt dispatches.
    pub compensation_id: Option<Uuid>,
    /// Immutable temporal trigger target.
    pub trigger_uid: Option<Uuid>,
    /// Asynchronous provider job target.
    pub external_job_uid: Option<Uuid>,
    /// Durable dispatch target kind.
    pub kind: ExecutionDispatchKind,
    /// Current run-controller generation fence.
    pub controller_generation: Option<u64>,
    /// Exact wake epoch for a run activation.
    pub wake_epoch: Option<u64>,
    /// Exact task-attempt generation fence.
    pub attempt_generation: Option<u64>,
    /// Exact compensation registration generation fence.
    pub compensation_generation: Option<u64>,
    /// Exact compensation-attempt generation fence.
    pub compensation_attempt_generation: Option<u64>,
    /// Earliest time at which delivery may be claimed.
    pub not_before_at: DateTime<Utc>,
    /// Bounded structured delivery payload.
    pub payload: Value,
}

/// One persisted dispatch row.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionDispatchRecord {
    /// Stable dispatch identity.
    pub dispatch_uid: Uuid,
    /// Tenant that owns the row.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Option<Uuid>,
    /// Owning task.
    pub task_id: Option<Uuid>,
    /// Owning compensation registration.
    pub compensation_id: Option<Uuid>,
    /// Target temporal trigger.
    pub trigger_uid: Option<Uuid>,
    /// Target external job.
    pub external_job_uid: Option<Uuid>,
    /// Durable target kind.
    pub kind: ExecutionDispatchKind,
    /// Current delivery lifecycle state.
    pub state: ExecutionDeliveryState,
    /// Run-controller generation fence.
    pub controller_generation: Option<u64>,
    /// Exact run wake epoch.
    pub wake_epoch: Option<u64>,
    /// Exact task-attempt generation.
    pub attempt_generation: Option<u64>,
    /// Exact compensation registration generation.
    pub compensation_generation: Option<u64>,
    /// Exact compensation-attempt generation.
    pub compensation_attempt_generation: Option<u64>,
    /// Earliest delivery time.
    pub not_before_at: DateTime<Utc>,
    /// Structured delivery payload.
    pub payload: Value,
    /// Current claim owner.
    pub claim_owner: Option<String>,
    /// Claim acquisition time.
    pub claimed_at: Option<DateTime<Utc>>,
    /// Claim expiry time.
    pub claim_expires_at: Option<DateTime<Utc>>,
    /// Number of bounded delivery attempts.
    pub delivery_attempts: u32,
    /// Monotonic repair generation, incremented once per recovery requeue.
    ///
    /// Delivery identity is `dispatch_uid` while this is zero and
    /// `{dispatch_uid}:{repair_epoch}` afterwards, so a repaired row cannot attach to the
    /// completed invocation its original identity already memoized.
    pub repair_epoch: u32,
    /// Successful delivery time.
    pub delivered_at: Option<DateTime<Utc>>,
    /// Latest bounded delivery error.
    pub last_error: Option<String>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last mutation time.
    pub updated_at: DateTime<Utc>,
}

/// Bounded retry and dead-letter policy for dispatch delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionDispatchRetryPolicy {
    /// Maximum claimed attempts before dead-lettering.
    pub max_attempts: u32,
    /// Delay after the first failed attempt.
    pub base_delay: Duration,
    /// Maximum retry delay.
    pub maximum_delay: Duration,
}

impl ExecutionDispatchRetryPolicy {
    /// Returns the bounded delay after one failed claimed attempt.
    #[must_use]
    pub fn retry_delay(self, attempts: u32) -> Duration {
        let shift = attempts.saturating_sub(1).min(16);
        self.base_delay
            .saturating_mul(1_u32 << shift)
            .min(self.maximum_delay)
    }

    fn validate(self) -> Result<()> {
        if self.max_attempts == 0
            || self.base_delay.is_zero()
            || self.maximum_delay < self.base_delay
        {
            return Err(Error::InvalidRepositoryInput {
                message: "execution dispatch retry policy requires positive ordered bounds"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// Result of recording a claimed dispatch failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionDispatchFailureOutcome {
    /// The exact claim was released behind this durable retry time.
    RetryScheduled { not_before_at: DateTime<Utc> },
    /// The exact claim exhausted its delivery budget and its owner was repaired.
    DeadLettered,
    /// The exact claim exhausted its delivery budget while its owner was already settled.
    ///
    /// The dead letter still commits: refusing it would re-poison every drain that reclaims
    /// the row. The caller is expected to surface this as an operator-visible anomaly.
    DeadLetteredWithoutOwnerRepair,
    /// The row was absent, already terminal, or owned by another claim.
    StaleClaim,
}

/// One count-capped queue sample suitable for low-frequency fleet metrics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionQueueBacklogSample {
    /// Oldest observed due or terminal timestamp.
    pub oldest_at: Option<DateTime<Utc>>,
    /// Number of observed rows, capped at the caller's sample limit.
    pub observed_count: u32,
    /// Whether at least one additional row existed beyond the reported count.
    pub saturated: bool,
}

/// Bounded long-horizon resource governed by execution admission.
///
/// The variants are exactly the durable
/// `moa.execution_capacity_bucket.resource_dimension` discriminators.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAdmissionResourceDimension {
    /// Nonterminal runs that are not fully parked.
    ActiveRuns,
    /// Forward and compensation attempts holding active-compute reservations.
    ActiveTasks,
    /// Runs retained in storage-only waiting or paused states.
    ParkedRuns,
    /// Pending durable trigger rows.
    ScheduledTriggers,
    /// Nonterminal asynchronous provider jobs.
    ExternalJobs,
}

impl ExecutionAdmissionResourceDimension {
    /// Every bounded dimension, in durable label order.
    pub const ALL: [Self; 5] = [
        Self::ActiveRuns,
        Self::ActiveTasks,
        Self::ParkedRuns,
        Self::ScheduledTriggers,
        Self::ExternalJobs,
    ];

    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ActiveRuns => "active_runs",
            Self::ActiveTasks => "active_tasks",
            Self::ParkedRuns => "parked_runs",
            Self::ScheduledTriggers => "scheduled_triggers",
            Self::ExternalJobs => "external_jobs",
        }
    }
}

impl FromStr for ExecutionAdmissionResourceDimension {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "active_runs" => Ok(Self::ActiveRuns),
            "active_tasks" => Ok(Self::ActiveTasks),
            "parked_runs" => Ok(Self::ParkedRuns),
            "scheduled_triggers" => Ok(Self::ScheduledTriggers),
            "external_jobs" => Ok(Self::ExternalJobs),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution admission resource `{value}`"),
            }),
        }
    }
}

/// Ceiling utilization and tenant concentration for one bounded admission resource.
///
/// Ratios are aggregated inside the database, so no tenant identity leaves the query.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionAdmissionUtilizationSample {
    /// Bounded resource whose ceilings were observed.
    pub resource: ExecutionAdmissionResourceDimension,
    /// Utilization of the shared fleet ceiling.
    pub fleet_ratio: f64,
    /// Highest utilization observed across tenant-scoped ceilings.
    pub tenant_peak_ratio: f64,
    /// Largest single tenant's share of everything tenants currently hold.
    ///
    /// This is a different question from `tenant_peak_ratio`, which measures a tenant
    /// against its own ceiling. A small tenant can sit at `tenant_peak_ratio` 1.0 while
    /// holding almost none of the fleet, and a large tenant can hold the entire fleet
    /// while far from its own ceiling. Only this ratio answers whether one tenant is
    /// crowding out the others.
    pub tenant_max_share_ratio: f64,
}

/// Bounded nonterminal run phase reported by the fleet run census.
///
/// The variants are exactly the nonterminal `moa.execution_run.status` discriminators
/// carried by the `execution_run_nonterminal_idx` predicate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRunPhaseDimension {
    /// The displayed plan and estimate require owning-user confirmation.
    AwaitingConfirmation,
    /// The run is accepted and may materialize or reserve work.
    Queued,
    /// At least one task may be executing.
    Running,
    /// The run is waiting for task input.
    WaitingInput,
    /// The run is waiting for a tenant review decision.
    WaitingReview,
    /// The run is waiting for a named external signal.
    WaitingSignal,
    /// The run is waiting for an exact durable timer.
    WaitingTimer,
    /// The run is waiting for an asynchronous external job.
    WaitingExternal,
    /// The run is waiting for a compiler-validated amendment.
    WaitingReplan,
    /// An authorized caller requested a safe pause.
    PauseRequested,
    /// Active work is reaching safe checkpoint boundaries before pausing.
    Pausing,
    /// The run is durably parked without active compute.
    Paused,
    /// Forward work is fenced while committed effects are undone in reverse order.
    Compensating,
}

impl ExecutionRunPhaseDimension {
    /// Every bounded nonterminal phase, in durable status order.
    pub const ALL: [Self; 13] = [
        Self::AwaitingConfirmation,
        Self::Queued,
        Self::Running,
        Self::WaitingInput,
        Self::WaitingReview,
        Self::WaitingSignal,
        Self::WaitingTimer,
        Self::WaitingExternal,
        Self::WaitingReplan,
        Self::PauseRequested,
        Self::Pausing,
        Self::Paused,
        Self::Compensating,
    ];

    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AwaitingConfirmation => "awaiting_confirmation",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::WaitingReview => "waiting_review",
            Self::WaitingSignal => "waiting_signal",
            Self::WaitingTimer => "waiting_timer",
            Self::WaitingExternal => "waiting_external",
            Self::WaitingReplan => "waiting_replan",
            Self::PauseRequested => "pause_requested",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Compensating => "compensating",
        }
    }
}

impl FromStr for ExecutionRunPhaseDimension {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "awaiting_confirmation" => Ok(Self::AwaitingConfirmation),
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "waiting_input" => Ok(Self::WaitingInput),
            "waiting_review" => Ok(Self::WaitingReview),
            "waiting_signal" => Ok(Self::WaitingSignal),
            "waiting_timer" => Ok(Self::WaitingTimer),
            "waiting_external" => Ok(Self::WaitingExternal),
            "waiting_replan" => Ok(Self::WaitingReplan),
            "pause_requested" => Ok(Self::PauseRequested),
            "pausing" => Ok(Self::Pausing),
            "paused" => Ok(Self::Paused),
            "compensating" => Ok(Self::Compensating),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown nonterminal execution run status `{value}`"),
            }),
        }
    }
}

/// Live run count for one bounded nonterminal phase.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionRunPhaseSample {
    /// Bounded nonterminal phase.
    pub phase: ExecutionRunPhaseDimension,
    /// Runs currently in that phase across the fleet.
    pub run_count: u64,
}

/// Bounded trigger/outbox health observed in one scoped transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionQueueHealthSnapshot {
    /// Canonical database observation time.
    pub observed_at: DateTime<Utc>,
    /// Pending triggers whose canonical deadline has arrived.
    pub due_triggers: ExecutionQueueBacklogSample,
    /// Pending or claim-expired dispatches eligible for delivery.
    pub claimable_dispatches: ExecutionQueueBacklogSample,
    /// Outbox deliveries that exhausted their retry policy.
    pub dead_letter_dispatches: ExecutionQueueBacklogSample,
    /// Nonterminal runs whose absolute deadline has elapsed, capped at the sample limit.
    pub overdue_deadlines: u32,
    /// Start time of the oldest active forward or compensation attempt.
    pub oldest_active_attempt_at: Option<DateTime<Utc>>,
    /// Live run count for every bounded nonterminal phase, including idle zeroes.
    pub run_phases: Vec<ExecutionRunPhaseSample>,
    /// Creation time of the oldest nonterminal asynchronous external job.
    pub oldest_external_job_at: Option<DateTime<Utc>>,
    /// Ceiling utilization for every bounded admission resource, including idle zeroes.
    pub admission_utilization: Vec<ExecutionAdmissionUtilizationSample>,
}

/// Database clock and earliest indexed deadline for pending dispatch work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionDispatchWake {
    /// Database time observed in the same statement as the pending deadline.
    pub observed_at: DateTime<Utc>,
    /// Exact queue-head dispatch, if any pending work exists.
    pub dispatch_uid: Option<Uuid>,
    /// Earliest pending delivery deadline, if any pending work exists.
    pub next_due_at: Option<DateTime<Utc>>,
    /// Revision of the exact queue head, changed whenever delivery is rearmed or requeued.
    pub head_updated_at: Option<DateTime<Utc>>,
}

/// Closed, low-cardinality execution maintenance job identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionMaintenanceJobKind {
    /// Repairs due trigger delivery and drains the transactional dispatch outbox.
    DispatchReconciliation,
}

impl ExecutionMaintenanceJobKind {
    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DispatchReconciliation => "execution_dispatch_reconciliation",
        }
    }
}

impl FromStr for ExecutionMaintenanceJobKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "execution_dispatch_reconciliation" => Ok(Self::DispatchReconciliation),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution maintenance job kind `{value}`"),
            }),
        }
    }
}

/// Durable fleet-wide execution maintenance checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionMaintenanceCheckpoint {
    /// Closed maintenance job kind.
    pub job_kind: ExecutionMaintenanceJobKind,
    /// Monotonic invocation generation.
    pub generation: u64,
    /// Most recent invocation start.
    pub last_started_at: Option<DateTime<Utc>>,
    /// Most recent successful completion.
    pub last_succeeded_at: Option<DateTime<Utc>>,
    /// Most recent failed completion.
    pub last_failure_at: Option<DateTime<Utc>>,
    /// Bounded error from the most recent failure.
    pub last_error: Option<String>,
    /// Last checkpoint mutation time.
    pub updated_at: DateTime<Utc>,
}

/// Generation-fenced maintenance completion result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionMaintenanceSettlementOutcome {
    /// The exact invocation generation recorded its result.
    Applied(ExecutionMaintenanceCheckpoint),
    /// The checkpoint was absent or a newer invocation generation already started.
    StaleOrMissing,
}

impl ExecutionRepository {
    /// Starts one fleet maintenance invocation and returns its new generation.
    pub async fn begin_execution_maintenance(
        &self,
        scope: ExecutionScope,
        job_kind: ExecutionMaintenanceJobKind,
    ) -> Result<ExecutionMaintenanceCheckpoint> {
        require_control_plane_scope(scope)?;
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(
            r#"
            INSERT INTO moa.execution_maintenance_checkpoint (
                job_kind, generation, last_started_at, updated_at
            ) VALUES ($1, 1, now(), now())
            ON CONFLICT (job_kind) DO UPDATE
            SET generation = moa.execution_maintenance_checkpoint.generation + 1,
                last_started_at = now(), updated_at = now()
            RETURNING *
            "#,
        )
        .bind(job_kind.as_str())
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let checkpoint = maintenance_checkpoint_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(checkpoint)
    }

    /// Records successful completion only for the exact invocation generation.
    pub async fn complete_execution_maintenance(
        &self,
        scope: ExecutionScope,
        job_kind: ExecutionMaintenanceJobKind,
        expected_generation: u64,
    ) -> Result<ExecutionMaintenanceSettlementOutcome> {
        settle_execution_maintenance(self, scope, job_kind, expected_generation, None).await
    }

    /// Records a bounded failure only for the exact invocation generation.
    pub async fn fail_execution_maintenance(
        &self,
        scope: ExecutionScope,
        job_kind: ExecutionMaintenanceJobKind,
        expected_generation: u64,
        error: &str,
    ) -> Result<ExecutionMaintenanceSettlementOutcome> {
        let error = bounded_maintenance_error(error)?;
        settle_execution_maintenance(self, scope, job_kind, expected_generation, Some(error)).await
    }

    /// Loads the durable health receipt for one fleet maintenance job.
    pub async fn load_execution_maintenance_checkpoint(
        &self,
        scope: ExecutionScope,
        job_kind: ExecutionMaintenanceJobKind,
    ) -> Result<Option<ExecutionMaintenanceCheckpoint>> {
        require_control_plane_scope(scope)?;
        let mut conn = scope.begin(&self.pool).await?;
        let row =
            sqlx::query("SELECT * FROM moa.execution_maintenance_checkpoint WHERE job_kind = $1")
                .bind(job_kind.as_str())
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let checkpoint = row
            .as_ref()
            .map(maintenance_checkpoint_from_row)
            .transpose()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(checkpoint)
    }

    /// Samples trigger/outbox health with a strict per-queue row-read bound.
    pub async fn sample_execution_queue_health(
        &self,
        scope: ExecutionScope,
        sample_limit: u32,
    ) -> Result<ExecutionQueueHealthSnapshot> {
        if sample_limit == 0 || sample_limit > MAX_HEALTH_SAMPLE_SIZE {
            return Err(Error::InvalidRepositoryInput {
                message: format!(
                    "execution queue health sample must be 1..={MAX_HEALTH_SAMPLE_SIZE}"
                ),
            });
        }
        let fetch_limit = i64::from(sample_limit) + 1;
        let mut conn = scope.begin(&self.pool).await?;
        let observed_at = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT now()")
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let due_triggers = sqlx::query_scalar::<_, DateTime<Utc>>(
            r#"
            SELECT due_at
            FROM moa.execution_trigger
            WHERE state = 'pending' AND due_at <= now()
            ORDER BY due_at, tenant_id, trigger_uid
            LIMIT $1
            "#,
        )
        .bind(fetch_limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let claimable_dispatches = sqlx::query_scalar::<_, DateTime<Utc>>(
            r#"
            SELECT claimable_at
            FROM (
                (SELECT not_before_at AS claimable_at, created_at, dispatch_uid
                 FROM moa.execution_dispatch_outbox
                 WHERE state = 'pending' AND not_before_at <= now()
                 ORDER BY not_before_at, created_at, dispatch_uid
                 LIMIT $1)
                UNION ALL
                (SELECT claim_expires_at AS claimable_at, created_at, dispatch_uid
                 FROM moa.execution_dispatch_outbox
                 WHERE state = 'dispatching' AND claim_expires_at <= now()
                 ORDER BY claim_expires_at, created_at, dispatch_uid
                 LIMIT $1)
            ) AS claimable
            ORDER BY claimable_at, created_at, dispatch_uid
            LIMIT $1
            "#,
        )
        .bind(fetch_limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let dead_letter_dispatches = sqlx::query_scalar::<_, DateTime<Utc>>(
            r#"
            SELECT created_at
            FROM moa.execution_dispatch_outbox
            WHERE state = 'dead_letter'
            ORDER BY created_at, tenant_id, dispatch_uid
            LIMIT $1
            "#,
        )
        .bind(fetch_limit)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        // The exact-deadline invariant guard. Counting is capped rather than exhaustive
        // because the alert only distinguishes zero from nonzero, so the cap bounds the
        // work without changing the answer. `execution_run_overdue_deadline_idx` leads on
        // `budget_deadline_at` and carries this exact status predicate, so the capped scan
        // is index-only.
        let overdue_deadlines = sqlx::query_scalar::<_, i32>(
            r#"
            SELECT count(*)::INTEGER
            FROM (
                SELECT 1
                FROM moa.execution_run
                WHERE status IN (
                    'awaiting_confirmation', 'queued', 'running', 'waiting_input',
                    'waiting_review', 'waiting_signal', 'waiting_timer',
                    'waiting_external', 'waiting_replan', 'pause_requested',
                    'pausing', 'paused', 'compensating'
                )
                  AND budget_deadline_at IS NOT NULL
                  AND budget_deadline_suspended_at IS NULL
                  AND budget_deadline_at <= now()
                LIMIT $1
            ) AS overdue
            "#,
        )
        .bind(i64::from(sample_limit))
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        // Compensation attempts hold the same active-compute reservation as forward attempts,
        // so a stuck rollback must not be invisible to the stuck-attempt alert. Each branch
        // takes its own ordered minimum so the aggregate reduces two rows, not two scans.
        let oldest_active_attempt_at = sqlx::query_scalar::<_, Option<DateTime<Utc>>>(
            r#"
            SELECT min(attempt_started_at)
            FROM (
                (SELECT attempt_started_at
                 FROM moa.execution_task
                 WHERE status = 'running' AND attempt_state = 'running'
                   AND attempt_started_at IS NOT NULL
                 ORDER BY attempt_started_at
                 LIMIT 1)
                UNION ALL
                (SELECT attempt_started_at
                 FROM moa.execution_compensation
                 WHERE status = 'running' AND attempt_state = 'running'
                   AND attempt_started_at IS NOT NULL
                 ORDER BY attempt_started_at
                 LIMIT 1)
            ) AS active_attempt
            "#,
        )
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        // The per-phase census of the live fleet. This is the one aggregate here that is not
        // capped: a census that stops counting reports a number no dashboard can sum, and the
        // partial nonterminal index bounds the scan to live work regardless of run history.
        let run_phase_rows = sqlx::query_as::<_, (String, i64)>(RUN_PHASE_CENSUS_SQL)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        // The oldest live asynchronous job, including `unbound` jobs that were never handed to
        // a provider. `execution_external_job_reconcile_idx` cannot serve this: it excludes
        // `unbound` and rows with no `next_reconcile_at`, which is exactly where a job that
        // never started is stranded. `execution_external_job_live_age_idx` leads on `created_at`
        // and carries this state predicate, so the ordered lookup reads one index tuple.
        let oldest_external_job_at = sqlx::query_scalar::<_, DateTime<Utc>>(
            r#"
            SELECT created_at
            FROM moa.execution_external_job
            WHERE state IN (
                'unbound', 'starting', 'running', 'waiting_reconcile', 'cancel_requested'
            )
            ORDER BY created_at, external_job_uid
            LIMIT 1
            "#,
        )
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        // One aggregate over the bounded capacity ledger. Every ratio is reduced inside the
        // database, so no tenant identifier reaches the metric layer. The share denominator is
        // the tenant total rather than the fleet bucket's counter so the ratio stays within
        // [0, 1] by construction instead of depending on cross-scope bookkeeping agreeing.
        let utilization_rows = sqlx::query_as::<_, (String, f64, f64, f64)>(
            r#"
            SELECT resource_dimension,
                   COALESCE(
                       max(reserved_quantity::DOUBLE PRECISION
                           / limit_value::DOUBLE PRECISION)
                       FILTER (WHERE scope_kind = 'fleet'), 0.0
                   ) AS fleet_ratio,
                   COALESCE(
                       max(reserved_quantity::DOUBLE PRECISION
                           / limit_value::DOUBLE PRECISION)
                       FILTER (WHERE scope_kind = 'tenant'), 0.0
                   ) AS tenant_peak_ratio,
                   COALESCE(
                       max(reserved_quantity)
                       FILTER (WHERE scope_kind = 'tenant')::DOUBLE PRECISION
                       / NULLIF(
                           sum(reserved_quantity) FILTER (WHERE scope_kind = 'tenant'), 0
                       )::DOUBLE PRECISION,
                       0.0
                   ) AS tenant_max_share_ratio
            FROM moa.execution_capacity_bucket
            GROUP BY resource_dimension
            "#,
        )
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionQueueHealthSnapshot {
            observed_at,
            due_triggers: backlog_sample(due_triggers, sample_limit),
            claimable_dispatches: backlog_sample(claimable_dispatches, sample_limit),
            dead_letter_dispatches: backlog_sample(dead_letter_dispatches, sample_limit),
            overdue_deadlines: to_u32(overdue_deadlines, "overdue execution deadlines")?,
            oldest_active_attempt_at,
            run_phases: run_phase_samples(&run_phase_rows)?,
            oldest_external_job_at,
            admission_utilization: admission_utilization_samples(&utilization_rows)?,
        })
    }

    /// Enqueues one dispatch in its own scoped transaction.
    pub async fn enqueue_dispatch(
        &self,
        scope: ExecutionScope,
        request: NewExecutionDispatch,
    ) -> Result<ExecutionDispatchRecord> {
        let mut conn = scope.begin(&self.pool).await?;
        let record = enqueue_dispatch_in_conn(conn.as_mut(), &request).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// Claims a bounded due batch, including claims abandoned past their expiry.
    ///
    /// Each eligibility branch is ordered and capped on its own partial index — pending rows
    /// by `not_before_at`, abandoned claims by `claim_expires_at` — so neither branch reads
    /// beyond one bounded index window. Only the merged head is then locked.
    pub async fn claim_due_dispatches(
        &self,
        scope: ExecutionScope,
        claim_owner: &str,
        batch_size: u32,
        claim_ttl: Duration,
    ) -> Result<Vec<ExecutionDispatchRecord>> {
        validate_claim_request(claim_owner, batch_size, claim_ttl)?;
        let claim_ttl_seconds = duration_seconds_ceil(claim_ttl, "dispatch claim TTL")?;
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            WITH head AS (
                SELECT dispatch_uid
                FROM (
                    (SELECT dispatch_uid, not_before_at AS claimable_at, created_at
                     FROM moa.execution_dispatch_outbox
                     WHERE state = 'pending' AND not_before_at <= now()
                     ORDER BY not_before_at, created_at, dispatch_uid
                     LIMIT $1)
                    UNION ALL
                    (SELECT dispatch_uid, claim_expires_at AS claimable_at, created_at
                     FROM moa.execution_dispatch_outbox
                     WHERE state = 'dispatching' AND claim_expires_at <= now()
                     ORDER BY claim_expires_at, created_at, dispatch_uid
                     LIMIT $1)
                ) AS candidate
                ORDER BY claimable_at, created_at, dispatch_uid
                LIMIT $1
            ),
            claimable AS (
                SELECT claimed.dispatch_uid
                FROM moa.execution_dispatch_outbox AS claimed
                JOIN head ON head.dispatch_uid = claimed.dispatch_uid
                WHERE (
                        claimed.state = 'pending'
                    AND claimed.not_before_at <= now()
                ) OR (
                        claimed.state = 'dispatching'
                    AND claimed.claim_expires_at <= now()
                )
                FOR UPDATE OF claimed SKIP LOCKED
            )
            UPDATE moa.execution_dispatch_outbox AS dispatch
            SET state = 'dispatching',
                claim_owner = $2,
                claimed_at = now(),
                claim_expires_at = now() + make_interval(secs => $3),
                delivery_attempts = dispatch.delivery_attempts + 1,
                updated_at = now()
            FROM claimable
            WHERE dispatch.dispatch_uid = claimable.dispatch_uid
            RETURNING dispatch.*
            "#,
        )
        .bind(i64::from(batch_size))
        .bind(claim_owner)
        .bind(claim_ttl_seconds)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let records = rows.iter().map(dispatch_from_row).collect::<Result<_>>()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(records)
    }

    /// Returns the earliest pending outbox deadline through the bounded queue index.
    pub async fn next_pending_dispatch_wake(
        &self,
        scope: ExecutionScope,
    ) -> Result<ExecutionDispatchWake> {
        let mut conn = scope.begin(&self.pool).await?;
        let (observed_at, dispatch_uid, next_due_at, head_updated_at) = sqlx::query_as::<
            _,
            (
                DateTime<Utc>,
                Option<Uuid>,
                Option<DateTime<Utc>>,
                Option<DateTime<Utc>>,
            ),
        >(
            r#"
            SELECT now(), head.dispatch_uid, head.not_before_at, head.updated_at
            FROM (SELECT 1) AS singleton
            LEFT JOIN LATERAL (
                SELECT candidate.dispatch_uid, candidate.not_before_at, candidate.updated_at
                FROM (
                    (SELECT dispatch_uid, not_before_at, updated_at, created_at
                     FROM moa.execution_dispatch_outbox
                     WHERE state = 'pending'
                     ORDER BY not_before_at, created_at, dispatch_uid
                     LIMIT 1)
                    UNION ALL
                    (SELECT dispatch_uid, claim_expires_at AS not_before_at, updated_at, created_at
                     FROM moa.execution_dispatch_outbox
                     WHERE state = 'dispatching' AND claim_expires_at IS NOT NULL
                     ORDER BY claim_expires_at, created_at, dispatch_uid
                     LIMIT 1)
                ) AS candidate
                ORDER BY candidate.not_before_at, candidate.created_at, candidate.dispatch_uid
                LIMIT 1
            ) AS head ON TRUE
            "#,
        )
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionDispatchWake {
            observed_at,
            dispatch_uid,
            next_due_at,
            head_updated_at,
        })
    }

    /// Acknowledges a bounded delivery batch only for the exact current claim owner.
    ///
    /// Returned identities retain request order. Missing identities were absent, terminal, or
    /// changed claim owner before this acknowledgement committed.
    pub async fn mark_dispatches_delivered(
        &self,
        scope: ExecutionScope,
        dispatch_uids: &[Uuid],
        claim_owner: &str,
    ) -> Result<Vec<Uuid>> {
        validate_claim_owner(claim_owner)?;
        validate_ack_batch(dispatch_uids)?;
        let mut conn = scope.begin(&self.pool).await?;
        let applied = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE moa.execution_dispatch_outbox
            SET state = 'delivered', delivered_at = now(), claim_owner = NULL,
                claimed_at = NULL, claim_expires_at = NULL, last_error = NULL,
                updated_at = now()
            WHERE dispatch_uid = ANY($1::UUID[])
              AND state = 'dispatching' AND claim_owner = $2
            RETURNING dispatch_uid
            "#,
        )
        .bind(dispatch_uids)
        .bind(claim_owner)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        let applied = applied.into_iter().collect::<HashSet<_>>();
        Ok(dispatch_uids
            .iter()
            .copied()
            .filter(|dispatch_uid| applied.contains(dispatch_uid))
            .collect())
    }

    /// Releases one exact failed claim behind backoff or moves it to dead letter.
    pub async fn record_dispatch_failure(
        &self,
        scope: ExecutionScope,
        dispatch_uid: Uuid,
        claim_owner: &str,
        error: &str,
        retry: ExecutionDispatchRetryPolicy,
    ) -> Result<ExecutionDispatchFailureOutcome> {
        validate_claim_owner(claim_owner)?;
        retry.validate()?;
        let mut conn = scope.begin(&self.pool).await?;
        let dispatch = sqlx::query(
            r#"
            SELECT *
            FROM moa.execution_dispatch_outbox
            WHERE dispatch_uid = $1 AND state = 'dispatching' AND claim_owner = $2
            FOR UPDATE
            "#,
        )
        .bind(dispatch_uid)
        .bind(claim_owner)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(dispatch) = dispatch else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionDispatchFailureOutcome::StaleClaim);
        };
        let dispatch = dispatch_from_row(&dispatch)?;
        let attempts = dispatch.delivery_attempts;
        let dispatch_kind = dispatch.kind;
        let last_error = error.chars().take(MAX_ERROR_CHARS).collect::<String>();
        let requires_durable_retry = dispatch_requires_durable_retry(dispatch_kind);
        let outcome = if attempts >= retry.max_attempts && !requires_durable_retry {
            let owner_repaired = if dispatch_kind == ExecutionDispatchKind::TaskAttempt {
                repair_dead_lettered_task_dispatch_in_conn(&mut conn, &dispatch).await?
            } else if dispatch_kind == ExecutionDispatchKind::CompensationAttempt {
                repair_dead_lettered_compensation_dispatch_in_conn(&mut conn, &dispatch).await?
            } else {
                true
            };
            sqlx::query(
                r#"
                UPDATE moa.execution_dispatch_outbox
                SET state = 'dead_letter', claim_owner = NULL, claimed_at = NULL,
                    claim_expires_at = NULL, last_error = $3, updated_at = now()
                WHERE dispatch_uid = $1 AND state = 'dispatching' AND claim_owner = $2
                "#,
            )
            .bind(dispatch_uid)
            .bind(claim_owner)
            .bind(&last_error)
            .execute(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if owner_repaired {
                ExecutionDispatchFailureOutcome::DeadLettered
            } else {
                ExecutionDispatchFailureOutcome::DeadLetteredWithoutOwnerRepair
            }
        } else {
            let delay = if attempts >= retry.max_attempts {
                retry.maximum_delay
            } else {
                retry.retry_delay(attempts)
            };
            let delay_seconds = duration_seconds_ceil(delay, "dispatch retry delay")?;
            let not_before_at = sqlx::query_scalar::<_, DateTime<Utc>>(
                r#"
                UPDATE moa.execution_dispatch_outbox
                SET state = 'pending', not_before_at = now() + make_interval(secs => $3),
                    claim_owner = NULL, claimed_at = NULL, claim_expires_at = NULL,
                    last_error = $4, updated_at = now()
                WHERE dispatch_uid = $1 AND state = 'dispatching' AND claim_owner = $2
                RETURNING not_before_at
                "#,
            )
            .bind(dispatch_uid)
            .bind(claim_owner)
            .bind(delay_seconds)
            .bind(last_error)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            ExecutionDispatchFailureOutcome::RetryScheduled { not_before_at }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }
}

fn dispatch_requires_durable_retry(kind: ExecutionDispatchKind) -> bool {
    match kind {
        ExecutionDispatchKind::RunActivation
        | ExecutionDispatchKind::TaskAttemptCancel
        | ExecutionDispatchKind::CompensationAttemptCancel
        | ExecutionDispatchKind::TriggerDelivery
        | ExecutionDispatchKind::ExternalCancel => true,
        ExecutionDispatchKind::TaskAttempt | ExecutionDispatchKind::CompensationAttempt => false,
    }
}

/// Settles the never-started task attempt owning one dead-lettered dispatch.
///
/// Returns whether the exact `Dispatching` owner was repaired. A `false` result means the
/// owner was already settled by another writer, which must not block the dead letter.
async fn repair_dead_lettered_task_dispatch_in_conn(
    conn: &mut super::ScopedConn<'_>,
    dispatch: &ExecutionDispatchRecord,
) -> Result<bool> {
    let request = serde_json::from_value::<ExecutionTaskAttemptRequest>(dispatch.payload.clone())
        .map_err(|error| Error::InvalidRepositoryData {
        message: format!("invalid dead-letter task-attempt payload: {error}"),
    })?;
    if request.dispatch_uid != dispatch.dispatch_uid
        || request.tenant_id != dispatch.tenant_id
        || Some(request.run_uid) != dispatch.run_uid
        || Some(request.task_id.as_uuid()) != dispatch.task_id
        || Some(request.controller_generation) != dispatch.controller_generation
        || Some(request.attempt_generation) != dispatch.attempt_generation
    {
        return Err(Error::InvalidRepositoryData {
            message: "dead-letter task-attempt payload lost immutable outbox fences".to_string(),
        });
    }
    let settled_at = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT now()")
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let outcome = super::task::settle_unstarted_task_attempt_in_conn(
        conn,
        TaskAttemptFence {
            tenant_id: request.tenant_id,
            run_uid: request.run_uid,
            task_id: request.task_id,
            controller_generation: request.controller_generation,
            attempt_generation: request.attempt_generation,
            dispatch_uid: request.dispatch_uid,
            capacity_reservation_uid: request.capacity_reservation_uid,
            watchdog_trigger_uid: request.watchdog_trigger_uid,
            attempt_deadline_at: request.attempt_deadline_at,
        },
        settled_at,
    )
    .await?;
    Ok(match outcome {
        TaskAttemptSettlementOutcome::Applied { .. }
        | TaskAttemptSettlementOutcome::Replayed { .. } => true,
        // The owner is already gone, superseded, or settled by another writer, so there is
        // nothing left to repair. Dead-lettering must still commit: refusing here would
        // re-poison every drain that reclaims this row and stop the fleet's only pump.
        TaskAttemptSettlementOutcome::NotFound
        | TaskAttemptSettlementOutcome::Stale
        | TaskAttemptSettlementOutcome::InvalidState => false,
    })
}

/// Settles the never-started compensation attempt owning one dead-lettered dispatch.
///
/// Returns whether the exact `Dispatching` owner was repaired; see the task twin.
async fn repair_dead_lettered_compensation_dispatch_in_conn(
    conn: &mut super::ScopedConn<'_>,
    dispatch: &ExecutionDispatchRecord,
) -> Result<bool> {
    let request =
        serde_json::from_value::<ExecutionCompensationAttemptRequest>(dispatch.payload.clone())
            .map_err(|error| Error::InvalidRepositoryData {
                message: format!("invalid dead-letter compensation-attempt payload: {error}"),
            })?;
    if request.dispatch_uid != dispatch.dispatch_uid
        || request.tenant_id != dispatch.tenant_id
        || Some(request.run_uid) != dispatch.run_uid
        || Some(request.compensation_id.as_uuid()) != dispatch.compensation_id
        || Some(request.controller_generation) != dispatch.controller_generation
        || Some(request.compensation_generation) != dispatch.compensation_generation
        || Some(request.compensation_attempt_generation) != dispatch.compensation_attempt_generation
    {
        return Err(Error::InvalidRepositoryData {
            message: "dead-letter compensation-attempt payload lost immutable outbox fences"
                .to_string(),
        });
    }
    let settled_at = sqlx::query_scalar::<_, DateTime<Utc>>("SELECT now()")
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    let outcome = super::compensation::settle_unstarted_compensation_attempt_in_conn(
        conn, &request, settled_at,
    )
    .await?;
    Ok(match outcome {
        super::compensation::CompensationAttemptWriteOutcome::Applied(_)
        | super::compensation::CompensationAttemptWriteOutcome::Replayed(_) => true,
        // See the task twin: an already-settled owner must not block the dead letter itself.
        super::compensation::CompensationAttemptWriteOutcome::Conflict
        | super::compensation::CompensationAttemptWriteOutcome::NotFound => false,
    })
}

async fn settle_execution_maintenance(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    job_kind: ExecutionMaintenanceJobKind,
    expected_generation: u64,
    error: Option<String>,
) -> Result<ExecutionMaintenanceSettlementOutcome> {
    require_control_plane_scope(scope)?;
    if expected_generation == 0 {
        return Err(Error::InvalidRepositoryInput {
            message: "execution maintenance generation must be positive".to_string(),
        });
    }
    let expected_generation =
        i64::try_from(expected_generation).map_err(|_| Error::InvalidRepositoryInput {
            message: "execution maintenance generation exceeds PostgreSQL BIGINT".to_string(),
        })?;
    let mut conn = scope.begin(&repository.pool).await?;
    let row = if let Some(error) = error {
        sqlx::query(
            r#"
            UPDATE moa.execution_maintenance_checkpoint
            SET last_failure_at = now(), last_error = $3, updated_at = now()
            WHERE job_kind = $1 AND generation = $2
            RETURNING *
            "#,
        )
        .bind(job_kind.as_str())
        .bind(expected_generation)
        .bind(error)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    } else {
        sqlx::query(
            r#"
            UPDATE moa.execution_maintenance_checkpoint
            SET last_succeeded_at = now(), updated_at = now()
            WHERE job_kind = $1 AND generation = $2
            RETURNING *
            "#,
        )
        .bind(job_kind.as_str())
        .bind(expected_generation)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    };
    let outcome = row
        .as_ref()
        .map(maintenance_checkpoint_from_row)
        .transpose()?
        .map_or(
            ExecutionMaintenanceSettlementOutcome::StaleOrMissing,
            ExecutionMaintenanceSettlementOutcome::Applied,
        );
    conn.commit().await.map_err(storage_error)?;
    Ok(outcome)
}

fn require_control_plane_scope(scope: ExecutionScope) -> Result<()> {
    if scope != ExecutionScope::ControlPlane {
        return Err(Error::InvalidRepositoryInput {
            message: "execution maintenance checkpoints require control-plane scope".to_string(),
        });
    }
    Ok(())
}

fn bounded_maintenance_error(error: &str) -> Result<String> {
    let error = error.trim();
    if error.is_empty() {
        return Err(Error::InvalidRepositoryInput {
            message: "execution maintenance failure requires a non-empty error".to_string(),
        });
    }
    let mut end = error.len().min(MAX_MAINTENANCE_ERROR_BYTES);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    Ok(error[..end].to_string())
}

fn maintenance_checkpoint_from_row(
    row: &sqlx::postgres::PgRow,
) -> Result<ExecutionMaintenanceCheckpoint> {
    let generation = row
        .try_get::<i64, _>("generation")
        .map_err(super::row_error)?;
    Ok(ExecutionMaintenanceCheckpoint {
        job_kind: row
            .try_get::<String, _>("job_kind")
            .map_err(super::row_error)?
            .parse()?,
        generation: super::to_u64(generation, "execution maintenance generation")?,
        last_started_at: row.try_get("last_started_at").map_err(super::row_error)?,
        last_succeeded_at: row.try_get("last_succeeded_at").map_err(super::row_error)?,
        last_failure_at: row.try_get("last_failure_at").map_err(super::row_error)?,
        last_error: row.try_get("last_error").map_err(super::row_error)?,
        updated_at: row.try_get("updated_at").map_err(super::row_error)?,
    })
}

/// Expands observed ledger rows to every bounded dimension, defaulting absent ones to idle.
///
/// A dimension with no bucket row has never been reserved. It must still be reported, or the
/// gauge would be absent on a quiet fleet and its `absent()` alert would fire.
fn admission_utilization_samples(
    rows: &[(String, f64, f64, f64)],
) -> Result<Vec<ExecutionAdmissionUtilizationSample>> {
    for (resource, _, _, _) in rows {
        resource.parse::<ExecutionAdmissionResourceDimension>()?;
    }
    Ok(ExecutionAdmissionResourceDimension::ALL
        .into_iter()
        .map(|resource| {
            let observed = rows
                .iter()
                .find(|(label, _, _, _)| label == resource.as_str());
            ExecutionAdmissionUtilizationSample {
                resource,
                fleet_ratio: observed.map_or(0.0, |(_, fleet, _, _)| *fleet),
                tenant_peak_ratio: observed.map_or(0.0, |(_, _, tenant_peak, _)| *tenant_peak),
                tenant_max_share_ratio: observed.map_or(0.0, |(_, _, _, share)| *share),
            }
        })
        .collect())
}

/// Expands observed census rows to every bounded phase, defaulting absent ones to zero.
///
/// `GROUP BY status` returns no row for a phase holding no runs, so an idle phase would
/// leave its gauge unwritten and its `absent()` alert would fire on a quiet fleet. An
/// unmodelled durable status fails the whole snapshot rather than silently vanishing from
/// a census whose sum is supposed to equal the live nonterminal fleet.
fn run_phase_samples(rows: &[(String, i64)]) -> Result<Vec<ExecutionRunPhaseSample>> {
    for (status, _) in rows {
        status.parse::<ExecutionRunPhaseDimension>()?;
    }
    ExecutionRunPhaseDimension::ALL
        .into_iter()
        .map(|phase| {
            let observed = rows.iter().find(|(label, _)| label == phase.as_str());
            let run_count = match observed {
                Some((_, count)) => super::to_u64(*count, "execution run phase count")?,
                None => 0,
            };
            Ok(ExecutionRunPhaseSample { phase, run_count })
        })
        .collect()
}

fn backlog_sample(
    mut timestamps: Vec<DateTime<Utc>>,
    sample_limit: u32,
) -> ExecutionQueueBacklogSample {
    let saturated = timestamps.len() > sample_limit as usize;
    timestamps.truncate(sample_limit as usize);
    ExecutionQueueBacklogSample {
        oldest_at: timestamps.first().copied(),
        observed_count: u32::try_from(timestamps.len()).unwrap_or(sample_limit),
        saturated,
    }
}

/// Inserts one durable dispatch without committing the caller-owned transaction.
pub async fn enqueue_dispatch_in_conn(
    conn: &mut PgConnection,
    request: &NewExecutionDispatch,
) -> Result<ExecutionDispatchRecord> {
    validate_dispatch(request)?;
    let controller_generation =
        to_optional_i64(request.controller_generation, "controller generation")?;
    let wake_epoch = to_optional_i64(request.wake_epoch, "wake epoch")?;
    let attempt_generation = to_optional_i64(request.attempt_generation, "attempt generation")?;
    let compensation_generation =
        to_optional_i64(request.compensation_generation, "compensation generation")?;
    let compensation_attempt_generation = to_optional_i64(
        request.compensation_attempt_generation,
        "compensation attempt generation",
    )?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO moa.execution_dispatch_outbox (
            dispatch_uid, tenant_id, run_uid, task_id, compensation_id,
            trigger_uid, external_job_uid,
            dispatch_kind, controller_generation, wake_epoch, attempt_generation,
            compensation_generation, compensation_attempt_generation,
            not_before_at, payload
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15
        )
        ON CONFLICT (dispatch_uid) DO NOTHING
        RETURNING *
        "#,
    )
    .bind(request.dispatch_uid)
    .bind(request.tenant_id.0)
    .bind(request.run_uid)
    .bind(request.task_id)
    .bind(request.compensation_id)
    .bind(request.trigger_uid)
    .bind(request.external_job_uid)
    .bind(request.kind.as_str())
    .bind(controller_generation)
    .bind(wake_epoch)
    .bind(attempt_generation)
    .bind(compensation_generation)
    .bind(compensation_attempt_generation)
    .bind(request.not_before_at)
    .bind(&request.payload)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let row = match inserted {
        Some(row) => row,
        None => sqlx::query("SELECT * FROM moa.execution_dispatch_outbox WHERE dispatch_uid = $1")
            .bind(request.dispatch_uid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_error)?
            .ok_or_else(|| Error::Storage {
                message: "dispatch insert conflicted without a visible replay row".to_string(),
            })?,
    };
    let record = dispatch_from_row(&row)?;
    if !dispatch_matches_request(&record, request) {
        return Err(Error::InvalidRepositoryInput {
            message: "dispatch UID is already bound to different immutable semantics".to_string(),
        });
    }
    Ok(record)
}

/// Requeues one previously accepted dispatch without changing its immutable identity.
///
/// The caller must first establish the authoritative generation fence while holding the
/// corresponding trigger row lock. A non-delivered replay is left untouched. The repair
/// epoch advances so redelivery cannot attach to the memoized completed invocation the
/// previous delivery identity already produced.
pub(super) async fn requeue_delivered_dispatch_in_conn(
    conn: &mut PgConnection,
    request: &NewExecutionDispatch,
) -> Result<Option<ExecutionDispatchRecord>> {
    validate_dispatch(request)?;
    let row = sqlx::query(
        r#"
        UPDATE moa.execution_dispatch_outbox
        SET state = 'pending', delivered_at = NULL, delivery_attempts = 0,
            repair_epoch = repair_epoch + 1,
            claim_owner = NULL, claimed_at = NULL, claim_expires_at = NULL,
            last_error = NULL, updated_at = now()
        WHERE dispatch_uid = $1 AND state = 'delivered'
        RETURNING *
        "#,
    )
    .bind(request.dispatch_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let record = dispatch_from_row(&row)?;
    if !dispatch_matches_request(&record, request) {
        return Err(Error::InvalidRepositoryData {
            message: "delivered dispatch replay no longer matches its immutable intent".to_string(),
        });
    }
    Ok(Some(record))
}

/// Requeues a bounded page of accepted run activations whose authoritative wake remains queued.
///
/// The repair epoch advances so the replacement activation carries a delivery identity the
/// original completed controller invocation cannot memoize.
pub(super) async fn requeue_current_run_activations_in_conn(
    conn: &mut PgConnection,
    batch_size: u32,
    grace_seconds: i64,
) -> Result<Vec<ExecutionDispatchRecord>> {
    if batch_size == 0 {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT dispatch.dispatch_uid
            FROM moa.execution_run AS run
            JOIN moa.execution_dispatch_outbox AS dispatch
              ON dispatch.tenant_id = run.tenant_id
             AND dispatch.run_uid = run.run_uid
             AND dispatch.controller_generation = run.controller_generation
             AND dispatch.wake_epoch = run.wake_epoch
             AND dispatch.dispatch_kind = 'run_activation'
            WHERE run.activation_state = 'queued'
              AND run.processed_wake_epoch < run.wake_epoch
              AND run.status NOT IN (
                  'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
              )
              AND dispatch.state = 'delivered'
              AND dispatch.delivered_at <= now() - make_interval(secs => $2)
            ORDER BY run.updated_at, run.run_uid
            LIMIT $1
            FOR UPDATE OF dispatch SKIP LOCKED
        )
        UPDATE moa.execution_dispatch_outbox AS dispatch
        SET state = 'pending', delivered_at = NULL, delivery_attempts = 0,
            repair_epoch = dispatch.repair_epoch + 1,
            claim_owner = NULL, claimed_at = NULL, claim_expires_at = NULL,
            last_error = NULL, updated_at = now()
        FROM candidates
        WHERE dispatch.dispatch_uid = candidates.dispatch_uid
          AND dispatch.state = 'delivered'
        RETURNING dispatch.*
        "#,
    )
    .bind(i64::from(batch_size))
    .bind(grace_seconds)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    rows.iter().map(dispatch_from_row).collect()
}

/// Requeues accepted deliveries only while their exact bounded work has not started.
///
/// A running attempt is deliberately excluded: once effects may have begun, its
/// watchdog owns ambiguity resolution and the dispatcher must not replay it. The repair
/// epoch advances so redelivery carries an identity Restate cannot memoize.
///
/// That fully repairs the two cancellation kinds and `external_cancel`, whose targets are
/// addressed by idempotency key alone. The two attempt kinds split by what Restate still
/// holds, because a workflow `run` handler is retained under its *workflow key* — the bare
/// dispatch UID, which `require_dispatch_key` pins — independently of any idempotency key:
///
/// - Total Restate state loss, the case this grace window exists for, leaves the workflow
///   key uninvoked, so redelivery starts the attempt normally.
/// - A key for which Restate still holds a completed `run` cannot be restarted under any
///   delivery identity. That attempt stays parked until its `attempt_deadline_at` watchdog
///   settles it — the same backstop a lost running attempt already relies on.
pub(super) async fn requeue_current_accepted_dispatches_in_conn(
    conn: &mut PgConnection,
    batch_size: u32,
    grace_seconds: i64,
) -> Result<Vec<ExecutionDispatchRecord>> {
    if batch_size == 0 {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        r#"
        WITH candidates AS (
            SELECT dispatch.dispatch_uid
            FROM moa.execution_dispatch_outbox AS dispatch
            WHERE dispatch.state='delivered'
              AND dispatch.delivered_at <= now() - make_interval(secs => $2)
              AND (
                (
                  dispatch.dispatch_kind='task_attempt'
                  AND EXISTS (
                    SELECT 1
                    FROM moa.execution_task AS task
                    JOIN moa.execution_run AS run
                      ON run.tenant_id=task.tenant_id AND run.run_uid=task.run_uid
                    WHERE task.tenant_id=dispatch.tenant_id
                      AND task.run_uid=dispatch.run_uid
                      AND task.task_id=dispatch.task_id
                      AND task.attempt_generation=dispatch.attempt_generation
                      AND task.active_dispatch_uid=dispatch.dispatch_uid
                      AND task.status='dispatching' AND task.attempt_state='dispatching'
                      AND run.controller_generation=dispatch.controller_generation
                  )
                ) OR (
                  dispatch.dispatch_kind='compensation_attempt'
                  AND EXISTS (
                    SELECT 1
                    FROM moa.execution_compensation AS compensation
                    JOIN moa.execution_run AS run
                      ON run.tenant_id=compensation.tenant_id
                     AND run.run_uid=compensation.run_uid
                    WHERE compensation.tenant_id=dispatch.tenant_id
                      AND compensation.run_uid=dispatch.run_uid
                      AND compensation.compensation_id=dispatch.compensation_id
                      AND compensation.generation=dispatch.compensation_generation
                      AND compensation.attempt_generation=dispatch.compensation_attempt_generation
                      AND compensation.active_dispatch_uid=dispatch.dispatch_uid
                      AND compensation.status='running'
                      AND compensation.attempt_state='dispatching'
                      AND run.controller_generation=dispatch.controller_generation
                  )
                ) OR (
                  dispatch.dispatch_kind='task_attempt_cancel'
                  AND EXISTS (
                    SELECT 1
                    FROM moa.execution_task AS task
                    JOIN moa.execution_run AS run
                      ON run.tenant_id=task.tenant_id AND run.run_uid=task.run_uid
                    WHERE task.tenant_id=dispatch.tenant_id
                      AND task.run_uid=dispatch.run_uid
                      AND task.task_id=dispatch.task_id
                      AND task.attempt_generation=dispatch.attempt_generation
                      AND task.attempt_state='cancelling'
                      AND task.active_dispatch_uid::text=dispatch.payload->>'active_dispatch_uid'
                      AND task.generation::text=dispatch.payload->>'task_generation'
                      AND run.controller_generation=dispatch.controller_generation
                  )
                ) OR (
                  dispatch.dispatch_kind='compensation_attempt_cancel'
                  AND EXISTS (
                    SELECT 1
                    FROM moa.execution_compensation AS compensation
                    JOIN moa.execution_run AS run
                      ON run.tenant_id=compensation.tenant_id
                     AND run.run_uid=compensation.run_uid
                    WHERE compensation.tenant_id=dispatch.tenant_id
                      AND compensation.run_uid=dispatch.run_uid
                      AND compensation.compensation_id=dispatch.compensation_id
                      AND compensation.generation=dispatch.compensation_generation
                      AND compensation.attempt_generation=dispatch.compensation_attempt_generation
                      AND compensation.attempt_state='cancelling'
                      AND compensation.active_dispatch_uid::text
                          =dispatch.payload->>'active_dispatch_uid'
                      AND run.controller_generation=dispatch.controller_generation
                  )
                ) OR (
                  dispatch.dispatch_kind='external_cancel'
                  AND EXISTS (
                    SELECT 1
                    FROM moa.execution_external_job AS job
                    JOIN moa.execution_run AS run
                      ON run.tenant_id=job.tenant_id AND run.run_uid=job.run_uid
                    WHERE job.tenant_id=dispatch.tenant_id
                      AND job.run_uid=dispatch.run_uid
                      AND job.external_job_uid=dispatch.external_job_uid
                      AND job.task_id IS NOT DISTINCT FROM dispatch.task_id
                      AND job.attempt_generation IS NOT DISTINCT FROM dispatch.attempt_generation
                      AND job.compensation_id IS NOT DISTINCT FROM dispatch.compensation_id
                      AND job.compensation_generation IS NOT DISTINCT FROM dispatch.compensation_generation
                      AND job.compensation_attempt_generation
                          IS NOT DISTINCT FROM dispatch.compensation_attempt_generation
                      AND job.job_generation::text=dispatch.payload->>'job_generation'
                      AND job.provider=dispatch.payload->>'provider'
                      AND job.provider_job_id=dispatch.payload->>'provider_job_id'
                      AND job.idempotency_key=dispatch.payload->>'idempotency_key'
                      AND job.state='cancel_requested'
                      AND run.controller_generation=dispatch.controller_generation
                  )
                )
              )
            ORDER BY dispatch.delivered_at, dispatch.tenant_id, dispatch.dispatch_uid
            LIMIT $1
            FOR UPDATE OF dispatch SKIP LOCKED
        )
        UPDATE moa.execution_dispatch_outbox AS dispatch
        SET state='pending', delivered_at=NULL, delivery_attempts=0,
            repair_epoch=dispatch.repair_epoch + 1,
            claim_owner=NULL, claimed_at=NULL, claim_expires_at=NULL,
            last_error=NULL, updated_at=now()
        FROM candidates
        WHERE dispatch.dispatch_uid=candidates.dispatch_uid
          AND dispatch.state='delivered'
        RETURNING dispatch.*
        "#,
    )
    .bind(i64::from(batch_size))
    .bind(grace_seconds)
    .fetch_all(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    rows.iter().map(dispatch_from_row).collect()
}

fn validate_dispatch(request: &NewExecutionDispatch) -> Result<()> {
    let generation_is_zero = [
        request.controller_generation,
        request.wake_epoch,
        request.attempt_generation,
        request.compensation_generation,
        request.compensation_attempt_generation,
    ]
    .into_iter()
    .flatten()
    .any(|generation| generation == 0);
    if request.dispatch_uid.is_nil() || !request.payload.is_object() || generation_is_zero {
        return Err(Error::InvalidRepositoryInput {
            message: "execution dispatch requires a non-nil UID, positive generations, and object payload"
                .to_string(),
        });
    }
    let shape_is_valid = match request.kind {
        ExecutionDispatchKind::RunActivation => {
            request.run_uid.is_some()
                && request.task_id.is_none()
                && request.compensation_id.is_none()
                && request.trigger_uid.is_none()
                && request.external_job_uid.is_none()
                && request.controller_generation.is_some()
                && request.wake_epoch.is_some()
                && request.attempt_generation.is_none()
                && request.compensation_generation.is_none()
                && request.compensation_attempt_generation.is_none()
        }
        ExecutionDispatchKind::TaskAttempt => {
            request.run_uid.is_some()
                && request.task_id.is_some()
                && request.compensation_id.is_none()
                && request.trigger_uid.is_none()
                && request.external_job_uid.is_none()
                && request.controller_generation.is_some()
                && request.wake_epoch.is_none()
                && request.attempt_generation.is_some()
                && request.compensation_generation.is_none()
                && request.compensation_attempt_generation.is_none()
        }
        ExecutionDispatchKind::TaskAttemptCancel => {
            request.run_uid.is_some()
                && request.task_id.is_some()
                && request.compensation_id.is_none()
                && request.trigger_uid.is_none()
                && request.external_job_uid.is_none()
                && request.controller_generation.is_some()
                && request.wake_epoch.is_none()
                && request.attempt_generation.is_some()
                && request.compensation_generation.is_none()
                && request.compensation_attempt_generation.is_none()
                && task_cancel_payload_matches(request)
        }
        ExecutionDispatchKind::CompensationAttempt => {
            request.run_uid.is_some()
                && request.task_id.is_none()
                && request.compensation_id.is_some()
                && request.trigger_uid.is_none()
                && request.external_job_uid.is_none()
                && request.controller_generation.is_some()
                && request.wake_epoch.is_none()
                && request.attempt_generation.is_none()
                && request.compensation_generation.is_some()
                && request.compensation_attempt_generation.is_some()
        }
        ExecutionDispatchKind::CompensationAttemptCancel => {
            request.run_uid.is_some()
                && request.task_id.is_none()
                && request.compensation_id.is_some()
                && request.trigger_uid.is_none()
                && request.external_job_uid.is_none()
                && request.controller_generation.is_some()
                && request.wake_epoch.is_none()
                && request.attempt_generation.is_none()
                && request.compensation_generation.is_some()
                && request.compensation_attempt_generation.is_some()
                && compensation_cancel_payload_matches(request)
        }
        ExecutionDispatchKind::TriggerDelivery => {
            request.run_uid.is_none()
                && request.task_id.is_none()
                && request.compensation_id.is_none()
                && request.trigger_uid.is_some()
                && request.external_job_uid.is_none()
                && request.wake_epoch.is_none()
                && request.attempt_generation.is_none()
                && request.controller_generation.is_none()
                && request.compensation_generation.is_none()
                && request.compensation_attempt_generation.is_none()
        }
        ExecutionDispatchKind::ExternalCancel => {
            request.run_uid.is_some()
                && request.trigger_uid.is_none()
                && request.external_job_uid.is_some()
                && request.controller_generation.is_some()
                && request.wake_epoch.is_none()
                && ((request.task_id.is_some()
                    && request.compensation_id.is_none()
                    && request.attempt_generation.is_some()
                    && request.compensation_generation.is_none()
                    && request.compensation_attempt_generation.is_none())
                    || (request.task_id.is_none()
                        && request.compensation_id.is_some()
                        && request.attempt_generation.is_none()
                        && request.compensation_generation.is_some()
                        && request.compensation_attempt_generation.is_some()))
                && external_cancel_payload_matches(request)
        }
    };
    if !shape_is_valid {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "execution dispatch target shape does not match {}",
                request.kind.as_str()
            ),
        });
    }
    Ok(())
}

fn external_cancel_payload_matches(request: &NewExecutionDispatch) -> bool {
    serde_json::from_value::<ExecutionExternalJobCancelRequest>(request.payload.clone()).is_ok_and(
        |payload| {
            payload.tenant_id == request.tenant_id
                && Some(payload.external_job_uid) == request.external_job_uid
                && payload.job_generation > 0
                && !payload.provider.trim().is_empty()
                && !payload.provider_job_id.trim().is_empty()
                && !payload.idempotency_key.trim().is_empty()
        },
    )
}

fn task_cancel_payload_matches(request: &NewExecutionDispatch) -> bool {
    serde_json::from_value::<ExecutionTaskAttemptCancelRequest>(request.payload.clone()).is_ok_and(
        |payload| {
            payload.cancellation_dispatch_uid == request.dispatch_uid
                && payload.tenant_id == request.tenant_id
                && Some(payload.run_uid) == request.run_uid
                && Some(payload.task_id.as_uuid()) == request.task_id
                && Some(payload.controller_generation) == request.controller_generation
                && Some(payload.attempt_generation) == request.attempt_generation
                && payload.attempt_controller_generation > 0
                && payload.task_generation > 0
                && !payload.active_dispatch_uid.is_nil()
                && !payload.capacity_reservation_uid.is_nil()
                && !payload.watchdog_trigger_uid.is_nil()
        },
    )
}

fn compensation_cancel_payload_matches(request: &NewExecutionDispatch) -> bool {
    serde_json::from_value::<ExecutionCompensationAttemptCancelRequest>(request.payload.clone())
        .is_ok_and(|payload| {
            payload.cancellation_dispatch_uid == request.dispatch_uid
                && payload.tenant_id == request.tenant_id
                && Some(payload.run_uid) == request.run_uid
                && Some(payload.compensation_id.as_uuid()) == request.compensation_id
                && Some(payload.controller_generation) == request.controller_generation
                && payload.attempt_controller_generation > 0
                && Some(payload.compensation_generation) == request.compensation_generation
                && Some(payload.compensation_attempt_generation)
                    == request.compensation_attempt_generation
                && !payload.active_dispatch_uid.is_nil()
                && !payload.capacity_reservation_uid.is_nil()
                && !payload.watchdog_trigger_uid.is_nil()
        })
}

fn validate_claim_request(claim_owner: &str, batch_size: u32, claim_ttl: Duration) -> Result<()> {
    validate_claim_owner(claim_owner)?;
    if batch_size == 0 || batch_size > MAX_CLAIM_BATCH_SIZE || claim_ttl.is_zero() {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "execution dispatch claim requires batch size 1..={MAX_CLAIM_BATCH_SIZE} and positive TTL"
            ),
        });
    }
    Ok(())
}

fn validate_claim_owner(claim_owner: &str) -> Result<()> {
    if claim_owner.trim().is_empty() || claim_owner.len() > 256 {
        return Err(Error::InvalidRepositoryInput {
            message: "execution dispatch claim owner must contain 1..=256 bytes".to_string(),
        });
    }
    Ok(())
}

fn validate_ack_batch(dispatch_uids: &[Uuid]) -> Result<()> {
    if dispatch_uids.is_empty() || dispatch_uids.len() > MAX_CLAIM_BATCH_SIZE as usize {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "execution dispatch acknowledgement requires 1..={MAX_CLAIM_BATCH_SIZE} identities"
            ),
        });
    }
    if dispatch_uids.iter().copied().collect::<HashSet<_>>().len() != dispatch_uids.len() {
        return Err(Error::InvalidRepositoryInput {
            message: "execution dispatch acknowledgement identities must be unique".to_string(),
        });
    }
    Ok(())
}

fn duration_seconds_ceil(duration: Duration, field: &str) -> Result<i64> {
    let seconds = duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0));
    i64::try_from(seconds).map_err(|_| Error::InvalidRepositoryInput {
        message: format!("{field} exceeds PostgreSQL interval bounds"),
    })
}

fn dispatch_matches_request(
    record: &ExecutionDispatchRecord,
    request: &NewExecutionDispatch,
) -> bool {
    record.dispatch_uid == request.dispatch_uid
        && record.tenant_id == request.tenant_id
        && record.run_uid == request.run_uid
        && record.task_id == request.task_id
        && record.compensation_id == request.compensation_id
        && record.trigger_uid == request.trigger_uid
        && record.external_job_uid == request.external_job_uid
        && record.kind == request.kind
        && record.controller_generation == request.controller_generation
        && record.wake_epoch == request.wake_epoch
        && record.attempt_generation == request.attempt_generation
        && record.compensation_generation == request.compensation_generation
        && record.compensation_attempt_generation == request.compensation_attempt_generation
        && record.not_before_at.timestamp_micros() == request.not_before_at.timestamp_micros()
        && record.payload == request.payload
}

fn dispatch_from_row(row: &sqlx::postgres::PgRow) -> Result<ExecutionDispatchRecord> {
    let controller_generation = row
        .try_get::<Option<i64>, _>("controller_generation")
        .map_err(super::row_error)?;
    let wake_epoch = row
        .try_get::<Option<i64>, _>("wake_epoch")
        .map_err(super::row_error)?;
    let attempt_generation = row
        .try_get::<Option<i64>, _>("attempt_generation")
        .map_err(super::row_error)?;
    let compensation_generation = row
        .try_get::<Option<i64>, _>("compensation_generation")
        .map_err(super::row_error)?;
    let compensation_attempt_generation = row
        .try_get::<Option<i64>, _>("compensation_attempt_generation")
        .map_err(super::row_error)?;
    Ok(ExecutionDispatchRecord {
        dispatch_uid: row.try_get("dispatch_uid").map_err(super::row_error)?,
        tenant_id: TenantId(row.try_get("tenant_id").map_err(super::row_error)?),
        run_uid: row.try_get("run_uid").map_err(super::row_error)?,
        task_id: row.try_get("task_id").map_err(super::row_error)?,
        compensation_id: row.try_get("compensation_id").map_err(super::row_error)?,
        trigger_uid: row.try_get("trigger_uid").map_err(super::row_error)?,
        external_job_uid: row.try_get("external_job_uid").map_err(super::row_error)?,
        kind: row
            .try_get::<String, _>("dispatch_kind")
            .map_err(super::row_error)?
            .parse()?,
        state: row
            .try_get::<String, _>("state")
            .map_err(super::row_error)?
            .parse()?,
        controller_generation: controller_generation
            .map(|value| super::to_u64(value, "controller generation"))
            .transpose()?,
        wake_epoch: wake_epoch
            .map(|value| super::to_u64(value, "wake epoch"))
            .transpose()?,
        attempt_generation: attempt_generation
            .map(|value| super::to_u64(value, "attempt generation"))
            .transpose()?,
        compensation_generation: compensation_generation
            .map(|value| super::to_u64(value, "compensation generation"))
            .transpose()?,
        compensation_attempt_generation: compensation_attempt_generation
            .map(|value| super::to_u64(value, "compensation attempt generation"))
            .transpose()?,
        not_before_at: row.try_get("not_before_at").map_err(super::row_error)?,
        payload: row.try_get("payload").map_err(super::row_error)?,
        claim_owner: row.try_get("claim_owner").map_err(super::row_error)?,
        claimed_at: row.try_get("claimed_at").map_err(super::row_error)?,
        claim_expires_at: row.try_get("claim_expires_at").map_err(super::row_error)?,
        delivery_attempts: to_u32(
            row.try_get("delivery_attempts").map_err(super::row_error)?,
            "delivery attempts",
        )?,
        repair_epoch: to_u32(
            row.try_get("repair_epoch").map_err(super::row_error)?,
            "repair epoch",
        )?,
        delivered_at: row.try_get("delivered_at").map_err(super::row_error)?,
        last_error: row.try_get("last_error").map_err(super::row_error)?,
        created_at: row.try_get("created_at").map_err(super::row_error)?,
        updated_at: row.try_get("updated_at").map_err(super::row_error)?,
    })
}

/// Decodes an outbox row for sibling repository transactions.
pub(super) fn dispatch_from_row_for_repository(
    row: &sqlx::postgres::PgRow,
) -> Result<ExecutionDispatchRecord> {
    dispatch_from_row(row)
}

#[cfg(test)]
mod tests {
    use moa_core::types::identifiers::TenantId;
    use serde_json::json;
    use uuid::Uuid;

    use super::{
        ExecutionAdmissionResourceDimension, ExecutionDispatchKind, ExecutionRunPhaseDimension,
        NewExecutionDispatch, RUN_PHASE_CENSUS_SQL, admission_utilization_samples,
        dispatch_requires_durable_retry, run_phase_samples, validate_dispatch,
    };
    use crate::{
        state::{CompensationId, ExecutionTaskId},
        wire::{
            ExecutionAttemptCancelReason, ExecutionCompensationAttemptCancelRequest,
            ExecutionTaskAttemptCancelRequest,
        },
    };

    #[test]
    fn cancel_dispatch_payload_must_duplicate_every_persisted_fence_offline() {
        // Pins: an identity-free cancellation payload cannot redirect the outbox's exact
        // tenant/run/owner/generation target or omit active ownership receipts.
        let tenant_id = TenantId::new();
        let run_uid = Uuid::now_v7();
        let task_id = ExecutionTaskId::from_uuid(Uuid::now_v7());
        let dispatch_uid = Uuid::now_v7();
        let payload = ExecutionTaskAttemptCancelRequest {
            cancellation_dispatch_uid: dispatch_uid,
            tenant_id,
            run_uid,
            task_id,
            controller_generation: 3,
            attempt_controller_generation: 3,
            task_generation: 4,
            attempt_generation: 5,
            active_dispatch_uid: Uuid::now_v7(),
            capacity_reservation_uid: Uuid::now_v7(),
            watchdog_trigger_uid: Uuid::now_v7(),
            reason: ExecutionAttemptCancelReason::PauseRequested,
        };
        let serialized_payload =
            serde_json::to_value(payload).expect("serialize typed task cancel");
        assert_eq!(serialized_payload["dispatch_uid"], json!(dispatch_uid));
        assert!(
            serialized_payload
                .get("cancellation_dispatch_uid")
                .is_none()
        );
        let mut dispatch = NewExecutionDispatch {
            dispatch_uid,
            tenant_id,
            run_uid: Some(run_uid),
            task_id: Some(task_id.as_uuid()),
            compensation_id: None,
            trigger_uid: None,
            external_job_uid: None,
            kind: ExecutionDispatchKind::TaskAttemptCancel,
            controller_generation: Some(3),
            wake_epoch: None,
            attempt_generation: Some(5),
            compensation_generation: None,
            compensation_attempt_generation: None,
            not_before_at: chrono::Utc::now(),
            payload: serialized_payload,
        };
        validate_dispatch(&dispatch).expect("exact typed task cancellation validates");
        dispatch.attempt_generation = Some(6);
        assert!(validate_dispatch(&dispatch).is_err());
        dispatch.attempt_generation = Some(5);
        dispatch.payload["capacity_reservation_uid"] = json!(Uuid::nil());
        assert!(validate_dispatch(&dispatch).is_err());
    }

    #[test]
    fn compensation_cancel_kind_and_payload_round_trip_exactly_offline() {
        // Pins: compensation cancellation has a distinct durable label and requires both
        // logical and bounded-attempt generations plus exact active ownership receipts.
        assert_eq!(
            "compensation_attempt_cancel"
                .parse::<ExecutionDispatchKind>()
                .expect("parse compensation cancel kind"),
            ExecutionDispatchKind::CompensationAttemptCancel
        );
        assert_eq!(
            ExecutionDispatchKind::TaskAttemptCancel.as_str(),
            "task_attempt_cancel"
        );
        let tenant_id = TenantId::new();
        let run_uid = Uuid::now_v7();
        let compensation_id = CompensationId::from_uuid(Uuid::now_v7());
        let dispatch_uid = Uuid::now_v7();
        let payload = ExecutionCompensationAttemptCancelRequest {
            cancellation_dispatch_uid: dispatch_uid,
            tenant_id,
            run_uid,
            compensation_id,
            controller_generation: 7,
            attempt_controller_generation: 7,
            compensation_generation: 8,
            compensation_attempt_generation: 9,
            active_dispatch_uid: Uuid::now_v7(),
            capacity_reservation_uid: Uuid::now_v7(),
            watchdog_trigger_uid: Uuid::now_v7(),
            intent: crate::wire::ExecutionCompensationReleaseIntent::RunTerminal,
        };
        let serialized_payload =
            serde_json::to_value(payload).expect("serialize typed compensation cancellation");
        assert_eq!(serialized_payload["dispatch_uid"], json!(dispatch_uid));
        assert!(
            serialized_payload
                .get("cancellation_dispatch_uid")
                .is_none()
        );
        let dispatch = NewExecutionDispatch {
            dispatch_uid,
            tenant_id,
            run_uid: Some(run_uid),
            task_id: None,
            compensation_id: Some(compensation_id.as_uuid()),
            trigger_uid: None,
            external_job_uid: None,
            kind: ExecutionDispatchKind::CompensationAttemptCancel,
            controller_generation: Some(7),
            wake_epoch: None,
            attempt_generation: None,
            compensation_generation: Some(8),
            compensation_attempt_generation: Some(9),
            not_before_at: chrono::Utc::now(),
            payload: serialized_payload,
        };
        validate_dispatch(&dispatch).expect("exact typed compensation cancellation validates");
    }

    #[test]
    fn idle_admission_dimensions_still_report_their_healthy_zero_offline() {
        // Pins: a dimension that has never been reserved has no capacity-bucket row, but its
        // gauge must still be written. The alerts carry `absent()`, so a utilization series
        // that only appears once work exists would page on a quiet fleet.
        let observed = vec![
            ("active_tasks".to_string(), 0.25, 0.75, 0.6),
            ("external_jobs".to_string(), 0.5, 0.5, 1.0),
        ];
        let samples =
            admission_utilization_samples(&observed).expect("known dimensions must decode");

        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.resource)
                .collect::<Vec<_>>(),
            ExecutionAdmissionResourceDimension::ALL.to_vec(),
            "every bounded dimension must be reported on every snapshot"
        );
        let active_tasks = samples
            .iter()
            .find(|sample| sample.resource == ExecutionAdmissionResourceDimension::ActiveTasks)
            .expect("observed dimension is retained");
        assert!((active_tasks.fleet_ratio - 0.25).abs() < f64::EPSILON);
        assert!((active_tasks.tenant_peak_ratio - 0.75).abs() < f64::EPSILON);
        assert!((active_tasks.tenant_max_share_ratio - 0.6).abs() < f64::EPSILON);
        for idle in samples.iter().filter(|sample| {
            !matches!(
                sample.resource,
                ExecutionAdmissionResourceDimension::ActiveTasks
                    | ExecutionAdmissionResourceDimension::ExternalJobs
            )
        }) {
            assert_eq!(idle.fleet_ratio, 0.0, "{:?}", idle.resource);
            assert_eq!(idle.tenant_peak_ratio, 0.0, "{:?}", idle.resource);
            assert_eq!(idle.tenant_max_share_ratio, 0.0, "{:?}", idle.resource);
        }

        assert!(
            admission_utilization_samples(&[("active_sandboxes".to_string(), 1.0, 1.0, 1.0)])
                .is_err(),
            "an unmodelled durable dimension must fail closed, not vanish from the report"
        );
    }

    #[test]
    fn idle_run_phases_still_report_their_healthy_zero_offline() {
        // Pins: `GROUP BY status` returns no row for a phase holding no runs. Every bounded
        // phase must still be reported, or its gauge would be absent on a quiet fleet and its
        // `absent()` alert would page. An unmodelled durable status must fail the snapshot
        // rather than silently drop runs out of a census whose sum is the live fleet.
        let observed = vec![("running".to_string(), 7_i64), ("paused".to_string(), 4)];
        let samples = run_phase_samples(&observed).expect("known statuses must decode");

        assert_eq!(
            samples
                .iter()
                .map(|sample| sample.phase)
                .collect::<Vec<_>>(),
            ExecutionRunPhaseDimension::ALL.to_vec(),
            "every bounded phase must be reported on every snapshot"
        );
        let count_of = |phase: ExecutionRunPhaseDimension| {
            samples
                .iter()
                .find(|sample| sample.phase == phase)
                .map(|sample| sample.run_count)
                .expect("every phase is reported")
        };
        assert_eq!(count_of(ExecutionRunPhaseDimension::Running), 7);
        assert_eq!(count_of(ExecutionRunPhaseDimension::Paused), 4);
        for idle in samples.iter().filter(|sample| {
            !matches!(
                sample.phase,
                ExecutionRunPhaseDimension::Running | ExecutionRunPhaseDimension::Paused
            )
        }) {
            assert_eq!(idle.run_count, 0, "{:?}", idle.phase);
        }
        assert_eq!(
            samples.iter().map(|sample| sample.run_count).sum::<u64>(),
            11,
            "the census must sum to exactly the observed live fleet"
        );

        assert!(
            run_phase_samples(&[("completed".to_string(), 1)]).is_err(),
            "a terminal status is not a live phase and must fail closed"
        );
        assert!(
            run_phase_samples(&[("quarantined".to_string(), 1)]).is_err(),
            "an unmodelled durable status must fail closed, not vanish from the census"
        );
    }

    #[test]
    fn run_phase_census_query_covers_exactly_the_bounded_phase_set_offline() {
        // Pins: the census status list is spelled out in SQL so it matches the partial index
        // predicate literally and keeps the index-only scan. That literal and the exported
        // label set can drift apart silently, which would drop a live phase from the census
        // while every gauge still looked healthy.
        let (_, after) = RUN_PHASE_CENSUS_SQL
            .split_once("status IN (")
            .expect("census query filters on the nonterminal status list");
        let (list, _) = after
            .split_once(')')
            .expect("the status list is parenthesized");
        let mut queried = list
            .split(',')
            .map(|status| status.trim().trim_matches('\'').to_string())
            .collect::<Vec<_>>();
        queried.sort();
        let mut bounded = ExecutionRunPhaseDimension::ALL
            .iter()
            .map(|phase| phase.as_str().to_string())
            .collect::<Vec<_>>();
        bounded.sort();
        assert_eq!(
            queried, bounded,
            "the census query and the bounded phase set must name the same statuses"
        );
    }

    #[test]
    fn correctness_dispatches_never_terminally_dead_letter_offline() {
        // Pins: correctness work remains behind capped sparse retries; only never-started
        // task and compensation attempts may dead-letter after exact owner repair.
        for kind in [
            ExecutionDispatchKind::RunActivation,
            ExecutionDispatchKind::TaskAttemptCancel,
            ExecutionDispatchKind::CompensationAttemptCancel,
            ExecutionDispatchKind::TriggerDelivery,
            ExecutionDispatchKind::ExternalCancel,
        ] {
            assert!(dispatch_requires_durable_retry(kind), "{kind:?}");
        }
        assert!(!dispatch_requires_durable_retry(
            ExecutionDispatchKind::TaskAttempt
        ));
        assert!(!dispatch_requires_durable_retry(
            ExecutionDispatchKind::CompensationAttempt
        ));
    }
}
