//! Generation-fenced asynchronous provider-job persistence and callback deduplication.

use std::str::FromStr;

use crate::wire::{
    ExecutionCompensationAttemptCancelRequest, ExecutionExternalJobCancelRequest,
    ExecutionExternalJobStartRecoveryOwner, ExecutionExternalJobStartRecoveryRequest,
};
use chrono::{DateTime, Utc};
use moa_config::ExecutionConfig;
use moa_core::types::identifiers::TenantId;
use moa_core::types::tools::{AsyncToolJobCallbackOutcome, AsyncToolJobTerminalOutcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::{
    Error, ExecutionRepository, ExecutionScope, Result,
    capacity::{
        CapacityReserveOutcome, ExecutionCapacityDimension, ExecutionCapacityOwner,
        ExecutionCapacityRequest, execution_capacity_reservation_uid,
        prelock_capacity_dimensions_in_tx, prelock_existing_capacity_dimensions_in_tx,
        release_capacity_in_tx, reserve_capacity_in_tx,
    },
    compensation::{
        CompensationExternalJobSettlementOutcome,
        CompensationExternalNotStartedReleaseClaimOutcome,
        CompensationRecoveredExternalReleaseClaimOutcome,
        begin_compensation_external_not_started_release_in_conn,
        begin_recovered_compensation_external_release_in_conn,
        settle_external_job_terminal_in_conn as settle_compensation_external_job_terminal_in_conn,
    },
    outbox::{
        ExecutionDispatchKind, ExecutionDispatchRecord, NewExecutionDispatch,
        enqueue_dispatch_in_conn,
    },
    run::enqueue_run_activation_in_conn,
    sqlx_error, storage_error,
    task::{
        ExternalJobTaskSettlementOutcome, TaskAttemptCheckpointKind, TaskAttemptExternalOutcome,
        TaskAttemptFence, TaskExternalStartRetryOutcome,
        external_start_checkpoint_payload_is_provisional,
        settle_external_job_terminal_in_conn as settle_task_external_job_terminal_in_conn,
    },
    to_i64,
    trigger::{
        ExecutionTriggerKind, NewExecutionTrigger, create_trigger_with_dispatch_in_conn,
        supersede_trigger_in_conn,
    },
};

const MAX_RECONCILE_BATCH_SIZE: u32 = 1_000;
const EXTERNAL_RECONCILE_TRIGGER_NAMESPACE: Uuid =
    Uuid::from_u128(0xf01c_6bd5_175f_581c_8f53_e2e3_c69a_0eab);
const EXTERNAL_START_RECOVERY_TRIGGER_NAMESPACE: Uuid =
    Uuid::from_u128(0x3551_037a_2e09_55fa_a23f_e1b9_3717_2172);
const EXTERNAL_CANCEL_DISPATCH_NAMESPACE: Uuid =
    Uuid::from_u128(0x8445_6cf7_3f09_5c27_9eb2_d9e8_1fc0_fa8c);

/// Durable asynchronous provider-job lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionExternalJobState {
    /// Capacity is reserved, but no provider call has been made or bound yet.
    Unbound,
    /// Provider start has been durably admitted but not confirmed running.
    Starting,
    /// Provider confirms active asynchronous work.
    Running,
    /// MOA is waiting for a sparse reconciliation wake.
    WaitingReconcile,
    /// Cancellation was requested but its provider outcome is unresolved.
    CancelRequested,
    /// Provider work completed successfully.
    Completed,
    /// Provider work failed definitively.
    Failed,
    /// Provider work was cancelled definitively.
    Cancelled,
    /// Provider outcome cannot safely be inferred.
    UnknownOutcome,
}

impl ExecutionExternalJobState {
    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unbound => "unbound",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::WaitingReconcile => "waiting_reconcile",
            Self::CancelRequested => "cancel_requested",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::UnknownOutcome => "unknown_outcome",
        }
    }

    /// Returns whether the state is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::UnknownOutcome
        )
    }
}

impl FromStr for ExecutionExternalJobState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "unbound" => Ok(Self::Unbound),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "waiting_reconcile" => Ok(Self::WaitingReconcile),
            "cancel_requested" => Ok(Self::CancelRequested),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "unknown_outcome" => Ok(Self::UnknownOutcome),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution external-job state `{value}`"),
            }),
        }
    }
}

/// Exact durable owner of one asynchronous provider job.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionExternalJobOwner {
    /// One exact active forward-task attempt.
    Task {
        /// Stable logical task identity.
        task_id: Uuid,
        /// Exact active task-attempt generation.
        attempt_generation: u64,
    },
    /// One exact active compensation attempt.
    Compensation {
        /// Stable compensation registration identity.
        compensation_id: Uuid,
        /// Exact compensation logical generation.
        compensation_generation: u64,
        /// Exact active compensation-attempt generation.
        compensation_attempt_generation: u64,
    },
}

/// Immutable pre-provider intent that reserves external-job capacity.
#[derive(Clone, Debug, PartialEq)]
pub struct NewExecutionExternalJobIntent {
    /// Stable MOA job identity.
    pub external_job_uid: Uuid,
    /// Tenant that owns the task and job.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Exact task or compensation attempt that owns the provider job.
    pub owner: ExecutionExternalJobOwner,
    /// Provider-job generation, incremented when replacement is explicit.
    pub job_generation: u64,
    /// Declared adapter/provider key used for crash-safe start recovery.
    pub provider: String,
    /// Stable provider idempotency key.
    pub idempotency_key: String,
    /// Deadline after which an unbound intent can be reclaimed safely.
    pub expires_at: DateTime<Utc>,
}

/// Generation-fenced provider identity bound after an asynchronous start response.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionExternalJobBinding {
    /// Stable MOA job identity selected before provider dispatch.
    pub external_job_uid: Uuid,
    /// Tenant that owns the task and job.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Exact task or compensation attempt that owns the provider job.
    pub owner: ExecutionExternalJobOwner,
    /// Exact pre-reserved provider-job generation.
    pub job_generation: u64,
    /// Stable provider idempotency key used for the provider call.
    pub idempotency_key: String,
    /// Provider name, which must match the pre-reserved declared provider.
    pub provider: String,
    /// Provider-issued job identity.
    pub provider_job_id: String,
    /// Reference used by the callback-authentication boundary.
    pub callback_auth_reference: String,
    /// Initial bound, nonterminal provider-job state.
    pub state: ExecutionExternalJobState,
    /// Optional provider progress phase.
    pub progress_phase: Option<String>,
    /// Whether provider cancellation is supported.
    pub cancel_supported: bool,
    /// Next sparse reconciliation time.
    pub next_reconcile_at: Option<DateTime<Utc>>,
    /// Bounded evidence when adapter output violated its reserved start contract.
    pub provider_contract_violation: Option<String>,
}

/// One persisted asynchronous provider job.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionExternalJobRecord {
    /// Stable MOA job identity.
    pub external_job_uid: Uuid,
    /// Tenant that owns the row.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Exact task or compensation attempt that owns the provider job.
    pub owner: ExecutionExternalJobOwner,
    /// Provider-job generation.
    pub job_generation: u64,
    /// Declared adapter/provider key reserved before provider dispatch.
    pub declared_provider: String,
    /// Bound provider name; absent only while the intent is unbound.
    pub provider: Option<String>,
    /// Provider-issued job identity.
    pub provider_job_id: Option<String>,
    /// Stable provider idempotency key.
    pub idempotency_key: String,
    /// Callback-authentication reference.
    pub callback_auth_reference: Option<String>,
    /// Current lifecycle state.
    pub state: ExecutionExternalJobState,
    /// Latest progress phase.
    pub progress_phase: Option<String>,
    /// Whether provider cancellation is supported.
    pub cancel_supported: bool,
    /// Next sparse reconciliation time.
    pub next_reconcile_at: Option<DateTime<Utc>>,
    /// Last accepted provider callback event identity.
    pub last_provider_event_id: Option<String>,
    /// Terminal provider output.
    pub output: Option<Value>,
    /// Terminal provider error.
    pub error: Option<Value>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last mutation time.
    pub updated_at: DateTime<Utc>,
    /// Terminal time.
    pub completed_at: Option<DateTime<Utc>>,
    /// Durable provider contract-violation evidence, if recovery containment was required.
    pub provider_contract_violation: Option<Value>,
}

/// Idempotent disposition of an exact unbound intent release.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionExternalJobIntentReleaseOutcome {
    /// The exact unbound intent and its capacity receipt were removed.
    Released,
    /// Neither the intent nor an active capacity receipt remains.
    AlreadyReleased,
    /// The stable UID exists with different immutable coordinates.
    Stale,
    /// Provider identity was already bound and must never be reclaimed as an intent.
    AlreadyBound,
}

/// Owner adoption committed after crash-safe provider start recovery.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum ExecutionExternalJobStartRecoveryAdoptionOutcome {
    /// Provider ownership or the no-start retry transition was committed.
    Applied {
        /// Compensation teardown request that must obtain its verified hand-release receipt.
        compensation_release: Option<Box<ExecutionCompensationAttemptCancelRequest>>,
    },
    /// The same exact owner transition was already committed.
    Replayed {
        /// Compensation teardown request replayed until its verified finalizer commits.
        compensation_release: Option<Box<ExecutionCompensationAttemptCancelRequest>>,
    },
    /// Another exact recovery already settled the intent and owner.
    AlreadySettled,
    /// No exact intent or owner exists.
    NotFound,
    /// An immutable owner, generation, checkpoint, capacity, or watchdog fence differed.
    Stale,
    /// The owner cannot adopt this recovery result from its current state.
    InvalidState,
}

/// Authenticated callback for one provider-job generation.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionExternalJobCallback {
    /// Stable MOA job identity selected after callback authentication.
    pub external_job_uid: Uuid,
    /// Expected provider-job generation.
    pub job_generation: u64,
    /// Expected provider name.
    pub provider: String,
    /// Expected provider-issued job identity.
    pub provider_job_id: String,
    /// Provider event identity used for deduplication.
    pub provider_event_id: String,
    /// Typed progress or terminal mutation.
    pub update: ExecutionExternalJobCallbackUpdate,
}

/// Mutation carried by one authenticated provider callback.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionExternalJobCallbackUpdate {
    /// Advances observable progress without terminalizing the provider job.
    Progress {
        /// Current nonterminal provider lifecycle state.
        state: ExecutionExternalJobState,
        /// Latest bounded provider progress phase.
        progress_phase: Option<String>,
        /// Next sparse reconciliation time, if provider polling remains necessary.
        next_reconcile_at: Option<DateTime<Utc>>,
    },
    /// Records the definitive provider outcome.
    Terminal {
        /// Terminal provider lifecycle state.
        state: ExecutionExternalJobState,
        /// Final bounded provider progress phase.
        progress_phase: Option<String>,
        /// Structured terminal output.
        output: Option<Value>,
        /// Structured terminal error.
        error: Option<Value>,
    },
}

impl From<AsyncToolJobCallbackOutcome> for ExecutionExternalJobCallbackUpdate {
    fn from(outcome: AsyncToolJobCallbackOutcome) -> Self {
        match outcome {
            AsyncToolJobCallbackOutcome::Progress {
                progress_phase,
                next_reconcile_at,
            } => Self::Progress {
                state: ExecutionExternalJobState::WaitingReconcile,
                progress_phase: Some(progress_phase),
                next_reconcile_at: Some(next_reconcile_at),
            },
            AsyncToolJobCallbackOutcome::Terminal { outcome } => match outcome {
                AsyncToolJobTerminalOutcome::Completed { output } => Self::Terminal {
                    state: ExecutionExternalJobState::Completed,
                    progress_phase: Some("completed".to_string()),
                    output: Some(output),
                    error: None,
                },
                AsyncToolJobTerminalOutcome::Failed { error } => Self::Terminal {
                    state: ExecutionExternalJobState::Failed,
                    progress_phase: Some("failed".to_string()),
                    output: None,
                    error: Some(error),
                },
                AsyncToolJobTerminalOutcome::Cancelled => Self::Terminal {
                    state: ExecutionExternalJobState::Cancelled,
                    progress_phase: Some("cancelled".to_string()),
                    output: None,
                    error: None,
                },
                AsyncToolJobTerminalOutcome::UnknownOutcome { error } => Self::Terminal {
                    state: ExecutionExternalJobState::UnknownOutcome,
                    progress_phase: Some("unknown_outcome".to_string()),
                    output: None,
                    error: Some(error),
                },
            },
        }
    }
}

/// Result of applying an authenticated provider callback.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionExternalJobCallbackOutcome {
    /// Callback advanced the exact current job generation.
    Applied(Box<ExecutionExternalJobRecord>),
    /// The exact provider event was already accepted.
    Duplicate,
    /// Callback generation or provider identity is stale.
    StaleGeneration,
    /// A terminal outcome already fenced later callbacks.
    AlreadyTerminal,
    /// No visible job has the supplied MOA identity.
    NotFound,
}

/// Atomic callback settlement and its persisted controller wake.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionExternalJobCallbackWrite {
    /// Generation-fenced callback disposition.
    pub outcome: ExecutionExternalJobCallbackOutcome,
    /// Exact controller activation committed with an applied callback.
    ///
    /// Terminal runs retain the callback receipt without creating an
    /// unreachable activation.
    pub activation: Option<ExecutionDispatchRecord>,
}

/// Exact asynchronous-provider cancellation settlement.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionExternalJobCancellation {
    /// Stable MOA job identity.
    pub external_job_uid: Uuid,
    /// Expected provider-job generation.
    pub job_generation: u64,
    /// Expected provider name.
    pub provider: String,
    /// Expected provider-issued job identity.
    pub provider_job_id: String,
    /// Cancellation lifecycle result.
    pub state: ExecutionExternalJobState,
    /// Next sparse reconciliation time when cancellation remains unresolved.
    pub next_reconcile_at: Option<DateTime<Utc>>,
    /// Structured uncertainty evidence for an unknown outcome.
    pub error: Option<Value>,
}

/// Result of settling one provider cancellation response.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionExternalJobCancellationOutcome {
    /// Cancellation state advanced for the exact current generation.
    Applied(Box<ExecutionExternalJobRecord>),
    /// Generation or provider identity no longer names the current job.
    StaleGeneration,
    /// A definitive provider outcome already fenced cancellation settlement.
    AlreadyTerminal,
    /// No visible job has the supplied MOA identity.
    NotFound,
}

/// Pending-terminal request to durably cancel one bound external owner.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionExternalJobCancellationRequestOutcome {
    /// Cancellation was requested and its exact dispatch is durable.
    Applied(ExecutionDispatchRecord),
    /// The exact cancellation request was already durable.
    Replayed(ExecutionDispatchRecord),
    /// The intent is still unbound and must be resolved by start recovery first.
    UnboundPendingRecovery,
    /// Provider work is already terminal.
    AlreadyTerminal,
    /// No matching external job exists.
    NotFound,
    /// Stable job identity no longer belongs to the expected owner generation.
    Stale,
}

impl ExecutionRepository {
    /// Loads one visible asynchronous provider job by its stable MOA identity.
    pub async fn load_external_job(
        &self,
        scope: ExecutionScope,
        external_job_uid: Uuid,
    ) -> Result<Option<ExecutionExternalJobRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row =
            sqlx::query("SELECT * FROM moa.execution_external_job WHERE external_job_uid = $1")
                .bind(external_job_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let record = row.as_ref().map(external_job_from_row).transpose()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// Loads the exact current unbound provider-start recovery that owns a running task attempt.
    ///
    /// A due task watchdog must defer to this intent: only the provider adapter's crash-safe
    /// `recover_start` result can decide whether the ambiguous start created external work.
    /// Returning `None` means no complete current recovery authority exists for the supplied
    /// attempt fence; malformed matched durable state is reported as repository corruption.
    pub async fn load_current_task_external_start_recovery(
        &self,
        fence: TaskAttemptFence,
    ) -> Result<Option<ExecutionExternalJobStartRecoveryRequest>> {
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let row = sqlx::query(
            r#"
            SELECT job.*, recovery.trigger_uid, recovery.payload AS recovery_payload,
                   checkpoint.checkpoint_kind, checkpoint.payload AS checkpoint_payload
            FROM moa.execution_task AS task
            JOIN moa.execution_run AS run
              ON run.run_uid=task.run_uid AND run.tenant_id=task.tenant_id
            JOIN moa.execution_capacity_reservation AS active_capacity
              ON active_capacity.reservation_uid=$7
             AND active_capacity.tenant_id=task.tenant_id
             AND active_capacity.run_uid=task.run_uid
             AND active_capacity.task_id=task.task_id
             AND active_capacity.controller_generation=$4
             AND active_capacity.attempt_generation=$3
             AND active_capacity.resource_dimension='active_tasks'
             AND active_capacity.state IN ('reserved','reconciling')
             AND active_capacity.released_at IS NULL
            JOIN moa.execution_trigger AS watchdog
              ON watchdog.trigger_uid=$6 AND watchdog.tenant_id=task.tenant_id
             AND watchdog.run_uid=task.run_uid AND watchdog.task_id=task.task_id
             AND watchdog.controller_generation=$4 AND watchdog.attempt_generation=$3
             AND watchdog.trigger_kind='task_watchdog' AND watchdog.state='pending'
            JOIN moa.execution_task_checkpoint AS checkpoint
              ON checkpoint.tenant_id=task.tenant_id AND checkpoint.run_uid=task.run_uid
             AND checkpoint.task_id=task.task_id AND checkpoint.controller_generation=$4
             AND checkpoint.attempt_generation=$3 AND checkpoint.dispatch_uid=$5
             AND checkpoint.superseded_at IS NULL
            JOIN moa.execution_external_job AS job
              ON job.tenant_id=task.tenant_id AND job.run_uid=task.run_uid
             AND job.task_id=task.task_id AND job.attempt_generation=$3
             AND job.state='unbound'
            JOIN moa.execution_capacity_reservation AS job_capacity
              ON job_capacity.tenant_id=job.tenant_id
             AND job_capacity.external_job_uid=job.external_job_uid
             AND job_capacity.resource_dimension='external_jobs'
             AND job_capacity.state='reserved' AND job_capacity.released_at IS NULL
            JOIN moa.execution_trigger AS recovery
              ON recovery.tenant_id=job.tenant_id AND recovery.run_uid=job.run_uid
             AND recovery.task_id=job.task_id AND recovery.attempt_generation=$3
             AND recovery.controller_generation=$4
             AND recovery.trigger_kind='external_start_recovery'
             AND recovery.state='pending'
            WHERE task.tenant_id=$1 AND task.run_uid=$2 AND task.task_id=$8
              AND task.attempt_generation=$3 AND task.active_dispatch_uid=$5
              AND task.status='running' AND task.attempt_state='running'
              AND run.controller_generation=$4 AND run.pending_terminal_status IS NULL
            "#,
        )
        .bind(fence.tenant_id.0)
        .bind(fence.run_uid)
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(to_i64(
            fence.controller_generation,
            "controller generation",
        )?)
        .bind(fence.dispatch_uid)
        .bind(fence.watchdog_trigger_uid)
        .bind(fence.capacity_reservation_uid)
        .bind(fence.task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(None);
        };
        let job = external_job_from_row(&row)?;
        let checkpoint_kind = TaskAttemptCheckpointKind::parse(
            &row.try_get::<String, _>("checkpoint_kind")
                .map_err(super::row_error)?,
        )?;
        let checkpoint_payload = row
            .try_get::<Value, _>("checkpoint_payload")
            .map_err(super::row_error)?;
        if !external_start_checkpoint_payload_is_provisional(checkpoint_kind, &checkpoint_payload) {
            return Err(Error::InvalidRepositoryData {
                message: "unbound task external job lost its provisional start checkpoint"
                    .to_string(),
            });
        }
        let trigger_uid = row
            .try_get::<Uuid, _>("trigger_uid")
            .map_err(super::row_error)?;
        let request = ExecutionExternalJobStartRecoveryRequest {
            tenant_id: job.tenant_id,
            run_uid: job.run_uid,
            owner: ExecutionExternalJobStartRecoveryOwner::Task {
                task_id: fence.task_id.as_uuid(),
                attempt_generation: fence.attempt_generation,
            },
            external_job_uid: job.external_job_uid,
            job_generation: job.job_generation,
            provider: job.declared_provider,
            idempotency_key: job.idempotency_key,
            trigger_uid,
        };
        let recovery_payload = row
            .try_get::<Value, _>("recovery_payload")
            .map_err(super::row_error)?;
        if recovery_payload
            != json!({
                "external_job_uid": request.external_job_uid,
                "job_generation": request.job_generation,
                "declared_provider": request.provider,
                "idempotency_key": request.idempotency_key,
            })
        {
            return Err(Error::InvalidRepositoryData {
                message: "unbound task external job recovery trigger payload is inconsistent"
                    .to_string(),
            });
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(Some(request))
    }

    /// Reserves one exact unbound external-job intent before provider dispatch.
    pub async fn reserve_external_job_intent(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        intent: NewExecutionExternalJobIntent,
    ) -> Result<ExecutionExternalJobRecord> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            intent.tenant_id,
            &[
                ExecutionCapacityDimension::ScheduledTriggers,
                ExecutionCapacityDimension::ExternalJobs,
            ],
        )
        .await?;
        let record = reserve_external_job_intent_in_conn(conn.as_mut(), config, &intent).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// Binds one live pre-reserved intent to the provider's asynchronous response.
    pub async fn bind_external_job(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        binding: ExecutionExternalJobBinding,
    ) -> Result<ExecutionExternalJobRecord> {
        let mut conn = scope.begin(&self.pool).await?;
        if let Some(tenant_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_external_job WHERE external_job_uid=$1",
        )
        .bind(binding.external_job_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        {
            prelock_existing_capacity_dimensions_in_tx(
                conn.as_mut(),
                TenantId(tenant_id),
                &[
                    ExecutionCapacityDimension::ScheduledTriggers,
                    ExecutionCapacityDimension::ExternalJobs,
                ],
            )
            .await?;
        }
        let record = bind_external_job_in_conn(conn.as_mut(), config, &binding).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(record)
    }

    /// Releases one exact unbound intent when no provider work was dispatched.
    pub async fn release_external_job_intent(
        &self,
        scope: ExecutionScope,
        intent: NewExecutionExternalJobIntent,
    ) -> Result<ExecutionExternalJobIntentReleaseOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let capacity_owner_tenant = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_external_job WHERE external_job_uid=$1 \
             UNION ALL SELECT tenant_id FROM moa.execution_capacity_reservation \
             WHERE reservation_uid=$2 AND released_at IS NULL LIMIT 1",
        )
        .bind(intent.external_job_uid)
        .bind(execution_capacity_reservation_uid(
            ExecutionCapacityDimension::ExternalJobs,
            intent.external_job_uid,
            None,
        ))
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if let Some(tenant_id) = capacity_owner_tenant {
            prelock_existing_capacity_dimensions_in_tx(
                conn.as_mut(),
                TenantId(tenant_id),
                &[
                    ExecutionCapacityDimension::ScheduledTriggers,
                    ExecutionCapacityDimension::ExternalJobs,
                ],
            )
            .await?;
        }
        let outcome = release_external_job_intent_in_conn(conn.as_mut(), &intent).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Commits a provider `NotStarted` recovery and adopts its exact owner in one transaction.
    pub async fn recover_external_job_start_not_started(
        &self,
        request: &ExecutionExternalJobStartRecoveryRequest,
        recovered_at: DateTime<Utc>,
    ) -> Result<ExecutionExternalJobStartRecoveryAdoptionOutcome> {
        let intent = external_job_intent_from_recovery_request(request);
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            request.tenant_id,
            &[
                ExecutionCapacityDimension::ActiveTasks,
                ExecutionCapacityDimension::ScheduledTriggers,
                ExecutionCapacityDimension::ExternalJobs,
            ],
        )
        .await?;
        match release_external_job_intent_in_conn(conn.as_mut(), &intent).await? {
            ExecutionExternalJobIntentReleaseOutcome::Released
            | ExecutionExternalJobIntentReleaseOutcome::AlreadyReleased => {}
            ExecutionExternalJobIntentReleaseOutcome::Stale
            | ExecutionExternalJobIntentReleaseOutcome::AlreadyBound => {
                conn.commit().await.map_err(storage_error)?;
                return Ok(ExecutionExternalJobStartRecoveryAdoptionOutcome::Stale);
            }
        }
        let outcome = match intent.owner {
            ExecutionExternalJobOwner::Task { .. } => {
                let outcome = ExecutionRepository::requeue_task_external_start_not_started_in_conn(
                    &mut conn,
                    &intent,
                    recovered_at,
                )
                .await?;
                map_task_not_started_recovery(outcome)
            }
            ExecutionExternalJobOwner::Compensation { .. } => {
                let outcome = begin_compensation_external_not_started_release_in_conn(
                    &mut conn,
                    &intent,
                    recovered_at,
                )
                .await?;
                map_compensation_not_started_recovery(outcome)
            }
        };
        if recovery_adoption_committed(&outcome) {
            conn.commit().await.map_err(storage_error)?;
        } else {
            conn.rollback().await.map_err(storage_error)?;
        }
        Ok(outcome)
    }

    /// Binds a recovered provider start and adopts its exact owner in one transaction.
    pub async fn recover_external_job_start_started(
        &self,
        config: &ExecutionConfig,
        request: &ExecutionExternalJobStartRecoveryRequest,
        binding: ExecutionExternalJobBinding,
        recovered_at: DateTime<Utc>,
    ) -> Result<ExecutionExternalJobStartRecoveryAdoptionOutcome> {
        if binding.external_job_uid != request.external_job_uid
            || binding.job_generation != request.job_generation
            || binding.tenant_id != request.tenant_id
            || binding.run_uid != request.run_uid
            || binding.owner != external_job_owner_from_recovery_request(request)
        {
            return Err(Error::InvalidRepositoryInput {
                message: "recovered external-job binding lost its trigger owner fences".to_string(),
            });
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            request.tenant_id,
            &[
                ExecutionCapacityDimension::ActiveTasks,
                ExecutionCapacityDimension::ScheduledTriggers,
                ExecutionCapacityDimension::ExternalJobs,
            ],
        )
        .await?;
        // Expiry fences the unresolved provider start, not a provider job that
        // recover_start has now proven exists. The bound job keeps this receipt
        // until its normal terminal or cancellation settlement releases it.
        sqlx::query(
            "UPDATE moa.execution_capacity_reservation SET expires_at=NULL, updated_at=NOW() \
             WHERE tenant_id=$1 AND external_job_uid=$2 \
               AND resource_dimension='external_jobs' AND state='reserved' \
               AND released_at IS NULL",
        )
        .bind(request.tenant_id.0)
        .bind(request.external_job_uid)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let job = bind_external_job_in_conn(conn.as_mut(), config, &binding).await?;
        let outcome = match job.owner {
            ExecutionExternalJobOwner::Task { .. } => {
                let outcome = ExecutionRepository::adopt_recovered_task_external_job_in_conn(
                    &mut conn,
                    &job,
                    recovered_at,
                )
                .await?;
                map_task_started_recovery(outcome)
            }
            ExecutionExternalJobOwner::Compensation { .. } => {
                let outcome = begin_recovered_compensation_external_release_in_conn(
                    &mut conn,
                    &job,
                    recovered_at,
                )
                .await?;
                map_compensation_started_recovery(outcome)
            }
        };
        if !recovery_adoption_committed(&outcome) {
            let _ = request_external_job_cancellation_in_conn(
                &mut conn,
                config,
                job.external_job_uid,
                job.owner,
                recovered_at,
            )
            .await?;
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Lists a bounded page of expired intents that require provider start recovery.
    ///
    /// This method never deletes an intent: the provider may have started work
    /// before a crash. The caller may release only after `recover_start` proves
    /// `NotStarted`; `Started` must bind and `Unknown` must remain durable.
    pub async fn list_expired_external_job_intents(
        &self,
        scope: ExecutionScope,
        batch_size: u32,
    ) -> Result<Vec<NewExecutionExternalJobIntent>> {
        if batch_size == 0 || batch_size > MAX_RECONCILE_BATCH_SIZE {
            return Err(Error::InvalidRepositoryInput {
                message: format!(
                    "external job intent reclaim batch must be 1..={MAX_RECONCILE_BATCH_SIZE}"
                ),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            SELECT job.*, capacity.expires_at AS capacity_expires_at
            FROM moa.execution_external_job AS job
            JOIN moa.execution_capacity_reservation AS capacity
              ON capacity.tenant_id=job.tenant_id
             AND capacity.external_job_uid=job.external_job_uid
             AND capacity.resource_dimension='external_jobs'
             AND capacity.released_at IS NULL
            WHERE job.state='unbound' AND capacity.expires_at <= now()
            ORDER BY capacity.expires_at, job.tenant_id, job.external_job_uid
            FOR UPDATE OF job, capacity SKIP LOCKED
            LIMIT $1
            "#,
        )
        .bind(i64::from(batch_size))
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let mut intents = Vec::with_capacity(rows.len());
        for row in rows {
            let record = external_job_from_row(&row)?;
            let expires_at = row
                .try_get::<Option<DateTime<Utc>>, _>("capacity_expires_at")
                .map_err(super::row_error)?
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "unbound external job capacity receipt has no expiry".to_string(),
                })?;
            intents.push(NewExecutionExternalJobIntent {
                external_job_uid: record.external_job_uid,
                tenant_id: record.tenant_id,
                run_uid: record.run_uid,
                owner: record.owner,
                job_generation: record.job_generation,
                provider: record.declared_provider,
                idempotency_key: record.idempotency_key,
                expires_at,
            });
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(intents)
    }

    /// Atomically applies one authenticated callback and wakes its live run controller.
    pub async fn apply_external_job_callback_and_activate(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        callback: ExecutionExternalJobCallback,
    ) -> Result<ExecutionExternalJobCallbackWrite> {
        let mut conn = scope.begin(&self.pool).await?;
        if let Some(tenant_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_external_job WHERE external_job_uid=$1",
        )
        .bind(callback.external_job_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        {
            prelock_capacity_dimensions_in_tx(
                conn.as_mut(),
                config,
                TenantId(tenant_id),
                &[
                    ExecutionCapacityDimension::ActiveRuns,
                    ExecutionCapacityDimension::ParkedRuns,
                    ExecutionCapacityDimension::ScheduledTriggers,
                    ExecutionCapacityDimension::ExternalJobs,
                ],
            )
            .await?;
        }
        let outcome = apply_external_job_callback_in_conn(conn.as_mut(), &callback).await?;
        let activation = if let ExecutionExternalJobCallbackOutcome::Applied(job) = &outcome {
            let deferred_release = if job.state.is_terminal() {
                settle_external_job_owner_terminal_in_conn(&mut conn, job, Utc::now()).await?
            } else {
                false
            };
            let run = sqlx::query_as::<_, (i64, String)>(
                "SELECT controller_generation, status \
                 FROM moa.execution_run WHERE tenant_id = $1 AND run_uid = $2",
            )
            .bind(job.tenant_id.0)
            .bind(job.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "external job callback references a missing execution run".to_string(),
            })?;
            let run_is_terminal = matches!(
                run.1.as_str(),
                "completed" | "partial" | "blocked" | "unsupported" | "failed" | "cancelled"
            );
            let run_is_paused = matches!(run.1.as_str(), "pause_requested" | "pausing" | "paused");
            replace_external_reconcile_trigger_in_conn(
                conn.as_mut(),
                config,
                job,
                super::to_u64(run.0, "controller generation")?,
            )
            .await?;
            if job.state.is_terminal() && !deferred_release && !run_is_terminal && !run_is_paused {
                Some(
                    enqueue_run_activation_in_conn(
                        conn.as_mut(),
                        job.tenant_id,
                        job.run_uid,
                        super::to_u64(run.0, "controller generation")?,
                        Utc::now(),
                        json!({
                            "source": "external_job_callback",
                            "external_job_uid": job.external_job_uid,
                            "provider_event_id": callback.provider_event_id,
                        }),
                    )
                    .await?,
                )
            } else {
                None
            }
        } else {
            None
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionExternalJobCallbackWrite {
            outcome,
            activation,
        })
    }

    /// Settles one provider cancellation response under its exact job generation.
    pub async fn settle_external_job_cancellation(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        cancellation: ExecutionExternalJobCancellation,
    ) -> Result<ExecutionExternalJobCancellationOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        if let Some(tenant_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_external_job WHERE external_job_uid=$1",
        )
        .bind(cancellation.external_job_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        {
            prelock_capacity_dimensions_in_tx(
                conn.as_mut(),
                config,
                TenantId(tenant_id),
                &[
                    ExecutionCapacityDimension::ActiveRuns,
                    ExecutionCapacityDimension::ParkedRuns,
                    ExecutionCapacityDimension::ScheduledTriggers,
                    ExecutionCapacityDimension::ExternalJobs,
                ],
            )
            .await?;
        }
        let outcome =
            settle_external_job_cancellation_in_conn(conn.as_mut(), &cancellation).await?;
        if let ExecutionExternalJobCancellationOutcome::Applied(job) = &outcome {
            let (controller_generation, status) = sqlx::query_as::<_, (i64, String)>(
                "SELECT controller_generation,status FROM moa.execution_run \
                 WHERE tenant_id=$1 AND run_uid=$2",
            )
            .bind(job.tenant_id.0)
            .bind(job.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            replace_external_reconcile_trigger_in_conn(
                conn.as_mut(),
                config,
                job,
                super::to_u64(controller_generation, "controller generation")?,
            )
            .await?;
            let deferred_release = if job.state.is_terminal() {
                settle_external_job_owner_terminal_in_conn(&mut conn, job, Utc::now()).await?
            } else {
                false
            };
            if job.state.is_terminal()
                && !deferred_release
                && !matches!(
                    status.as_str(),
                    "pause_requested"
                        | "pausing"
                        | "paused"
                        | "completed"
                        | "partial"
                        | "blocked"
                        | "unsupported"
                        | "failed"
                        | "cancelled"
                )
            {
                enqueue_run_activation_in_conn(
                    conn.as_mut(),
                    job.tenant_id,
                    job.run_uid,
                    super::to_u64(controller_generation, "controller generation")?,
                    Utc::now(),
                    json!({
                        "source": "external_job_cancellation",
                        "external_job_uid": job.external_job_uid,
                    }),
                )
                .await?;
            }
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Lists a bounded indexed page of active provider jobs due for reconciliation.
    pub async fn list_due_external_jobs(
        &self,
        scope: ExecutionScope,
        batch_size: u32,
    ) -> Result<Vec<ExecutionExternalJobRecord>> {
        if batch_size == 0 || batch_size > MAX_RECONCILE_BATCH_SIZE {
            return Err(Error::InvalidRepositoryInput {
                message: format!(
                    "execution external-job reconciliation batch must be 1..={MAX_RECONCILE_BATCH_SIZE}"
                ),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let rows = sqlx::query(
            r#"
            SELECT *
            FROM moa.execution_external_job
            WHERE state IN ('starting', 'running', 'waiting_reconcile', 'cancel_requested')
              AND next_reconcile_at IS NOT NULL
              AND next_reconcile_at <= now()
            ORDER BY next_reconcile_at, tenant_id, external_job_uid
            LIMIT $1
            "#,
        )
        .bind(i64::from(batch_size))
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let records = rows
            .iter()
            .map(external_job_from_row)
            .collect::<Result<_>>()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(records)
    }
}

async fn replace_external_reconcile_trigger_in_conn(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    job: &ExecutionExternalJobRecord,
    controller_generation: u64,
) -> Result<()> {
    let (
        task_id,
        attempt_generation,
        compensation_id,
        compensation_generation,
        compensation_attempt_generation,
    ) = external_job_trigger_owner(job.owner);
    let existing = sqlx::query_as::<
        _,
        (
            Uuid,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            DateTime<Utc>,
            Value,
        ),
    >(
        "SELECT trigger_uid, controller_generation, attempt_generation, \
                compensation_generation, compensation_attempt_generation, due_at, payload \
         FROM moa.execution_trigger WHERE tenant_id=$1 AND run_uid=$2 \
           AND task_id IS NOT DISTINCT FROM $3 \
           AND compensation_id IS NOT DISTINCT FROM $4 \
           AND trigger_kind='external_reconcile' AND state = 'pending' \
         FOR UPDATE",
    )
    .bind(job.tenant_id.0)
    .bind(job.run_uid)
    .bind(task_id)
    .bind(compensation_id)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if let Some((
        trigger_uid,
        trigger_generation,
        existing_attempt_generation,
        existing_compensation_generation,
        existing_compensation_attempt_generation,
        due_at,
        payload,
    )) = existing
    {
        let exact_replay = job.next_reconcile_at == Some(due_at)
            && payload
                .get("external_job_uid")
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                == Some(job.external_job_uid)
            && payload.get("job_generation").and_then(Value::as_u64) == Some(job.job_generation);
        if exact_replay {
            return Ok(());
        }
        supersede_trigger_in_conn(
            conn,
            trigger_uid,
            ExecutionTriggerKind::ExternalReconcile,
            trigger_generation
                .map(|value| super::to_u64(value, "controller generation"))
                .transpose()?,
            existing_attempt_generation
                .map(|value| super::to_u64(value, "attempt generation"))
                .transpose()?,
            existing_compensation_generation
                .map(|value| super::to_u64(value, "compensation generation"))
                .transpose()?,
            existing_compensation_attempt_generation
                .map(|value| super::to_u64(value, "compensation attempt generation"))
                .transpose()?,
        )
        .await?;
    }
    let Some(next_reconcile_at) = job.next_reconcile_at else {
        return Ok(());
    };
    if job.state.is_terminal() {
        return Err(Error::InvalidRepositoryData {
            message: "terminal external job retained a reconciliation deadline".to_string(),
        });
    }
    let identity = format!(
        "{}:{}:{}",
        job.external_job_uid,
        job.job_generation,
        job.updated_at.timestamp_micros()
    );
    create_trigger_with_dispatch_in_conn(
        conn,
        config,
        &NewExecutionTrigger {
            trigger_uid: Uuid::new_v5(&EXTERNAL_RECONCILE_TRIGGER_NAMESPACE, identity.as_bytes()),
            tenant_id: job.tenant_id,
            run_uid: Some(job.run_uid),
            task_id,
            compensation_id,
            schedule_uid: None,
            schedule_incarnation: None,
            kind: ExecutionTriggerKind::ExternalReconcile,
            controller_generation: Some(controller_generation),
            attempt_generation,
            compensation_generation,
            compensation_attempt_generation,
            occurrence_sequence: None,
            due_at: next_reconcile_at,
            payload: json!({
                "external_job_uid": job.external_job_uid,
                "job_generation": job.job_generation,
            }),
        },
    )
    .await?;
    Ok(())
}

fn external_start_recovery_trigger_uid(external_job_uid: Uuid, job_generation: u64) -> Uuid {
    Uuid::new_v5(
        &EXTERNAL_START_RECOVERY_TRIGGER_NAMESPACE,
        format!("{external_job_uid}:{job_generation}").as_bytes(),
    )
}

async fn create_external_start_recovery_trigger_in_conn(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    intent: &NewExecutionExternalJobIntent,
    controller_generation: u64,
) -> Result<()> {
    let (
        task_id,
        attempt_generation,
        compensation_id,
        compensation_generation,
        compensation_attempt_generation,
    ) = external_job_trigger_owner(intent.owner);
    create_trigger_with_dispatch_in_conn(
        conn,
        config,
        &NewExecutionTrigger {
            trigger_uid: external_start_recovery_trigger_uid(
                intent.external_job_uid,
                intent.job_generation,
            ),
            tenant_id: intent.tenant_id,
            run_uid: Some(intent.run_uid),
            task_id,
            compensation_id,
            schedule_uid: None,
            schedule_incarnation: None,
            kind: ExecutionTriggerKind::ExternalStartRecovery,
            controller_generation: Some(controller_generation),
            attempt_generation,
            compensation_generation,
            compensation_attempt_generation,
            occurrence_sequence: None,
            due_at: intent.expires_at,
            payload: json!({
                "external_job_uid": intent.external_job_uid,
                "job_generation": intent.job_generation,
                "declared_provider": intent.provider,
                "idempotency_key": intent.idempotency_key,
            }),
        },
    )
    .await?;
    Ok(())
}

async fn settle_external_start_recovery_trigger_in_conn(
    conn: &mut PgConnection,
    job: &ExecutionExternalJobRecord,
) -> Result<()> {
    let trigger_uid = external_start_recovery_trigger_uid(job.external_job_uid, job.job_generation);
    let row = sqlx::query_as::<_, (Option<i64>, Option<i64>, Option<i64>, Option<i64>)>(
        "SELECT controller_generation,attempt_generation,compensation_generation, \
                compensation_attempt_generation \
         FROM moa.execution_trigger WHERE trigger_uid=$1",
    )
    .bind(trigger_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some((controller, attempt, compensation, compensation_attempt)) = row else {
        return Err(Error::InvalidRepositoryData {
            message: "external job intent is missing its start-recovery trigger".to_string(),
        });
    };
    supersede_trigger_in_conn(
        conn,
        trigger_uid,
        ExecutionTriggerKind::ExternalStartRecovery,
        controller
            .map(|value| super::to_u64(value, "controller generation"))
            .transpose()?,
        attempt
            .map(|value| super::to_u64(value, "attempt generation"))
            .transpose()?,
        compensation
            .map(|value| super::to_u64(value, "compensation generation"))
            .transpose()?,
        compensation_attempt
            .map(|value| super::to_u64(value, "compensation attempt generation"))
            .transpose()?,
    )
    .await?;
    Ok(())
}

/// Reserves an unbound external-job intent without committing the caller transaction.
pub async fn reserve_external_job_intent_in_conn(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    intent: &NewExecutionExternalJobIntent,
) -> Result<ExecutionExternalJobRecord> {
    validate_external_job_intent(intent, true)?;
    lock_external_job_intent_owner_in_conn(conn, intent).await?;
    let (
        task_id,
        attempt_generation,
        compensation_id,
        compensation_generation,
        compensation_attempt_generation,
    ) = external_job_owner_columns(intent.owner)?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO moa.execution_external_job (
            external_job_uid, tenant_id, run_uid, task_id, attempt_generation,
            compensation_id, compensation_generation, compensation_attempt_generation,
            job_generation, declared_provider, idempotency_key, state, cancel_supported
        )
        SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'unbound', FALSE
        WHERE (
            ($4 IS NOT NULL AND EXISTS (
                SELECT 1 FROM moa.execution_task AS task
                WHERE task.task_id = $4 AND task.run_uid = $3 AND task.tenant_id = $2
                  AND task.attempt_generation = $5
                  AND (
                    (task.status='running' AND task.attempt_state IN ('running','cancelling'))
                    OR (task.status='waiting_review' AND task.attempt_state='waiting')
                  )
            ))
            OR
            ($6 IS NOT NULL AND EXISTS (
                SELECT 1 FROM moa.execution_compensation AS compensation
                WHERE compensation.compensation_id = $6
                  AND compensation.run_uid = $3 AND compensation.tenant_id = $2
                  AND compensation.generation = $7
                  AND compensation.attempt_generation = $8
                  AND compensation.status = 'running'
                  AND compensation.attempt_state IN ('running','cancelling','waiting_review')
            ))
        )
        ON CONFLICT (external_job_uid) DO NOTHING
        RETURNING *
        "#,
    )
    .bind(intent.external_job_uid)
    .bind(intent.tenant_id.0)
    .bind(intent.run_uid)
    .bind(task_id)
    .bind(attempt_generation)
    .bind(compensation_id)
    .bind(compensation_generation)
    .bind(compensation_attempt_generation)
    .bind(to_i64(intent.job_generation, "external job generation")?)
    .bind(&intent.provider)
    .bind(&intent.idempotency_key)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let row = match inserted {
        Some(row) => row,
        None => sqlx::query("SELECT * FROM moa.execution_external_job WHERE external_job_uid = $1")
            .bind(intent.external_job_uid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_error)?
            .ok_or_else(|| Error::InvalidRepositoryInput {
                message: "external job owner generation is not current".to_string(),
            })?,
    };
    let record = external_job_from_row(&row)?;
    if !external_job_matches_intent(&record, intent) {
        return Err(Error::InvalidRepositoryInput {
            message: "external job intent UID is already bound to different immutable semantics"
                .to_string(),
        });
    }
    let controller_generation = sqlx::query_scalar::<_, i64>(
        "SELECT controller_generation FROM moa.execution_run \
         WHERE tenant_id = $1 AND run_uid = $2 \
           AND status NOT IN ('completed','partial','blocked','unsupported','failed','cancelled')",
    )
    .bind(record.tenant_id.0)
    .bind(record.run_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryInput {
        message: "external job intent run is missing or terminal".to_string(),
    })?;
    let controller_generation = super::to_u64(controller_generation, "controller generation")?;
    let capacity =
        external_job_capacity_request(&record, controller_generation, Some(intent.expires_at));
    if reserve_capacity_in_tx(conn, config, capacity).await? == CapacityReserveOutcome::Saturated {
        return Err(Error::CapacitySaturated {
            dimension: ExecutionCapacityDimension::ExternalJobs.as_str(),
        });
    }
    create_external_start_recovery_trigger_in_conn(conn, config, intent, controller_generation)
        .await?;
    Ok(record)
}

/// Binds provider identity to a live intent without reserving capacity again.
pub async fn bind_external_job_in_conn(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    binding: &ExecutionExternalJobBinding,
) -> Result<ExecutionExternalJobRecord> {
    validate_external_job_binding(binding)?;
    let row = sqlx::query(
        "SELECT job.*, COALESCE(capacity.expires_at > now(), TRUE) AS capacity_live, \
                now() AS observed_at \
         FROM moa.execution_external_job AS job \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.tenant_id=job.tenant_id \
          AND capacity.external_job_uid=job.external_job_uid \
          AND capacity.resource_dimension='external_jobs' AND capacity.released_at IS NULL \
         WHERE job.external_job_uid=$1 FOR UPDATE OF job, capacity",
    )
    .bind(binding.external_job_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryInput {
        message: "external job binding requires a live reserved intent".to_string(),
    })?;
    let current = external_job_from_row(&row)?;
    if !external_job_matches_binding_identity(&current, binding) {
        return Err(Error::InvalidRepositoryInput {
            message: "external job binding does not match its reserved intent".to_string(),
        });
    }
    if current.state != ExecutionExternalJobState::Unbound {
        if external_job_matches_provider_result(&current, binding) {
            return Ok(current);
        }
        return Err(Error::InvalidRepositoryInput {
            message: "external job intent is already bound to different provider semantics"
                .to_string(),
        });
    }
    let capacity_live = row
        .try_get::<bool, _>("capacity_live")
        .map_err(super::row_error)?;
    let observed_at = row
        .try_get::<DateTime<Utc>, _>("observed_at")
        .map_err(super::row_error)?;
    let (controller_generation, run_status) = sqlx::query_as::<_, (i64, String)>(
        "SELECT controller_generation,status FROM moa.execution_run \
         WHERE tenant_id=$1 AND run_uid=$2 FOR UPDATE",
    )
    .bind(current.tenant_id.0)
    .bind(current.run_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: "reserved external job intent references a missing run".to_string(),
    })?;
    let run_is_terminal = matches!(
        run_status.as_str(),
        "completed" | "partial" | "blocked" | "unsupported" | "failed" | "cancelled"
    );
    let provider_mismatch = binding.provider != current.declared_provider;
    let contract_violation = binding.provider_contract_violation.clone().or_else(|| {
        provider_mismatch.then(|| {
            format!(
                "adapter returned provider `{}` for declared provider `{}`",
                binding.provider, current.declared_provider
            )
        })
    });
    let violation_audit = contract_violation.as_ref().map(|detail| {
        json!({
            "kind": "provider_contract_mismatch",
            "observed_at": observed_at.to_rfc3339(),
            "detail": detail,
        })
    });
    let requires_recovery =
        !capacity_live || run_is_terminal || provider_mismatch || contract_violation.is_some();
    let state = if requires_recovery {
        ExecutionExternalJobState::CancelRequested
    } else {
        binding.state
    };
    let next_reconcile_at = if requires_recovery {
        Some(observed_at)
    } else {
        binding.next_reconcile_at
    };
    settle_external_start_recovery_trigger_in_conn(conn, &current).await?;
    let row = sqlx::query(
        r#"
        UPDATE moa.execution_external_job
        SET provider=$2, provider_job_id=$3, callback_auth_reference=$4,
            state=$5, progress_phase=$6, cancel_supported=$7,
            next_reconcile_at=$8, provider_contract_violation=$9, updated_at=now()
        WHERE external_job_uid=$1 AND state='unbound' AND job_generation=$10
        RETURNING *
        "#,
    )
    .bind(binding.external_job_uid)
    .bind(&current.declared_provider)
    .bind(&binding.provider_job_id)
    .bind(&binding.callback_auth_reference)
    .bind(state.as_str())
    .bind(&binding.progress_phase)
    .bind(binding.cancel_supported)
    .bind(next_reconcile_at)
    .bind(&violation_audit)
    .bind(to_i64(binding.job_generation, "external job generation")?)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let record = external_job_from_row(&row)?;
    let controller_generation = super::to_u64(controller_generation, "controller generation")?;
    replace_external_reconcile_trigger_in_conn(conn, config, &record, controller_generation)
        .await?;
    if requires_recovery {
        enqueue_external_cancel_in_conn(conn, &record, controller_generation, observed_at).await?;
    }
    Ok(record)
}

/// Releases an exact unbound intent and its capacity receipt transactionally.
pub async fn release_external_job_intent_in_conn(
    conn: &mut PgConnection,
    intent: &NewExecutionExternalJobIntent,
) -> Result<ExecutionExternalJobIntentReleaseOutcome> {
    validate_external_job_intent(intent, false)?;
    let row = sqlx::query(
        "SELECT * FROM moa.execution_external_job WHERE external_job_uid=$1 FOR UPDATE",
    )
    .bind(intent.external_job_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        let receipt_exists = sqlx::query_scalar::<_, bool>(
            "SELECT TRUE FROM moa.execution_capacity_reservation \
             WHERE reservation_uid=$1 AND released_at IS NULL",
        )
        .bind(execution_capacity_reservation_uid(
            ExecutionCapacityDimension::ExternalJobs,
            intent.external_job_uid,
            None,
        ))
        .fetch_optional(&mut *conn)
        .await
        .map_err(sqlx_error)?
        .unwrap_or(false);
        return Ok(if receipt_exists {
            ExecutionExternalJobIntentReleaseOutcome::Stale
        } else {
            ExecutionExternalJobIntentReleaseOutcome::AlreadyReleased
        });
    };
    let record = external_job_from_row(&row)?;
    if !external_job_matches_intent(&record, intent) {
        return Ok(ExecutionExternalJobIntentReleaseOutcome::Stale);
    }
    if record.state != ExecutionExternalJobState::Unbound {
        return Ok(ExecutionExternalJobIntentReleaseOutcome::AlreadyBound);
    }
    settle_external_start_recovery_trigger_in_conn(conn, &record).await?;
    release_external_job_capacity_in_conn(conn, &record).await?;
    let deleted = sqlx::query(
        "DELETE FROM moa.execution_external_job \
         WHERE external_job_uid=$1 AND job_generation=$2 AND state='unbound'",
    )
    .bind(intent.external_job_uid)
    .bind(to_i64(intent.job_generation, "external job generation")?)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    if deleted.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: "external job intent release lost its exact state fence".to_string(),
        });
    }
    Ok(ExecutionExternalJobIntentReleaseOutcome::Released)
}

/// Loads and locks one external job inside a caller-owned transaction.
pub(super) async fn load_external_job_for_update_in_conn(
    conn: &mut PgConnection,
    external_job_uid: Uuid,
) -> Result<Option<ExecutionExternalJobRecord>> {
    let row = sqlx::query(
        "SELECT * FROM moa.execution_external_job WHERE external_job_uid=$1 FOR UPDATE",
    )
    .bind(external_job_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    row.as_ref().map(external_job_from_row).transpose()
}

/// Applies a callback without committing the caller-owned transaction.
pub async fn apply_external_job_callback_in_conn(
    conn: &mut PgConnection,
    callback: &ExecutionExternalJobCallback,
) -> Result<ExecutionExternalJobCallbackOutcome> {
    validate_callback(callback)?;
    let row = sqlx::query(
        "SELECT * FROM moa.execution_external_job WHERE external_job_uid = $1 FOR UPDATE",
    )
    .bind(callback.external_job_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(ExecutionExternalJobCallbackOutcome::NotFound);
    };
    let record = external_job_from_row(&row)?;
    if record.state == ExecutionExternalJobState::Unbound
        || record.job_generation != callback.job_generation
        || record.provider.as_deref() != Some(callback.provider.as_str())
        || record.provider_job_id.as_deref() != Some(callback.provider_job_id.as_str())
    {
        return Ok(ExecutionExternalJobCallbackOutcome::StaleGeneration);
    }
    if record.last_provider_event_id.as_deref() == Some(callback.provider_event_id.as_str()) {
        return Ok(ExecutionExternalJobCallbackOutcome::Duplicate);
    }
    if record.state.is_terminal() {
        return Ok(ExecutionExternalJobCallbackOutcome::AlreadyTerminal);
    }
    let receipt_inserted = sqlx::query_scalar::<_, bool>(
        r#"
        INSERT INTO moa.execution_external_job_callback_receipt (
            tenant_id, external_job_uid, provider, provider_event_id, job_generation
        ) VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT DO NOTHING
        RETURNING TRUE
        "#,
    )
    .bind(record.tenant_id.0)
    .bind(record.external_job_uid)
    .bind(&callback.provider)
    .bind(&callback.provider_event_id)
    .bind(to_i64(callback.job_generation, "external job generation")?)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .is_some();
    if !receipt_inserted {
        return Ok(ExecutionExternalJobCallbackOutcome::Duplicate);
    }
    let row = match &callback.update {
        ExecutionExternalJobCallbackUpdate::Progress {
            state,
            progress_phase,
            next_reconcile_at,
        } => sqlx::query(
            r#"
                UPDATE moa.execution_external_job
                SET state = $2, progress_phase = $3, next_reconcile_at = $4,
                    last_provider_event_id = $5, updated_at = now()
                WHERE external_job_uid = $1 AND job_generation = $6
                  AND state IN ('starting', 'running', 'waiting_reconcile', 'cancel_requested')
                RETURNING *
                "#,
        )
        .bind(callback.external_job_uid)
        .bind(state.as_str())
        .bind(progress_phase)
        .bind(next_reconcile_at)
        .bind(&callback.provider_event_id)
        .bind(to_i64(callback.job_generation, "external job generation")?)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlx_error)?,
        ExecutionExternalJobCallbackUpdate::Terminal {
            state,
            progress_phase,
            output,
            error,
        } => sqlx::query(
            r#"
                UPDATE moa.execution_external_job
                SET state = $2, progress_phase = $3, next_reconcile_at = NULL,
                    last_provider_event_id = $4, output = $5, error = $6,
                    completed_at = now(), updated_at = now()
                WHERE external_job_uid = $1 AND job_generation = $7
                  AND state IN ('starting', 'running', 'waiting_reconcile', 'cancel_requested')
                RETURNING *
                "#,
        )
        .bind(callback.external_job_uid)
        .bind(state.as_str())
        .bind(progress_phase)
        .bind(&callback.provider_event_id)
        .bind(output)
        .bind(error)
        .bind(to_i64(callback.job_generation, "external job generation")?)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlx_error)?,
    };
    let record = external_job_from_row(&row)?;
    if record.state.is_terminal() {
        release_external_job_capacity_in_conn(conn, &record).await?;
    }
    Ok(ExecutionExternalJobCallbackOutcome::Applied(Box::new(
        record,
    )))
}

/// Settles provider cancellation without committing the caller-owned transaction.
pub async fn settle_external_job_cancellation_in_conn(
    conn: &mut PgConnection,
    cancellation: &ExecutionExternalJobCancellation,
) -> Result<ExecutionExternalJobCancellationOutcome> {
    validate_cancellation(cancellation)?;
    let row = sqlx::query(
        "SELECT * FROM moa.execution_external_job WHERE external_job_uid = $1 FOR UPDATE",
    )
    .bind(cancellation.external_job_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(ExecutionExternalJobCancellationOutcome::NotFound);
    };
    let record = external_job_from_row(&row)?;
    if record.state == ExecutionExternalJobState::Unbound
        || record.job_generation != cancellation.job_generation
        || record.provider.as_deref() != Some(cancellation.provider.as_str())
        || record.provider_job_id.as_deref() != Some(cancellation.provider_job_id.as_str())
    {
        return Ok(ExecutionExternalJobCancellationOutcome::StaleGeneration);
    }
    if record.state.is_terminal() {
        return Ok(ExecutionExternalJobCancellationOutcome::AlreadyTerminal);
    }
    let completed_at = cancellation.state.is_terminal().then(Utc::now);
    let row = sqlx::query(
        r#"
        UPDATE moa.execution_external_job
        SET state = $2, next_reconcile_at = $3, error = $4, completed_at = $5,
            updated_at = now()
        WHERE external_job_uid = $1 AND job_generation = $6
          AND state IN ('starting', 'running', 'waiting_reconcile', 'cancel_requested')
        RETURNING *
        "#,
    )
    .bind(cancellation.external_job_uid)
    .bind(cancellation.state.as_str())
    .bind(cancellation.next_reconcile_at)
    .bind(&cancellation.error)
    .bind(completed_at)
    .bind(to_i64(
        cancellation.job_generation,
        "external job generation",
    )?)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let record = external_job_from_row(&row)?;
    if record.state.is_terminal() {
        release_external_job_capacity_in_conn(conn, &record).await?;
    }
    Ok(ExecutionExternalJobCancellationOutcome::Applied(Box::new(
        record,
    )))
}

async fn settle_external_job_owner_terminal_in_conn(
    conn: &mut super::ScopedConn<'_>,
    job: &ExecutionExternalJobRecord,
    settled_at: DateTime<Utc>,
) -> Result<bool> {
    match job.owner {
        ExecutionExternalJobOwner::Task { .. } => {
            match settle_task_external_job_terminal_in_conn(conn, job, settled_at).await? {
                ExternalJobTaskSettlementOutcome::Applied(_)
                | ExternalJobTaskSettlementOutcome::Replayed(_) => Ok(false),
                ExternalJobTaskSettlementOutcome::DeferredRelease(_) => Ok(true),
                ExternalJobTaskSettlementOutcome::Stale
                | ExternalJobTaskSettlementOutcome::NotFound => Err(Error::InvalidRepositoryData {
                    message: "terminal external job lost its exact task-attempt owner fence"
                        .to_string(),
                }),
            }
        }
        ExecutionExternalJobOwner::Compensation { .. } => {
            match settle_compensation_external_job_terminal_in_conn(conn, job, settled_at).await? {
                CompensationExternalJobSettlementOutcome::Applied(_)
                | CompensationExternalJobSettlementOutcome::Replayed(_) => Ok(false),
                CompensationExternalJobSettlementOutcome::DeferredRelease(_) => Ok(true),
                CompensationExternalJobSettlementOutcome::Stale
                | CompensationExternalJobSettlementOutcome::NotFound => {
                    Err(Error::InvalidRepositoryData {
                        message:
                            "terminal external job lost its exact compensation-attempt owner fence"
                                .to_string(),
                    })
                }
            }
        }
    }
}

async fn enqueue_external_cancel_in_conn(
    conn: &mut PgConnection,
    job: &ExecutionExternalJobRecord,
    controller_generation: u64,
    not_before_at: DateTime<Utc>,
) -> Result<ExecutionDispatchRecord> {
    let provider = job
        .provider
        .as_ref()
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "bound external job is missing provider identity".to_string(),
        })?;
    let provider_job_id =
        job.provider_job_id
            .as_ref()
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "bound external job is missing provider job identity".to_string(),
            })?;
    let (task_id, attempt_generation, compensation_id, compensation_generation, comp_attempt) =
        external_job_trigger_owner(job.owner);
    let payload = ExecutionExternalJobCancelRequest {
        tenant_id: job.tenant_id,
        external_job_uid: job.external_job_uid,
        job_generation: job.job_generation,
        provider: provider.clone(),
        provider_job_id: provider_job_id.clone(),
        idempotency_key: job.idempotency_key.clone(),
    };
    enqueue_dispatch_in_conn(
        conn,
        &NewExecutionDispatch {
            dispatch_uid: Uuid::new_v5(
                &EXTERNAL_CANCEL_DISPATCH_NAMESPACE,
                format!("{}:{}", job.external_job_uid, job.job_generation).as_bytes(),
            ),
            tenant_id: job.tenant_id,
            run_uid: Some(job.run_uid),
            task_id,
            compensation_id,
            trigger_uid: None,
            external_job_uid: Some(job.external_job_uid),
            kind: ExecutionDispatchKind::ExternalCancel,
            controller_generation: Some(controller_generation),
            wake_epoch: None,
            attempt_generation,
            compensation_generation,
            compensation_attempt_generation: comp_attempt,
            not_before_at,
            payload: serde_json::to_value(payload).map_err(|error| {
                Error::InvalidRepositoryInput {
                    message: format!("failed to encode external cancel request: {error}"),
                }
            })?,
        },
    )
    .await
}

/// Requests cancellation for one exact bound owner inside a caller transaction.
pub(super) async fn request_external_job_cancellation_in_conn(
    conn: &mut super::ScopedConn<'_>,
    config: &ExecutionConfig,
    external_job_uid: Uuid,
    expected_owner: ExecutionExternalJobOwner,
    requested_at: DateTime<Utc>,
) -> Result<ExecutionExternalJobCancellationRequestOutcome> {
    let Some(job) = load_external_job_for_update_in_conn(conn.as_mut(), external_job_uid).await?
    else {
        return Ok(ExecutionExternalJobCancellationRequestOutcome::NotFound);
    };
    if job.owner != expected_owner {
        return Ok(ExecutionExternalJobCancellationRequestOutcome::Stale);
    }
    if job.state == ExecutionExternalJobState::Unbound {
        return Ok(ExecutionExternalJobCancellationRequestOutcome::UnboundPendingRecovery);
    }
    if job.state.is_terminal() {
        return Ok(ExecutionExternalJobCancellationRequestOutcome::AlreadyTerminal);
    }
    let replayed = job.state == ExecutionExternalJobState::CancelRequested;
    let row = if replayed {
        job
    } else {
        let row = sqlx::query(
            "UPDATE moa.execution_external_job SET state='cancel_requested', \
             next_reconcile_at=COALESCE(next_reconcile_at,$2),updated_at=now() \
             WHERE external_job_uid=$1 AND job_generation=$3 \
               AND state IN ('starting','running','waiting_reconcile') RETURNING *",
        )
        .bind(external_job_uid)
        .bind(requested_at)
        .bind(to_i64(job.job_generation, "external job generation")?)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        external_job_from_row(&row)?
    };
    let controller_generation = sqlx::query_scalar::<_, i64>(
        "SELECT controller_generation FROM moa.execution_run \
         WHERE tenant_id=$1 AND run_uid=$2 FOR UPDATE",
    )
    .bind(row.tenant_id.0)
    .bind(row.run_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let controller_generation = super::to_u64(controller_generation, "controller generation")?;
    replace_external_reconcile_trigger_in_conn(conn.as_mut(), config, &row, controller_generation)
        .await?;
    let dispatch =
        enqueue_external_cancel_in_conn(conn.as_mut(), &row, controller_generation, requested_at)
            .await?;
    Ok(if replayed {
        ExecutionExternalJobCancellationRequestOutcome::Replayed(dispatch)
    } else {
        ExecutionExternalJobCancellationRequestOutcome::Applied(dispatch)
    })
}

fn external_job_capacity_request(
    job: &ExecutionExternalJobRecord,
    controller_generation: u64,
    expires_at: Option<DateTime<Utc>>,
) -> ExecutionCapacityRequest {
    ExecutionCapacityRequest {
        reservation_uid: execution_capacity_reservation_uid(
            ExecutionCapacityDimension::ExternalJobs,
            job.external_job_uid,
            None,
        ),
        tenant_id: job.tenant_id,
        run_uid: Some(job.run_uid),
        controller_generation: Some(controller_generation),
        dimension: ExecutionCapacityDimension::ExternalJobs,
        owner: ExecutionCapacityOwner::ExternalJob {
            external_job_uid: job.external_job_uid,
        },
        expires_at,
    }
}

fn external_job_owner_from_recovery_request(
    request: &ExecutionExternalJobStartRecoveryRequest,
) -> ExecutionExternalJobOwner {
    match request.owner {
        ExecutionExternalJobStartRecoveryOwner::Task {
            task_id,
            attempt_generation,
        } => ExecutionExternalJobOwner::Task {
            task_id,
            attempt_generation,
        },
        ExecutionExternalJobStartRecoveryOwner::Compensation {
            compensation_id,
            compensation_generation,
            compensation_attempt_generation,
        } => ExecutionExternalJobOwner::Compensation {
            compensation_id,
            compensation_generation,
            compensation_attempt_generation,
        },
    }
}

fn recovery_adoption_committed(outcome: &ExecutionExternalJobStartRecoveryAdoptionOutcome) -> bool {
    matches!(
        outcome,
        ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied { .. }
            | ExecutionExternalJobStartRecoveryAdoptionOutcome::Replayed { .. }
            | ExecutionExternalJobStartRecoveryAdoptionOutcome::AlreadySettled
    )
}

fn external_job_intent_from_recovery_request(
    request: &ExecutionExternalJobStartRecoveryRequest,
) -> NewExecutionExternalJobIntent {
    NewExecutionExternalJobIntent {
        external_job_uid: request.external_job_uid,
        tenant_id: request.tenant_id,
        run_uid: request.run_uid,
        owner: external_job_owner_from_recovery_request(request),
        job_generation: request.job_generation,
        provider: request.provider.clone(),
        idempotency_key: request.idempotency_key.clone(),
        // Expiry is not an immutable intent coordinate. Recovery authorization comes from the
        // exact trigger payload and the provider's journaled NotStarted/Started disposition.
        expires_at: DateTime::<Utc>::MAX_UTC,
    }
}

fn map_task_not_started_recovery(
    outcome: TaskExternalStartRetryOutcome,
) -> ExecutionExternalJobStartRecoveryAdoptionOutcome {
    match outcome {
        TaskExternalStartRetryOutcome::Applied { .. } => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied {
                compensation_release: None,
            }
        }
        TaskExternalStartRetryOutcome::Replayed { .. } => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Replayed {
                compensation_release: None,
            }
        }
        TaskExternalStartRetryOutcome::NotFound => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::NotFound
        }
        TaskExternalStartRetryOutcome::Stale => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Stale
        }
        TaskExternalStartRetryOutcome::InvalidState => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::InvalidState
        }
    }
}

fn map_task_started_recovery(
    outcome: TaskAttemptExternalOutcome,
) -> ExecutionExternalJobStartRecoveryAdoptionOutcome {
    match outcome {
        TaskAttemptExternalOutcome::Applied { .. } => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied {
                compensation_release: None,
            }
        }
        TaskAttemptExternalOutcome::Replayed { .. } => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Replayed {
                compensation_release: None,
            }
        }
        TaskAttemptExternalOutcome::NotFound => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::NotFound
        }
        TaskAttemptExternalOutcome::Stale => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Stale
        }
        TaskAttemptExternalOutcome::InvalidState => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::InvalidState
        }
    }
}

fn map_compensation_not_started_recovery(
    outcome: CompensationExternalNotStartedReleaseClaimOutcome,
) -> ExecutionExternalJobStartRecoveryAdoptionOutcome {
    match outcome {
        CompensationExternalNotStartedReleaseClaimOutcome::Applied { request, .. } => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied {
                compensation_release: Some(Box::new(request)),
            }
        }
        CompensationExternalNotStartedReleaseClaimOutcome::Replayed { request, .. } => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Replayed {
                compensation_release: Some(Box::new(request)),
            }
        }
        CompensationExternalNotStartedReleaseClaimOutcome::AlreadySettled => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::AlreadySettled
        }
        CompensationExternalNotStartedReleaseClaimOutcome::NotFound => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::NotFound
        }
        CompensationExternalNotStartedReleaseClaimOutcome::Stale => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Stale
        }
        CompensationExternalNotStartedReleaseClaimOutcome::InvalidState => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::InvalidState
        }
    }
}

fn map_compensation_started_recovery(
    outcome: CompensationRecoveredExternalReleaseClaimOutcome,
) -> ExecutionExternalJobStartRecoveryAdoptionOutcome {
    match outcome {
        CompensationRecoveredExternalReleaseClaimOutcome::Applied { request, .. } => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied {
                compensation_release: Some(Box::new(request)),
            }
        }
        CompensationRecoveredExternalReleaseClaimOutcome::Replayed { request, .. } => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Replayed {
                compensation_release: Some(Box::new(request)),
            }
        }
        CompensationRecoveredExternalReleaseClaimOutcome::AlreadySettled => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::AlreadySettled
        }
        CompensationRecoveredExternalReleaseClaimOutcome::NotFound => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::NotFound
        }
        CompensationRecoveredExternalReleaseClaimOutcome::Stale => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::Stale
        }
        CompensationRecoveredExternalReleaseClaimOutcome::InvalidState => {
            ExecutionExternalJobStartRecoveryAdoptionOutcome::InvalidState
        }
    }
}

type ExternalJobTriggerOwnerColumns = (
    Option<Uuid>,
    Option<u64>,
    Option<Uuid>,
    Option<u64>,
    Option<u64>,
);

type ExternalJobSqlOwnerColumns = (
    Option<Uuid>,
    Option<i64>,
    Option<Uuid>,
    Option<i64>,
    Option<i64>,
);

fn external_job_trigger_owner(owner: ExecutionExternalJobOwner) -> ExternalJobTriggerOwnerColumns {
    match owner {
        ExecutionExternalJobOwner::Task {
            task_id,
            attempt_generation,
        } => (Some(task_id), Some(attempt_generation), None, None, None),
        ExecutionExternalJobOwner::Compensation {
            compensation_id,
            compensation_generation,
            compensation_attempt_generation,
        } => (
            None,
            None,
            Some(compensation_id),
            Some(compensation_generation),
            Some(compensation_attempt_generation),
        ),
    }
}

fn external_job_owner_columns(
    owner: ExecutionExternalJobOwner,
) -> Result<ExternalJobSqlOwnerColumns> {
    let (task_id, attempt, compensation_id, generation, compensation_attempt) =
        external_job_trigger_owner(owner);
    Ok((
        task_id,
        attempt
            .map(|value| to_i64(value, "attempt generation"))
            .transpose()?,
        compensation_id,
        generation
            .map(|value| to_i64(value, "compensation generation"))
            .transpose()?,
        compensation_attempt
            .map(|value| to_i64(value, "compensation attempt generation"))
            .transpose()?,
    ))
}

async fn lock_external_job_intent_owner_in_conn(
    conn: &mut PgConnection,
    intent: &NewExecutionExternalJobIntent,
) -> Result<()> {
    let current = match intent.owner {
        ExecutionExternalJobOwner::Task {
            task_id,
            attempt_generation,
        } => sqlx::query_scalar::<_, bool>(
            r#"
                SELECT TRUE FROM moa.execution_task
                WHERE tenant_id=$1 AND run_uid=$2 AND task_id=$3
                  AND attempt_generation=$4
                  AND (
                    (status='running' AND attempt_state IN ('running','cancelling'))
                    OR (status='waiting_review' AND attempt_state='waiting')
                  )
                FOR UPDATE
                "#,
        )
        .bind(intent.tenant_id.0)
        .bind(intent.run_uid)
        .bind(task_id)
        .bind(to_i64(attempt_generation, "attempt generation")?)
        .fetch_optional(&mut *conn)
        .await
        .map_err(sqlx_error)?
        .unwrap_or(false),
        ExecutionExternalJobOwner::Compensation {
            compensation_id,
            compensation_generation,
            compensation_attempt_generation,
        } => sqlx::query_scalar::<_, bool>(
            r#"
                SELECT TRUE FROM moa.execution_compensation
                WHERE tenant_id=$1 AND run_uid=$2 AND compensation_id=$3
                  AND generation=$4 AND attempt_generation=$5
                  AND status='running'
                  AND attempt_state IN ('running','cancelling','waiting_review')
                FOR UPDATE
                "#,
        )
        .bind(intent.tenant_id.0)
        .bind(intent.run_uid)
        .bind(compensation_id)
        .bind(to_i64(compensation_generation, "compensation generation")?)
        .bind(to_i64(
            compensation_attempt_generation,
            "compensation attempt generation",
        )?)
        .fetch_optional(&mut *conn)
        .await
        .map_err(sqlx_error)?
        .unwrap_or(false),
    };
    if !current {
        return Err(Error::InvalidRepositoryInput {
            message: "external job intent owner generation is not dispatchable".to_string(),
        });
    }
    Ok(())
}

async fn release_external_job_capacity_in_conn(
    conn: &mut PgConnection,
    job: &ExecutionExternalJobRecord,
) -> Result<()> {
    let reservation_uid = execution_capacity_reservation_uid(
        ExecutionCapacityDimension::ExternalJobs,
        job.external_job_uid,
        None,
    );
    let controller_generation = sqlx::query_scalar::<_, i64>(
        "SELECT controller_generation FROM moa.execution_capacity_reservation \
         WHERE reservation_uid = $1 AND tenant_id = $2 AND external_job_uid = $3",
    )
    .bind(reservation_uid)
    .bind(job.tenant_id.0)
    .bind(job.external_job_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?
    .ok_or_else(|| Error::InvalidRepositoryData {
        message: "terminal external job is missing its capacity reservation".to_string(),
    })?;
    let request = external_job_capacity_request(
        job,
        super::to_u64(controller_generation, "controller generation")?,
        None,
    );
    match release_capacity_in_tx(conn, request).await? {
        super::capacity::CapacityReleaseOutcome::Released
        | super::capacity::CapacityReleaseOutcome::AlreadyReleased => Ok(()),
        super::capacity::CapacityReleaseOutcome::NotFound
        | super::capacity::CapacityReleaseOutcome::Stale => Err(Error::InvalidRepositoryData {
            message: "external-job capacity release lost its exact owner fence".to_string(),
        }),
    }
}

fn validate_external_job_intent(
    intent: &NewExecutionExternalJobIntent,
    require_future_expiry: bool,
) -> Result<()> {
    let owner_is_valid = match intent.owner {
        ExecutionExternalJobOwner::Task {
            task_id,
            attempt_generation,
        } => !task_id.is_nil() && attempt_generation > 0,
        ExecutionExternalJobOwner::Compensation {
            compensation_id,
            compensation_generation,
            compensation_attempt_generation,
        } => {
            !compensation_id.is_nil()
                && compensation_generation > 0
                && compensation_attempt_generation > 0
        }
    };
    if intent.external_job_uid.is_nil()
        || intent.idempotency_key.trim().is_empty()
        || intent.idempotency_key.len() > 256
        || intent.provider.trim().is_empty()
        || intent.provider.len() > 128
        || !owner_is_valid
        || intent.job_generation == 0
        || (require_future_expiry && intent.expires_at <= Utc::now())
    {
        return Err(Error::InvalidRepositoryInput {
            message: "external job intent requires exact owner identity and a future expiry"
                .to_string(),
        });
    }
    Ok(())
}

fn validate_external_job_binding(binding: &ExecutionExternalJobBinding) -> Result<()> {
    validate_external_job_binding_identity_fields(binding)?;
    if matches!(
        binding.state,
        ExecutionExternalJobState::Unbound
            | ExecutionExternalJobState::CancelRequested
            | ExecutionExternalJobState::Completed
            | ExecutionExternalJobState::Failed
            | ExecutionExternalJobState::Cancelled
            | ExecutionExternalJobState::UnknownOutcome
    ) {
        return Err(Error::InvalidRepositoryInput {
            message: "external job binding requires a live provider state".to_string(),
        });
    }
    Ok(())
}

fn validate_external_job_binding_identity_fields(
    binding: &ExecutionExternalJobBinding,
) -> Result<()> {
    let intent_shape = NewExecutionExternalJobIntent {
        external_job_uid: binding.external_job_uid,
        tenant_id: binding.tenant_id,
        run_uid: binding.run_uid,
        owner: binding.owner,
        job_generation: binding.job_generation,
        provider: binding.provider.clone(),
        idempotency_key: binding.idempotency_key.clone(),
        expires_at: DateTime::<Utc>::MAX_UTC,
    };
    validate_external_job_intent(&intent_shape, true)?;
    if binding.provider.trim().is_empty()
        || binding.provider.len() > 128
        || binding.provider_job_id.trim().is_empty()
        || binding.provider_job_id.len() > 512
        || binding.callback_auth_reference.trim().is_empty()
        || binding.callback_auth_reference.len() > 2_048
        || !bounded_optional_text(&binding.progress_phase, 256)
        || !bounded_optional_text(&binding.provider_contract_violation, 4_096)
    {
        return Err(Error::InvalidRepositoryInput {
            message: "external job binding requires bounded provider identity".to_string(),
        });
    }
    Ok(())
}

fn validate_callback(callback: &ExecutionExternalJobCallback) -> Result<()> {
    if callback.external_job_uid.is_nil()
        || callback.job_generation == 0
        || callback.provider.trim().is_empty()
        || callback.provider.len() > 128
        || callback.provider_job_id.trim().is_empty()
        || callback.provider_job_id.len() > 512
        || callback.provider_event_id.trim().is_empty()
        || callback.provider_event_id.len() > 512
    {
        return Err(Error::InvalidRepositoryInput {
            message: "external job callback requires an exact bounded provider event identity"
                .to_string(),
        });
    }
    let update_shape_is_valid = match &callback.update {
        ExecutionExternalJobCallbackUpdate::Progress {
            state,
            progress_phase,
            ..
        } => {
            !state.is_terminal()
                && !matches!(state, ExecutionExternalJobState::Starting)
                && bounded_optional_text(progress_phase, 256)
        }
        ExecutionExternalJobCallbackUpdate::Terminal {
            state,
            progress_phase,
            output,
            error,
        } => {
            let outcome_is_object = output.as_ref().is_none_or(Value::is_object)
                && error.as_ref().is_none_or(Value::is_object);
            let outcome_matches_state = match state {
                ExecutionExternalJobState::Completed => output.is_some() && error.is_none(),
                ExecutionExternalJobState::Failed | ExecutionExternalJobState::UnknownOutcome => {
                    output.is_none() && error.is_some()
                }
                ExecutionExternalJobState::Cancelled => output.is_none() && error.is_none(),
                ExecutionExternalJobState::Unbound
                | ExecutionExternalJobState::Starting
                | ExecutionExternalJobState::Running
                | ExecutionExternalJobState::WaitingReconcile
                | ExecutionExternalJobState::CancelRequested => false,
            };
            outcome_is_object && outcome_matches_state && bounded_optional_text(progress_phase, 256)
        }
    };
    if !update_shape_is_valid {
        return Err(Error::InvalidRepositoryInput {
            message: "external job callback update does not match its lifecycle state".to_string(),
        });
    }
    Ok(())
}

fn validate_cancellation(cancellation: &ExecutionExternalJobCancellation) -> Result<()> {
    let identity_is_valid = !cancellation.external_job_uid.is_nil()
        && cancellation.job_generation > 0
        && !cancellation.provider.trim().is_empty()
        && cancellation.provider.len() <= 128
        && !cancellation.provider_job_id.trim().is_empty()
        && cancellation.provider_job_id.len() <= 512;
    let state_is_valid = match cancellation.state {
        ExecutionExternalJobState::CancelRequested => {
            cancellation.next_reconcile_at.is_some() && cancellation.error.is_none()
        }
        ExecutionExternalJobState::Cancelled => {
            cancellation.next_reconcile_at.is_none() && cancellation.error.is_none()
        }
        ExecutionExternalJobState::UnknownOutcome => {
            cancellation.next_reconcile_at.is_none()
                && cancellation.error.as_ref().is_some_and(Value::is_object)
        }
        ExecutionExternalJobState::Unbound
        | ExecutionExternalJobState::Starting
        | ExecutionExternalJobState::Running
        | ExecutionExternalJobState::WaitingReconcile
        | ExecutionExternalJobState::Completed
        | ExecutionExternalJobState::Failed => false,
    };
    if !identity_is_valid || !state_is_valid {
        return Err(Error::InvalidRepositoryInput {
            message:
                "external job cancellation requires exact identity and a typed cancellation state"
                    .to_string(),
        });
    }
    Ok(())
}

fn bounded_optional_text(value: &Option<String>, max_len: usize) -> bool {
    value
        .as_ref()
        .is_none_or(|value| !value.trim().is_empty() && value.len() <= max_len)
}

fn external_job_matches_intent(
    record: &ExecutionExternalJobRecord,
    intent: &NewExecutionExternalJobIntent,
) -> bool {
    record.external_job_uid == intent.external_job_uid
        && record.tenant_id == intent.tenant_id
        && record.run_uid == intent.run_uid
        && record.owner == intent.owner
        && record.job_generation == intent.job_generation
        && record.declared_provider == intent.provider
        && record.idempotency_key == intent.idempotency_key
}

fn external_job_matches_binding_identity(
    record: &ExecutionExternalJobRecord,
    binding: &ExecutionExternalJobBinding,
) -> bool {
    record.external_job_uid == binding.external_job_uid
        && record.tenant_id == binding.tenant_id
        && record.run_uid == binding.run_uid
        && record.owner == binding.owner
        && record.job_generation == binding.job_generation
        && record.idempotency_key == binding.idempotency_key
}

fn external_job_matches_provider_result(
    record: &ExecutionExternalJobRecord,
    binding: &ExecutionExternalJobBinding,
) -> bool {
    external_job_matches_binding_identity(record, binding)
        && record.provider.as_deref() == Some(record.declared_provider.as_str())
        && record.provider_job_id.as_deref() == Some(binding.provider_job_id.as_str())
        && record.callback_auth_reference.as_deref()
            == Some(binding.callback_auth_reference.as_str())
        && matches!(
            record.state,
            ExecutionExternalJobState::Starting
                | ExecutionExternalJobState::Running
                | ExecutionExternalJobState::WaitingReconcile
                | ExecutionExternalJobState::CancelRequested
        )
        && record.progress_phase == binding.progress_phase
        && record.cancel_supported == binding.cancel_supported
}

fn external_job_from_row(row: &sqlx::postgres::PgRow) -> Result<ExecutionExternalJobRecord> {
    let task_id = row
        .try_get::<Option<Uuid>, _>("task_id")
        .map_err(super::row_error)?;
    let attempt_generation = row
        .try_get::<Option<i64>, _>("attempt_generation")
        .map_err(super::row_error)?;
    let compensation_id = row
        .try_get::<Option<Uuid>, _>("compensation_id")
        .map_err(super::row_error)?;
    let compensation_generation = row
        .try_get::<Option<i64>, _>("compensation_generation")
        .map_err(super::row_error)?;
    let compensation_attempt_generation = row
        .try_get::<Option<i64>, _>("compensation_attempt_generation")
        .map_err(super::row_error)?;
    let owner = match (
        task_id,
        attempt_generation,
        compensation_id,
        compensation_generation,
        compensation_attempt_generation,
    ) {
        (Some(task_id), Some(attempt_generation), None, None, None) => {
            ExecutionExternalJobOwner::Task {
                task_id,
                attempt_generation: super::to_u64(attempt_generation, "attempt generation")?,
            }
        }
        (None, None, Some(compensation_id), Some(generation), Some(attempt_generation)) => {
            ExecutionExternalJobOwner::Compensation {
                compensation_id,
                compensation_generation: super::to_u64(generation, "compensation generation")?,
                compensation_attempt_generation: super::to_u64(
                    attempt_generation,
                    "compensation attempt generation",
                )?,
            }
        }
        _ => {
            return Err(Error::InvalidRepositoryData {
                message: "external job has an invalid task/compensation owner shape".to_string(),
            });
        }
    };
    let job_generation = row
        .try_get::<i64, _>("job_generation")
        .map_err(super::row_error)?;
    Ok(ExecutionExternalJobRecord {
        external_job_uid: row.try_get("external_job_uid").map_err(super::row_error)?,
        tenant_id: TenantId(row.try_get("tenant_id").map_err(super::row_error)?),
        run_uid: row.try_get("run_uid").map_err(super::row_error)?,
        owner,
        job_generation: super::to_u64(job_generation, "external job generation")?,
        declared_provider: row.try_get("declared_provider").map_err(super::row_error)?,
        provider: row.try_get("provider").map_err(super::row_error)?,
        provider_job_id: row.try_get("provider_job_id").map_err(super::row_error)?,
        idempotency_key: row.try_get("idempotency_key").map_err(super::row_error)?,
        callback_auth_reference: row
            .try_get("callback_auth_reference")
            .map_err(super::row_error)?,
        state: row
            .try_get::<String, _>("state")
            .map_err(super::row_error)?
            .parse()?,
        progress_phase: row.try_get("progress_phase").map_err(super::row_error)?,
        cancel_supported: row.try_get("cancel_supported").map_err(super::row_error)?,
        next_reconcile_at: row.try_get("next_reconcile_at").map_err(super::row_error)?,
        last_provider_event_id: row
            .try_get("last_provider_event_id")
            .map_err(super::row_error)?,
        output: row.try_get("output").map_err(super::row_error)?,
        error: row.try_get("error").map_err(super::row_error)?,
        created_at: row.try_get("created_at").map_err(super::row_error)?,
        updated_at: row.try_get("updated_at").map_err(super::row_error)?,
        completed_at: row.try_get("completed_at").map_err(super::row_error)?,
        provider_contract_violation: row
            .try_get("provider_contract_violation")
            .map_err(super::row_error)?,
    })
}
