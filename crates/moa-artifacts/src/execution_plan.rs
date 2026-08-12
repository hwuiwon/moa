//! Canonical execution-plan, goal, outcome, and amendment definitions.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::reference::ArtifactRef;

/// Immutable user-derived goal and completion contract for an execution run.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionGoalContract {
    /// User objective preserved for planning and terminal synthesis.
    pub objective: String,
    /// Individually identifiable requirements the run must serve.
    pub requirements: Vec<ExecutionRequirement>,
    /// Structured deliverables the run must produce.
    pub deliverables: Vec<ExecutionDeliverable>,
    /// Required map coverage over a declared item universe.
    pub coverage: Vec<CoverageRequirement>,
    /// User constraints that remain in force throughout execution.
    pub constraints: Vec<ExecutionConstraint>,
    /// Checks that gate successful completion.
    pub completion_checks: Vec<CompletionCheck>,
}

/// One identifiable user requirement.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRequirement {
    /// Stable requirement identifier.
    pub id: String,
    /// Requirement preserved from the user request.
    pub description: String,
}

/// One required structured deliverable.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionDeliverable {
    /// Stable deliverable identifier.
    pub id: String,
    /// Human-readable deliverable description.
    pub description: String,
    /// JSON Pointer locating the deliverable in terminal output.
    pub output_pointer: String,
    /// JSON Schema that the located value must satisfy.
    pub schema: Value,
}

/// Required coverage for one map node.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageRequirement {
    /// Stable coverage requirement identifier.
    pub id: String,
    /// Human-readable coverage description.
    pub description: String,
    /// Stable map node identifier that supplies the coverage.
    pub map_node_id: String,
    /// Expected item universe or structured universe descriptor.
    pub expected_items: Value,
    /// Whether every expected item must complete successfully.
    pub require_all: bool,
}

/// One immutable execution constraint.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionConstraint {
    /// Stable constraint identifier.
    pub id: String,
    /// Constraint preserved from the user request.
    pub description: String,
}

/// One check that gates successful completion.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionCheck {
    /// Stable completion-check identifier.
    pub id: String,
    /// Human-readable completion condition.
    pub description: String,
    /// Stable requirement identifiers verified by this check.
    pub requirement_ids: Vec<String>,
    /// Stable constraint identifiers verified by this check.
    pub constraint_ids: Vec<String>,
    /// Deterministic or bounded semantic check to perform.
    pub kind: CompletionCheckKind,
}

/// Supported execution completion checks.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompletionCheckKind {
    /// Validate the terminal output against its declared schemas.
    OutputSchema,
    /// Require the listed nodes to complete.
    RequiredNodes {
        /// Stable node identifiers that must complete.
        node_ids: Vec<String>,
    },
    /// Require coverage for one map node.
    MapCoverage {
        /// Stable map node identifier to check.
        map_node_id: String,
    },
    /// Require a minimum citation count from each listed node task.
    Citations {
        /// Stable node identifiers whose task citations are checked.
        node_ids: Vec<String>,
        /// Minimum citations required per logical task.
        min_per_task: u32,
    },
    /// Run one bounded semantic verifier agent.
    AgentVerifier {
        /// Instructions supplied to the verifier.
        instructions: String,
        /// Maximum autonomous verifier turns.
        max_turns: u32,
    },
}

/// Canonical immutable execution-plan definition.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlanDefinition {
    /// Explicit policy for effects already committed when the run is cancelled.
    pub cancel_policy: ExecutionCancelPolicy,
    /// Expiry behavior for runtime input requests returned by executable tasks.
    pub input_wait_policy: ExecutionWaitPolicy,
    /// JSON Schema for run input.
    pub input_schema: Value,
    /// JSON Schema for terminal output.
    pub output_schema: Value,
    /// Acyclic execution nodes in deterministic authoring order.
    pub nodes: Vec<ExecutionNode>,
}

/// One node in an execution plan.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionNode {
    /// Stable node identifier.
    pub id: String,
    /// Stable goal requirement identifiers served by this node.
    pub requirement_ids: Vec<String>,
    /// Direct predecessor node identifiers.
    pub depends_on: Vec<String>,
    /// Optional condition evaluated before the node runs.
    pub when: Option<ExecutionCondition>,
    /// Static or reference-bound node input.
    pub input: Value,
    /// JSON Schema for the node's resolved output.
    pub output_schema: Value,
    /// Operation performed by the node.
    pub operation: ExecutionOperation,
    /// Exact opt-in rollback contract for a direct side-effecting capability.
    pub compensation: Option<ExecutionCompensation>,
    /// Retry policy applied to executable work.
    pub retry: RetryPolicy,
    /// Optional per-node resource ceiling.
    pub budget: Option<ExecutionBudgetLimit>,
}

/// Policy for committed effects when an execution run is cancelled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionCancelPolicy {
    /// Keep effects that reached a durable terminal outcome before cancellation.
    RetainEffects,
    /// Undo committed effects whose capability contract promises exact rollback.
    CompensateCommitted,
}

/// Exact rollback contract selected by one direct capability node.
///
/// Compilation accepts this contract only when it exactly matches the forward
/// capability's immutable catalog-owned rollback contract.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCompensation {
    /// Exact governed capability version that reverses the forward effect.
    pub compensator: CapabilityReference,
    /// Bounded construction of compensator input from committed forward values.
    pub input_mapping: CompensationInputMapping,
}

/// Bounded object-field mapping used to construct compensator input.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationInputMapping {
    /// Deterministically ordered target bindings. Compilation rejects duplicates
    /// and more than 64 bindings.
    pub bindings: Vec<CompensationInputBinding>,
}

/// One JSON Pointer target populated from the committed forward invocation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompensationInputBinding {
    /// JSON Pointer in the compensator input object to populate.
    pub target_pointer: String,
    /// Exact committed value source for this binding.
    pub source: CompensationValueSource,
}

/// Values visible to the bounded compensation mapping language.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompensationValueSource {
    /// Read a value from the exact resolved forward capability input.
    OriginalInput {
        /// RFC 6901 JSON Pointer, with an empty pointer selecting the whole input.
        pointer: String,
    },
    /// Read a value from the exact committed forward capability output.
    OriginalOutput {
        /// RFC 6901 JSON Pointer, with an empty pointer selecting the whole output.
        pointer: String,
    },
}

/// Stable reference to one registered governed capability version.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityReference {
    /// Stable capability name.
    pub name: String,
    /// Stable capability version.
    pub version: String,
}

/// Optional condition controlling whether an execution node runs.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionCondition {
    /// Run when the referenced value exists.
    Exists {
        /// Visible input or dependency-output reference.
        reference: ExecutionReference,
    },
    /// Run when the referenced value equals the expected JSON value.
    Equals {
        /// Visible input or dependency-output reference.
        reference: ExecutionReference,
        /// Expected JSON value.
        value: Value,
    },
}

/// Explicit reference to run input or one declared dependency output.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReference {
    /// Restricted execution reference path.
    #[serde(rename = "$ref")]
    pub path: String,
}

/// Retry policy for one execution node or materialized task.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Maximum execution attempts, including the first attempt.
    pub max_attempts: u32,
    /// Initial retry backoff in milliseconds.
    pub initial_backoff_ms: u64,
    /// Maximum retry backoff in milliseconds.
    pub max_backoff_ms: u64,
}

/// Supported non-recursive map task templates.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MapTask {
    /// Invoke one governed capability for each map item.
    Capability {
        /// Registered capability version to invoke.
        reference: CapabilityReference,
    },
    /// Run one bounded agent task for each map item.
    Agent {
        /// Instructions supplied to each agent task.
        instructions: String,
        /// Activated skill artifacts available to each agent task.
        skill_refs: Vec<ArtifactRef>,
        /// Governed capabilities available to each agent task.
        capability_refs: Vec<CapabilityReference>,
        /// Maximum autonomous turns per agent task.
        max_turns: u32,
    },
}

/// Supported reducers for structured map or node outputs.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionReducer {
    /// Reduce each structured batch through one governed capability.
    Capability {
        /// Registered capability version to invoke.
        reference: CapabilityReference,
    },
    /// Reduce hierarchical structured batches through a bounded agent.
    Agent {
        /// Instructions supplied to each reducer agent task.
        instructions: String,
        /// Activated skill artifacts available to the reducer.
        skill_refs: Vec<ArtifactRef>,
        /// Governed capabilities available to the reducer.
        capability_refs: Vec<CapabilityReference>,
        /// Maximum autonomous turns per reducer batch.
        max_turns: u32,
    },
}

/// The eight operations supported by the execution-plan DSL.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOperation {
    /// Invoke one registered governed capability.
    Capability {
        /// Registered capability version to invoke.
        reference: CapabilityReference,
    },
    /// Run one bounded task-local agent.
    Agent {
        /// Instructions supplied to the agent task.
        instructions: String,
        /// Activated skill artifacts available to the agent task.
        skill_refs: Vec<ArtifactRef>,
        /// Governed capabilities available to the agent task.
        capability_refs: Vec<CapabilityReference>,
        /// Maximum autonomous agent turns.
        max_turns: u32,
    },
    /// Materialize one stable logical task for each input item.
    Map {
        /// Static or reference-bound collection of items.
        items: Value,
        /// RFC 6901 JSON Pointer evaluated against each item for its stable key.
        item_key: String,
        /// Maximum logical items this map may materialize.
        max_items: u64,
        /// JSON Schema applied to each completed map-item output.
        item_output_schema: Value,
        /// Non-recursive task template applied to each item.
        task: MapTask,
    },
    /// Hierarchically reduce structured values until one value remains.
    Reduce {
        /// Static or reference-bound collection of values to reduce.
        items: Value,
        /// Maximum values this reducer may consume.
        max_items: u64,
        /// Capability or bounded agent reducer.
        reducer: ExecutionReducer,
        /// Maximum values supplied to each reduction task.
        batch_size: u32,
    },
    /// Pause for a tenant review decision.
    Review {
        /// Review prompt shown to the tenant reviewer.
        prompt: String,
        /// Exact expiry and settlement behavior for the review wait.
        wait_policy: ExecutionWaitPolicy,
    },
    /// Pause for one external or user signal.
    WaitSignal {
        /// Stable signal name awaited by the run.
        signal_name: String,
        /// Exact expiry and settlement behavior for the signal wait.
        wait_policy: ExecutionWaitPolicy,
    },
    /// Park until an exact or wait-entry-relative time without retaining active compute.
    WaitUntil {
        /// Temporal target at which the node becomes ready to continue.
        wake: ExecutionTemporalTarget,
        /// Structured result made available when the timer fires.
        result: Value,
    },
    /// Resolve and validate the plan's terminal output.
    Output {
        /// Static or reference-bound terminal output value.
        value: Value,
    },
}

/// Exact expiry policy for a storage-only execution wait.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionWaitPolicy {
    /// Temporal target at which the unresolved wait expires.
    pub expiry: ExecutionTemporalTarget,
    /// Deterministic settlement applied when the wait expires.
    pub on_expiry: ExecutionWaitExpiryAction,
}

/// Exact or wait-entry-relative target for a durable execution timer.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionTemporalTarget {
    /// Exact absolute UTC instant used by one-off generated and compiled plans.
    At {
        /// Absolute UTC instant at which the timer becomes due.
        at: DateTime<Utc>,
    },
    /// Positive delay resolved when the owning wait state is entered.
    After {
        /// Number of seconds after wait entry at which the timer becomes due.
        delay_seconds: u64,
    },
}

/// Deterministic action applied when an execution wait expires.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionWaitExpiryAction {
    /// Fail only the waiting logical task.
    FailTask,
    /// Fail the complete execution run.
    FailRun,
    /// Settle the wait successfully with a declared structured output.
    ContinueWith {
        /// Structured output supplied to downstream nodes.
        output: Value,
    },
}

/// Shared resource ceiling for an execution run or node.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionBudgetLimit {
    /// Maximum billed cost in integer micro-US-dollars.
    pub max_cost_microusd: Option<u64>,
    /// Maximum model tokens.
    pub max_tokens: Option<u64>,
    /// Maximum logical tasks.
    pub max_tasks: Option<u64>,
    /// Maximum governed tool or capability calls.
    pub max_tool_calls: Option<u64>,
    /// Maximum bytes retrieved from external or memory sources.
    pub max_retrieved_bytes: Option<u64>,
    /// Absolute execution deadline.
    pub deadline_at: Option<DateTime<Utc>>,
}

/// Versioned outcome returned by one executable task.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct ExecutionTaskOutcome {
    /// Task-outcome schema version, which must be `1`.
    pub schema_version: u32,
    /// Cumulative actual usage for this logical task since first dispatch.
    pub usage: ExecutionUsage,
    /// Typed task result fields flattened into the outcome envelope.
    #[serde(flatten)]
    pub result: ExecutionTaskResult,
}

/// Supported execution task result states.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionTaskResult {
    /// Task completed with structured output and provenance citations.
    Completed {
        /// Structured task output.
        output: Value,
        /// Provenance citations produced by this task.
        citations: Vec<ExecutionCitation>,
    },
    /// Task requires input from a declared audience.
    NeedsInput {
        /// Question that must be answered before execution can continue.
        question: String,
        /// Audience authorized and expected to answer.
        audience: InputAudience,
    },
    /// Task requires a compiler-validated plan amendment.
    NeedsReplan {
        /// Reason the active plan cannot continue unchanged.
        reason: String,
        /// Structured evidence supplied to amendment planning.
        evidence: Value,
    },
    /// Task was cancelled before successful completion.
    Cancelled {
        /// Human-readable cancellation reason.
        reason: String,
    },
    /// A side effect may have committed, so automatic replay or compensation is unsafe.
    UnknownOutcome {
        /// Human-readable reconciliation guidance.
        message: String,
    },
    /// Task failed with a typed failure class.
    Failed {
        /// Failure class used by retry and terminal policy.
        class: ExecutionFailureClass,
        /// Human-readable failure message.
        message: String,
    },
}

/// Integer resource usage reported by one execution task.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionUsage {
    /// Billed cost in integer micro-US-dollars.
    pub cost_microusd: u64,
    /// Model tokens consumed.
    pub tokens: u64,
    /// Governed tool or capability calls made.
    pub tool_calls: u64,
    /// Bytes retrieved from external or memory sources.
    pub retrieved_bytes: u64,
}

/// Audience from which a task may request input.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAudience {
    /// The user who owns the run.
    User,
    /// A tenant administrator or operator.
    TenantAdmin,
    /// An identified external system.
    ExternalSystem,
}

/// Typed failure classes returned by executable tasks.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionFailureClass {
    /// Transient failure eligible for retry policy.
    Retryable,
    /// A required predecessor ended in terminal failure.
    DependencyFailed,
    /// Task input was invalid.
    InvalidInput,
    /// Task output did not satisfy its schema.
    InvalidOutput,
    /// Authorization policy denied execution.
    AuthorizationDenied,
    /// Approved resource budget was exhausted.
    BudgetExceeded,
    /// Execution deadline elapsed.
    DeadlineExceeded,
    /// Execution was cancelled.
    Cancelled,
    /// The requested operation is unsupported.
    Unsupported,
    /// Non-retryable terminal failure.
    Terminal,
}

/// One provenance citation returned by an execution task.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCitation {
    /// Stable non-empty source identifier.
    pub source_id: String,
    /// Optional source URI.
    pub uri: Option<String>,
    /// Optional structured source locator.
    pub locator: Option<Value>,
}

/// Patch over pending or downstream plan work.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanAmendment {
    /// Active plan revision to which the amendment applies.
    pub base_plan_revision: u64,
    /// Human-readable reason for amending the plan.
    pub reason: String,
    /// Structured evidence supporting the amendment.
    pub evidence: Value,
    /// Restricted pending/downstream patch operations.
    pub operations: Vec<PlanAmendmentOperation>,
}

/// Restricted operations available in a plan amendment.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanAmendmentOperation {
    /// Add one downstream node.
    AddNode {
        /// Node to add.
        node: ExecutionNode,
    },
    /// Replace one still-pending node.
    ReplacePendingNode {
        /// Stable identifier of the pending node being replaced.
        node_id: String,
        /// Replacement node definition.
        node: ExecutionNode,
    },
    /// Remove one still-pending node.
    RemovePendingNode {
        /// Stable identifier of the pending node being removed.
        node_id: String,
    },
}

/// Reusable goal semantics stored alongside an activated skill plan template.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionGoalTemplate {
    /// Individually identifiable reusable requirements.
    pub requirements: Vec<ExecutionRequirement>,
    /// Reusable structured deliverables.
    pub deliverables: Vec<ExecutionDeliverable>,
    /// Reusable map-coverage requirements.
    pub coverage: Vec<CoverageRequirement>,
    /// Reusable immutable constraints.
    pub constraints: Vec<ExecutionConstraint>,
    /// Reusable completion checks.
    pub completion_checks: Vec<CompletionCheck>,
}

/// Published execution template pairing reusable completion semantics with one plan.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPlanTemplate {
    /// Reusable goal semantics instantiated with the current user objective.
    pub goal: ExecutionGoalTemplate,
    /// Canonical execution plan.
    pub plan: ExecutionPlanDefinition,
}

impl ExecutionPlanTemplate {
    /// Instantiates the exact current user objective with this reusable goal template.
    #[must_use]
    pub fn instantiate_goal(&self, objective: impl Into<String>) -> ExecutionGoalContract {
        ExecutionGoalContract {
            objective: objective.into(),
            requirements: self.goal.requirements.clone(),
            deliverables: self.goal.deliverables.clone(),
            coverage: self.goal.coverage.clone(),
            constraints: self.goal.constraints.clone(),
            completion_checks: self.goal.completion_checks.clone(),
        }
    }

    pub(crate) fn skill_reference_paths(&self, root: &str) -> Vec<(String, ArtifactRef)> {
        self.plan.skill_reference_paths(&format!("{root}.plan"))
    }
}

/// Strict initial planner response envelope.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedExecutionCandidate {
    /// Immutable complete user goal contract.
    pub goal: ExecutionGoalContract,
    /// Candidate execution plan.
    pub plan: ExecutionPlanDefinition,
    /// Structured run input validated by the candidate plan.
    pub run_input: Value,
}

/// Strict amendment planner response envelope.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GeneratedAmendmentCandidate {
    /// Restricted plan amendment candidate.
    pub amendment: PlanAmendment,
}

impl ExecutionPlanDefinition {
    pub(crate) fn skill_reference_paths(&self, root: &str) -> Vec<(String, ArtifactRef)> {
        let mut references = Vec::new();
        for (node_index, node) in self.nodes.iter().enumerate() {
            let operation_path = format!("{root}.nodes[{node_index}].operation");
            match &node.operation {
                ExecutionOperation::Agent { skill_refs, .. } => append_skill_refs(
                    &mut references,
                    &format!("{operation_path}.skill_refs"),
                    skill_refs,
                ),
                ExecutionOperation::Map {
                    task: MapTask::Agent { skill_refs, .. },
                    ..
                } => append_skill_refs(
                    &mut references,
                    &format!("{operation_path}.task.skill_refs"),
                    skill_refs,
                ),
                ExecutionOperation::Reduce {
                    reducer: ExecutionReducer::Agent { skill_refs, .. },
                    ..
                } => append_skill_refs(
                    &mut references,
                    &format!("{operation_path}.reducer.skill_refs"),
                    skill_refs,
                ),
                ExecutionOperation::Capability { .. }
                | ExecutionOperation::Map { .. }
                | ExecutionOperation::Reduce { .. }
                | ExecutionOperation::Review { .. }
                | ExecutionOperation::WaitSignal { .. }
                | ExecutionOperation::WaitUntil { .. }
                | ExecutionOperation::Output { .. } => {}
            }
        }
        references
    }
}

fn append_skill_refs(
    output: &mut Vec<(String, ArtifactRef)>,
    path: &str,
    skill_refs: &[ArtifactRef],
) {
    output.extend(
        skill_refs
            .iter()
            .enumerate()
            .map(|(index, artifact_ref)| (format!("{path}[{index}]"), artifact_ref.clone())),
    );
}
