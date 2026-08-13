//! Logical-task reservation, redispatch, waiting, and review-resolution persistence.

use crate::state::{cancelled_task_outcome, completed_task_outcome, failed_task_outcome};
use crate::wire::{ExecutionActionReviewResolution, ExecutionTaskAttemptRequest};
use moa_config::ExecutionConfig;
use moa_core::{
    canonical_json::canonical_json_bytes,
    types::{
        context::ContextMessage,
        sandbox_workspace::{ExecutionHandReleaseOwner, ExecutionHandReleaseReceipt},
    },
};

use super::*;
use super::{
    capacity::{
        CapacityReleaseOutcome, ExecutionCapacityDimension, prelock_capacity_dimensions_in_tx,
        prelock_existing_capacity_dimensions_in_tx, release_task_capacity_in_tx,
    },
    external_job::{
        ExecutionExternalJobOwner, ExecutionExternalJobRecord, ExecutionExternalJobState,
        NewExecutionExternalJobIntent, load_external_job_for_update_in_conn,
    },
    materialize::DbEstimate,
    outcome::{record_task_outcome_in_conn, record_waiting_external_task_outcome_in_conn},
    outcome_support::*,
    ready::{
        append_run_wait_reason_in_tx, transition_node_counters_in_tx,
        transition_node_counters_with_input_audience_in_tx,
    },
    rows::*,
    run::enqueue_run_activation_in_conn,
    sql::*,
    transition::{refresh_run_after_wait_settlement_in_conn, task_outcome_is_exact_replay},
    trigger::{
        ExecutionTriggerKind, ExecutionTriggerSupersedeOutcome, NewExecutionTrigger,
        create_trigger_with_dispatch_in_conn, supersede_trigger_in_conn,
    },
};

const TASK_INPUT_WAIT_TRIGGER_NAMESPACE: Uuid =
    Uuid::from_u128(0x9a2e_18f4_1c5e_57c4_8bf5_ea73_52e9_4a11);

/// Immutable identity of one admitted bounded task-attempt slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskAttemptFence {
    /// Tenant that owns every referenced row.
    pub tenant_id: TenantId,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable logical task.
    pub task_id: ExecutionTaskId,
    /// Run-controller generation that admitted the slice.
    pub controller_generation: u64,
    /// Exact bounded attempt generation.
    pub attempt_generation: u64,
    /// Immutable durable-dispatch identity.
    pub dispatch_uid: Uuid,
    /// Exact shared-capacity receipt.
    pub capacity_reservation_uid: Uuid,
    /// Exact active-attempt watchdog trigger.
    pub watchdog_trigger_uid: Uuid,
    /// Absolute deadline frozen by admission.
    pub attempt_deadline_at: DateTime<Utc>,
}

/// Generation-fenced transition from durable dispatch to active execution.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskAttemptStartOutcome {
    /// The exact admitted dispatch became active.
    Started(Box<TaskAttemptRecord>),
    /// The same dispatch was already marked active.
    AlreadyStarted(Box<TaskAttemptRecord>),
    /// No exact run or task exists.
    NotFound,
    /// Dispatch, controller, attempt, deadline, or tenant identity is stale.
    Stale,
    /// The run or task is not currently dispatchable.
    InvalidState,
}

/// Authoritative active task slice loaded from its row-locked owning run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TaskAttemptRecord {
    /// Immutable run projection, including admitted identity and parent session.
    pub run: ExecutionRunRecord,
    /// Exact bounded task projection.
    pub task: ExecutionTaskRecord,
}

/// Liveness of one active attempt, observed from its persisted deadline and progress.
///
/// The two failure classes are deliberately distinct. `DeadlineExceeded` means the attempt
/// consumed its whole authorized window; `Stalled` means the attempt is still inside that
/// window but has not committed a durable step within the configured heartbeat interval, so a
/// wedged model call or tool no longer has to burn the full window before it is observable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActiveAttemptLiveness {
    /// The attempt is inside its deadline and reported durable progress recently.
    Live,
    /// The attempt is inside its deadline but has not reported progress within the window.
    Stalled,
    /// The attempt reached the absolute deadline frozen by admission.
    DeadlineExceeded,
}

impl ActiveAttemptLiveness {
    /// Reports whether this observation must terminate the attempt.
    #[must_use]
    pub const fn is_expired(self) -> bool {
        matches!(self, Self::Stalled | Self::DeadlineExceeded)
    }

    /// Stable label for durable messages and telemetry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stalled => "stalled",
            Self::DeadlineExceeded => "deadline_exceeded",
        }
    }
}

/// Returns the staleness window for an attempt whose in-flight step declared `step_bound`.
///
/// Shared by watchdog arming, watchdog deferral, and liveness classification so all three
/// derive the same window from one configured value and one recorded step bound.
///
/// The heartbeat is written at step boundaries and never while a step runs, so the window
/// has to outlast the step in flight or it classifies a working attempt as stalled. A
/// single global window would therefore have to clear the slowest step any attempt can
/// take, which pushes detection for every attempt out to the worst case. Taking the bound
/// from the step actually running keeps the common case tight: a step that declared
/// nothing gets the configured floor, and one that declared a long timeout gets exactly
/// that plus [`ATTEMPT_STEP_BOUND_MARGIN_SECONDS`].
///
/// The floor also applies to declared bounds shorter than it, because scheduling jitter
/// around a two-second step must not read as a stall.
pub fn attempt_heartbeat_staleness_window(
    config: &ExecutionConfig,
    step_bound: Option<chrono::TimeDelta>,
) -> Result<chrono::TimeDelta> {
    let floor = i64::try_from(config.attempt_heartbeat_staleness_seconds)
        .ok()
        .and_then(chrono::TimeDelta::try_seconds)
        .ok_or_else(|| Error::InvalidRepositoryInput {
            message: "attempt heartbeat staleness window exceeds chrono duration".to_string(),
        })?;
    let Some(bound) = step_bound else {
        return Ok(floor);
    };
    let margin =
        chrono::TimeDelta::try_seconds(ATTEMPT_STEP_BOUND_MARGIN_SECONDS).ok_or_else(|| {
            Error::InvalidRepositoryInput {
                message: "attempt step bound margin exceeds chrono duration".to_string(),
            }
        })?;
    let bounded = bound
        .checked_add(&margin)
        .ok_or_else(|| Error::InvalidRepositoryInput {
            message: "attempt step bound plus margin exceeds chrono duration".to_string(),
        })?;
    Ok(floor.max(bounded))
}

/// Grace added to a declared step bound before the step is treated as stalled.
///
/// A step that promised N seconds is not late at exactly N: the promise bounds the work,
/// not the dispatch, result write, and clock skew around it. Without this margin every
/// step that legitimately runs to its own limit races the watchdog.
pub const ATTEMPT_STEP_BOUND_MARGIN_SECONDS: i64 = 30;

/// Classifies one active attempt from its deadline, last durable progress, and step bound.
///
/// The deadline is evaluated first so the pre-existing absolute-deadline behaviour is
/// unchanged; heartbeat staleness only ever classifies an attempt that is still inside its
/// deadline. A staleness window is only meaningful when it is shorter than the attempt
/// timeout, which [`moa_config::ExecutionConfig::validate`] enforces for the floor.
///
/// `step_bound` is the bound recorded for the step in flight, or `None` when the attempt is
/// between steps or running a step that declares no bound.
#[must_use]
pub fn classify_active_attempt_liveness(
    config: &ExecutionConfig,
    attempt_deadline_at: DateTime<Utc>,
    last_progress_at: DateTime<Utc>,
    step_bound: Option<chrono::TimeDelta>,
    observed_at: DateTime<Utc>,
) -> ActiveAttemptLiveness {
    if attempt_deadline_at <= observed_at {
        return ActiveAttemptLiveness::DeadlineExceeded;
    }
    let Ok(staleness) = attempt_heartbeat_staleness_window(config, step_bound) else {
        // An unrepresentable window can never elapse, so the deadline stays the only authority.
        return ActiveAttemptLiveness::Live;
    };
    match last_progress_at.checked_add_signed(staleness) {
        Some(stale_at) if stale_at <= observed_at => ActiveAttemptLiveness::Stalled,
        Some(_) => ActiveAttemptLiveness::Live,
        None => ActiveAttemptLiveness::Live,
    }
}

/// Result of recording durable progress for one active attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TaskAttemptProgressOutcome {
    /// The exact active attempt advanced its progress timestamp.
    Applied,
    /// The supplied timestamp was already durably covered.
    Replayed,
    /// No exact run or task exists.
    NotFound,
    /// The immutable attempt identity no longer matches canonical state.
    Stale,
    /// The task does not currently own active capacity.
    InvalidState,
}

/// Result of claiming exact teardown ownership before provider checkpoint/destroy I/O.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum TaskAttemptReleaseClaimOutcome {
    /// The active attempt entered the non-admissible cancelling phase.
    Applied(Box<TaskAttemptRecord>),
    /// The same exact attempt already owns the cancelling phase.
    Replayed(Box<TaskAttemptRecord>),
    /// No exact run or task exists.
    NotFound,
    /// An immutable generation, dispatch, capacity, or watchdog coordinate is stale.
    Stale,
    /// The task is not currently eligible to relinquish active ownership.
    InvalidState,
}

/// Result of the short, receipt-fenced capacity release before logical outcome settlement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ReleasedTaskAttemptCapacityOutcome {
    /// Capacity and watchdog ownership were released by this call.
    Applied,
    /// The exact capacity and watchdog release had already committed.
    Replayed,
    /// No exact run, task, or capacity receipt exists.
    NotFound,
    /// One immutable attempt or receipt coordinate no longer matches canonical state.
    Stale,
    /// The task is not in its resource-release phase.
    InvalidState,
}

/// Result of atomically settling one bounded task attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskAttemptSettlementOutcome {
    /// The exact active attempt committed its outcome and released capacity.
    Applied {
        /// Run after its controller activation was enqueued.
        run: ExecutionRunRecord,
        /// Logical task after its bounded attempt yielded or settled.
        task: ExecutionTaskRecord,
    },
    /// The exact complete settlement had already committed.
    Replayed {
        /// Current owning run.
        run: ExecutionRunRecord,
        /// Current logical task.
        task: ExecutionTaskRecord,
    },
    /// No exact run, task, or capacity receipt exists.
    NotFound,
    /// One immutable attempt coordinate no longer matches canonical state.
    Stale,
    /// The task or run cannot accept this settlement.
    InvalidState,
}

/// Exact storage disposition for an admitted attempt whose receiver never started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnstartedTaskAttemptDisposition {
    /// A terminal cancellation won before the receiver committed its start fence.
    Cancelled {
        /// Bounded durable cancellation reason.
        reason: String,
    },
    /// A pause fenced the run before the receiver committed its start fence.
    Paused {
        /// Current run-controller generation that owns the pause drain.
        controller_generation: u64,
    },
    /// Durable dispatch was accepted but no receiver start transaction committed.
    DispatchDeliveryLost,
}

/// Result of yielding one attempt to an asynchronous provider job.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskAttemptExternalOutcome {
    /// The exact provider job and storage-only wait committed atomically.
    Applied {
        /// Run after its controller activation was enqueued.
        run: ExecutionRunRecord,
        /// Waiting logical task.
        task: ExecutionTaskRecord,
        /// Durable provider job.
        external_job: ExecutionExternalJobRecord,
    },
    /// The exact external-job yield had already committed.
    Replayed {
        /// Current owning run.
        run: ExecutionRunRecord,
        /// Current waiting task.
        task: ExecutionTaskRecord,
        /// Existing durable provider job.
        external_job: ExecutionExternalJobRecord,
    },
    /// No exact run, task, or capacity receipt exists.
    NotFound,
    /// One immutable attempt coordinate no longer matches canonical state.
    Stale,
    /// The task or run cannot yield to an external job.
    InvalidState,
}

/// Result of settling a terminal provider job into its exact waiting task.
#[derive(Clone, Debug, PartialEq)]
pub enum ExternalJobTaskSettlementOutcome {
    /// The waiting task consumed the terminal provider outcome.
    Applied(ExecutionTaskRecord),
    /// The same terminal provider outcome was already consumed.
    Replayed(ExecutionTaskRecord),
    /// Provider terminal state is durable, but sandbox release still owns task settlement.
    DeferredRelease(ExecutionTaskRecord),
    /// The job no longer owns the current waiting attempt.
    Stale,
    /// No owning task exists.
    NotFound,
}

/// Durable kind of one bounded task continuation checkpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TaskAttemptCheckpointKind {
    /// Task-local agent state at a bounded model/tool boundary.
    AgentContinuation,
    /// Direct capability invocation waiting on an exact action review.
    CapabilityReview,
    /// Direct async-capable invocation persisted before provider start.
    CapabilityExternalStart,
}

impl TaskAttemptCheckpointKind {
    /// Returns the stable PostgreSQL label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentContinuation => "agent_continuation",
            Self::CapabilityReview => "capability_review",
            Self::CapabilityExternalStart => "capability_external_start",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "agent_continuation" => Ok(Self::AgentContinuation),
            "capability_review" => Ok(Self::CapabilityReview),
            "capability_external_start" => Ok(Self::CapabilityExternalStart),
            other => Err(Error::InvalidRepositoryData {
                message: format!("unknown task-attempt checkpoint kind `{other}`"),
            }),
        }
    }
}

/// Immutable bounded continuation written before an attempt relinquishes ownership.
#[derive(Clone, Debug, PartialEq)]
pub struct NewTaskAttemptCheckpoint {
    /// Exact active attempt identity.
    pub fence: TaskAttemptFence,
    /// Exact logical task generation.
    pub task_generation: u64,
    /// Closed continuation kind.
    pub kind: TaskAttemptCheckpointKind,
    /// Typed payload schema version.
    pub schema_version: u32,
    /// Canonical object payload, capped at one MiB.
    pub payload: Value,
    /// Verified sandbox release receipt, when the attempt owned sandbox compute.
    pub workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    /// Durable checkpoint time.
    pub created_at: DateTime<Utc>,
}

/// One immutable persisted task continuation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TaskAttemptCheckpointRecord {
    /// Deterministic checkpoint identity.
    pub checkpoint_uid: Uuid,
    /// Monotonic per-task checkpoint sequence.
    pub checkpoint_sequence: u64,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning run.
    pub run_uid: Uuid,
    /// Stable logical task.
    pub task_id: ExecutionTaskId,
    /// Exact controller generation.
    pub controller_generation: u64,
    /// Exact attempt generation.
    pub attempt_generation: u64,
    /// Immutable active dispatch that produced this checkpoint.
    pub dispatch_uid: Uuid,
    /// Exact logical task generation.
    pub task_generation: u64,
    /// Closed continuation kind.
    pub kind: TaskAttemptCheckpointKind,
    /// Typed payload schema version.
    pub schema_version: u32,
    /// Canonical bounded payload.
    pub payload: Value,
    /// Canonical BLAKE3 digest of the payload.
    pub payload_hash: String,
    /// Verified sandbox release receipt.
    pub workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    /// Durable creation time.
    pub created_at: DateTime<Utc>,
}

/// Result of generation-fenced checkpoint persistence.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskAttemptCheckpointWriteOutcome {
    /// A new immutable checkpoint superseded the prior current row.
    Applied(Box<TaskAttemptCheckpointRecord>),
    /// The exact checkpoint was already current.
    Replayed(Box<TaskAttemptCheckpointRecord>),
    /// No exact task exists.
    NotFound,
    /// The task generation, attempt, dispatch, or receipt is stale.
    Stale,
    /// The payload or task state cannot be checkpointed.
    InvalidState,
}

/// Recovery result for an async provider start that did not create provider-owned work.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskExternalStartRetryOutcome {
    /// Active ownership was released and the exact provisional continuation became ready.
    Applied {
        /// Ready task on the successor bounded-attempt generation.
        task: ExecutionTaskRecord,
        /// Current predecessor checkpoint consumed by that successor.
        checkpoint: Box<TaskAttemptCheckpointRecord>,
    },
    /// The same recovery transition was already committed.
    Replayed {
        /// Current ready task.
        task: ExecutionTaskRecord,
        /// Exact preserved provisional checkpoint.
        checkpoint: Box<TaskAttemptCheckpointRecord>,
    },
    /// No exact active task or capacity owner exists.
    NotFound,
    /// Task, attempt, dispatch, checkpoint, or intent identity is obsolete.
    Stale,
    /// The current owner is not a running provisional external start.
    InvalidState,
}

/// Result of checkpointing one bounded agent slice for immediate redispatch.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskAttemptContinuationYieldOutcome {
    /// The exact continuation was persisted and the task became ready.
    Applied {
        /// Ready logical task with an advanced attempt generation.
        task: ExecutionTaskRecord,
        /// Immutable checkpoint consumed by the next admitted slice.
        checkpoint: Box<TaskAttemptCheckpointRecord>,
    },
    /// The same continuation yield already committed.
    Replayed {
        /// Current logical task.
        task: ExecutionTaskRecord,
        /// Current immutable checkpoint.
        checkpoint: Box<TaskAttemptCheckpointRecord>,
    },
    /// No exact task or capacity receipt exists.
    NotFound,
    /// An immutable task, attempt, dispatch, or checkpoint coordinate is stale.
    Stale,
    /// The task cannot yield a continuation from its current state.
    InvalidState,
}

/// Result of atomically parking one reviewed effect in storage.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskAttemptReviewParkOutcome {
    /// Capacity/watchdog ownership moved to an immutable continuation.
    Applied {
        /// Waiting logical task.
        task: ExecutionTaskRecord,
        /// Current bounded continuation.
        checkpoint: Box<TaskAttemptCheckpointRecord>,
    },
    /// The exact review park had already committed.
    Replayed {
        /// Current waiting task.
        task: ExecutionTaskRecord,
        /// Existing bounded continuation.
        checkpoint: Box<TaskAttemptCheckpointRecord>,
    },
    /// No exact task, capacity receipt, or checkpoint exists.
    NotFound,
    /// An immutable generation, dispatch, watchdog, or review identity is stale.
    Stale,
    /// The task cannot enter a review wait from its current state.
    InvalidState,
}

/// Result of consuming one storage-only action-review resolution.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum TaskAttemptReviewResolutionOutcome {
    /// The exact resolution was checkpointed and the task became Ready.
    Applied {
        /// Ready task projection.
        task: ExecutionTaskRecord,
        /// Immutable resolved continuation.
        checkpoint: Box<TaskAttemptCheckpointRecord>,
    },
    /// The same resolution had already been consumed.
    Replayed {
        /// Current task projection.
        task: ExecutionTaskRecord,
        /// Current resolved continuation.
        checkpoint: Box<TaskAttemptCheckpointRecord>,
    },
    /// The review decision arrived before the attempt completed its durable park.
    NotReady,
    /// No exact task or checkpoint exists.
    NotFound,
    /// The logical generation or review identity is obsolete.
    Stale,
}

/// Exact storage request for consuming one reviewed task-attempt effect.
#[derive(Clone, Debug)]
pub struct ResolveTaskAttemptReviewRequest {
    /// Repository visibility boundary for the mutation.
    pub scope: ExecutionScope,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Stable logical task identity.
    pub task_id: ExecutionTaskId,
    /// Exact logical task generation that parked the review.
    pub expected_task_generation: u64,
    /// Stable review identity persisted in the current checkpoint.
    pub review_uid: Uuid,
    /// Durable reviewed-effect resolution.
    pub resolution: ExecutionActionReviewResolution,
    /// Timestamp at which the resolution was accepted.
    pub resolved_at: DateTime<Utc>,
}

struct SettleTaskAttemptRequest<'a> {
    config: &'a ExecutionConfig,
    fence: TaskAttemptFence,
    outcome: ExecutionTaskOutcome,
    retry_at: Option<DateTime<Utc>>,
    settled_at: DateTime<Utc>,
    expected_attempt_state: ExecutionAttemptState,
    workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    continuation_checkpoint: Option<NewTaskAttemptCheckpoint>,
}

struct ResumeTaskRequest<'a> {
    scope: ExecutionScope,
    config: Option<&'a ExecutionConfig>,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
    generation: u64,
    kind: ResumeKind,
    resume_input: Option<Value>,
}

impl ExecutionRepository {
    /// Loads the current bounded continuation for one visible logical task.
    pub async fn load_task_attempt_checkpoint(
        &self,
        scope: ExecutionScope,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
    ) -> Result<Option<TaskAttemptCheckpointRecord>> {
        let mut conn = scope.begin(&self.pool).await?;
        let row = sqlx::query(
            "SELECT * FROM moa.execution_task_checkpoint \
             WHERE run_uid = $1 AND task_id = $2 AND superseded_at IS NULL",
        )
        .bind(run_uid)
        .bind(task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let checkpoint = row.as_ref().map(task_checkpoint_from_row).transpose()?;
        conn.commit().await.map_err(storage_error)?;
        Ok(checkpoint)
    }

    /// Persists the exact async-provider start continuation while its attempt remains active.
    ///
    /// This checkpoint is the replay authority if provider start recovery later proves that no
    /// asynchronous work began. It never releases capacity, clears the watchdog, or changes task
    /// lifecycle state.
    pub async fn persist_running_task_external_start_checkpoint(
        &self,
        checkpoint: NewTaskAttemptCheckpoint,
    ) -> Result<TaskAttemptCheckpointWriteOutcome> {
        if checkpoint.workspace_release_receipt.is_some()
            || !external_start_checkpoint_payload_is_provisional(
                checkpoint.kind,
                &checkpoint.payload,
            )
        {
            return Ok(TaskAttemptCheckpointWriteOutcome::InvalidState);
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let outcome = persist_task_attempt_checkpoint_for_state_in_conn(
            &mut conn,
            &checkpoint,
            ExecutionAttemptState::Running,
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Consumes one exact reviewed effect without resolving a workflow promise.
    pub async fn resolve_task_attempt_review(
        &self,
        config: &ExecutionConfig,
        request: ResolveTaskAttemptReviewRequest,
    ) -> Result<TaskAttemptReviewResolutionOutcome> {
        let ResolveTaskAttemptReviewRequest {
            scope,
            run_uid,
            task_id,
            expected_task_generation,
            review_uid,
            resolution,
            resolved_at,
        } = request;
        if !matches!(scope, ExecutionScope::ControlPlane)
            || expected_task_generation == 0
            || review_uid.is_nil()
        {
            return Err(Error::InvalidRepositoryInput {
                message: "task review resolution requires control-plane scope and exact identity"
                    .to_string(),
            });
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let tenant_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_run WHERE run_uid=$1",
        )
        .bind(run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(tenant_id) = tenant_id else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewResolutionOutcome::NotFound);
        };
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            TenantId(tenant_id),
            &[
                ExecutionCapacityDimension::ActiveRuns,
                ExecutionCapacityDimension::ParkedRuns,
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
            return Ok(TaskAttemptReviewResolutionOutcome::NotFound);
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
            return Ok(TaskAttemptReviewResolutionOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        let checkpoint_row = sqlx::query(
            "SELECT * FROM moa.execution_task_checkpoint WHERE tenant_id=$1 AND run_uid=$2 \
             AND task_id=$3 AND superseded_at IS NULL FOR UPDATE",
        )
        .bind(run.tenant_id.0)
        .bind(run_uid)
        .bind(task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(checkpoint_row) = checkpoint_row else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(if task.status == ExecutionTaskStatus::Running {
                TaskAttemptReviewResolutionOutcome::NotReady
            } else {
                TaskAttemptReviewResolutionOutcome::NotFound
            });
        };
        let checkpoint = task_checkpoint_from_row(&checkpoint_row)?;
        if task.generation != expected_task_generation
            || checkpoint.task_generation != expected_task_generation
            || checkpoint_review_uid(&checkpoint.payload) != Some(review_uid)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewResolutionOutcome::Stale);
        }
        let resolution_value = serde_json::to_value(&resolution)?;
        if task.status == ExecutionTaskStatus::Ready
            && checkpoint_review_resolution(&checkpoint.payload) == Some(&resolution_value)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewResolutionOutcome::Replayed {
                task,
                checkpoint: Box::new(checkpoint),
            });
        }
        if task.status == ExecutionTaskStatus::Running {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewResolutionOutcome::NotReady);
        }
        if task.status != ExecutionTaskStatus::WaitingReview
            || task.attempt_state != ExecutionAttemptState::Waiting
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewResolutionOutcome::Stale);
        }
        let mut payload = checkpoint.payload.clone();
        let Some(object) = payload.as_object_mut() else {
            return Err(Error::InvalidRepositoryData {
                message: "task review checkpoint payload is not an object".to_string(),
            });
        };
        object.insert("review_resolution".to_string(), resolution_value);
        let resolved_checkpoint = insert_resolved_task_checkpoint_in_conn(
            &mut conn,
            &checkpoint,
            run.controller_generation,
            checkpoint.task_generation,
            checkpoint.attempt_generation,
            payload,
            resolved_at,
        )
        .await?;
        let next_attempt_generation = task.attempt_generation.checked_add(1).ok_or_else(|| {
            Error::InvalidRepositoryInput {
                message: "task attempt generation overflow".to_string(),
            }
        })?;
        let row = sqlx::query(
            "UPDATE moa.execution_task SET status='ready', attempt_state='idle', \
             attempt_generation=$5, waiting_since=NULL, ready_at=$6, \
             progress_step_bound_seconds=NULL, \
             last_progress_at=GREATEST(last_progress_at,$6), \
             generation_history=generation_history || jsonb_build_array(jsonb_build_object( \
                'kind','action_review_resolved','review_uid',$4::TEXT,'recorded_at',$6)), \
             updated_at=NOW() WHERE run_uid=$1 AND task_id=$2 AND generation=$3 \
               AND status='waiting_review' RETURNING *",
        )
        .bind(run_uid)
        .bind(task_id.as_uuid())
        .bind(to_i64(expected_task_generation, "task generation")?)
        .bind(review_uid)
        .bind(to_i64(next_attempt_generation, "next attempt generation")?)
        .bind(resolved_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewResolutionOutcome::Stale);
        };
        let task = task_from_row(&row)?;
        transition_node_counters_in_tx(
            &mut conn,
            run_uid,
            &task.node_id,
            &task.item_key,
            ExecutionTaskStatus::WaitingReview,
            ExecutionTaskStatus::Ready,
        )
        .await?;
        refresh_run_after_wait_settlement_in_conn(&mut conn, run_uid, task_id, resolved_at).await?;
        if !matches!(
            run.status,
            ExecutionRunStatus::PauseRequested
                | ExecutionRunStatus::Pausing
                | ExecutionRunStatus::Paused
        ) {
            enqueue_run_activation_in_conn(
                conn.as_mut(),
                run.tenant_id,
                run_uid,
                run.controller_generation,
                resolved_at,
                json!({
                    "source": "task_action_review_resolution",
                    "task_id": task_id,
                    "review_uid": review_uid,
                }),
            )
            .await?;
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptReviewResolutionOutcome::Applied {
            task,
            checkpoint: Box::new(resolved_checkpoint),
        })
    }

    /// Persists an exact provider job and fences its task before sandbox teardown begins.
    pub async fn begin_task_attempt_external_release(
        &self,
        fence: TaskAttemptFence,
        expected_task_generation: u64,
        external_job_uid: Uuid,
        claimed_at: DateTime<Utc>,
    ) -> Result<TaskAttemptReleaseClaimOutcome> {
        if expected_task_generation == 0 {
            return Err(Error::InvalidRepositoryInput {
                message: "external release must name the exact active task attempt".to_string(),
            });
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some(persisted_job) =
            load_external_job_for_update_in_conn(conn.as_mut(), external_job_uid).await?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::NotFound);
        };
        if persisted_job.tenant_id != fence.tenant_id
            || persisted_job.run_uid != fence.run_uid
            || persisted_job.owner
                != (ExecutionExternalJobOwner::Task {
                    task_id: fence.task_id.as_uuid(),
                    attempt_generation: fence.attempt_generation,
                })
            || persisted_job.state == ExecutionExternalJobState::Unbound
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::Stale);
        }
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if !task_attempt_fence_matches(&run, &task, &fence)
            || task.generation != expected_task_generation
            || !task_attempt_resources_match(&mut conn, &fence).await?
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::Stale);
        }
        if task.status == ExecutionTaskStatus::Running
            && task.attempt_state == ExecutionAttemptState::Cancelling
            && task.external_job_uid == Some(external_job_uid)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::Replayed(Box::new(
                TaskAttemptRecord { run, task },
            )));
        }
        if task.status != ExecutionTaskStatus::Running
            || task.attempt_state != ExecutionAttemptState::Running
            || task.external_job_uid.is_some()
            || run.status.is_terminal()
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::InvalidState);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_task SET attempt_state='cancelling', external_job_uid=$5, \
             progress_step_bound_seconds=NULL, \
             last_progress_at=GREATEST(last_progress_at,$6), \
             generation_history=generation_history || \
             jsonb_build_array(jsonb_build_object( \
                 'kind','external_job_release_claimed','dispatch_uid',$4::TEXT, \
                 'attempt_generation',$3,'external_job_uid',$5::TEXT,'recorded_at',$6)), \
             updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
               AND active_dispatch_uid=$4 AND attempt_state='running' \
               AND external_job_uid IS NULL RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(external_job_uid)
        .bind(claimed_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::Stale);
        };
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptReleaseClaimOutcome::Applied(Box::new(
            TaskAttemptRecord { run, task },
        )))
    }

    /// Claims one exact active attempt for checkpoint-and-release before provider I/O.
    pub async fn begin_task_attempt_release(
        &self,
        fence: TaskAttemptFence,
        expected_task_generation: u64,
        reason: &str,
        claimed_at: DateTime<Utc>,
    ) -> Result<TaskAttemptReleaseClaimOutcome> {
        if expected_task_generation == 0 || reason.trim().is_empty() {
            return Err(Error::InvalidRepositoryInput {
                message: "task-attempt release requires a generation and reason".to_string(),
            });
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if !task_attempt_fence_matches(&run, &task, &fence)
            || task.generation != expected_task_generation
            || !task_attempt_resources_match(&mut conn, &fence).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::Stale);
        }
        if task.status == ExecutionTaskStatus::Running
            && task.attempt_state == ExecutionAttemptState::Cancelling
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::Replayed(Box::new(
                TaskAttemptRecord { run, task },
            )));
        }
        if task.status != ExecutionTaskStatus::Running
            || task.attempt_state != ExecutionAttemptState::Running
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::InvalidState);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_task SET attempt_state='cancelling', \
             progress_step_bound_seconds=NULL, \
             last_progress_at=GREATEST(last_progress_at,$5), \
             generation_history=generation_history || jsonb_build_array(jsonb_build_object( \
                 'kind','bounded_attempt_release_claimed','dispatch_uid',$4::TEXT, \
                 'attempt_generation',$3,'reason',$6,'recorded_at',$5)), updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
               AND active_dispatch_uid=$4 AND attempt_state='running' RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(claimed_at)
        .bind(reason)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReleaseClaimOutcome::Stale);
        };
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptReleaseClaimOutcome::Applied(Box::new(
            TaskAttemptRecord { run, task },
        )))
    }

    /// Persists one reviewed continuation and releases active capacity atomically.
    pub async fn park_task_attempt_on_review(
        &self,
        checkpoint: NewTaskAttemptCheckpoint,
        review_uid: Uuid,
    ) -> Result<TaskAttemptReviewParkOutcome> {
        if review_uid.is_nil() || checkpoint_review_uid(&checkpoint.payload) != Some(review_uid) {
            return Ok(TaskAttemptReviewParkOutcome::InvalidState);
        }
        let fence = checkpoint.fence;
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let capacity = release_task_capacity_in_tx(
            &mut conn,
            fence.capacity_reservation_uid,
            fence.run_uid,
            fence.task_id,
            fence.attempt_generation,
        )
        .await?;
        if capacity == CapacityReleaseOutcome::NotFound {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewParkOutcome::NotFound);
        }
        if capacity == CapacityReleaseOutcome::Stale {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewParkOutcome::Stale);
        }
        if capacity == CapacityReleaseOutcome::AlreadyReleased {
            let task_row = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
                .bind(fence.run_uid)
                .bind(fence.task_id.as_uuid())
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let current_checkpoint = sqlx::query(
                "SELECT * FROM moa.execution_task_checkpoint WHERE tenant_id=$1 AND run_uid=$2 \
                 AND task_id=$3 AND superseded_at IS NULL FOR UPDATE",
            )
            .bind(fence.tenant_id.0)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let Some(task_row) = task_row else {
                conn.commit().await.map_err(storage_error)?;
                return Ok(TaskAttemptReviewParkOutcome::NotFound);
            };
            let Some(current_checkpoint) = current_checkpoint else {
                conn.commit().await.map_err(storage_error)?;
                return Ok(TaskAttemptReviewParkOutcome::NotFound);
            };
            let task = task_from_row(&task_row)?;
            let current_checkpoint = task_checkpoint_from_row(&current_checkpoint)?;
            let replay = task.status == ExecutionTaskStatus::WaitingReview
                && task.attempt_generation == fence.attempt_generation
                && task.active_dispatch_uid.is_none()
                && current_checkpoint.dispatch_uid == fence.dispatch_uid
                && checkpoint_review_uid(&current_checkpoint.payload) == Some(review_uid);
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                TaskAttemptReviewParkOutcome::Replayed {
                    task,
                    checkpoint: Box::new(current_checkpoint),
                }
            } else {
                TaskAttemptReviewParkOutcome::Stale
            });
        }
        if let Some(receipt) = checkpoint.workspace_release_receipt.as_ref()
            && !persisted_task_release_receipt_matches(
                &mut conn,
                &fence,
                checkpoint.task_generation,
                receipt,
            )
            .await?
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewParkOutcome::Stale);
        }

        let persisted =
            match persist_task_attempt_checkpoint_in_conn(&mut conn, &checkpoint).await? {
                TaskAttemptCheckpointWriteOutcome::Applied(checkpoint)
                | TaskAttemptCheckpointWriteOutcome::Replayed(checkpoint) => checkpoint,
                TaskAttemptCheckpointWriteOutcome::NotFound => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptReviewParkOutcome::NotFound);
                }
                TaskAttemptCheckpointWriteOutcome::Stale => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptReviewParkOutcome::Stale);
                }
                TaskAttemptCheckpointWriteOutcome::InvalidState => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptReviewParkOutcome::InvalidState);
                }
            };
        if supersede_trigger_in_conn(
            conn.as_mut(),
            fence.watchdog_trigger_uid,
            ExecutionTriggerKind::TaskWatchdog,
            Some(fence.controller_generation),
            Some(fence.attempt_generation),
            None,
            None,
        )
        .await?
            == ExecutionTriggerSupersedeOutcome::StaleOrMissing
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewParkOutcome::Stale);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_task SET status='waiting_review', attempt_state='waiting', \
             waiting_since=$5, active_dispatch_uid=NULL, attempt_deadline_at=NULL, \
             progress_step_bound_seconds=NULL, \
             last_progress_at=GREATEST(last_progress_at,$5), updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
               AND active_dispatch_uid=$4 AND status='running' RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(checkpoint.created_at)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptReviewParkOutcome::Stale);
        };
        let task = task_from_row(&row)?;
        transition_node_counters_in_tx(
            &mut conn,
            fence.run_uid,
            &task.node_id,
            &task.item_key,
            ExecutionTaskStatus::Running,
            ExecutionTaskStatus::WaitingReview,
        )
        .await?;
        let review_reason = checkpoint_review_waiting_reason(&persisted, &task)?;
        append_run_wait_reason_in_tx(
            &mut conn,
            fence.run_uid,
            &review_reason,
            checkpoint.created_at,
        )
        .await?;
        enqueue_run_activation_in_conn(
            conn.as_mut(),
            fence.tenant_id,
            fence.run_uid,
            fence.controller_generation,
            checkpoint.created_at,
            json!({
                "source": "task_attempt_review_park",
                "task_id": fence.task_id,
                "dispatch_uid": fence.dispatch_uid,
                "attempt_generation": fence.attempt_generation,
            }),
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptReviewParkOutcome::Applied {
            task,
            checkpoint: persisted,
        })
    }

    /// Checkpoints one bounded agent slice and atomically returns the task to ready storage.
    pub async fn yield_task_attempt_continuation(
        &self,
        checkpoint: NewTaskAttemptCheckpoint,
    ) -> Result<TaskAttemptContinuationYieldOutcome> {
        let fence = checkpoint.fence;
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let capacity = release_task_capacity_in_tx(
            &mut conn,
            fence.capacity_reservation_uid,
            fence.run_uid,
            fence.task_id,
            fence.attempt_generation,
        )
        .await?;
        if capacity == CapacityReleaseOutcome::NotFound {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptContinuationYieldOutcome::NotFound);
        }
        if capacity == CapacityReleaseOutcome::Stale {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptContinuationYieldOutcome::Stale);
        }
        if capacity == CapacityReleaseOutcome::AlreadyReleased {
            let task_row = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
                .bind(fence.run_uid)
                .bind(fence.task_id.as_uuid())
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
            let checkpoint_row = sqlx::query(
                "SELECT * FROM moa.execution_task_checkpoint WHERE tenant_id=$1 AND run_uid=$2 \
                 AND task_id=$3 AND superseded_at IS NULL FOR UPDATE",
            )
            .bind(fence.tenant_id.0)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let (Some(task_row), Some(checkpoint_row)) = (task_row, checkpoint_row) else {
                conn.commit().await.map_err(storage_error)?;
                return Ok(TaskAttemptContinuationYieldOutcome::NotFound);
            };
            let task = task_from_row(&task_row)?;
            let persisted = task_checkpoint_from_row(&checkpoint_row)?;
            let replay = task.status == ExecutionTaskStatus::Ready
                && task.attempt_state == ExecutionAttemptState::Idle
                && task.active_dispatch_uid.is_none()
                && persisted.dispatch_uid == fence.dispatch_uid;
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                TaskAttemptContinuationYieldOutcome::Replayed {
                    task,
                    checkpoint: Box::new(persisted),
                }
            } else {
                TaskAttemptContinuationYieldOutcome::Stale
            });
        }
        if let Some(receipt) = checkpoint.workspace_release_receipt.as_ref()
            && !persisted_task_release_receipt_matches(
                &mut conn,
                &fence,
                checkpoint.task_generation,
                receipt,
            )
            .await?
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptContinuationYieldOutcome::Stale);
        }
        let persisted =
            match persist_task_attempt_checkpoint_in_conn(&mut conn, &checkpoint).await? {
                TaskAttemptCheckpointWriteOutcome::Applied(checkpoint)
                | TaskAttemptCheckpointWriteOutcome::Replayed(checkpoint) => checkpoint,
                TaskAttemptCheckpointWriteOutcome::NotFound => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptContinuationYieldOutcome::NotFound);
                }
                TaskAttemptCheckpointWriteOutcome::Stale => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptContinuationYieldOutcome::Stale);
                }
                TaskAttemptCheckpointWriteOutcome::InvalidState => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptContinuationYieldOutcome::InvalidState);
                }
            };
        if supersede_trigger_in_conn(
            conn.as_mut(),
            fence.watchdog_trigger_uid,
            ExecutionTriggerKind::TaskWatchdog,
            Some(fence.controller_generation),
            Some(fence.attempt_generation),
            None,
            None,
        )
        .await?
            == ExecutionTriggerSupersedeOutcome::StaleOrMissing
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptContinuationYieldOutcome::Stale);
        }
        let next_attempt_generation = fence.attempt_generation.checked_add(1).ok_or_else(|| {
            Error::InvalidRepositoryData {
                message: "task attempt generation overflow".to_string(),
            }
        })?;
        let row = sqlx::query(
            "UPDATE moa.execution_task SET status='ready', attempt_state='idle', ready_at=$5, \
             waiting_since=NULL, active_dispatch_uid=NULL, attempt_deadline_at=NULL, \
             progress_step_bound_seconds=NULL, \
             attempt_generation=$6, last_progress_at=GREATEST(last_progress_at,$5), \
             updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
               AND active_dispatch_uid=$4 AND status='running' AND attempt_state='cancelling' \
             RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(checkpoint.created_at)
        .bind(to_i64(next_attempt_generation, "next attempt generation")?)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptContinuationYieldOutcome::Stale);
        };
        let task = task_from_row(&row)?;
        transition_node_counters_in_tx(
            &mut conn,
            fence.run_uid,
            &task.node_id,
            &task.item_key,
            ExecutionTaskStatus::Running,
            ExecutionTaskStatus::Ready,
        )
        .await?;
        enqueue_run_activation_in_conn(
            conn.as_mut(),
            fence.tenant_id,
            fence.run_uid,
            fence.controller_generation,
            checkpoint.created_at,
            json!({
                "source": "task_attempt_continuation",
                "task_id": fence.task_id,
                "dispatch_uid": fence.dispatch_uid,
                "attempt_generation": fence.attempt_generation,
            }),
        )
        .await?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptContinuationYieldOutcome::Applied {
            task,
            checkpoint: persisted,
        })
    }

    /// Starts one exact admitted task attempt in a control-plane transaction.
    pub async fn start_task_attempt(
        &self,
        fence: TaskAttemptFence,
    ) -> Result<TaskAttemptStartOutcome> {
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptStartOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptStartOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if !task_attempt_fence_matches(&run, &task, &fence)
            || !task_attempt_resources_match(&mut conn, &fence).await?
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptStartOutcome::Stale);
        }
        if task.status == ExecutionTaskStatus::Running
            && task.attempt_state == ExecutionAttemptState::Running
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptStartOutcome::AlreadyStarted(Box::new(
                TaskAttemptRecord { run, task },
            )));
        }
        if task.status != ExecutionTaskStatus::Dispatching
            || task.attempt_state != ExecutionAttemptState::Dispatching
            || !matches!(
                run.status,
                ExecutionRunStatus::Queued | ExecutionRunStatus::Running
            )
            || run.pending_terminal.is_some()
            || storage_only_task_kind(&task.kind)
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptStartOutcome::InvalidState);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_task \
             SET status = 'running', attempt_state = 'running', \
                 attempt_started_at = COALESCE(attempt_started_at, NOW()), \
                 started_at = COALESCE(started_at, NOW()), last_progress_at = NOW(), \
                 progress_step_bound_seconds = NULL, \
                 updated_at = NOW() \
             WHERE run_uid = $1 AND task_id = $2 AND status = 'dispatching' \
               AND attempt_generation = $3 AND active_dispatch_uid = $4 \
             RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        sqlx::query(
            "UPDATE moa.execution_run \
             SET status = CASE WHEN status = 'queued' THEN 'running' ELSE status END, \
                 started_at = COALESCE(started_at, NOW()), last_progress_at = NOW(), \
                 updated_at = NOW() WHERE run_uid = $1",
        )
        .bind(fence.run_uid)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptStartOutcome::Started(Box::new(
            TaskAttemptRecord { run, task },
        )))
    }

    /// Releases and settles an exact dispatch whose receiver never committed its start fence.
    pub async fn settle_unstarted_task_attempt(
        &self,
        fence: TaskAttemptFence,
        disposition: UnstartedTaskAttemptDisposition,
        settled_at: DateTime<Utc>,
    ) -> Result<TaskAttemptSettlementOutcome> {
        if let UnstartedTaskAttemptDisposition::Cancelled { reason } = &disposition
            && (reason.trim().is_empty() || reason.chars().count() > 1_024)
        {
            return Err(Error::InvalidRepositoryInput {
                message: "unstarted task cancellation reason must contain 1..=1024 characters"
                    .to_string(),
            });
        }
        if matches!(
            &disposition,
            UnstartedTaskAttemptDisposition::Paused {
                controller_generation: 0
            }
        ) {
            return Err(Error::InvalidRepositoryInput {
                message: "unstarted task pause controller generation must be positive".to_string(),
            });
        }
        if disposition == UnstartedTaskAttemptDisposition::DispatchDeliveryLost {
            let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
            let outcome =
                settle_unstarted_task_attempt_in_conn(&mut conn, fence, settled_at).await?;
            if matches!(
                outcome,
                TaskAttemptSettlementOutcome::Applied { .. }
                    | TaskAttemptSettlementOutcome::Replayed { .. }
            ) {
                conn.commit().await.map_err(storage_error)?;
            } else {
                conn.rollback().await.map_err(storage_error)?;
            }
            return Ok(outcome);
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let capacity = release_task_capacity_in_tx(
            &mut conn,
            fence.capacity_reservation_uid,
            fence.run_uid,
            fence.task_id,
            fence.attempt_generation,
        )
        .await?;
        if capacity == CapacityReleaseOutcome::NotFound {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        }
        if capacity == CapacityReleaseOutcome::Stale {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if capacity == CapacityReleaseOutcome::AlreadyReleased {
            let replay = unstarted_task_attempt_run_fence_matches(&run, &fence, &disposition)
                && unstarted_task_attempt_settlement_replayed(&task, &fence, &disposition);
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                TaskAttemptSettlementOutcome::Replayed { run, task }
            } else {
                TaskAttemptSettlementOutcome::Stale
            });
        }
        if !unstarted_task_attempt_fence_matches(&run, &task, &fence, &disposition)
            || run.status.is_terminal()
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        if matches!(&disposition, UnstartedTaskAttemptDisposition::Paused { .. })
            && !matches!(
                run.status,
                ExecutionRunStatus::PauseRequested
                    | ExecutionRunStatus::Pausing
                    | ExecutionRunStatus::Paused
            )
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::InvalidState);
        }
        let expected_attempt_state = match &disposition {
            UnstartedTaskAttemptDisposition::Cancelled { .. }
            | UnstartedTaskAttemptDisposition::Paused { .. } => ExecutionAttemptState::Cancelling,
            UnstartedTaskAttemptDisposition::DispatchDeliveryLost => {
                ExecutionAttemptState::Dispatching
            }
        };
        if task.status != ExecutionTaskStatus::Dispatching
            || task.attempt_state != expected_attempt_state
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::InvalidState);
        }
        if supersede_trigger_in_conn(
            conn.as_mut(),
            fence.watchdog_trigger_uid,
            ExecutionTriggerKind::TaskWatchdog,
            Some(fence.controller_generation),
            Some(fence.attempt_generation),
            None,
            None,
        )
        .await?
            == ExecutionTriggerSupersedeOutcome::StaleOrMissing
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        let history = json!({
            "kind": "unstarted_attempt_settlement",
            "dispatch_uid": fence.dispatch_uid,
            "attempt_generation": fence.attempt_generation,
            "disposition": unstarted_disposition_label(&disposition),
            "reason": match &disposition {
                UnstartedTaskAttemptDisposition::Cancelled { reason } => Some(reason.as_str()),
                UnstartedTaskAttemptDisposition::Paused { .. }
                | UnstartedTaskAttemptDisposition::DispatchDeliveryLost => None,
            },
            "controller_generation": match &disposition {
                UnstartedTaskAttemptDisposition::Paused { controller_generation } => {
                    Some(*controller_generation)
                }
                UnstartedTaskAttemptDisposition::Cancelled { .. }
                | UnstartedTaskAttemptDisposition::DispatchDeliveryLost => None,
            },
            "recorded_at": settled_at,
        });
        let task = match &disposition {
            UnstartedTaskAttemptDisposition::Cancelled { reason } => {
                let outcome = cancelled_task_outcome(reason.clone(), task.actual.clone());
                let accepted = match record_task_outcome_in_conn(
                    &mut conn,
                    fence.run_uid,
                    fence.task_id,
                    task.generation,
                    outcome,
                )
                .await?
                {
                    TaskOutcomeWrite::Applied { task, .. } => task,
                    TaskOutcomeWrite::Replayed { run, task, .. } => {
                        conn.rollback().await.map_err(storage_error)?;
                        return Ok(TaskAttemptSettlementOutcome::Replayed { run, task });
                    }
                    TaskOutcomeWrite::NotFound => {
                        conn.rollback().await.map_err(storage_error)?;
                        return Ok(TaskAttemptSettlementOutcome::NotFound);
                    }
                    TaskOutcomeWrite::Rejected { .. } => {
                        conn.rollback().await.map_err(storage_error)?;
                        return Ok(TaskAttemptSettlementOutcome::InvalidState);
                    }
                };
                let row = sqlx::query(
                    "UPDATE moa.execution_task SET active_dispatch_uid=NULL, \
                         attempt_deadline_at=NULL, progress_step_bound_seconds=NULL, \
                         generation_history=generation_history || \
                         jsonb_build_array($5::JSONB), \
                         last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
                     WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
                       AND active_dispatch_uid=$4 AND status='cancelled' RETURNING *",
                )
                .bind(fence.run_uid)
                .bind(fence.task_id.as_uuid())
                .bind(to_i64(fence.attempt_generation, "attempt generation")?)
                .bind(fence.dispatch_uid)
                .bind(&history)
                .bind(settled_at)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
                let Some(row) = row else {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptSettlementOutcome::Stale);
                };
                let task = task_from_row(&row)?;
                debug_assert_eq!(task.task_id, accepted.task_id);
                task
            }
            UnstartedTaskAttemptDisposition::Paused { .. }
            | UnstartedTaskAttemptDisposition::DispatchDeliveryLost => {
                let next_attempt_generation =
                    fence.attempt_generation.checked_add(1).ok_or_else(|| {
                        Error::InvalidRepositoryData {
                            message: "task attempt generation overflow".to_string(),
                        }
                    })?;
                let expected_attempt_state =
                    if matches!(&disposition, UnstartedTaskAttemptDisposition::Paused { .. }) {
                        "cancelling"
                    } else {
                        "dispatching"
                    };
                let row = sqlx::query(
                    "UPDATE moa.execution_task SET status='ready', attempt_state='idle', \
                         attempt_generation=$5, active_dispatch_uid=NULL, \
                         attempt_deadline_at=NULL, ready_at=$6, waiting_since=NULL, \
                         progress_step_bound_seconds=NULL, \
                         generation_history=generation_history || jsonb_build_array($7::JSONB), \
                         last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
                     WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
                       AND active_dispatch_uid=$4 AND status='dispatching' \
                       AND attempt_state=$8 RETURNING *",
                )
                .bind(fence.run_uid)
                .bind(fence.task_id.as_uuid())
                .bind(to_i64(fence.attempt_generation, "attempt generation")?)
                .bind(fence.dispatch_uid)
                .bind(to_i64(next_attempt_generation, "next attempt generation")?)
                .bind(settled_at)
                .bind(&history)
                .bind(expected_attempt_state)
                .fetch_optional(conn.as_mut())
                .await
                .map_err(sqlx_error)?;
                let Some(row) = row else {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptSettlementOutcome::Stale);
                };
                task_from_row(&row)?
            }
        };
        transition_node_counters_in_tx(
            &mut conn,
            fence.run_uid,
            &task.node_id,
            &task.item_key,
            ExecutionTaskStatus::Dispatching,
            task.status,
        )
        .await?;
        if !matches!(&disposition, UnstartedTaskAttemptDisposition::Paused { .. }) {
            enqueue_run_activation_in_conn(
                conn.as_mut(),
                fence.tenant_id,
                fence.run_uid,
                fence.controller_generation,
                settled_at,
                json!({
                    "source": "unstarted_task_attempt_settlement",
                    "task_id": fence.task_id,
                    "dispatch_uid": fence.dispatch_uid,
                    "attempt_generation": fence.attempt_generation,
                    "disposition": unstarted_disposition_label(&disposition),
                }),
            )
            .await?;
        }
        let run_row = sqlx::query(LOAD_RUN_SQL)
            .bind(fence.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&run_row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptSettlementOutcome::Applied { run, task })
    }

    /// Records monotonic progress for one exact active task attempt.
    ///
    /// `step_bound_seconds` is the upper bound declared by the step the attempt is about to
    /// enter, or `None` when the attempt is between steps. The watchdog widens its staleness
    /// window to that bound, so a heartbeat written before a long declared step is what keeps
    /// the step from being classified as stalled while it legitimately runs.
    pub async fn record_task_attempt_progress(
        &self,
        fence: TaskAttemptFence,
        observed_at: DateTime<Utc>,
        step_bound_seconds: Option<u32>,
    ) -> Result<TaskAttemptProgressOutcome> {
        let step_bound_seconds_db = step_bound_seconds
            .map(|seconds| {
                if seconds == 0 {
                    return Err(Error::InvalidRepositoryInput {
                        message: "progress step bound seconds must be positive".to_string(),
                    });
                }
                i32::try_from(seconds).map_err(|_| Error::InvalidRepositoryInput {
                    message: "progress step bound seconds exceeds PostgreSQL INTEGER".to_string(),
                })
            })
            .transpose()?;
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptProgressOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptProgressOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if !task_attempt_fence_matches(&run, &task, &fence) {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptProgressOutcome::Stale);
        }
        if task.status != ExecutionTaskStatus::Running
            || task.attempt_state != ExecutionAttemptState::Running
        {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskAttemptProgressOutcome::InvalidState);
        }
        let outcome = if observed_at <= task.last_progress_at {
            TaskAttemptProgressOutcome::Replayed
        } else {
            TaskAttemptProgressOutcome::Applied
        };
        // The bound is overwritten rather than merged: it describes the step now in flight,
        // so a stale wider bound from the previous step must not outlive it and keep the
        // watchdog lenient after a long step has already finished.
        sqlx::query(
            "UPDATE moa.execution_task \
             SET last_progress_at = GREATEST(last_progress_at, $5), \
                 progress_step_bound_seconds = $6, updated_at = NOW() \
             WHERE run_uid = $1 AND task_id = $2 AND attempt_generation = $3 \
               AND active_dispatch_uid = $4 AND attempt_state = 'running'",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(observed_at)
        .bind(step_bound_seconds_db)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if outcome == TaskAttemptProgressOutcome::Replayed {
            conn.commit().await.map_err(storage_error)?;
            return Ok(outcome);
        }
        sqlx::query(
            "UPDATE moa.execution_run SET last_progress_at = GREATEST(last_progress_at, $2), \
             updated_at = NOW() WHERE run_uid = $1",
        )
        .bind(fence.run_uid)
        .bind(observed_at)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(outcome)
    }

    /// Persists one bounded task outcome, releases active capacity, and wakes its controller.
    ///
    /// `retry_at` must be present exactly for a retryable failure. The logical and
    /// attempt generations are advanced before that delayed controller activation
    /// can admit the next slice.
    pub async fn settle_task_attempt(
        &self,
        config: &ExecutionConfig,
        fence: TaskAttemptFence,
        outcome: ExecutionTaskOutcome,
        retry_at: Option<DateTime<Utc>>,
        settled_at: DateTime<Utc>,
    ) -> Result<TaskAttemptSettlementOutcome> {
        self.settle_task_attempt_inner(SettleTaskAttemptRequest {
            config,
            fence,
            outcome,
            retry_at,
            settled_at,
            expected_attempt_state: ExecutionAttemptState::Running,
            workspace_release_receipt: None,
            continuation_checkpoint: None,
        })
        .await
    }

    /// Finalizes a cancelling attempt only after exact sandbox release proof is available.
    pub async fn settle_released_task_attempt(
        &self,
        config: &ExecutionConfig,
        fence: TaskAttemptFence,
        outcome: ExecutionTaskOutcome,
        retry_at: Option<DateTime<Utc>>,
        settled_at: DateTime<Utc>,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    ) -> Result<TaskAttemptSettlementOutcome> {
        if workspace_release_receipt.as_ref().is_some_and(|receipt| {
            receipt.tenant_id != fence.tenant_id
                || receipt.run_id.0 != fence.run_uid
                || !matches!(
                    receipt.owner,
                    ExecutionHandReleaseOwner::Task { task_id, .. }
                        if task_id.0 == fence.task_id.as_uuid()
                )
                || receipt.attempt_generation != fence.attempt_generation
        }) {
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        self.settle_task_attempt_inner(SettleTaskAttemptRequest {
            config,
            fence,
            outcome,
            retry_at,
            settled_at,
            expected_attempt_state: ExecutionAttemptState::Cancelling,
            workspace_release_receipt,
            continuation_checkpoint: None,
        })
        .await
    }

    /// Releases the exact normal-outcome task capacity after durable sandbox-release proof.
    pub async fn release_released_task_attempt_capacity(
        &self,
        fence: TaskAttemptFence,
        logical_generation: u64,
        workspace_release_receipt: ExecutionHandReleaseReceipt,
    ) -> Result<ReleasedTaskAttemptCapacityOutcome> {
        if workspace_release_receipt.tenant_id != fence.tenant_id
            || workspace_release_receipt.run_id.0 != fence.run_uid
            || !matches!(
                workspace_release_receipt.owner,
                ExecutionHandReleaseOwner::Task { task_id, logical_generation: receipt_generation }
                    if task_id.0 == fence.task_id.as_uuid()
                        && receipt_generation == logical_generation
            )
            || workspace_release_receipt.attempt_generation != fence.attempt_generation
        {
            return Ok(ReleasedTaskAttemptCapacityOutcome::Stale);
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        // The exact admitted task and watchdog receipts prove these four buckets already exist.
        // Release must preserve their persisted limits rather than reconcile current config.
        prelock_existing_capacity_dimensions_in_tx(
            conn.as_mut(),
            fence.tenant_id,
            &[
                ExecutionCapacityDimension::ActiveTasks,
                ExecutionCapacityDimension::ScheduledTriggers,
            ],
        )
        .await?;
        let capacity = release_task_capacity_in_tx(
            &mut conn,
            fence.capacity_reservation_uid,
            fence.run_uid,
            fence.task_id,
            fence.attempt_generation,
        )
        .await?;
        if capacity == CapacityReleaseOutcome::NotFound {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReleasedTaskAttemptCapacityOutcome::NotFound);
        }
        if capacity == CapacityReleaseOutcome::Stale {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReleasedTaskAttemptCapacityOutcome::Stale);
        }
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReleasedTaskAttemptCapacityOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReleasedTaskAttemptCapacityOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if !task_attempt_fence_matches(&run, &task, &fence)
            || task.generation != logical_generation
            || !persisted_task_release_receipt_matches(
                &mut conn,
                &fence,
                logical_generation,
                &workspace_release_receipt,
            )
            .await?
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReleasedTaskAttemptCapacityOutcome::Stale);
        }
        if task.status != ExecutionTaskStatus::Running
            || task.attempt_state != ExecutionAttemptState::Cancelling
            || run.status.is_terminal()
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReleasedTaskAttemptCapacityOutcome::InvalidState);
        }
        let superseded = supersede_trigger_in_conn(
            conn.as_mut(),
            fence.watchdog_trigger_uid,
            ExecutionTriggerKind::TaskWatchdog,
            Some(fence.controller_generation),
            Some(fence.attempt_generation),
            None,
            None,
        )
        .await?;
        if superseded == ExecutionTriggerSupersedeOutcome::StaleOrMissing {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(ReleasedTaskAttemptCapacityOutcome::Stale);
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(if capacity == CapacityReleaseOutcome::Released {
            ReleasedTaskAttemptCapacityOutcome::Applied
        } else {
            ReleasedTaskAttemptCapacityOutcome::Replayed
        })
    }

    /// Settles an input wait while preserving the exact resumable agent continuation.
    pub async fn settle_released_task_attempt_with_checkpoint(
        &self,
        config: &ExecutionConfig,
        fence: TaskAttemptFence,
        outcome: ExecutionTaskOutcome,
        settled_at: DateTime<Utc>,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
        checkpoint: NewTaskAttemptCheckpoint,
    ) -> Result<TaskAttemptSettlementOutcome> {
        if !matches!(outcome.result, ExecutionTaskResult::NeedsInput { .. })
            || checkpoint.fence != fence
            || checkpoint.workspace_release_receipt != workspace_release_receipt
        {
            return Ok(TaskAttemptSettlementOutcome::InvalidState);
        }
        self.settle_task_attempt_inner(SettleTaskAttemptRequest {
            config,
            fence,
            outcome,
            retry_at: None,
            settled_at,
            expected_attempt_state: ExecutionAttemptState::Cancelling,
            workspace_release_receipt,
            continuation_checkpoint: Some(checkpoint),
        })
        .await
    }

    /// Finalizes a pause-owned release by returning the logical task to Ready without dispatch.
    pub async fn finalize_paused_task_attempt_release(
        &self,
        current_controller_generation: u64,
        fence: TaskAttemptFence,
        settled_at: DateTime<Utc>,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
    ) -> Result<TaskAttemptSettlementOutcome> {
        if workspace_release_receipt.as_ref().is_some_and(|receipt| {
            receipt.tenant_id != fence.tenant_id
                || receipt.run_id.0 != fence.run_uid
                || !matches!(
                    receipt.owner,
                    ExecutionHandReleaseOwner::Task { task_id, .. }
                        if task_id.0 == fence.task_id.as_uuid()
                )
                || receipt.attempt_generation != fence.attempt_generation
        }) {
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let capacity = release_task_capacity_in_tx(
            &mut conn,
            fence.capacity_reservation_uid,
            fence.run_uid,
            fence.task_id,
            fence.attempt_generation,
        )
        .await?;
        if capacity == CapacityReleaseOutcome::NotFound {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        }
        if capacity == CapacityReleaseOutcome::Stale {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if capacity == CapacityReleaseOutcome::AlreadyReleased {
            let replay = task.status == ExecutionTaskStatus::Ready
                && task.active_dispatch_uid.is_none()
                && task.attempt_generation == fence.attempt_generation.saturating_add(1)
                && run.controller_generation == current_controller_generation
                && matches!(
                    run.status,
                    ExecutionRunStatus::PauseRequested
                        | ExecutionRunStatus::Pausing
                        | ExecutionRunStatus::Paused
                )
                && paused_task_attempt_release_history_matches(
                    &task.generation_history,
                    &fence,
                    current_controller_generation,
                );
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                TaskAttemptSettlementOutcome::Replayed { run, task }
            } else {
                TaskAttemptSettlementOutcome::Stale
            });
        }
        if !task_attempt_resource_fence_matches(&run, &task, &fence)
            || run.controller_generation != current_controller_generation
            || task.status != ExecutionTaskStatus::Running
            || task.attempt_state != ExecutionAttemptState::Cancelling
            || !matches!(
                run.status,
                ExecutionRunStatus::PauseRequested
                    | ExecutionRunStatus::Pausing
                    | ExecutionRunStatus::Paused
            )
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::InvalidState);
        }
        if let Some(receipt) = workspace_release_receipt.as_ref()
            && !persisted_task_release_receipt_matches(&mut conn, &fence, task.generation, receipt)
                .await?
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        if supersede_trigger_in_conn(
            conn.as_mut(),
            fence.watchdog_trigger_uid,
            ExecutionTriggerKind::TaskWatchdog,
            Some(fence.controller_generation),
            Some(fence.attempt_generation),
            None,
            None,
        )
        .await?
            == ExecutionTriggerSupersedeOutcome::StaleOrMissing
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_task SET status='ready', attempt_state='idle', \
             attempt_generation=attempt_generation+1, active_dispatch_uid=NULL, \
             attempt_deadline_at=NULL, ready_at=$5, waiting_since=NULL, \
             progress_step_bound_seconds=NULL, \
             last_progress_at=GREATEST(last_progress_at,$5), \
             generation_history=generation_history || jsonb_build_array(jsonb_build_object( \
                'kind','pause_release_finalized','dispatch_uid',$4::TEXT, \
                'attempt_generation',$3,'attempt_controller_generation',$7, \
                'controller_generation',$8,'workspace_release_receipt_id',$6::TEXT, \
                'recorded_at',$5)), updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
               AND active_dispatch_uid=$4 AND status='running' \
               AND attempt_state='cancelling' RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(settled_at)
        .bind(
            workspace_release_receipt
                .as_ref()
                .map(|receipt| receipt.receipt_id),
        )
        .bind(to_i64(
            fence.controller_generation,
            "attempt controller generation",
        )?)
        .bind(to_i64(
            current_controller_generation,
            "current controller generation",
        )?)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        transition_node_counters_in_tx(
            &mut conn,
            fence.run_uid,
            &task.node_id,
            &task.item_key,
            ExecutionTaskStatus::Running,
            ExecutionTaskStatus::Ready,
        )
        .await?;
        let run_row = sqlx::query(LOAD_RUN_SQL)
            .bind(fence.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&run_row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptSettlementOutcome::Applied { run, task })
    }

    async fn settle_task_attempt_inner(
        &self,
        request: SettleTaskAttemptRequest<'_>,
    ) -> Result<TaskAttemptSettlementOutcome> {
        let SettleTaskAttemptRequest {
            config,
            fence,
            outcome,
            retry_at,
            settled_at,
            expected_attempt_state,
            workspace_release_receipt,
            continuation_checkpoint,
        } = request;
        let is_retry = matches!(
            outcome.result,
            ExecutionTaskResult::Failed {
                class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
                ..
            }
        );
        if is_retry != retry_at.is_some() || retry_at.is_some_and(|at| at < settled_at) {
            return Err(Error::InvalidRepositoryInput {
                message: "task attempt retry time must be present exactly for a future retry"
                    .to_string(),
            });
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let capacity_was_pre_released = workspace_release_receipt.is_some()
            && sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_reservation \
                 WHERE reservation_uid=$1 AND tenant_id=$2 AND run_uid=$3 AND task_id=$4 \
                   AND controller_generation=$5 AND attempt_generation=$6 \
                   AND resource_dimension='active_tasks' AND state='released')",
            )
            .bind(fence.capacity_reservation_uid)
            .bind(fence.tenant_id.0)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .bind(to_i64(
                fence.controller_generation,
                "controller generation",
            )?)
            .bind(to_i64(fence.attempt_generation, "attempt generation")?)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let capacity = if capacity_was_pre_released {
            CapacityReleaseOutcome::AlreadyReleased
        } else {
            release_task_capacity_in_tx(
                &mut conn,
                fence.capacity_reservation_uid,
                fence.run_uid,
                fence.task_id,
                fence.attempt_generation,
            )
            .await?
        };
        if matches!(capacity, CapacityReleaseOutcome::NotFound) {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        }
        if matches!(capacity, CapacityReleaseOutcome::Stale) {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        // A relative input-wait expiry resolves against wait entry, not compile time, so a delay
        // that was legal when the plan compiled can land past the run deadline here. That is a
        // product outcome for a long-horizon run, so the task fails terminally with a typed
        // deadline failure instead of parking on a wait that can never settle in time.
        let needs_input = matches!(outcome.result, ExecutionTaskResult::NeedsInput { .. });
        let outcome = match run.approved_budget.deadline_at {
            Some(run_deadline_at) if needs_input => {
                match crate::interpreter::resolve_temporal_target_within_deadline(
                    &run.active_plan.definition.input_wait_policy.expiry,
                    settled_at,
                    run_deadline_at,
                )? {
                    crate::interpreter::TemporalTargetResolution::Due(_) => outcome,
                    crate::interpreter::TemporalTargetResolution::DeadlineExceeded {
                        due_at,
                        run_deadline_at,
                    } => failed_task_outcome(
                        moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
                        format!(
                            "input wait on node `{}` entered at {settled_at} resolves at \
                             {due_at}, at or after the run deadline {run_deadline_at}",
                            task.node_id
                        ),
                        outcome.usage.clone(),
                    ),
                }
            }
            _ => outcome,
        };
        if capacity == CapacityReleaseOutcome::AlreadyReleased {
            let replay = task_attempt_settlement_replayed(&task, &fence, &outcome);
            if replay {
                conn.commit().await.map_err(storage_error)?;
                return Ok(TaskAttemptSettlementOutcome::Replayed { run, task });
            }
            if !capacity_was_pre_released {
                conn.commit().await.map_err(storage_error)?;
                return Ok(TaskAttemptSettlementOutcome::Stale);
            }
        }
        if !task_attempt_fence_matches(&run, &task, &fence) {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        if let Some(receipt) = workspace_release_receipt.as_ref()
            && !persisted_task_release_receipt_matches(&mut conn, &fence, task.generation, receipt)
                .await?
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::Stale);
        }
        if task.status != ExecutionTaskStatus::Running
            || task.attempt_state != expected_attempt_state
            || run.status.is_terminal()
            || (is_retry && task.attempt >= task.retry.max_attempts)
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptSettlementOutcome::InvalidState);
        }
        if capacity_was_pre_released {
            let watchdog_was_superseded = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM moa.execution_trigger \
                 WHERE trigger_uid=$1 AND tenant_id=$2 AND run_uid=$3 AND task_id=$4 \
                   AND trigger_kind='task_watchdog' AND controller_generation=$5 \
                   AND attempt_generation=$6 AND state='superseded')",
            )
            .bind(fence.watchdog_trigger_uid)
            .bind(fence.tenant_id.0)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .bind(to_i64(
                fence.controller_generation,
                "controller generation",
            )?)
            .bind(to_i64(fence.attempt_generation, "attempt generation")?)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if !watchdog_was_superseded {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(TaskAttemptSettlementOutcome::Stale);
            }
        } else {
            let superseded = supersede_trigger_in_conn(
                conn.as_mut(),
                fence.watchdog_trigger_uid,
                ExecutionTriggerKind::TaskWatchdog,
                Some(fence.controller_generation),
                Some(fence.attempt_generation),
                None,
                None,
            )
            .await?;
            if superseded == ExecutionTriggerSupersedeOutcome::StaleOrMissing {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(TaskAttemptSettlementOutcome::Stale);
            }
        }
        if let Some(checkpoint) = &continuation_checkpoint {
            match persist_task_attempt_checkpoint_in_conn(&mut conn, checkpoint).await? {
                TaskAttemptCheckpointWriteOutcome::Applied(_)
                | TaskAttemptCheckpointWriteOutcome::Replayed(_) => {}
                TaskAttemptCheckpointWriteOutcome::NotFound => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptSettlementOutcome::NotFound);
                }
                TaskAttemptCheckpointWriteOutcome::Stale => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptSettlementOutcome::Stale);
                }
                TaskAttemptCheckpointWriteOutcome::InvalidState => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptSettlementOutcome::InvalidState);
                }
            }
        }
        let logical_generation = task.generation;
        let write = record_task_outcome_in_conn(
            &mut conn,
            fence.run_uid,
            fence.task_id,
            logical_generation,
            outcome,
        )
        .await?;
        let accepted_task = match write {
            TaskOutcomeWrite::Applied { task, .. } => task,
            TaskOutcomeWrite::NotFound => {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(TaskAttemptSettlementOutcome::NotFound);
            }
            TaskOutcomeWrite::Rejected { .. } => {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(TaskAttemptSettlementOutcome::InvalidState);
            }
            TaskOutcomeWrite::Replayed { run, task, .. } => {
                conn.rollback().await.map_err(storage_error)?;
                return Ok(TaskAttemptSettlementOutcome::Replayed { run, task });
            }
        };
        let (status, attempt_state, next_generation, next_attempt_generation, ready_at) =
            if let Some(retry_at) = retry_at {
                (
                    ExecutionTaskStatus::Ready,
                    ExecutionAttemptState::Idle,
                    logical_generation.checked_add(1).ok_or_else(|| {
                        Error::InvalidRepositoryInput {
                            message: "task logical generation overflow".to_string(),
                        }
                    })?,
                    fence.attempt_generation.checked_add(1).ok_or_else(|| {
                        Error::InvalidRepositoryInput {
                            message: "task attempt generation overflow".to_string(),
                        }
                    })?,
                    Some(retry_at),
                )
            } else {
                let accepted_outcome = accepted_task.current_outcome.as_ref().ok_or_else(|| {
                    Error::InvalidRepositoryData {
                        message: "accepted task outcome is missing from its projection".to_string(),
                    }
                })?;
                let status = task_status_from_outcome(accepted_outcome, false);
                let attempt_state = if status == ExecutionTaskStatus::UnknownOutcome {
                    ExecutionAttemptState::UnknownOutcome
                } else if status.is_terminal() {
                    ExecutionAttemptState::Terminal
                } else {
                    ExecutionAttemptState::Waiting
                };
                (
                    status,
                    attempt_state,
                    logical_generation,
                    fence.attempt_generation,
                    None,
                )
            };
        let row = sqlx::query(
            "UPDATE moa.execution_task \
             SET status = $3, attempt_state = $4, generation = $5, \
                 attempt_generation = $6, attempt = CASE WHEN $7::BOOLEAN THEN attempt + 1 ELSE attempt END, \
                 active_dispatch_uid = NULL, attempt_deadline_at = NULL, \
                 progress_step_bound_seconds = NULL, \
                 waiting_since = CASE WHEN $4 = 'waiting' THEN $8 ELSE NULL END, \
                 ready_at = $9, last_progress_at = GREATEST(last_progress_at, $8), \
                 generation_history = generation_history || jsonb_build_array($11::JSONB), \
                 updated_at = NOW() \
             WHERE run_uid = $1 AND task_id = $2 AND active_dispatch_uid = $10 \
             RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(status.as_str())
        .bind(attempt_state.as_str())
        .bind(to_i64(next_generation, "next task generation")?)
        .bind(to_i64(
            next_attempt_generation,
            "next attempt generation",
        )?)
        .bind(is_retry)
        .bind(settled_at)
        .bind(ready_at)
        .bind(fence.dispatch_uid)
        .bind(json!({
            "kind": "bounded_attempt_settlement",
            "dispatch_uid": fence.dispatch_uid,
            "attempt_generation": fence.attempt_generation,
            "retry_scheduled": is_retry,
            "workspace_release_receipt_id": workspace_release_receipt
                .as_ref()
                .map(|receipt| receipt.receipt_id),
            "recorded_at": settled_at,
        }))
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        let input_wait_due_at = if task.status == ExecutionTaskStatus::WaitingInput {
            let run_deadline_at =
                run.approved_budget
                    .deadline_at
                    .ok_or_else(|| Error::InvalidRepositoryInput {
                        message: "input waits require an absolute run deadline".to_string(),
                    })?;
            Some(crate::interpreter::resolve_temporal_target(
                &run.active_plan.definition.input_wait_policy.expiry,
                settled_at,
                run_deadline_at,
            )?)
        } else {
            None
        };
        if task.status == ExecutionTaskStatus::WaitingInput {
            let input_audience = task
                .current_outcome
                .as_ref()
                .and_then(|outcome| match &outcome.result {
                    ExecutionTaskResult::NeedsInput { audience, .. } => Some(audience),
                    _ => None,
                })
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "waiting-input task is missing its typed input audience".to_string(),
                })?;
            transition_node_counters_with_input_audience_in_tx(
                &mut conn,
                fence.run_uid,
                &task.node_id,
                &task.item_key,
                ExecutionTaskStatus::Running,
                task.status,
                input_audience,
            )
            .await?;
            let ExecutionTaskResult::NeedsInput { question, audience } = task
                .current_outcome
                .as_ref()
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "waiting-input task is missing its typed outcome".to_string(),
                })?
                .result
                .clone()
            else {
                return Err(Error::InvalidRepositoryData {
                    message: "waiting-input task is missing its typed input request".to_string(),
                });
            };
            append_run_wait_reason_in_tx(
                &mut conn,
                fence.run_uid,
                &WaitingReason::Input {
                    task_id: task.task_id,
                    audience,
                    question,
                    wait_policy: moa_artifacts::execution_plan::ExecutionWaitPolicy {
                        expiry: moa_artifacts::execution_plan::ExecutionTemporalTarget::At {
                            at: input_wait_due_at.ok_or_else(|| Error::InvalidRepositoryData {
                                message: "waiting-input task is missing its resolved expiry"
                                    .to_string(),
                            })?,
                        },
                        on_expiry: run
                            .active_plan
                            .definition
                            .input_wait_policy
                            .on_expiry
                            .clone(),
                    },
                },
                settled_at,
            )
            .await?;
        } else {
            transition_node_counters_in_tx(
                &mut conn,
                fence.run_uid,
                &task.node_id,
                &task.item_key,
                ExecutionTaskStatus::Running,
                task.status,
            )
            .await?;
        }
        if task.status == ExecutionTaskStatus::WaitingInput {
            let due_at = input_wait_due_at.ok_or_else(|| Error::InvalidRepositoryData {
                message: "waiting-input task is missing its resolved expiry".to_string(),
            })?;
            let trigger_uid = Uuid::new_v5(
                &TASK_INPUT_WAIT_TRIGGER_NAMESPACE,
                format!(
                    "{}:{}:{}:{}",
                    fence.run_uid, task.task_id, task.generation, settled_at
                )
                .as_bytes(),
            );
            create_trigger_with_dispatch_in_conn(
                conn.as_mut(),
                config,
                &NewExecutionTrigger {
                    trigger_uid,
                    tenant_id: fence.tenant_id,
                    run_uid: Some(fence.run_uid),
                    task_id: Some(fence.task_id.as_uuid()),
                    compensation_id: None,
                    schedule_uid: None,
                    schedule_incarnation: None,
                    kind: ExecutionTriggerKind::WaitExpiry,
                    controller_generation: Some(fence.controller_generation),
                    attempt_generation: Some(task.generation),
                    compensation_generation: None,
                    compensation_attempt_generation: None,
                    occurrence_sequence: None,
                    due_at,
                    payload: json!({
                        "task_generation": task.generation,
                        "waiting_since": settled_at,
                        "source": "active_task_input_wait",
                    }),
                },
            )
            .await?;
        }
        let activation_at = retry_at.unwrap_or(settled_at);
        enqueue_run_activation_in_conn(
            conn.as_mut(),
            fence.tenant_id,
            fence.run_uid,
            fence.controller_generation,
            activation_at,
            json!({
                "source": "task_attempt_settlement",
                "task_id": fence.task_id,
                "dispatch_uid": fence.dispatch_uid,
                "attempt_generation": fence.attempt_generation,
            }),
        )
        .await?;
        let run_row = sqlx::query(LOAD_RUN_SQL)
            .bind(fence.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&run_row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptSettlementOutcome::Applied { run, task })
    }

    /// Requeues a running task after provider recovery proved its reserved start did not happen.
    ///
    /// The caller must release the exact unbound external-job intent in the same transaction.
    /// The provisional checkpoint deliberately remains current and is consumed by the successor
    /// attempt so neither the model turn nor the direct capability call is reconstructed.
    pub(super) async fn requeue_task_external_start_not_started_in_conn(
        conn: &mut ScopedConn<'_>,
        intent: &NewExecutionExternalJobIntent,
        recovered_at: DateTime<Utc>,
    ) -> Result<TaskExternalStartRetryOutcome> {
        let ExecutionExternalJobOwner::Task {
            task_id,
            attempt_generation,
        } = intent.owner
        else {
            return Ok(TaskExternalStartRetryOutcome::InvalidState);
        };
        let task_id = ExecutionTaskId::from_uuid(task_id);
        let Some(fence) = load_running_external_start_fence(
            conn,
            intent.tenant_id,
            intent.run_uid,
            task_id,
            attempt_generation,
        )
        .await?
        else {
            return Ok(TaskExternalStartRetryOutcome::NotFound);
        };
        let capacity = release_task_capacity_in_tx(
            conn,
            fence.capacity_reservation_uid,
            fence.run_uid,
            fence.task_id,
            fence.attempt_generation,
        )
        .await?;
        if capacity == CapacityReleaseOutcome::NotFound {
            return Ok(TaskExternalStartRetryOutcome::NotFound);
        }
        if capacity == CapacityReleaseOutcome::Stale {
            return Ok(TaskExternalStartRetryOutcome::Stale);
        }
        let Some((run, task, checkpoint)) = load_locked_external_start_owner(conn, fence).await?
        else {
            return Ok(TaskExternalStartRetryOutcome::NotFound);
        };
        if capacity == CapacityReleaseOutcome::AlreadyReleased {
            let external_job_uid = intent.external_job_uid.to_string();
            let replay = task.status == ExecutionTaskStatus::Ready
                && task.attempt_state == ExecutionAttemptState::Idle
                && task.attempt_generation == fence.attempt_generation.saturating_add(1)
                && task.active_dispatch_uid.is_none()
                && task.generation_history.iter().any(|entry| {
                    entry.get("kind").and_then(Value::as_str) == Some("external_start_not_started")
                        && entry.get("external_job_uid").and_then(Value::as_str)
                            == Some(external_job_uid.as_str())
                });
            return Ok(if replay {
                TaskExternalStartRetryOutcome::Replayed {
                    task,
                    checkpoint: Box::new(checkpoint),
                }
            } else {
                TaskExternalStartRetryOutcome::Stale
            });
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        ) || task.status != ExecutionTaskStatus::Running
            || task.attempt_state != ExecutionAttemptState::Running
        {
            return Ok(TaskExternalStartRetryOutcome::InvalidState);
        }
        if supersede_trigger_in_conn(
            conn.as_mut(),
            fence.watchdog_trigger_uid,
            ExecutionTriggerKind::TaskWatchdog,
            Some(fence.controller_generation),
            Some(fence.attempt_generation),
            None,
            None,
        )
        .await?
            == ExecutionTriggerSupersedeOutcome::StaleOrMissing
        {
            return Ok(TaskExternalStartRetryOutcome::Stale);
        }
        let next_attempt_generation = fence.attempt_generation.checked_add(1).ok_or_else(|| {
            Error::InvalidRepositoryData {
                message: "task attempt generation overflow during external-start recovery"
                    .to_string(),
            }
        })?;
        let row = sqlx::query(
            "UPDATE moa.execution_task SET status='ready',attempt_state='idle', \
                 attempt_generation=$5,active_dispatch_uid=NULL,attempt_deadline_at=NULL, \
                 progress_step_bound_seconds=NULL, \
                 waiting_since=NULL,ready_at=$6, \
                 last_progress_at=GREATEST(last_progress_at,$6), \
                 generation_history=generation_history || jsonb_build_array(jsonb_build_object( \
                    'kind','external_start_not_started','external_job_uid',$7::TEXT, \
                    'dispatch_uid',$4::TEXT,'attempt_generation',$3,'recorded_at',$6)), \
                 updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
               AND active_dispatch_uid=$4 AND status='running' AND attempt_state='running' \
             RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(to_i64(next_attempt_generation, "next attempt generation")?)
        .bind(recovered_at)
        .bind(intent.external_job_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            return Ok(TaskExternalStartRetryOutcome::Stale);
        };
        let task = task_from_row(&row)?;
        transition_node_counters_in_tx(
            conn,
            fence.run_uid,
            &task.node_id,
            &task.item_key,
            ExecutionTaskStatus::Running,
            ExecutionTaskStatus::Ready,
        )
        .await?;
        enqueue_run_activation_in_conn(
            conn.as_mut(),
            fence.tenant_id,
            fence.run_uid,
            fence.controller_generation,
            recovered_at,
            json!({
                "source": "external_start_not_started",
                "task_id": fence.task_id,
                "external_job_uid": intent.external_job_uid,
                "attempt_generation": fence.attempt_generation,
            }),
        )
        .await?;
        Ok(TaskExternalStartRetryOutcome::Applied {
            task,
            checkpoint: Box::new(checkpoint),
        })
    }

    /// Adopts one recovered provider job and parks its exact running task atomically.
    ///
    /// The caller must bind the job and prelock `active_tasks` before `external_jobs` in the same
    /// scoped transaction. Async-capable task execution never owns a sandbox hand, so this path
    /// releases capacity and the watchdog without accepting a caller-supplied hand receipt.
    pub(super) async fn adopt_recovered_task_external_job_in_conn(
        conn: &mut ScopedConn<'_>,
        job: &ExecutionExternalJobRecord,
        adopted_at: DateTime<Utc>,
    ) -> Result<TaskAttemptExternalOutcome> {
        let ExecutionExternalJobOwner::Task {
            task_id,
            attempt_generation,
        } = job.owner
        else {
            return Ok(TaskAttemptExternalOutcome::InvalidState);
        };
        if job.state == ExecutionExternalJobState::Unbound {
            return Ok(TaskAttemptExternalOutcome::InvalidState);
        }
        let task_id = ExecutionTaskId::from_uuid(task_id);
        let Some(fence) = load_running_external_start_fence(
            conn,
            job.tenant_id,
            job.run_uid,
            task_id,
            attempt_generation,
        )
        .await?
        else {
            return Ok(TaskAttemptExternalOutcome::NotFound);
        };
        let capacity = release_task_capacity_in_tx(
            conn,
            fence.capacity_reservation_uid,
            fence.run_uid,
            fence.task_id,
            fence.attempt_generation,
        )
        .await?;
        if capacity == CapacityReleaseOutcome::NotFound {
            return Ok(TaskAttemptExternalOutcome::NotFound);
        }
        if capacity == CapacityReleaseOutcome::Stale {
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        let Some((run, task, checkpoint)) = load_locked_external_start_owner(conn, fence).await?
        else {
            return Ok(TaskAttemptExternalOutcome::NotFound);
        };
        if capacity == CapacityReleaseOutcome::AlreadyReleased {
            let replay = task.attempt_generation == fence.attempt_generation
                && task.active_dispatch_uid.is_none()
                && (task.status == ExecutionTaskStatus::WaitingExternal
                    || task.status.is_terminal())
                && task.external_job_uid == Some(job.external_job_uid);
            return Ok(if replay {
                TaskAttemptExternalOutcome::Replayed {
                    run,
                    task,
                    external_job: job.clone(),
                }
            } else {
                TaskAttemptExternalOutcome::Stale
            });
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::Queued | ExecutionRunStatus::Running
        ) || task.status != ExecutionTaskStatus::Running
            || task.attempt_state != ExecutionAttemptState::Running
        {
            return Ok(TaskAttemptExternalOutcome::InvalidState);
        }
        let mut payload = checkpoint.payload.clone();
        bind_recovered_external_job_in_checkpoint(
            &mut payload,
            checkpoint.kind,
            job.external_job_uid,
        )?;
        if payload != checkpoint.payload {
            insert_resolved_task_checkpoint_in_conn(
                conn,
                &checkpoint,
                run.controller_generation,
                checkpoint.task_generation,
                checkpoint.attempt_generation,
                payload,
                adopted_at,
            )
            .await?;
        }
        if supersede_trigger_in_conn(
            conn.as_mut(),
            fence.watchdog_trigger_uid,
            ExecutionTriggerKind::TaskWatchdog,
            Some(fence.controller_generation),
            Some(fence.attempt_generation),
            None,
            None,
        )
        .await?
            == ExecutionTriggerSupersedeOutcome::StaleOrMissing
        {
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_task SET status='waiting_external',attempt_state='waiting', \
                 waiting_since=$5,external_job_uid=$6,active_dispatch_uid=NULL, \
                 attempt_deadline_at=NULL, \
                 progress_step_bound_seconds=NULL, \
                 last_progress_at=GREATEST(last_progress_at,$5),updated_at=NOW() \
             WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
               AND active_dispatch_uid=$4 AND status='running' AND attempt_state='running' \
             RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(adopted_at)
        .bind(job.external_job_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(row) = row else {
            return Ok(TaskAttemptExternalOutcome::Stale);
        };
        let task = task_from_row(&row)?;
        transition_node_counters_in_tx(
            conn,
            fence.run_uid,
            &task.node_id,
            &task.item_key,
            ExecutionTaskStatus::Running,
            ExecutionTaskStatus::WaitingExternal,
        )
        .await?;
        append_run_wait_reason_in_tx(
            conn,
            fence.run_uid,
            &WaitingReason::External {
                task_id: task.task_id,
            },
            adopted_at,
        )
        .await?;
        enqueue_run_activation_in_conn(
            conn.as_mut(),
            fence.tenant_id,
            fence.run_uid,
            fence.controller_generation,
            adopted_at,
            json!({
                "source": "external_start_recovered",
                "task_id": fence.task_id,
                "external_job_uid": job.external_job_uid,
            }),
        )
        .await?;
        let run_row = sqlx::query(LOAD_RUN_SQL)
            .bind(fence.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        Ok(TaskAttemptExternalOutcome::Applied {
            run: run_from_row(&run_row)?,
            task,
            external_job: job.clone(),
        })
    }

    /// Commits one provider job before parking the task and releasing active resources.
    pub async fn yield_task_attempt_to_external_job(
        &self,
        fence: TaskAttemptFence,
        external_job_uid: Uuid,
        continuation_checkpoint: Option<NewTaskAttemptCheckpoint>,
        workspace_release_receipt: Option<ExecutionHandReleaseReceipt>,
        yielded_at: DateTime<Utc>,
    ) -> Result<TaskAttemptExternalOutcome> {
        if workspace_release_receipt.as_ref().is_some_and(|receipt| {
            receipt.tenant_id != fence.tenant_id
                || receipt.run_id.0 != fence.run_uid
                || !matches!(
                    receipt.owner,
                    ExecutionHandReleaseOwner::Task { task_id, .. }
                        if task_id.0 == fence.task_id.as_uuid()
                )
                || receipt.attempt_generation != fence.attempt_generation
        }) {
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        if continuation_checkpoint.as_ref().is_some_and(|checkpoint| {
            checkpoint.fence != fence
                || checkpoint_external_job_uid(&checkpoint.payload) != Some(external_job_uid)
                || checkpoint.workspace_release_receipt != workspace_release_receipt
        }) {
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        let mut conn = ExecutionScope::ControlPlane.begin(&self.pool).await?;
        let capacity = release_task_capacity_in_tx(
            &mut conn,
            fence.capacity_reservation_uid,
            fence.run_uid,
            fence.task_id,
            fence.attempt_generation,
        )
        .await?;
        if matches!(capacity, CapacityReleaseOutcome::NotFound) {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::NotFound);
        }
        if matches!(capacity, CapacityReleaseOutcome::Stale) {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        let Some(persisted_job) =
            load_external_job_for_update_in_conn(conn.as_mut(), external_job_uid).await?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::NotFound);
        };
        if persisted_job.tenant_id != fence.tenant_id
            || persisted_job.run_uid != fence.run_uid
            || persisted_job.owner
                != (ExecutionExternalJobOwner::Task {
                    task_id: fence.task_id.as_uuid(),
                    attempt_generation: fence.attempt_generation,
                })
            || persisted_job.state == ExecutionExternalJobState::Unbound
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::NotFound);
        };
        let run = run_from_row(&run_row)?;
        let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
            .bind(fence.run_uid)
            .bind(fence.task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?
        else {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::NotFound);
        };
        let task = task_from_row(&task_row)?;
        if capacity == CapacityReleaseOutcome::AlreadyReleased {
            let replay = task.attempt_generation == fence.attempt_generation
                && task.active_dispatch_uid.is_none()
                && (task.status == ExecutionTaskStatus::WaitingExternal
                    || task.status.is_terminal())
                && task.external_job_uid == Some(external_job_uid);
            conn.commit().await.map_err(storage_error)?;
            return Ok(if replay {
                TaskAttemptExternalOutcome::Replayed {
                    run,
                    task,
                    external_job: persisted_job,
                }
            } else {
                TaskAttemptExternalOutcome::Stale
            });
        }
        if !task_attempt_fence_matches(&run, &task, &fence) {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        if let Some(receipt) = workspace_release_receipt.as_ref()
            && !persisted_task_release_receipt_matches(&mut conn, &fence, task.generation, receipt)
                .await?
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        if let Some(checkpoint) = &continuation_checkpoint {
            match persist_task_attempt_checkpoint_in_conn(&mut conn, checkpoint).await? {
                TaskAttemptCheckpointWriteOutcome::Applied(_)
                | TaskAttemptCheckpointWriteOutcome::Replayed(_) => {}
                TaskAttemptCheckpointWriteOutcome::NotFound => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptExternalOutcome::NotFound);
                }
                TaskAttemptCheckpointWriteOutcome::Stale => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptExternalOutcome::Stale);
                }
                TaskAttemptCheckpointWriteOutcome::InvalidState => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptExternalOutcome::InvalidState);
                }
            }
        }
        if task.status != ExecutionTaskStatus::Running
            || task.attempt_state != ExecutionAttemptState::Cancelling
            || run.status.is_terminal()
        {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::InvalidState);
        }
        let superseded = supersede_trigger_in_conn(
            conn.as_mut(),
            fence.watchdog_trigger_uid,
            ExecutionTriggerKind::TaskWatchdog,
            Some(fence.controller_generation),
            Some(fence.attempt_generation),
            None,
            None,
        )
        .await?;
        if superseded == ExecutionTriggerSupersedeOutcome::StaleOrMissing {
            conn.rollback().await.map_err(storage_error)?;
            return Ok(TaskAttemptExternalOutcome::Stale);
        }
        let row = sqlx::query(
            "UPDATE moa.execution_task \
             SET status = 'waiting_external', attempt_state = 'waiting', \
                 waiting_since = $5, external_job_uid = $6, active_dispatch_uid = NULL, \
                 attempt_deadline_at = NULL, progress_step_bound_seconds = NULL, \
                 last_progress_at = GREATEST(last_progress_at, $5), updated_at = NOW() \
             WHERE run_uid = $1 AND task_id = $2 AND attempt_generation = $3 \
               AND active_dispatch_uid = $4 RETURNING *",
        )
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(fence.attempt_generation, "attempt generation")?)
        .bind(fence.dispatch_uid)
        .bind(yielded_at)
        .bind(external_job_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let task = task_from_row(&row)?;
        transition_node_counters_in_tx(
            &mut conn,
            fence.run_uid,
            &task.node_id,
            &task.item_key,
            ExecutionTaskStatus::Running,
            ExecutionTaskStatus::WaitingExternal,
        )
        .await?;
        append_run_wait_reason_in_tx(
            &mut conn,
            fence.run_uid,
            &WaitingReason::External {
                task_id: task.task_id,
            },
            yielded_at,
        )
        .await?;
        let task = if persisted_job.state.is_terminal() {
            match settle_external_job_terminal_in_conn(&mut conn, &persisted_job, yielded_at)
                .await?
            {
                ExternalJobTaskSettlementOutcome::Applied(task)
                | ExternalJobTaskSettlementOutcome::Replayed(task) => task,
                ExternalJobTaskSettlementOutcome::DeferredRelease(_) => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptExternalOutcome::InvalidState);
                }
                ExternalJobTaskSettlementOutcome::Stale => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptExternalOutcome::Stale);
                }
                ExternalJobTaskSettlementOutcome::NotFound => {
                    conn.rollback().await.map_err(storage_error)?;
                    return Ok(TaskAttemptExternalOutcome::NotFound);
                }
            }
        } else {
            task
        };
        enqueue_run_activation_in_conn(
            conn.as_mut(),
            fence.tenant_id,
            fence.run_uid,
            fence.controller_generation,
            yielded_at,
            json!({
                "source": if persisted_job.state.is_terminal() {
                    "external_job_terminal_after_release"
                } else {
                    "external_job_started"
                },
                "task_id": fence.task_id,
                "external_job_uid": external_job_uid,
            }),
        )
        .await?;
        let run_row = sqlx::query(LOAD_RUN_SQL)
            .bind(fence.run_uid)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let run = run_from_row(&run_row)?;
        conn.commit().await.map_err(storage_error)?;
        Ok(TaskAttemptExternalOutcome::Applied {
            run,
            task,
            external_job: persisted_job,
        })
    }
}

/// Settles one exact terminal external job into its waiting logical task.
pub async fn settle_external_job_terminal_in_conn(
    conn: &mut ScopedConn<'_>,
    job: &ExecutionExternalJobRecord,
    settled_at: DateTime<Utc>,
) -> Result<ExternalJobTaskSettlementOutcome> {
    if !job.state.is_terminal() {
        return Err(Error::InvalidRepositoryInput {
            message: "external-job task settlement requires a terminal job".to_string(),
        });
    }
    let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(job.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(ExternalJobTaskSettlementOutcome::NotFound);
    };
    let run = run_from_row(&run_row)?;
    let ExecutionExternalJobOwner::Task {
        task_id: external_task_id,
        attempt_generation: external_attempt_generation,
    } = job.owner
    else {
        return Ok(ExternalJobTaskSettlementOutcome::Stale);
    };
    let task_id = ExecutionTaskId::from_uuid(external_task_id);
    let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
        .bind(job.run_uid)
        .bind(external_task_id)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(ExternalJobTaskSettlementOutcome::NotFound);
    };
    let task = task_from_row(&task_row)?;
    if task.status.is_terminal() {
        return Ok(if task.external_job_uid == Some(job.external_job_uid) {
            ExternalJobTaskSettlementOutcome::Replayed(task)
        } else {
            ExternalJobTaskSettlementOutcome::Stale
        });
    }
    let exact_active_owner = run.tenant_id == job.tenant_id
        && task.tenant_id == job.tenant_id
        && task.attempt_generation == external_attempt_generation
        && task.status == ExecutionTaskStatus::Running;
    let release_pending = task.attempt_state == ExecutionAttemptState::Cancelling
        && task.external_job_uid == Some(job.external_job_uid);
    let release_not_started = task.attempt_state == ExecutionAttemptState::Running
        && task.external_job_uid.is_none()
        && task.active_dispatch_uid.is_some()
        && task.attempt_deadline_at.is_some();
    if exact_active_owner && (release_pending || release_not_started) {
        return Ok(ExternalJobTaskSettlementOutcome::DeferredRelease(task));
    }
    if run.tenant_id != job.tenant_id
        || task.tenant_id != job.tenant_id
        || task.attempt_generation != external_attempt_generation
        || task.external_job_uid != Some(job.external_job_uid)
        || task.status != ExecutionTaskStatus::WaitingExternal
        || task.attempt_state != ExecutionAttemptState::Waiting
    {
        return Ok(ExternalJobTaskSettlementOutcome::Stale);
    }
    let terminal_resolution = match job.state {
        super::external_job::ExecutionExternalJobState::Completed => {
            moa_core::types::tools::AsyncToolJobTerminalOutcome::Completed {
                output: job.output.clone().unwrap_or(Value::Null),
            }
        }
        super::external_job::ExecutionExternalJobState::Failed => {
            moa_core::types::tools::AsyncToolJobTerminalOutcome::Failed {
                error: job
                    .error
                    .clone()
                    .unwrap_or_else(|| json!({"message": "asynchronous provider job failed"})),
            }
        }
        super::external_job::ExecutionExternalJobState::Cancelled => {
            moa_core::types::tools::AsyncToolJobTerminalOutcome::Cancelled
        }
        super::external_job::ExecutionExternalJobState::UnknownOutcome => {
            moa_core::types::tools::AsyncToolJobTerminalOutcome::UnknownOutcome {
                error: job.error.clone().unwrap_or_else(
                    || json!({"message": "asynchronous provider outcome is unknown"}),
                ),
            }
        }
        super::external_job::ExecutionExternalJobState::Unbound
        | super::external_job::ExecutionExternalJobState::Starting
        | super::external_job::ExecutionExternalJobState::Running
        | super::external_job::ExecutionExternalJobState::WaitingReconcile
        | super::external_job::ExecutionExternalJobState::CancelRequested => {
            return Err(Error::InvalidRepositoryInput {
                message: "external-job task settlement observed a nonterminal state".to_string(),
            });
        }
    };
    let current_checkpoint = sqlx::query(
        "SELECT * FROM moa.execution_task_checkpoint WHERE tenant_id=$1 AND run_uid=$2 \
         AND task_id=$3 AND superseded_at IS NULL FOR UPDATE",
    )
    .bind(job.tenant_id.0)
    .bind(job.run_uid)
    .bind(external_task_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if let Some(current_checkpoint) = current_checkpoint {
        let current_checkpoint = task_checkpoint_from_row(&current_checkpoint)?;
        if current_checkpoint.kind == TaskAttemptCheckpointKind::AgentContinuation
            && checkpoint_external_job_uid(&current_checkpoint.payload)
                == Some(job.external_job_uid)
        {
            let mut payload = current_checkpoint.payload.clone();
            payload["external_job_resolution"] = serde_json::to_value(&terminal_resolution)?;
            insert_resolved_task_checkpoint_in_conn(
                conn,
                &current_checkpoint,
                run.controller_generation,
                current_checkpoint.task_generation,
                current_checkpoint.attempt_generation,
                payload,
                settled_at,
            )
            .await?;
            let next_attempt_generation =
                task.attempt_generation.checked_add(1).ok_or_else(|| {
                    Error::InvalidRepositoryData {
                        message: "task attempt generation overflow".to_string(),
                    }
                })?;
            let row = sqlx::query(
                "UPDATE moa.execution_task SET status='ready', attempt_state='idle', \
                 waiting_since=NULL, ready_at=$3, external_job_uid=NULL, \
                 progress_step_bound_seconds=NULL, \
                 attempt_generation=$4, last_progress_at=GREATEST(last_progress_at,$3), \
                 updated_at=NOW() \
                 WHERE run_uid=$1 AND task_id=$2 AND status='waiting_external' \
                 RETURNING *",
            )
            .bind(job.run_uid)
            .bind(external_task_id)
            .bind(settled_at)
            .bind(to_i64(next_attempt_generation, "next attempt generation")?)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let resumed = task_from_row(&row)?;
            transition_node_counters_in_tx(
                conn,
                job.run_uid,
                &resumed.node_id,
                &resumed.item_key,
                ExecutionTaskStatus::WaitingExternal,
                ExecutionTaskStatus::Ready,
            )
            .await?;
            refresh_run_after_wait_settlement_in_conn(conn, job.run_uid, task.task_id, settled_at)
                .await?;
            return Ok(ExternalJobTaskSettlementOutcome::Applied(resumed));
        }
    }
    let outcome = match job.state {
        super::external_job::ExecutionExternalJobState::Completed => completed_task_outcome(
            job.output.clone().unwrap_or(Value::Null),
            task.actual.clone(),
        ),
        super::external_job::ExecutionExternalJobState::Failed => failed_task_outcome(
            moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
            job.error
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_else(|| "asynchronous provider job failed".to_string()),
            task.actual.clone(),
        ),
        super::external_job::ExecutionExternalJobState::Cancelled => cancelled_task_outcome(
            "asynchronous provider job was cancelled".to_string(),
            task.actual.clone(),
        ),
        super::external_job::ExecutionExternalJobState::UnknownOutcome => ExecutionTaskOutcome {
            schema_version: 1,
            usage: task.actual.clone(),
            result: ExecutionTaskResult::UnknownOutcome {
                message: job.error.as_ref().map(Value::to_string).unwrap_or_else(|| {
                    "asynchronous provider job has an unknown outcome".to_string()
                }),
            },
        },
        super::external_job::ExecutionExternalJobState::Unbound
        | super::external_job::ExecutionExternalJobState::Starting
        | super::external_job::ExecutionExternalJobState::Running
        | super::external_job::ExecutionExternalJobState::WaitingReconcile
        | super::external_job::ExecutionExternalJobState::CancelRequested => {
            return Err(Error::InvalidRepositoryInput {
                message: "external-job task settlement observed a nonterminal state".to_string(),
            });
        }
    };
    let write = record_waiting_external_task_outcome_in_conn(
        conn,
        job.run_uid,
        task_id,
        task.generation,
        outcome,
    )
    .await?;
    let settled = match write {
        TaskOutcomeWrite::Applied { task, .. } | TaskOutcomeWrite::Replayed { task, .. } => task,
        TaskOutcomeWrite::NotFound => return Ok(ExternalJobTaskSettlementOutcome::NotFound),
        TaskOutcomeWrite::Rejected { .. } => {
            return Ok(ExternalJobTaskSettlementOutcome::Stale);
        }
    };
    transition_node_counters_in_tx(
        conn,
        job.run_uid,
        &settled.node_id,
        &settled.item_key,
        ExecutionTaskStatus::WaitingExternal,
        settled.status,
    )
    .await?;
    refresh_run_after_wait_settlement_in_conn(conn, job.run_uid, task.task_id, settled_at).await?;
    Ok(ExternalJobTaskSettlementOutcome::Applied(settled))
}

async fn persist_task_attempt_checkpoint_in_conn(
    conn: &mut ScopedConn<'_>,
    request: &NewTaskAttemptCheckpoint,
) -> Result<TaskAttemptCheckpointWriteOutcome> {
    persist_task_attempt_checkpoint_for_state_in_conn(
        conn,
        request,
        ExecutionAttemptState::Cancelling,
    )
    .await
}

async fn persist_task_attempt_checkpoint_for_state_in_conn(
    conn: &mut ScopedConn<'_>,
    request: &NewTaskAttemptCheckpoint,
    expected_attempt_state: ExecutionAttemptState,
) -> Result<TaskAttemptCheckpointWriteOutcome> {
    if request.task_generation == 0 || request.schema_version == 0 || !request.payload.is_object() {
        return Ok(TaskAttemptCheckpointWriteOutcome::InvalidState);
    }
    let payload_bytes = canonical_json_bytes(&request.payload).map_err(Error::from)?;
    if payload_bytes.len() > 1024 * 1024 {
        return Ok(TaskAttemptCheckpointWriteOutcome::InvalidState);
    }
    let payload_hash = blake3::hash(&payload_bytes).to_hex().to_string();
    let workspace_release_receipt = request
        .workspace_release_receipt
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    if workspace_release_receipt.as_ref().is_some_and(|receipt| {
        serde_json::to_vec(receipt)
            .map(|bytes| bytes.len() > 256 * 1024)
            .unwrap_or(true)
    }) {
        return Ok(TaskAttemptCheckpointWriteOutcome::InvalidState);
    }
    if request
        .workspace_release_receipt
        .as_ref()
        .is_some_and(|receipt| {
            receipt.tenant_id != request.fence.tenant_id
                || receipt.run_id.0 != request.fence.run_uid
                || !matches!(
                    receipt.owner,
                    ExecutionHandReleaseOwner::Task { task_id, logical_generation }
                        if task_id.0 == request.fence.task_id.as_uuid()
                            && logical_generation == request.task_generation
                )
                || receipt.attempt_generation != request.fence.attempt_generation
        })
    {
        return Ok(TaskAttemptCheckpointWriteOutcome::Stale);
    }
    let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(request.fence.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(TaskAttemptCheckpointWriteOutcome::NotFound);
    };
    let run = run_from_row(&run_row)?;
    let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
        .bind(request.fence.run_uid)
        .bind(request.fence.task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(TaskAttemptCheckpointWriteOutcome::NotFound);
    };
    let task = task_from_row(&task_row)?;
    if !task_attempt_fence_matches(&run, &task, &request.fence)
        || task.generation != request.task_generation
    {
        return Ok(TaskAttemptCheckpointWriteOutcome::Stale);
    }
    if task.status != ExecutionTaskStatus::Running || task.attempt_state != expected_attempt_state {
        return Ok(TaskAttemptCheckpointWriteOutcome::InvalidState);
    }

    let current = sqlx::query(
        "SELECT * FROM moa.execution_task_checkpoint \
         WHERE tenant_id = $1 AND run_uid = $2 AND task_id = $3 \
           AND superseded_at IS NULL FOR UPDATE",
    )
    .bind(request.fence.tenant_id.0)
    .bind(request.fence.run_uid)
    .bind(request.fence.task_id.as_uuid())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if let Some(current) = current {
        let current = task_checkpoint_from_row(&current)?;
        let replay = current.controller_generation == request.fence.controller_generation
            && current.task_generation == request.task_generation
            && current.attempt_generation == request.fence.attempt_generation
            && current.dispatch_uid == request.fence.dispatch_uid
            && current.kind == request.kind
            && current.schema_version == request.schema_version
            && current.payload_hash == payload_hash
            && current.workspace_release_receipt == request.workspace_release_receipt;
        if replay {
            return Ok(TaskAttemptCheckpointWriteOutcome::Replayed(Box::new(
                current,
            )));
        }
        sqlx::query(
            "UPDATE moa.execution_task_checkpoint SET superseded_at = $4 \
             WHERE tenant_id = $1 AND run_uid = $2 AND task_id = $3 \
               AND superseded_at IS NULL",
        )
        .bind(request.fence.tenant_id.0)
        .bind(request.fence.run_uid)
        .bind(request.fence.task_id.as_uuid())
        .bind(request.created_at)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    }
    let checkpoint_sequence = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(checkpoint_sequence), 0) + 1 \
         FROM moa.execution_task_checkpoint WHERE tenant_id = $1 AND run_uid = $2 AND task_id = $3",
    )
    .bind(request.fence.tenant_id.0)
    .bind(request.fence.run_uid)
    .bind(request.fence.task_id.as_uuid())
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let checkpoint_sequence = to_u64(checkpoint_sequence, "task checkpoint sequence")?;
    let checkpoint_uid = Uuid::new_v5(
        &request.fence.task_id.as_uuid(),
        format!("task-checkpoint-v1:{checkpoint_sequence}:{payload_hash}").as_bytes(),
    );
    let row = sqlx::query(
        "INSERT INTO moa.execution_task_checkpoint (\
             checkpoint_uid, tenant_id, run_uid, task_id, checkpoint_sequence, \
             controller_generation, task_generation, attempt_generation, dispatch_uid, \
             checkpoint_kind, schema_version, payload, payload_hash, \
             workspace_release_receipt, created_at\
         ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) RETURNING *",
    )
    .bind(checkpoint_uid)
    .bind(request.fence.tenant_id.0)
    .bind(request.fence.run_uid)
    .bind(request.fence.task_id.as_uuid())
    .bind(to_i64(checkpoint_sequence, "task checkpoint sequence")?)
    .bind(to_i64(
        request.fence.controller_generation,
        "controller generation",
    )?)
    .bind(to_i64(request.task_generation, "task generation")?)
    .bind(to_i64(
        request.fence.attempt_generation,
        "attempt generation",
    )?)
    .bind(request.fence.dispatch_uid)
    .bind(request.kind.as_str())
    .bind(i64::from(request.schema_version))
    .bind(&request.payload)
    .bind(payload_hash)
    .bind(workspace_release_receipt)
    .bind(request.created_at)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    Ok(TaskAttemptCheckpointWriteOutcome::Applied(Box::new(
        task_checkpoint_from_row(&row)?,
    )))
}

pub(super) fn external_start_checkpoint_payload_is_provisional(
    kind: TaskAttemptCheckpointKind,
    payload: &Value,
) -> bool {
    let Some(state) = payload.get("state").and_then(Value::as_object) else {
        return false;
    };
    match kind {
        TaskAttemptCheckpointKind::AgentContinuation => {
            state.get("kind").and_then(Value::as_str) == Some("agent")
                && state
                    .get("pending_external")
                    .and_then(Value::as_object)
                    .is_some_and(|pending| {
                        pending.get("external_job_uid").is_some_and(Value::is_null)
                            && pending.get("invocation").is_some_and(Value::is_object)
                    })
        }
        TaskAttemptCheckpointKind::CapabilityExternalStart => {
            state.get("kind").and_then(Value::as_str) == Some("capability_external_start")
                && state.get("tool_id").and_then(Value::as_str).is_some()
        }
        TaskAttemptCheckpointKind::CapabilityReview => false,
    }
}

async fn insert_resolved_task_checkpoint_in_conn(
    conn: &mut ScopedConn<'_>,
    current: &TaskAttemptCheckpointRecord,
    controller_generation: u64,
    task_generation: u64,
    attempt_generation: u64,
    payload: Value,
    resolved_at: DateTime<Utc>,
) -> Result<TaskAttemptCheckpointRecord> {
    let canonical = canonical_json_bytes(&payload).map_err(Error::from)?;
    if canonical.len() > 1024 * 1024 {
        return Err(Error::InvalidRepositoryInput {
            message: "resolved task continuation exceeds one MiB".to_string(),
        });
    }
    let payload_hash = blake3::hash(&canonical).to_hex().to_string();
    let updated = sqlx::query(
        "UPDATE moa.execution_task_checkpoint SET superseded_at=$4 \
         WHERE tenant_id=$1 AND run_uid=$2 AND task_id=$3 AND checkpoint_uid=$5 \
           AND superseded_at IS NULL",
    )
    .bind(current.tenant_id.0)
    .bind(current.run_uid)
    .bind(current.task_id.as_uuid())
    .bind(resolved_at)
    .bind(current.checkpoint_uid)
    .execute(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    if updated.rows_affected() != 1 {
        return Err(Error::InvalidRepositoryData {
            message: "current task review checkpoint was concurrently superseded".to_string(),
        });
    }
    let checkpoint_sequence = current.checkpoint_sequence.checked_add(1).ok_or_else(|| {
        Error::InvalidRepositoryInput {
            message: "task checkpoint sequence overflow".to_string(),
        }
    })?;
    let checkpoint_uid = Uuid::new_v5(
        &current.task_id.as_uuid(),
        format!("task-checkpoint-v1:{checkpoint_sequence}:{payload_hash}").as_bytes(),
    );
    let workspace_release_receipt = current
        .workspace_release_receipt
        .as_ref()
        .map(serde_json::to_value)
        .transpose()?;
    let row = sqlx::query(
        "INSERT INTO moa.execution_task_checkpoint (checkpoint_uid,tenant_id,run_uid,task_id, \
         checkpoint_sequence,controller_generation,task_generation,attempt_generation, \
         dispatch_uid,checkpoint_kind,schema_version,payload,payload_hash, \
         workspace_release_receipt,created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15) RETURNING *",
    )
    .bind(checkpoint_uid)
    .bind(current.tenant_id.0)
    .bind(current.run_uid)
    .bind(current.task_id.as_uuid())
    .bind(to_i64(checkpoint_sequence, "task checkpoint sequence")?)
    .bind(to_i64(controller_generation, "controller generation")?)
    .bind(to_i64(task_generation, "task generation")?)
    .bind(to_i64(attempt_generation, "attempt generation")?)
    .bind(current.dispatch_uid)
    .bind(current.kind.as_str())
    .bind(i64::from(current.schema_version))
    .bind(payload)
    .bind(payload_hash)
    .bind(workspace_release_receipt)
    .bind(resolved_at)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    task_checkpoint_from_row(&row)
}

fn append_agent_resume_input(payload: &mut Value, input: &Value) -> Result<()> {
    let state = payload
        .get_mut("state")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "agent continuation checkpoint is missing its state object".to_string(),
        })?;
    if state.get("kind").and_then(Value::as_str) != Some("agent") {
        return Err(Error::InvalidRepositoryData {
            message: "waiting-input checkpoint is not an agent continuation".to_string(),
        });
    }
    let messages = state
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "agent continuation checkpoint is missing its message history".to_string(),
        })?;
    let content = match input {
        Value::String(content) => content.clone(),
        other => String::from_utf8(canonical_json_bytes(other).map_err(Error::from)?).map_err(
            |error| Error::InvalidRepositoryData {
                message: format!("canonical input is not UTF-8: {error}"),
            },
        )?,
    };
    messages.push(serde_json::to_value(ContextMessage::user(content))?);
    Ok(())
}

pub(super) fn task_checkpoint_from_row(row: &PgRow) -> Result<TaskAttemptCheckpointRecord> {
    let payload: Value = row.try_get("payload").map_err(row_error)?;
    let payload_hash: String = row.try_get("payload_hash").map_err(row_error)?;
    let canonical = canonical_json_bytes(&payload).map_err(Error::from)?;
    if blake3::hash(&canonical).to_hex().as_str() != payload_hash {
        return Err(Error::InvalidRepositoryData {
            message: "task-attempt checkpoint payload hash mismatch".to_string(),
        });
    }
    let workspace_release_receipt = row
        .try_get::<Option<Value>, _>("workspace_release_receipt")
        .map_err(row_error)?
        .map(serde_json::from_value)
        .transpose()?;
    Ok(TaskAttemptCheckpointRecord {
        checkpoint_uid: row.try_get("checkpoint_uid").map_err(row_error)?,
        checkpoint_sequence: required_u64(row, "checkpoint_sequence")?,
        tenant_id: TenantId(row.try_get("tenant_id").map_err(row_error)?),
        run_uid: row.try_get("run_uid").map_err(row_error)?,
        task_id: ExecutionTaskId::from_uuid(row.try_get("task_id").map_err(row_error)?),
        controller_generation: required_u64(row, "controller_generation")?,
        task_generation: required_u64(row, "task_generation")?,
        attempt_generation: required_u64(row, "attempt_generation")?,
        dispatch_uid: row.try_get("dispatch_uid").map_err(row_error)?,
        kind: TaskAttemptCheckpointKind::parse(
            &row.try_get::<String, _>("checkpoint_kind")
                .map_err(row_error)?,
        )?,
        schema_version: u32::try_from(row.try_get::<i64, _>("schema_version").map_err(row_error)?)
            .map_err(|_| Error::InvalidRepositoryData {
                message: "task checkpoint schema version is outside u32".to_string(),
            })?,
        payload,
        payload_hash,
        workspace_release_receipt,
        created_at: row.try_get("created_at").map_err(row_error)?,
    })
}

pub(super) fn checkpoint_review_uid(payload: &Value) -> Option<Uuid> {
    payload
        .get("state")?
        .get("pending_review")?
        .get("review_uid")?
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn checkpoint_review_waiting_reason(
    checkpoint: &TaskAttemptCheckpointRecord,
    task: &ExecutionTaskRecord,
) -> Result<WaitingReason> {
    let pending = checkpoint
        .payload
        .get("state")
        .and_then(|state| state.get("pending_review"))
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "review checkpoint is missing its pending review".to_string(),
        })?;
    let expires_at = pending
        .get("expires_at")
        .cloned()
        .map(serde_json::from_value)
        .transpose()?
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "review checkpoint is missing its expiry".to_string(),
        })?;
    let invocation_name = pending
        .get("invocation")
        .and_then(|invocation| invocation.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| Error::InvalidRepositoryData {
            message: "review checkpoint is missing its invocation name".to_string(),
        })?;
    Ok(WaitingReason::Review {
        task_id: task.task_id,
        prompt: format!("Review governed capability `{invocation_name}`"),
        wait_policy: moa_artifacts::execution_plan::ExecutionWaitPolicy {
            expiry: moa_artifacts::execution_plan::ExecutionTemporalTarget::At { at: expires_at },
            on_expiry: moa_artifacts::execution_plan::ExecutionWaitExpiryAction::FailTask,
        },
    })
}

fn checkpoint_review_resolution(payload: &Value) -> Option<&Value> {
    payload.get("review_resolution")
}

fn checkpoint_external_job_uid(payload: &Value) -> Option<Uuid> {
    payload
        .get("state")?
        .get("pending_external")?
        .get("external_job_uid")?
        .as_str()
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn task_attempt_fence_matches(
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    fence: &TaskAttemptFence,
) -> bool {
    run.controller_generation == fence.controller_generation
        && task_attempt_resource_fence_matches(run, task, fence)
}

fn task_attempt_resource_fence_matches(
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    fence: &TaskAttemptFence,
) -> bool {
    run.run_uid == fence.run_uid
        && run.tenant_id == fence.tenant_id
        && task.run_uid == fence.run_uid
        && task.tenant_id == fence.tenant_id
        && task.task_id == fence.task_id
        && task.attempt_generation == fence.attempt_generation
        && task.active_dispatch_uid == Some(fence.dispatch_uid)
        && task.attempt_deadline_at == Some(fence.attempt_deadline_at)
}

fn unstarted_task_attempt_fence_matches(
    run: &ExecutionRunRecord,
    task: &ExecutionTaskRecord,
    fence: &TaskAttemptFence,
    disposition: &UnstartedTaskAttemptDisposition,
) -> bool {
    unstarted_task_attempt_run_fence_matches(run, fence, disposition)
        && task_attempt_resource_fence_matches(run, task, fence)
}

fn unstarted_task_attempt_run_fence_matches(
    run: &ExecutionRunRecord,
    fence: &TaskAttemptFence,
    disposition: &UnstartedTaskAttemptDisposition,
) -> bool {
    let controller_generation = match disposition {
        UnstartedTaskAttemptDisposition::Paused {
            controller_generation,
        } => *controller_generation,
        UnstartedTaskAttemptDisposition::Cancelled { .. }
        | UnstartedTaskAttemptDisposition::DispatchDeliveryLost => fence.controller_generation,
    };
    run.run_uid == fence.run_uid
        && run.tenant_id == fence.tenant_id
        && run.controller_generation == controller_generation
}

fn task_attempt_settlement_replayed(
    task: &ExecutionTaskRecord,
    fence: &TaskAttemptFence,
    outcome: &ExecutionTaskOutcome,
) -> bool {
    let dispatch_uid = fence.dispatch_uid.to_string();
    task.active_dispatch_uid.is_none()
        && task.current_outcome.as_ref() == Some(outcome)
        && task.generation_history.iter().any(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("bounded_attempt_settlement")
                && entry.get("dispatch_uid").and_then(Value::as_str) == Some(dispatch_uid.as_str())
                && entry.get("attempt_generation").and_then(Value::as_u64)
                    == Some(fence.attempt_generation)
        })
}

/// Atomically repairs one admitted dispatch whose receiver never committed its start fence.
///
/// The caller owns the transaction and must commit only `Applied` or `Replayed`; every other
/// outcome may follow a tentative capacity release and therefore requires rollback.
pub(super) async fn settle_unstarted_task_attempt_in_conn(
    conn: &mut ScopedConn<'_>,
    fence: TaskAttemptFence,
    settled_at: DateTime<Utc>,
) -> Result<TaskAttemptSettlementOutcome> {
    let disposition = UnstartedTaskAttemptDisposition::DispatchDeliveryLost;
    let capacity = release_task_capacity_in_tx(
        conn,
        fence.capacity_reservation_uid,
        fence.run_uid,
        fence.task_id,
        fence.attempt_generation,
    )
    .await?;
    if capacity == CapacityReleaseOutcome::NotFound {
        return Ok(TaskAttemptSettlementOutcome::NotFound);
    }
    if capacity == CapacityReleaseOutcome::Stale {
        return Ok(TaskAttemptSettlementOutcome::Stale);
    }
    let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(fence.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(TaskAttemptSettlementOutcome::NotFound);
    };
    let run = run_from_row(&run_row)?;
    let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(TaskAttemptSettlementOutcome::NotFound);
    };
    let task = task_from_row(&task_row)?;
    if capacity == CapacityReleaseOutcome::AlreadyReleased {
        return Ok(
            if unstarted_task_attempt_settlement_replayed(&task, &fence, &disposition) {
                TaskAttemptSettlementOutcome::Replayed { run, task }
            } else {
                TaskAttemptSettlementOutcome::Stale
            },
        );
    }
    if !task_attempt_fence_matches(&run, &task, &fence) || run.status.is_terminal() {
        return Ok(TaskAttemptSettlementOutcome::Stale);
    }
    if task.status != ExecutionTaskStatus::Dispatching
        || task.attempt_state != ExecutionAttemptState::Dispatching
    {
        return Ok(TaskAttemptSettlementOutcome::InvalidState);
    }
    if supersede_trigger_in_conn(
        conn.as_mut(),
        fence.watchdog_trigger_uid,
        ExecutionTriggerKind::TaskWatchdog,
        Some(fence.controller_generation),
        Some(fence.attempt_generation),
        None,
        None,
    )
    .await?
        == ExecutionTriggerSupersedeOutcome::StaleOrMissing
    {
        return Ok(TaskAttemptSettlementOutcome::Stale);
    }
    let next_attempt_generation =
        fence
            .attempt_generation
            .checked_add(1)
            .ok_or_else(|| Error::InvalidRepositoryData {
                message: "task attempt generation overflow".to_string(),
            })?;
    let history = json!({
        "kind": "unstarted_attempt_settlement",
        "dispatch_uid": fence.dispatch_uid,
        "attempt_generation": fence.attempt_generation,
        "disposition": unstarted_disposition_label(&disposition),
        "reason": Value::Null,
        "recorded_at": settled_at,
    });
    let row = sqlx::query(
        "UPDATE moa.execution_task SET status='ready', attempt_state='idle', \
             attempt_generation=$5, active_dispatch_uid=NULL, attempt_deadline_at=NULL, \
             progress_step_bound_seconds=NULL, \
             ready_at=$6, waiting_since=NULL, generation_history=generation_history || \
             jsonb_build_array($7::JSONB), \
             last_progress_at=GREATEST(last_progress_at,$6), updated_at=NOW() \
         WHERE run_uid=$1 AND task_id=$2 AND attempt_generation=$3 \
           AND active_dispatch_uid=$4 AND status='dispatching' \
           AND attempt_state='dispatching' RETURNING *",
    )
    .bind(fence.run_uid)
    .bind(fence.task_id.as_uuid())
    .bind(to_i64(fence.attempt_generation, "attempt generation")?)
    .bind(fence.dispatch_uid)
    .bind(to_i64(next_attempt_generation, "next attempt generation")?)
    .bind(settled_at)
    .bind(history)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(row) = row else {
        return Ok(TaskAttemptSettlementOutcome::Stale);
    };
    let task = task_from_row(&row)?;
    transition_node_counters_in_tx(
        conn,
        fence.run_uid,
        &task.node_id,
        &task.item_key,
        ExecutionTaskStatus::Dispatching,
        ExecutionTaskStatus::Ready,
    )
    .await?;
    enqueue_run_activation_in_conn(
        conn.as_mut(),
        fence.tenant_id,
        fence.run_uid,
        fence.controller_generation,
        settled_at,
        json!({
            "source": "unstarted_task_attempt_settlement",
            "task_id": fence.task_id,
            "dispatch_uid": fence.dispatch_uid,
            "attempt_generation": fence.attempt_generation,
            "disposition": unstarted_disposition_label(&disposition),
        }),
    )
    .await?;
    let run_row = sqlx::query(LOAD_RUN_SQL)
        .bind(fence.run_uid)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
    Ok(TaskAttemptSettlementOutcome::Applied {
        run: run_from_row(&run_row)?,
        task,
    })
}

fn unstarted_disposition_label(disposition: &UnstartedTaskAttemptDisposition) -> &'static str {
    match disposition {
        UnstartedTaskAttemptDisposition::Cancelled { .. } => "cancelled",
        UnstartedTaskAttemptDisposition::Paused { .. } => "paused",
        UnstartedTaskAttemptDisposition::DispatchDeliveryLost => "dispatch_delivery_lost",
    }
}

fn unstarted_task_attempt_settlement_replayed(
    task: &ExecutionTaskRecord,
    fence: &TaskAttemptFence,
    disposition: &UnstartedTaskAttemptDisposition,
) -> bool {
    unstarted_task_attempt_history_matches(&task.generation_history, fence, disposition)
}

fn unstarted_task_attempt_history_matches(
    generation_history: &[Value],
    fence: &TaskAttemptFence,
    disposition: &UnstartedTaskAttemptDisposition,
) -> bool {
    let dispatch_uid = fence.dispatch_uid.to_string();
    let expected_reason = match disposition {
        UnstartedTaskAttemptDisposition::Cancelled { reason } => Some(reason.as_str()),
        UnstartedTaskAttemptDisposition::Paused { .. }
        | UnstartedTaskAttemptDisposition::DispatchDeliveryLost => None,
    };
    generation_history.iter().any(|entry| {
        entry.get("kind").and_then(Value::as_str) == Some("unstarted_attempt_settlement")
            && entry.get("dispatch_uid").and_then(Value::as_str) == Some(dispatch_uid.as_str())
            && entry.get("attempt_generation").and_then(Value::as_u64)
                == Some(fence.attempt_generation)
            && entry.get("disposition").and_then(Value::as_str)
                == Some(unstarted_disposition_label(disposition))
            && entry.get("reason").and_then(Value::as_str) == expected_reason
            && match disposition {
                UnstartedTaskAttemptDisposition::Paused {
                    controller_generation,
                } => {
                    entry.get("controller_generation").and_then(Value::as_u64)
                        == Some(*controller_generation)
                }
                UnstartedTaskAttemptDisposition::Cancelled { .. }
                | UnstartedTaskAttemptDisposition::DispatchDeliveryLost => true,
            }
    })
}

fn paused_task_attempt_release_history_matches(
    generation_history: &[Value],
    fence: &TaskAttemptFence,
    controller_generation: u64,
) -> bool {
    let dispatch_uid = fence.dispatch_uid.to_string();
    generation_history.iter().any(|entry| {
        entry.get("kind").and_then(Value::as_str) == Some("pause_release_finalized")
            && entry.get("dispatch_uid").and_then(Value::as_str) == Some(dispatch_uid.as_str())
            && entry.get("attempt_generation").and_then(Value::as_u64)
                == Some(fence.attempt_generation)
            && entry
                .get("attempt_controller_generation")
                .and_then(Value::as_u64)
                == Some(fence.controller_generation)
            && entry.get("controller_generation").and_then(Value::as_u64)
                == Some(controller_generation)
    })
}

fn storage_only_task_kind(kind: &LogicalTaskKind) -> bool {
    matches!(
        kind,
        LogicalTaskKind::Review { .. }
            | LogicalTaskKind::WaitSignal { .. }
            | LogicalTaskKind::WaitUntil { .. }
    )
}

async fn load_running_external_start_fence(
    conn: &mut ScopedConn<'_>,
    tenant_id: TenantId,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
    attempt_generation: u64,
) -> Result<Option<TaskAttemptFence>> {
    let payload = sqlx::query_scalar::<_, Value>(
        "SELECT dispatch.payload \
         FROM moa.execution_task_checkpoint AS checkpoint \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.dispatch_uid=checkpoint.dispatch_uid \
          AND dispatch.tenant_id=checkpoint.tenant_id \
          AND dispatch.run_uid=checkpoint.run_uid \
          AND dispatch.task_id=checkpoint.task_id \
         WHERE checkpoint.tenant_id=$1 AND checkpoint.run_uid=$2 \
           AND checkpoint.task_id=$3 AND checkpoint.attempt_generation=$4 \
           AND checkpoint.superseded_at IS NULL \
           AND dispatch.dispatch_kind='task_attempt'",
    )
    .bind(tenant_id.0)
    .bind(run_uid)
    .bind(task_id.as_uuid())
    .bind(to_i64(attempt_generation, "attempt generation")?)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(payload) = payload else {
        return Ok(None);
    };
    let request: ExecutionTaskAttemptRequest =
        serde_json::from_value(payload).map_err(|error| Error::InvalidRepositoryData {
            message: format!("task external-start checkpoint has invalid dispatch: {error}"),
        })?;
    if request.tenant_id != tenant_id
        || request.run_uid != run_uid
        || request.task_id != task_id
        || request.attempt_generation != attempt_generation
    {
        return Err(Error::InvalidRepositoryData {
            message: "task external-start checkpoint dispatch changed immutable owner coordinates"
                .to_string(),
        });
    }
    Ok(Some(TaskAttemptFence {
        tenant_id,
        run_uid,
        task_id,
        controller_generation: request.controller_generation,
        attempt_generation,
        dispatch_uid: request.dispatch_uid,
        capacity_reservation_uid: request.capacity_reservation_uid,
        watchdog_trigger_uid: request.watchdog_trigger_uid,
        attempt_deadline_at: request.attempt_deadline_at,
    }))
}

async fn load_locked_external_start_owner(
    conn: &mut ScopedConn<'_>,
    fence: TaskAttemptFence,
) -> Result<
    Option<(
        ExecutionRunRecord,
        ExecutionTaskRecord,
        TaskAttemptCheckpointRecord,
    )>,
> {
    let Some(run_row) = sqlx::query(LOAD_RUN_FOR_UPDATE_SQL)
        .bind(fence.run_uid)
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(None);
    };
    let run = run_from_row(&run_row)?;
    let Some(task_row) = sqlx::query(LOAD_TASK_FOR_UPDATE_SQL)
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?
    else {
        return Ok(None);
    };
    let task = task_from_row(&task_row)?;
    let checkpoint = sqlx::query(
        "SELECT * FROM moa.execution_task_checkpoint \
         WHERE tenant_id=$1 AND run_uid=$2 AND task_id=$3 \
           AND attempt_generation=$4 AND dispatch_uid=$5 AND superseded_at IS NULL \
         FOR UPDATE",
    )
    .bind(fence.tenant_id.0)
    .bind(fence.run_uid)
    .bind(fence.task_id.as_uuid())
    .bind(to_i64(fence.attempt_generation, "attempt generation")?)
    .bind(fence.dispatch_uid)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(sqlx_error)?;
    let Some(checkpoint) = checkpoint else {
        return Ok(None);
    };
    let checkpoint = task_checkpoint_from_row(&checkpoint)?;
    if run.tenant_id != fence.tenant_id
        || run.controller_generation != fence.controller_generation
        || task.tenant_id != fence.tenant_id
        || checkpoint.controller_generation != fence.controller_generation
        || checkpoint.task_generation != task.generation
        || !external_start_checkpoint_payload_is_provisional(checkpoint.kind, &checkpoint.payload)
    {
        return Ok(None);
    }
    Ok(Some((run, task, checkpoint)))
}

fn bind_recovered_external_job_in_checkpoint(
    payload: &mut Value,
    kind: TaskAttemptCheckpointKind,
    external_job_uid: Uuid,
) -> Result<()> {
    let external_job_uid_text = external_job_uid.to_string();
    match kind {
        TaskAttemptCheckpointKind::AgentContinuation => {
            let pending = payload
                .get_mut("state")
                .and_then(|state| state.get_mut("pending_external"))
                .and_then(Value::as_object_mut)
                .ok_or_else(|| Error::InvalidRepositoryData {
                    message: "agent external-start checkpoint lost its pending invocation"
                        .to_string(),
                })?;
            match pending.get("external_job_uid") {
                Some(value) if value.is_null() => {
                    pending.insert("external_job_uid".to_string(), json!(external_job_uid));
                }
                Some(value) if value.as_str() == Some(external_job_uid_text.as_str()) => {}
                _ => {
                    return Err(Error::InvalidRepositoryData {
                        message: "agent external-start checkpoint is bound to another job"
                            .to_string(),
                    });
                }
            }
            Ok(())
        }
        TaskAttemptCheckpointKind::CapabilityExternalStart => Ok(()),
        TaskAttemptCheckpointKind::CapabilityReview => Err(Error::InvalidRepositoryData {
            message: "review checkpoint cannot adopt a direct recovered external start".to_string(),
        }),
    }
}

async fn task_attempt_resources_match(
    conn: &mut ScopedConn<'_>,
    fence: &TaskAttemptFence,
) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS ( \
             SELECT 1 FROM moa.execution_capacity_reservation \
             WHERE reservation_uid = $1 AND tenant_id = $2 AND run_uid = $3 AND task_id = $4 \
               AND controller_generation = $5 AND attempt_generation = $6 \
               AND resource_dimension = 'active_tasks' AND state = 'reserved' \
         ) AND EXISTS ( \
             SELECT 1 FROM moa.execution_trigger \
             WHERE trigger_uid = $7 AND tenant_id = $2 AND run_uid = $3 AND task_id = $4 \
               AND trigger_kind = 'task_watchdog' AND controller_generation = $5 \
               AND attempt_generation = $6 AND state = 'pending' \
         )",
    )
    .bind(fence.capacity_reservation_uid)
    .bind(fence.tenant_id.0)
    .bind(fence.run_uid)
    .bind(fence.task_id.as_uuid())
    .bind(to_i64(
        fence.controller_generation,
        "controller generation",
    )?)
    .bind(to_i64(fence.attempt_generation, "attempt generation")?)
    .bind(fence.watchdog_trigger_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)
}

async fn persisted_task_release_receipt_matches(
    conn: &mut ScopedConn<'_>,
    fence: &TaskAttemptFence,
    logical_generation: u64,
    receipt: &ExecutionHandReleaseReceipt,
) -> Result<bool> {
    let ExecutionHandReleaseOwner::Task {
        task_id,
        logical_generation: receipt_logical_generation,
    } = &receipt.owner
    else {
        return Ok(false);
    };
    if receipt.tenant_id != fence.tenant_id
        || receipt.run_id.0 != fence.run_uid
        || task_id.0 != fence.task_id.as_uuid()
        || *receipt_logical_generation != logical_generation
        || receipt.attempt_generation != fence.attempt_generation
    {
        return Ok(false);
    }
    let verified_absence = task_release_receipt_is_verified_absence(receipt);
    if verified_absence {
        return sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.sandbox_execution_hand_release_receipts \
             WHERE receipt_id=$1 AND tenant_id=$2 AND run_uid=$3 AND owner_kind='task' \
               AND task_id=$4 AND compensation_id IS NULL AND logical_generation=$5 \
               AND attempt_generation=$6 AND receipt_state='released' \
               AND destroy_outcome='verified_absent' AND released_at IS NOT NULL \
               AND workspace_id IS NULL AND writer_epoch IS NULL \
               AND instance_generation IS NULL \
               AND hand_provisioning_operation_id IS NULL \
               AND hand_lease_generation IS NULL AND checkpoint_id IS NULL \
               AND checkpoint_generation IS NULL \
               AND checkpoint_manifest_digest IS NULL \
               AND checkpoint_logical_bytes IS NULL)",
        )
        .bind(receipt.receipt_id)
        .bind(fence.tenant_id.0)
        .bind(fence.run_uid)
        .bind(fence.task_id.as_uuid())
        .bind(to_i64(
            logical_generation,
            "release receipt logical generation",
        )?)
        .bind(to_i64(
            fence.attempt_generation,
            "release receipt attempt generation",
        )?)
        .fetch_one(conn.as_mut())
        .await
        .map_err(sqlx_error);
    }
    let (
        Some(workspace_id),
        Some(writer_epoch),
        Some(instance_generation),
        Some(hand_provisioning_operation_id),
        Some(hand_lease_generation),
        Some(checkpoint_id),
        Some(checkpoint_generation),
        Some(checkpoint_manifest_digest),
        Some(checkpoint_logical_bytes),
    ) = (
        receipt.workspace_id,
        receipt.writer_epoch,
        receipt.instance_generation,
        receipt.hand_provisioning_operation_id,
        receipt.hand_lease_generation,
        receipt.checkpoint_id,
        receipt.checkpoint_generation,
        receipt.checkpoint_manifest_digest.as_deref(),
        receipt.checkpoint_logical_bytes,
    )
    else {
        return Ok(false);
    };
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM moa.sandbox_execution_hand_release_receipts \
         WHERE receipt_id=$1 AND tenant_id=$2 AND run_uid=$3 AND owner_kind='task' \
           AND task_id=$4 AND compensation_id IS NULL AND logical_generation=$5 \
           AND attempt_generation=$6 AND receipt_state='released' \
           AND destroy_outcome='verified_absent' AND released_at IS NOT NULL \
           AND workspace_id=$7 AND writer_epoch=$8 AND instance_generation=$9 \
           AND hand_provisioning_operation_id=$10 AND hand_lease_generation=$11 \
           AND checkpoint_id=$12 AND checkpoint_generation=$13 \
           AND checkpoint_manifest_digest=$14 AND checkpoint_logical_bytes=$15)",
    )
    .bind(receipt.receipt_id)
    .bind(fence.tenant_id.0)
    .bind(fence.run_uid)
    .bind(fence.task_id.as_uuid())
    .bind(to_i64(
        logical_generation,
        "release receipt logical generation",
    )?)
    .bind(to_i64(
        fence.attempt_generation,
        "release receipt attempt generation",
    )?)
    .bind(workspace_id.0)
    .bind(to_i64(writer_epoch, "release receipt writer epoch")?)
    .bind(to_i64(
        instance_generation,
        "release receipt instance generation",
    )?)
    .bind(hand_provisioning_operation_id.0)
    .bind(to_i64(
        hand_lease_generation,
        "release receipt hand lease generation",
    )?)
    .bind(checkpoint_id.0)
    .bind(to_i64(
        checkpoint_generation,
        "release receipt checkpoint generation",
    )?)
    .bind(checkpoint_manifest_digest)
    .bind(to_i64(
        checkpoint_logical_bytes,
        "release receipt checkpoint logical bytes",
    )?)
    .fetch_one(conn.as_mut())
    .await
    .map_err(sqlx_error)
}

fn task_release_receipt_is_verified_absence(receipt: &ExecutionHandReleaseReceipt) -> bool {
    receipt.workspace_id.is_none()
        && receipt.writer_epoch.is_none()
        && receipt.instance_generation.is_none()
        && receipt.hand_provisioning_operation_id.is_none()
        && receipt.hand_lease_generation.is_none()
        && receipt.checkpoint_id.is_none()
        && receipt.checkpoint_generation.is_none()
        && receipt.checkpoint_manifest_digest.is_none()
        && receipt.checkpoint_logical_bytes.is_none()
}

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
        if task.generation != generation || task.attempt_generation != generation {
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
        if task.generation != generation || task.attempt_generation != generation {
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
        self.resume_task_inner(ResumeTaskRequest {
            scope,
            config: None,
            run_uid,
            task_id,
            generation,
            kind,
            resume_input: None,
        })
        .await
    }

    /// Resumes one waiting-input task and atomically appends the exact supplied payload.
    pub async fn resume_task_with_input(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        input: Value,
    ) -> Result<TransitionOutcome> {
        self.resume_task_inner(ResumeTaskRequest {
            scope,
            config: Some(config),
            run_uid,
            task_id,
            generation,
            kind: ResumeKind::Input,
            resume_input: Some(input),
        })
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
        self.resume_task_inner(ResumeTaskRequest {
            scope,
            config: None,
            run_uid,
            task_id,
            generation,
            kind: ResumeKind::Retry,
            resume_input: None,
        })
        .await
    }

    async fn resume_task_inner(&self, request: ResumeTaskRequest<'_>) -> Result<TransitionOutcome> {
        let ResumeTaskRequest {
            scope,
            config,
            run_uid,
            task_id,
            generation,
            kind,
            resume_input,
        } = request;
        let mut conn = scope.begin(&self.pool).await?;
        let locked_wait_trigger_uid = if kind == ResumeKind::Input {
            let config = config.ok_or_else(|| Error::InvalidRepositoryInput {
                message: "input resume requires validated execution capacity configuration"
                    .to_string(),
            })?;
            let tenant_id = sqlx::query_scalar::<_, Uuid>(
                "SELECT tenant_id FROM moa.execution_task WHERE run_uid=$1 AND task_id=$2",
            )
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            let Some(tenant_id) = tenant_id else {
                conn.commit().await.map_err(storage_error)?;
                return Ok(TransitionOutcome::NotFound);
            };
            prelock_capacity_dimensions_in_tx(
                conn.as_mut(),
                config,
                TenantId(tenant_id),
                &[
                    ExecutionCapacityDimension::ActiveRuns,
                    ExecutionCapacityDimension::ParkedRuns,
                    ExecutionCapacityDimension::ScheduledTriggers,
                ],
            )
            .await?;
            let trigger_uids = sqlx::query_scalar::<_, Uuid>(
                "SELECT trigger_uid FROM moa.execution_trigger \
                 WHERE run_uid=$1 AND task_id=$2 AND trigger_kind='wait_expiry' \
                   AND state = 'pending' \
                 ORDER BY trigger_uid LIMIT 2 FOR UPDATE",
            )
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_all(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if trigger_uids.len() > 1 {
                return Err(Error::InvalidRepositoryData {
                    message: "waiting-input task owns multiple active expiry triggers".to_string(),
                });
            }
            trigger_uids.into_iter().next()
        } else {
            None
        };
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
        if task.generation != generation
            || (kind == ResumeKind::Retry && task.attempt_generation != generation)
        {
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
                    ExecutionRunStatus::WaitingInput
                        | ExecutionRunStatus::Running
                        | ExecutionRunStatus::PauseRequested
                        | ExecutionRunStatus::Pausing
                        | ExecutionRunStatus::Paused
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
        let Some(next_attempt_generation) = task.attempt_generation.checked_add(1) else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TransitionOutcome::Rejected(
                TransitionRejection::CounterOverflow,
            ));
        };
        let resumed_at = Utc::now();
        let input_audience = if kind == ResumeKind::Input {
            Some(
                task.current_outcome
                    .as_ref()
                    .and_then(|outcome| match &outcome.result {
                        ExecutionTaskResult::NeedsInput { audience, .. } => Some(audience.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| Error::InvalidRepositoryData {
                        message: "waiting-input task is missing its typed input audience"
                            .to_string(),
                    })?,
            )
        } else {
            None
        };
        if kind == ResumeKind::Input {
            let Some(trigger_uid) = locked_wait_trigger_uid else {
                return Err(Error::InvalidRepositoryData {
                    message: "waiting-input task is missing its active expiry trigger".to_string(),
                });
            };
            match supersede_trigger_in_conn(
                conn.as_mut(),
                trigger_uid,
                ExecutionTriggerKind::WaitExpiry,
                Some(run.controller_generation),
                Some(task.generation),
                None,
                None,
            )
            .await?
            {
                ExecutionTriggerSupersedeOutcome::Superseded
                | ExecutionTriggerSupersedeOutcome::AlreadySuperseded
                | ExecutionTriggerSupersedeOutcome::AlreadyInactive => {}
                ExecutionTriggerSupersedeOutcome::StaleOrMissing => {
                    return Err(Error::InvalidRepositoryData {
                        message: "waiting-input expiry trigger lost its generation fence"
                            .to_string(),
                    });
                }
            }

            let current_checkpoint = sqlx::query(
                "SELECT * FROM moa.execution_task_checkpoint WHERE tenant_id=$1 AND run_uid=$2 \
                 AND task_id=$3 AND superseded_at IS NULL FOR UPDATE",
            )
            .bind(run.tenant_id.0)
            .bind(run_uid)
            .bind(task_id.as_uuid())
            .fetch_optional(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
            if let Some(current_checkpoint) = current_checkpoint {
                let current_checkpoint = task_checkpoint_from_row(&current_checkpoint)?;
                if current_checkpoint.kind != TaskAttemptCheckpointKind::AgentContinuation
                    || current_checkpoint.task_generation != generation
                {
                    return Err(Error::InvalidRepositoryData {
                        message: "waiting-input agent checkpoint is generation-stale".to_string(),
                    });
                }
                let mut payload = current_checkpoint.payload.clone();
                append_agent_resume_input(
                    &mut payload,
                    resume_input
                        .as_ref()
                        .ok_or_else(|| Error::InvalidRepositoryInput {
                            message: "input resume is missing its payload".to_string(),
                        })?,
                )?;
                insert_resolved_task_checkpoint_in_conn(
                    &mut conn,
                    &current_checkpoint,
                    run.controller_generation,
                    next_generation,
                    next_attempt_generation,
                    payload,
                    resumed_at,
                )
                .await?;
            } else if matches!(task.kind, LogicalTaskKind::Agent { .. }) {
                return Err(Error::InvalidRepositoryData {
                    message: "waiting-input agent task is missing its durable continuation"
                        .to_string(),
                });
            }
        }
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
            .bind(to_i64(next_attempt_generation, "next attempt generation")?)
            .bind(resumed_at)
            .fetch_one(conn.as_mut())
            .await
            .map_err(sqlx_error)?;
        let resumed_task = task_from_row(&row)?;
        let task = if let Some((class, reason)) = admission_rejection {
            terminalize_redispatch_rejection(
                &mut conn,
                &run,
                &resumed_task,
                history_kind,
                class,
                reason,
            )
            .await?
        } else {
            resumed_task
        };
        if let Some(input_audience) = input_audience.as_ref() {
            transition_node_counters_with_input_audience_in_tx(
                &mut conn,
                run_uid,
                &task.node_id,
                &task.item_key,
                ExecutionTaskStatus::WaitingInput,
                task.status,
                input_audience,
            )
            .await?;
            refresh_run_after_wait_settlement_in_conn(&mut conn, run_uid, task_id, resumed_at)
                .await?;
        } else {
            transition_node_counters_in_tx(
                &mut conn,
                run_uid,
                &task.node_id,
                &task.item_key,
                expected_status,
                task.status,
            )
            .await?;
        }
        if !matches!(
            run.status,
            ExecutionRunStatus::PauseRequested
                | ExecutionRunStatus::Pausing
                | ExecutionRunStatus::Paused
        ) {
            enqueue_run_activation_in_conn(
                conn.as_mut(),
                run.tenant_id,
                run_uid,
                run.controller_generation,
                resumed_at,
                json!({
                    "source": history_kind,
                    "task_id": task_id,
                    "requested_generation": generation,
                    "next_generation": task.generation,
                    "next_attempt_generation": task.attempt_generation,
                }),
            )
            .await?;
        }
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

    /// Records a generation-fenced zero- or nonzero-usage outcome for a parked external wait.
    pub async fn complete_external_wait(
        &self,
        scope: ExecutionScope,
        config: &ExecutionConfig,
        run_uid: Uuid,
        task_id: ExecutionTaskId,
        generation: u64,
        outcome: ExecutionTaskOutcome,
    ) -> Result<TaskOutcomeWrite> {
        let mut conn = scope.begin(&self.pool).await?;
        let tenant_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT tenant_id FROM moa.execution_task WHERE run_uid=$1 AND task_id=$2",
        )
        .bind(run_uid)
        .bind(task_id.as_uuid())
        .fetch_optional(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        let Some(tenant_id) = tenant_id else {
            conn.commit().await.map_err(storage_error)?;
            return Ok(TaskOutcomeWrite::NotFound);
        };
        prelock_capacity_dimensions_in_tx(
            conn.as_mut(),
            config,
            TenantId(tenant_id),
            &[
                ExecutionCapacityDimension::ActiveRuns,
                ExecutionCapacityDimension::ParkedRuns,
                ExecutionCapacityDimension::ScheduledTriggers,
            ],
        )
        .await?;
        let trigger_uids = sqlx::query_scalar::<_, Uuid>(
            "SELECT trigger_uid FROM moa.execution_trigger \
             WHERE run_uid=$1 AND task_id=$2 AND trigger_kind='wait_expiry' \
               AND state = 'pending' \
             ORDER BY trigger_uid LIMIT 2 FOR UPDATE",
        )
        .bind(run_uid)
        .bind(task_id.as_uuid())
        .fetch_all(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if trigger_uids.len() > 1 {
            return Err(Error::InvalidRepositoryData {
                message: "storage wait owns multiple active expiry triggers".to_string(),
            });
        }
        let locked_wait_trigger_uid = trigger_uids.into_iter().next();
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
        if task_outcome_is_exact_replay(&task, generation, &outcome)
            || task.generation != generation
            || !matches!(
                task.status,
                ExecutionTaskStatus::WaitingReview | ExecutionTaskStatus::WaitingSignal
            )
        {
            let write =
                record_task_outcome_in_conn(&mut conn, run_uid, task_id, generation, outcome)
                    .await?;
            conn.commit().await.map_err(storage_error)?;
            return Ok(write);
        }
        let Some(trigger_uid) = locked_wait_trigger_uid else {
            return Err(Error::InvalidRepositoryData {
                message: "storage wait is missing its active expiry trigger".to_string(),
            });
        };
        match supersede_trigger_in_conn(
            conn.as_mut(),
            trigger_uid,
            ExecutionTriggerKind::WaitExpiry,
            Some(run.controller_generation),
            Some(task.generation),
            None,
            None,
        )
        .await?
        {
            ExecutionTriggerSupersedeOutcome::Superseded
            | ExecutionTriggerSupersedeOutcome::AlreadySuperseded
            | ExecutionTriggerSupersedeOutcome::AlreadyInactive => {}
            ExecutionTriggerSupersedeOutcome::StaleOrMissing => {
                return Err(Error::InvalidRepositoryData {
                    message: "storage-wait expiry trigger lost its generation fence".to_string(),
                });
            }
        }
        let settled_at = Utc::now();
        let transitioned = sqlx::query(
            "UPDATE moa.execution_task SET status='running', attempt_state='running', \
                 generation_history=generation_history || jsonb_build_array(jsonb_build_object( \
                   'kind','external_wait_resolution','generation',$3,'recorded_at',$4)), \
                 last_progress_at=GREATEST(last_progress_at,$4), updated_at=$4 \
             WHERE run_uid=$1 AND task_id=$2 AND generation=$3 \
               AND status IN ('waiting_review','waiting_signal') AND attempt_state='waiting'",
        )
        .bind(run_uid)
        .bind(task_id.as_uuid())
        .bind(to_i64(generation, "task generation")?)
        .bind(settled_at)
        .execute(conn.as_mut())
        .await
        .map_err(sqlx_error)?;
        if transitioned.rows_affected() != 1 {
            return Err(Error::InvalidRepositoryData {
                message: "storage wait lost its locked transition fence".to_string(),
            });
        }
        let write =
            record_task_outcome_in_conn(&mut conn, run_uid, task_id, generation, outcome).await?;
        let settled_task = match &write {
            TaskOutcomeWrite::Applied { task, .. } | TaskOutcomeWrite::Replayed { task, .. } => {
                task
            }
            TaskOutcomeWrite::NotFound | TaskOutcomeWrite::Rejected { .. } => {
                return Err(Error::InvalidRepositoryData {
                    message: "locked storage-wait outcome was not accepted".to_string(),
                });
            }
        };
        transition_node_counters_in_tx(
            &mut conn,
            run_uid,
            &settled_task.node_id,
            &settled_task.item_key,
            task.status,
            settled_task.status,
        )
        .await?;
        refresh_run_after_wait_settlement_in_conn(&mut conn, run_uid, task_id, settled_at).await?;
        if !matches!(
            run.status,
            ExecutionRunStatus::PauseRequested
                | ExecutionRunStatus::Pausing
                | ExecutionRunStatus::Paused
        ) {
            enqueue_run_activation_in_conn(
                conn.as_mut(),
                run.tenant_id,
                run_uid,
                run.controller_generation,
                settled_at,
                json!({
                    "source": "external_wait_resolution",
                    "task_id": task_id,
                    "task_generation": generation,
                }),
            )
            .await?;
        }
        conn.commit().await.map_err(storage_error)?;
        Ok(write)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ATTEMPT_STEP_BOUND_MARGIN_SECONDS, ActiveAttemptLiveness, TaskAttemptCheckpointKind,
        TaskAttemptFence, UnstartedTaskAttemptDisposition, append_agent_resume_input,
        classify_active_attempt_liveness, external_start_checkpoint_payload_is_provisional,
        paused_task_attempt_release_history_matches, task_release_receipt_is_verified_absence,
        unstarted_task_attempt_history_matches,
    };
    use chrono::{Duration, Utc};
    use moa_config::ExecutionConfig;
    use moa_core::types::context::ContextMessage;
    use moa_core::types::{
        identifiers::{ExecutionRunScopeId, ExecutionTaskScopeId, TenantId},
        sandbox_workspace::{ExecutionHandReleaseOwner, ExecutionHandReleaseReceipt},
    };
    use serde_json::json;
    use uuid::Uuid;

    use crate::state::ExecutionTaskId;

    #[test]
    fn agent_input_resume_appends_exact_user_message_to_durable_checkpoint() {
        // Pins: a public input acknowledgement is not complete unless the next bounded Agent
        // attempt can observe the exact supplied reply from its canonical checkpoint.
        let mut checkpoint = json!({
            "schema_version": 1,
            "state": {
                "kind": "agent",
                "messages": [serde_json::to_value(ContextMessage::assistant("question"))
                    .expect("fixture message serializes")],
                "next_turn": 1,
                "usage": {},
                "security_circuit": {},
                "disabled_capabilities": {},
                "pending_review": null,
                "pending_external": null
            },
            "review_resolution": null,
            "external_job_resolution": null,
            "workspace_release_receipt_id": null
        });

        append_agent_resume_input(&mut checkpoint, &json!({"answer": "approved"}))
            .expect("agent input appends");

        let messages = checkpoint["state"]["messages"]
            .as_array()
            .expect("messages remain an array");
        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[1],
            serde_json::to_value(ContextMessage::user(r#"{"answer":"approved"}"#))
                .expect("expected message serializes")
        );
        assert_eq!(checkpoint["state"]["next_turn"], 1);
    }

    #[test]
    fn external_start_checkpoint_requires_exact_typed_provisional_shape() {
        // Pins: provider start cannot precede a durable continuation that identifies either the
        // exact pending Agent invocation or the stable direct capability tool-call identity.
        let agent = json!({
            "state": {
                "kind": "agent",
                "pending_external": {
                    "external_job_uid": null,
                    "invocation": {"id": "call-1"}
                }
            }
        });
        assert!(external_start_checkpoint_payload_is_provisional(
            TaskAttemptCheckpointKind::AgentContinuation,
            &agent,
        ));
        let bound_agent = json!({
            "state": {
                "kind": "agent",
                "pending_external": {
                    "external_job_uid": Uuid::new_v4(),
                    "invocation": {"id": "call-1"}
                }
            }
        });
        assert!(!external_start_checkpoint_payload_is_provisional(
            TaskAttemptCheckpointKind::AgentContinuation,
            &bound_agent,
        ));
        assert!(external_start_checkpoint_payload_is_provisional(
            TaskAttemptCheckpointKind::CapabilityExternalStart,
            &json!({"state":{"kind":"capability_external_start","tool_id":Uuid::new_v4()}}),
        ));
        assert!(!external_start_checkpoint_payload_is_provisional(
            TaskAttemptCheckpointKind::CapabilityReview,
            &json!({"state":{"kind":"capability_external_start","tool_id":Uuid::new_v4()}}),
        ));
    }

    #[test]
    fn unstarted_attempt_replay_requires_exact_dispatch_generation_and_disposition() {
        // Pins: a dead-letter or cancel replay must never consume another admitted attempt's
        // capacity or treat a different terminal disposition as already settled.
        let fence = TaskAttemptFence {
            tenant_id: TenantId(Uuid::new_v4()),
            run_uid: Uuid::new_v4(),
            task_id: ExecutionTaskId::from_uuid(Uuid::new_v4()),
            controller_generation: 4,
            attempt_generation: 7,
            dispatch_uid: Uuid::new_v4(),
            capacity_reservation_uid: Uuid::new_v4(),
            watchdog_trigger_uid: Uuid::new_v4(),
            attempt_deadline_at: Utc::now(),
        };
        let history = vec![json!({
            "kind": "unstarted_attempt_settlement",
            "dispatch_uid": fence.dispatch_uid,
            "attempt_generation": fence.attempt_generation,
            "disposition": "dispatch_delivery_lost",
            "reason": null,
        })];

        assert!(unstarted_task_attempt_history_matches(
            &history,
            &fence,
            &UnstartedTaskAttemptDisposition::DispatchDeliveryLost,
        ));
        assert!(!unstarted_task_attempt_history_matches(
            &history,
            &fence,
            &UnstartedTaskAttemptDisposition::Cancelled {
                reason: "pause requested".to_string(),
            },
        ));
        assert!(!unstarted_task_attempt_history_matches(
            &history,
            &fence,
            &UnstartedTaskAttemptDisposition::Paused {
                controller_generation: 5,
            },
        ));
        let mut stale_fence = fence;
        stale_fence.attempt_generation += 1;
        assert!(!unstarted_task_attempt_history_matches(
            &history,
            &stale_fence,
            &UnstartedTaskAttemptDisposition::DispatchDeliveryLost,
        ));
    }

    #[test]
    fn pause_release_replay_requires_both_controller_generations() {
        // Pins: pause increments the run generation, while the released capacity and watchdog
        // remain owned by the prior admission generation; neither coordinate is interchangeable.
        let fence = TaskAttemptFence {
            tenant_id: TenantId(Uuid::new_v4()),
            run_uid: Uuid::new_v4(),
            task_id: ExecutionTaskId::from_uuid(Uuid::new_v4()),
            controller_generation: 4,
            attempt_generation: 7,
            dispatch_uid: Uuid::new_v4(),
            capacity_reservation_uid: Uuid::new_v4(),
            watchdog_trigger_uid: Uuid::new_v4(),
            attempt_deadline_at: Utc::now(),
        };
        let history = vec![json!({
            "kind": "pause_release_finalized",
            "dispatch_uid": fence.dispatch_uid,
            "attempt_generation": fence.attempt_generation,
            "attempt_controller_generation": fence.controller_generation,
            "controller_generation": 5,
        })];

        assert!(paused_task_attempt_release_history_matches(
            &history, &fence, 5,
        ));
        assert!(!paused_task_attempt_release_history_matches(
            &history, &fence, 6,
        ));
        let mut stale_fence = fence;
        stale_fence.controller_generation += 1;
        assert!(!paused_task_attempt_release_history_matches(
            &history,
            &stale_fence,
            5,
        ));
    }

    #[test]
    fn unstarted_pause_replay_is_exact_and_distinct_from_terminal_cancel() {
        // Pins: a pause received before start requeues the task and replay detection must not
        // reinterpret that durable disposition as a terminal cancellation.
        let fence = TaskAttemptFence {
            tenant_id: TenantId(Uuid::new_v4()),
            run_uid: Uuid::new_v4(),
            task_id: ExecutionTaskId::from_uuid(Uuid::new_v4()),
            controller_generation: 8,
            attempt_generation: 3,
            dispatch_uid: Uuid::new_v4(),
            capacity_reservation_uid: Uuid::new_v4(),
            watchdog_trigger_uid: Uuid::new_v4(),
            attempt_deadline_at: Utc::now(),
        };
        let history = vec![json!({
            "kind": "unstarted_attempt_settlement",
            "dispatch_uid": fence.dispatch_uid,
            "attempt_generation": fence.attempt_generation,
            "disposition": "paused",
            "reason": null,
            "controller_generation": 9,
        })];

        assert!(unstarted_task_attempt_history_matches(
            &history,
            &fence,
            &UnstartedTaskAttemptDisposition::Paused {
                controller_generation: 9,
            },
        ));
        assert!(!unstarted_task_attempt_history_matches(
            &history,
            &fence,
            &UnstartedTaskAttemptDisposition::Paused {
                controller_generation: 10,
            },
        ));
        assert!(!unstarted_task_attempt_history_matches(
            &history,
            &fence,
            &UnstartedTaskAttemptDisposition::Cancelled {
                reason: "pause requested".to_string(),
            },
        ));
    }

    #[test]
    fn verified_absence_receipt_shape_rejects_partial_workspace_identity() {
        // Pins: a no-hand task may settle only with the canonical all-NULL durable absence proof;
        // dropping part of a real checkpoint identity must never be treated as absence.
        let task_id = ExecutionTaskScopeId(Uuid::new_v4());
        let now = Utc::now();
        let mut receipt = ExecutionHandReleaseReceipt {
            receipt_id: Uuid::new_v4(),
            tenant_id: TenantId(Uuid::new_v4()),
            run_id: ExecutionRunScopeId(Uuid::new_v4()),
            owner: ExecutionHandReleaseOwner::Task {
                task_id,
                logical_generation: 3,
            },
            attempt_generation: 5,
            workspace_id: None,
            writer_epoch: None,
            instance_generation: None,
            hand_provisioning_operation_id: None,
            hand_lease_generation: None,
            checkpoint_id: None,
            checkpoint_generation: None,
            checkpoint_manifest_digest: None,
            checkpoint_logical_bytes: None,
            requested_at: now,
            released_at: now,
        };

        assert!(task_release_receipt_is_verified_absence(&receipt));
        receipt.writer_epoch = Some(1);
        assert!(!task_release_receipt_is_verified_absence(&receipt));
    }

    #[test]
    fn attempt_liveness_separates_a_stall_from_a_slow_but_progressing_attempt_offline() {
        // Pins: an attempt that keeps committing durable steps stays live for its whole
        // authorized window, a wedged attempt inside that window is classified stalled well
        // before the deadline, and the absolute deadline still outranks the heartbeat window.
        let config = ExecutionConfig {
            attempt_heartbeat_staleness_seconds: 60,
            active_attempt_timeout_seconds: 600,
            ..ExecutionConfig::default()
        };
        let started_at = Utc::now();
        let deadline = started_at + Duration::seconds(600);

        // Nine minutes in, having reported progress thirty seconds ago.
        let observed_at = started_at + Duration::seconds(540);
        assert_eq!(
            classify_active_attempt_liveness(
                &config,
                deadline,
                observed_at - Duration::seconds(30),
                None,
                observed_at,
            ),
            ActiveAttemptLiveness::Live
        );

        // Same instant, but the attempt has committed nothing since it started.
        assert_eq!(
            classify_active_attempt_liveness(&config, deadline, started_at, None, observed_at),
            ActiveAttemptLiveness::Stalled
        );

        // The stall is visible nine minutes before the deadline would have exposed it.
        assert_eq!(
            classify_active_attempt_liveness(
                &config,
                deadline,
                started_at,
                None,
                started_at + Duration::seconds(60),
            ),
            ActiveAttemptLiveness::Stalled
        );
        assert_eq!(
            classify_active_attempt_liveness(
                &config,
                deadline,
                started_at,
                None,
                started_at + Duration::seconds(59),
            ),
            ActiveAttemptLiveness::Live
        );

        // A progressing attempt that reaches its deadline is deadline-exceeded, never stalled.
        assert_eq!(
            classify_active_attempt_liveness(&config, deadline, deadline, None, deadline),
            ActiveAttemptLiveness::DeadlineExceeded
        );
        assert!(!ActiveAttemptLiveness::Live.is_expired());
        assert!(ActiveAttemptLiveness::Stalled.is_expired());
        assert!(ActiveAttemptLiveness::DeadlineExceeded.is_expired());
    }

    #[test]
    fn declared_step_bound_widens_only_its_own_stall_window() {
        // Pins: a step that declared a bound longer than the configured floor is Live until
        // that bound plus the margin elapses, while an attempt between steps at the very same
        // instant is already Stalled. Without this, one long-running step would have to raise
        // the floor for every attempt, which is what made the stall guard barely beat the
        // deadline it is supposed to improve on.
        let config = ExecutionConfig {
            attempt_heartbeat_staleness_seconds: 120,
            active_attempt_timeout_seconds: 600,
            ..ExecutionConfig::default()
        };
        let started_at = Utc::now();
        let deadline = started_at + Duration::seconds(600);
        let bound = Some(Duration::seconds(300));

        // Past the floor, inside the declared bound: the step is still working.
        let inside = started_at + Duration::seconds(200);
        assert_eq!(
            classify_active_attempt_liveness(&config, deadline, started_at, bound, inside),
            ActiveAttemptLiveness::Live
        );
        // The same silence with no declared bound is a stall at the floor.
        assert_eq!(
            classify_active_attempt_liveness(&config, deadline, started_at, None, inside),
            ActiveAttemptLiveness::Stalled
        );

        // The bound is not a grace-free cliff: it ends one margin after the declared bound.
        let at_bound = started_at + Duration::seconds(300);
        assert_eq!(
            classify_active_attempt_liveness(&config, deadline, started_at, bound, at_bound),
            ActiveAttemptLiveness::Live
        );
        assert_eq!(
            classify_active_attempt_liveness(
                &config,
                deadline,
                started_at,
                bound,
                at_bound + Duration::seconds(ATTEMPT_STEP_BOUND_MARGIN_SECONDS),
            ),
            ActiveAttemptLiveness::Stalled
        );

        // A bound shorter than the floor never tightens the window below it, so jitter around
        // a two-second step cannot read as a stall.
        assert_eq!(
            classify_active_attempt_liveness(
                &config,
                deadline,
                started_at,
                Some(Duration::seconds(2)),
                started_at + Duration::seconds(119),
            ),
            ActiveAttemptLiveness::Live
        );
    }
}
