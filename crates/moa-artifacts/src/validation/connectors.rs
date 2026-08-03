//! Connector artifact and agent-binding validation.

use std::collections::HashSet;

use serde_json::Value;

use crate::agent::ConnectorBinding;
use crate::connector::{
    ConnectorDefinition, HttpOperationContract, HttpPathInput, RuntimeConnectorAction,
    RuntimeConnectorAuthRequirement, RuntimeOperationPolicy, is_runtime_action_id,
};
use crate::document::ArtifactKind;

use super::{ValidationReport, is_json_pointer, require_non_empty};

const MAX_CONNECTOR_ACTIONS: usize = 64;

/// Validates connector bindings embedded in an agent action policy.
pub(super) fn validate_bindings(bindings: &[ConnectorBinding], report: &mut ValidationReport) {
    let mut connector_refs = HashSet::new();
    let mut connection_ids = HashSet::new();
    for (index, binding) in bindings.iter().enumerate() {
        let root = format!("definition.spec.action_policy.connector_bindings[{index}]");
        if binding.connector_ref.artifact_kind() != Some(&ArtifactKind::Connector) {
            report.push_error(
                format!("{root}.connector_ref"),
                "connector binding reference must use connector://",
            );
        }
        let canonical = binding.connector_ref.to_string();
        if !connector_refs.insert(canonical) {
            report.push_error(
                format!("{root}.connector_ref"),
                "duplicate connector binding reference",
            );
        }
        if !connection_ids.insert(binding.connection_id) {
            report.push_error(
                format!("{root}.connection_id"),
                "connection may be bound to only one logical connector reference",
            );
        }
    }
}

/// Validates one reviewed HTTP connector definition.
pub(super) fn validate(definition: &ConnectorDefinition, report: &mut ValidationReport) {
    require_non_empty(
        "definition.spec.display_name",
        &definition.display_name,
        "connector display_name",
        report,
    );

    let declared_slots = validate_runtime_connector_auth(&definition.auth, report);
    let has_no_auth = matches!(
        definition.auth.as_slice(),
        [RuntimeConnectorAuthRequirement::None]
    );
    if definition.actions.is_empty() {
        report.push_error(
            "definition.spec.actions",
            "runtime connector must declare at least one action",
        );
    } else if definition.actions.len() > MAX_CONNECTOR_ACTIONS {
        report.push_error(
            "definition.spec.actions",
            "runtime connector must declare at most 64 actions",
        );
    }
    let mut action_ids = HashSet::new();
    for (index, action) in definition.actions.iter().enumerate() {
        let root = format!("definition.spec.actions[{index}]");
        if !is_runtime_action_id(&action.id) {
            report.push_error(
                format!("{root}.id"),
                "runtime connector action id must match [A-Za-z][A-Za-z0-9_-]{0,23}",
            );
        } else if !action_ids.insert(action.id.as_str()) {
            report.push_error(
                format!("{root}.id"),
                "duplicate runtime connector action id",
            );
        }
        validate_runtime_connector_action(
            &root,
            action,
            &definition.auth,
            &declared_slots,
            has_no_auth,
            report,
        );
    }
}

fn validate_runtime_connector_auth<'a>(
    auth: &'a [RuntimeConnectorAuthRequirement],
    report: &mut ValidationReport,
) -> HashSet<&'a str> {
    if auth.is_empty() {
        report.push_error(
            "definition.spec.auth",
            "runtime connector must declare one or more auth requirements",
        );
    }
    let none_count = auth
        .iter()
        .filter(|requirement| matches!(requirement, RuntimeConnectorAuthRequirement::None))
        .count();
    if none_count > 0 && auth.len() != 1 {
        report.push_error(
            "definition.spec.auth",
            "none must be the sole auth requirement",
        );
    }

    let mut slots = HashSet::new();
    for (index, requirement) in auth.iter().enumerate() {
        if let Some(slot) = requirement.slot()
            && !slots.insert(slot.as_str())
        {
            report.push_error(
                format!("definition.spec.auth[{index}].slot"),
                "duplicate credential slot",
            );
        }
    }
    slots
}

fn validate_runtime_connector_action(
    root: &str,
    action: &RuntimeConnectorAction,
    auth: &[RuntimeConnectorAuthRequirement],
    declared_slots: &HashSet<&str>,
    has_no_auth: bool,
    report: &mut ValidationReport,
) {
    validate_http_operation_contract(
        &format!("{root}.contract"),
        &action.contract,
        auth,
        declared_slots,
        has_no_auth,
        report,
    );
}

fn validate_http_operation_contract(
    root: &str,
    contract: &HttpOperationContract,
    auth: &[RuntimeConnectorAuthRequirement],
    declared_slots: &HashSet<&str>,
    has_no_auth: bool,
    report: &mut ValidationReport,
) {
    validate_http_path_template(root, contract, report);

    let mut query_parameters = HashSet::new();
    for (index, mapping) in contract.query_inputs.iter().enumerate() {
        let mapping_root = format!("{root}.query_inputs[{index}]");
        if !is_http_parameter_name(&mapping.parameter) {
            report.push_error(
                format!("{mapping_root}.parameter"),
                "query parameter must be a fixed RFC-compatible name",
            );
        } else if !query_parameters.insert(mapping.parameter.as_str()) {
            report.push_error(
                format!("{mapping_root}.parameter"),
                "duplicate query parameter mapping",
            );
        }
        if !is_json_pointer(&mapping.input_pointer) {
            report.push_error(
                format!("{mapping_root}.input_pointer"),
                "value must be an RFC 6901 JSON Pointer",
            );
        }
    }
    if let Some(body) = &contract.body_input
        && !is_json_pointer(&body.input_pointer)
    {
        report.push_error(
            format!("{root}.body_input.input_pointer"),
            "value must be an RFC 6901 JSON Pointer",
        );
    }
    if let Some(pointer) = &contract.response_pointer
        && !is_json_pointer(pointer)
    {
        report.push_error(
            format!("{root}.response_pointer"),
            "value must be an RFC 6901 JSON Pointer",
        );
    }

    match contract.credential_slot.as_ref() {
        Some(_) if has_no_auth => report.push_error(
            format!("{root}.credential_slot"),
            "an unauthenticated connector operation cannot select a credential slot",
        ),
        Some(slot) if !declared_slots.contains(slot.as_str()) => report.push_error(
            format!("{root}.credential_slot"),
            "HTTP operation selects an unknown credential slot",
        ),
        None if !has_no_auth => report.push_error(
            format!("{root}.credential_slot"),
            "authenticated HTTP operation must select one declared credential slot",
        ),
        Some(_) | None => {}
    }

    if let Some(header) = &contract.upstream_idempotency_header {
        if contract.policy.idempotency != moa_core::types::tools::IdempotencyClass::Idempotent {
            report.push_error(
                format!("{root}.upstream_idempotency_header"),
                "upstream idempotency header requires idempotent operation semantics",
            );
        }
        if auth.iter().any(|requirement| {
            matches!(
                requirement,
                RuntimeConnectorAuthRequirement::ApiKeyHeader {
                    header: auth_header,
                    ..
                } if auth_header.as_str() == header.as_str()
            )
        }) {
            report.push_error(
                format!("{root}.upstream_idempotency_header"),
                "upstream idempotency header must not collide with an authentication header",
            );
        }
    }

    if !(1..=1_048_576).contains(&contract.max_request_bytes) {
        report.push_error(
            format!("{root}.max_request_bytes"),
            "max_request_bytes must be in 1..=1048576",
        );
    }
    if !(1..=10_485_760).contains(&contract.max_response_bytes) {
        report.push_error(
            format!("{root}.max_response_bytes"),
            "max_response_bytes must be in 1..=10485760",
        );
    }
    if !(100..=10_000).contains(&contract.connect_timeout_ms) {
        report.push_error(
            format!("{root}.connect_timeout_ms"),
            "connect_timeout_ms must be in 100..=10000",
        );
    }
    if !(100..=60_000).contains(&contract.total_timeout_ms) {
        report.push_error(
            format!("{root}.total_timeout_ms"),
            "total_timeout_ms must be in 100..=60000",
        );
    }
    if contract.total_timeout_ms < contract.connect_timeout_ms {
        report.push_error(
            format!("{root}.total_timeout_ms"),
            "total_timeout_ms must be at least connect_timeout_ms",
        );
    }
    validate_runtime_operation_policy(&format!("{root}.policy"), &contract.policy, report);
}

fn validate_http_path_template(
    root: &str,
    contract: &HttpOperationContract,
    report: &mut ValidationReport,
) {
    validate_http_path_parts(root, &contract.path_template, &contract.path_inputs, report);
}

fn validate_http_path_parts(
    root: &str,
    path: &str,
    path_inputs: &[HttpPathInput],
    report: &mut ValidationReport,
) {
    let lowercase = path.to_ascii_lowercase();
    let invalid_path = !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('?')
        || path.contains('#')
        || path.contains('\\')
        || lowercase.contains("://")
        || lowercase.contains("%2e")
        || lowercase.contains("%2f")
        || lowercase.contains("%5c")
        || lowercase.contains("%25")
        || path.split('/').any(|segment| matches!(segment, "." | ".."));
    if invalid_path {
        report.push_error(
            format!("{root}.path_template"),
            "path_template must be a safe origin-relative path without authority, query, fragment, dot segments, or encoded separators",
        );
    }

    let mut placeholders = HashSet::new();
    for segment in path.split('/').skip(1) {
        if segment.starts_with('{') && segment.ends_with('}') && segment.len() > 2 {
            let placeholder = &segment[1..segment.len() - 1];
            if !is_http_parameter_name(placeholder) || !placeholders.insert(placeholder) {
                report.push_error(
                    format!("{root}.path_template"),
                    "path placeholders must be unique complete segments with stable names",
                );
            }
        } else if segment.contains('{') || segment.contains('}') {
            report.push_error(
                format!("{root}.path_template"),
                "path placeholders must occupy a complete path segment",
            );
        }
    }

    let mut mappings = HashSet::new();
    for (index, mapping) in path_inputs.iter().enumerate() {
        let mapping_root = format!("{root}.path_inputs[{index}]");
        if !is_http_parameter_name(&mapping.placeholder) {
            report.push_error(
                format!("{mapping_root}.placeholder"),
                "path placeholder mapping must use a stable name",
            );
        } else if !mappings.insert(mapping.placeholder.as_str()) {
            report.push_error(
                format!("{mapping_root}.placeholder"),
                "duplicate path placeholder mapping",
            );
        }
        if !is_json_pointer(&mapping.input_pointer) {
            report.push_error(
                format!("{mapping_root}.input_pointer"),
                "value must be an RFC 6901 JSON Pointer",
            );
        }
    }
    if placeholders != mappings {
        report.push_error(
            format!("{root}.path_inputs"),
            "path placeholder mappings must exactly match the path template",
        );
    }
}

fn validate_runtime_operation_policy(
    root: &str,
    policy: &RuntimeOperationPolicy,
    report: &mut ValidationReport,
) {
    validate_bounded_runtime_schema(
        &format!("{root}.input_schema"),
        &policy.input_schema,
        report,
    );
    validate_bounded_runtime_schema(
        &format!("{root}.output_schema"),
        &policy.output_schema,
        report,
    );
    if policy.data_classes.is_empty() {
        report.push_error(
            format!("{root}.data_classes"),
            "operation must declare at least one data class",
        );
    }
    let mut data_classes = HashSet::new();
    for (index, data_class) in policy.data_classes.iter().enumerate() {
        if !data_classes.insert(*data_class) {
            report.push_error(
                format!("{root}.data_classes[{index}]"),
                "duplicate operation data class",
            );
        }
    }
}

fn validate_bounded_runtime_schema(path: &str, schema: &Value, report: &mut ValidationReport) {
    if !schema.is_object() {
        report.push_error(path, "JSON schema must be an object");
        return;
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        report.push_error(path, "JSON schema root type must be object");
    }
    let serialized_len = serde_json::to_vec(schema).map_or(usize::MAX, |bytes| bytes.len());
    if serialized_len > 65_536 {
        report.push_error(path, "JSON schema must serialize to at most 65536 bytes");
    }
    let mut summary = RuntimeSchemaSummary {
        contains_reference: contains_schema_reference(schema),
        ..RuntimeSchemaSummary::default()
    };
    summarize_runtime_schema(schema, 1, &mut summary);
    if summary.max_depth > 16 {
        report.push_error(path, "JSON schema must have at most 16 nested levels");
    }
    if summary.property_count > 256 {
        report.push_error(
            path,
            "JSON schema must declare at most 256 object properties",
        );
    }
    if summary.invalid_child_schema {
        report.push_error(
            path,
            "JSON schema child keywords must contain object or boolean schemas",
        );
    }
    if summary.contains_reference {
        report.push_error(path, "JSON schema must not contain $ref or $dynamicRef");
    }
}

#[derive(Default)]
struct RuntimeSchemaSummary {
    max_depth: usize,
    property_count: usize,
    invalid_child_schema: bool,
    contains_reference: bool,
}

fn summarize_runtime_schema(value: &Value, depth: usize, summary: &mut RuntimeSchemaSummary) {
    summary.max_depth = summary.max_depth.max(depth);
    let Value::Object(object) = value else {
        return;
    };
    for key in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(Value::Object(children)) = object.get(key) {
            if key == "properties" {
                summary.property_count = summary.property_count.saturating_add(children.len());
            }
            for child in children.values() {
                summarize_runtime_child_schema(child, depth.saturating_add(1), summary);
            }
        } else if object.contains_key(key) {
            summary.invalid_child_schema = true;
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "contentSchema",
    ] {
        if let Some(child) = object.get(key) {
            summarize_runtime_child_schema(child, depth.saturating_add(1), summary);
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(Value::Array(children)) = object.get(key) {
            for child in children {
                summarize_runtime_child_schema(child, depth.saturating_add(1), summary);
            }
        } else if object.contains_key(key) {
            summary.invalid_child_schema = true;
        }
    }
    if let Some(Value::Object(children)) = object.get("dependentSchemas") {
        for child in children.values() {
            summarize_runtime_child_schema(child, depth.saturating_add(1), summary);
        }
    } else if object.contains_key("dependentSchemas") {
        summary.invalid_child_schema = true;
    }
}

fn summarize_runtime_child_schema(value: &Value, depth: usize, summary: &mut RuntimeSchemaSummary) {
    match value {
        Value::Object(_) | Value::Bool(_) => summarize_runtime_schema(value, depth, summary),
        Value::Null | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            summary.invalid_child_schema = true;
        }
    }
}

fn contains_schema_reference(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.contains_key("$ref")
                || object.contains_key("$dynamicRef")
                || object.values().any(contains_schema_reference)
        }
        Value::Array(array) => array.iter().any(contains_schema_reference),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

fn is_http_parameter_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphabetic)
        && bytes[1..]
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}
