//! Structural, schema, reference, catalog, and authorization validation.

pub(super) mod schema_references;

use self::schema_references::validate_one_schema;
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
    match catalog_hash(&catalog.capabilities) {
        Ok(hash) if hash != catalog.catalog_hash => report.error(
            "catalog_hash_mismatch",
            "catalog.catalog_hash",
            "catalog_hash does not match canonical capabilities JSON",
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
        if let Some(compensation) = &node.compensation {
            insert_reference_key(&mut references.capabilities, &compensation.compensator);
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
    validate_compensations(plan, catalog, report);
    validate_cancel_compensation_coverage(plan, catalog, report);
}

pub(super) fn validate_cancel_compensation_coverage(
    plan: &ExecutionPlanDefinition,
    catalog: &ExecutionCapabilityCatalog,
    report: &mut ExecutionValidationReport,
) {
    use moa_artifacts::execution_plan::ExecutionCancelPolicy;
    use moa_core::types::action_policy::ActionClass;

    if plan.cancel_policy != ExecutionCancelPolicy::CompensateCommitted {
        return;
    }
    let capabilities = capability_lookup(catalog);
    let is_side_effecting = |reference: &moa_artifacts::execution_plan::CapabilityReference| {
        canonical_sort_key(reference)
            .ok()
            .and_then(|key| capabilities.get(&key).copied())
            .is_some_and(|capability| capability.action_class != ActionClass::Read)
    };

    for (index, node) in plan.nodes.iter().enumerate() {
        let operation_path = format!("plan.nodes[{index}].operation");
        match &node.operation {
            ExecutionOperation::Capability { reference } => {
                if is_side_effecting(reference) && node.compensation.is_none() {
                    report.error(
                        "uncompensated_cancel_effect",
                        format!("plan.nodes[{index}].compensation"),
                        "compensate_committed requires every direct side-effecting capability to declare compensation",
                    );
                }
            }
            ExecutionOperation::Agent {
                capability_refs, ..
            } => validate_no_indirect_side_effects(
                capability_refs,
                &format!("{operation_path}.capability_refs"),
                &is_side_effecting,
                report,
            ),
            ExecutionOperation::Map { task, .. } => match task {
                MapTask::Capability { reference } if is_side_effecting(reference) => report.error(
                    "indirect_compensation_unsupported",
                    format!("{operation_path}.task.reference"),
                    "compensate_committed does not support side-effecting map capability tasks",
                ),
                MapTask::Agent {
                    capability_refs, ..
                } => validate_no_indirect_side_effects(
                    capability_refs,
                    &format!("{operation_path}.task.capability_refs"),
                    &is_side_effecting,
                    report,
                ),
                MapTask::Capability { .. } => {}
            },
            ExecutionOperation::Reduce { reducer, .. } => match reducer {
                ExecutionReducer::Capability { reference } if is_side_effecting(reference) => {
                    report.error(
                        "indirect_compensation_unsupported",
                        format!("{operation_path}.reducer.reference"),
                        "compensate_committed does not support side-effecting reduce capability tasks",
                    );
                }
                ExecutionReducer::Agent {
                    capability_refs, ..
                } => validate_no_indirect_side_effects(
                    capability_refs,
                    &format!("{operation_path}.reducer.capability_refs"),
                    &is_side_effecting,
                    report,
                ),
                ExecutionReducer::Capability { .. } => {}
            },
            ExecutionOperation::Review { .. }
            | ExecutionOperation::WaitSignal { .. }
            | ExecutionOperation::Output { .. } => {}
        }
    }
}

fn validate_no_indirect_side_effects(
    references: &[moa_artifacts::execution_plan::CapabilityReference],
    path: &str,
    is_side_effecting: &impl Fn(&moa_artifacts::execution_plan::CapabilityReference) -> bool,
    report: &mut ExecutionValidationReport,
) {
    for (index, reference) in references.iter().enumerate() {
        if is_side_effecting(reference) {
            report.error(
                "indirect_compensation_unsupported",
                format!("{path}[{index}]"),
                "compensate_committed supports side effects only on directly compensated capability nodes",
            );
        }
    }
}

pub(super) fn validate_compensations(
    plan: &ExecutionPlanDefinition,
    catalog: &ExecutionCapabilityCatalog,
    report: &mut ExecutionValidationReport,
) {
    use moa_core::types::{action_policy::ActionClass, tools::IdempotencyClass};

    let capabilities = capability_lookup(catalog);
    for (index, node) in plan.nodes.iter().enumerate() {
        let Some(compensation) = &node.compensation else {
            continue;
        };
        let path = format!("plan.nodes[{index}].compensation");
        let ExecutionOperation::Capability { reference } = &node.operation else {
            continue;
        };
        let forward = canonical_sort_key(reference)
            .ok()
            .and_then(|key| capabilities.get(&key).copied());
        let Some(forward) = forward else {
            continue;
        };
        if forward.action_class == ActionClass::Read {
            report.error(
                "compensation_on_read",
                path.clone(),
                "read capabilities cannot declare compensation",
            );
        }
        match &forward.rollback {
            Some(rollback) if rollback.matches(compensation) => {}
            Some(_) => report.error(
                "compensation_contract_mismatch",
                path.clone(),
                "node compensation must exactly match the forward capability rollback contract",
            ),
            None => report.error(
                "compensation_not_promised",
                path.clone(),
                "forward capability does not promise exact rollback",
            ),
        }

        let compensator = canonical_sort_key(&compensation.compensator)
            .ok()
            .and_then(|key| capabilities.get(&key).copied());
        let Some(compensator) = compensator else {
            continue;
        };
        if compensator.action_class == ActionClass::Read {
            report.error(
                "read_compensator",
                format!("{path}.compensator"),
                "compensator must be a side-effecting capability, not a read",
            );
        }
        if compensator.idempotency_class != IdempotencyClass::Idempotent {
            report.error(
                "non_idempotent_compensator",
                format!("{path}.compensator"),
                "compensator must be idempotent for durable retry",
            );
        }

        let mut decoded_targets = Vec::<Vec<String>>::new();
        for (binding_index, binding) in compensation.input_mapping.bindings.iter().enumerate() {
            let binding_path = format!("{path}.input_mapping.bindings[{binding_index}]");
            let (source_schema, source_pointer) = match &binding.source {
                moa_artifacts::execution_plan::CompensationValueSource::OriginalInput {
                    pointer,
                } => (&forward.input_schema, pointer),
                moa_artifacts::execution_plan::CompensationValueSource::OriginalOutput {
                    pointer,
                } => (&forward.output_schema, pointer),
            };
            let Some(source_segments) = decode_json_pointer_segments(source_pointer) else {
                report.error(
                    "invalid_compensation_source_pointer",
                    format!("{binding_path}.source.pointer"),
                    "compensation source must be an RFC 6901 JSON Pointer",
                );
                continue;
            };
            let Some(target_segments) = decode_json_pointer_segments(&binding.target_pointer)
            else {
                report.error(
                    "invalid_compensation_target_pointer",
                    format!("{binding_path}.target_pointer"),
                    "compensation target must be an RFC 6901 JSON Pointer",
                );
                continue;
            };
            if decoded_targets.iter().any(|existing| {
                pointer_segments_are_strict_prefix(existing, &target_segments)
                    || pointer_segments_are_strict_prefix(&target_segments, existing)
            }) {
                report.error(
                    "compensation_target_collision",
                    format!("{binding_path}.target_pointer"),
                    "compensation target pointers must not overlap by parent/child path",
                );
            }
            decoded_targets.push(target_segments.clone());

            let Some(source_subschema) =
                resolve_simple_object_subschema(source_schema, &source_segments, true)
            else {
                report.error(
                    "unguaranteed_compensation_source_path",
                    format!("{binding_path}.source.pointer"),
                    "compensation source must be a required path in the supported object-schema subset",
                );
                continue;
            };
            let Some(target_subschema) =
                resolve_simple_object_subschema(&compensator.input_schema, &target_segments, false)
            else {
                report.error(
                    "unknown_compensation_target_path",
                    format!("{binding_path}.target_pointer"),
                    "compensation target must be declared in the supported object-schema subset",
                );
                continue;
            };
            match (
                canonical_json_bytes(source_subschema),
                canonical_json_bytes(target_subschema),
            ) {
                (Ok(source), Ok(target)) if source != target => report.error(
                    "incompatible_compensation_mapping",
                    binding_path,
                    "compensation source and target subschemas must be canonically identical",
                ),
                (Ok(_), Ok(_)) => {}
                (Err(error), _) | (_, Err(error)) => report.error(
                    "compensation_schema_compare_failed",
                    binding_path,
                    error.to_string(),
                ),
            }
        }
        if !required_compensation_targets_are_covered(&compensator.input_schema, &decoded_targets) {
            report.error(
                "unmapped_required_compensation_target",
                format!("{path}.input_mapping.bindings"),
                "compensation mappings must construct every required compensator input field",
            );
        }
    }
}

pub(super) fn decode_json_pointer_segments(pointer: &str) -> Option<Vec<String>> {
    if pointer.is_empty() {
        return Some(Vec::new());
    }
    let encoded_segments = pointer.strip_prefix('/')?;
    let mut segments = Vec::new();
    for encoded in encoded_segments.split('/') {
        let mut decoded = String::with_capacity(encoded.len());
        let mut chars = encoded.chars();
        while let Some(character) = chars.next() {
            if character != '~' {
                decoded.push(character);
                continue;
            }
            match chars.next() {
                Some('0') => decoded.push('~'),
                Some('1') => decoded.push('/'),
                Some(_) | None => return None,
            }
        }
        segments.push(decoded);
    }
    Some(segments)
}

pub(super) fn pointer_segments_are_strict_prefix(left: &[String], right: &[String]) -> bool {
    left.len() < right.len() && right.starts_with(left)
}

pub(super) fn resolve_simple_object_subschema<'a>(
    schema: &'a Value,
    segments: &[String],
    require_path: bool,
) -> Option<&'a Value> {
    let mut current = schema;
    for segment in segments {
        let object = current.as_object()?;
        if object.keys().any(|key| {
            matches!(
                key.as_str(),
                "$ref" | "allOf" | "anyOf" | "oneOf" | "if" | "then" | "else"
            )
        }) {
            return None;
        }
        if require_path
            && !object
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| {
                    required
                        .iter()
                        .any(|required| required.as_str() == Some(segment.as_str()))
                })
        {
            return None;
        }
        current = object
            .get("properties")
            .and_then(Value::as_object)?
            .get(segment)?;
    }
    Some(current)
}

pub(super) fn required_compensation_targets_are_covered(
    schema: &Value,
    targets: &[Vec<String>],
) -> bool {
    required_object_fields_are_covered(schema, &[], targets)
}

fn required_object_fields_are_covered(
    schema: &Value,
    prefix: &[String],
    targets: &[Vec<String>],
) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    if object.keys().any(|key| {
        matches!(
            key.as_str(),
            "$ref" | "allOf" | "anyOf" | "oneOf" | "if" | "then" | "else"
        )
    }) {
        return false;
    }
    let Some(required) = object.get("required") else {
        return true;
    };
    let Some(required) = required.as_array() else {
        return false;
    };
    let Some(properties) = object.get("properties").and_then(Value::as_object) else {
        return required.is_empty();
    };

    required.iter().all(|required| {
        let Some(field) = required.as_str() else {
            return false;
        };
        let mut path = prefix.to_vec();
        path.push(field.to_string());
        if targets
            .iter()
            .any(|target| path.starts_with(target.as_slice()))
        {
            return true;
        }
        if !targets
            .iter()
            .any(|target| target.len() > path.len() && target.starts_with(path.as_slice()))
        {
            return false;
        }
        properties
            .get(field)
            .is_some_and(|property| required_object_fields_are_covered(property, &path, targets))
    })
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
