//! Generation-fenced temporal trigger persistence and delivery activation.

use crate::wire::{
    ExecutionExternalJobStartRecoveryOwner, ExecutionExternalJobStartRecoveryRequest,
};
use std::str::FromStr;

use chrono::{DateTime, Utc};
use moa_config::ExecutionConfig;
use moa_core::types::identifiers::TenantId;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use super::{
    Error, ExecutionRepository, ExecutionScope, Result,
    capacity::{
        CapacityReleaseOutcome, CapacityReserveOutcome, ExecutionCapacityDimension,
        ExecutionCapacityOwner, ExecutionCapacityRequest, execution_capacity_reservation_uid,
        prelock_existing_capacity_dimensions_in_tx, release_capacity_in_tx, reserve_capacity_in_tx,
    },
    outbox::{
        ExecutionDeliveryState, ExecutionDispatchKind, ExecutionDispatchRecord,
        NewExecutionDispatch, enqueue_dispatch_in_conn,
        requeue_current_accepted_dispatches_in_conn, requeue_current_run_activations_in_conn,
        requeue_delivered_dispatch_in_conn,
    },
    run::enqueue_run_activation_in_conn,
    sqlx_error, storage_error, to_i64, to_optional_i64, to_positive_u32,
};

const MAX_RECONCILE_BATCH_SIZE: u32 = 1_000;
const RESTATE_STATE_LOSS_REDRIVE_GRACE_SECONDS: i64 = 30;
const TRIGGER_DISPATCH_NAMESPACE: Uuid = Uuid::from_u128(0xa431_37f6_2bd7_5bdd_8a24_3da7_0fa1_c017);

/// Kind of durable temporal condition represented by a trigger row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionTriggerKind {
    /// Absolute run-budget deadline.
    RunDeadline,
    /// Plan-authored task timer.
    TaskTimer,
    /// Expiry for a human or external-signal wait.
    WaitExpiry,
    /// Liveness deadline for one active task attempt.
    TaskWatchdog,
    /// Sparse reconciliation wake for one asynchronous provider job.
    ExternalReconcile,
    /// Recovers an unbound provider start after its reservation deadline.
    ExternalStartRecovery,
    /// One immutable tenant-schedule occurrence.
    ScheduleOccurrence,
    /// Liveness deadline for one active compensation attempt.
    CompensationWatchdog,
}

impl ExecutionTriggerKind {
    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RunDeadline => "run_deadline",
            Self::TaskTimer => "task_timer",
            Self::WaitExpiry => "wait_expiry",
            Self::TaskWatchdog => "task_watchdog",
            Self::ExternalReconcile => "external_reconcile",
            Self::ExternalStartRecovery => "external_start_recovery",
            Self::ScheduleOccurrence => "schedule_occurrence",
            Self::CompensationWatchdog => "compensation_watchdog",
        }
    }
}

impl FromStr for ExecutionTriggerKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "run_deadline" => Ok(Self::RunDeadline),
            "task_timer" => Ok(Self::TaskTimer),
            "wait_expiry" => Ok(Self::WaitExpiry),
            "task_watchdog" => Ok(Self::TaskWatchdog),
            "external_reconcile" => Ok(Self::ExternalReconcile),
            "external_start_recovery" => Ok(Self::ExternalStartRecovery),
            "schedule_occurrence" => Ok(Self::ScheduleOccurrence),
            "compensation_watchdog" => Ok(Self::CompensationWatchdog),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution trigger kind `{value}`"),
            }),
        }
    }
}

/// Immutable temporal trigger inserted with its delayed-delivery outbox row.
#[derive(Clone, Debug, PartialEq)]
pub struct NewExecutionTrigger {
    /// Stable trigger identity.
    pub trigger_uid: Uuid,
    /// Tenant that owns every target row.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Option<Uuid>,
    /// Owning logical task.
    pub task_id: Option<Uuid>,
    /// Owning compensation registration.
    pub compensation_id: Option<Uuid>,
    /// Owning tenant schedule.
    pub schedule_uid: Option<Uuid>,
    /// Exact tenant-schedule incarnation for immutable occurrence fencing.
    pub schedule_incarnation: Option<u64>,
    /// Temporal condition kind.
    pub kind: ExecutionTriggerKind,
    /// Current run-controller generation.
    pub controller_generation: Option<u64>,
    /// Current task-attempt generation.
    pub attempt_generation: Option<u64>,
    /// Current compensation registration generation.
    pub compensation_generation: Option<u64>,
    /// Current compensation-attempt generation.
    pub compensation_attempt_generation: Option<u64>,
    /// Immutable schedule occurrence sequence.
    pub occurrence_sequence: Option<u64>,
    /// Exact absolute delivery time.
    pub due_at: DateTime<Utc>,
    /// Bounded structured trigger payload.
    pub payload: Value,
}

/// One persisted temporal trigger.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionTriggerRecord {
    /// Stable trigger identity.
    pub trigger_uid: Uuid,
    /// Tenant that owns the row.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Option<Uuid>,
    /// Owning logical task.
    pub task_id: Option<Uuid>,
    /// Owning compensation registration.
    pub compensation_id: Option<Uuid>,
    /// Owning tenant schedule.
    pub schedule_uid: Option<Uuid>,
    /// Tenant-schedule incarnation fence.
    pub schedule_incarnation: Option<u64>,
    /// Temporal condition kind.
    pub kind: ExecutionTriggerKind,
    /// Delivery lifecycle state.
    pub state: ExecutionDeliveryState,
    /// Run-controller generation fence.
    pub controller_generation: Option<u64>,
    /// Task-attempt generation fence.
    pub attempt_generation: Option<u64>,
    /// Compensation registration generation fence.
    pub compensation_generation: Option<u64>,
    /// Compensation-attempt generation fence.
    pub compensation_attempt_generation: Option<u64>,
    /// Tenant-schedule occurrence sequence.
    pub occurrence_sequence: Option<u64>,
    /// Exact absolute delivery time.
    pub due_at: DateTime<Utc>,
    /// Structured trigger payload.
    pub payload: Value,
    /// Successful delivery time.
    pub delivered_at: Option<DateTime<Utc>>,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last mutation time.
    pub updated_at: DateTime<Utc>,
}

/// Durable trigger plus the delayed dispatch that targets its immutable ID.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionTriggerWrite {
    /// Persisted trigger row.
    pub trigger: ExecutionTriggerRecord,
    /// Persisted delayed trigger-delivery dispatch.
    pub dispatch: ExecutionDispatchRecord,
}

/// Why a trigger delivery completed without advancing canonical state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionTriggerNoOp {
    /// No visible trigger has this immutable ID.
    NotFound,
    /// Delivery was already acknowledged.
    Duplicate,
    /// Cancellation or supersession fenced delivery.
    Inactive,
    /// The canonical database deadline has not arrived yet.
    NotDue,
    /// The referenced run, task, schedule, or generation is no longer current.
    StaleGeneration,
}

/// Result of idempotently superseding one exact trigger generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionTriggerSupersedeOutcome {
    /// An active trigger was superseded by this call.
    Superseded,
    /// The exact trigger generation was already superseded.
    AlreadySuperseded,
    /// The exact trigger generation had already reached another inactive state.
    AlreadyInactive,
    /// No trigger matched the immutable identity and generation fence.
    StaleOrMissing,
}

/// Result of atomically firing one immutable trigger.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionTriggerFireOutcome {
    /// The trigger was current and delivered; run-scoped triggers enqueue one activation.
    Delivered {
        /// Run activation created by the same transaction, if the trigger owns a run.
        activation: Option<Box<ExecutionDispatchRecord>>,
    },
    /// Delivery is an idempotent success with no canonical advancement.
    NoOp(ExecutionTriggerNoOp),
}

/// Read-only disposition for one exact sparse external-job reconciliation trigger.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionExternalReconcileTriggerOutcome {
    /// The trigger is due and still owns the exact provider-job generation.
    Ready(crate::wire::ExecutionExternalJobReconcileRequest),
    /// Canonical state makes delivery an idempotent success without a provider call.
    NoOp(ExecutionTriggerNoOp),
}

/// Preparation result for one crash-safe external provider start recovery.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionExternalStartRecoveryTriggerOutcome {
    /// The exact due unbound intent is ready for its typed receiver.
    Ready(ExecutionExternalJobStartRecoveryRequest),
    /// Delivery is early, stale, inactive, duplicated, or missing.
    NoOp(ExecutionTriggerNoOp),
}

/// Result of preserving an ambiguous provider start behind bounded retry.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionExternalStartRecoveryRearmOutcome {
    /// The same immutable trigger and delivery were rearmed for sparse retry.
    Rearmed(Box<ExecutionDispatchRecord>),
    /// The trigger or unbound intent no longer matched the exact recovery request.
    StaleOrMissing,
}

/// Read-only disposition for one exact absolute run-deadline trigger.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionRunDeadlineTriggerOutcome {
    /// The deadline is due for the run's current controller generation and wake.
    Ready {
        /// Owning run.
        run_uid: Uuid,
        /// Current locked controller generation, independent of the trigger's arm generation.
        controller_generation: u64,
        /// Current locked wake epoch consumed by deadline fencing.
        wake_epoch: u64,
        /// Canonical database observation time proving the absolute deadline is due.
        observed_at: DateTime<Utc>,
    },
    /// Canonical state makes delivery an idempotent no-op.
    NoOp(ExecutionTriggerNoOp),
}

/// Read-only disposition for one exact bounded-attempt watchdog trigger.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionWatchdogTriggerOutcome {
    /// Due watchdog for a forward task attempt.
    Task(crate::wire::ExecutionTaskAttemptWatchdogRequest),
    /// Due watchdog for a compensation attempt.
    Compensation(crate::wire::ExecutionCompensationAttemptWatchdogRequest),
    /// Canonical state makes delivery an idempotent no-op.
    NoOp(ExecutionTriggerNoOp),
}

/// Result of rearming one live task watchdog for its next staleness observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionWatchdogDeferOutcome {
    /// The same immutable watchdog was rearmed for a strictly later observation.
    Deferred {
        /// New absolute observation time, never beyond the attempt deadline.
        next_due_at: DateTime<Utc>,
    },
    /// The watchdog must stay due: the attempt is already stale, at its deadline, superseded,
    /// or not a live task attempt.
    NotDeferred,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalReconcileTriggerPayload {
    external_job_uid: Uuid,
    job_generation: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalStartRecoveryTriggerPayload {
    external_job_uid: Uuid,
    job_generation: u64,
    declared_provider: String,
    idempotency_key: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunDeadlineTriggerPayload {
    run_uid: Uuid,
    deadline_at: DateTime<Utc>,
}

/// Atomic storage-wait trigger delivery without controller activation.
#[derive(Clone, Debug, PartialEq)]
pub enum ExecutionWaitTriggerDeliveryOutcome {
    /// The exact due wait generation was marked delivered at the database clock.
    Delivered {
        /// Immutable delivered trigger.
        trigger: Box<ExecutionTriggerRecord>,
        /// PostgreSQL observation time used for due validation and settlement.
        observed_at: DateTime<Utc>,
    },
    /// Delivery was safely ignored without changing live task state.
    NoOp(ExecutionTriggerNoOp),
}

impl ExecutionRepository {
    /// Loads one visible immutable trigger for trusted delivery routing.
    pub async fn load_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<Option<ExecutionTriggerRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query("SELECT * FROM moa.execution_trigger WHERE trigger_uid = $1")
            .bind(trigger_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let trigger = row.as_ref().map(trigger_from_row).transpose()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(trigger)
    }

    /// Validates one due external reconciliation trigger without settling it before provider work.
    pub async fn prepare_external_reconcile_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionExternalReconcileTriggerOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row =
            sqlx::query("SELECT * FROM moa.execution_trigger WHERE trigger_uid = $1 FOR UPDATE")
                .bind(trigger_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalReconcileTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotFound,
            ));
        };
        let trigger = trigger_from_row(&row)?;
        if trigger.kind != ExecutionTriggerKind::ExternalReconcile {
            return Err(Error::InvalidRepositoryInput {
                message: "external reconcile preparation requires an external_reconcile trigger"
                    .to_string(),
            });
        }
        let inactive = match trigger.state {
            ExecutionDeliveryState::Pending | ExecutionDeliveryState::Dispatching => None,
            ExecutionDeliveryState::Delivered => Some(ExecutionTriggerNoOp::Duplicate),
            ExecutionDeliveryState::Superseded
            | ExecutionDeliveryState::Cancelled
            | ExecutionDeliveryState::DeadLetter => Some(ExecutionTriggerNoOp::Inactive),
        };
        if let Some(reason) = inactive {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalReconcileTriggerOutcome::NoOp(reason));
        }
        let observed_at: DateTime<Utc> = sqlx::query_scalar("SELECT NOW()")
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        if trigger.due_at > observed_at {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalReconcileTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotDue,
            ));
        }
        if !trigger_is_current(conn.as_mut(), &trigger).await? {
            supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                ExecutionTriggerKind::ExternalReconcile,
                trigger.controller_generation,
                trigger.attempt_generation,
                trigger.compensation_generation,
                trigger.compensation_attempt_generation,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalReconcileTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            ));
        }
        let payload: ExternalReconcileTriggerPayload =
            serde_json::from_value(trigger.payload.clone()).map_err(|error| {
                Error::InvalidRepositoryData {
                    message: format!("invalid external reconcile trigger payload: {error}"),
                }
            })?;
        if payload.external_job_uid.is_nil() || payload.job_generation == 0 {
            return Err(Error::InvalidRepositoryData {
                message: "external reconcile trigger payload has an invalid identity".to_string(),
            });
        }
        let run_uid = trigger
            .run_uid
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "external reconcile trigger is missing run identity".to_string(),
            })?;
        let job = sqlx::query_as::<_, (String, String, String)>(
            "SELECT provider, provider_job_id, idempotency_key \
             FROM moa.execution_external_job \
             WHERE external_job_uid=$1 AND tenant_id=$2 AND run_uid=$3 \
               AND job_generation=$4 \
               AND ( \
                 (task_id=$5 AND attempt_generation=$6 AND compensation_id IS NULL) \
                 OR \
                 (compensation_id=$7 AND compensation_generation=$8 \
                    AND compensation_attempt_generation=$9 AND task_id IS NULL) \
               )",
        )
        .bind(payload.external_job_uid)
        .bind(trigger.tenant_id.0)
        .bind(run_uid)
        .bind(to_i64(payload.job_generation, "job generation")?)
        .bind(trigger.task_id)
        .bind(to_optional_i64(
            trigger.attempt_generation,
            "attempt generation",
        )?)
        .bind(trigger.compensation_id)
        .bind(to_optional_i64(
            trigger.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_optional_i64(
            trigger.compensation_attempt_generation,
            "compensation attempt generation",
        )?)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some((provider, provider_job_id, idempotency_key)) = job else {
            supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                ExecutionTriggerKind::ExternalReconcile,
                trigger.controller_generation,
                trigger.attempt_generation,
                trigger.compensation_generation,
                trigger.compensation_attempt_generation,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalReconcileTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            ));
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionExternalReconcileTriggerOutcome::Ready(
            crate::wire::ExecutionExternalJobReconcileRequest {
                tenant_id: trigger.tenant_id,
                external_job_uid: payload.external_job_uid,
                trigger_uid,
                job_generation: payload.job_generation,
                provider,
                provider_job_id,
                idempotency_key,
            },
        ))
    }

    /// Validates one due unbound provider-start recovery trigger.
    pub async fn prepare_external_start_recovery_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionExternalStartRecoveryTriggerOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row = sqlx::query(
            "SELECT trigger.*, now() AS observed_at FROM moa.execution_trigger AS trigger \
             WHERE trigger_uid=$1 FOR UPDATE",
        )
        .bind(trigger_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalStartRecoveryTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotFound,
            ));
        };
        let trigger = trigger_from_row(&row)?;
        if trigger.kind != ExecutionTriggerKind::ExternalStartRecovery {
            return Err(Error::InvalidRepositoryInput {
                message: "start recovery preparation requires external_start_recovery".to_string(),
            });
        }
        let inactive = match trigger.state {
            ExecutionDeliveryState::Pending | ExecutionDeliveryState::Dispatching => None,
            ExecutionDeliveryState::Delivered => Some(ExecutionTriggerNoOp::Duplicate),
            ExecutionDeliveryState::Superseded
            | ExecutionDeliveryState::Cancelled
            | ExecutionDeliveryState::DeadLetter => Some(ExecutionTriggerNoOp::Inactive),
        };
        if let Some(reason) = inactive {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalStartRecoveryTriggerOutcome::NoOp(reason));
        }
        let observed_at = row
            .try_get::<DateTime<Utc>, _>("observed_at")
            .map_err(super::row_error)?;
        if trigger.due_at > observed_at {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalStartRecoveryTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotDue,
            ));
        }
        if !trigger_is_current(conn.as_mut(), &trigger).await? {
            supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                trigger.kind,
                trigger.controller_generation,
                trigger.attempt_generation,
                trigger.compensation_generation,
                trigger.compensation_attempt_generation,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalStartRecoveryTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            ));
        }
        let payload: ExternalStartRecoveryTriggerPayload =
            serde_json::from_value(trigger.payload.clone()).map_err(|error| {
                Error::InvalidRepositoryData {
                    message: format!("invalid external start recovery payload: {error}"),
                }
            })?;
        let run_uid = trigger
            .run_uid
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "external start recovery trigger is missing run identity".to_string(),
            })?;
        let owner = match (
            trigger.task_id,
            trigger.attempt_generation,
            trigger.compensation_id,
            trigger.compensation_generation,
            trigger.compensation_attempt_generation,
        ) {
            (Some(task_id), Some(attempt_generation), None, None, None) => {
                ExecutionExternalJobStartRecoveryOwner::Task {
                    task_id,
                    attempt_generation,
                }
            }
            (None, None, Some(compensation_id), Some(generation), Some(attempt_generation)) => {
                ExecutionExternalJobStartRecoveryOwner::Compensation {
                    compensation_id,
                    compensation_generation: generation,
                    compensation_attempt_generation: attempt_generation,
                }
            }
            _ => {
                return Err(Error::InvalidRepositoryData {
                    message: "external start recovery trigger has invalid owner shape".to_string(),
                });
            }
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionExternalStartRecoveryTriggerOutcome::Ready(
            ExecutionExternalJobStartRecoveryRequest {
                tenant_id: trigger.tenant_id,
                run_uid,
                owner,
                external_job_uid: payload.external_job_uid,
                job_generation: payload.job_generation,
                provider: payload.declared_provider,
                idempotency_key: payload.idempotency_key,
                trigger_uid,
            },
        ))
    }

    /// Validates an absolute run deadline against DB time and reloads current run fences.
    pub async fn prepare_run_deadline_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionRunDeadlineTriggerOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row = sqlx::query(
            "SELECT trigger.*, now() AS observed_at, trigger.due_at <= now() AS is_due \
             FROM moa.execution_trigger AS trigger WHERE trigger_uid=$1 FOR UPDATE",
        )
        .bind(trigger_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionRunDeadlineTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotFound,
            ));
        };
        let trigger = trigger_from_row(&row)?;
        if trigger.kind != ExecutionTriggerKind::RunDeadline {
            return Err(Error::InvalidRepositoryInput {
                message: "run deadline preparation requires a run_deadline trigger".to_string(),
            });
        }
        let inactive = match trigger.state {
            ExecutionDeliveryState::Pending | ExecutionDeliveryState::Dispatching => None,
            ExecutionDeliveryState::Delivered => Some(ExecutionTriggerNoOp::Duplicate),
            ExecutionDeliveryState::Superseded
            | ExecutionDeliveryState::Cancelled
            | ExecutionDeliveryState::DeadLetter => Some(ExecutionTriggerNoOp::Inactive),
        };
        if let Some(reason) = inactive {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionRunDeadlineTriggerOutcome::NoOp(reason));
        }
        if !row.try_get::<bool, _>("is_due").map_err(super::row_error)? {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionRunDeadlineTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotDue,
            ));
        }
        let run_uid = trigger
            .run_uid
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "run deadline trigger is missing run identity".to_string(),
            })?;
        let run = sqlx::query_as::<
            _,
            (
                i64,
                i64,
                String,
                Option<DateTime<Utc>>,
                Option<DateTime<Utc>>,
            ),
        >(
            "SELECT controller_generation, wake_epoch, status, budget_deadline_at, \
                    budget_deadline_suspended_at \
             FROM moa.execution_run \
             WHERE tenant_id=$1 AND run_uid=$2 FOR UPDATE",
        )
        .bind(trigger.tenant_id.0)
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some((controller_generation, wake_epoch, status, approved_deadline_at, suspended_at)) =
            run
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionRunDeadlineTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotFound,
            ));
        };
        if suspended_at.is_some() {
            supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                ExecutionTriggerKind::RunDeadline,
                trigger.controller_generation,
                None,
                None,
                None,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionRunDeadlineTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            ));
        }
        if matches!(
            status.as_str(),
            "completed" | "partial" | "blocked" | "unsupported" | "failed" | "cancelled"
        ) {
            supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                ExecutionTriggerKind::RunDeadline,
                trigger.controller_generation,
                None,
                None,
                None,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionRunDeadlineTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            ));
        }
        if approved_deadline_at != Some(trigger.due_at) || !run_deadline_payload_matches(&trigger)?
        {
            supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                ExecutionTriggerKind::RunDeadline,
                trigger.controller_generation,
                None,
                None,
                None,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionRunDeadlineTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            ));
        }
        let observed_at = row
            .try_get::<DateTime<Utc>, _>("observed_at")
            .map_err(super::row_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionRunDeadlineTriggerOutcome::Ready {
            run_uid,
            controller_generation: super::to_u64(controller_generation, "controller generation")?,
            wake_epoch: super::to_u64(wake_epoch, "wake epoch")?,
            observed_at,
        })
    }

    /// Validates one due watchdog and resolves its exact active dispatch and capacity receipt.
    pub async fn prepare_watchdog_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionWatchdogTriggerOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row = sqlx::query(
            "SELECT trigger.*, trigger.due_at <= now() AS is_due \
             FROM moa.execution_trigger AS trigger WHERE trigger_uid=$1 FOR UPDATE",
        )
        .bind(trigger_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotFound,
            ));
        };
        let trigger = trigger_from_row(&row)?;
        if !matches!(
            trigger.kind,
            ExecutionTriggerKind::TaskWatchdog | ExecutionTriggerKind::CompensationWatchdog
        ) {
            return Err(Error::InvalidRepositoryInput {
                message: "watchdog preparation requires a task or compensation watchdog"
                    .to_string(),
            });
        }
        let inactive = match trigger.state {
            ExecutionDeliveryState::Pending | ExecutionDeliveryState::Dispatching => None,
            ExecutionDeliveryState::Delivered => Some(ExecutionTriggerNoOp::Duplicate),
            ExecutionDeliveryState::Superseded
            | ExecutionDeliveryState::Cancelled
            | ExecutionDeliveryState::DeadLetter => Some(ExecutionTriggerNoOp::Inactive),
        };
        if let Some(reason) = inactive {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogTriggerOutcome::NoOp(reason));
        }
        if !row.try_get::<bool, _>("is_due").map_err(super::row_error)? {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::NotDue,
            ));
        }
        if !trigger_is_current(conn.as_mut(), &trigger).await? {
            supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                trigger.kind,
                trigger.controller_generation,
                trigger.attempt_generation,
                trigger.compensation_generation,
                trigger.compensation_attempt_generation,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            ));
        }
        let run_uid = trigger
            .run_uid
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "watchdog trigger is missing run identity".to_string(),
            })?;
        let controller_generation =
            trigger
                .controller_generation
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "watchdog trigger is missing controller generation".to_string(),
                })?;
        let outcome = match trigger.kind {
            ExecutionTriggerKind::TaskWatchdog => {
                let task_id = trigger
                    .task_id
                    .ok_or_else(|| Error::InvalidRepositoryData {
                        message: "task watchdog is missing task identity".to_string(),
                    })?;
                let attempt_generation =
                    trigger
                        .attempt_generation
                        .ok_or_else(|| Error::InvalidRepositoryData {
                            message: "task watchdog is missing attempt generation".to_string(),
                        })?;
                let owner = sqlx::query_as::<_, (Uuid, Uuid)>(
                    "SELECT task.active_dispatch_uid, reservation.reservation_uid \
                     FROM moa.execution_task AS task \
                     JOIN moa.execution_capacity_reservation AS reservation \
                       ON reservation.tenant_id=task.tenant_id \
                      AND reservation.run_uid=task.run_uid \
                      AND reservation.task_id=task.task_id \
                      AND reservation.attempt_generation=task.attempt_generation \
                      AND reservation.controller_generation=$5 \
                      AND reservation.resource_dimension='active_tasks' \
                      AND reservation.state IN ('reserved','reconciling') \
                     WHERE task.tenant_id=$1 AND task.run_uid=$2 AND task.task_id=$3 \
                       AND task.attempt_generation=$4 AND task.active_dispatch_uid IS NOT NULL",
                )
                .bind(trigger.tenant_id.0)
                .bind(run_uid)
                .bind(task_id)
                .bind(to_i64(attempt_generation, "attempt generation")?)
                .bind(to_i64(controller_generation, "controller generation")?)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
                owner.map(|(dispatch_uid, capacity_reservation_uid)| {
                    ExecutionWatchdogTriggerOutcome::Task(
                        crate::wire::ExecutionTaskAttemptWatchdogRequest {
                            dispatch_uid,
                            capacity_reservation_uid,
                            watchdog_trigger_uid: trigger_uid,
                            run_uid,
                            task_id: crate::state::ExecutionTaskId::from_uuid(task_id),
                            controller_generation,
                            attempt_generation,
                            tenant_id: trigger.tenant_id,
                        },
                    )
                })
            }
            ExecutionTriggerKind::CompensationWatchdog => {
                let compensation_id =
                    trigger
                        .compensation_id
                        .ok_or_else(|| Error::InvalidRepositoryData {
                            message: "compensation watchdog is missing compensation identity"
                                .to_string(),
                        })?;
                let compensation_generation = trigger.compensation_generation.ok_or_else(|| {
                    Error::InvalidRepositoryData {
                        message: "compensation watchdog is missing logical generation".to_string(),
                    }
                })?;
                let attempt_generation =
                    trigger.compensation_attempt_generation.ok_or_else(|| {
                        Error::InvalidRepositoryData {
                            message: "compensation watchdog is missing attempt generation"
                                .to_string(),
                        }
                    })?;
                let owner = sqlx::query_as::<_, (Uuid, Uuid)>(
                    "SELECT compensation.active_dispatch_uid, reservation.reservation_uid \
                     FROM moa.execution_compensation AS compensation \
                     JOIN moa.execution_capacity_reservation AS reservation \
                       ON reservation.tenant_id=compensation.tenant_id \
                      AND reservation.run_uid=compensation.run_uid \
                      AND reservation.compensation_id=compensation.compensation_id \
                      AND reservation.compensation_generation=compensation.generation \
                      AND reservation.compensation_attempt_generation=compensation.attempt_generation \
                      AND reservation.controller_generation=$6 \
                      AND reservation.resource_dimension='active_tasks' \
                      AND reservation.state IN ('reserved','reconciling') \
                     WHERE compensation.tenant_id=$1 AND compensation.run_uid=$2 \
                       AND compensation.compensation_id=$3 AND compensation.generation=$4 \
                       AND compensation.attempt_generation=$5 \
                       AND compensation.active_dispatch_uid IS NOT NULL",
                )
                .bind(trigger.tenant_id.0)
                .bind(run_uid)
                .bind(compensation_id)
                .bind(to_i64(
                    compensation_generation,
                    "compensation generation",
                )?)
                .bind(to_i64(
                    attempt_generation,
                    "compensation attempt generation",
                )?)
                .bind(to_i64(controller_generation, "controller generation")?)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
                owner.map(|(dispatch_uid, capacity_reservation_uid)| {
                    ExecutionWatchdogTriggerOutcome::Compensation(
                        crate::wire::ExecutionCompensationAttemptWatchdogRequest {
                            dispatch_uid,
                            capacity_reservation_uid,
                            watchdog_trigger_uid: trigger_uid,
                            run_uid,
                            compensation_id: crate::state::CompensationId::from_uuid(
                                compensation_id,
                            ),
                            compensation_generation,
                            compensation_attempt_generation: attempt_generation,
                            controller_generation,
                            tenant_id: trigger.tenant_id,
                        },
                    )
                })
            }
            _ => None,
        };
        let Some(outcome) = outcome else {
            supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                trigger.kind,
                trigger.controller_generation,
                trigger.attempt_generation,
                trigger.compensation_generation,
                trigger.compensation_attempt_generation,
            )
            .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogTriggerOutcome::NoOp(
                ExecutionTriggerNoOp::StaleGeneration,
            ));
        };
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Supersedes one external reconciliation trigger after its provider result is durable.
    pub async fn settle_external_reconcile_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionTriggerSupersedeOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row =
            sqlx::query("SELECT * FROM moa.execution_trigger WHERE trigger_uid = $1 FOR UPDATE")
                .bind(trigger_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionTriggerSupersedeOutcome::StaleOrMissing);
        };
        let trigger = trigger_from_row(&row)?;
        if trigger.kind != ExecutionTriggerKind::ExternalReconcile {
            return Err(Error::InvalidRepositoryInput {
                message: "external reconcile settlement requires an external_reconcile trigger"
                    .to_string(),
            });
        }
        let outcome = supersede_trigger_in_conn(
            conn.as_mut(),
            trigger_uid,
            ExecutionTriggerKind::ExternalReconcile,
            trigger.controller_generation,
            trigger.attempt_generation,
            trigger.compensation_generation,
            trigger.compensation_attempt_generation,
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Supersedes one provider-start recovery trigger after recovery is durable.
    pub async fn settle_external_start_recovery_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionTriggerSupersedeOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row =
            sqlx::query("SELECT * FROM moa.execution_trigger WHERE trigger_uid=$1 FOR UPDATE")
                .bind(trigger_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionTriggerSupersedeOutcome::StaleOrMissing);
        };
        let trigger = trigger_from_row(&row)?;
        if trigger.kind != ExecutionTriggerKind::ExternalStartRecovery {
            return Err(Error::InvalidRepositoryInput {
                message: "start recovery settlement requires external_start_recovery".to_string(),
            });
        }
        let outcome = supersede_trigger_in_conn(
            conn.as_mut(),
            trigger_uid,
            trigger.kind,
            trigger.controller_generation,
            trigger.attempt_generation,
            trigger.compensation_generation,
            trigger.compensation_attempt_generation,
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Rearms the same exact start-recovery trigger after an ambiguous provider lookup.
    pub async fn rearm_external_start_recovery(
        &self,
        scope: ExecutionScope,
        request: &ExecutionExternalJobStartRecoveryRequest,
        retry_at: DateTime<Utc>,
        error: &str,
    ) -> Result<ExecutionExternalStartRecoveryRearmOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), request.trigger_uid).await?;
        let row = sqlx::query(
            "SELECT trigger.*, $2::TIMESTAMPTZ > now() AS retry_is_future \
             FROM moa.execution_trigger AS trigger WHERE trigger_uid=$1 FOR UPDATE",
        )
        .bind(request.trigger_uid)
        .bind(retry_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalStartRecoveryRearmOutcome::StaleOrMissing);
        };
        let trigger = trigger_from_row(&row)?;
        if trigger.kind != ExecutionTriggerKind::ExternalStartRecovery
            || !row
                .try_get::<bool, _>("retry_is_future")
                .map_err(super::row_error)?
            || !start_recovery_request_matches_trigger(request, &trigger)
            || !trigger_is_current(conn.as_mut(), &trigger).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionExternalStartRecoveryRearmOutcome::StaleOrMissing);
        }
        let last_error = error.chars().take(4_096).collect::<String>();
        // Restate journals both ExecutionTrigger/fire and provider recovery by dispatch UID. A
        // rearm therefore needs a new persisted delivery identity or the next due claim would
        // replay the completed Unknown/NotDue invocation without another provider lookup.
        let next_dispatch_uid =
            rearmed_trigger_delivery_dispatch_uid(request.trigger_uid, retry_at);
        sqlx::query(
            "UPDATE moa.execution_trigger SET state='pending', due_at=$2, \
             delivered_at=NULL, last_error=$3, updated_at=now() WHERE trigger_uid=$1",
        )
        .bind(request.trigger_uid)
        .bind(retry_at)
        .bind(&last_error)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let row = sqlx::query(
            "UPDATE moa.execution_dispatch_outbox SET dispatch_uid=$4,state='pending', \
             not_before_at=$2,delivery_attempts=0, \
             claim_owner=NULL,claimed_at=NULL,claim_expires_at=NULL,delivered_at=NULL, \
             last_error=$3,updated_at=now() \
             WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery' \
               AND state IN ('pending','dispatching','delivered') RETURNING *",
        )
        .bind(request.trigger_uid)
        .bind(retry_at)
        .bind(last_error)
        .bind(next_dispatch_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "start recovery trigger is missing its exact delivery outbox".to_string(),
        })?;
        let dispatch = super::outbox::dispatch_from_row_for_repository(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionExternalStartRecoveryRearmOutcome::Rearmed(
            Box::new(dispatch),
        ))
    }

    /// Rearms one live task watchdog for its next heartbeat-staleness observation.
    ///
    /// The watchdog is armed at a staleness window rather than the attempt deadline, so a live
    /// attempt is observed repeatedly and must be pushed forward each time it proves progress.
    /// The next observation is `min(attempt_deadline_at, last_progress_at + staleness)`, so the
    /// deadline remains the hard backstop and deferral can never postpone it.
    ///
    /// This is a rearm, not a supersede: the trigger stays `pending` and keeps its
    /// `scheduled_triggers` capacity receipt, and the existing delivery row is rewritten in place
    /// rather than released and recreated. Every failure to establish a strictly later, still
    /// live observation returns [`ExecutionWatchdogDeferOutcome::NotDeferred`] and leaves the
    /// watchdog due, which preserves the pre-existing retry behaviour exactly.
    pub async fn defer_task_attempt_watchdog(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        trigger_uid: Uuid,
    ) -> Result<ExecutionWatchdogDeferOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row =
            sqlx::query("SELECT * FROM moa.execution_trigger WHERE trigger_uid=$1 FOR UPDATE")
                .bind(trigger_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogDeferOutcome::NotDeferred);
        };
        let trigger = trigger_from_row(&row)?;
        // A non-task watchdog is not an error here. Trigger delivery routes task and compensation
        // watchdogs through one retry branch, and compensation attempts carry no heartbeat, so the
        // safe answer for anything but a live task watchdog is to leave the trigger alone.
        if trigger.kind != ExecutionTriggerKind::TaskWatchdog
            || !matches!(
                trigger.state,
                ExecutionDeliveryState::Pending | ExecutionDeliveryState::Dispatching
            )
            || !trigger_is_current(conn.as_mut(), &trigger).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogDeferOutcome::NotDeferred);
        }
        let progress = sqlx::query_as::<
            _,
            (
                Option<DateTime<Utc>>,
                DateTime<Utc>,
                Option<i32>,
                DateTime<Utc>,
            ),
        >(
            "SELECT attempt_deadline_at, last_progress_at, progress_step_bound_seconds, now() \
             FROM moa.execution_task \
             WHERE tenant_id=$1 AND run_uid=$2 AND task_id=$3 AND attempt_generation=$4 \
               AND active_dispatch_uid IS NOT NULL",
        )
        .bind(trigger.tenant_id.0)
        .bind(trigger.run_uid)
        .bind(trigger.task_id)
        .bind(to_optional_i64(
            trigger.attempt_generation,
            "attempt generation",
        )?)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some((Some(attempt_deadline_at), last_progress_at, step_bound_seconds, observed_at)) =
            progress
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogDeferOutcome::NotDeferred);
        };
        // Deferral must use the same window the classifier used. An attempt is Live only
        // because its in-flight step declared a wider bound, so recomputing the next
        // observation from the bare floor cannot advance past the due time that just fired:
        // the deferral is refused, and the caller treats a still-current receiver as an error
        // and retries the delivery forever.
        let staleness = crate::repository::task::attempt_heartbeat_staleness_window(
            config,
            step_bound_seconds
                .map(|seconds| {
                    let seconds = to_positive_u32(seconds, "progress step bound seconds")?;
                    chrono::TimeDelta::try_seconds(i64::from(seconds)).ok_or_else(|| {
                        Error::InvalidRepositoryData {
                            message: "progress step bound exceeds chrono duration".to_string(),
                        }
                    })
                })
                .transpose()?,
        )?;
        let Some(calculated_due_at) = last_progress_at
            .checked_add_signed(staleness)
            .map(|stale_at| stale_at.min(attempt_deadline_at))
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogDeferOutcome::NotDeferred);
        };
        // The receiver and this transaction observe time independently. A recovered delivery can
        // classify the attempt live immediately before its staleness boundary, then reach this
        // transaction immediately after it. Give that already-journaled RetryDelivery one fresh
        // identity instead of retrying the same memoized receiver response forever.
        let next_due_at = if calculated_due_at <= observed_at || calculated_due_at <= trigger.due_at
        {
            observed_at
                .checked_add_signed(chrono::TimeDelta::seconds(1))
                .map(|retry_at| retry_at.min(attempt_deadline_at))
                .unwrap_or(attempt_deadline_at)
        } else {
            calculated_due_at
        };
        // Strictly forward only. Once the deadline itself is due, the receiver must settle the
        // attempt (or yield to another exact recovery owner) rather than extending its authority.
        if next_due_at <= observed_at || next_due_at <= trigger.due_at {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionWatchdogDeferOutcome::NotDeferred);
        }
        // Restate journals trigger delivery by dispatch UID, so a rearm needs a new delivery
        // identity or the next claim would replay the completed RetryDelivery invocation.
        let next_dispatch_uid = rearmed_trigger_delivery_dispatch_uid(trigger_uid, next_due_at);
        sqlx::query(
            "UPDATE moa.execution_trigger SET state='pending', due_at=$2, delivered_at=NULL, \
             updated_at=now() WHERE trigger_uid=$1",
        )
        .bind(trigger_uid)
        .bind(next_due_at)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let rearmed = sqlx::query(
            "UPDATE moa.execution_dispatch_outbox SET dispatch_uid=$3,state='pending', \
             not_before_at=$2,delivery_attempts=0, \
             claim_owner=NULL,claimed_at=NULL,claim_expires_at=NULL,delivered_at=NULL, \
             updated_at=now() \
             WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery' \
               AND state IN ('pending','dispatching','delivered')",
        )
        .bind(trigger_uid)
        .bind(next_due_at)
        .bind(next_dispatch_uid)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if rearmed.rows_affected() != 1 {
            return Err(Error::InvalidRepositoryData {
                message: "task watchdog trigger is missing its exact delivery outbox".to_string(),
            });
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(ExecutionWatchdogDeferOutcome::Deferred { next_due_at })
    }

    /// Supersedes one run deadline trigger after its bounded terminal fence is durable.
    pub async fn settle_run_deadline_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionTriggerSupersedeOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row =
            sqlx::query("SELECT * FROM moa.execution_trigger WHERE trigger_uid=$1 FOR UPDATE")
                .bind(trigger_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionTriggerSupersedeOutcome::StaleOrMissing);
        };
        let trigger = trigger_from_row(&row)?;
        if trigger.kind != ExecutionTriggerKind::RunDeadline {
            return Err(Error::InvalidRepositoryInput {
                message: "run deadline settlement requires a run_deadline trigger".to_string(),
            });
        }
        let outcome = supersede_trigger_in_conn(
            conn.as_mut(),
            trigger_uid,
            ExecutionTriggerKind::RunDeadline,
            trigger.controller_generation,
            None,
            None,
            None,
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Supersedes one watchdog after its keyed attempt receiver durably completes.
    pub async fn settle_watchdog_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionTriggerSupersedeOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        prelock_trigger_scheduled_capacity_in_conn(conn.as_mut(), trigger_uid).await?;
        let row =
            sqlx::query("SELECT * FROM moa.execution_trigger WHERE trigger_uid=$1 FOR UPDATE")
                .bind(trigger_uid)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionTriggerSupersedeOutcome::StaleOrMissing);
        };
        let trigger = trigger_from_row(&row)?;
        if !matches!(
            trigger.kind,
            ExecutionTriggerKind::TaskWatchdog | ExecutionTriggerKind::CompensationWatchdog
        ) {
            return Err(Error::InvalidRepositoryInput {
                message: "watchdog settlement requires a task or compensation watchdog".to_string(),
            });
        }
        let outcome = supersede_trigger_in_conn(
            conn.as_mut(),
            trigger_uid,
            trigger.kind,
            trigger.controller_generation,
            trigger.attempt_generation,
            trigger.compensation_generation,
            trigger.compensation_attempt_generation,
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Creates a trigger and its delayed delivery dispatch atomically.
    pub async fn create_trigger(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        request: NewExecutionTrigger,
    ) -> Result<ExecutionTriggerWrite> {
        let mut conn = scope.begin(&self.pool).await?;
        let write = create_trigger_with_dispatch_in_conn(conn.as_mut(), config, &request).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(write)
    }

    /// Fires one trigger under current run/task/schedule generation fences.
    pub async fn fire_trigger(
        &self,
        scope: ExecutionScope,
        trigger_uid: Uuid,
    ) -> Result<ExecutionTriggerFireOutcome> {
        let mut conn = scope.begin(&self.pool).await?;
        let owner = sqlx::query_as::<_, (Uuid, Option<Uuid>)>(
            "SELECT tenant_id,run_uid FROM moa.execution_trigger WHERE trigger_uid=$1",
        )
        .bind(trigger_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some((tenant_id, run_uid)) = owner else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(ExecutionTriggerFireOutcome::NoOp(
                ExecutionTriggerNoOp::NotFound,
            ));
        };
        let dimensions = if run_uid.is_some() {
            vec![
                ExecutionCapacityDimension::ActiveRuns,
                ExecutionCapacityDimension::ParkedRuns,
                ExecutionCapacityDimension::ScheduledTriggers,
            ]
        } else {
            vec![ExecutionCapacityDimension::ScheduledTriggers]
        };
        prelock_existing_capacity_dimensions_in_tx(conn.as_mut(), TenantId(tenant_id), &dimensions)
            .await?;
        let outcome = fire_trigger_in_conn(conn.as_mut(), trigger_uid).await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Repairs due trigger deliveries and queued run activations after dispatch-state loss.
    pub async fn reconcile_due_trigger_dispatches(
        &self,
        scope: ExecutionScope,
        batch_size: u32,
    ) -> Result<Vec<ExecutionDispatchRecord>> {
        if !(3..=MAX_RECONCILE_BATCH_SIZE).contains(&batch_size) {
            return Err(Error::InvalidRepositoryInput {
                message: format!(
                    "execution trigger reconciliation batch must be 3..={MAX_RECONCILE_BATCH_SIZE}"
                ),
            });
        }
        let mut conn = scope.begin(&self.pool).await?;
        let (trigger_budget, accepted_budget, run_budget) = reconcile_lane_budgets(batch_size);
        let candidates = sqlx::query_as::<_, (Uuid, Uuid)>(
            r#"
            SELECT trigger.tenant_id, trigger.trigger_uid
            FROM moa.execution_trigger AS trigger
            WHERE trigger.state = 'pending'
              AND trigger.due_at <= now() - make_interval(secs => $2)
              AND (
                  NOT EXISTS (
                      SELECT 1
                      FROM moa.execution_dispatch_outbox AS dispatch
                      WHERE dispatch.tenant_id = trigger.tenant_id
                        AND dispatch.trigger_uid = trigger.trigger_uid
                        AND dispatch.dispatch_kind = 'trigger_delivery'
                  )
                  OR EXISTS (
                      SELECT 1
                      FROM moa.execution_dispatch_outbox AS dispatch
                      WHERE dispatch.tenant_id = trigger.tenant_id
                        AND dispatch.trigger_uid = trigger.trigger_uid
                        AND dispatch.dispatch_kind = 'trigger_delivery'
                        AND dispatch.state = 'delivered'
                        AND dispatch.delivered_at
                            <= now() - make_interval(secs => $2)
                  )
              )
              AND (
                  (
                      trigger.run_uid IS NOT NULL
                      AND EXISTS (
                          SELECT 1 FROM moa.execution_run AS run
                          WHERE run.tenant_id = trigger.tenant_id
                            AND run.run_uid = trigger.run_uid
                            AND run.status NOT IN (
                                'completed', 'partial', 'blocked', 'unsupported',
                                'failed', 'cancelled'
                            )
                      )
                  ) OR (
                      trigger.schedule_uid IS NOT NULL
                      AND EXISTS (
                          SELECT 1 FROM moa.execution_schedule AS schedule
                          WHERE schedule.tenant_id = trigger.tenant_id
                            AND schedule.schedule_uid = trigger.schedule_uid
                            AND schedule.status = 'active'
                      )
                  )
              )
            ORDER BY trigger.due_at, trigger.tenant_id, trigger.trigger_uid
            LIMIT $1
            "#,
        )
        .bind(i64::from(trigger_budget))
        .bind(RESTATE_STATE_LOSS_REDRIVE_GRACE_SECONDS)
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let candidate_trigger_uids = candidates
            .iter()
            .map(|(_, trigger_uid)| *trigger_uid)
            .collect::<Vec<_>>();
        let mut candidate_tenant_ids = candidates
            .into_iter()
            .map(|(tenant_id, _)| tenant_id)
            .collect::<Vec<_>>();
        candidate_tenant_ids.sort_unstable();
        candidate_tenant_ids.dedup();
        for tenant_id in candidate_tenant_ids {
            prelock_existing_capacity_dimensions_in_tx(
                conn.as_mut(),
                TenantId(tenant_id),
                &[ExecutionCapacityDimension::ScheduledTriggers],
            )
            .await?;
        }
        let rows = if candidate_trigger_uids.is_empty() {
            Vec::new()
        } else {
            sqlx::query(
                r#"
                SELECT trigger.*
                FROM moa.execution_trigger AS trigger
                WHERE trigger.trigger_uid = ANY($1)
                  AND trigger.state = 'pending'
                  AND trigger.due_at <= now() - make_interval(secs => $2)
                  AND (
                      NOT EXISTS (
                          SELECT 1
                          FROM moa.execution_dispatch_outbox AS dispatch
                          WHERE dispatch.tenant_id = trigger.tenant_id
                            AND dispatch.trigger_uid = trigger.trigger_uid
                            AND dispatch.dispatch_kind = 'trigger_delivery'
                      )
                      OR EXISTS (
                          SELECT 1
                          FROM moa.execution_dispatch_outbox AS dispatch
                          WHERE dispatch.tenant_id = trigger.tenant_id
                            AND dispatch.trigger_uid = trigger.trigger_uid
                            AND dispatch.dispatch_kind = 'trigger_delivery'
                            AND dispatch.state = 'delivered'
                            AND dispatch.delivered_at
                                <= now() - make_interval(secs => $2)
                      )
                  )
                ORDER BY trigger.due_at, trigger.tenant_id, trigger.trigger_uid
                FOR UPDATE OF trigger SKIP LOCKED
                "#,
            )
            .bind(&candidate_trigger_uids)
            .bind(RESTATE_STATE_LOSS_REDRIVE_GRACE_SECONDS)
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        };
        let mut dispatches = Vec::with_capacity(batch_size as usize);
        for row in rows {
            let mut trigger = trigger_from_row(&row)?;
            if !trigger_is_current(conn.as_mut(), &trigger).await? {
                sqlx::query(
                    "UPDATE moa.execution_trigger SET state = 'superseded', updated_at = now() \
                     WHERE trigger_uid = $1 AND state = 'pending'",
                )
                .bind(trigger.trigger_uid)
                .execute(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
                release_trigger_capacity_in_conn(conn.as_mut(), &trigger).await?;
                continue;
            }
            let persisted_dispatch_uid: Option<Uuid> = sqlx::query_scalar(
                "SELECT dispatch_uid FROM moa.execution_dispatch_outbox \
                 WHERE tenant_id=$1 AND trigger_uid=$2 \
                   AND dispatch_kind='trigger_delivery'",
            )
            .bind(trigger.tenant_id.0)
            .bind(trigger.trigger_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let dispatch_uid = match persisted_dispatch_uid {
                Some(dispatch_uid) => dispatch_uid,
                None => {
                    trigger.updated_at = sqlx::query_scalar(
                        "UPDATE moa.execution_trigger \
                         SET updated_at=GREATEST(now(), updated_at + INTERVAL '1 microsecond') \
                         WHERE trigger_uid=$1 RETURNING updated_at",
                    )
                    .bind(trigger.trigger_uid)
                    .fetch_one(conn.as_mut())
                    .await
                    .map_err(sqlx_error)?;
                    repaired_trigger_delivery_dispatch_uid(&trigger)
                }
            };
            let request = trigger_delivery_dispatch_with_uid(&trigger, dispatch_uid);
            dispatches.push(
                match requeue_delivered_dispatch_in_conn(conn.as_mut(), &request).await? {
                    Some(dispatch) => dispatch,
                    None => enqueue_dispatch_in_conn(conn.as_mut(), &request).await?,
                },
            );
        }
        if run_budget > 0 {
            let run_dispatches = requeue_current_run_activations_in_conn(
                conn.as_mut(),
                run_budget,
                RESTATE_STATE_LOSS_REDRIVE_GRACE_SECONDS,
            )
            .await?;
            dispatches.extend(run_dispatches);
        }
        if accepted_budget > 0 {
            dispatches.extend(
                requeue_current_accepted_dispatches_in_conn(
                    conn.as_mut(),
                    accepted_budget,
                    RESTATE_STATE_LOSS_REDRIVE_GRACE_SECONDS,
                )
                .await?,
            );
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(dispatches)
    }
}

async fn prelock_trigger_scheduled_capacity_in_conn(
    conn: &mut PgConnection,
    trigger_uid: Uuid,
) -> Result<()> {
    let tenant_id: Option<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM moa.execution_trigger WHERE trigger_uid=$1")
            .bind(trigger_uid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_error)?;
    if let Some(tenant_id) = tenant_id {
        prelock_existing_capacity_dimensions_in_tx(
            conn,
            TenantId(tenant_id),
            &[ExecutionCapacityDimension::ScheduledTriggers],
        )
        .await?;
    }
    Ok(())
}

fn reconcile_lane_budgets(batch_size: u32) -> (u32, u32, u32) {
    let lane_base = batch_size / 3;
    let lane_remainder = batch_size % 3;
    (
        lane_base + u32::from(lane_remainder > 0),
        lane_base + u32::from(lane_remainder > 1),
        lane_base,
    )
}

/// Inserts a trigger and matching delayed outbox row in the caller transaction.
pub async fn create_trigger_with_dispatch_in_conn(
    conn: &mut PgConnection,
    config: &ExecutionConfig,
    request: &NewExecutionTrigger,
) -> Result<ExecutionTriggerWrite> {
    let trigger = create_trigger_in_conn(conn, request).await?;
    if trigger.state == ExecutionDeliveryState::Pending
        && reserve_capacity_in_tx(conn, config, trigger_capacity_request(&trigger)).await?
            == CapacityReserveOutcome::Saturated
    {
        return Err(Error::CapacitySaturated {
            dimension: ExecutionCapacityDimension::ScheduledTriggers.as_str(),
        });
    }
    let dispatch = enqueue_dispatch_in_conn(conn, &trigger_delivery_dispatch(&trigger)).await?;
    Ok(ExecutionTriggerWrite { trigger, dispatch })
}

/// Fires a trigger and enqueues its run activation in the caller transaction.
pub async fn fire_trigger_in_conn(
    conn: &mut PgConnection,
    trigger_uid: Uuid,
) -> Result<ExecutionTriggerFireOutcome> {
    let row = sqlx::query(
        "SELECT trigger.*, trigger.due_at <= now() AS is_due \
         FROM moa.execution_trigger AS trigger WHERE trigger_uid = $1 FOR UPDATE",
    )
    .bind(trigger_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(ExecutionTriggerFireOutcome::NoOp(
            ExecutionTriggerNoOp::NotFound,
        ));
    };
    let trigger = trigger_from_row(&row)?;
    if matches!(
        trigger.kind,
        ExecutionTriggerKind::TaskTimer
            | ExecutionTriggerKind::WaitExpiry
            | ExecutionTriggerKind::TaskWatchdog
            | ExecutionTriggerKind::CompensationWatchdog
            | ExecutionTriggerKind::ExternalReconcile
            | ExecutionTriggerKind::ExternalStartRecovery
    ) {
        return Err(Error::InvalidRepositoryInput {
            message: "wait, watchdog, and external-job triggers require typed settlement"
                .to_string(),
        });
    }
    match trigger.state {
        ExecutionDeliveryState::Delivered => {
            settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Delivered).await?;
            release_trigger_capacity_in_conn(conn, &trigger).await?;
            return Ok(ExecutionTriggerFireOutcome::NoOp(
                ExecutionTriggerNoOp::Duplicate,
            ));
        }
        ExecutionDeliveryState::Superseded
        | ExecutionDeliveryState::Cancelled
        | ExecutionDeliveryState::DeadLetter => {
            settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Cancelled).await?;
            release_trigger_capacity_in_conn(conn, &trigger).await?;
            return Ok(ExecutionTriggerFireOutcome::NoOp(
                ExecutionTriggerNoOp::Inactive,
            ));
        }
        ExecutionDeliveryState::Pending | ExecutionDeliveryState::Dispatching => {}
    }
    let is_due = row.try_get::<bool, _>("is_due").map_err(super::row_error)?;
    if !is_due {
        return Ok(ExecutionTriggerFireOutcome::NoOp(
            ExecutionTriggerNoOp::NotDue,
        ));
    }
    if !trigger_is_current(conn, &trigger).await? {
        sqlx::query(
            r#"
            UPDATE moa.execution_trigger
            SET state = 'superseded', updated_at = now()
            WHERE trigger_uid = $1 AND state = 'pending'
            "#,
        )
        .bind(trigger_uid)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
        settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Cancelled).await?;
        release_trigger_capacity_in_conn(conn, &trigger).await?;
        return Ok(ExecutionTriggerFireOutcome::NoOp(
            ExecutionTriggerNoOp::StaleGeneration,
        ));
    }

    sqlx::query(
        r#"
        UPDATE moa.execution_trigger
        SET state = 'delivered', delivered_at = now(), last_error = NULL,
            updated_at = now()
        WHERE trigger_uid = $1 AND state = 'pending'
        "#,
    )
    .bind(trigger_uid)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Delivered).await?;
    release_trigger_capacity_in_conn(conn, &trigger).await?;

    let activation = if let (Some(run_uid), Some(controller_generation)) =
        (trigger.run_uid, trigger.controller_generation)
    {
        let activation_allowed = sqlx::query_scalar::<_, bool>(
            "SELECT status NOT IN ('pause_requested','pausing','paused') \
             FROM moa.execution_run WHERE tenant_id = $1 AND run_uid = $2 \
               AND controller_generation = $3",
        )
        .bind(trigger.tenant_id.0)
        .bind(run_uid)
        .bind(to_i64(controller_generation, "controller generation")?)
        .fetch_optional(&mut *conn)
        .await
        .map_err(sqlx_error)?
        .unwrap_or(false);
        if activation_allowed {
            Some(Box::new(
                enqueue_run_activation_in_conn(
                    conn,
                    trigger.tenant_id,
                    run_uid,
                    controller_generation,
                    Utc::now(),
                    json!({ "trigger_uid": trigger.trigger_uid }),
                )
                .await?,
            ))
        } else {
            None
        }
    } else {
        None
    };
    Ok(ExecutionTriggerFireOutcome::Delivered { activation })
}

/// Delivers a due task wait trigger without committing or enqueueing controller work.
pub(super) async fn deliver_wait_trigger_in_conn(
    conn: &mut PgConnection,
    trigger_uid: Uuid,
) -> Result<ExecutionWaitTriggerDeliveryOutcome> {
    let row = sqlx::query(
        "SELECT trigger.*, now() AS observed_at, trigger.due_at <= now() AS is_due \
         FROM moa.execution_trigger AS trigger WHERE trigger_uid = $1 FOR UPDATE",
    )
    .bind(trigger_uid)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(ExecutionWaitTriggerDeliveryOutcome::NoOp(
            ExecutionTriggerNoOp::NotFound,
        ));
    };
    let trigger = trigger_from_row(&row)?;
    if !matches!(
        trigger.kind,
        ExecutionTriggerKind::TaskTimer | ExecutionTriggerKind::WaitExpiry
    ) {
        return Err(Error::InvalidRepositoryInput {
            message: "wait-trigger delivery accepts only task_timer or wait_expiry".to_string(),
        });
    }
    match trigger.state {
        ExecutionDeliveryState::Delivered => {
            settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Delivered).await?;
            release_trigger_capacity_in_conn(conn, &trigger).await?;
            return Ok(ExecutionWaitTriggerDeliveryOutcome::NoOp(
                ExecutionTriggerNoOp::Duplicate,
            ));
        }
        ExecutionDeliveryState::Superseded
        | ExecutionDeliveryState::Cancelled
        | ExecutionDeliveryState::DeadLetter => {
            settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Cancelled).await?;
            release_trigger_capacity_in_conn(conn, &trigger).await?;
            return Ok(ExecutionWaitTriggerDeliveryOutcome::NoOp(
                ExecutionTriggerNoOp::Inactive,
            ));
        }
        ExecutionDeliveryState::Pending | ExecutionDeliveryState::Dispatching => {}
    }
    if !row.try_get::<bool, _>("is_due").map_err(super::row_error)? {
        return Ok(ExecutionWaitTriggerDeliveryOutcome::NoOp(
            ExecutionTriggerNoOp::NotDue,
        ));
    }
    if !trigger_is_current(conn, &trigger).await? {
        sqlx::query(
            "UPDATE moa.execution_trigger SET state='superseded', updated_at=now() \
             WHERE trigger_uid=$1 AND state='pending'",
        )
        .bind(trigger_uid)
        .execute(&mut *conn)
        .await
        .map_err(sqlx_error)?;
        settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Cancelled).await?;
        release_trigger_capacity_in_conn(conn, &trigger).await?;
        return Ok(ExecutionWaitTriggerDeliveryOutcome::NoOp(
            ExecutionTriggerNoOp::StaleGeneration,
        ));
    }
    let observed_at = row
        .try_get::<DateTime<Utc>, _>("observed_at")
        .map_err(super::row_error)?;
    sqlx::query(
        "UPDATE moa.execution_trigger SET state='delivered', delivered_at=$2, \
         last_error=NULL, updated_at=now() \
         WHERE trigger_uid=$1 AND state='pending'",
    )
    .bind(trigger_uid)
    .bind(observed_at)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Delivered).await?;
    release_trigger_capacity_in_conn(conn, &trigger).await?;
    Ok(ExecutionWaitTriggerDeliveryOutcome::Delivered {
        trigger: Box::new(trigger),
        observed_at,
    })
}

/// Supersedes one exact trigger generation without committing the caller transaction.
#[allow(clippy::too_many_arguments)]
pub async fn supersede_trigger_in_conn(
    conn: &mut PgConnection,
    trigger_uid: Uuid,
    expected_kind: ExecutionTriggerKind,
    expected_controller_generation: Option<u64>,
    expected_attempt_generation: Option<u64>,
    expected_compensation_generation: Option<u64>,
    expected_compensation_attempt_generation: Option<u64>,
) -> Result<ExecutionTriggerSupersedeOutcome> {
    let row = sqlx::query(
        r#"
        SELECT *
        FROM moa.execution_trigger
        WHERE trigger_uid = $1
          AND trigger_kind = $2
          AND controller_generation IS NOT DISTINCT FROM $3
          AND attempt_generation IS NOT DISTINCT FROM $4
          AND compensation_generation IS NOT DISTINCT FROM $5
          AND compensation_attempt_generation IS NOT DISTINCT FROM $6
        FOR UPDATE
        "#,
    )
    .bind(trigger_uid)
    .bind(expected_kind.as_str())
    .bind(to_optional_i64(
        expected_controller_generation,
        "controller generation",
    )?)
    .bind(to_optional_i64(
        expected_attempt_generation,
        "attempt generation",
    )?)
    .bind(to_optional_i64(
        expected_compensation_generation,
        "compensation generation",
    )?)
    .bind(to_optional_i64(
        expected_compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(ExecutionTriggerSupersedeOutcome::StaleOrMissing);
    };
    let trigger = trigger_from_row(&row)?;
    let state = trigger.state;
    match state {
        ExecutionDeliveryState::Pending | ExecutionDeliveryState::Dispatching => {
            sqlx::query(
                r#"
                UPDATE moa.execution_trigger
                SET state = 'superseded', updated_at = now()
                WHERE trigger_uid = $1
                "#,
            )
            .bind(trigger_uid)
            .execute(&mut *conn)
            .await
            .map_err(sqlx_error)?;
            settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Cancelled).await?;
            release_trigger_capacity_in_conn(conn, &trigger).await?;
            Ok(ExecutionTriggerSupersedeOutcome::Superseded)
        }
        ExecutionDeliveryState::Superseded => {
            settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Cancelled).await?;
            release_trigger_capacity_in_conn(conn, &trigger).await?;
            Ok(ExecutionTriggerSupersedeOutcome::AlreadySuperseded)
        }
        ExecutionDeliveryState::Delivered => {
            settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Delivered).await?;
            release_trigger_capacity_in_conn(conn, &trigger).await?;
            Ok(ExecutionTriggerSupersedeOutcome::AlreadyInactive)
        }
        ExecutionDeliveryState::Cancelled | ExecutionDeliveryState::DeadLetter => {
            settle_trigger_dispatch(conn, trigger_uid, ExecutionDeliveryState::Cancelled).await?;
            release_trigger_capacity_in_conn(conn, &trigger).await?;
            Ok(ExecutionTriggerSupersedeOutcome::AlreadyInactive)
        }
    }
}

/// Releases the exact capacity receipt for one trigger owner fence.
pub(super) async fn release_trigger_capacity_in_conn(
    conn: &mut PgConnection,
    trigger: &ExecutionTriggerRecord,
) -> Result<()> {
    match release_capacity_in_tx(conn, trigger_capacity_request(trigger)).await? {
        CapacityReleaseOutcome::Released | CapacityReleaseOutcome::AlreadyReleased => Ok(()),
        CapacityReleaseOutcome::NotFound | CapacityReleaseOutcome::Stale => {
            Err(Error::InvalidRepositoryData {
                message: "trigger capacity release lost its exact owner fence".to_string(),
            })
        }
    }
}

fn trigger_capacity_request(trigger: &ExecutionTriggerRecord) -> ExecutionCapacityRequest {
    ExecutionCapacityRequest {
        reservation_uid: execution_capacity_reservation_uid(
            ExecutionCapacityDimension::ScheduledTriggers,
            trigger.trigger_uid,
            None,
        ),
        tenant_id: trigger.tenant_id,
        run_uid: trigger.run_uid,
        controller_generation: trigger.controller_generation,
        dimension: ExecutionCapacityDimension::ScheduledTriggers,
        owner: ExecutionCapacityOwner::Trigger {
            trigger_uid: trigger.trigger_uid,
        },
        expires_at: None,
    }
}

pub(super) async fn settle_trigger_dispatch(
    conn: &mut PgConnection,
    trigger_uid: Uuid,
    state: ExecutionDeliveryState,
) -> Result<()> {
    let delivered_at = (state == ExecutionDeliveryState::Delivered).then(Utc::now);
    sqlx::query(
        r#"
        UPDATE moa.execution_dispatch_outbox
        SET state = $2, claim_owner = NULL, claimed_at = NULL, claim_expires_at = NULL,
            delivered_at = $3, last_error = NULL, updated_at = now()
        WHERE trigger_uid = $1 AND dispatch_kind = 'trigger_delivery'
          AND state IN ('pending', 'dispatching')
        "#,
    )
    .bind(trigger_uid)
    .bind(state.as_str())
    .bind(delivered_at)
    .execute(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    Ok(())
}

async fn create_trigger_in_conn(
    conn: &mut PgConnection,
    request: &NewExecutionTrigger,
) -> Result<ExecutionTriggerRecord> {
    validate_trigger(request)?;
    let inserted = sqlx::query(
        r#"
        INSERT INTO moa.execution_trigger (
            trigger_uid, tenant_id, run_uid, task_id, compensation_id, schedule_uid,
            schedule_incarnation,
            trigger_kind, controller_generation, attempt_generation,
            compensation_generation, compensation_attempt_generation,
            occurrence_sequence, due_at, payload
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        ON CONFLICT (trigger_uid) DO NOTHING
        RETURNING *
        "#,
    )
    .bind(request.trigger_uid)
    .bind(request.tenant_id.0)
    .bind(request.run_uid)
    .bind(request.task_id)
    .bind(request.compensation_id)
    .bind(request.schedule_uid)
    .bind(to_optional_i64(
        request.schedule_incarnation,
        "schedule incarnation",
    )?)
    .bind(request.kind.as_str())
    .bind(to_optional_i64(
        request.controller_generation,
        "controller generation",
    )?)
    .bind(to_optional_i64(
        request.attempt_generation,
        "attempt generation",
    )?)
    .bind(to_optional_i64(
        request.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_optional_i64(
        request.compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(to_optional_i64(
        request.occurrence_sequence,
        "occurrence sequence",
    )?)
    .bind(request.due_at)
    .bind(&request.payload)
    .fetch_optional(&mut *conn)
    .await
    .map_err(sqlx_error)?;
    let row = match inserted {
        Some(row) => row,
        None => sqlx::query("SELECT * FROM moa.execution_trigger WHERE trigger_uid = $1")
            .bind(request.trigger_uid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(sqlx_error)?
            .ok_or_else(|| Error::Storage {
                message: "trigger insert conflicted without a visible replay row".to_string(),
            })?,
    };
    let record = trigger_from_row(&row)?;
    if !trigger_matches_request(&record, request) {
        return Err(Error::InvalidRepositoryInput {
            message: "trigger UID is already bound to different immutable semantics".to_string(),
        });
    }
    Ok(record)
}

async fn trigger_is_current(
    conn: &mut PgConnection,
    trigger: &ExecutionTriggerRecord,
) -> Result<bool> {
    let current = match trigger.kind {
        ExecutionTriggerKind::ScheduleOccurrence => sqlx::query_scalar::<_, bool>(
            r#"
                SELECT EXISTS (
                    SELECT 1 FROM moa.execution_schedule
                    WHERE schedule_uid = $1 AND tenant_id = $2 AND status = 'active'
                      AND schedule_incarnation = $3
                      AND last_occurrence_sequence + 1 = $4
                )
                "#,
        )
        .bind(trigger.schedule_uid)
        .bind(trigger.tenant_id.0)
        .bind(to_optional_i64(
            trigger.schedule_incarnation,
            "schedule incarnation",
        )?)
        .bind(to_optional_i64(
            trigger.occurrence_sequence,
            "occurrence sequence",
        )?)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlx_error)?,
        ExecutionTriggerKind::RunDeadline => run_deadline_is_current(conn, trigger).await?,
        ExecutionTriggerKind::TaskTimer
        | ExecutionTriggerKind::WaitExpiry
        | ExecutionTriggerKind::TaskWatchdog => {
            let expected_statuses: &[&str] = match trigger.kind {
                ExecutionTriggerKind::TaskTimer => &["waiting_timer"],
                ExecutionTriggerKind::WaitExpiry => {
                    &["waiting_input", "waiting_review", "waiting_signal"]
                }
                ExecutionTriggerKind::TaskWatchdog => &["dispatching", "running"],
                ExecutionTriggerKind::RunDeadline
                | ExecutionTriggerKind::ExternalReconcile
                | ExecutionTriggerKind::ExternalStartRecovery
                | ExecutionTriggerKind::ScheduleOccurrence
                | ExecutionTriggerKind::CompensationWatchdog => {
                    unreachable!("non-task trigger handled above")
                }
            };
            let task_is_current = sqlx::query_scalar::<_, bool>(
                r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM moa.execution_task AS task
                    JOIN moa.execution_run AS run
                      ON run.run_uid = task.run_uid AND run.tenant_id = task.tenant_id
                    WHERE task.task_id = $1 AND task.run_uid = $2 AND task.tenant_id = $3
                      AND (
                          ($7 AND task.generation = $4)
                          OR (NOT $7 AND task.attempt_generation = $4)
                      )
                      AND task.status = ANY($5)
                      AND ($7 OR run.controller_generation = $6)
                      AND run.status NOT IN (
                          'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
                      )
                )
                "#,
            )
            .bind(trigger.task_id)
            .bind(trigger.run_uid)
            .bind(trigger.tenant_id.0)
            .bind(to_optional_i64(
                trigger.attempt_generation,
                "attempt generation",
            )?)
            .bind(expected_statuses)
            .bind(to_optional_i64(
                trigger.controller_generation,
                "controller generation",
            )?)
            .bind(matches!(
                trigger.kind,
                ExecutionTriggerKind::TaskTimer | ExecutionTriggerKind::WaitExpiry
            ))
            .fetch_one(&mut *conn)
            .await
            .map_err(sqlx_error)?;
            if trigger.kind == ExecutionTriggerKind::TaskWatchdog && task_is_current {
                sqlx::query_scalar::<_, bool>(
                    r#"
                    SELECT EXISTS (
                        SELECT 1 FROM moa.execution_task
                        WHERE task_id = $1 AND tenant_id = $2
                          AND attempt_state IN ('dispatching','running')
                    )
                    "#,
                )
                .bind(trigger.task_id)
                .bind(trigger.tenant_id.0)
                .fetch_one(&mut *conn)
                .await
                .map_err(sqlx_error)?
            } else {
                task_is_current
            }
        }
        ExecutionTriggerKind::ExternalReconcile => {
            external_job_trigger_is_current(conn, trigger, false).await?
        }
        ExecutionTriggerKind::ExternalStartRecovery => {
            external_job_trigger_is_current(conn, trigger, true).await?
        }
        ExecutionTriggerKind::CompensationWatchdog => sqlx::query_scalar::<_, bool>(
            r#"
                SELECT EXISTS (
                    SELECT 1
                    FROM moa.execution_compensation AS compensation
                    JOIN moa.execution_run AS run
                      ON run.run_uid = compensation.run_uid
                     AND run.tenant_id = compensation.tenant_id
                    WHERE compensation.compensation_id = $1
                      AND compensation.run_uid = $2
                      AND compensation.tenant_id = $3
                      AND compensation.generation = $4
                      AND compensation.attempt_generation = $5
                      AND compensation.status = 'running'
                      AND compensation.attempt_state = ANY($6)
                      AND run.controller_generation = $7
                      AND run.status = 'compensating'
                )
                "#,
        )
        .bind(trigger.compensation_id)
        .bind(trigger.run_uid)
        .bind(trigger.tenant_id.0)
        .bind(to_optional_i64(
            trigger.compensation_generation,
            "compensation generation",
        )?)
        .bind(to_optional_i64(
            trigger.compensation_attempt_generation,
            "compensation attempt generation",
        )?)
        .bind(["dispatching", "running"])
        .bind(to_optional_i64(
            trigger.controller_generation,
            "controller generation",
        )?)
        .fetch_one(&mut *conn)
        .await
        .map_err(sqlx_error)?,
    };
    Ok(current)
}

async fn external_job_trigger_is_current(
    conn: &mut PgConnection,
    trigger: &ExecutionTriggerRecord,
    require_unbound: bool,
) -> Result<bool> {
    let external_job_uid = trigger
        .payload
        .get("external_job_uid")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    let job_generation = trigger
        .payload
        .get("job_generation")
        .and_then(Value::as_u64)
        .map(|value| to_i64(value, "external job generation"))
        .transpose()?;
    if external_job_uid.is_none() || job_generation.is_none() {
        return Ok(false);
    }
    let declared_provider = trigger
        .payload
        .get("declared_provider")
        .and_then(Value::as_str);
    let idempotency_key = trigger
        .payload
        .get("idempotency_key")
        .and_then(Value::as_str);
    if require_unbound && (declared_provider.is_none() || idempotency_key.is_none()) {
        return Ok(false);
    }
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
          SELECT 1 FROM moa.execution_external_job AS job
          WHERE job.external_job_uid=$1 AND job.tenant_id=$2 AND job.run_uid=$3
            AND job.job_generation=$4
            AND job.task_id IS NOT DISTINCT FROM $5
            AND job.attempt_generation IS NOT DISTINCT FROM $6
            AND job.compensation_id IS NOT DISTINCT FROM $7
            AND job.compensation_generation IS NOT DISTINCT FROM $8
            AND job.compensation_attempt_generation IS NOT DISTINCT FROM $9
            AND (
              ($10 AND job.state='unbound' AND job.declared_provider=$11
                   AND job.idempotency_key=$12)
              OR (NOT $10 AND job.state IN (
                   'starting','running','waiting_reconcile','cancel_requested'
              ))
            )
        )
        "#,
    )
    .bind(external_job_uid)
    .bind(trigger.tenant_id.0)
    .bind(trigger.run_uid)
    .bind(job_generation)
    .bind(trigger.task_id)
    .bind(to_optional_i64(
        trigger.attempt_generation,
        "attempt generation",
    )?)
    .bind(trigger.compensation_id)
    .bind(to_optional_i64(
        trigger.compensation_generation,
        "compensation generation",
    )?)
    .bind(to_optional_i64(
        trigger.compensation_attempt_generation,
        "compensation attempt generation",
    )?)
    .bind(require_unbound)
    .bind(declared_provider)
    .bind(idempotency_key)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)
}

fn start_recovery_request_matches_trigger(
    request: &ExecutionExternalJobStartRecoveryRequest,
    trigger: &ExecutionTriggerRecord,
) -> bool {
    let owner_matches = match (&request.owner, trigger.task_id, trigger.compensation_id) {
        (
            ExecutionExternalJobStartRecoveryOwner::Task {
                task_id,
                attempt_generation,
            },
            Some(trigger_task_id),
            None,
        ) => {
            *task_id == trigger_task_id
                && Some(*attempt_generation) == trigger.attempt_generation
                && trigger.compensation_generation.is_none()
                && trigger.compensation_attempt_generation.is_none()
        }
        (
            ExecutionExternalJobStartRecoveryOwner::Compensation {
                compensation_id,
                compensation_generation,
                compensation_attempt_generation,
            },
            None,
            Some(trigger_compensation_id),
        ) => {
            *compensation_id == trigger_compensation_id
                && Some(*compensation_generation) == trigger.compensation_generation
                && Some(*compensation_attempt_generation) == trigger.compensation_attempt_generation
                && trigger.attempt_generation.is_none()
        }
        _ => false,
    };
    owner_matches
        && request.trigger_uid == trigger.trigger_uid
        && request.tenant_id == trigger.tenant_id
        && Some(request.run_uid) == trigger.run_uid
        && trigger.payload
            == json!({
                "external_job_uid": request.external_job_uid,
                "job_generation": request.job_generation,
                "declared_provider": request.provider,
                "idempotency_key": request.idempotency_key,
            })
}

async fn run_deadline_is_current(
    conn: &mut PgConnection,
    trigger: &ExecutionTriggerRecord,
) -> Result<bool> {
    if !run_deadline_payload_matches(trigger)? {
        return Ok(false);
    }
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM moa.execution_run
            WHERE run_uid = $1 AND tenant_id = $2 AND budget_deadline_at = $3
              AND budget_deadline_suspended_at IS NULL
              AND status NOT IN (
                  'completed', 'partial', 'blocked', 'unsupported', 'failed', 'cancelled'
              )
        )
        "#,
    )
    .bind(trigger.run_uid)
    .bind(trigger.tenant_id.0)
    .bind(trigger.due_at)
    .fetch_one(&mut *conn)
    .await
    .map_err(sqlx_error)
}

fn run_deadline_payload_matches(trigger: &ExecutionTriggerRecord) -> Result<bool> {
    let payload: RunDeadlineTriggerPayload = serde_json::from_value(trigger.payload.clone())
        .map_err(|error| Error::InvalidRepositoryData {
            message: format!("invalid run deadline trigger payload: {error}"),
        })?;
    Ok(Some(payload.run_uid) == trigger.run_uid && payload.deadline_at == trigger.due_at)
}

fn trigger_delivery_dispatch(trigger: &ExecutionTriggerRecord) -> NewExecutionDispatch {
    trigger_delivery_dispatch_with_uid(
        trigger,
        Uuid::new_v5(&TRIGGER_DISPATCH_NAMESPACE, trigger.trigger_uid.as_bytes()),
    )
}

fn trigger_delivery_dispatch_with_uid(
    trigger: &ExecutionTriggerRecord,
    dispatch_uid: Uuid,
) -> NewExecutionDispatch {
    NewExecutionDispatch {
        dispatch_uid,
        tenant_id: trigger.tenant_id,
        run_uid: None,
        task_id: None,
        compensation_id: None,
        trigger_uid: Some(trigger.trigger_uid),
        external_job_uid: None,
        kind: ExecutionDispatchKind::TriggerDelivery,
        controller_generation: None,
        wake_epoch: None,
        attempt_generation: None,
        compensation_generation: None,
        compensation_attempt_generation: None,
        not_before_at: trigger.due_at,
        payload: json!({
            "trigger_uid": trigger.trigger_uid,
            "trigger_kind": trigger.kind.as_str(),
        }),
    }
}

fn repaired_trigger_delivery_dispatch_uid(trigger: &ExecutionTriggerRecord) -> Uuid {
    Uuid::new_v5(
        &TRIGGER_DISPATCH_NAMESPACE,
        format!(
            "{}:repair:{}:{}",
            trigger.trigger_uid,
            trigger.due_at.timestamp_micros(),
            trigger.updated_at.timestamp_micros()
        )
        .as_bytes(),
    )
}

fn rearmed_trigger_delivery_dispatch_uid(trigger_uid: Uuid, retry_at: DateTime<Utc>) -> Uuid {
    Uuid::new_v5(
        &TRIGGER_DISPATCH_NAMESPACE,
        format!("{trigger_uid}:rearm:{}", retry_at.timestamp_micros()).as_bytes(),
    )
}

fn validate_trigger(request: &NewExecutionTrigger) -> Result<()> {
    let generation_is_zero = [
        request.schedule_incarnation,
        request.controller_generation,
        request.attempt_generation,
        request.compensation_generation,
        request.compensation_attempt_generation,
        request.occurrence_sequence,
    ]
    .into_iter()
    .flatten()
    .any(|generation| generation == 0);
    if request.trigger_uid.is_nil() || !request.payload.is_object() || generation_is_zero {
        return Err(Error::InvalidRepositoryInput {
            message:
                "execution trigger requires a non-nil UID, positive generations, and object payload"
                    .to_string(),
        });
    }
    let valid = match request.kind {
        ExecutionTriggerKind::ScheduleOccurrence => {
            request.schedule_uid.is_some()
                && request.schedule_incarnation.is_some()
                && request.occurrence_sequence.is_some()
                && request.run_uid.is_none()
                && request.task_id.is_none()
                && request.compensation_id.is_none()
                && request.controller_generation.is_none()
                && request.attempt_generation.is_none()
                && request.compensation_generation.is_none()
                && request.compensation_attempt_generation.is_none()
        }
        ExecutionTriggerKind::RunDeadline => {
            request.run_uid.is_some()
                && request.task_id.is_none()
                && request.compensation_id.is_none()
                && request.schedule_uid.is_none()
                && request.schedule_incarnation.is_none()
                && request.controller_generation.is_some()
                && request.attempt_generation.is_none()
                && request.compensation_generation.is_none()
                && request.compensation_attempt_generation.is_none()
                && request.occurrence_sequence.is_none()
        }
        ExecutionTriggerKind::TaskTimer
        | ExecutionTriggerKind::WaitExpiry
        | ExecutionTriggerKind::TaskWatchdog => {
            request.run_uid.is_some()
                && request.task_id.is_some()
                && request.compensation_id.is_none()
                && request.schedule_uid.is_none()
                && request.schedule_incarnation.is_none()
                && request.controller_generation.is_some()
                && request.attempt_generation.is_some()
                && request.compensation_generation.is_none()
                && request.compensation_attempt_generation.is_none()
                && request.occurrence_sequence.is_none()
        }
        ExecutionTriggerKind::ExternalReconcile | ExecutionTriggerKind::ExternalStartRecovery => {
            let task_owner = request.task_id.is_some()
                && request.attempt_generation.is_some()
                && request.compensation_id.is_none()
                && request.compensation_generation.is_none()
                && request.compensation_attempt_generation.is_none();
            let compensation_owner = request.task_id.is_none()
                && request.attempt_generation.is_none()
                && request.compensation_id.is_some()
                && request.compensation_generation.is_some()
                && request.compensation_attempt_generation.is_some();
            request.run_uid.is_some()
                && request.schedule_uid.is_none()
                && request.schedule_incarnation.is_none()
                && request.controller_generation.is_some()
                && request.occurrence_sequence.is_none()
                && (task_owner || compensation_owner)
        }
        ExecutionTriggerKind::CompensationWatchdog => {
            request.run_uid.is_some()
                && request.task_id.is_none()
                && request.compensation_id.is_some()
                && request.schedule_uid.is_none()
                && request.schedule_incarnation.is_none()
                && request.controller_generation.is_some()
                && request.attempt_generation.is_none()
                && request.compensation_generation.is_some()
                && request.compensation_attempt_generation.is_some()
                && request.occurrence_sequence.is_none()
        }
    };
    if !valid {
        return Err(Error::InvalidRepositoryInput {
            message: format!(
                "execution trigger target shape does not match {}",
                request.kind.as_str()
            ),
        });
    }
    Ok(())
}

fn trigger_matches_request(record: &ExecutionTriggerRecord, request: &NewExecutionTrigger) -> bool {
    record.trigger_uid == request.trigger_uid
        && record.tenant_id == request.tenant_id
        && record.run_uid == request.run_uid
        && record.task_id == request.task_id
        && record.compensation_id == request.compensation_id
        && record.schedule_uid == request.schedule_uid
        && record.schedule_incarnation == request.schedule_incarnation
        && record.kind == request.kind
        && record.controller_generation == request.controller_generation
        && record.attempt_generation == request.attempt_generation
        && record.compensation_generation == request.compensation_generation
        && record.compensation_attempt_generation == request.compensation_attempt_generation
        && record.occurrence_sequence == request.occurrence_sequence
        && record.due_at.timestamp_micros() == request.due_at.timestamp_micros()
        && record.payload == request.payload
}

pub(super) fn trigger_from_row(row: &sqlx::postgres::PgRow) -> Result<ExecutionTriggerRecord> {
    let controller_generation = row
        .try_get::<Option<i64>, _>("controller_generation")
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
    let occurrence_sequence = row
        .try_get::<Option<i64>, _>("occurrence_sequence")
        .map_err(super::row_error)?;
    let schedule_incarnation = row
        .try_get::<Option<i64>, _>("schedule_incarnation")
        .map_err(super::row_error)?;
    Ok(ExecutionTriggerRecord {
        trigger_uid: row.try_get("trigger_uid").map_err(super::row_error)?,
        tenant_id: TenantId(row.try_get("tenant_id").map_err(super::row_error)?),
        run_uid: row.try_get("run_uid").map_err(super::row_error)?,
        task_id: row.try_get("task_id").map_err(super::row_error)?,
        compensation_id: row.try_get("compensation_id").map_err(super::row_error)?,
        schedule_uid: row.try_get("schedule_uid").map_err(super::row_error)?,
        schedule_incarnation: schedule_incarnation
            .map(|value| super::to_u64(value, "schedule incarnation"))
            .transpose()?,
        kind: row
            .try_get::<String, _>("trigger_kind")
            .map_err(super::row_error)?
            .parse()?,
        state: row
            .try_get::<String, _>("state")
            .map_err(super::row_error)?
            .parse()?,
        controller_generation: controller_generation
            .map(|value| super::to_u64(value, "controller generation"))
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
        occurrence_sequence: occurrence_sequence
            .map(|value| super::to_u64(value, "occurrence sequence"))
            .transpose()?,
        due_at: row.try_get("due_at").map_err(super::row_error)?,
        payload: row.try_get("payload").map_err(super::row_error)?,
        delivered_at: row.try_get("delivered_at").map_err(super::row_error)?,
        created_at: row.try_get("created_at").map_err(super::row_error)?,
        updated_at: row.try_get("updated_at").map_err(super::row_error)?,
    })
}

#[cfg(test)]
mod tests {
    use super::reconcile_lane_budgets;

    #[test]
    fn every_reconciliation_lane_has_budget_under_sustained_backlog() {
        // Pins: a due-trigger backlog cannot consume the entire bounded pass and starve
        // accepted-before-start dispatches or queued run activations.
        for batch_size in [3, 4, 5, 32, 1_000] {
            let (triggers, accepted, runs) = reconcile_lane_budgets(batch_size);
            assert!(triggers > 0, "batch {batch_size}");
            assert!(accepted > 0, "batch {batch_size}");
            assert!(runs > 0, "batch {batch_size}");
            assert_eq!(triggers + accepted + runs, batch_size);
        }
    }
}
