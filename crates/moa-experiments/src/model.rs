//! Typed experiment definitions and run records.

use chrono::{DateTime, Utc};
use moa_artifacts::simulation::ExperimentTargetKind;
use moa_core::{
    types::action_policy::ActionRuleScope,
    types::agent::AgentSessionSelection,
    types::channel::Attachment,
    types::execution_planning::PinnedExecutionTemplateRef,
    types::experiments::ExperimentScorecard,
    types::identifiers::ModelId,
    types::identifiers::SessionId,
    types::resource::{
        RESOURCE_CONTRACT_VERSION, ResourceAmounts, ResourceEnvelope, ResourceError,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Lifecycle state for an experiment run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentRunStatus {
    /// The run has been accepted but has not started execution.
    Accepted,
    /// The run is executing.
    Running,
    /// The run finished successfully.
    Completed,
    /// The run finished with an error.
    Failed,
    /// The run was cancelled before completion.
    Cancelled,
}

impl ExperimentRunStatus {
    /// Returns the persisted database representation for this status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses a status loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "accepted" => Some(Self::Accepted),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Returns true when the status should no longer accept lifecycle rewrites.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Lifecycle state for one experiment trial.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentTrialStatus {
    /// The trial has been created but no execution has started.
    Accepted,
    /// The parent run has reserved a dispatch slot for the trial.
    Dispatched,
    /// The trial is executing.
    Running,
    /// The trial completed successfully.
    Completed,
    /// The trial finished with an error.
    Failed,
    /// The trial was cancelled before completion.
    Cancelled,
}

impl ExperimentTrialStatus {
    /// Returns the persisted database representation for this status.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Dispatched => "dispatched",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses a status loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "accepted" => Some(Self::Accepted),
            "dispatched" => Some(Self::Dispatched),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Returns true when the status should no longer accept lifecycle rewrites.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

/// Durable reason why a trial stopped producing turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentTrialStopReason {
    /// The scenario success criteria were met.
    Success,
    /// The scenario failure criteria were met.
    Failure,
    /// The trial reached the configured turn cap.
    MaxTurns,
    /// The trial reached its configured budget cap.
    BudgetCap,
    /// The simulator indicated it had no more user-visible messages.
    SimulatorDone,
    /// The target session or execution run reached a terminal state.
    TargetTerminal,
    /// The trial stopped because execution failed.
    Error,
    /// The trial stopped because it was cancelled.
    Cancelled,
}

impl ExperimentTrialStopReason {
    /// Returns the persisted database representation for this stop reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::MaxTurns => "max_turns",
            Self::BudgetCap => "budget_cap",
            Self::SimulatorDone => "simulator_done",
            Self::TargetTerminal => "target_terminal",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parses a stop reason loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "max_turns" => Some(Self::MaxTurns),
            "budget_cap" => Some(Self::BudgetCap),
            "simulator_done" => Some(Self::SimulatorDone),
            "target_terminal" => Some(Self::TargetTerminal),
            "error" => Some(Self::Error),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// Micro-US-dollars in one US cent.
///
/// Plan budgets are authored in cents; the runtime ledger is integer micro-USD
/// so a hard limit is never decided by floating-point rounding.
pub const MICRO_USD_PER_CENT: u64 = 10_000;

/// A metered participant in one experiment run's spend.
///
/// Every reservation names the component it pays for, so a finished run can
/// answer "how much of this went to the simulator rather than the target" from
/// the ledger instead of from telemetry that was already discarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentResourceComponent {
    /// The agent or execution run under test.
    Target,
    /// The model that generates simulated user turns.
    Simulator,
    /// A model-backed evaluator scoring terminal evidence.
    Judge,
    /// A tool, sandbox, or MCP dispatch made on behalf of a trial.
    Tool,
}

impl ExperimentResourceComponent {
    /// Every metered component, in stable reporting order.
    pub const ALL: [Self; 4] = [Self::Target, Self::Simulator, Self::Judge, Self::Tool];

    /// Returns the persisted database representation for this component.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Simulator => "simulator",
            Self::Judge => "judge",
            Self::Tool => "tool",
        }
    }

    /// Parses a component loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "target" => Some(Self::Target),
            "simulator" => Some(Self::Simulator),
            "judge" => Some(Self::Judge),
            "tool" => Some(Self::Tool),
            _ => None,
        }
    }
}

/// Durable resource ceiling for one experiment run and each of its trials.
///
/// The run limits bound everything the run may ever spend across every trial,
/// the trial limits bound one trial inside that total, and `deadline_at` is an
/// absolute wall-clock instant after which no further work may start. All three
/// are persisted, so a workflow replay reads the same ceiling it started under
/// rather than recomputing one from a clock that has moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentResourceEnvelope {
    /// Runtime resource contract version the envelope was authored against.
    pub version: u32,
    /// Inclusive ceiling for the whole run.
    pub run_limits: ResourceAmounts,
    /// Inclusive ceiling for any single trial of the run.
    pub trial_limits: ResourceAmounts,
    /// Absolute wall-clock deadline for the run and every trial under it.
    pub deadline_at: DateTime<Utc>,
}

impl ExperimentResourceEnvelope {
    /// Creates an envelope at the current runtime resource contract version.
    #[must_use]
    pub const fn new(
        run_limits: ResourceAmounts,
        trial_limits: ResourceAmounts,
        deadline_at: DateTime<Utc>,
    ) -> Self {
        Self {
            version: RESOURCE_CONTRACT_VERSION,
            run_limits,
            trial_limits,
            deadline_at,
        }
    }

    /// Returns the run-level runtime envelope.
    #[must_use]
    pub const fn run_envelope(&self) -> ResourceEnvelope {
        ResourceEnvelope {
            version: self.version,
            limits: self.run_limits,
            deadline: Some(self.deadline_at),
        }
    }

    /// Returns the per-trial runtime envelope.
    #[must_use]
    pub const fn trial_envelope(&self) -> ResourceEnvelope {
        ResourceEnvelope {
            version: self.version,
            limits: self.trial_limits,
            deadline: Some(self.deadline_at),
        }
    }

    /// Rejects an envelope this build does not implement.
    ///
    /// # Errors
    ///
    /// Returns [`ResourceError::UnsupportedVersion`] when the persisted contract
    /// version is not the one this build enforces.
    pub fn validate(&self) -> Result<(), ResourceError> {
        self.run_envelope().validate()
    }
}

/// Actual usage reconciled against one reservation.
///
/// `amounts.tokens` is the input plus output total the ledger meters; the split
/// is kept beside it because an operator reading a finished run needs to know
/// whether a token overrun came from prompt growth or from output length.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExperimentResourceUsage {
    /// Model input tokens attributed to this reservation.
    pub input_tokens: u64,
    /// Model output tokens attributed to this reservation.
    pub output_tokens: u64,
    /// Metered amounts committed to the ledger.
    pub amounts: ResourceAmounts,
}

impl ExperimentResourceUsage {
    /// No usage at all: the reservation covered work that never happened.
    pub const ZERO: Self = Self {
        input_tokens: 0,
        output_tokens: 0,
        amounts: ResourceAmounts::ZERO,
    };

    /// Builds usage for one completed model call.
    #[must_use]
    pub fn model_call(input_tokens: u64, output_tokens: u64, cost_micro_usd: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            amounts: ResourceAmounts {
                cost_micro_usd,
                tokens: input_tokens.saturating_add(output_tokens),
                turns: 0,
                model_calls: 1,
                tool_calls: 0,
            },
        }
    }

    /// Builds usage for one target turn and everything it consumed.
    #[must_use]
    pub fn target_turn(
        input_tokens: u64,
        output_tokens: u64,
        cost_micro_usd: u64,
        model_calls: u64,
        tool_calls: u64,
    ) -> Self {
        Self {
            input_tokens,
            output_tokens,
            amounts: ResourceAmounts {
                cost_micro_usd,
                tokens: input_tokens.saturating_add(output_tokens),
                turns: 1,
                model_calls,
                tool_calls,
            },
        }
    }

    /// Validates that the model-token split agrees with the metered total.
    ///
    /// # Errors
    ///
    /// Returns [`ExperimentResourceUsageError`] when the input and output token
    /// counts overflow while being summed or do not equal `amounts.tokens`.
    pub fn validate(&self) -> Result<(), ExperimentResourceUsageError> {
        let split_total = self.input_tokens.checked_add(self.output_tokens).ok_or(
            ExperimentResourceUsageError::TokenSplitOverflow {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            },
        )?;
        if split_total != self.amounts.tokens {
            return Err(ExperimentResourceUsageError::TokenTotalMismatch {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
                recorded_tokens: self.amounts.tokens,
            });
        }
        Ok(())
    }

    /// Adds two usage readings, saturating rather than wrapping.
    #[must_use]
    pub fn saturating_add(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            amounts: self
                .amounts
                .checked_add(&other.amounts)
                .unwrap_or(ResourceAmounts {
                    cost_micro_usd: u64::MAX,
                    tokens: u64::MAX,
                    turns: u64::MAX,
                    model_calls: u64::MAX,
                    tool_calls: u64::MAX,
                }),
        }
    }
}

/// Invalid accounting in one actual experiment usage reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExperimentResourceUsageError {
    /// The input and output token split cannot be represented as a `u64` total.
    #[error("experiment input tokens {input_tokens} plus output tokens {output_tokens} overflow")]
    TokenSplitOverflow {
        /// Model input tokens in the invalid reading.
        input_tokens: u64,
        /// Model output tokens in the invalid reading.
        output_tokens: u64,
    },
    /// The split total differs from the token total committed to the ledger.
    #[error(
        "experiment input tokens {input_tokens} plus output tokens {output_tokens} do not match recorded total {recorded_tokens}"
    )]
    TokenTotalMismatch {
        /// Model input tokens in the invalid reading.
        input_tokens: u64,
        /// Model output tokens in the invalid reading.
        output_tokens: u64,
        /// Token total carried in the metered resource amounts.
        recorded_tokens: u64,
    },
}

/// Lifecycle of one durable reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentResourceReservationState {
    /// Capacity is withheld and the dispatch it covers has not settled.
    Open,
    /// Actual usage was committed and the unused remainder freed.
    Reconciled,
    /// The reservation was returned without committing any usage.
    Released,
}

impl ExperimentResourceReservationState {
    /// Returns the persisted database representation for this state.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Reconciled => "reconciled",
            Self::Released => "released",
        }
    }

    /// Parses a reservation state loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "open" => Some(Self::Open),
            "reconciled" => Some(Self::Reconciled),
            "released" => Some(Self::Released),
            _ => None,
        }
    }
}

/// One durable reservation row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentResourceReservationRecord {
    /// Stable reservation identifier.
    pub reservation_uid: Uuid,
    /// Run whose ledger holds the reservation.
    pub run_uid: Uuid,
    /// Trial the reservation was made for, when it was made inside one.
    pub trial_uid: Option<Uuid>,
    /// Deterministic dispatch coordinate, unique inside the run.
    pub reservation_key: String,
    /// Component the reservation pays for.
    pub component: ExperimentResourceComponent,
    /// Current reservation lifecycle state.
    pub state: ExperimentResourceReservationState,
    /// Worst-case amounts withheld from the envelope.
    pub reserved: ResourceAmounts,
    /// Actual usage, once the reservation reconciled.
    pub actual: Option<ExperimentResourceUsage>,
    /// Timestamp when the reservation was granted.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the reservation last changed state.
    pub updated_at: DateTime<Utc>,
}

/// Request for capacity ahead of one paid or side-effecting dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentResourceReservationRequest {
    /// Run whose ledger the request is made against.
    pub run_uid: Uuid,
    /// Trial making the request, when one owns the dispatch.
    pub trial_uid: Option<Uuid>,
    /// Deterministic dispatch coordinate, unique inside the run.
    ///
    /// A Restate replay of the same dispatch must produce the same key: that is
    /// what makes a re-executed journal step find its own reservation instead of
    /// charging the envelope a second time.
    pub reservation_key: String,
    /// Component the dispatch will pay for.
    pub component: ExperimentResourceComponent,
    /// Worst case the dispatch may consume.
    pub worst_case: ResourceAmounts,
}

/// Why a durable reservation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExperimentResourceDenialReason {
    /// The run-level envelope has no room left for the request.
    RunEnvelopeExhausted,
    /// The trial-level envelope has no room left for the request.
    TrialEnvelopeExhausted,
    /// The absolute deadline has passed.
    DeadlineExceeded,
    /// The request itself was not admissible against this ledger.
    Invalid,
}

/// A refused reservation, in a form a durable workflow step can journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentResourceDenial {
    /// Stable machine-readable refusal reason.
    pub reason: ExperimentResourceDenialReason,
    /// Dimension that ran out, when exhaustion caused the refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<moa_core::types::resource::ResourceKind>,
    /// Amount the caller asked to withhold.
    pub requested: u64,
    /// Amount still available before the request.
    pub remaining: u64,
    /// Configured maximum for the exhausted dimension.
    pub limit: u64,
    /// Deadline that was missed, when the deadline caused the refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<DateTime<Utc>>,
    /// Stable operator-facing description.
    pub message: String,
}

/// Ledger level whose limits are being applied to a reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExperimentResourceLimitScope {
    /// Limits shared by every dispatch in the run.
    Run,
    /// Limits shared by every dispatch in one trial.
    Trial,
}

impl ExperimentResourceDenial {
    /// Converts a runtime resource error into a journalable denial.
    #[must_use]
    pub fn from_resource_error(
        error: &ResourceError,
        limit_scope: ExperimentResourceLimitScope,
    ) -> Self {
        let message = error.to_string();
        match error {
            ResourceError::Exhausted {
                kind,
                requested,
                remaining,
                limit,
            } => Self {
                reason: match limit_scope {
                    ExperimentResourceLimitScope::Run => {
                        ExperimentResourceDenialReason::RunEnvelopeExhausted
                    }
                    ExperimentResourceLimitScope::Trial => {
                        ExperimentResourceDenialReason::TrialEnvelopeExhausted
                    }
                },
                kind: Some(*kind),
                requested: *requested,
                remaining: *remaining,
                limit: *limit,
                deadline_at: None,
                message,
            },
            ResourceError::DeadlineExceeded { deadline } => Self {
                reason: ExperimentResourceDenialReason::DeadlineExceeded,
                kind: None,
                requested: 0,
                remaining: 0,
                limit: 0,
                deadline_at: Some(*deadline),
                message,
            },
            _ => Self {
                reason: ExperimentResourceDenialReason::Invalid,
                kind: None,
                requested: 0,
                remaining: 0,
                limit: 0,
                deadline_at: None,
                message,
            },
        }
    }

    /// Returns whether the refusal was caused by the absolute deadline.
    #[must_use]
    pub const fn is_deadline(&self) -> bool {
        matches!(
            self.reason,
            ExperimentResourceDenialReason::DeadlineExceeded
        )
    }
}

/// Outcome of one durable reservation attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExperimentResourceAdmission {
    /// Capacity is withheld; the caller must dispatch and then reconcile.
    Granted(ExperimentResourceReservationRecord),
    /// This exact dispatch already settled, so it must not be issued again.
    AlreadySettled(ExperimentResourceReservationRecord),
    /// The ledger refused; the caller must not dispatch.
    Denied(ExperimentResourceDenial),
}

impl ExperimentResourceAdmission {
    /// Returns the reservation when capacity was withheld for a fresh dispatch.
    #[must_use]
    pub const fn granted(&self) -> Option<&ExperimentResourceReservationRecord> {
        match self {
            Self::Granted(record) => Some(record),
            Self::AlreadySettled(_) | Self::Denied(_) => None,
        }
    }

    /// Returns the refusal when the ledger declined the request.
    #[must_use]
    pub const fn denial(&self) -> Option<&ExperimentResourceDenial> {
        match self {
            Self::Denied(denial) => Some(denial),
            Self::Granted(_) | Self::AlreadySettled(_) => None,
        }
    }
}

/// Usage attributed to one metered component of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentComponentUsage {
    /// Component the usage is attributed to.
    pub component: ExperimentResourceComponent,
    /// Reconciled usage for that component.
    pub usage: ExperimentResourceUsage,
}

/// One run's durable ledger state plus its per-component attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExperimentResourceLedgerState {
    /// Envelope the ledger enforces.
    pub envelope: ExperimentResourceEnvelope,
    /// Reconciled actual usage across the whole run.
    pub committed: ResourceAmounts,
    /// Capacity withheld for dispatches that have not settled.
    pub outstanding: ResourceAmounts,
    /// Capacity still available to reserve.
    pub remaining: ResourceAmounts,
    /// Reservations still open.
    pub open_reservations: u64,
    /// Reconciled usage grouped by component, in stable component order.
    pub by_component: Vec<ExperimentComponentUsage>,
}

impl ExperimentResourceLedgerState {
    /// Returns reconciled usage for one component.
    #[must_use]
    pub fn component(&self, component: ExperimentResourceComponent) -> ExperimentResourceUsage {
        self.by_component
            .iter()
            .find(|entry| entry.component == component)
            .map(|entry| entry.usage)
            .unwrap_or(ExperimentResourceUsage::ZERO)
    }
}

/// Target payload for an experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExperimentTarget {
    /// Run an agent loop prompt through the session execution path.
    ///
    /// An agent-loop experiment never continues a caller-owned session: the
    /// simulator drives live turns into the target and reads its durable event
    /// log, so every run gets an eval-owned session created for it. There is no
    /// field to name an existing session, which makes that unreachable rather
    /// than merely rejected.
    AgentLoop {
        /// User-facing prompt used to start the agent loop.
        prompt: String,
        /// Installed deployment or exact agent revision to pin when creating the session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentSessionSelection>,
        /// Model requested for the agent loop.
        model: ModelId,
        /// Input attachments supplied with the prompt.
        attachments: Vec<Attachment>,
    },
    /// Run one exact published execution-template revision.
    ExecutionTemplate {
        /// Exact published skill template revision.
        template: PinnedExecutionTemplateRef,
        /// Explicit immutable execution objective.
        objective: String,
        /// Structured execution-template input payload.
        input: Value,
        /// Optional caller-owned session; `None` creates an internal experiment session.
        session_id: Option<SessionId>,
        /// Optional idempotency key for execution admission.
        idempotency_key: Option<String>,
    },
}

impl ExperimentTarget {
    /// Returns the target kind discriminator for this payload.
    #[must_use]
    pub const fn kind(&self) -> ExperimentTargetKind {
        match self {
            Self::AgentLoop { .. } => ExperimentTargetKind::AgentLoop,
            Self::ExecutionTemplate { .. } => ExperimentTargetKind::ExecutionTemplate,
        }
    }

    /// Returns the caller-owned session this target attaches to, when present.
    #[must_use]
    pub const fn attached_session_id(&self) -> Option<SessionId> {
        match self {
            Self::AgentLoop { .. } => None,
            Self::ExecutionTemplate { session_id, .. } => *session_id,
        }
    }
}

/// Variant under evaluation for a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentVariant {
    /// Human-readable variant name.
    pub name: String,
    /// Model selected for this variant.
    pub model: Option<ModelId>,
    /// Artifact revision identifiers included in this variant.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Skill artifact references included in this variant.
    pub skill_refs: Vec<String>,
    /// Exact execution-template revision included in this variant.
    pub execution_template: Option<PinnedExecutionTemplateRef>,
    /// Variant-specific metadata.
    pub metadata: Value,
}

/// Simulator settings used when expanding an experiment plan into trials.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentSimulatorConfig {
    /// Model used for simulator user turns.
    pub model: ModelId,
    /// Optional sampling temperature for simulator calls.
    pub temperature: Option<f32>,
    /// Maximum simulator-visible turns allowed for one trial.
    pub max_turns: u32,
    /// Optional total token budget for the simulator side.
    pub token_budget: Option<u32>,
    /// Additional simulator metadata that does not affect storage invariants.
    pub metadata: Value,
}

/// Plan expansion metadata used to create deterministic trial rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentPlanExpansion {
    /// Artifact revision that defined the plan, when the run came from a plan artifact.
    pub plan_revision_uid: Option<Uuid>,
    /// Scenario ID selected from the embedded plan simulation.
    pub scenario_id: Option<String>,
    /// Persona ID selected from the embedded plan simulation.
    pub persona_id: Option<String>,
    /// Profile ID selected from the embedded plan simulation.
    pub profile_id: Option<String>,
    /// Data bundle IDs selected from the embedded plan simulation.
    pub data_bundle_ids: Vec<String>,
    /// Additional artifact revisions pinned by the target variant.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Stable variant key selected from the plan matrix.
    pub variant_key: String,
    /// Optional deterministic simulator seed.
    pub seed: Option<String>,
}

/// Input used to create a durable experiment run row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewExperimentRun {
    /// Human-readable run name.
    pub name: String,
    /// Target payload used for execution.
    pub target: ExperimentTarget,
    /// Variant under evaluation.
    pub variant: ExperimentVariant,
    /// Scorecard attached to this experiment.
    pub scorecard: ExperimentScorecard,
    /// Score run identifier used to join against analytics scores.
    pub score_run_id: Uuid,
    /// Session linked to the experiment run, when one exists.
    pub session_id: Option<SessionId>,
    /// Execution run linked to the experiment run, when one exists.
    pub execution_run_uid: Option<Uuid>,
    /// Artifact revisions associated with this experiment run.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Optional idempotency key for scoped create deduplication.
    pub idempotency_key: Option<String>,
    /// Identity payload that accepted or created the experiment row.
    pub created_by_identity: Value,
    /// Plan artifact this run expands, when the run came from a plan artifact.
    ///
    /// Admission quotas group by the artifact rather than by the revision, so a
    /// caller cannot reset its own per-plan allowance by publishing another
    /// revision of the same plan.
    pub plan_artifact_uid: Option<Uuid>,
    /// Trials this run's matrix will mint, consumed by admission quotas.
    ///
    /// A run that mints no trials still consumes a run slot; this is the trial
    /// load it adds on top of that.
    pub expected_trials: u64,
    /// Durable resource ceiling this run and its trials execute inside.
    pub resource_envelope: ExperimentResourceEnvelope,
}

/// Input used to create a durable experiment trial row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewExperimentTrial {
    /// Experiment run that owns the trial.
    pub run_uid: Uuid,
    /// Deterministic key unique within the owning run.
    pub trial_key: String,
    /// Execution shape targeted by this trial.
    pub target_kind: ExperimentTargetKind,
    /// Stable variant key selected from the plan matrix.
    pub variant_key: String,
    /// Artifact revision that defined the plan used by this trial.
    pub plan_revision_uid: Uuid,
    /// Scenario ID selected from the embedded plan simulation.
    pub scenario_id: Option<String>,
    /// Persona ID selected from the embedded plan simulation.
    pub persona_id: Option<String>,
    /// Profile ID selected from the embedded plan simulation.
    pub profile_id: Option<String>,
    /// Data bundle IDs selected from the embedded plan simulation.
    pub data_bundle_ids: Vec<String>,
    /// Additional artifact revisions pinned by the target variant.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Simulator settings for user-turn generation.
    pub simulator: ExperimentSimulatorConfig,
    /// Target model requested for this trial, when applicable.
    pub target_model: Option<ModelId>,
    /// Optional deterministic simulator seed.
    pub seed: Option<String>,
    /// Score run identifier used for trial-level scores.
    pub score_run_id: Uuid,
}

/// Durable record for one experiment run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentRunRecord {
    /// Artifact/default inheritance scope that owns the experiment row.
    pub scope: ActionRuleScope,
    /// Stable experiment run identifier.
    pub run_uid: Uuid,
    /// Human-readable run name.
    pub name: String,
    /// Fast discriminator for the target payload.
    pub target_kind: ExperimentTargetKind,
    /// Current run lifecycle status.
    pub status: ExperimentRunStatus,
    /// Target payload used for execution.
    pub target: ExperimentTarget,
    /// Variant under evaluation.
    pub variant: ExperimentVariant,
    /// Scorecard attached to this experiment.
    pub scorecard: ExperimentScorecard,
    /// Score run identifier used to join against analytics scores.
    pub score_run_id: Uuid,
    /// Session linked to the experiment run, when one exists.
    pub session_id: Option<SessionId>,
    /// Execution run linked to the experiment run, when one exists.
    pub execution_run_uid: Option<Uuid>,
    /// Artifact revisions associated with this experiment run.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Optional idempotency key for scoped create deduplication.
    pub idempotency_key: Option<String>,
    /// Identity payload that accepted or created the experiment row.
    pub created_by_identity: Value,
    /// Plan artifact this run expands, when the run came from a plan artifact.
    pub plan_artifact_uid: Option<Uuid>,
    /// Durable resource ceiling this run and its trials execute inside.
    pub resource_envelope: ExperimentResourceEnvelope,
    /// Terminal error message for failed runs.
    pub error: Option<String>,
    /// Timestamp when the row was created and the run was accepted.
    pub created_at: DateTime<Utc>,
    /// Timestamp when execution started.
    pub started_at: Option<DateTime<Utc>>,
    /// Timestamp when execution reached a terminal state.
    pub completed_at: Option<DateTime<Utc>>,
    /// Timestamp when the record was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Durable record for one experiment trial.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExperimentTrialRecord {
    /// Artifact/default inheritance scope that owns the trial row.
    pub scope: ActionRuleScope,
    /// Stable trial identifier.
    pub trial_uid: Uuid,
    /// Experiment run that owns the trial.
    pub run_uid: Uuid,
    /// Deterministic key unique within the owning run.
    pub trial_key: String,
    /// Current trial lifecycle status.
    pub status: ExperimentTrialStatus,
    /// Execution shape targeted by this trial.
    pub target_kind: ExperimentTargetKind,
    /// Stable variant key selected from the plan matrix.
    pub variant_key: String,
    /// Artifact revision that defined the plan used by this trial.
    pub plan_revision_uid: Uuid,
    /// Persona ID selected from the embedded plan simulation.
    pub persona_id: Option<String>,
    /// Profile ID selected from the embedded plan simulation.
    pub profile_id: Option<String>,
    /// Scenario ID selected from the embedded plan simulation.
    pub scenario_id: Option<String>,
    /// Data bundle IDs selected from the embedded plan simulation.
    pub data_bundle_ids: Vec<String>,
    /// Additional artifact revisions pinned by the target variant.
    pub artifact_revision_uids: Vec<Uuid>,
    /// Simulator settings for user-turn generation.
    pub simulator: ExperimentSimulatorConfig,
    /// Target model requested for this trial, when applicable.
    pub target_model: Option<ModelId>,
    /// Optional deterministic simulator seed.
    pub seed: Option<String>,
    /// Session linked to the trial, when one exists.
    pub session_id: Option<SessionId>,
    /// Execution run linked to the trial, when one exists.
    pub execution_run_uid: Option<Uuid>,
    /// Score run identifier used for trial-level scores.
    pub score_run_id: Uuid,
    /// Independent digest of the terminal evidence finalized for this trial.
    pub final_evidence_hash: Option<Vec<u8>>,
    /// Number of simulator-target turns persisted for this trial.
    pub turn_count: i32,
    /// Durable per-trial resource ceiling, derived from the owning run envelope.
    ///
    /// Persisted on the trial so a trial workflow reads the same ceiling on
    /// every replay instead of re-deriving one from a clock that has moved.
    pub resource_envelope: ResourceEnvelope,
    /// Durable reason why the trial stopped.
    pub stop_reason: Option<ExperimentTrialStopReason>,
    /// Terminal error message for failed trials.
    pub error: Option<String>,
    /// Trace identifier for observability drill-down.
    pub trace_id: Option<String>,
    /// Timestamp when execution started.
    pub started_at: Option<DateTime<Utc>>,
    /// Timestamp when execution reached a terminal state.
    pub completed_at: Option<DateTime<Utc>>,
    /// Timestamp when the row was created and the trial was accepted.
    pub created_at: DateTime<Utc>,
    /// Timestamp when the record was last updated.
    pub updated_at: DateTime<Utc>,
}

impl From<&ExperimentTrialRecord> for NewExperimentTrial {
    fn from(record: &ExperimentTrialRecord) -> Self {
        Self {
            run_uid: record.run_uid,
            trial_key: record.trial_key.clone(),
            target_kind: record.target_kind,
            variant_key: record.variant_key.clone(),
            plan_revision_uid: record.plan_revision_uid,
            scenario_id: record.scenario_id.clone(),
            persona_id: record.persona_id.clone(),
            profile_id: record.profile_id.clone(),
            data_bundle_ids: record.data_bundle_ids.clone(),
            artifact_revision_uids: record.artifact_revision_uids.clone(),
            simulator: record.simulator.clone(),
            target_model: record.target_model.clone(),
            seed: record.seed.clone(),
            score_run_id: record.score_run_id,
        }
    }
}
