//! Offline contract coverage for constrained-HTTP connector definitions.

use moa_artifacts::connector::{
    ApiKeyHeaderName, UpstreamIdempotencyHeaderName, validate_connector_action_id,
};
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::validation::{ValidationError, validate_for_status};
use serde_json::{Value, json};

#[test]
fn connector_definition_round_trips_as_one_versioned_http_shape() {
    // Pins: connector artifacts have one explicit versioned HTTP contract and
    // cannot decode legacy aliases, runtime selectors, or managed operations.
    let document =
        connector_document_from_spec(http_spec()).expect("valid connector should decode");
    let encoded = document.to_json().expect("connector should serialize");
    assert_eq!(
        ArtifactDocument::from_json(&encoded).expect("connector should round trip"),
        document
    );
    assert!(validate_for_status(&document, ArtifactStatus::Draft).is_ok());

    for invalid in [
        json!({"auth": {}, "actions": [], "ui": {}}),
        json!({
            "definition_version": "v1",
            "display_name": "Invalid",
            "runtime": {"type": "constrained_http"},
            "auth": [{"type": "none"}],
            "actions": []
        }),
        json!({
            "definition_version": "v1",
            "display_name": "Invalid",
            "auth": [{"type": "none"}],
            "actions": [{
                "id": "read",
                "binding": {"type": "built_in_managed", "operation": "read"}
            }]
        }),
    ] {
        assert!(connector_document_from_spec(invalid).is_err());
    }
}

#[test]
fn connector_definition_accepts_64_actions_and_rejects_65() {
    // Pins: artifact validation enforces the connector action fanout limit at
    // the exact accepted/rejected boundary before connection activation.
    let accepted = connector_document_from_spec(http_spec_with_action_count(64))
        .expect("64-action connector should decode");
    assert!(
        validate_for_status(&accepted, ArtifactStatus::Draft).is_ok(),
        "exactly 64 connector actions should be accepted"
    );

    let rejected = connector_document_from_spec(http_spec_with_action_count(65))
        .expect("65-action connector should decode before semantic validation");
    assert_eq!(
        validate_for_status(&rejected, ArtifactStatus::Draft).errors,
        vec![ValidationError {
            path: "definition.spec.actions".to_string(),
            message: "runtime connector must declare at most 64 actions".to_string(),
        }]
    );
}

#[test]
fn connector_policy_floor_is_not_authorable() {
    // Pins: every custom HTTP action is classified as external-write/high and
    // requires admin review from host policy rather than authored fields.
    for field in ["action_class", "risk_level", "minimum_effect"] {
        let mut invalid = http_spec();
        invalid["actions"][0]["contract"]["policy"][field] = json!("allow");
        assert!(connector_document_from_spec(invalid).is_err());
    }
}

#[test]
fn connector_http_validation_rejects_escaping_paths_and_inexact_placeholders() {
    // Pins: model inputs may fill declared segments but never select an origin or URL shape.
    for path in [
        "https://attacker.example/invoices/{invoice_id}",
        "//attacker.example/invoices/{invoice_id}",
        "/invoices/../secrets",
        "/invoices/%2e%2e/secrets",
        "/invoices/{invoice_id}?admin=true",
        "/invoices/prefix-{invoice_id}",
    ] {
        let mut spec = http_spec();
        spec["actions"][0]["contract"]["path_template"] = json!(path);
        assert_validation_error(&spec, "definition.spec.actions[0].contract.path_template");
    }

    let mut missing_mapping = http_spec();
    missing_mapping["actions"][0]["contract"]["path_inputs"] = json!([]);
    assert_validation_error(
        &missing_mapping,
        "definition.spec.actions[0].contract.path_inputs",
    );
}

#[test]
fn connector_auth_slots_are_explicit_unique_and_selected() {
    // Pins: an operation cannot guess, omit, or ambiguously select credentials.
    let mut empty = http_spec();
    empty["auth"] = json!([]);
    assert_validation_error(&empty, "definition.spec.auth");

    let mut duplicate = http_spec();
    duplicate["auth"] = json!([
        {"type": "bearer", "slot": "primary"},
        {"type": "managed_oauth", "slot": "primary"}
    ]);
    assert_validation_error(&duplicate, "definition.spec.auth[1].slot");

    let mut missing = http_spec();
    missing["actions"][0]["contract"]
        .as_object_mut()
        .expect("contract should be an object")
        .remove("credential_slot");
    assert_validation_error(
        &missing,
        "definition.spec.actions[0].contract.credential_slot",
    );
}

#[test]
fn connector_headers_reject_transport_overrides_and_auth_collisions() {
    // Pins: fixed credential and idempotency headers cannot smuggle transport policy.
    for header in [
        "Authorization",
        "Host",
        "Content-Length",
        "Cookie",
        "Proxy-Authorization",
        "X-Forwarded-Host",
        "X-HTTP-Method-Override",
    ] {
        assert!(header.parse::<ApiKeyHeaderName>().is_err());
        assert!(header.parse::<UpstreamIdempotencyHeaderName>().is_err());
    }

    let accepted = "X-Acme-Key"
        .parse::<ApiKeyHeaderName>()
        .expect("ordinary vendor header should validate");
    assert_eq!(accepted.as_str(), "x-acme-key");

    let mut collision = http_spec();
    collision["auth"] = json!([{
        "type": "api_key_header",
        "slot": "primary",
        "header": "X-Request-Key"
    }]);
    collision["actions"][0]["contract"]["upstream_idempotency_header"] = json!("x-request-key");
    assert_validation_error(
        &collision,
        "definition.spec.actions[0].contract.upstream_idempotency_header",
    );
}

#[test]
fn connector_schemas_and_transport_limits_are_bounded() {
    // Pins: authored schemas cannot resolve externally or remove byte/deadline ceilings.
    let mut external_ref = http_spec();
    set_input_schema(
        &mut external_ref,
        json!({"type": "object", "$ref": "https://example.invalid/schema"}),
    );
    assert_validation_error(
        &external_ref,
        "definition.spec.actions[0].contract.policy.input_schema",
    );

    for (field, value) in [
        ("max_request_bytes", json!(0)),
        ("max_response_bytes", json!(10_485_761)),
        ("connect_timeout_ms", json!(99)),
        ("total_timeout_ms", json!(60_001)),
    ] {
        let mut invalid = http_spec();
        invalid["actions"][0]["contract"][field] = value;
        assert_validation_error(
            &invalid,
            &format!("definition.spec.actions[0].contract.{field}"),
        );
    }
}

#[test]
fn connector_action_ids_and_secret_free_shape_fail_closed() {
    // Pins: action IDs retain the artifact-owned grammar and connector DTOs
    // have no field capable of carrying credential material.
    assert!(validate_connector_action_id("Action_12345678901234567").is_ok());
    assert!(validate_connector_action_id("1bad").is_err());
    assert!(validate_connector_action_id(&"A".repeat(25)).is_err());

    let mut duplicate = http_spec();
    let duplicate_action = duplicate["actions"][0].clone();
    duplicate["actions"]
        .as_array_mut()
        .expect("actions should be an array")
        .push(duplicate_action);
    assert_validation_error(&duplicate, "definition.spec.actions[1].id");

    let mut secret = http_spec();
    secret["auth"][0]["secret"] = json!("must-not-decode");
    assert!(connector_document_from_spec(secret).is_err());
}

fn http_spec() -> Value {
    json!({
        "definition_version": "v1",
        "display_name": "Billing API",
        "description": "Reviewed billing operations.",
        "auth": [{"type": "bearer", "slot": "primary"}],
        "actions": [{
            "id": "create_invoice",
            "description": "Create one invoice.",
            "contract": {
                "method": "POST",
                "path_template": "/invoices/{invoice_id}",
                "path_inputs": [{
                    "placeholder": "invoice_id",
                    "input_pointer": "/invoice_id"
                }],
                "body_input": {"input_pointer": "/body"},
                "credential_slot": "primary",
                "upstream_idempotency_header": "Idempotency-Key",
                "response_pointer": "/invoice",
                "max_request_bytes": 65536,
                "max_response_bytes": 1048576,
                "connect_timeout_ms": 1000,
                "total_timeout_ms": 5000,
                "policy": {
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "invoice_id": {"type": "string"},
                            "body": {"type": "object"}
                        },
                        "required": ["invoice_id", "body"],
                        "additionalProperties": false
                    },
                    "output_schema": {"type": "object"},
                    "data_classes": ["restricted"],
                    "idempotency": "idempotent"
                }
            }
        }]
    })
}

fn http_spec_with_action_count(action_count: usize) -> Value {
    let mut spec = http_spec();
    let action = spec["actions"][0].clone();
    spec["actions"] = Value::Array(
        (0..action_count)
            .map(|index| {
                let mut action = action.clone();
                action["id"] = json!(format!("action_{index}"));
                action
            })
            .collect(),
    );
    spec
}

fn set_input_schema(spec: &mut Value, schema: Value) {
    spec["actions"][0]["contract"]["policy"]["input_schema"] = schema;
}

fn connector_document_from_spec(spec: Value) -> moa_artifacts::Result<ArtifactDocument> {
    let envelope = json!({
        "api_version": "moa.artifact/v1",
        "kind": "connector",
        "metadata": {"name": "billing"},
        "definition": {"type": "connector", "spec": spec}
    });
    ArtifactDocument::from_json(&envelope.to_string())
}

fn assert_validation_error(spec: &Value, path: &str) {
    let document = connector_document_from_spec(spec.clone()).expect("fixture should decode");
    assert_error_path(
        &validate_for_status(&document, ArtifactStatus::Draft).errors,
        path,
    );
}

fn assert_error_path(errors: &[ValidationError], path: &str) {
    assert!(
        errors.iter().any(|error| error.path == path),
        "expected validation error at {path}, got {errors:?}"
    );
}
