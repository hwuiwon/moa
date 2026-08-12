//! Public pure execution projection, task, waiting, and terminal state types.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};

use moa_artifacts::{
    execution_plan::{
        CapabilityReference, ExecutionCitation, ExecutionCompensation, ExecutionFailureClass,
        ExecutionTaskOutcome, ExecutionTaskResult, ExecutionTemporalTarget, ExecutionUsage,
        ExecutionWaitPolicy, InputAudience, RetryPolicy,
    },
    reference::ArtifactRef,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::replan::ReplanStopReason;
use crate::{Error, Result, capability::ExecutionEstimate};

const TASK_NAMESPACE_NAME: &str = "https://moa.ai/execution-task";
const COMPENSATION_NAMESPACE_NAME: &str = "https://moa.ai/execution-compensation";

/// Stable UUIDv5 newtype for one logical execution task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ExecutionTaskId(Uuid);

impl ExecutionTaskId {
    /// Wraps a UUID loaded from durable execution persistence.
    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Derives the stable task ID from length-framed run UUID, node ID, and item key bytes.
    pub fn derive(run_uid: Uuid, node_id: &str, item_key: &str) -> Result<Self> {
        let namespace = Uuid::new_v5(&Uuid::NAMESPACE_URL, TASK_NAMESPACE_NAME.as_bytes());
        let mut name = Vec::with_capacity(28 + node_id.len() + item_key.len());
        append_frame(&mut name, run_uid.as_bytes())?;
        append_frame(&mut name, node_id.as_bytes())?;
        append_frame(&mut name, item_key.as_bytes())?;
        Ok(Self(Uuid::new_v5(&namespace, &name)))
    }

    /// Returns the wrapped UUID.
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for ExecutionTaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Stable UUIDv5 newtype for the compensation paired with one forward task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct CompensationId(Uuid);

impl CompensationId {
    /// Wraps a UUID loaded from durable compensation persistence.
    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// Derives the stable compensation identity from the immutable forward-task identity.
    #[must_use]
    pub fn derive(forward_task_id: ExecutionTaskId) -> Self {
        let namespace = Uuid::new_v5(&Uuid::NAMESPACE_URL, COMPENSATION_NAMESPACE_NAME.as_bytes());
        Self(Uuid::new_v5(
            &namespace,
            forward_task_id.as_uuid().as_bytes(),
        ))
    }

    /// Returns the wrapped UUID.
    #[must_use]
    pub const fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for CompensationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Aggregate status of one plan node.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionNodeStatus {
    /// Node has not started.
    Pending,
    /// At least one node task is running.
    Running,
    /// Node is waiting for input, review, signal, or replan.
    Waiting,
    /// Every required node task completed.
    Completed,
    /// Node condition evaluated false.
    Skipped,
    /// Node ended in terminal failure.
    Failed,
    /// Node was cancelled.
    Cancelled,
}

/// Durable status of one execution run.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionRunStatus {
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
    /// Every required completion check passed.
    Completed,
    /// Useful work exists but the goal contract is not fully satisfied.
    Partial,
    /// A live input, review, signal, or authorization condition blocked progress.
    Blocked,
    /// Every available serving path for required work was unsupported.
    Unsupported,
    /// No required result could be produced.
    Failed,
    /// The run was explicitly cancelled.
    Cancelled,
}

impl ExecutionRunStatus {
    /// Returns the stable database and wire label.
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
            Self::Completed => "completed",
            Self::Partial => "partial",
            Self::Blocked => "blocked",
            Self::Unsupported => "unsupported",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Returns whether this run status is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Partial
                | Self::Blocked
                | Self::Unsupported
                | Self::Failed
                | Self::Cancelled
        )
    }
}

impl FromStr for ExecutionRunStatus {
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
            "completed" => Ok(Self::Completed),
            "partial" => Ok(Self::Partial),
            "blocked" => Ok(Self::Blocked),
            "unsupported" => Ok(Self::Unsupported),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution run status `{value}`"),
            }),
        }
    }
}

/// Durable lifecycle state of one registered compensation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompensationStatus {
    /// The compensator is waiting to be claimed.
    Pending,
    /// The current generation is executing.
    Running,
    /// The compensator completed successfully.
    Completed,
    /// The compensator failed terminally and requires manual repair.
    Failed,
    /// The effect may have been applied but cannot be proven and requires manual repair.
    UnknownOutcome,
}

impl CompensationStatus {
    /// Returns the stable database and wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::UnknownOutcome => "unknown_outcome",
        }
    }

    /// Returns whether no automatic execution remains for this registration.
    #[must_use]
    pub const fn is_settled(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::UnknownOutcome)
    }
}

impl FromStr for CompensationStatus {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "unknown_outcome" => Ok(Self::UnknownOutcome),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution compensation status `{value}`"),
            }),
        }
    }
}

/// Original terminal result held while compensations execute.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PendingExecutionTerminal {
    /// Terminal run status to install after every compensation completes.
    pub status: ExecutionRunStatus,
    /// Stable normalized terminal reason.
    pub reason: ExecutionTerminalReason,
    /// Complete typed terminal evidence, including requirement counts.
    pub terminal_evidence: ExecutionTerminalEvidence,
    /// Persisted completion-check evidence selected before compensation began.
    pub completion_check_results: Vec<Value>,
    /// Explicit terminal completion gaps selected before compensation began.
    pub terminal_gaps: Vec<String>,
    /// Structured terminal output selected before compensation began.
    pub output: Option<Value>,
    /// User-supplied cancellation reason, present only for cancelled intent.
    pub cancellation_reason: Option<String>,
}

impl PendingExecutionTerminal {
    /// Validates that this intent represents a real terminal state.
    pub fn validate(&self) -> Result<()> {
        if !self.status.is_terminal() {
            return Err(Error::InvalidRepositoryInput {
                message: "pending compensation terminal status must be terminal".to_string(),
            });
        }
        if (self.status == ExecutionRunStatus::Cancelled) != self.cancellation_reason.is_some() {
            return Err(Error::InvalidRepositoryInput {
                message: "pending cancellation reason must be present exactly for cancelled intent"
                    .to_string(),
            });
        }
        if self
            .cancellation_reason
            .as_deref()
            .is_some_and(|reason| reason.trim().is_empty())
        {
            return Err(Error::InvalidRepositoryInput {
                message: "pending cancellation reason must not be blank".to_string(),
            });
        }
        if self.terminal_evidence.satisfied_requirement_count
            > self.terminal_evidence.requirement_count
        {
            return Err(Error::InvalidRepositoryInput {
                message: "pending terminal satisfied requirements exceed requirement count"
                    .to_string(),
            });
        }
        Ok(())
    }
}

/// Typed outcome returned by one compensation attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionCompensationOutcome {
    /// The compensator committed its exact undo effect.
    Completed {
        /// Structured compensator output.
        output: Value,
        /// Cumulative actual usage across compensation attempts.
        usage: ExecutionUsage,
    },
    /// The attempt failed before a successful undo could be committed.
    Failed {
        /// Stable diagnostic message.
        message: String,
        /// Whether the persisted contract permits another automatic attempt.
        retryable: bool,
        /// Cumulative actual usage across compensation attempts.
        usage: ExecutionUsage,
    },
    /// The compensator may have committed and must never be resent automatically.
    UnknownOutcome {
        /// Stable diagnostic message for manual reconciliation.
        message: String,
        /// Cumulative conservatively observed usage across compensation attempts.
        usage: ExecutionUsage,
    },
}

impl ExecutionCompensationOutcome {
    /// Returns cumulative usage reported by this compensation generation.
    #[must_use]
    pub const fn usage(&self) -> &ExecutionUsage {
        match self {
            Self::Completed { usage, .. }
            | Self::Failed { usage, .. }
            | Self::UnknownOutcome { usage, .. } => usage,
        }
    }
}

/// Immutable registered compensation and its generation-fenced lifecycle projection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationRegistrationProjection {
    /// Stable registration identifier.
    pub compensation_id: CompensationId,
    /// Owning execution run.
    pub run_uid: Uuid,
    /// Forward task whose committed effect is undone.
    pub forward_task_id: ExecutionTaskId,
    /// Monotonic commit sequence used for strict reverse-order execution.
    pub registered_sequence: u64,
    /// Accepted forward-task generation that created the registration.
    pub forward_generation: u64,
    /// Immutable exact compensator contract.
    pub compensator: ExecutionCompensation,
    /// Fully resolved and schema-validated compensator input.
    pub mapped_input: Value,
    /// Current durable compensation status.
    pub status: CompensationStatus,
    /// One-based compensation attempt.
    pub attempt: u64,
    /// One-based compensation dispatch generation.
    pub generation: u64,
    /// Latest accepted typed outcome.
    pub outcome: Option<ExecutionCompensationOutcome>,
    /// Structured terminal error projection.
    pub error: Option<Value>,
    /// Creation timestamp.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Last mutation timestamp.
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// First claim timestamp.
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Settlement timestamp.
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Closed normalized source cohort for an execution run.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionSourceKind {
    /// One-off model-generated plan.
    GeneratedPlan,
    /// Exact pinned skill execution template.
    SkillTemplate,
    /// Experiment-owned pinned execution template.
    ExperimentTemplate,
}

impl ExecutionSourceKind {
    /// Returns the stable database and metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GeneratedPlan => "generated_plan",
            Self::SkillTemplate => "skill_template",
            Self::ExperimentTemplate => "experiment_template",
        }
    }
}

impl FromStr for ExecutionSourceKind {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "generated_plan" => Ok(Self::GeneratedPlan),
            "skill_template" => Ok(Self::SkillTemplate),
            "experiment_template" => Ok(Self::ExperimentTemplate),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution source kind `{value}`"),
            }),
        }
    }
}

/// Closed normalized reason why one execution run became terminal.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTerminalReason {
    /// Every completion gate passed.
    Completed,
    /// Ordinary completion left the goal incomplete.
    GoalIncomplete,
    /// Approved resource budget was exceeded.
    BudgetExceeded,
    /// Approved deadline elapsed.
    DeadlineExceeded,
    /// An authorized caller cancelled the run.
    Cancelled,
    /// Scheduling or replanning could not make progress.
    NoProgress,
    /// Replanning repeated a prior plan.
    DuplicatePlan,
    /// Replanning repeated a prior amendment.
    DuplicateAmendment,
    /// Replanning repeated the same normalized failure.
    RepeatedFailure,
    /// Remaining approved resources could not admit replacement work.
    BudgetExhausted,
    /// A typed task failure ended required work.
    TaskFailure,
    /// Every available serving path was unsupported.
    UnsupportedPlan,
    /// A live wait or authorization condition blocked progress.
    Blocked,
    /// Execution infrastructure failed outside a typed task result.
    InternalFailure,
    /// At least one exact undo failed or became ambiguous and requires manual repair.
    CompensationFailed,
}

impl ExecutionTerminalReason {
    /// Returns the stable database and metric label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::GoalIncomplete => "goal_incomplete",
            Self::BudgetExceeded => "budget_exceeded",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Cancelled => "cancelled",
            Self::NoProgress => "no_progress",
            Self::DuplicatePlan => "duplicate_plan",
            Self::DuplicateAmendment => "duplicate_amendment",
            Self::RepeatedFailure => "repeated_failure",
            Self::BudgetExhausted => "budget_exhausted",
            Self::TaskFailure => "task_failure",
            Self::UnsupportedPlan => "unsupported_plan",
            Self::Blocked => "blocked",
            Self::InternalFailure => "internal_failure",
            Self::CompensationFailed => "compensation_failed",
        }
    }
}

impl FromStr for ExecutionTerminalReason {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "completed" => Ok(Self::Completed),
            "goal_incomplete" => Ok(Self::GoalIncomplete),
            "budget_exceeded" => Ok(Self::BudgetExceeded),
            "deadline_exceeded" => Ok(Self::DeadlineExceeded),
            "cancelled" => Ok(Self::Cancelled),
            "no_progress" => Ok(Self::NoProgress),
            "duplicate_plan" => Ok(Self::DuplicatePlan),
            "duplicate_amendment" => Ok(Self::DuplicateAmendment),
            "repeated_failure" => Ok(Self::RepeatedFailure),
            "budget_exhausted" => Ok(Self::BudgetExhausted),
            "task_failure" => Ok(Self::TaskFailure),
            "unsupported_plan" => Ok(Self::UnsupportedPlan),
            "blocked" => Ok(Self::Blocked),
            "internal_failure" => Ok(Self::InternalFailure),
            "compensation_failed" => Ok(Self::CompensationFailed),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution terminal reason `{value}`"),
            }),
        }
    }
}

/// Closed typed reason why an execution run became terminal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionTerminalCause {
    /// The scheduler reached deterministic completion evaluation.
    Completion {
        /// Typed limit observed after ordinary work had already reached a terminal projection.
        limit_stop: Option<ExecutionLimitStop>,
    },
    /// A typed task failure ended required work.
    TaskFailure {
        /// Exact persisted task failure class.
        class: ExecutionFailureClass,
    },
    /// No further work was dispatched because an approved limit stopped scheduling.
    LimitStop {
        /// Exact exhausted limit, with deadline taking precedence over budget.
        reason: ExecutionLimitStop,
    },
    /// Deterministic replan stop policy ended the run.
    ReplanStop {
        /// Exact closed replan stop reason.
        reason: ReplanStopReason,
    },
    /// An authorized caller cancelled the run.
    Cancellation,
    /// Execution infrastructure failed outside a typed task result.
    InternalFailure,
    /// Exact undo failed or became ambiguous after the original terminal decision.
    CompensationFailure {
        /// Original terminal status held while compensation ran.
        original_status: ExecutionRunStatus,
        /// Original terminal reason held while compensation ran.
        original_reason: ExecutionTerminalReason,
        /// Original typed terminal cause.
        original_cause: Box<ExecutionTerminalCause>,
        /// Compensation whose failure requires manual repair.
        compensation_id: CompensationId,
        /// Exact failed or ambiguous compensation outcome.
        outcome: ExecutionCompensationOutcome,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StrictExecutionTerminalCause {
    Completion {
        limit_stop: Option<ExecutionLimitStop>,
    },
    TaskFailure {
        class: ExecutionFailureClass,
    },
    LimitStop {
        reason: ExecutionLimitStop,
    },
    ReplanStop {
        reason: ReplanStopReason,
    },
    Cancellation {},
    InternalFailure {},
    CompensationFailure {
        original_status: ExecutionRunStatus,
        original_reason: ExecutionTerminalReason,
        original_cause: Box<ExecutionTerminalCause>,
        compensation_id: CompensationId,
        outcome: ExecutionCompensationOutcome,
    },
}

impl<'de> Deserialize<'de> for ExecutionTerminalCause {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(
            match StrictExecutionTerminalCause::deserialize(deserializer)? {
                StrictExecutionTerminalCause::Completion { limit_stop } => {
                    Self::Completion { limit_stop }
                }
                StrictExecutionTerminalCause::TaskFailure { class } => Self::TaskFailure { class },
                StrictExecutionTerminalCause::LimitStop { reason } => Self::LimitStop { reason },
                StrictExecutionTerminalCause::ReplanStop { reason } => Self::ReplanStop { reason },
                StrictExecutionTerminalCause::Cancellation {} => Self::Cancellation,
                StrictExecutionTerminalCause::InternalFailure {} => Self::InternalFailure,
                StrictExecutionTerminalCause::CompensationFailure {
                    original_status,
                    original_reason,
                    original_cause,
                    compensation_id,
                    outcome,
                } => Self::CompensationFailure {
                    original_status,
                    original_reason,
                    original_cause,
                    compensation_id,
                    outcome,
                },
            },
        )
    }
}

/// Closed approved-limit reason that stopped execution.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionLimitStop {
    /// The approved deadline elapsed.
    DeadlineExceeded,
    /// At least one approved resource budget was exhausted.
    BudgetExceeded,
}

/// Immutable terminal cause and requirement-coverage replay identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTerminalEvidence {
    /// Exact closed terminal cause.
    pub cause: ExecutionTerminalCause,
    /// Number of goal requirements evidenced as satisfied.
    pub satisfied_requirement_count: u64,
    /// Total number of declared goal requirements.
    pub requirement_count: u64,
}

/// Durable status of one logical execution task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTaskStatus {
    /// Task is materialized and ready for reservation.
    Pending,
    /// Task is admitted to the bounded durable ready queue.
    Ready,
    /// Worst-case budget is reserved.
    Reserved,
    /// One generation-fenced attempt is awaiting durable delivery.
    Dispatching,
    /// Current generation is executing or has a retry scheduled.
    Running,
    /// Task is waiting for audience input.
    WaitingInput,
    /// Task is waiting for a tenant review decision.
    WaitingReview,
    /// Task is waiting for a named external signal.
    WaitingSignal,
    /// Task is waiting for an exact durable timer.
    WaitingTimer,
    /// Task is waiting for an asynchronous external job.
    WaitingExternal,
    /// Task is waiting for a compiler-validated amendment.
    WaitingReplan,
    /// Task completed successfully.
    Completed,
    /// Task was skipped without execution.
    Skipped,
    /// Task ended in terminal failure.
    Failed,
    /// A non-idempotent attempt may have committed and requires reconciliation.
    UnknownOutcome,
    /// Task was cancelled.
    Cancelled,
}

impl ExecutionTaskStatus {
    /// Returns the stable database and wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Reserved => "reserved",
            Self::Dispatching => "dispatching",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::WaitingReview => "waiting_review",
            Self::WaitingSignal => "waiting_signal",
            Self::WaitingTimer => "waiting_timer",
            Self::WaitingExternal => "waiting_external",
            Self::WaitingReplan => "waiting_replan",
            Self::Completed => "completed",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::UnknownOutcome => "unknown_outcome",
            Self::Cancelled => "cancelled",
        }
    }

    /// Returns whether this task status is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Skipped | Self::Failed | Self::UnknownOutcome | Self::Cancelled
        )
    }
}

impl FromStr for ExecutionTaskStatus {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "reserved" => Ok(Self::Reserved),
            "dispatching" => Ok(Self::Dispatching),
            "running" => Ok(Self::Running),
            "waiting_input" => Ok(Self::WaitingInput),
            "waiting_review" => Ok(Self::WaitingReview),
            "waiting_signal" => Ok(Self::WaitingSignal),
            "waiting_timer" => Ok(Self::WaitingTimer),
            "waiting_external" => Ok(Self::WaitingExternal),
            "waiting_replan" => Ok(Self::WaitingReplan),
            "completed" => Ok(Self::Completed),
            "skipped" => Ok(Self::Skipped),
            "failed" => Ok(Self::Failed),
            "unknown_outcome" => Ok(Self::UnknownOutcome),
            "cancelled" => Ok(Self::Cancelled),
            _ => Err(Error::InvalidRepositoryData {
                message: format!("unknown execution task status `{value}`"),
            }),
        }
    }
}

/// Pure projection of one durable logical task.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskProjection {
    /// Stable logical task ID.
    pub task_id: ExecutionTaskId,
    /// Stable plan node ID or synthetic completion-check node ID.
    pub node_id: String,
    /// Stable ordinary, map, reducer, or verifier item key.
    pub item_key: String,
    /// Current durable task status.
    pub status: ExecutionTaskStatus,
    /// One-based execution attempt.
    pub attempt: u32,
    /// One-based dispatch generation fence.
    pub generation: u64,
    /// Resolved task input.
    pub input: Value,
    /// Latest typed cumulative-usage outcome.
    pub outcome: Option<ExecutionTaskOutcome>,
}

/// Pure run projection consumed by scheduling and completion evaluation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProjection {
    /// Active immutable plan revision.
    pub plan_revision: u64,
    /// Ordered node status map.
    pub node_statuses: BTreeMap<String, ExecutionNodeStatus>,
    /// Logical task projections.
    pub tasks: Vec<ExecutionTaskProjection>,
}

/// Compact bounded node/task evidence accepted by restricted plan amendments.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAmendmentProjection {
    /// Active immutable plan revision.
    pub plan_revision: u64,
    /// Exact aggregate status for every compiler-bounded plan node.
    pub node_statuses: BTreeMap<String, ExecutionNodeStatus>,
    /// Nodes with any persisted task materialization or non-pending aggregate state.
    pub started_node_ids: BTreeSet<String>,
    /// Bounded current replan origins; repository correctness requires exactly one.
    pub replan_tasks: Vec<ExecutionTaskProjection>,
}

/// Pure description of one logical task ready for durable materialization or dispatch.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalTask {
    /// Stable logical task ID.
    pub task_id: ExecutionTaskId,
    /// Stable plan or synthetic verifier node ID.
    pub node_id: String,
    /// Stable ordinary, map, reducer, or verifier item key.
    pub item_key: String,
    /// Goal requirements served by this task.
    pub requirement_ids: Vec<String>,
    /// Plan revision that created this task.
    pub plan_revision: u64,
    /// Current one-based dispatch generation.
    pub generation: u64,
    /// Fully resolved structured task input.
    pub input: Value,
    /// Executable task descriptor.
    pub kind: LogicalTaskKind,
    /// Exact rollback contract for a direct capability node, if opted in.
    pub compensation: Option<ExecutionCompensation>,
    /// Retry policy for this logical task.
    pub retry: RetryPolicy,
    /// Worst-case resource reservation held once across retries and resumes.
    pub reservation: ExecutionEstimate,
}

/// Executable descriptor for one ready logical task.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LogicalTaskKind {
    /// Invoke one governed capability.
    Capability {
        /// Registered capability version to invoke.
        reference: CapabilityReference,
    },
    /// Run one bounded task-local agent.
    Agent {
        /// Agent instructions.
        instructions: String,
        /// Activated skills available to the task.
        skill_refs: Vec<ArtifactRef>,
        /// Governed capabilities available to the task.
        capability_refs: Vec<CapabilityReference>,
        /// Maximum autonomous turns.
        max_turns: u32,
    },
    /// Pause for tenant review.
    Review {
        /// Review prompt.
        prompt: String,
        /// Absolute expiry and deterministic expiry action.
        wait_policy: ExecutionWaitPolicy,
    },
    /// Pause for one named signal.
    WaitSignal {
        /// Stable signal name.
        signal_name: String,
        /// Absolute expiry and deterministic expiry action.
        wait_policy: ExecutionWaitPolicy,
    },
    /// Park until an exact or wait-entry-relative timestamp.
    WaitUntil {
        /// Exact or wait-entry-relative wake target.
        wake: ExecutionTemporalTarget,
        /// Structured output installed when the timer fires.
        result: Value,
    },
    /// Validate and persist terminal output.
    Output {
        /// Resolved terminal output value.
        value: Value,
    },
    /// Run one bounded semantic completion verifier.
    CompletionVerifier {
        /// Stable completion-check ID.
        check_id: String,
        /// Verifier instructions.
        instructions: String,
        /// Maximum verifier turns.
        max_turns: u32,
    },
}

impl LogicalTaskKind {
    /// Returns the stable execution metric label for this task kind.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Capability { .. } => "capability",
            Self::Agent { .. } => "agent",
            Self::Review { .. } => "review",
            Self::WaitSignal { .. } => "wait_signal",
            Self::WaitUntil { .. } => "wait_until",
            Self::Output { .. } => "output",
            Self::CompletionVerifier { .. } => "completion_verifier",
        }
    }
}

/// One deterministic storage-only wait transition applied by the repository.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitSettlement {
    /// A `WaitUntil` node reached its resolved wake time.
    TimerElapsed {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
        /// Structured node output declared by the immutable plan.
        output: Value,
    },
    /// An input, review, or signal wait reached its resolved expiry time.
    WaitExpired {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
        /// Deterministic expiry action declared by the immutable plan.
        action: moa_artifacts::execution_plan::ExecutionWaitExpiryAction,
    },
}

/// Reason the pure scheduler cannot currently advance.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitingReason {
    /// One or more tasks are reserved or running.
    RunningTasks,
    /// One task needs declared-audience input.
    Input {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
        /// Audience authorized to answer.
        audience: InputAudience,
        /// Exact task question.
        question: String,
        /// Absolute expiry and deterministic expiry action.
        wait_policy: ExecutionWaitPolicy,
    },
    /// One task needs a tenant review decision.
    Review {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
        /// Exact review prompt.
        prompt: String,
        /// Absolute expiry and deterministic expiry action.
        wait_policy: ExecutionWaitPolicy,
    },
    /// One task awaits a named signal.
    Signal {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
        /// Stable signal name.
        signal_name: String,
        /// Absolute expiry and deterministic expiry action.
        wait_policy: ExecutionWaitPolicy,
    },
    /// One task is parked until an exact or wait-entry-relative timestamp.
    Timer {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
        /// Exact or wait-entry-relative wake target.
        wake: ExecutionTemporalTarget,
    },
    /// One task is awaiting completion of an asynchronous external job.
    External {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
    },
    /// Pending nodes still depend on unfinished predecessors.
    Dependencies {
        /// Stable blocked node IDs.
        node_ids: Vec<String>,
    },
}

/// Terminal run projection returned by the pure scheduler.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum TerminalProjection {
    /// Every completion gate passed.
    Completed {
        /// Validated terminal output.
        output: Value,
    },
    /// Some useful work completed but required scope remains missing.
    Partial {
        /// Optional terminal output.
        output: Option<Value>,
        /// Exact completion gaps.
        gaps: Vec<String>,
    },
    /// Input, review, signal, or authorization prevents progress.
    Blocked {
        /// Optional terminal output.
        output: Option<Value>,
        /// Exact completion gaps.
        gaps: Vec<String>,
    },
    /// Every available serving path for required work was unsupported.
    Unsupported {
        /// Human-readable unsupported reason.
        reason: String,
        /// Exact completion gaps.
        gaps: Vec<String>,
    },
    /// No required result could be produced.
    Failed {
        /// Typed terminal failure.
        failure: ExecutionTaskFailure,
    },
    /// Run was cancelled.
    Cancelled {
        /// Human-readable cancellation reason.
        reason: String,
    },
}

/// Typed task failure persisted in projections and map aggregates.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionTaskFailure {
    /// Stable failure class.
    pub class: ExecutionFailureClass,
    /// Human-readable failure message.
    pub message: String,
    /// Capability associated with the failure, when present.
    pub capability_ref: Option<CapabilityReference>,
}

/// Deterministic aggregate output for one completed map node.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionMapOutput {
    /// Map items sorted by stable item key.
    pub items: Vec<ExecutionMapItem>,
}

/// One terminal map-item record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionMapItem {
    /// Stable encoded item key.
    pub item_key: String,
    /// Terminal item status.
    pub status: ExecutionMapItemStatus,
    /// Structured output when completed.
    pub output: Option<Value>,
    /// Typed failure when failed or dependency-blocked.
    pub failure: Option<ExecutionTaskFailure>,
    /// Cumulative actual usage.
    pub usage: ExecutionUsage,
    /// Provenance citations.
    pub citations: Vec<ExecutionCitation>,
}

/// Terminal map-item status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMapItemStatus {
    /// Item completed successfully.
    Completed,
    /// Item was conditionally skipped.
    Skipped,
    /// Item failed.
    Failed,
    /// Item was cancelled.
    Cancelled,
}

/// Canonical input to execution-failure fingerprinting.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FailureFingerprintInput {
    /// Stable failure class.
    pub class: ExecutionFailureClass,
    /// Stable node ID.
    pub node_id: String,
    /// Optional failed capability reference.
    pub capability_ref: Option<CapabilityReference>,
    /// Human-readable failure message, normalized before hashing.
    pub message: String,
}

/// Derives the durable task status implied by one persisted outcome.
#[must_use]
pub fn task_status_from_outcome(
    outcome: &ExecutionTaskOutcome,
    retry_scheduled: bool,
) -> ExecutionTaskStatus {
    match &outcome.result {
        ExecutionTaskResult::Completed { .. } => ExecutionTaskStatus::Completed,
        ExecutionTaskResult::NeedsInput { .. } => ExecutionTaskStatus::WaitingInput,
        ExecutionTaskResult::NeedsReplan { .. } => ExecutionTaskStatus::WaitingReplan,
        ExecutionTaskResult::Cancelled { .. } => ExecutionTaskStatus::Cancelled,
        ExecutionTaskResult::UnknownOutcome { .. } => ExecutionTaskStatus::UnknownOutcome,
        ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Retryable,
            ..
        } if retry_scheduled => ExecutionTaskStatus::Running,
        ExecutionTaskResult::Failed { .. } => ExecutionTaskStatus::Failed,
    }
}

/// Returns whether one outcome releases the logical task's complete reservation.
#[must_use]
pub fn task_outcome_is_terminal(outcome: &ExecutionTaskOutcome) -> bool {
    !matches!(
        &outcome.result,
        ExecutionTaskResult::NeedsInput { .. }
            | ExecutionTaskResult::NeedsReplan { .. }
            | ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Retryable,
                ..
            }
    )
}

/// Selects the run status implied by an accepted task outcome.
#[must_use]
pub fn run_status_after_task_outcome(
    current: ExecutionRunStatus,
    outcome: &ExecutionTaskOutcome,
) -> ExecutionRunStatus {
    if matches!(
        current,
        ExecutionRunStatus::Compensating
            | ExecutionRunStatus::PauseRequested
            | ExecutionRunStatus::Pausing
            | ExecutionRunStatus::Paused
    ) {
        return current;
    }
    match &outcome.result {
        ExecutionTaskResult::NeedsInput { .. } => ExecutionRunStatus::WaitingInput,
        ExecutionTaskResult::NeedsReplan { .. } => ExecutionRunStatus::WaitingReplan,
        ExecutionTaskResult::Completed { .. }
        | ExecutionTaskResult::Cancelled { .. }
        | ExecutionTaskResult::UnknownOutcome { .. }
        | ExecutionTaskResult::Failed { .. }
            if matches!(
                current,
                ExecutionRunStatus::WaitingInput
                    | ExecutionRunStatus::WaitingReview
                    | ExecutionRunStatus::WaitingSignal
                    | ExecutionRunStatus::WaitingTimer
                    | ExecutionRunStatus::WaitingExternal
            ) =>
        {
            ExecutionRunStatus::Running
        }
        ExecutionTaskResult::Completed { .. }
        | ExecutionTaskResult::Cancelled { .. }
        | ExecutionTaskResult::UnknownOutcome { .. }
        | ExecutionTaskResult::Failed { .. } => current,
    }
}

/// Returns the durable run status represented by one terminal projection.
#[must_use]
pub const fn run_status_from_terminal_projection(
    projection: &TerminalProjection,
) -> ExecutionRunStatus {
    match projection {
        TerminalProjection::Completed { .. } => ExecutionRunStatus::Completed,
        TerminalProjection::Partial { .. } => ExecutionRunStatus::Partial,
        TerminalProjection::Blocked { .. } => ExecutionRunStatus::Blocked,
        TerminalProjection::Unsupported { .. } => ExecutionRunStatus::Unsupported,
        TerminalProjection::Failed { .. } => ExecutionRunStatus::Failed,
        TerminalProjection::Cancelled { .. } => ExecutionRunStatus::Cancelled,
    }
}

/// Converts an exhausted retryable outcome into the fixed terminal failure.
#[must_use]
pub fn exhaust_retry_outcome(
    attempt: u32,
    policy: &RetryPolicy,
    mut outcome: ExecutionTaskOutcome,
) -> ExecutionTaskOutcome {
    if attempt >= policy.max_attempts
        && let ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Retryable,
            message,
        } = &outcome.result
    {
        outcome.result = ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Terminal,
            message: format!("retry policy exhausted after {attempt} attempts: {message}"),
        };
    }
    outcome
}

/// Computes the bounded exponential backoff before the current retry attempt.
#[must_use]
pub fn retry_delay_ms(attempt: u32, policy: &RetryPolicy) -> u64 {
    let exponent = attempt.saturating_sub(2).min(31);
    policy
        .initial_backoff_ms
        .saturating_mul(1_u64 << exponent)
        .min(policy.max_backoff_ms)
}

/// Builds one successful task outcome without citations.
#[must_use]
pub fn completed_task_outcome(output: Value, usage: ExecutionUsage) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage,
        result: ExecutionTaskResult::Completed {
            output,
            citations: Vec::new(),
        },
    }
}

/// Builds one failed task outcome.
#[must_use]
pub fn failed_task_outcome(
    class: ExecutionFailureClass,
    message: String,
    usage: ExecutionUsage,
) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage,
        result: ExecutionTaskResult::Failed { class, message },
    }
}

/// Builds one cancelled task outcome.
#[must_use]
pub fn cancelled_task_outcome(reason: String, usage: ExecutionUsage) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage,
        result: ExecutionTaskResult::Cancelled { reason },
    }
}

/// Parses an agent's final text as a typed task result or ordinary JSON output.
#[must_use]
pub fn parse_agent_task_outcome(text: &str, usage: ExecutionUsage) -> ExecutionTaskOutcome {
    if let Ok(result) = serde_json::from_str::<ExecutionTaskResult>(text) {
        return ExecutionTaskOutcome {
            schema_version: 1,
            usage,
            result,
        };
    }
    match serde_json::from_str::<Value>(text) {
        Ok(output) => completed_task_outcome(output, usage),
        Err(error) => failed_task_outcome(
            ExecutionFailureClass::InvalidOutput,
            format!("agent final response is not JSON: {error}"),
            usage,
        ),
    }
}

/// Returns the attempt and generation counters for a durably scheduled retry.
///
/// Retry dispatch keeps the logical task identity and increments both counters.
pub fn retry_dispatch_counters(attempt: u32, generation: u64) -> Result<(u32, u64)> {
    if attempt == 0 || generation == 0 {
        return Err(Error::InvalidProjection {
            message: "retry counters must be one-based".to_string(),
        });
    }
    let attempt = attempt
        .checked_add(1)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "execution task retry attempt".to_string(),
        })?;
    let generation = generation
        .checked_add(1)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "execution task retry generation".to_string(),
        })?;
    Ok((attempt, generation))
}

/// Returns the unchanged attempt and incremented generation for an input resume.
pub fn input_resume_counters(attempt: u32, generation: u64) -> Result<(u32, u64)> {
    if attempt == 0 || generation == 0 {
        return Err(Error::InvalidProjection {
            message: "input-resume counters must be one-based".to_string(),
        });
    }
    let generation = generation
        .checked_add(1)
        .ok_or_else(|| Error::ArithmeticOverflow {
            context: "execution task input-resume generation".to_string(),
        })?;
    Ok((attempt, generation))
}

/// Rejects a stale task outcome whose dispatch generation is no longer current.
pub fn validate_outcome_generation(current: u64, received: u64) -> Result<()> {
    if current == 0 || received == 0 {
        return Err(Error::InvalidProjection {
            message: "outcome generations must be one-based".to_string(),
        });
    }
    if current != received {
        return Err(Error::InvalidProjection {
            message: format!(
                "stale execution outcome generation {received}; current generation is {current}"
            ),
        });
    }
    Ok(())
}

/// Terminalizes the originating `WaitingReplan` task under the fixed supersession reason.
pub fn supersede_waiting_replan(task: &ExecutionTaskProjection) -> Result<ExecutionTaskProjection> {
    let Some(outcome) = &task.outcome else {
        return Err(Error::InvalidProjection {
            message: "WaitingReplan task has no NeedsReplan outcome".to_string(),
        });
    };
    if task.status != ExecutionTaskStatus::WaitingReplan
        || !matches!(outcome.result, ExecutionTaskResult::NeedsReplan { .. })
    {
        return Err(Error::InvalidProjection {
            message: "only the originating WaitingReplan task may be superseded".to_string(),
        });
    }

    let mut superseded = task.clone();
    superseded.status = ExecutionTaskStatus::Cancelled;
    superseded.outcome = Some(ExecutionTaskOutcome {
        schema_version: 1,
        usage: outcome.usage.clone(),
        result: ExecutionTaskResult::Cancelled {
            reason: "superseded_by_plan_revision".to_string(),
        },
    });
    Ok(superseded)
}

fn append_frame(output: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    let length = u32::try_from(value.len()).map_err(|_| Error::InvalidTaskIdentity {
        message: "task identity component exceeds the four-byte length frame".to_string(),
    })?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use moa_artifacts::execution_plan::{
        ExecutionFailureClass, ExecutionTaskResult, ExecutionUsage, RetryPolicy,
    };
    use serde_json::json;

    use super::{
        CompensationId, CompensationStatus, ExecutionCompensationOutcome, ExecutionLimitStop,
        ExecutionRunStatus, ExecutionTaskId, ExecutionTerminalCause, ExecutionTerminalEvidence,
        ExecutionTerminalReason, PendingExecutionTerminal, exhaust_retry_outcome,
        failed_task_outcome, retry_delay_ms,
    };
    use crate::replan::ReplanStopReason;

    #[test]
    fn terminal_cause_serde_is_closed_and_recursively_strict() {
        let cases = [
            (
                ExecutionTerminalCause::Completion { limit_stop: None },
                json!({"kind":"completion","limit_stop":null}),
            ),
            (
                ExecutionTerminalCause::Completion {
                    limit_stop: Some(ExecutionLimitStop::DeadlineExceeded),
                },
                json!({"kind":"completion","limit_stop":"deadline_exceeded"}),
            ),
            (
                ExecutionTerminalCause::TaskFailure {
                    class: ExecutionFailureClass::InvalidOutput,
                },
                json!({"kind":"task_failure","class":"invalid_output"}),
            ),
            (
                ExecutionTerminalCause::LimitStop {
                    reason: ExecutionLimitStop::BudgetExceeded,
                },
                json!({"kind":"limit_stop","reason":"budget_exceeded"}),
            ),
            (
                ExecutionTerminalCause::ReplanStop {
                    reason: ReplanStopReason::DuplicateAmendment,
                },
                json!({"kind":"replan_stop","reason":"duplicate_amendment"}),
            ),
            (
                ExecutionTerminalCause::Cancellation,
                json!({"kind":"cancellation"}),
            ),
            (
                ExecutionTerminalCause::InternalFailure,
                json!({"kind":"internal_failure"}),
            ),
            (
                ExecutionTerminalCause::CompensationFailure {
                    original_status: ExecutionRunStatus::Cancelled,
                    original_reason: ExecutionTerminalReason::Cancelled,
                    original_cause: Box::new(ExecutionTerminalCause::Cancellation),
                    compensation_id: compensation_id(),
                    outcome: ExecutionCompensationOutcome::Failed {
                        message: "undo rejected".to_string(),
                        retryable: false,
                        usage: usage(),
                    },
                },
                json!({
                    "kind":"compensation_failure",
                    "original_status":"cancelled",
                    "original_reason":"cancelled",
                    "original_cause":{"kind":"cancellation"},
                    "compensation_id": compensation_id().as_uuid(),
                    "outcome":{
                        "kind":"failed",
                        "message":"undo rejected",
                        "retryable":false,
                        "usage":{
                            "cost_microusd":7,
                            "tokens":11,
                            "tool_calls":1,
                            "retrieved_bytes":0,
                        },
                    },
                }),
            ),
        ];
        for (cause, expected) in cases {
            assert_eq!(
                serde_json::to_value(&cause).expect("serialize cause"),
                expected
            );
            assert_eq!(
                serde_json::from_value::<ExecutionTerminalCause>(expected)
                    .expect("deserialize cause"),
                cause
            );
        }
        assert!(
            serde_json::from_value::<ExecutionTerminalCause>(
                json!({"kind":"internal_failure","message":"not schema"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ExecutionTerminalCause>(
                json!({"kind":"limit_stop","reason":"unknown"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<ExecutionTerminalCause>(json!({
                "kind":"compensation_failure",
                "original_status":"cancelled",
                "original_reason":"cancelled",
                "original_cause":{"kind":"cancellation","message":"not schema"},
                "compensation_id": compensation_id().as_uuid(),
                "outcome":{
                    "kind":"failed",
                    "message":"undo rejected",
                    "retryable":false,
                    "usage":{
                        "cost_microusd":7,
                        "tokens":11,
                        "tool_calls":1,
                        "retrieved_bytes":0,
                    },
                },
            }))
            .is_err(),
            "the boxed original cause must stay closed to unknown fields"
        );
        assert!(
            serde_json::from_value::<ExecutionTerminalCause>(json!({
                "kind":"compensation_failure",
                "original_status":"cancelled",
                "original_reason":"cancelled",
                "compensation_id": compensation_id().as_uuid(),
                "outcome":{
                    "kind":"failed",
                    "message":"undo rejected",
                    "retryable":false,
                    "usage":{
                        "cost_microusd":7,
                        "tokens":11,
                        "tool_calls":1,
                        "retrieved_bytes":0,
                    },
                },
            }))
            .is_err(),
            "a compensation failure must never default away the terminal decision it superseded"
        );
    }

    fn compensation_id() -> CompensationId {
        CompensationId::derive(ExecutionTaskId::from_uuid(
            uuid::Uuid::parse_str("019c2222-3333-7444-8555-666666666666")
                .expect("valid compensation forward task UUID"),
        ))
    }

    fn usage() -> ExecutionUsage {
        ExecutionUsage {
            cost_microusd: 7,
            tokens: 11,
            tool_calls: 1,
            retrieved_bytes: 0,
        }
    }

    #[test]
    fn compensation_state_uses_stable_ids_and_closed_labels() {
        // Pins: compensation identity and persisted status labels are stable across replay.
        let forward = ExecutionTaskId::from_uuid(
            uuid::Uuid::parse_str("019c1111-2222-7333-8444-555555555555")
                .expect("valid forward task UUID"),
        );
        assert_eq!(
            CompensationId::derive(forward),
            CompensationId::derive(forward)
        );
        assert_eq!(ExecutionRunStatus::Compensating.as_str(), "compensating");
        assert_eq!(
            "compensating"
                .parse::<ExecutionRunStatus>()
                .expect("known run status"),
            ExecutionRunStatus::Compensating
        );
        assert_eq!(
            CompensationStatus::UnknownOutcome.as_str(),
            "unknown_outcome"
        );
        assert!("unknown".parse::<CompensationStatus>().is_err());
    }

    #[test]
    fn pending_terminal_rejects_invalid_cancellation_and_requirement_evidence() {
        // Pins: repository input validation mirrors the pending-terminal database guards.
        let mut pending = PendingExecutionTerminal {
            status: ExecutionRunStatus::Cancelled,
            reason: ExecutionTerminalReason::Cancelled,
            terminal_evidence: ExecutionTerminalEvidence {
                cause: ExecutionTerminalCause::Cancellation,
                satisfied_requirement_count: 0,
                requirement_count: 1,
            },
            completion_check_results: Vec::new(),
            terminal_gaps: Vec::new(),
            output: None,
            cancellation_reason: Some("user requested cancellation".to_string()),
        };
        assert!(pending.validate().is_ok());

        pending.cancellation_reason = Some("  ".to_string());
        assert!(pending.validate().is_err());
        pending.cancellation_reason = Some("user requested cancellation".to_string());
        pending.terminal_evidence.satisfied_requirement_count = 2;
        assert!(pending.validate().is_err());
    }

    #[test]
    fn retry_transition_exhausts_and_bounds_backoff_from_one_policy() {
        // Pins: task workflow retry outcome and delay decisions come from the pure execution
        // domain and cannot drift between Restate replay and repository persistence.
        let policy = RetryPolicy {
            max_attempts: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 250,
        };
        let usage = moa_artifacts::execution_plan::ExecutionUsage {
            cost_microusd: 0,
            tokens: 4,
            tool_calls: 0,
            retrieved_bytes: 0,
        };

        assert_eq!(retry_delay_ms(2, &policy), 100);
        assert_eq!(retry_delay_ms(3, &policy), 200);
        assert_eq!(retry_delay_ms(4, &policy), 250);
        assert!(matches!(
            exhaust_retry_outcome(
                3,
                &policy,
                failed_task_outcome(
                    ExecutionFailureClass::Retryable,
                    "provider unavailable".to_string(),
                    usage,
                ),
            )
            .result,
            ExecutionTaskResult::Failed {
                class: ExecutionFailureClass::Terminal,
                ..
            }
        ));
    }
}
