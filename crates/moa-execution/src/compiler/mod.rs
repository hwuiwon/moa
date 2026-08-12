//! Deterministic initial-plan compilation and restricted amendment validation.

mod amendment;
mod estimate;
mod validation;

use amendment::*;
use estimate::*;
use validation::activation_bounds::{
    validate_completion_activation_bounds, validate_plan_activation_bound,
};
use validation::schema_references::{validate_declared_reference_paths, validate_schemas};
use validation::{
    append_artifact_reports, append_error, validate_amendment_reference_narrowing,
    validate_authorization, validate_catalog, validate_goal_plan_links, validate_plan_references,
};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use moa_artifacts::{
    execution_plan::{
        CompletionCheckKind, ExecutionBudgetLimit, ExecutionGoalContract, ExecutionNode,
        ExecutionOperation, ExecutionPlanDefinition, ExecutionReducer, ExecutionTemporalTarget,
        ExecutionWaitExpiryAction, MapTask, PlanAmendment, PlanAmendmentOperation,
    },
    reference::ArtifactRef,
    validation::{validate_execution_goal_contract, validate_execution_plan_definition},
};
use moa_config::ExecutionConfig;
use moa_core::canonical_json::canonical_json_bytes;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Error,
    bindings::extract_map_key,
    budget::estimate_fits_limit,
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapability, ExecutionCapabilityCatalog,
        ExecutionEstimate, ExecutionHash, canonical_sort_key, catalog_hash, plan_hash,
    },
    schema::validate_instance,
    state::{ExecutionAmendmentProjection, ExecutionNodeStatus, ExecutionTaskStatus},
};

/// Complete input to deterministic initial execution compilation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompileExecutionRequest {
    /// Immutable user-derived goal contract.
    pub goal: ExecutionGoalContract,
    /// Candidate execution plan.
    pub plan: ExecutionPlanDefinition,
    /// Concrete run input validated against the plan input schema.
    pub run_input: Value,
    /// Immutable tenant- and policy-filtered capability catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Immutable exact capability and skill allowlist.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Approved run-level resource envelope.
    pub approved_budget: ExecutionBudgetLimit,
    /// Tenant-independent execution defaults and per-turn estimates.
    pub config: ExecutionConfig,
    /// Deterministic compile time used for deadline validation.
    pub now: DateTime<Utc>,
}

/// Complete input to restricted plan-amendment validation.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidateAmendmentRequest {
    /// Immutable goal contract that amendments cannot replace.
    pub goal: ExecutionGoalContract,
    /// Active canonical plan snapshot.
    pub active_plan: CanonicalExecutionPlan,
    /// Restricted pending/downstream patch.
    pub amendment: PlanAmendment,
    /// Current durable run projection.
    pub projection: ExecutionAmendmentProjection,
    /// Current immutable capability catalog.
    pub catalog: ExecutionCapabilityCatalog,
    /// Original immutable authorization envelope.
    pub authorization: ExecutionAuthorizationEnvelope,
    /// Resource envelope remaining before the amendment is accepted.
    pub remaining_budget: ExecutionBudgetLimit,
    /// Execution defaults and per-turn estimates.
    pub config: ExecutionConfig,
    /// Deterministic validation time.
    pub now: DateTime<Utc>,
}

/// Outcome of initial compilation with inspectable ordered validation issues.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompileExecutionOutcome {
    /// Canonical goal and plan when no validation error exists.
    pub compiled: Option<CompiledExecution>,
    /// Ordered validation report.
    pub report: ExecutionValidationReport,
}

/// Outcome of amendment validation with inspectable ordered issues.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmendmentValidationOutcome {
    /// New canonical plan snapshot when the amendment is valid.
    pub plan: Option<CanonicalExecutionPlan>,
    /// Worst-case estimate of only work still executable after the amendment.
    pub remaining_estimate: Option<ExecutionEstimate>,
    /// Ordered validation report.
    pub report: ExecutionValidationReport,
}

/// Validated immutable goal and canonical execution plan.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledExecution {
    /// Validated immutable goal contract.
    pub goal: ExecutionGoalContract,
    /// Canonical immutable plan snapshot.
    pub plan: CanonicalExecutionPlan,
}

/// Canonical compiled plan and its immutable hashes and estimate.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanonicalExecutionPlan {
    /// Exact validated plan definition.
    pub definition: ExecutionPlanDefinition,
    /// Domain-separated canonical plan hash.
    pub plan_hash: ExecutionHash,
    /// Pinned capability-catalog hash.
    pub catalog_hash: ExecutionHash,
    /// Exact worst-case plan estimate, including verifier tasks.
    pub estimate: ExecutionEstimate,
    /// Compiler report stored with the immutable snapshot.
    pub report: ExecutionValidationReport,
}

/// Ordered compiler validation report.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionValidationReport {
    /// Validation issues in deterministic discovery order.
    pub issues: Vec<ExecutionValidationIssue>,
}

impl ExecutionValidationReport {
    /// Returns whether at least one blocking error exists.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|issue| issue.severity == ExecutionValidationSeverity::Error)
    }

    pub(crate) fn error(
        &mut self,
        code: impl Into<String>,
        path: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.issues.push(ExecutionValidationIssue {
            severity: ExecutionValidationSeverity::Error,
            code: code.into(),
            path: path.into(),
            message: message.into(),
        });
    }
}

/// One deterministic compiler validation issue.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionValidationIssue {
    /// Blocking or informational severity.
    pub severity: ExecutionValidationSeverity,
    /// Stable machine-readable issue code.
    pub code: String,
    /// JSON-ish location of the invalid value.
    pub path: String,
    /// Human-readable validation failure.
    pub message: String,
}

/// Compiler validation severity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionValidationSeverity {
    /// Blocking validation issue.
    Error,
    /// Non-blocking validation issue persisted for audit.
    Warning,
}

/// Deterministically validates, estimates, canonicalizes, and hashes an initial plan.
#[must_use]
pub fn compile(request: CompileExecutionRequest) -> CompileExecutionOutcome {
    let mut report = ExecutionValidationReport::default();
    append_artifact_reports(&request.goal, &request.plan, &mut report);
    validate_goal_plan_links(&request.goal, &request.plan, &mut report);
    validate_plan_activation_bound(&request.plan, &request.config, &mut report);
    validate_completion_activation_bounds(&request.goal, &request.config, &mut report);
    validate_condition_scope(&request.goal, &request.plan, &mut report);
    validate_catalog(&request.catalog, &mut report);
    validate_authorization(&request.authorization, &mut report);
    validate_schemas(&request.goal, &request.plan, &mut report);
    validate_declared_reference_paths(&request.goal, &request.plan, &mut report);
    if let Err(error) = validate_instance(
        &request.plan.input_schema,
        &request.run_input,
        "plan.input_schema",
    ) {
        append_error(&mut report, "invalid_run_input", "run_input", error);
    }
    validate_plan_references(
        &request.plan,
        &request.catalog,
        &request.authorization,
        &mut report,
    );
    append_execution_config_validation(&request.config, &mut report);
    validate_temporal_contract(
        &request.plan,
        request.approved_budget.deadline_at,
        request.now,
        &request.config,
        "approved_budget.deadline_at",
        &mut report,
    );

    let estimate = estimate_plan(
        &request.goal,
        &request.plan,
        &request.catalog,
        &request.config,
        &mut report,
    );
    if let Some(estimate) = estimate
        && let Err(error) = estimate_fits_limit(estimate, &request.approved_budget)
    {
        append_error(
            &mut report,
            "approved_budget_exceeded",
            "approved_budget",
            error,
        );
    }

    let hash = match plan_hash(&request.plan) {
        Ok(hash) => Some(hash),
        Err(error) => {
            append_error(&mut report, "plan_hash_failed", "plan", error);
            None
        }
    };

    if report.has_errors() {
        return CompileExecutionOutcome {
            compiled: None,
            report,
        };
    }

    let Some(estimate) = estimate else {
        report.error(
            "estimate_missing",
            "plan",
            "validated plan did not produce a worst-case estimate",
        );
        return CompileExecutionOutcome {
            compiled: None,
            report,
        };
    };
    let Some(hash) = hash else {
        report.error(
            "plan_hash_missing",
            "plan",
            "validated plan did not produce a canonical hash",
        );
        return CompileExecutionOutcome {
            compiled: None,
            report,
        };
    };

    let plan = CanonicalExecutionPlan {
        definition: request.plan,
        plan_hash: hash,
        catalog_hash: request.catalog.catalog_hash,
        estimate,
        report: report.clone(),
    };
    CompileExecutionOutcome {
        compiled: Some(CompiledExecution {
            goal: request.goal,
            plan,
        }),
        report,
    }
}

/// Validates and canonicalizes one restricted amendment over pending work.
#[must_use]
pub fn validate_amendment(request: ValidateAmendmentRequest) -> AmendmentValidationOutcome {
    let mut report = ExecutionValidationReport::default();
    if request.amendment.base_plan_revision != request.projection.plan_revision {
        report.error(
            "stale_plan_revision",
            "amendment.base_plan_revision",
            "amendment base revision does not match the active projection",
        );
    }
    if request.amendment.reason.trim().is_empty() {
        report.error(
            "empty_amendment_reason",
            "amendment.reason",
            "amendment reason must not be empty",
        );
    }

    validate_catalog(&request.catalog, &mut report);
    validate_authorization(&request.authorization, &mut report);
    if request.catalog.catalog_hash != request.active_plan.catalog_hash {
        report.error(
            "catalog_hash_changed",
            "catalog.catalog_hash",
            "amendment must use the capability catalog pinned by the active plan",
        );
    }

    let mut definition = request.active_plan.definition.clone();
    apply_amendment(
        &request.amendment,
        &request.projection,
        &request.active_plan.definition,
        &mut definition,
        &mut report,
    );
    validate_amendment_reference_narrowing(
        &request.active_plan.definition,
        &definition,
        &mut report,
    );

    append_artifact_reports(&request.goal, &definition, &mut report);
    validate_goal_plan_links(&request.goal, &definition, &mut report);
    validate_condition_scope(&request.goal, &definition, &mut report);
    validate_plan_activation_bound(&definition, &request.config, &mut report);
    validate_completion_activation_bounds(&request.goal, &request.config, &mut report);
    validate_schemas(&request.goal, &definition, &mut report);
    validate_declared_reference_paths(&request.goal, &definition, &mut report);
    validate_plan_references(
        &definition,
        &request.catalog,
        &request.authorization,
        &mut report,
    );

    append_execution_config_validation(&request.config, &mut report);
    validate_temporal_contract(
        &definition,
        request.remaining_budget.deadline_at,
        request.now,
        &request.config,
        "remaining_budget.deadline_at",
        &mut report,
    );

    let full_estimate = estimate_plan(
        &request.goal,
        &definition,
        &request.catalog,
        &request.config,
        &mut report,
    );
    let remaining_estimate = estimate_remaining_plan(
        &request.goal,
        &definition,
        &request.projection,
        &request.catalog,
        &request.config,
        &mut report,
    );
    if let Some(estimate) = remaining_estimate
        && let Err(error) = estimate_fits_limit(estimate, &request.remaining_budget)
    {
        append_error(
            &mut report,
            "remaining_budget_exceeded",
            "remaining_budget",
            error,
        );
    }

    let hash = match plan_hash(&definition) {
        Ok(hash) => Some(hash),
        Err(error) => {
            append_error(&mut report, "plan_hash_failed", "plan", error);
            None
        }
    };

    if report.has_errors() {
        return AmendmentValidationOutcome {
            plan: None,
            remaining_estimate,
            report,
        };
    }

    let (Some(estimate), Some(hash)) = (full_estimate, hash) else {
        report.error(
            "canonical_plan_missing",
            "plan",
            "validated amendment did not produce a canonical plan",
        );
        return AmendmentValidationOutcome {
            plan: None,
            remaining_estimate,
            report,
        };
    };

    AmendmentValidationOutcome {
        plan: Some(CanonicalExecutionPlan {
            definition,
            plan_hash: hash,
            catalog_hash: request.catalog.catalog_hash,
            estimate,
            report: report.clone(),
        }),
        remaining_estimate,
        report,
    }
}

fn append_execution_config_validation(
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
) {
    if let Err(error) = config.validate() {
        report.error("invalid_execution_config", "config", error.to_string());
    }
}

/// Confines conditional nodes to the exact semantics the runtime implements.
///
/// A false condition commits `node_status = 'skipped'` with a JSON `null` aggregate
/// output. Version one therefore treats a conditional node as an *effectful leaf*:
/// other nodes may depend on it for ordering, but nothing may read its output, and
/// nothing may make the run's success contingent on it having run. Every rule below
/// rejects a plan whose meaning would otherwise depend on a value or an outcome that
/// a skipped branch cannot produce:
///
/// - a condition may read only run input or a *declared dependency's* output, so the
///   value it tests is guaranteed to exist by the time the node is activated;
/// - no `$ref` may read a conditional node's output, because the skipped value is
///   `null` and neither `resolve_reference` nor a capability input schema accepts it;
/// - a conditional node may not be the plan's `Output` operation, whose value is the
///   run's deliverable;
/// - a conditional node may not appear in a `RequiredNodes` completion check, which
///   counts a skipped node as failed and would turn a legitimately false branch into
///   a partial run; and
/// - a requirement served *only* by conditional nodes is rejected, because all
///   branches evaluating false would leave it with no eligible node at all.
fn validate_condition_scope(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    report: &mut ExecutionValidationReport,
) {
    let conditional_ids = plan
        .nodes
        .iter()
        .filter(|node| node.when.is_some())
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    if conditional_ids.is_empty() {
        return;
    }

    for (index, node) in plan.nodes.iter().enumerate() {
        let root = format!("plan.nodes[{index}]");
        if let Some(condition) = &node.when {
            let reference = match condition {
                moa_artifacts::execution_plan::ExecutionCondition::Exists { reference }
                | moa_artifacts::execution_plan::ExecutionCondition::Equals { reference, .. } => {
                    reference
                }
            };
            validate_condition_reference_visibility(
                &format!("{root}.when.reference.$ref"),
                &reference.path,
                node,
                report,
            );
            if matches!(node.operation, ExecutionOperation::Output { .. }) {
                report.error(
                    "conditional_output_node",
                    format!("{root}.when"),
                    "the terminal output node must not be conditional",
                );
            }
            for check in &goal.completion_checks {
                let CompletionCheckKind::RequiredNodes { node_ids } = &check.kind else {
                    continue;
                };
                if node_ids.contains(&node.id) {
                    report.error(
                        "conditional_required_node",
                        format!("{root}.when"),
                        format!(
                            "completion check `{}` requires node `{}`, which a false condition \
                             would skip and the check would count as failed",
                            check.id, node.id
                        ),
                    );
                }
            }
            for coverage in goal
                .coverage
                .iter()
                .filter(|coverage| coverage.map_node_id == node.id)
            {
                report.error(
                    "conditional_coverage_node",
                    format!("{root}.when"),
                    format!(
                        "coverage requirement `{}` measures node `{}`, which a false condition \
                         would skip and the coverage would count as unmet",
                        coverage.id, node.id
                    ),
                );
            }
        }
        validate_no_conditional_reference(
            &format!("{root}.input"),
            &node.input,
            &conditional_ids,
            report,
        );
        match &node.operation {
            ExecutionOperation::Map { items, .. } | ExecutionOperation::Reduce { items, .. } => {
                validate_no_conditional_reference(
                    &format!("{root}.operation.items"),
                    items,
                    &conditional_ids,
                    report,
                );
            }
            ExecutionOperation::Output { value } => validate_no_conditional_reference(
                &format!("{root}.operation.value"),
                value,
                &conditional_ids,
                report,
            ),
            ExecutionOperation::WaitUntil { result, .. } => validate_no_conditional_reference(
                &format!("{root}.operation.result"),
                result,
                &conditional_ids,
                report,
            ),
            ExecutionOperation::Capability { .. }
            | ExecutionOperation::Agent { .. }
            | ExecutionOperation::Review { .. }
            | ExecutionOperation::WaitSignal { .. } => {}
        }
    }

    for (index, coverage) in goal.coverage.iter().enumerate() {
        validate_no_conditional_reference(
            &format!("goal.coverage[{index}].expected_items"),
            &coverage.expected_items,
            &conditional_ids,
            report,
        );
    }

    let mut unconditional_requirements = BTreeSet::new();
    let mut conditional_requirements = BTreeMap::new();
    for node in &plan.nodes {
        for requirement_id in &node.requirement_ids {
            if node.when.is_some() {
                conditional_requirements
                    .entry(requirement_id.as_str())
                    .or_insert(node.id.as_str());
            } else {
                unconditional_requirements.insert(requirement_id.as_str());
            }
        }
    }
    for (requirement_id, node_id) in conditional_requirements {
        if !unconditional_requirements.contains(requirement_id) {
            report.error(
                "requirement_only_conditional",
                format!("plan.nodes.{node_id}.requirement_ids"),
                format!(
                    "every node serving requirement `{requirement_id}` is conditional, so all \
                     branches evaluating false would leave it with no eligible node"
                ),
            );
        }
    }
}

/// Rejects a condition that reads anything other than run input or a declared dependency.
fn validate_condition_reference_visibility(
    path: &str,
    reference: &str,
    node: &ExecutionNode,
    report: &mut ExecutionValidationReport,
) {
    let Some(node_id) = condition_reference_node(reference) else {
        return;
    };
    if node_id == node.id {
        report.error(
            "condition_reference_not_visible",
            path,
            "a node condition cannot reference its own output",
        );
    } else if !node.depends_on.iter().any(|id| id == node_id) {
        report.error(
            "condition_reference_not_visible",
            path,
            "a node condition may only read run input or a declared dependency output",
        );
    }
}

fn condition_reference_node(reference: &str) -> Option<&str> {
    reference
        .strip_prefix("$.nodes.")
        .and_then(|rest| rest.split_once(".output"))
        .map(|(node_id, _)| node_id)
}

/// Rejects any `$ref` whose source node declares a condition.
fn validate_no_conditional_reference(
    path: &str,
    value: &Value,
    conditional_ids: &BTreeSet<&str>,
    report: &mut ExecutionValidationReport,
) {
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                validate_no_conditional_reference(
                    &format!("{path}[{index}]"),
                    value,
                    conditional_ids,
                    report,
                );
            }
        }
        Value::Object(object) => {
            if object.len() == 1
                && let Some(reference) = object.get("$ref").and_then(Value::as_str)
            {
                if condition_reference_node(reference)
                    .is_some_and(|node_id| conditional_ids.contains(node_id))
                {
                    report.error(
                        "conditional_output_read",
                        path,
                        "a conditional node's output cannot be read; it is null when the \
                         condition is false",
                    );
                }
                return;
            }
            if object.keys().any(|key| key.starts_with('$')) {
                return;
            }
            for (key, value) in object {
                validate_no_conditional_reference(
                    &format!("{path}.{key}"),
                    value,
                    conditional_ids,
                    report,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn validate_temporal_contract(
    plan: &ExecutionPlanDefinition,
    deadline_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    config: &ExecutionConfig,
    deadline_path: &str,
    report: &mut ExecutionValidationReport,
) {
    let Some(deadline_at) = deadline_at else {
        report.error(
            "missing_deadline",
            deadline_path,
            "durable execution requires an absolute deadline",
        );
        return;
    };
    if deadline_at <= now {
        report.error(
            "deadline_exceeded",
            deadline_path,
            "execution deadline must be later than the validation time",
        );
        return;
    }
    let horizon_seconds = deadline_at
        .signed_duration_since(now)
        .to_std()
        .map(|duration| duration.as_secs());
    if !matches!(
        horizon_seconds,
        Ok(seconds) if seconds <= config.maximum_horizon_seconds
    ) {
        report.error(
            "deadline_out_of_horizon",
            deadline_path,
            format!(
                "execution deadline exceeds the configured maximum horizon of {} seconds",
                config.maximum_horizon_seconds
            ),
        );
    }

    validate_input_wait_policy(
        &plan.input_wait_policy,
        "plan.input_wait_policy",
        now,
        deadline_at,
        report,
    );
    for (index, node) in plan.nodes.iter().enumerate() {
        let path = format!("plan.nodes[{index}].operation");
        match &node.operation {
            ExecutionOperation::Review { wait_policy, .. }
            | ExecutionOperation::WaitSignal { wait_policy, .. } => {
                validate_wait_policy(
                    wait_policy,
                    &format!("{path}.wait_policy"),
                    now,
                    deadline_at,
                    report,
                );
                if let ExecutionWaitExpiryAction::ContinueWith { output } = &wait_policy.on_expiry
                    && let Err(error) = validate_instance(
                        &node.output_schema,
                        output,
                        "wait_policy.on_expiry.output",
                    )
                {
                    append_error(
                        report,
                        "invalid_wait_expiry_output",
                        format!("{path}.wait_policy.on_expiry.output"),
                        error,
                    );
                }
            }
            ExecutionOperation::WaitUntil { wake, result } => {
                validate_temporal_target(wake, &format!("{path}.wake"), now, deadline_at, report);
                if !value_contains_binding(result)
                    && let Err(error) =
                        validate_instance(&node.output_schema, result, "wait_until.result")
                {
                    append_error(
                        report,
                        "invalid_wait_until_result",
                        format!("{path}.result"),
                        error,
                    );
                }
            }
            ExecutionOperation::Capability { .. }
            | ExecutionOperation::Agent { .. }
            | ExecutionOperation::Map { .. }
            | ExecutionOperation::Reduce { .. }
            | ExecutionOperation::Output { .. } => {}
        }
    }
}

fn validate_wait_policy(
    policy: &moa_artifacts::execution_plan::ExecutionWaitPolicy,
    path: &str,
    now: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
    report: &mut ExecutionValidationReport,
) {
    validate_temporal_target(
        &policy.expiry,
        &format!("{path}.expiry"),
        now,
        deadline_at,
        report,
    );
}

/// Validates the plan-level expiry policy for runtime `NeedsInput` outcomes.
///
/// Unlike a `Review` or `WaitSignal` policy, this one policy settles whichever
/// logical task returned `NeedsInput`, so no single declared output can be checked
/// against a specific node's `output_schema` at compile time. `ContinueWith` is
/// therefore rejected outright rather than deferred to the materialization
/// transaction, where the schema check is a non-retryable failure.
fn validate_input_wait_policy(
    policy: &moa_artifacts::execution_plan::ExecutionWaitPolicy,
    path: &str,
    now: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
    report: &mut ExecutionValidationReport,
) {
    validate_wait_policy(policy, path, now, deadline_at, report);
    if matches!(
        policy.on_expiry,
        ExecutionWaitExpiryAction::ContinueWith { .. }
    ) {
        report.error(
            "unsupported_input_wait_expiry",
            format!("{path}.on_expiry"),
            "input wait expiry must fail the waiting task; continue_with cannot be validated \
             against the output schema of the node that requested input",
        );
    }
}

fn validate_temporal_target(
    target: &ExecutionTemporalTarget,
    path: &str,
    now: DateTime<Utc>,
    deadline_at: DateTime<Utc>,
    report: &mut ExecutionValidationReport,
) {
    match target {
        ExecutionTemporalTarget::At { at } => {
            if *at <= now {
                report.error(
                    "temporal_target_elapsed",
                    path,
                    "absolute temporal target must be later than the validation time",
                );
            }
            if *at >= deadline_at {
                report.error(
                    "temporal_target_after_deadline",
                    path,
                    "temporal target must be earlier than the run deadline",
                );
            }
        }
        // A relative delay is resolved against the clock at wait entry, which is
        // never earlier than compile time, so the remaining horizon at wait entry is
        // never larger than it is here. `delay < remaining` is therefore a necessary
        // condition, and rejecting a delay that fails it rejects only delays that can
        // never be legal. It is not a sufficient one: a delay accepted here can still
        // be illegal once the wait is actually entered, and only wait entry can fence
        // that. See `interpreter::resolve_temporal_target`.
        ExecutionTemporalTarget::After { delay_seconds } => {
            let remaining_seconds = deadline_at
                .signed_duration_since(now)
                .to_std()
                .map(|duration| duration.as_secs())
                .unwrap_or_default();
            if *delay_seconds == 0 {
                report.error(
                    "temporal_delay_zero",
                    path,
                    "relative temporal delay must be greater than zero",
                );
            } else if *delay_seconds >= remaining_seconds {
                report.error(
                    "temporal_target_after_deadline",
                    path,
                    "relative temporal delay must fit strictly inside the remaining run horizon",
                );
            }
        }
    }
}

fn value_contains_binding(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key("$ref")
                || object.contains_key("$item")
                || object.contains_key("$item_key")
                || object.values().any(value_contains_binding)
        }
        Value::Array(values) => values.iter().any(value_contains_binding),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

#[cfg(test)]
mod tests;
