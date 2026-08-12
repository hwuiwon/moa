//! Scoped PostgreSQL persistence for durable execution runs and logical tasks.

mod admission;
pub mod amendment;
/// Immutable execution planning-context and normalized audit persistence.
pub mod audit;
mod audit_codec;
pub mod capacity;
pub mod compensation;
pub mod completion;
pub mod external_job;
mod materialize;
pub mod outbox;
mod outcome;
mod outcome_support;
mod projection;
pub mod ready;
/// Durable bounded replan-stop intent handoff.
pub mod replan_stop;
pub mod retention;
mod rows;
pub mod run;
pub mod schedule;
mod sql;
pub mod task;
/// Bounded terminal fencing, trigger drain, compensation, and finalization persistence.
pub mod terminal;
mod transition;
pub mod trigger;

use std::{collections::BTreeMap, str::FromStr};

use chrono::{DateTime, Utc};
use moa_artifacts::execution_plan::{
    ExecutionBudgetLimit, ExecutionCitation, ExecutionCompensation, ExecutionGoalContract,
    ExecutionOperation, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionUsage, PlanAmendment,
};
use moa_core::{
    traits::Identity,
    types::contact::ContactId,
    types::execution_planning::{
        ExecutionCompileOutcome, ExecutionCompileSource, ExecutionPlannerCallKind,
        ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload,
        ExecutionRouteClassifierOutcome, ExecutionRouteKind, ExecutionRouteProvenance,
        ExecutionRouteSource, ExecutionRouteStage, ExecutionRouteUsage, ExecutionSourceProvenance,
        ExecutionStrategy, route_provenance_semantically_equal, validate_planning_audit_envelope,
    },
    types::identifiers::{SessionId, TenantId, UserId},
};
use moa_db::ScopedConn;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool, Row, postgres::PgRow};
use uuid::Uuid;

use self::outbox::ExecutionDispatchRecord;

use crate::{
    Error, Result,
    budget::{BudgetLedger, BudgetReconciliation},
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash, amendment_hash,
    },
    compiler::CanonicalExecutionPlan,
    completion::{
        CompletionEvaluation, execution_terminal_reason, run_status_from_completion,
        terminal_evidence_from_evaluation,
    },
    replan::failure_fingerprint,
    state::{
        CompensationId, CompensationRegistrationProjection, CompensationStatus,
        ExecutionCompensationOutcome, ExecutionNodeStatus, ExecutionProjection, ExecutionRunStatus,
        ExecutionSourceKind, ExecutionTaskId, ExecutionTaskProjection, ExecutionTaskStatus,
        ExecutionTerminalCause, ExecutionTerminalEvidence, ExecutionTerminalReason,
        FailureFingerprintInput, LogicalTask, LogicalTaskKind, PendingExecutionTerminal,
        TerminalProjection, WaitingReason, run_status_after_task_outcome,
        run_status_from_terminal_projection, task_outcome_is_terminal, task_status_from_outcome,
    },
    wire::{
        ExecutionActionReviewResolution, ExecutionPlanningContextSnapshot,
        ExecutionTemplateAdmissionRequest, ExecutionTerminalDelivery,
        ExecutionToolDispatchRejection, PinnedInstructionSkill,
    },
};

const DEFAULT_RUN_PAGE_LIMIT: u32 = 100;
const MAX_RUN_PAGE_LIMIT: u32 = 1_000;
const DEFAULT_TASK_PAGE_LIMIT: u32 = 100;
const MAX_TASK_PAGE_LIMIT: u32 = 1_000;
const EXECUTION_AUDIT_NAMESPACE: Uuid = Uuid::from_u128(0x7b83_c5c2_5cf7_5fa0_8eb6_2d7c_6e0f_1d11);

/// Phase of one execution-scoped external effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionEffectPhase {
    /// The active attempt is invoking an effect without an action-review handoff.
    Direct,
    /// The exact parked action review was cleared and claimed for execution.
    Reviewed {
        /// Stable action-review identity persisted by both the attempt and review owner.
        review_uid: Uuid,
    },
}

/// Persisted execution operation seeking permission to begin one external effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionEffectOwner {
    /// Forward task coordinates fenced by the current task and bounded-attempt generations.
    Task {
        /// Stable forward task identifier.
        task_id: ExecutionTaskId,
        /// Exact current logical task generation.
        generation: u64,
        /// Exact current bounded-attempt generation.
        attempt_generation: u64,
        /// Exact direct or reviewed invocation phase.
        phase: ExecutionEffectPhase,
    },
    /// Compensation coordinates fenced by the current logical and bounded-attempt generations.
    Compensation {
        /// Stable compensation identifier.
        compensation_id: CompensationId,
        /// Exact current logical compensation generation.
        generation: u64,
        /// Exact current bounded-attempt generation.
        attempt_generation: u64,
        /// Exact direct or reviewed invocation phase.
        phase: ExecutionEffectPhase,
    },
}

/// Row-locked external-effect admission result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionEffectAdmissionOutcome {
    /// The operation was current and linearized before any terminal fence.
    Admitted,
    /// The effect was definitively rejected before dispatch.
    Rejected(ExecutionToolDispatchRejection),
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CompensationPersistedOutcome {
    result: Option<ExecutionCompensationOutcome>,
    review_audit: Vec<CompensationReviewAuditEntry>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CompensationReviewAuditEntry {
    review_uid: Uuid,
    generation: u64,
    accepted: bool,
    resolution: Option<ExecutionActionReviewResolution>,
    expires_at: Option<DateTime<Utc>>,
    recorded_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PendingTerminalEvidencePayload {
    terminal_evidence: ExecutionTerminalEvidence,
    completion_check_results: Vec<Value>,
    terminal_gaps: Vec<String>,
}

/// Database scope used for one repository operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionScope {
    /// Explicit platform control-plane access. New rows still declare their tenant/contact owner.
    ControlPlane,
    /// Tenant-owned rows whose `contact_id` is null.
    Tenant {
        /// Owning tenant.
        tenant_id: TenantId,
    },
    /// Rows owned by one contact inside a tenant.
    Contact {
        /// Owning tenant.
        tenant_id: TenantId,
        /// Owning contact.
        contact_id: ContactId,
    },
}

impl ExecutionScope {
    async fn begin<'p>(self, pool: &'p PgPool) -> Result<ScopedConn<'p>> {
        if matches!(
            self,
            Self::Contact { contact_id, .. } if contact_id.0.is_nil()
        ) {
            return Err(Error::InvalidRepositoryInput {
                message: "execution repository contact scope must not use a nil contact"
                    .to_string(),
            });
        }
        let mut conn = match self {
            Self::ControlPlane => ScopedConn::begin_control_plane(pool).await,
            Self::Tenant { tenant_id } => ScopedConn::begin_tenant(pool, tenant_id).await,
            Self::Contact {
                tenant_id,
                contact_id,
            } => ScopedConn::begin_contact(pool, tenant_id, contact_id).await,
        }
        .map_err(storage_error)?;
        conn.assume_app_role().await.map_err(storage_error)?;
        Ok(conn)
    }

    fn permits_owner(self, tenant_id: TenantId, contact_id: Option<ContactId>) -> bool {
        match self {
            Self::ControlPlane => true,
            Self::Tenant {
                tenant_id: scoped_tenant,
            } => scoped_tenant == tenant_id && contact_id.is_none(),
            Self::Contact {
                tenant_id: scoped_tenant,
                contact_id: scoped_contact,
            } => scoped_tenant == tenant_id && contact_id == Some(scoped_contact),
        }
    }
}

/// Input used to create one immutable execution run snapshot.
#[derive(Clone, Debug)]
pub struct NewExecutionRun {
    /// Tenant that owns the run.
    pub tenant_id: TenantId,
    /// Optional contact owner; null denotes tenant-owned work.
    pub contact_id: Option<ContactId>,
    /// Parent session that requested the run.
    pub session_id: SessionId,
    /// Exact persisted user-message sequence that originated the run.
    pub originating_user_sequence_num: u64,
    /// Immutable planning-context row used for admission.
    pub planning_context_uid: Uuid,
    /// Expected canonical planning-context hash.
    pub planning_context_hash: ExecutionHash,
    /// Authenticated tenant user that owns the run.
    pub owner_user_id: UserId,
    /// Exact authenticated principal admitted to create this durable run.
    pub admitted_identity: Identity,
    /// Immutable user-derived goal contract.
    pub goal: ExecutionGoalContract,
    /// Initial canonical plan, also installed as revision one.
    pub plan: CanonicalExecutionPlan,
    /// Exact immutable capability catalog used by compilation.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact immutable capability and skill authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Sorted exact instruction-skill revisions available to task-local agents.
    pub pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    /// Skill-template or generated-plan provenance.
    pub source_provenance: ExecutionSourceProvenance,
    /// Structured run input.
    pub input: Value,
    /// Initial status, which must be queued or awaiting confirmation.
    pub status: ExecutionRunStatus,
    /// Approved resource envelope displayed or accepted for this run.
    pub approved_budget: ExecutionBudgetLimit,
    /// Optional caller idempotency key scoped to tenant and contact.
    pub idempotency_key: Option<String>,
}

/// Permanent replay row for one external execution-template admission operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTemplateAdmissionRecord {
    /// Stable operation identity reserved before any Session mutation.
    pub operation_uid: Uuid,
    /// Canonical fingerprint of the complete first request.
    pub request_fingerprint: String,
    /// Exact persisted objective sequence, when committed.
    pub originating_user_sequence_num: Option<u64>,
    /// Exact execution run UID, when committed.
    pub execution_run_uid: Option<Uuid>,
}

/// Persisted execution-run projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionRunRecord {
    /// Durable run identifier.
    pub run_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Originating parent session.
    pub session_id: SessionId,
    /// Exact persisted user-message sequence that originated the run.
    pub originating_user_sequence_num: u64,
    /// Immutable planning-context row used for admission.
    pub planning_context_uid: Uuid,
    /// Canonical planning-context hash used for admission.
    pub planning_context_hash: ExecutionHash,
    /// Originating tenant user.
    pub owner_user_id: UserId,
    /// Exact authenticated principal admitted when the run was created.
    pub admitted_identity: Identity,
    /// Immutable goal contract.
    pub goal: ExecutionGoalContract,
    /// Immutable initial canonical plan.
    pub initial_plan: CanonicalExecutionPlan,
    /// Current canonical plan.
    pub active_plan: CanonicalExecutionPlan,
    /// Immutable initial plan hash.
    pub initial_plan_hash: ExecutionHash,
    /// Current active plan hash.
    pub active_plan_hash: ExecutionHash,
    /// Exact plan hash accepted by confirmation, when confirmation occurred.
    pub confirmed_plan_hash: Option<ExecutionHash>,
    /// Current one-based plan revision.
    pub plan_revision: u64,
    /// Append-only amendment metadata.
    pub plan_history: Vec<Value>,
    /// Exact immutable persisted capability catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact immutable persisted authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Sorted exact persisted instruction-skill revisions.
    pub pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    /// Skill-template or generated-plan provenance.
    pub source_provenance: ExecutionSourceProvenance,
    /// Normalized source cohort persisted by the execution-analytics schema.
    pub source_kind: ExecutionSourceKind,
    /// Exact pinned skill-template reference for template-backed runs.
    pub skill_template_ref: Option<String>,
    /// Exact pinned skill-template revision for template-backed runs.
    pub skill_template_revision_uid: Option<Uuid>,
    /// Structured run input.
    pub input: Value,
    /// Terminal structured output, when present.
    pub output: Option<Value>,
    /// Persisted completion-check evidence.
    pub completion_check_results: Vec<Value>,
    /// Explicit terminal completion gaps.
    pub terminal_gaps: Vec<String>,
    /// Immutable typed terminal cause and requirement counts, when terminal.
    pub terminal_evidence: Option<ExecutionTerminalEvidence>,
    /// Exact normalized terminal reason, present exactly for terminal runs.
    pub terminal_reason: Option<ExecutionTerminalReason>,
    /// Current durable run status.
    pub status: ExecutionRunStatus,
    /// Monotonic generation fencing controller activations and delayed wakes.
    pub controller_generation: u64,
    /// Current bounded-controller activation lifecycle.
    pub activation_state: ExecutionActivationState,
    /// Earliest exact time at which the controller should be reactivated.
    pub next_wake_at: Option<DateTime<Utc>>,
    /// Time at which the run entered its current storage-only wait.
    pub waiting_since: Option<DateTime<Utc>>,
    /// Latest durable scheduler progress timestamp.
    pub last_progress_at: DateTime<Utc>,
    /// Time at which an authorized pause was first requested.
    pub pause_requested_at: Option<DateTime<Utc>>,
    /// Time at which the run became fully paused.
    pub paused_at: Option<DateTime<Utc>>,
    /// Number of tasks currently admitted to the durable ready queue.
    pub ready_task_count: u64,
    /// Number of task attempts currently consuming active capacity.
    pub active_task_count: u64,
    /// Exact number of logical tasks parked on durable waits.
    pub waiting_task_count: u64,
    /// Exact number of tasks waiting for user input.
    pub waiting_input_task_count: u64,
    /// Exact number of tasks waiting for governed review.
    pub waiting_review_task_count: u64,
    /// Exact number of tasks waiting for a named signal.
    pub waiting_signal_task_count: u64,
    /// Exact number of tasks waiting for an absolute timer.
    pub waiting_timer_task_count: u64,
    /// Exact number of tasks waiting for an external job.
    pub waiting_external_task_count: u64,
    /// Exact number of tasks waiting for bounded replanning.
    pub waiting_replan_task_count: u64,
    /// Exact input waits whose authorized audience is the owning user.
    pub waiting_input_user_task_count: u64,
    /// Exact input waits whose authorized audience is a tenant administrator.
    pub waiting_input_tenant_admin_task_count: u64,
    /// Exact input waits whose authorized audience is an external system.
    pub waiting_input_external_task_count: u64,
    /// Approved resource limits.
    pub approved_budget: ExecutionBudgetLimit,
    /// Resources held by nonterminal tasks.
    pub reserved: ExecutionEstimate,
    /// Reconciled actual resources and terminal logical tasks.
    pub consumed: ExecutionEstimate,
    /// Whether actual usage exceeded a reservation or approved limit.
    pub budget_overrun: bool,
    /// Number of materialized tasks.
    pub progress_total_tasks: u64,
    /// Number of successfully completed tasks.
    pub progress_completed_tasks: u64,
    /// Number of failed tasks.
    pub progress_failed_tasks: u64,
    /// Number of cancelled tasks.
    pub progress_cancelled_tasks: u64,
    /// Bounded deterministic sample of current scheduler wait reasons.
    pub waiting_reasons: Vec<WaitingReason>,
    /// Whether exact waiting tasks exist outside the bounded reason sample.
    pub waiting_reasons_truncated: bool,
    /// Monotonic epoch incremented by scheduling-relevant mutations.
    pub wake_epoch: u64,
    /// Last scheduler epoch acknowledged by compare-and-set.
    pub processed_wake_epoch: u64,
    /// Next monotonic compensation registration sequence.
    pub next_compensation_sequence: u64,
    /// Original terminal intent held after compensation admission is fenced.
    pub pending_terminal: Option<PendingExecutionTerminal>,
    /// Whether compensation ambiguity or failure requires governed manual repair.
    pub manual_repair_required: bool,
    /// Scoped idempotency key.
    pub idempotency_key: Option<String>,
    /// Cancellation reason, when cancelled.
    pub cancellation_reason: Option<String>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Database-owned timestamp when the run first became queued.
    pub queued_at: Option<DateTime<Utc>>,
    /// Last mutation timestamp.
    pub updated_at: DateTime<Utc>,
    /// First task-start timestamp.
    pub started_at: Option<DateTime<Utc>>,
    /// Terminal timestamp.
    pub completed_at: Option<DateTime<Utc>>,
    /// Confirmation timestamp.
    pub confirmed_at: Option<DateTime<Utc>>,
}

/// Persisted logical-task projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionTaskRecord {
    /// Stable logical task identifier.
    pub task_id: ExecutionTaskId,
    /// Owning run.
    pub run_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional owning contact.
    pub contact_id: Option<ContactId>,
    /// Stable plan node ID.
    pub node_id: String,
    /// Stable ordinary, map, reducer, or verifier item key.
    pub item_key: String,
    /// Goal requirements served by this task.
    pub requirement_ids: Vec<String>,
    /// Plan revision that created this task.
    pub plan_revision: u64,
    /// Current durable task status.
    pub status: ExecutionTaskStatus,
    /// One-based execution attempt.
    pub attempt: u32,
    /// One-based dispatch generation fence.
    pub generation: u64,
    /// One-based generation fencing the currently persisted attempt lifecycle.
    pub attempt_generation: u64,
    /// Current bounded-attempt lifecycle state.
    pub attempt_state: ExecutionAttemptState,
    /// Time at which the current active attempt began.
    pub attempt_started_at: Option<DateTime<Utc>>,
    /// Latest durable progress timestamp for this logical task.
    pub last_progress_at: DateTime<Utc>,
    /// Absolute watchdog deadline for the current active attempt.
    pub attempt_deadline_at: Option<DateTime<Utc>>,
    /// Time at which the task entered its current storage-only wait.
    pub waiting_since: Option<DateTime<Utc>>,
    /// Time at which the task entered the ready queue.
    pub ready_at: Option<DateTime<Utc>>,
    /// Current asynchronous provider job, when the task is waiting externally.
    pub external_job_uid: Option<Uuid>,
    /// Dispatch lease currently owning this attempt, when one is active.
    pub active_dispatch_uid: Option<Uuid>,
    /// Monotonic dispatch fence for this logical task.
    pub dispatch_sequence: u64,
    /// Resolved structured task input.
    pub input: Value,
    /// Append-only ordered payloads supplied by input resumes.
    pub resume_input_history: Vec<Value>,
    /// Executable task descriptor.
    pub kind: LogicalTaskKind,
    /// Immutable exact rollback contract for a direct capability task.
    pub compensation_contract: Option<ExecutionCompensation>,
    /// Retry policy serialized with the task.
    pub retry: moa_artifacts::execution_plan::RetryPolicy,
    /// Worst-case reservation requested by this task.
    pub estimate: ExecutionEstimate,
    /// Remaining reservation currently held by the task.
    pub reserved: ExecutionEstimate,
    /// Cumulative actual resource usage.
    pub actual: ExecutionUsage,
    /// Whether this logical task has been reconciled terminally.
    pub actual_tasks: u64,
    /// Latest accepted outcome.
    pub current_outcome: Option<ExecutionTaskOutcome>,
    /// Current structured output.
    pub output: Option<Value>,
    /// Current structured error details.
    pub error: Option<Value>,
    /// Current accepted citations.
    pub citations: Vec<ExecutionCitation>,
    /// Immutable attempt/generation dispatch history.
    pub generation_history: Vec<Value>,
    /// Append-only accepted and rejected outcome audit.
    pub outcome_audit: Vec<Value>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last mutation timestamp.
    pub updated_at: DateTime<Utc>,
    /// Reservation timestamp.
    pub reserved_at: Option<DateTime<Utc>>,
    /// First running timestamp.
    pub started_at: Option<DateTime<Utc>>,
    /// Terminal timestamp.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Bounded controller activation state persisted independently from run status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionActivationState {
    /// No controller activation is queued or running.
    Idle,
    /// One generation-fenced activation is ready for durable dispatch.
    Queued,
    /// The current generation is executing one bounded activation.
    Advancing,
    /// The run is explicitly paused and owns no controller activation.
    Paused,
    /// The run is terminal and cannot be activated again.
    Terminal,
}

impl ExecutionActivationState {
    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Queued => "queued",
            Self::Advancing => "advancing",
            Self::Paused => "paused",
            Self::Terminal => "terminal",
        }
    }
}

impl FromStr for ExecutionActivationState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "idle" => Ok(Self::Idle),
            "queued" => Ok(Self::Queued),
            "advancing" => Ok(Self::Advancing),
            "paused" => Ok(Self::Paused),
            "terminal" => Ok(Self::Terminal),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution activation state `{value}`"),
            }),
        }
    }
}

/// Bounded task-attempt state persisted independently from logical task status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAttemptState {
    /// No attempt is currently dispatched or running.
    Idle,
    /// The attempt is committed for durable dispatch.
    Dispatching,
    /// The attempt currently consumes active task capacity.
    Running,
    /// Provider teardown was claimed and capacity remains owned until verified release.
    Cancelling,
    /// The logical task is parked without an active attempt.
    Waiting,
    /// The logical task settled terminally.
    Terminal,
    /// A non-idempotent attempt has an ambiguous external outcome.
    UnknownOutcome,
}

impl ExecutionAttemptState {
    /// Returns the canonical database label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Dispatching => "dispatching",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Waiting => "waiting",
            Self::Terminal => "terminal",
            Self::UnknownOutcome => "unknown_outcome",
        }
    }
}

impl FromStr for ExecutionAttemptState {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "idle" => Ok(Self::Idle),
            "dispatching" => Ok(Self::Dispatching),
            "running" => Ok(Self::Running),
            "cancelling" => Ok(Self::Cancelling),
            "waiting" => Ok(Self::Waiting),
            "terminal" => Ok(Self::Terminal),
            "unknown_outcome" => Ok(Self::UnknownOutcome),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution attempt state `{value}`"),
            }),
        }
    }
}

/// Exact durable checkpoint written when one bounded controller activation returns.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionRunActivationCheckpoint {
    /// Product-visible run state after the bounded activation.
    pub status: ExecutionRunStatus,
    /// Whether another activation is queued or the run is parked/terminal.
    pub activation_state: ExecutionActivationState,
    /// Earliest exact wake time, when time can make more work ready.
    pub next_wake_at: Option<DateTime<Utc>>,
    /// Start of the current storage-only wait, when parked.
    pub waiting_since: Option<DateTime<Utc>>,
    /// Exact number of ready logical tasks.
    pub ready_task_count: u64,
    /// Exact number of active task attempts.
    pub active_task_count: u64,
}

/// Generation-fenced result of claiming or checkpointing a run activation.
#[derive(Clone, Debug, PartialEq)]
pub enum RunActivationWriteOutcome {
    /// The exact generation mutation was committed.
    Applied(ExecutionRunRecord),
    /// The requested state was already durably present.
    AlreadyApplied(ExecutionRunRecord),
    /// No visible run exists under the supplied scope.
    NotFound,
    /// The supplied controller generation is stale or from the future.
    GenerationMismatch,
    /// The run is not in a lifecycle state that accepts this mutation.
    InvalidState,
}

/// Exact generation-and-wake claim made by one bounded run-controller activation.
#[derive(Clone, Debug, PartialEq)]
pub enum RunControllerClaimOutcome {
    /// The queued wake was claimed and the run is now advancing.
    Claimed(ExecutionRunRecord),
    /// The same wake is already being advanced by a replay of the same durable invocation.
    Resumed(ExecutionRunRecord),
    /// The requested wake was already durably acknowledged.
    Replayed(ExecutionRunRecord),
    /// The run is terminal, so the activation is a successful no-op.
    Terminal(ExecutionRunRecord),
    /// No visible run exists under the supplied scope.
    NotFound,
    /// The request did not name the current controller generation.
    StaleGeneration {
        /// Current persisted controller generation.
        current_generation: u64,
    },
    /// The request did not name the current unprocessed wake.
    StaleWake {
        /// Current persisted wake epoch.
        current_wake_epoch: u64,
        /// Greatest wake epoch already acknowledged by the controller.
        processed_wake_epoch: u64,
    },
    /// The run lifecycle cannot accept an activation claim.
    InvalidState,
}

/// Atomic checkpoint request for one bounded run-controller activation.
#[derive(Clone, Debug, PartialEq)]
pub struct RunControllerCompletionRequest {
    /// Exact controller generation claimed by the invocation.
    pub controller_generation: u64,
    /// Exact wake epoch claimed by the invocation.
    pub wake_epoch: u64,
    /// Durable run checkpoint produced by bounded scheduler work.
    pub checkpoint: ExecutionRunActivationCheckpoint,
    /// Structured activation payload when bounded work requires one continuation.
    pub continuation_payload: Option<Value>,
    /// Earliest time at which the continuation may be dispatched.
    pub continuation_not_before_at: DateTime<Utc>,
}

/// Atomic checkpoint, wake acknowledgement, and optional continuation result.
#[derive(Clone, Debug, PartialEq)]
pub enum RunControllerCompletionOutcome {
    /// The checkpoint and exact wake acknowledgement committed.
    Applied {
        /// Current run after the commit.
        run: Box<ExecutionRunRecord>,
        /// Exactly one continuation outbox record, when requested.
        continuation: Option<Box<ExecutionDispatchRecord>>,
    },
    /// The exact wake had already completed and changed nothing.
    Replayed(Box<ExecutionRunRecord>),
    /// The storage-only checkpoint could not reserve its parked-run capacity.
    CapacitySaturated {
        /// Exact durable capacity dimension that rejected the checkpoint.
        dimension: capacity::ExecutionCapacityDimension,
    },
    /// No visible run exists under the supplied scope.
    NotFound,
    /// The request did not name the current controller generation.
    StaleGeneration {
        /// Current persisted controller generation.
        current_generation: u64,
    },
    /// The activation lost its exact wake fence.
    StaleWake {
        /// Current persisted wake epoch.
        current_wake_epoch: u64,
        /// Greatest wake epoch already acknowledged by the controller.
        processed_wake_epoch: u64,
    },
    /// The run lifecycle cannot accept this completion.
    InvalidState,
}

/// Result of materializing the exact current run-deadline trigger.
#[derive(Clone, Debug, PartialEq)]
pub enum RunDeadlineArmOutcome {
    /// The current generation's immutable deadline trigger is durable.
    Armed(Box<trigger::ExecutionTriggerWrite>),
    /// The run has no approved deadline.
    NoDeadline,
    /// No visible run exists under the supplied scope.
    NotFound,
    /// The supplied generation is no longer current.
    StaleGeneration {
        /// Current persisted controller generation.
        current_generation: u64,
    },
    /// The run is terminal and owns no new deadline trigger.
    Terminal,
}

/// Result of confirming an awaiting execution run.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfirmationOutcome {
    /// The displayed plan hash matched and the approved budget was persisted.
    Confirmed(ExecutionRunRecord),
    /// The same plan hash and budget were already confirmed.
    AlreadyConfirmed(ExecutionRunRecord),
    /// No visible run exists.
    NotFound,
    /// Confirmation differed from persisted state and changed nothing.
    Conflict(ConfirmationConflict),
}

/// Stable reason a run confirmation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfirmationConflict {
    /// The displayed plan hash no longer matches the active plan.
    PlanHashMismatch,
    /// A replay supplied a different approved budget.
    BudgetMismatch,
    /// The run is not awaiting confirmation and is not an exact confirmed replay.
    InvalidStatus,
}

/// Closed first-materialization marker for one aggregate execution node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExecutionNodeMaterialization {
    /// One map node with its exact resolved item count.
    Map {
        /// Stable plan node identifier.
        node_id: String,
        /// Exact fan-out item count, including zero.
        fanout_items: u64,
    },
    /// One reducer node with its exact tree depth.
    Reduce {
        /// Stable plan node identifier.
        node_id: String,
        /// Exact reducer tree depth.
        reducer_depth: u64,
    },
}

impl ExecutionNodeMaterialization {
    fn node_id(&self) -> &str {
        match self {
            Self::Map { node_id, .. } | Self::Reduce { node_id, .. } => node_id,
        }
    }

    const fn kind_label(&self) -> &'static str {
        match self {
            Self::Map { .. } => "map",
            Self::Reduce { .. } => "reduce",
        }
    }

    const fn fanout_items(&self) -> Option<u64> {
        match self {
            Self::Map { fanout_items, .. } => Some(*fanout_items),
            Self::Reduce { .. } => None,
        }
    }

    const fn reducer_depth(&self) -> Option<u64> {
        match self {
            Self::Map { .. } => None,
            Self::Reduce { reducer_depth, .. } => Some(*reducer_depth),
        }
    }
}

/// First-applied task and aggregate-marker evidence from one materialization transaction.
#[derive(Clone, Debug, PartialEq)]
pub struct MaterializationEvidence {
    /// Complete persisted task projections for the requested node.
    pub tasks: Vec<ExecutionTaskRecord>,
    /// Stable task IDs inserted by this transaction, sorted ascending.
    pub inserted_task_ids: Vec<ExecutionTaskId>,
    /// First-applied aggregate marker, when this was a map or reducer node.
    pub marker: Option<ExecutionNodeMaterialization>,
}

/// Durable outcome of one node materialization transaction.
#[derive(Clone, Debug, PartialEq)]
pub enum MaterializationOutcome {
    /// This transaction first applied task rows or its aggregate marker.
    Applied(MaterializationEvidence),
    /// The exact tasks and marker were already committed.
    Replayed {
        /// Complete persisted task projections for the requested node.
        tasks: Vec<ExecutionTaskRecord>,
    },
    /// The durable node identity already carries different materialization semantics.
    Conflict,
}

/// Result of atomically reserving one task's five-dimensional estimate.
#[derive(Clone, Debug, PartialEq)]
pub enum ReservationOutcome {
    /// Budget was reserved and the task moved to reserved.
    Reserved(ExecutionTaskRecord),
    /// The same generation is already reserved.
    AlreadyReserved(ExecutionTaskRecord),
    /// Admission failed and the typed terminal outcome committed atomically.
    Terminalized(Box<ReservationTerminalization>),
    /// The same generation's terminal admission result was already committed.
    AlreadyTerminalized(Box<ReservationTerminalization>),
    /// No visible task exists.
    NotFound,
    /// Reservation changed nothing.
    Rejected(ReservationRejection),
}

/// Stable reason a task reservation was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationRejection {
    /// The supplied generation is stale or otherwise mismatched.
    GenerationMismatch,
    /// The task is not pending.
    InvalidTaskStatus,
    /// The run is not queued or running.
    InvalidRunStatus,
    /// The approved deadline has elapsed.
    DeadlineElapsed,
    /// At least one resource dimension cannot be reserved.
    BudgetExceeded,
}

/// Committed task and run projections for a terminal reservation rejection.
#[derive(Clone, Debug, PartialEq)]
pub struct ReservationTerminalization {
    /// Updated run with reconciled accounting and advanced wake epoch.
    pub run: ExecutionRunRecord,
    /// Generation-fenced typed terminal task projection.
    pub task: ExecutionTaskRecord,
    /// Stable admission reason represented by the task outcome.
    pub rejection: ReservationRejection,
}

/// Result of a generation-fenced task state transition.
#[derive(Clone, Debug, PartialEq)]
pub enum TransitionOutcome {
    /// The transition was applied.
    Applied(ExecutionTaskRecord),
    /// A run wait transition was applied.
    RunApplied(ExecutionRunRecord),
    /// The requested target state already exists for the same generation.
    AlreadyApplied(ExecutionTaskRecord),
    /// The requested run wait state already exists.
    RunAlreadyApplied(ExecutionRunRecord),
    /// No visible task exists.
    NotFound,
    /// Source status or generation did not match.
    Rejected(TransitionRejection),
}

/// Stable reason a task transition was rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionRejection {
    /// The supplied generation is stale or otherwise mismatched.
    GenerationMismatch,
    /// The task source status does not permit the operation.
    InvalidTaskStatus,
    /// The run status does not permit the operation.
    InvalidRunStatus,
    /// The approved run deadline elapsed before redispatch admission.
    DeadlineElapsed,
    /// The current run envelope has no remaining resource admission.
    BudgetExceeded,
    /// A counter increment overflowed its persisted representation.
    CounterOverflow,
}

/// Resume operation applied to one waiting or retryable task.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResumeKind {
    /// Resume a waiting-input task, preserving attempt and incrementing generation.
    Input,
    /// Dispatch a retry, incrementing both attempt and generation.
    Retry,
}

/// Result of recording one task outcome message.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskOutcomeWrite {
    /// The outcome was accepted and reconciled into the current projection.
    Applied {
        /// Updated run projection committed in the same transaction.
        run: ExecutionRunRecord,
        /// Updated task projection.
        task: ExecutionTaskRecord,
        /// Whether this write caused or retained a budget overrun.
        budget_overrun: bool,
    },
    /// The same generation and outcome were already accepted durably.
    Replayed {
        /// Current persisted run projection associated with the first accepted outcome.
        run: ExecutionRunRecord,
        /// Current task projection recovered from the accepted audit entry.
        task: ExecutionTaskRecord,
        /// Current persisted run overrun flag associated with the handoff.
        budget_overrun: bool,
    },
    /// The message was appended to audit history but rejected from current state.
    Rejected {
        /// Updated task containing the rejected audit entry.
        task: ExecutionTaskRecord,
        /// Stable rejection reason.
        reason: TaskOutcomeRejection,
    },
    /// No visible task exists.
    NotFound,
}

/// Stable reason an outcome message was audit-only.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcomeRejection {
    /// The received generation is not current.
    StaleGeneration,
    /// The task is already terminal.
    TerminalTask,
    /// The owning run is cancelled or otherwise terminal.
    TerminalRun,
    /// The task is not currently running.
    InvalidTaskStatus,
    /// Reported cumulative usage moved backward.
    NonCumulativeUsage,
    /// The outcome schema version is unsupported.
    UnsupportedSchemaVersion,
}

/// Compiler-validated amendment data persisted under a revision fence.
#[derive(Clone, Debug)]
pub struct ValidatedAmendment {
    /// Exact compiler-validated patch.
    pub amendment: PlanAmendment,
    /// Domain-separated hash of the patch.
    pub amendment_hash: ExecutionHash,
    /// Replacement canonical active plan.
    pub active_plan: CanonicalExecutionPlan,
    /// Mapping from added/replaced node IDs to unresolved goal requirements.
    pub requirement_mapping: BTreeMap<String, Vec<String>>,
    /// Waiting-replan task superseded by this amendment.
    pub superseded_task_id: ExecutionTaskId,
}

/// Result of an amendment append.
#[derive(Clone, Debug, PartialEq)]
pub struct AmendmentCommit {
    /// Run projection carrying the committed wake epoch and revision.
    pub run: ExecutionRunRecord,
    /// Stable task workflows superseded or terminalized by the amendment.
    pub task_ids_to_release: Vec<ExecutionTaskId>,
}

/// Result of checking persisted amendment identity before current-revision validation.
#[derive(Clone, Debug, PartialEq)]
pub enum AmendmentReplayOutcome {
    /// No matching committed amendment exists and the requested base revision is current.
    NotApplied,
    /// The exact revision/hash/audit identity was already committed.
    Replayed(Box<AmendmentCommit>),
    /// No visible run exists.
    NotFound,
    /// A different revision or amendment occupies the requested durable identity.
    Conflict,
}

/// Result of an amendment append.
#[derive(Clone, Debug, PartialEq)]
pub enum AmendmentWrite {
    /// The revision was fenced, history appended, and waiting task superseded.
    Applied(Box<AmendmentCommit>),
    /// The same revision/hash/audit identity was already committed.
    Replayed(Box<AmendmentCommit>),
    /// No visible run exists.
    NotFound,
    /// Current revision, status, plan, or superseded task did not match.
    Conflict,
}

/// Cursor for stable ascending task pagination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionTaskCursor {
    /// Node identifier of the last returned task.
    pub node_id: String,
    /// Item key of the last returned task.
    pub item_key: String,
    /// Stable task ID of the last returned task.
    pub task_id: ExecutionTaskId,
}

/// Bounded task-list request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExecutionTaskPageRequest {
    /// Requested page size, capped at 1,000. Zero selects the default of 100.
    pub limit: u32,
    /// Exclusive pagination cursor.
    pub cursor: Option<ExecutionTaskCursor>,
}

/// One bounded page of execution tasks.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionTaskPage {
    /// Visible tasks in stable ascending order.
    pub tasks: Vec<ExecutionTaskRecord>,
    /// Cursor for the next page, when more tasks exist.
    pub next_cursor: Option<ExecutionTaskCursor>,
}

/// Cursor for stable descending run pagination.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExecutionRunCursor {
    /// Creation timestamp of the last returned run.
    pub created_at: DateTime<Utc>,
    /// Stable run identifier of the last returned run.
    pub run_uid: Uuid,
}

/// Bounded run-list request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExecutionRunPageRequest {
    /// Requested page size, capped at 1,000. Zero selects the default of 100.
    pub limit: u32,
    /// Exclusive descending pagination cursor.
    pub cursor: Option<ExecutionRunCursor>,
}

/// One bounded page of execution runs.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionRunPage {
    /// Visible runs in descending creation order.
    pub runs: Vec<ExecutionRunRecord>,
    /// Cursor for the next page, when more runs exist.
    pub next_cursor: Option<ExecutionRunCursor>,
}

/// Atomic scheduler input loaded from one active run revision.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionSchedulingSnapshot {
    /// Complete durable run record.
    pub run: ExecutionRunRecord,
    /// Exact persisted immutable capability catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Exact persisted immutable authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Exact persisted immutable instruction-skill revisions.
    pub pinned_instruction_skills: Vec<PinnedInstructionSkill>,
    /// Current persisted budget ledger.
    pub budget_ledger: BudgetLedger,
    /// Complete ordered task and node projection for the active plan revision.
    pub projection: ExecutionProjection,
}

/// Scoped repository for durable execution runs and logical tasks.
#[derive(Clone, Debug)]
pub struct ExecutionRepository {
    pool: PgPool,
}

impl ExecutionRepository {
    /// Creates a repository over the shared Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DbBudgetLimit {
    max_cost_microusd: Option<i64>,
    max_tokens: Option<i64>,
    max_tasks: Option<i64>,
    max_tool_calls: Option<i64>,
    max_retrieved_bytes: Option<i64>,
    deadline_at: Option<DateTime<Utc>>,
}

impl TryFrom<&ExecutionBudgetLimit> for DbBudgetLimit {
    type Error = Error;

    fn try_from(value: &ExecutionBudgetLimit) -> Result<Self> {
        Ok(Self {
            max_cost_microusd: to_optional_i64(value.max_cost_microusd, "max cost")?,
            max_tokens: to_optional_i64(value.max_tokens, "max tokens")?,
            max_tasks: to_optional_i64(value.max_tasks, "max tasks")?,
            max_tool_calls: to_optional_i64(value.max_tool_calls, "max tool calls")?,
            max_retrieved_bytes: to_optional_i64(value.max_retrieved_bytes, "max retrieved bytes")?,
            deadline_at: value.deadline_at,
        })
    }
}

fn to_optional_i64(value: Option<u64>, field: &str) -> Result<Option<i64>> {
    value.map(|value| to_i64(value, field)).transpose()
}

fn to_i64(value: u64, field: &str) -> Result<i64> {
    i64::try_from(value).map_err(|_| Error::InvalidRepositoryInput {
        message: format!("{field} exceeds PostgreSQL BIGINT"),
    })
}

fn to_u64(value: i64, field: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::InvalidRepositoryData {
        message: format!("{field} is negative"),
    })
}

fn to_u32(value: i32, field: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::InvalidRepositoryData {
        message: format!("{field} is negative"),
    })
}

fn storage_error(error: moa_core::error::MoaError) -> Error {
    match error {
        moa_core::error::MoaError::StorageUnavailable(message) => {
            Error::StorageUnavailable { message }
        }
        terminal => Error::Storage {
            message: terminal.to_string(),
        },
    }
}

fn sqlx_error(error: sqlx::Error) -> Error {
    Error::Database { source: error }
}

fn row_error(error: sqlx::Error) -> Error {
    Error::InvalidRepositoryData {
        message: error.to_string(),
    }
}

#[cfg(test)]
mod storage_error_tests {
    use super::*;

    #[test]
    fn repository_conversions_keep_retry_and_decode_boundaries_distinct() {
        // Pins: SQL execution retains concrete SQLx provenance, shared transient
        // failures remain retryable, and row decoding is deterministic corruption.
        let direct = sqlx_error(sqlx::Error::PoolTimedOut);
        assert!(direct.is_retryable_storage());
        assert!(matches!(
            direct,
            Error::Database {
                source: sqlx::Error::PoolTimedOut
            }
        ));

        let scoped = storage_error(moa_core::error::MoaError::StorageUnavailable(
            "database restarting".to_string(),
        ));
        assert!(scoped.is_retryable_storage());
        assert!(matches!(scoped, Error::StorageUnavailable { .. }));

        let corrupt_row = row_error(sqlx::Error::ColumnNotFound("status".to_string()));
        assert!(!corrupt_row.is_retryable_storage());
        assert!(matches!(corrupt_row, Error::InvalidRepositoryData { .. }));
    }
}

#[cfg(test)]
mod tests;
