//! Structural, schema, reference, catalog, and authorization validation.

use super::*;

pub(super) fn append_artifact_reports(
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

pub(super) fn validate_goal_plan_links(
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

pub(super) fn validate_catalog(
    catalog: &ExecutionCapabilityCatalog,
    report: &mut ExecutionValidationReport,
) {
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
        if let Err(error) = capability.validate_policy_context() {
            append_error(
                report,
                "invalid_capability_policy_context",
                format!("{path}.policy_context"),
                error,
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

pub(super) fn validate_capability_source(
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
        | CapabilitySource::InstalledConnectorAction {
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

pub(super) fn validate_authorization(
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

pub(super) fn validate_sorted_unique<'a, T: Serialize + 'a>(
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

pub(super) fn validate_schemas(
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

pub(super) fn validate_one_schema(
    schema: &Value,
    path: &str,
    report: &mut ExecutionValidationReport,
) {
    if let Err(error) = validate_schema(schema, path) {
        append_error(report, "invalid_json_schema", path, error);
    }
}

pub(super) fn validate_declared_reference_paths(
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

pub(super) fn validate_dynamic_reference_paths(
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

pub(super) fn validate_declared_reference_path(
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

pub(super) fn reference_tail_segments(tail: &str) -> Option<Vec<&str>> {
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

pub(super) fn valid_reference_segment(segment: &str) -> bool {
    let mut characters = segment.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic() || character == '_')
        && characters
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

pub(super) fn schema_declares_path(root: &Value, segments: &[&str]) -> bool {
    schema_declares_path_inner(root, root, segments, &mut HashSet::new())
}

pub(super) fn schema_declares_path_inner(
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

pub(super) fn resolve_local_schema_reference<'a>(
    root: &'a Value,
    reference: &str,
) -> Option<&'a Value> {
    let fragment = reference.strip_prefix('#')?;
    if fragment.is_empty() {
        return Some(root);
    }
    if fragment.starts_with('/') {
        return root.pointer(fragment);
    }
    find_schema_anchor(root, fragment)
}

pub(super) fn find_schema_anchor<'a>(schema: &'a Value, anchor: &str) -> Option<&'a Value> {
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
pub(super) struct PlanReferences {
    capabilities: BTreeSet<Vec<u8>>,
    skills: BTreeSet<Vec<u8>>,
}

pub(super) fn collect_plan_references(plan: &ExecutionPlanDefinition) -> PlanReferences {
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

pub(super) fn validate_amendment_reference_narrowing(
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

pub(super) fn insert_agent_references(
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

pub(super) fn insert_reference_key<T: Serialize>(set: &mut BTreeSet<Vec<u8>>, reference: &T) {
    if let Ok(key) = canonical_sort_key(reference) {
        set.insert(key);
    }
}

pub(super) fn validate_plan_references(
    plan: &ExecutionPlanDefinition,
    catalog: &ExecutionCapabilityCatalog,
    authorization: &ExecutionAuthorizationEnvelope,
    report: &mut ExecutionValidationReport,
) {
    validate_agent_tool_name_ambiguity(plan, catalog, report);
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

pub(super) fn validate_agent_tool_name_ambiguity(
    plan: &ExecutionPlanDefinition,
    catalog: &ExecutionCapabilityCatalog,
    report: &mut ExecutionValidationReport,
) {
    let capabilities = capability_lookup(catalog);
    for (index, node) in plan.nodes.iter().enumerate() {
        match &node.operation {
            ExecutionOperation::Agent {
                capability_refs, ..
            } => validate_agent_tool_refs(
                capability_refs,
                &format!("plan.nodes[{index}].operation.capability_refs"),
                &capabilities,
                report,
            ),
            ExecutionOperation::Map {
                task: MapTask::Agent {
                    capability_refs, ..
                },
                ..
            } => validate_agent_tool_refs(
                capability_refs,
                &format!("plan.nodes[{index}].operation.task.capability_refs"),
                &capabilities,
                report,
            ),
            ExecutionOperation::Reduce {
                reducer:
                    ExecutionReducer::Agent {
                        capability_refs, ..
                    },
                ..
            } => validate_agent_tool_refs(
                capability_refs,
                &format!("plan.nodes[{index}].operation.reducer.capability_refs"),
                &capabilities,
                report,
            ),
            ExecutionOperation::Capability { .. }
            | ExecutionOperation::Map { .. }
            | ExecutionOperation::Reduce { .. }
            | ExecutionOperation::Review { .. }
            | ExecutionOperation::WaitSignal { .. }
            | ExecutionOperation::Output { .. } => {}
        }
    }
}

pub(super) fn validate_agent_tool_refs(
    references: &[moa_artifacts::execution_plan::CapabilityReference],
    path: &str,
    catalog: &BTreeMap<Vec<u8>, &ExecutionCapability>,
    report: &mut ExecutionValidationReport,
) {
    let mut visible_names =
        BTreeMap::<&str, Vec<&moa_artifacts::execution_plan::CapabilityReference>>::new();
    for reference in references {
        let Some(capability) = canonical_sort_key(reference)
            .ok()
            .and_then(|key| catalog.get(&key).copied())
        else {
            continue;
        };
        if let Some(tool_name) = capability.source.model_visible_tool_name() {
            let visible_references = visible_names.entry(tool_name).or_default();
            if !visible_references
                .iter()
                .any(|visible_reference| **visible_reference == *reference)
            {
                visible_references.push(reference);
            }
        }
    }
    for (tool_name, mut visible_references) in visible_names {
        if visible_references.len() > 1 {
            visible_references.sort_by(|left, right| {
                left.name
                    .cmp(&right.name)
                    .then_with(|| left.version.cmp(&right.version))
            });
            let references = visible_references
                .iter()
                .map(|reference| format!("{}@{}", reference.name, reference.version))
                .collect::<Vec<_>>()
                .join(" and ");
            report.error(
                "ambiguous_agent_capability_tool",
                path,
                format!(
                    "task-local agent capability references {references} resolve to ambiguous model-visible tool `{tool_name}`"
                ),
            );
        }
    }
}

pub(super) fn append_error(
    report: &mut ExecutionValidationReport,
    code: &str,
    path: impl Into<String>,
    error: Error,
) {
    report.error(code, path, error.to_string());
}
