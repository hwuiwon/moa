//! Deterministic initial-plan compilation and restricted amendment validation.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};
use moa_artifacts::{
    execution_plan::{
        CompletionCheckKind, ExecutionBudgetLimit, ExecutionCondition, ExecutionGoalContract,
        ExecutionNode, ExecutionOperation, ExecutionPlanDefinition, ExecutionReducer, MapTask,
        PlanAmendment, PlanAmendmentOperation,
    },
    reference::ArtifactRef,
    validation::{validate_execution_goal_contract, validate_execution_plan_definition},
};
use moa_config::ExecutionConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    Error,
    bindings::extract_map_key,
    budget::estimate_fits_limit,
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapability, ExecutionCapabilityCatalog,
        ExecutionEstimate, ExecutionHash, canonical_json_bytes, canonical_sort_key, catalog_hash,
        plan_hash,
    },
    schema::{validate_instance, validate_schema},
    state::{ExecutionNodeStatus, ExecutionProjection, ExecutionTaskStatus},
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
    pub projection: ExecutionProjection,
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

    if request
        .approved_budget
        .deadline_at
        .is_some_and(|deadline| request.now > deadline)
    {
        report.error(
            "deadline_exceeded",
            "approved_budget.deadline_at",
            "approved execution deadline has already elapsed",
        );
    }

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
    if request.amendment.schema_version != 1 {
        report.error(
            "unsupported_schema_version",
            "amendment.schema_version",
            "schema_version must equal 1",
        );
    }
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
    validate_schemas(&request.goal, &definition, &mut report);
    validate_declared_reference_paths(&request.goal, &definition, &mut report);
    validate_plan_references(
        &definition,
        &request.catalog,
        &request.authorization,
        &mut report,
    );

    if request
        .remaining_budget
        .deadline_at
        .is_some_and(|deadline| request.now > deadline)
    {
        report.error(
            "deadline_exceeded",
            "remaining_budget.deadline_at",
            "execution deadline has already elapsed",
        );
    }

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

fn append_artifact_reports(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    report: &mut ExecutionValidationReport,
) {
    for error in validate_execution_goal_contract(goal).errors {
        report.error("goal_structure", error.path, error.message);
    }
    for error in validate_execution_plan_definition(plan).errors {
        report.error("plan_structure", error.path, error.message);
    }
}

fn validate_goal_plan_links(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    report: &mut ExecutionValidationReport,
) {
    if goal.objective.trim().is_empty() {
        report.error(
            "empty_objective",
            "goal.objective",
            "execution objective must not be empty",
        );
    }

    let requirement_ids = goal
        .requirements
        .iter()
        .map(|requirement| requirement.id.as_str())
        .collect::<HashSet<_>>();
    let constraint_ids = goal
        .constraints
        .iter()
        .map(|constraint| constraint.id.as_str())
        .collect::<HashSet<_>>();
    let nodes = plan
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();

    for (node_index, node) in plan.nodes.iter().enumerate() {
        for (requirement_index, requirement_id) in node.requirement_ids.iter().enumerate() {
            if !requirement_ids.contains(requirement_id.as_str()) {
                report.error(
                    "unknown_requirement",
                    format!("plan.nodes[{node_index}].requirement_ids[{requirement_index}]"),
                    "node requirement ID does not exist in the goal contract",
                );
            }
        }
    }
    for (index, requirement) in goal.requirements.iter().enumerate() {
        if !plan
            .nodes
            .iter()
            .any(|node| node.requirement_ids.contains(&requirement.id))
        {
            report.error(
                "unserved_requirement",
                format!("goal.requirements[{index}].id"),
                "goal requirement is not served by any plan node",
            );
        }
    }

    let mut covered_requirements = HashSet::new();
    let mut covered_constraints = HashSet::new();
    for (check_index, check) in goal.completion_checks.iter().enumerate() {
        for (index, requirement_id) in check.requirement_ids.iter().enumerate() {
            if !requirement_ids.contains(requirement_id.as_str()) {
                report.error(
                    "unknown_completion_requirement",
                    format!("goal.completion_checks[{check_index}].requirement_ids[{index}]"),
                    "completion-check requirement ID does not exist",
                );
            } else {
                covered_requirements.insert(requirement_id.as_str());
            }
        }
        for (index, constraint_id) in check.constraint_ids.iter().enumerate() {
            if !constraint_ids.contains(constraint_id.as_str()) {
                report.error(
                    "unknown_completion_constraint",
                    format!("goal.completion_checks[{check_index}].constraint_ids[{index}]"),
                    "completion-check constraint ID does not exist",
                );
            } else {
                covered_constraints.insert(constraint_id.as_str());
            }
        }

        match &check.kind {
            CompletionCheckKind::RequiredNodes { node_ids }
            | CompletionCheckKind::Citations { node_ids, .. } => {
                for (index, node_id) in node_ids.iter().enumerate() {
                    if !nodes.contains_key(node_id.as_str()) {
                        report.error(
                            "unknown_completion_node",
                            format!("goal.completion_checks[{check_index}].node_ids[{index}]"),
                            "completion-check node ID does not exist",
                        );
                    }
                }
            }
            CompletionCheckKind::MapCoverage { map_node_id } => {
                if !nodes
                    .get(map_node_id.as_str())
                    .is_some_and(|node| matches!(node.operation, ExecutionOperation::Map { .. }))
                {
                    report.error(
                        "invalid_coverage_node",
                        format!("goal.completion_checks[{check_index}].map_node_id"),
                        "map coverage check must reference a map node",
                    );
                }
            }
            CompletionCheckKind::OutputSchema | CompletionCheckKind::AgentVerifier { .. } => {}
        }
    }

    for (index, requirement) in goal.requirements.iter().enumerate() {
        if !covered_requirements.contains(requirement.id.as_str()) {
            report.error(
                "unchecked_requirement",
                format!("goal.requirements[{index}].id"),
                "every requirement must be linked to at least one completion check",
            );
        }
    }

    for (index, constraint) in goal.constraints.iter().enumerate() {
        if !covered_constraints.contains(constraint.id.as_str()) {
            report.error(
                "unchecked_constraint",
                format!("goal.constraints[{index}].id"),
                "every constraint must be linked to at least one completion check",
            );
        }
    }

    for (index, coverage) in goal.coverage.iter().enumerate() {
        if !nodes
            .get(coverage.map_node_id.as_str())
            .is_some_and(|node| matches!(node.operation, ExecutionOperation::Map { .. }))
        {
            report.error(
                "invalid_coverage_node",
                format!("goal.coverage[{index}].map_node_id"),
                "coverage requirement must reference a map node",
            );
        }
    }
}

fn validate_catalog(catalog: &ExecutionCapabilityCatalog, report: &mut ExecutionValidationReport) {
    if catalog.schema_version != 1 {
        report.error(
            "unsupported_catalog_version",
            "catalog.schema_version",
            "catalog schema_version must equal 1",
        );
    }
    validate_sorted_unique(
        catalog
            .capabilities
            .iter()
            .map(|capability| &capability.reference),
        "catalog.capabilities",
        "capability catalog",
        report,
    );
    for (index, capability) in catalog.capabilities.iter().enumerate() {
        let path = format!("catalog.capabilities[{index}]");
        if capability.description.trim().is_empty() {
            report.error(
                "empty_capability_description",
                format!("{path}.description"),
                "capability description must not be empty",
            );
        }
        if capability.contract_revision.trim().is_empty() {
            report.error(
                "empty_capability_contract_revision",
                format!("{path}.contract_revision"),
                "capability contract revision must not be empty",
            );
        }
        validate_capability_source(capability, &path, report);
        if capability.estimate.tasks != 1 {
            report.error(
                "invalid_capability_task_estimate",
                format!("{path}.estimate.tasks"),
                "every catalog capability estimate must declare exactly one logical task",
            );
        }
        validate_one_schema(
            &capability.input_schema,
            &format!("{path}.input_schema"),
            report,
        );
        validate_one_schema(
            &capability.output_schema,
            &format!("{path}.output_schema"),
            report,
        );
    }
    match catalog_hash(catalog.schema_version, &catalog.capabilities) {
        Ok(hash) if hash != catalog.catalog_hash => report.error(
            "catalog_hash_mismatch",
            "catalog.catalog_hash",
            "catalog_hash does not match canonical { schema_version, capabilities } JSON",
        ),
        Ok(_) => {}
        Err(error) => append_error(report, "catalog_hash_failed", "catalog.catalog_hash", error),
    }
}

fn validate_capability_source(
    capability: &ExecutionCapability,
    path: &str,
    report: &mut ExecutionValidationReport,
) {
    use crate::capability::CapabilitySource;
    let invalid = match &capability.source {
        CapabilitySource::BuiltInTool { name } => name.trim().is_empty(),
        CapabilitySource::HandTool { name } => name.trim().is_empty(),
        CapabilitySource::McpTool {
            server,
            tool_name,
            remote_name,
            ..
        } => {
            server.trim().is_empty() || tool_name.trim().is_empty() || remote_name.trim().is_empty()
        }
        CapabilitySource::ActionArtifact { tool_name, .. } => tool_name.trim().is_empty(),
        CapabilitySource::ConnectorAction {
            action_id,
            tool_name,
            ..
        }
        | CapabilitySource::SkillAction {
            action_id,
            tool_name,
            ..
        } => action_id.trim().is_empty() || tool_name.trim().is_empty(),
        CapabilitySource::SkillCode { entrypoint, .. } => entrypoint.trim().is_empty(),
        CapabilitySource::Memory {
            operation,
            tool_name,
        } => operation.trim().is_empty() || tool_name.trim().is_empty(),
        CapabilitySource::Knowledge { operation } => operation.trim().is_empty(),
        CapabilitySource::Model => false,
    };
    if invalid {
        report.error(
            "invalid_capability_source",
            format!("{path}.source"),
            "capability source names and entrypoints must not be empty",
        );
    }
}

fn validate_authorization(
    authorization: &ExecutionAuthorizationEnvelope,
    report: &mut ExecutionValidationReport,
) {
    validate_sorted_unique(
        authorization.capability_refs.iter(),
        "authorization.capability_refs",
        "capability authorization",
        report,
    );
    validate_sorted_unique(
        authorization.skill_refs.iter(),
        "authorization.skill_refs",
        "skill authorization",
        report,
    );
}

fn validate_sorted_unique<'a, T: Serialize + 'a>(
    values: impl Iterator<Item = &'a T>,
    path: &str,
    label: &str,
    report: &mut ExecutionValidationReport,
) {
    let mut previous: Option<Vec<u8>> = None;
    for (index, value) in values.enumerate() {
        let key = match canonical_sort_key(value) {
            Ok(key) => key,
            Err(error) => {
                append_error(
                    report,
                    "canonical_sort_failed",
                    format!("{path}[{index}]"),
                    error,
                );
                continue;
            }
        };
        if let Some(previous) = &previous {
            if key == *previous {
                report.error(
                    "duplicate_collection_entry",
                    format!("{path}[{index}]"),
                    format!("{label} vector must not contain duplicates"),
                );
            } else if key < *previous {
                report.error(
                    "unsorted_collection",
                    format!("{path}[{index}]"),
                    format!("{label} vector must be sorted by canonical serialized reference"),
                );
            }
        }
        previous = Some(key);
    }
}

fn validate_schemas(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    report: &mut ExecutionValidationReport,
) {
    for (index, deliverable) in goal.deliverables.iter().enumerate() {
        validate_one_schema(
            &deliverable.schema,
            &format!("goal.deliverables[{index}].schema"),
            report,
        );
    }
    validate_one_schema(&plan.input_schema, "plan.input_schema", report);
    validate_one_schema(&plan.output_schema, "plan.output_schema", report);
    for (index, node) in plan.nodes.iter().enumerate() {
        validate_one_schema(
            &node.output_schema,
            &format!("plan.nodes[{index}].output_schema"),
            report,
        );
        if let ExecutionOperation::Map {
            item_output_schema, ..
        } = &node.operation
        {
            validate_one_schema(
                item_output_schema,
                &format!("plan.nodes[{index}].operation.item_output_schema"),
                report,
            );
        }
    }
}

fn validate_one_schema(schema: &Value, path: &str, report: &mut ExecutionValidationReport) {
    if let Err(error) = validate_schema(schema, path) {
        append_error(report, "invalid_json_schema", path, error);
    }
}

fn validate_declared_reference_paths(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    report: &mut ExecutionValidationReport,
) {
    let output_schemas = plan
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), &node.output_schema))
        .collect::<HashMap<_, _>>();

    for (index, coverage) in goal.coverage.iter().enumerate() {
        validate_dynamic_reference_paths(
            &format!("goal.coverage[{index}].expected_items"),
            &coverage.expected_items,
            plan,
            &output_schemas,
            report,
        );
    }

    for (index, node) in plan.nodes.iter().enumerate() {
        let root = format!("plan.nodes[{index}]");
        if let Some(condition) = &node.when {
            let reference = match condition {
                ExecutionCondition::Exists { reference }
                | ExecutionCondition::Equals { reference, .. } => reference,
            };
            validate_declared_reference_path(
                &format!("{root}.when.reference.$ref"),
                &reference.path,
                plan,
                &output_schemas,
                report,
            );
        }
        validate_dynamic_reference_paths(
            &format!("{root}.input"),
            &node.input,
            plan,
            &output_schemas,
            report,
        );
        match &node.operation {
            ExecutionOperation::Map { items, .. } | ExecutionOperation::Reduce { items, .. } => {
                validate_dynamic_reference_paths(
                    &format!("{root}.operation.items"),
                    items,
                    plan,
                    &output_schemas,
                    report,
                )
            }
            ExecutionOperation::Output { value } => validate_dynamic_reference_paths(
                &format!("{root}.operation.value"),
                value,
                plan,
                &output_schemas,
                report,
            ),
            ExecutionOperation::Capability { .. }
            | ExecutionOperation::Agent { .. }
            | ExecutionOperation::Review { .. }
            | ExecutionOperation::WaitSignal { .. } => {}
        }
    }
}

fn validate_dynamic_reference_paths(
    path: &str,
    value: &Value,
    plan: &ExecutionPlanDefinition,
    output_schemas: &HashMap<&str, &Value>,
    report: &mut ExecutionValidationReport,
) {
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                validate_dynamic_reference_paths(
                    &format!("{path}[{index}]"),
                    value,
                    plan,
                    output_schemas,
                    report,
                );
            }
        }
        Value::Object(object) => {
            if object.len() == 1 {
                if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                    validate_declared_reference_path(path, reference, plan, output_schemas, report);
                    return;
                }
                if object.contains_key("$item") || object.contains_key("$item_key") {
                    return;
                }
            }
            if object.keys().any(|key| key.starts_with('$')) {
                return;
            }
            for (key, value) in object {
                validate_dynamic_reference_paths(
                    &format!("{path}.{key}"),
                    value,
                    plan,
                    output_schemas,
                    report,
                );
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn validate_declared_reference_path(
    path: &str,
    reference: &str,
    plan: &ExecutionPlanDefinition,
    output_schemas: &HashMap<&str, &Value>,
    report: &mut ExecutionValidationReport,
) {
    let source = if let Some(tail) = reference.strip_prefix("$.input") {
        Some((&plan.input_schema, tail))
    } else {
        reference
            .strip_prefix("$.nodes.")
            .and_then(|rest| rest.split_once(".output"))
            .and_then(|(node_id, tail)| {
                output_schemas
                    .get(node_id)
                    .copied()
                    .map(|schema| (schema, tail))
            })
    };
    let Some((schema, tail)) = source else {
        return;
    };
    let Some(segments) = reference_tail_segments(tail) else {
        return;
    };
    if segments.is_empty() || validate_schema(schema, path).is_err() {
        return;
    }

    if !schema_declares_path(schema, &segments) {
        report.error(
            "unknown_reference_path",
            path,
            "execution reference path is not declared by its source schema",
        );
    }
}

fn reference_tail_segments(tail: &str) -> Option<Vec<&str>> {
    if tail.is_empty() {
        return Some(Vec::new());
    }
    let fields = tail.strip_prefix('.')?;
    let segments = fields.split('.').collect::<Vec<_>>();
    if segments
        .iter()
        .any(|segment| !valid_reference_segment(segment))
    {
        return None;
    }
    Some(segments)
}

fn valid_reference_segment(segment: &str) -> bool {
    let mut characters = segment.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn schema_declares_path(root: &Value, segments: &[&str]) -> bool {
    schema_declares_path_inner(root, root, segments, &mut HashSet::new())
}

fn schema_declares_path_inner(
    root: &Value,
    schema: &Value,
    segments: &[&str],
    visiting: &mut HashSet<(usize, usize)>,
) -> bool {
    if segments.is_empty() {
        return true;
    }
    let key = (schema as *const Value as usize, segments.len());
    if !visiting.insert(key) {
        return false;
    }

    let declared = schema.as_object().is_some_and(|object| {
        let property_declared = object
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(segments[0]))
            .is_some_and(|property| {
                schema_declares_path_inner(root, property, &segments[1..], visiting)
            });
        let required_leaf = segments.len() == 1
            && object
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| {
                    required
                        .iter()
                        .any(|field| field.as_str() == Some(segments[0]))
                });
        let reference_declared = object
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|reference| resolve_local_schema_reference(root, reference))
            .is_some_and(|target| schema_declares_path_inner(root, target, segments, visiting));
        let all_of_declared =
            object
                .get("allOf")
                .and_then(Value::as_array)
                .is_some_and(|branches| {
                    branches
                        .iter()
                        .any(|branch| schema_declares_path_inner(root, branch, segments, visiting))
                });
        let alternatives_declare = |keyword: &str, visiting: &mut HashSet<(usize, usize)>| {
            object
                .get(keyword)
                .and_then(Value::as_array)
                .is_some_and(|branches| {
                    !branches.is_empty()
                        && branches.iter().all(|branch| {
                            schema_declares_path_inner(root, branch, segments, visiting)
                        })
                })
        };
        let conditional_declared = object.get("if").is_some()
            && object
                .get("then")
                .is_some_and(|branch| schema_declares_path_inner(root, branch, segments, visiting))
            && object
                .get("else")
                .is_some_and(|branch| schema_declares_path_inner(root, branch, segments, visiting));

        property_declared
            || required_leaf
            || reference_declared
            || all_of_declared
            || alternatives_declare("anyOf", visiting)
            || alternatives_declare("oneOf", visiting)
            || conditional_declared
    });

    visiting.remove(&key);
    declared
}

fn resolve_local_schema_reference<'a>(root: &'a Value, reference: &str) -> Option<&'a Value> {
    let fragment = reference.strip_prefix('#')?;
    if fragment.is_empty() {
        return Some(root);
    }
    if fragment.starts_with('/') {
        return root.pointer(fragment);
    }
    find_schema_anchor(root, fragment)
}

fn find_schema_anchor<'a>(schema: &'a Value, anchor: &str) -> Option<&'a Value> {
    match schema {
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_schema_anchor(value, anchor)),
        Value::Object(object) => {
            if object.get("$anchor").and_then(Value::as_str) == Some(anchor) {
                return Some(schema);
            }
            object
                .values()
                .find_map(|value| find_schema_anchor(value, anchor))
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

#[derive(Default)]
struct PlanReferences {
    capabilities: BTreeSet<Vec<u8>>,
    skills: BTreeSet<Vec<u8>>,
}

fn collect_plan_references(plan: &ExecutionPlanDefinition) -> PlanReferences {
    let mut references = PlanReferences::default();
    for node in &plan.nodes {
        match &node.operation {
            ExecutionOperation::Capability { reference } => {
                insert_reference_key(&mut references.capabilities, reference);
            }
            ExecutionOperation::Agent {
                skill_refs,
                capability_refs,
                ..
            } => insert_agent_references(&mut references, skill_refs, capability_refs),
            ExecutionOperation::Map { task, .. } => match task {
                MapTask::Capability { reference } => {
                    insert_reference_key(&mut references.capabilities, reference);
                }
                MapTask::Agent {
                    skill_refs,
                    capability_refs,
                    ..
                } => insert_agent_references(&mut references, skill_refs, capability_refs),
            },
            ExecutionOperation::Reduce { reducer, .. } => match reducer {
                ExecutionReducer::Capability { reference } => {
                    insert_reference_key(&mut references.capabilities, reference);
                }
                ExecutionReducer::Agent {
                    skill_refs,
                    capability_refs,
                    ..
                } => insert_agent_references(&mut references, skill_refs, capability_refs),
            },
            ExecutionOperation::Review { .. }
            | ExecutionOperation::WaitSignal { .. }
            | ExecutionOperation::Output { .. } => {}
        }
    }
    references
}

fn validate_amendment_reference_narrowing(
    active: &ExecutionPlanDefinition,
    amended: &ExecutionPlanDefinition,
    report: &mut ExecutionValidationReport,
) {
    let active = collect_plan_references(active);
    let amended = collect_plan_references(amended);
    if !amended.capabilities.is_subset(&active.capabilities) {
        report.error(
            "authorization_broadened",
            "amendment.operations",
            "amendment introduces a capability reference not used by the active plan",
        );
    }
    if !amended.skills.is_subset(&active.skills) {
        report.error(
            "authorization_broadened",
            "amendment.operations",
            "amendment introduces a skill reference not used by the active plan",
        );
    }
}

fn insert_agent_references(
    references: &mut PlanReferences,
    skills: &[ArtifactRef],
    capabilities: &[moa_artifacts::execution_plan::CapabilityReference],
) {
    for reference in skills {
        insert_reference_key(&mut references.skills, reference);
    }
    for reference in capabilities {
        insert_reference_key(&mut references.capabilities, reference);
    }
}

fn insert_reference_key<T: Serialize>(set: &mut BTreeSet<Vec<u8>>, reference: &T) {
    if let Ok(key) = canonical_sort_key(reference) {
        set.insert(key);
    }
}

fn validate_plan_references(
    plan: &ExecutionPlanDefinition,
    catalog: &ExecutionCapabilityCatalog,
    authorization: &ExecutionAuthorizationEnvelope,
    report: &mut ExecutionValidationReport,
) {
    let catalog_refs = catalog
        .capabilities
        .iter()
        .filter_map(|capability| canonical_sort_key(&capability.reference).ok())
        .collect::<HashSet<_>>();
    let authorized_capabilities = authorization
        .capability_refs
        .iter()
        .filter_map(|reference| canonical_sort_key(reference).ok())
        .collect::<HashSet<_>>();
    let authorized_skills = authorization
        .skill_refs
        .iter()
        .filter_map(|reference| canonical_sort_key(reference).ok())
        .collect::<HashSet<_>>();

    let references = collect_plan_references(plan);
    for reference in &references.capabilities {
        if !catalog_refs.contains(reference) {
            report.error(
                "capability_not_in_catalog",
                "plan.nodes",
                "plan references a capability outside the pinned catalog",
            );
        }
        if !authorized_capabilities.contains(reference) {
            report.error(
                "capability_not_authorized",
                "plan.nodes",
                "plan references a capability outside the authorization envelope",
            );
        }
    }
    for reference in &references.skills {
        if !authorized_skills.contains(reference) {
            report.error(
                "skill_not_authorized",
                "plan.nodes",
                "plan references a skill outside the authorization envelope",
            );
        }
    }
}

fn estimate_plan(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    catalog: &ExecutionCapabilityCatalog,
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
) -> Option<ExecutionEstimate> {
    let catalog = capability_lookup(catalog);
    let mut total = ExecutionEstimate::default();
    for (index, node) in plan.nodes.iter().enumerate() {
        let Some(estimate) = estimate_node(node, &catalog, config, report, index) else {
            continue;
        };
        if let Some(limit) = &node.budget
            && let Err(error) = estimate_fits_limit(estimate, limit)
        {
            append_error(
                report,
                "node_budget_exceeded",
                format!("plan.nodes[{index}].budget"),
                error,
            );
        }
        match total.checked_add(estimate, "plan total estimate") {
            Ok(next) => total = next,
            Err(error) => append_error(
                report,
                "estimate_overflow",
                format!("plan.nodes[{index}]"),
                error,
            ),
        }
    }

    for (index, check) in goal.completion_checks.iter().enumerate() {
        let CompletionCheckKind::AgentVerifier { max_turns, .. } = check.kind else {
            continue;
        };
        let estimate = verifier_estimate(config, max_turns);
        match estimate.and_then(|estimate| total.checked_add(estimate, "verifier estimate")) {
            Ok(next) => total = next,
            Err(error) => append_error(
                report,
                "estimate_overflow",
                format!("goal.completion_checks[{index}]"),
                error,
            ),
        }
    }

    (!report.has_errors()).then_some(total)
}

fn estimate_remaining_plan(
    goal: &ExecutionGoalContract,
    plan: &ExecutionPlanDefinition,
    projection: &ExecutionProjection,
    catalog: &ExecutionCapabilityCatalog,
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
) -> Option<ExecutionEstimate> {
    let lookup = capability_lookup(catalog);
    let mut total = ExecutionEstimate::default();
    for (index, node) in plan.nodes.iter().enumerate() {
        if projection
            .node_statuses
            .get(&node.id)
            .is_some_and(|status| {
                matches!(
                    status,
                    ExecutionNodeStatus::Completed
                        | ExecutionNodeStatus::Skipped
                        | ExecutionNodeStatus::Failed
                        | ExecutionNodeStatus::Cancelled
                )
            })
        {
            continue;
        }
        if let Some(estimate) = estimate_node(node, &lookup, config, report, index) {
            match total.checked_add(estimate, "remaining plan estimate") {
                Ok(next) => total = next,
                Err(error) => append_error(
                    report,
                    "estimate_overflow",
                    format!("plan.nodes[{index}]"),
                    error,
                ),
            }
        }
    }

    for (index, check) in goal.completion_checks.iter().enumerate() {
        let CompletionCheckKind::AgentVerifier { max_turns, .. } = check.kind else {
            continue;
        };
        let node_id = format!("@check/{}", check.id);
        if projection.tasks.iter().any(|task| {
            task.node_id == node_id
                && matches!(
                    task.status,
                    ExecutionTaskStatus::Completed
                        | ExecutionTaskStatus::Failed
                        | ExecutionTaskStatus::Cancelled
                )
        }) {
            continue;
        }
        match verifier_estimate(config, max_turns)
            .and_then(|estimate| total.checked_add(estimate, "remaining verifier estimate"))
        {
            Ok(next) => total = next,
            Err(error) => append_error(
                report,
                "estimate_overflow",
                format!("goal.completion_checks[{index}]"),
                error,
            ),
        }
    }
    (!report.has_errors()).then_some(total)
}

fn capability_lookup(
    catalog: &ExecutionCapabilityCatalog,
) -> BTreeMap<Vec<u8>, &ExecutionCapability> {
    catalog
        .capabilities
        .iter()
        .filter_map(|capability| {
            canonical_sort_key(&capability.reference)
                .ok()
                .map(|key| (key, capability))
        })
        .collect()
}

fn estimate_node(
    node: &ExecutionNode,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
    config: &ExecutionConfig,
    report: &mut ExecutionValidationReport,
    index: usize,
) -> Option<ExecutionEstimate> {
    let attempts = u64::from(node.retry.max_attempts);
    let path = format!("plan.nodes[{index}].operation");
    let result = match &node.operation {
        ExecutionOperation::Capability { reference } => capability_estimate(reference, catalog)
            .and_then(|estimate| {
                estimate.checked_multiply_resources(attempts, "capability retry estimate")
            }),
        ExecutionOperation::Agent { max_turns, .. } => {
            agent_estimate(config, *max_turns).and_then(|estimate| {
                estimate.checked_multiply_resources(attempts, "agent retry estimate")
            })
        }
        ExecutionOperation::Map {
            max_items, task, ..
        } => map_task_estimate(task, catalog, config).and_then(|estimate| {
            estimate
                .checked_multiply_resources(attempts, "map retry estimate")?
                .checked_multiply_all(*max_items, "map cardinality estimate")
        }),
        ExecutionOperation::Reduce {
            max_items,
            reducer,
            batch_size,
            ..
        } => reducer_estimate(reducer, catalog, config).and_then(|estimate| {
            let tasks = reducer_task_count(*max_items, u64::from(*batch_size))?;
            estimate
                .checked_multiply_resources(attempts, "reduce retry estimate")?
                .checked_multiply_all(tasks, "reduce hierarchy estimate")
        }),
        ExecutionOperation::Review { .. }
        | ExecutionOperation::WaitSignal { .. }
        | ExecutionOperation::Output { .. } => Ok(ExecutionEstimate {
            tasks: 1,
            ..ExecutionEstimate::default()
        }),
    };

    match result {
        Ok(estimate) => Some(estimate),
        Err(error) => {
            append_error(report, "estimate_failed", path, error);
            None
        }
    }
}

fn capability_estimate(
    reference: &moa_artifacts::execution_plan::CapabilityReference,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
) -> Result<ExecutionEstimate, Error> {
    let key = canonical_sort_key(reference)?;
    catalog
        .get(&key)
        .map(|capability| capability.estimate)
        .ok_or_else(|| Error::InvalidProjection {
            message: format!(
                "capability {}@{} is absent from the catalog",
                reference.name, reference.version
            ),
        })
}

fn agent_estimate(config: &ExecutionConfig, max_turns: u32) -> Result<ExecutionEstimate, Error> {
    ExecutionEstimate {
        cost_microusd: config.agent_turn_cost_microusd,
        tokens: config.agent_turn_tokens,
        tool_calls: config.agent_turn_tool_calls,
        retrieved_bytes: config.agent_turn_retrieved_bytes,
        tasks: 1,
    }
    .checked_multiply_resources(u64::from(max_turns), "agent turn estimate")
}

fn verifier_estimate(config: &ExecutionConfig, max_turns: u32) -> Result<ExecutionEstimate, Error> {
    ExecutionEstimate {
        cost_microusd: config.verifier_turn_cost_microusd,
        tokens: config.verifier_turn_tokens,
        tool_calls: config.verifier_turn_tool_calls,
        retrieved_bytes: config.verifier_turn_retrieved_bytes,
        tasks: 1,
    }
    .checked_multiply_resources(u64::from(max_turns), "verifier turn estimate")
}

fn map_task_estimate(
    task: &MapTask,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
    config: &ExecutionConfig,
) -> Result<ExecutionEstimate, Error> {
    match task {
        MapTask::Capability { reference } => capability_estimate(reference, catalog),
        MapTask::Agent { max_turns, .. } => agent_estimate(config, *max_turns),
    }
}

fn reducer_estimate(
    reducer: &ExecutionReducer,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
    config: &ExecutionConfig,
) -> Result<ExecutionEstimate, Error> {
    match reducer {
        ExecutionReducer::Capability { reference } => capability_estimate(reference, catalog),
        ExecutionReducer::Agent { max_turns, .. } => agent_estimate(config, *max_turns),
    }
}

fn reducer_task_count(mut items: u64, batch_size: u64) -> Result<u64, Error> {
    if batch_size < 2 {
        return Err(Error::InvalidProjection {
            message: "reduce batch_size must be at least two".to_string(),
        });
    }
    let mut total = 0_u64;
    while items > 1 {
        let batches = items / batch_size + u64::from(!items.is_multiple_of(batch_size));
        total = total
            .checked_add(batches)
            .ok_or_else(|| Error::ArithmeticOverflow {
                context: "reduce task count".to_string(),
            })?;
        items = batches;
    }
    Ok(total)
}

fn apply_amendment(
    amendment: &PlanAmendment,
    projection: &ExecutionProjection,
    active: &ExecutionPlanDefinition,
    definition: &mut ExecutionPlanDefinition,
    report: &mut ExecutionValidationReport,
) {
    let waiting_replan_nodes = projection
        .tasks
        .iter()
        .filter(|task| task.status == ExecutionTaskStatus::WaitingReplan)
        .map(|task| task.node_id.as_str())
        .collect::<BTreeSet<_>>();
    if waiting_replan_nodes.len() > 1 {
        report.error(
            "multiple_replan_origins",
            "projection.tasks",
            "an amendment may supersede only one WaitingReplan node",
        );
    }

    let mut removed = BTreeSet::new();
    let mut replacement_ids = BTreeSet::new();
    let mut replaced_pending_ids = BTreeSet::new();
    for (index, operation) in amendment.operations.iter().enumerate() {
        let path = format!("amendment.operations[{index}]");
        match operation {
            PlanAmendmentOperation::AddNode { node } => {
                if active.nodes.iter().any(|existing| existing.id == node.id)
                    || definition
                        .nodes
                        .iter()
                        .any(|existing| existing.id == node.id)
                {
                    report.error(
                        "reused_task_identity",
                        format!("{path}.node.id"),
                        "added node ID must be distinct from every active-plan node ID",
                    );
                    continue;
                }
                if !is_downstream_of_completed(node, projection, active) {
                    report.error(
                        "addition_not_downstream",
                        format!("{path}.node.depends_on"),
                        "added work must be downstream of completed work",
                    );
                }
                replacement_ids.insert(node.id.as_str());
                definition.nodes.push(node.clone());
            }
            PlanAmendmentOperation::ReplacePendingNode { node_id, node } => {
                if node.id == *node_id
                    || active.nodes.iter().any(|existing| existing.id == node.id)
                    || definition
                        .nodes
                        .iter()
                        .any(|existing| existing.id == node.id)
                {
                    report.error(
                        "reused_task_identity",
                        format!("{path}.node.id"),
                        "replacement work must use a distinct new node ID",
                    );
                    continue;
                }
                if !node_is_replaceable(node_id, projection, false) {
                    report.error(
                        "immutable_node",
                        format!("{path}.node_id"),
                        "only a pending node may be replaced",
                    );
                    continue;
                }
                let Some(position) = definition.nodes.iter().position(|node| node.id == *node_id)
                else {
                    report.error(
                        "unknown_amendment_node",
                        format!("{path}.node_id"),
                        "replacement target does not exist",
                    );
                    continue;
                };
                validate_budget_narrowing(&definition.nodes[position], node, &path, report);
                validate_map_narrowing(&definition.nodes[position], node, &path, report);
                removed.insert(node_id.as_str());
                replacement_ids.insert(node.id.as_str());
                replaced_pending_ids.insert(node_id.as_str());
                definition.nodes[position] = node.clone();
            }
            PlanAmendmentOperation::RemovePendingNode { node_id } => {
                let waiting_origin = waiting_replan_nodes.contains(node_id.as_str());
                if !node_is_replaceable(node_id, projection, waiting_origin) {
                    report.error(
                        "immutable_node",
                        format!("{path}.node_id"),
                        "only a pending node or the originating WaitingReplan node may be removed",
                    );
                    continue;
                }
                let before = definition.nodes.len();
                definition.nodes.retain(|node| node.id != *node_id);
                if before == definition.nodes.len() {
                    report.error(
                        "unknown_amendment_node",
                        format!("{path}.node_id"),
                        "removal target does not exist",
                    );
                } else {
                    removed.insert(node_id.as_str());
                }
            }
        }
    }

    if let Some(waiting_node) = waiting_replan_nodes.first() {
        if !removed.contains(waiting_node) {
            report.error(
                "replan_origin_not_removed",
                "amendment.operations",
                "accepted replan must remove the originating WaitingReplan node",
            );
        }
        if replacement_ids.is_empty() {
            report.error(
                "replan_replacement_missing",
                "amendment.operations",
                "accepted replan must add replacement work under a distinct node ID",
            );
        }
        for dependent in active
            .nodes
            .iter()
            .filter(|node| {
                node.depends_on
                    .iter()
                    .any(|dependency| dependency == waiting_node)
            })
            .filter(|node| {
                projection
                    .node_statuses
                    .get(&node.id)
                    .copied()
                    .unwrap_or(ExecutionNodeStatus::Pending)
                    == ExecutionNodeStatus::Pending
            })
        {
            if !replaced_pending_ids.contains(dependent.id.as_str()) {
                report.error(
                    "stale_replan_dependent",
                    "amendment.operations",
                    format!(
                        "pending dependent {} must be replaced when its WaitingReplan dependency is removed",
                        dependent.id
                    ),
                );
            }
        }
    }

    for node in &definition.nodes {
        for dependency in &node.depends_on {
            if removed.contains(dependency.as_str()) {
                report.error(
                    "removed_dependency_referenced",
                    format!("plan.nodes.{}.depends_on", node.id),
                    "amended plan still references a removed node",
                );
            }
        }
    }
}

fn validate_budget_narrowing(
    active: &ExecutionNode,
    replacement: &ExecutionNode,
    path: &str,
    report: &mut ExecutionValidationReport,
) {
    if !budget_is_equal_or_narrower(active.budget.as_ref(), replacement.budget.as_ref()) {
        report.error(
            "node_budget_broadened",
            format!("{path}.node.budget"),
            "replacement node budget must be equal to or narrower than the active node budget",
        );
    }
}

fn budget_is_equal_or_narrower(
    active: Option<&ExecutionBudgetLimit>,
    replacement: Option<&ExecutionBudgetLimit>,
) -> bool {
    let Some(active) = active else {
        return true;
    };
    let Some(replacement) = replacement else {
        return false;
    };

    ceiling_is_equal_or_narrower(
        active.max_cost_microusd.as_ref(),
        replacement.max_cost_microusd.as_ref(),
    ) && ceiling_is_equal_or_narrower(active.max_tokens.as_ref(), replacement.max_tokens.as_ref())
        && ceiling_is_equal_or_narrower(active.max_tasks.as_ref(), replacement.max_tasks.as_ref())
        && ceiling_is_equal_or_narrower(
            active.max_tool_calls.as_ref(),
            replacement.max_tool_calls.as_ref(),
        )
        && ceiling_is_equal_or_narrower(
            active.max_retrieved_bytes.as_ref(),
            replacement.max_retrieved_bytes.as_ref(),
        )
        && ceiling_is_equal_or_narrower(
            active.deadline_at.as_ref(),
            replacement.deadline_at.as_ref(),
        )
}

fn ceiling_is_equal_or_narrower<T: Ord>(active: Option<&T>, replacement: Option<&T>) -> bool {
    active.is_none_or(|active| replacement.is_some_and(|replacement| replacement <= active))
}

fn validate_map_narrowing(
    active: &ExecutionNode,
    replacement: &ExecutionNode,
    path: &str,
    report: &mut ExecutionValidationReport,
) {
    let ExecutionOperation::Map {
        items: active_items,
        item_key: active_item_key,
        max_items: active_max_items,
        ..
    } = &active.operation
    else {
        return;
    };
    let ExecutionOperation::Map {
        items: replacement_items,
        item_key: replacement_item_key,
        max_items: replacement_max_items,
        ..
    } = &replacement.operation
    else {
        return;
    };

    if replacement_max_items > active_max_items {
        report.error(
            "map_scope_broadened",
            format!("{path}.node.operation.max_items"),
            "replacement map max_items must not exceed the active map bound",
        );
    }
    if replacement_item_key != active_item_key {
        report.error(
            "map_scope_broadened",
            format!("{path}.node.operation.item_key"),
            "replacement map must preserve the active item_key pointer",
        );
        return;
    }

    match map_items_are_equal_or_narrower(
        active_items,
        replacement_items,
        active_item_key,
    ) {
        Ok(true) => {}
        Ok(false) => report.error(
            "map_scope_broadened",
            format!("{path}.node.operation.items"),
            "replacement map items must be equal to or a provable literal subset of the active items",
        ),
        Err(error) => append_error(
            report,
            "map_scope_comparison_failed",
            format!("{path}.node.operation.items"),
            error,
        ),
    }
}

fn map_items_are_equal_or_narrower(
    active: &Value,
    replacement: &Value,
    item_key: &str,
) -> Result<bool, Error> {
    if canonical_json_bytes(active)? == canonical_json_bytes(replacement)? {
        return Ok(true);
    }
    let (Some(active), Some(replacement)) = (active.as_array(), replacement.as_array()) else {
        return Ok(false);
    };
    let mut active_by_key = BTreeMap::new();
    for item in active {
        active_by_key.insert(
            extract_map_key(item, item_key)?,
            canonical_json_bytes(item)?,
        );
    }
    for item in replacement {
        let key = extract_map_key(item, item_key)?;
        if active_by_key.get(&key) != Some(&canonical_json_bytes(item)?) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn node_is_replaceable(
    node_id: &str,
    projection: &ExecutionProjection,
    allow_waiting_replan: bool,
) -> bool {
    let status = projection
        .node_statuses
        .get(node_id)
        .copied()
        .unwrap_or(ExecutionNodeStatus::Pending);
    let task_evidence_is_replaceable = projection
        .tasks
        .iter()
        .filter(|task| task.node_id == node_id)
        .all(|task| {
            task.status == ExecutionTaskStatus::Pending
                || (allow_waiting_replan && task.status == ExecutionTaskStatus::WaitingReplan)
        });
    task_evidence_is_replaceable
        && (status == ExecutionNodeStatus::Pending
            || (allow_waiting_replan && status == ExecutionNodeStatus::Waiting))
}

fn is_downstream_of_completed(
    node: &ExecutionNode,
    projection: &ExecutionProjection,
    active: &ExecutionPlanDefinition,
) -> bool {
    if node.depends_on.iter().any(|dependency| {
        projection.node_statuses.get(dependency) == Some(&ExecutionNodeStatus::Completed)
    }) {
        return true;
    }
    let by_id = active
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect::<HashMap<_, _>>();
    let mut stack = node
        .depends_on
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut visited = HashSet::new();
    while let Some(id) = stack.pop() {
        if !visited.insert(id) {
            continue;
        }
        if projection.node_statuses.get(id) == Some(&ExecutionNodeStatus::Completed) {
            return true;
        }
        if let Some(dependency) = by_id.get(id) {
            stack.extend(dependency.depends_on.iter().map(String::as_str));
        }
    }
    false
}

fn append_error(
    report: &mut ExecutionValidationReport,
    code: &str,
    path: impl Into<String>,
    error: Error,
) {
    report.error(code, path, error.to_string());
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::Utc;
    use moa_artifacts::execution_plan::{
        CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit, ExecutionGoalContract,
        ExecutionNode, ExecutionOperation, ExecutionPlanDefinition, ExecutionRequirement,
        PlanAmendment, PlanAmendmentOperation, RetryPolicy,
    };
    use moa_config::ExecutionConfig;
    use serde_json::json;

    use super::*;

    #[test]
    fn execution_planning_compiler_rejects_unserved_immutable_requirement() {
        // Pins: a generated plan cannot silently drop one immutable user requirement.
        let mut request = output_only_compile_request();
        request.goal.requirements.push(ExecutionRequirement {
            id: "req_omitted".to_string(),
            description: "Preserve this second requirement.".to_string(),
        });

        let outcome = compile(request);

        assert!(outcome.compiled.is_none());
        assert_eq!(
            outcome
                .report
                .issues
                .iter()
                .filter(|issue| issue.code == "unserved_requirement")
                .count(),
            1
        );
    }

    #[test]
    fn execution_planning_amendment_cannot_remove_sole_goal_serving_output() {
        // Pins: restricted amendment validation cannot erase goal coverage or terminal output.
        let request = output_only_compile_request();
        let compiled = compile(request.clone())
            .compiled
            .expect("output-only fixture should compile");
        let outcome = validate_amendment(ValidateAmendmentRequest {
            goal: compiled.goal,
            active_plan: compiled.plan,
            amendment: PlanAmendment {
                schema_version: 1,
                base_plan_revision: 1,
                reason: "Remove the required output".to_string(),
                evidence: json!({ "failure": "none" }),
                operations: vec![PlanAmendmentOperation::RemovePendingNode {
                    node_id: "output".to_string(),
                }],
            },
            projection: ExecutionProjection {
                plan_revision: 1,
                node_statuses: BTreeMap::from([(
                    "output".to_string(),
                    crate::state::ExecutionNodeStatus::Pending,
                )]),
                tasks: Vec::new(),
            },
            catalog: request.catalog,
            authorization: request.authorization,
            remaining_budget: generous_budget(),
            config: ExecutionConfig::default(),
            now: Utc::now(),
        });

        assert!(outcome.plan.is_none());
        assert!(outcome.report.issues.iter().any(|issue| {
            issue.code == "unserved_requirement" || issue.code == "plan_structure"
        }));
    }

    fn output_only_compile_request() -> CompileExecutionRequest {
        let catalog = ExecutionCapabilityCatalog::build(Vec::new())
            .expect("empty capability catalog should be valid");
        CompileExecutionRequest {
            goal: ExecutionGoalContract {
                objective: "Produce the requested report.".to_string(),
                requirements: vec![ExecutionRequirement {
                    id: "req_report".to_string(),
                    description: "Produce the requested report.".to_string(),
                }],
                deliverables: Vec::new(),
                coverage: Vec::new(),
                constraints: Vec::new(),
                completion_checks: vec![CompletionCheck {
                    id: "check_output".to_string(),
                    description: "Validate the output schema.".to_string(),
                    requirement_ids: vec!["req_report".to_string()],
                    constraint_ids: Vec::new(),
                    kind: CompletionCheckKind::OutputSchema,
                }],
            },
            plan: ExecutionPlanDefinition {
                schema_version: 1,
                input_schema: json!({ "type": "object" }),
                output_schema: json!({ "type": "object" }),
                nodes: vec![ExecutionNode {
                    id: "output".to_string(),
                    requirement_ids: vec!["req_report".to_string()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({}),
                    output_schema: json!({ "type": "object" }),
                    operation: ExecutionOperation::Output {
                        value: json!({ "status": "complete" }),
                    },
                    retry: RetryPolicy {
                        max_attempts: 1,
                        initial_backoff_ms: 0,
                        max_backoff_ms: 0,
                    },
                    budget: None,
                }],
            },
            run_input: json!({}),
            authorization: ExecutionAuthorizationEnvelope {
                capability_refs: Vec::new(),
                skill_refs: Vec::new(),
            },
            approved_budget: generous_budget(),
            config: ExecutionConfig::default(),
            now: Utc::now(),
            catalog,
        }
    }

    fn generous_budget() -> ExecutionBudgetLimit {
        ExecutionBudgetLimit {
            max_cost_microusd: Some(1_000_000),
            max_tokens: Some(100_000),
            max_tasks: Some(100),
            max_tool_calls: Some(100),
            max_retrieved_bytes: Some(1_000_000),
            deadline_at: None,
        }
    }
}
