//! JSON Schema and declared execution-reference validation.

use std::collections::{HashMap, HashSet};

use moa_artifacts::execution_plan::{
    ExecutionCondition, ExecutionGoalContract, ExecutionOperation, ExecutionPlanDefinition,
};
use serde_json::Value;

use crate::{compiler::ExecutionValidationReport, schema::validate_schema};

use super::append_error;

pub(in crate::compiler) fn validate_schemas(
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

pub(in crate::compiler) fn validate_declared_reference_paths(
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
