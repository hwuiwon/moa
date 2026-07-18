//! Public pure execution projection, task, waiting, and terminal state types.

use std::{collections::BTreeMap, fmt, str::FromStr};

use moa_artifacts::{
    execution_plan::{
        CapabilityReference, ExecutionCitation, ExecutionFailureClass, ExecutionTaskOutcome,
        ExecutionTaskResult, ExecutionUsage, InputAudience, RetryPolicy,
    },
    reference::ArtifactRef,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::replan::ReplanStopReason;
use crate::{Error, Result, capability::ExecutionEstimate};

const TASK_NAMESPACE_NAME: &str = "https://moa.ai/execution-task";

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
    /// The run is waiting for a compiler-validated amendment.
    WaitingReplan,
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
            Self::WaitingReplan => "waiting_replan",
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
            "waiting_replan" => Ok(Self::WaitingReplan),
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
    /// Pending work existed but the scheduler could neither dispatch nor wait.
    SchedulerNoProgress,
    /// Deterministic replan stop policy ended the run.
    ReplanStop {
        /// Exact closed replan stop reason.
        reason: ReplanStopReason,
    },
    /// An authorized caller cancelled the run.
    Cancellation,
    /// Execution infrastructure failed outside a typed task result.
    InternalFailure,
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
    SchedulerNoProgress {},
    ReplanStop {
        reason: ReplanStopReason,
    },
    Cancellation {},
    InternalFailure {},
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
                StrictExecutionTerminalCause::SchedulerNoProgress {} => Self::SchedulerNoProgress,
                StrictExecutionTerminalCause::ReplanStop { reason } => Self::ReplanStop { reason },
                StrictExecutionTerminalCause::Cancellation {} => Self::Cancellation,
                StrictExecutionTerminalCause::InternalFailure {} => Self::InternalFailure,
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

/// Normalized routing fields persisted with every execution run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRouteFields {
    /// Bounded human-readable route rationale.
    pub rationale: String,
}

/// Durable status of one logical execution task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionTaskStatus {
    /// Task is materialized and ready for reservation.
    Pending,
    /// Worst-case budget is reserved.
    Reserved,
    /// Current generation is executing or has a retry scheduled.
    Running,
    /// Task is waiting for audience input.
    WaitingInput,
    /// Task is waiting for a compiler-validated amendment.
    WaitingReplan,
    /// Task completed successfully.
    Completed,
    /// Task was skipped without execution.
    Skipped,
    /// Task ended in terminal failure.
    Failed,
    /// Task was cancelled.
    Cancelled,
}

impl ExecutionTaskStatus {
    /// Returns the stable database and wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Reserved => "reserved",
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::WaitingReplan => "waiting_replan",
            Self::Completed => "completed",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Returns whether this task status is terminal.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Skipped | Self::Failed | Self::Cancelled
        )
    }
}

impl FromStr for ExecutionTaskStatus {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "reserved" => Ok(Self::Reserved),
            "running" => Ok(Self::Running),
            "waiting_input" => Ok(Self::WaitingInput),
            "waiting_replan" => Ok(Self::WaitingReplan),
            "completed" => Ok(Self::Completed),
            "skipped" => Ok(Self::Skipped),
            "failed" => Ok(Self::Failed),
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
        /// Published skills available to the task.
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
    },
    /// Pause for one named signal.
    WaitSignal {
        /// Stable signal name.
        signal_name: String,
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
            Self::Output { .. } => "output",
            Self::CompletionVerifier { .. } => "completion_verifier",
        }
    }
}

/// Pure scheduler decision.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleDecision {
    /// Newly ready logical tasks.
    Ready(Vec<LogicalTask>),
    /// Durable work is waiting on execution or an external condition.
    Waiting(Vec<WaitingReason>),
    /// The run has a terminal projection.
    Terminal(TerminalProjection),
    /// Unfinished nodes exist but no work or wait can advance them.
    NoProgress {
        /// Stable pending node IDs, sorted and duplicate-free.
        pending_node_ids: Vec<String>,
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
    },
    /// One task needs a tenant review decision.
    Review {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
        /// Exact review prompt.
        prompt: String,
    },
    /// One task awaits a named signal.
    Signal {
        /// Stable waiting task ID.
        task_id: ExecutionTaskId,
        /// Stable signal name.
        signal_name: String,
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

/// Compact task summary supplied to a completion verifier.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerifierTaskSummary {
    /// Stable task ID.
    pub task_id: ExecutionTaskId,
    /// Stable node ID.
    pub node_id: String,
    /// Stable task item key.
    pub item_key: String,
    /// Current terminal task status.
    pub status: ExecutionTaskStatus,
    /// Canonical structured-output hash when output exists.
    pub output_hash: Option<crate::capability::ExecutionHash>,
    /// Typed failure when present.
    pub failure: Option<ExecutionTaskFailure>,
    /// Sorted unique citation source IDs.
    pub citation_source_ids: Vec<String>,
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
        ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Retryable,
            ..
        } if retry_scheduled => ExecutionTaskStatus::Running,
        ExecutionTaskResult::Failed { .. } => ExecutionTaskStatus::Failed,
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
    use moa_artifacts::execution_plan::ExecutionFailureClass;
    use serde_json::json;

    use super::{ExecutionLimitStop, ExecutionTerminalCause};
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
                ExecutionTerminalCause::SchedulerNoProgress,
                json!({"kind":"scheduler_no_progress"}),
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
    }
}
