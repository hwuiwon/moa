//! Offline contract coverage for versioned connector definitions.

use std::collections::HashSet;

use moa_artifacts::canonical::canonical_hash;
use moa_artifacts::connector::{
    ApiKeyHeaderName, ConnectorActionRef, UpstreamIdempotencyHeaderName,
    connection_action_tool_reference,
};
use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument, ArtifactStatus};
use moa_artifacts::validation::{ValidationError, validate_for_status};
use moa_core::types::identifiers::ConnectorConnectionId;
use serde_json::{Value, json};
use uuid::Uuid;

const REVIEWED_CONNECTOR_CANONICAL_HASHES: [(&str, &str); 7] = [
    (
        "legacy",
        "6de50e0eaf752c84b85207e00fdda6b2ebcc5841f7ce448aec85adc07c8dc8c6",
    ),
    (
        "http_get_bearer",
        "55d28ac78fcf17a02838f294be22d9923d6428b4efb2706619b1eac70cba1347",
    ),
    (
        "http_post_none",
        "b8a5a01d90601439872f312945b86d8215af6708c071483aa61db84e95899c8e",
    ),
    (
        "http_put_api_key_header",
        "4d0857ab54c3bd553ff29d8ae83608a0295483dd077533918076959c11b41c23",
    ),
    (
        "http_patch_bearer",
        "7616063ee5537cd4e975ee6e10f2f254d16dc3574d5957e1f32e2a4b698ce80b",
    ),
    (
        "http_delete_managed_oauth",
        "dbd2f4863a93b923a6789fc321a8f2e727685ebae8a3e5724ab9bd5f319443a9",
    ),
    (
        "built_in_managed",
        "512722864d4a05cb13a3cfaf335448391d1cc774c72d46d4bc495e7cc970f257",
    ),
];

#[test]
fn connector_definition_versions_round_trip_with_distinct_canonical_hashes() {
    // Pins: every auth, HTTP method, and transport binding has one stable
    // discriminator-aware canonical wire shape.
    let documents = [
        ("legacy", legacy_document()),
        (
            "http_get_bearer",
            runtime_http_document_with(
                "GET",
                json!([{"type": "bearer", "slot": "primary"}]),
                Some("primary"),
            ),
        ),
        (
            "http_post_none",
            runtime_http_document_with("POST", json!([{"type": "none"}]), None),
        ),
        (
            "http_put_api_key_header",
            runtime_http_document_with(
                "PUT",
                json!([{
                    "type": "api_key_header",
                    "slot": "api_key",
                    "header": "x-acme-key"
                }]),
                Some("api_key"),
            ),
        ),
        (
            "http_patch_bearer",
            runtime_http_document_with(
                "PATCH",
                json!([{"type": "bearer", "slot": "secondary"}]),
                Some("secondary"),
            ),
        ),
        (
            "http_delete_managed_oauth",
            runtime_http_document_with(
                "DELETE",
                json!([{"type": "managed_oauth", "slot": "oauth"}]),
                Some("oauth"),
            ),
        ),
        ("built_in_managed", runtime_managed_document()),
    ];
    let mut hashes = HashSet::new();

    for ((name, document), (reviewed_name, reviewed_hash)) in documents
        .into_iter()
        .zip(REVIEWED_CONNECTOR_CANONICAL_HASHES)
    {
        assert_eq!(
            name, reviewed_name,
            "reviewed canonical hash mapping drifted"
        );
        let encoded = document
            .to_json()
            .expect("connector definition should serialize as JSON");
        let reparsed = ArtifactDocument::from_json(&encoded)
            .expect("serialized connector definition should parse");
        assert_eq!(reparsed, document);
        let hash = canonical_hash(&document).expect("connector should hash");
        assert_eq!(
            canonical_hash(&reparsed).expect("reparsed connector should hash"),
            hash
        );
        assert!(
            validate_for_status(&document, ArtifactStatus::Draft).is_ok(),
            "valid connector fixture {name} should pass semantic validation"
        );
        assert!(
            hashes.insert(hash),
            "connector fixture {name} must have distinct canonical bytes"
        );
        assert_eq!(
            canonical_hash_hex(hash),
            reviewed_hash,
            "reviewed canonical hash changed for connector fixture {name}"
        );
    }

    assert_eq!(
        hashes.len(),
        7,
        "each definition variant needs distinct canonical bytes"
    );
}

#[test]
fn legacy_connector_exports_the_exact_pre_version_body_shape() {
    // Pins: introducing runtime V1 does not rewrite existing legacy connector exports.
    let document = legacy_document();
    let ArtifactDefinition::Connector(definition) = &document.definition else {
        panic!("fixture should be a connector definition");
    };

    assert!(definition.legacy().is_some());
    assert!(definition.runtime_v1().is_none());
    assert!(!definition.is_connection_installable());
    assert_eq!(
        serde_json::to_value(definition).expect("legacy connector should serialize"),
        json!({
            "auth": {},
            "actions": [{
                "id": "ping",
                "description": "Ping the provider.",
                "input_schema": {"type": "object"},
                "output_schema": {"type": "object"},
                "admin_review_required": false,
                "ui": {}
            }],
            "ui": {}
        })
    );
    assert_eq!(
        serde_json::to_vec(definition).expect("legacy connector body should serialize"),
        br#"{"auth":{},"actions":[{"id":"ping","description":"Ping the provider.","input_schema":{"type":"object"},"output_schema":{"type":"object"},"admin_review_required":false,"ui":{}}],"ui":{}}"#,
        "legacy connector body bytes are a compatibility contract"
    );
    assert_eq!(
        definition
            .actions()
            .map(ConnectorActionRef::id)
            .collect::<Vec<_>>(),
        vec!["ping"]
    );
}

#[test]
fn runtime_discriminator_and_identity_fields_fail_closed() {
    // Pins: runtime-only fields cannot be ignored into a permissive legacy connector.
    let valid = runtime_http_spec();
    let mut cases = Vec::new();

    let mut missing = valid.clone();
    missing
        .as_object_mut()
        .expect("fixture spec should be an object")
        .remove("definition_version");
    cases.push(missing);

    let mut unknown = valid.clone();
    unknown["definition_version"] = json!("v2");
    cases.push(unknown);

    let mut malformed = valid.clone();
    malformed["definition_version"] = json!(1);
    cases.push(malformed);

    for field in [
        "name",
        "version",
        "artifact_uid",
        "revision_uid",
        "base_url",
    ] {
        let mut duplicated = valid.clone();
        duplicated[field] = json!("must-not-live-in-spec");
        cases.push(duplicated);
    }

    for spec in cases {
        assert!(
            connector_document_from_spec(spec).is_err(),
            "invalid discriminator or duplicated identity must fail during decoding"
        );
    }

    let runtime_only_legacy = json!({
        "auth": {},
        "actions": [],
        "runtime": {"type": "constrained_http"}
    });
    assert!(connector_document_from_spec(runtime_only_legacy).is_err());
}

#[test]
fn raw_connector_json_rejects_duplicate_legacy_fields_before_collapse() {
    // Pins: repeated legacy fields are rejected rather than silently choosing
    // the first or last value before legacy/runtime version dispatch.
    let cases = [
        (
            "auth",
            r#"{"auth":{},"auth":{"mode":"other"},"actions":[],"ui":{}}"#,
        ),
        (
            "actions",
            r#"{"auth":{},"actions":[],"actions":[{"id":"late"}],"ui":{}}"#,
        ),
        ("ui", r#"{"auth":{},"actions":[],"ui":{},"ui":{"x":1}}"#),
    ];
    for (field, spec) in cases {
        assert_raw_duplicate_rejected(spec, field);
    }
}

#[test]
fn raw_connector_json_rejects_duplicate_discriminators_in_every_order() {
    // Pins: duplicate version fields fail as duplicates before an unsupported,
    // malformed, or later v1 value can influence dispatch.
    let cases = [
        r#"{"definition_version":"v1","definition_version":"v1"}"#,
        r#"{"definition_version":"v2","definition_version":"v1"}"#,
        r#"{"definition_version":"v1","definition_version":"v2"}"#,
        r#"{"definition_version":1,"definition_version":"v1"}"#,
        r#"{"definition_version":"v1","definition_version":null}"#,
    ];
    for spec in cases {
        assert_raw_duplicate_rejected(spec, "definition_version");
    }
}

#[test]
fn raw_connector_values_reject_nested_json_and_yaml_duplicates() {
    // Pins: duplicate rejection applies recursively inside auth and operation
    // contracts and equally to the JSON and YAML import paths.
    assert_raw_duplicate_rejected(
        r#"{
            "definition_version":"v1",
            "auth":[{"type":"bearer","slot":"primary","slot":"secondary"}]
        }"#,
        "slot",
    );

    let yaml = r#"
api_version: moa.artifact/v1
kind: connector
metadata:
  name: duplicate-yaml
definition:
  type: connector
  spec:
    auth: {}
    auth: {}
    actions: []
    ui: {}
"#;
    let error = ArtifactDocument::from_yaml(yaml)
        .expect_err("duplicate YAML connector field must fail before map collapse")
        .to_string();
    assert!(
        error.contains("duplicate connector definition field `auth`"),
        "unexpected duplicate YAML error: {error}"
    );
}

#[test]
fn runtime_http_validation_rejects_escaping_paths_and_inexact_placeholders() {
    // Pins: model inputs may fill declared segments but can never select an origin or URL shape.
    let cases = [
        "https://attacker.example/invoices/{invoice_id}",
        "//attacker.example/invoices/{invoice_id}",
        "/invoices/../secrets",
        "/invoices/%2e%2e/secrets",
        "/invoices/%252e%252e/secrets",
        "/invoices/{invoice_id}?admin=true",
        "/invoices/prefix-{invoice_id}",
    ];
    for path in cases {
        let mut spec = runtime_http_spec();
        spec["actions"][0]["binding"]["contract"]["path_template"] = json!(path);
        let document = connector_document_from_spec(spec)
            .expect("unsafe path is semantic rather than syntactic invalidity");
        assert_error_path(
            &validate_for_status(&document, ArtifactStatus::Draft).errors,
            "definition.spec.actions[0].binding.contract.path_template",
        );
    }

    let mut missing_mapping = runtime_http_spec();
    missing_mapping["actions"][0]["binding"]["contract"]["path_inputs"] = json!([]);
    let document = connector_document_from_spec(missing_mapping).expect("fixture should decode");
    assert_error_path(
        &validate_for_status(&document, ArtifactStatus::Draft).errors,
        "definition.spec.actions[0].binding.contract.path_inputs",
    );
}

#[test]
fn api_key_headers_reject_transport_and_security_overrides() {
    // Pins: a connector definition cannot turn credential injection into header smuggling.
    let rejected = [
        "Authorization",
        "Host",
        "Content-Length",
        "Content-Type",
        "Cookie",
        "Connection",
        "Proxy-Authorization",
        "Forwarded",
        "X-Forwarded-Host",
        "X-HTTP-Method-Override",
        "X-Original-URL",
        "X-Rewrite-URL",
        "Sec-Fetch-Site",
    ];
    for header in rejected {
        assert!(
            header.parse::<ApiKeyHeaderName>().is_err(),
            "reserved header should fail: {header}"
        );
    }

    let accepted = "X-Acme-Key"
        .parse::<ApiKeyHeaderName>()
        .expect("ordinary vendor API-key header should validate");
    assert_eq!(accepted.as_str(), "x-acme-key");
    assert_eq!(
        serde_json::to_value(accepted).expect("header should serialize"),
        json!("x-acme-key")
    );
}

#[test]
fn upstream_idempotency_headers_are_reviewed_and_cannot_collide_with_auth() {
    // Pins: only a fixed safe header on an idempotent contract may carry the
    // durable call ID, and it cannot overwrite the selected auth material.
    let accepted = "Idempotency-Key"
        .parse::<UpstreamIdempotencyHeaderName>()
        .expect("ordinary idempotency header should validate");
    assert_eq!(accepted.as_str(), "idempotency-key");
    for rejected in ["Authorization", "Content-Length", "X-Forwarded-For"] {
        assert!(
            rejected.parse::<UpstreamIdempotencyHeaderName>().is_err(),
            "reserved idempotency header should fail: {rejected}"
        );
    }

    let mut valid = runtime_http_spec();
    valid["actions"][0]["binding"]["contract"]["upstream_idempotency_header"] =
        json!("Idempotency-Key");
    let valid_document = connector_document_from_spec(valid).expect("valid header should decode");
    assert!(
        validate_for_status(&valid_document, ArtifactStatus::Draft).is_ok(),
        "fixed safe header on an idempotent operation should validate"
    );
    let encoded = valid_document
        .to_json()
        .expect("idempotency contract should serialize");
    let encoded: Value =
        serde_json::from_str(&encoded).expect("serialized connector should be valid JSON");
    assert_eq!(
        encoded["definition"]["spec"]["actions"][0]["binding"]["contract"]["upstream_idempotency_header"],
        json!("idempotency-key"),
        "header should serialize canonically",
    );

    let mut non_idempotent = runtime_http_spec();
    non_idempotent["actions"][0]["binding"]["contract"]["upstream_idempotency_header"] =
        json!("Idempotency-Key");
    non_idempotent["actions"][0]["binding"]["contract"]["policy"]["idempotency"] =
        json!("non_idempotent");
    assert_validation_error(
        &non_idempotent,
        "definition.spec.actions[0].binding.contract.upstream_idempotency_header",
    );

    let mut auth_collision = runtime_http_spec();
    auth_collision["auth"] = json!([{
        "type": "api_key_header",
        "slot": "primary",
        "header": "X-Request-Key"
    }]);
    auth_collision["actions"][0]["binding"]["contract"]["upstream_idempotency_header"] =
        json!("x-request-key");
    assert_validation_error(
        &auth_collision,
        "definition.spec.actions[0].binding.contract.upstream_idempotency_header",
    );
}

#[test]
fn runtime_connector_auth_slots_are_explicit_unique_and_selected() {
    // Pins: an operation cannot guess, omit, or ambiguously select credential material.
    let mut empty = runtime_http_spec();
    empty["auth"] = json!([]);
    assert_validation_error(&empty, "definition.spec.auth");

    let mut none_plus_slot = runtime_http_spec();
    none_plus_slot["auth"] = json!([
        {"type": "none"},
        {"type": "bearer", "slot": "primary"}
    ]);
    assert_validation_error(&none_plus_slot, "definition.spec.auth");

    let mut duplicate = runtime_http_spec();
    duplicate["auth"] = json!([
        {"type": "bearer", "slot": "primary"},
        {"type": "managed_oauth", "slot": "primary"}
    ]);
    assert_validation_error(&duplicate, "definition.spec.auth[1].slot");

    let mut missing = runtime_http_spec();
    missing["actions"][0]["binding"]["contract"]
        .as_object_mut()
        .expect("contract should be an object")
        .remove("credential_slot");
    assert_validation_error(
        &missing,
        "definition.spec.actions[0].binding.contract.credential_slot",
    );

    let mut unknown = runtime_http_spec();
    unknown["actions"][0]["binding"]["contract"]["credential_slot"] = json!("secondary");
    assert_validation_error(
        &unknown,
        "definition.spec.actions[0].binding.contract.credential_slot",
    );
}

#[test]
fn runtime_schemas_enforce_reference_size_depth_and_property_bounds() {
    // Pins: custom definitions cannot trigger external resolution or unbounded schema work.
    let mut referenced = runtime_http_spec();
    set_input_schema(
        &mut referenced,
        json!({"type": "object", "$ref": "https://example/x"}),
    );
    assert_validation_error(
        &referenced,
        "definition.spec.actions[0].binding.contract.policy.input_schema",
    );

    let mut unconstrained = runtime_http_spec();
    set_input_schema(&mut unconstrained, json!({}));
    assert_validation_error(
        &unconstrained,
        "definition.spec.actions[0].binding.contract.policy.input_schema",
    );

    let mut sixteen = json!({"type": "object"});
    for _ in 1..16 {
        sixteen = json!({"type": "object", "properties": {"next": sixteen}});
    }
    let mut max_depth = runtime_http_spec();
    set_input_schema(&mut max_depth, sixteen);
    let max_document = connector_document_from_spec(max_depth).expect("fixture should decode");
    assert!(
        validate_for_status(&max_document, ArtifactStatus::Draft).is_ok(),
        "exactly 16 schema nodes must remain valid"
    );

    let mut seventeen = json!({"type": "object"});
    for _ in 1..17 {
        seventeen = json!({"type": "object", "properties": {"next": seventeen}});
    }
    let mut too_deep = runtime_http_spec();
    set_input_schema(&mut too_deep, seventeen);
    assert_validation_error(
        &too_deep,
        "definition.spec.actions[0].binding.contract.policy.input_schema",
    );

    let boundary_properties = (0..256)
        .map(|index| (format!("p{index}"), json!({"type": "string"})))
        .collect::<serde_json::Map<_, _>>();
    let mut max_properties = runtime_http_spec();
    set_input_schema(
        &mut max_properties,
        Value::Object(serde_json::Map::from_iter([
            ("type".to_string(), json!("object")),
            ("properties".to_string(), Value::Object(boundary_properties)),
        ])),
    );
    let max_properties_document =
        connector_document_from_spec(max_properties).expect("fixture should decode");
    assert!(
        validate_for_status(&max_properties_document, ArtifactStatus::Draft).is_ok(),
        "exactly 256 declared properties must remain valid"
    );

    let properties = (0..257)
        .map(|index| (format!("p{index}"), json!({"type": "string"})))
        .collect::<serde_json::Map<_, _>>();
    let mut too_many_properties = runtime_http_spec();
    set_input_schema(
        &mut too_many_properties,
        Value::Object(serde_json::Map::from_iter([
            ("type".to_string(), json!("object")),
            ("properties".to_string(), Value::Object(properties)),
        ])),
    );
    assert_validation_error(
        &too_many_properties,
        "definition.spec.actions[0].binding.contract.policy.input_schema",
    );

    let mut too_large = runtime_http_spec();
    set_input_schema(
        &mut too_large,
        json!({"type": "object", "description": "x".repeat(65_536)}),
    );
    assert_validation_error(
        &too_large,
        "definition.spec.actions[0].binding.contract.policy.input_schema",
    );
}

#[test]
fn runtime_schemas_bound_content_schema_depth_and_total_properties() {
    // Pins: Draft 2020-12 contentSchema children consume the same depth and
    // aggregate property budgets as root and other child-schema keywords.
    let mut sixteen = json!({"type": "object"});
    for _ in 1..16 {
        sixteen = json!({"type": "object", "contentSchema": sixteen});
    }
    let mut max_depth = runtime_http_spec();
    set_input_schema(&mut max_depth, sixteen);
    let max_depth_document =
        connector_document_from_spec(max_depth).expect("contentSchema fixture should decode");
    assert!(
        validate_for_status(&max_depth_document, ArtifactStatus::Draft).is_ok(),
        "exactly 16 contentSchema nodes must remain valid"
    );

    let mut seventeen = json!({"type": "object"});
    for _ in 1..17 {
        seventeen = json!({"type": "object", "contentSchema": seventeen});
    }
    let mut too_deep = runtime_http_spec();
    set_input_schema(&mut too_deep, seventeen);
    assert_validation_error(
        &too_deep,
        "definition.spec.actions[0].binding.contract.policy.input_schema",
    );

    let boundary_properties = (0..256)
        .map(|index| (format!("content_p{index}"), json!({"type": "string"})))
        .collect::<serde_json::Map<_, _>>();
    let mut max_properties = runtime_http_spec();
    set_input_schema(
        &mut max_properties,
        json!({
            "type": "object",
            "contentSchema": {
                "type": "object",
                "properties": boundary_properties
            }
        }),
    );
    let max_properties_document = connector_document_from_spec(max_properties)
        .expect("contentSchema property fixture should decode");
    assert!(
        validate_for_status(&max_properties_document, ArtifactStatus::Draft).is_ok(),
        "exactly 256 properties beneath contentSchema must remain valid"
    );

    let excessive_properties = (0..257)
        .map(|index| (format!("content_p{index}"), json!({"type": "string"})))
        .collect::<serde_json::Map<_, _>>();
    let mut too_many_properties = runtime_http_spec();
    set_input_schema(
        &mut too_many_properties,
        json!({
            "type": "object",
            "contentSchema": {
                "type": "object",
                "properties": excessive_properties
            }
        }),
    );
    assert_validation_error(
        &too_many_properties,
        "definition.spec.actions[0].binding.contract.policy.input_schema",
    );
}

#[test]
fn runtime_schemas_count_boolean_children_across_keyword_groups() {
    // Pins: boolean schemas consume one depth level through singular, array,
    // and schema-map children, including Draft 2020-12 contentSchema.
    type SchemaWrapper = fn(Value) -> Value;
    let groups: [(&str, SchemaWrapper); 3] = [
        ("contentSchema", |child| json!({"contentSchema": child})),
        ("allOf", |child| json!({"allOf": [child]})),
        (
            "dependentSchemas",
            |child| json!({"dependentSchemas": {"trigger": child}}),
        ),
    ];

    for (keyword, wrap_child) in groups {
        let mut max_depth = runtime_http_spec();
        set_input_schema(
            &mut max_depth,
            mixed_boolean_schema_at_depth(16, wrap_child),
        );
        let max_depth_document = connector_document_from_spec(max_depth)
            .expect("mixed object/boolean schema fixture should decode");
        assert!(
            validate_for_status(&max_depth_document, ArtifactStatus::Draft).is_ok(),
            "exactly 16 schema nodes through {keyword} must remain valid"
        );

        let mut too_deep = runtime_http_spec();
        set_input_schema(&mut too_deep, mixed_boolean_schema_at_depth(17, wrap_child));
        assert_validation_error(
            &too_deep,
            "definition.spec.actions[0].binding.contract.policy.input_schema",
        );
    }
}

#[test]
fn runtime_schemas_reject_invalid_child_schema_shapes() {
    // Pins: schema-bearing keywords fail closed when a child is neither an
    // object schema nor a boolean schema.
    let invalid_schemas = [
        json!({"type": "object", "contentSchema": "invalid"}),
        json!({"type": "object", "contentSchema": []}),
        json!({"type": "object", "allOf": [42]}),
        json!({"type": "object", "dependentSchemas": {"trigger": []}}),
    ];

    for schema in invalid_schemas {
        let mut spec = runtime_http_spec();
        set_input_schema(&mut spec, schema);
        assert_validation_error(
            &spec,
            "definition.spec.actions[0].binding.contract.policy.input_schema",
        );
    }
}

#[test]
fn runtime_http_limits_enforce_exact_request_response_and_timeout_bounds() {
    // Pins: tenant definitions cannot remove transport byte or deadline ceilings.
    let cases = [
        ("max_request_bytes", json!(0)),
        ("max_request_bytes", json!(1_048_577)),
        ("max_response_bytes", json!(0)),
        ("max_response_bytes", json!(10_485_761)),
        ("connect_timeout_ms", json!(99)),
        ("connect_timeout_ms", json!(10_001)),
        ("total_timeout_ms", json!(99)),
        ("total_timeout_ms", json!(60_001)),
    ];
    for (field, value) in cases {
        let mut spec = runtime_http_spec();
        spec["actions"][0]["binding"]["contract"][field] = value;
        assert_validation_error(
            &spec,
            &format!("definition.spec.actions[0].binding.contract.{field}"),
        );
    }

    let mut inverted = runtime_http_spec();
    inverted["actions"][0]["binding"]["contract"]["connect_timeout_ms"] = json!(1_000);
    inverted["actions"][0]["binding"]["contract"]["total_timeout_ms"] = json!(500);
    assert_validation_error(
        &inverted,
        "definition.spec.actions[0].binding.contract.total_timeout_ms",
    );
}

#[test]
fn runtime_actions_enforce_kind_identity_bounds_and_authoritative_policy_floor() {
    // Pins: tenant-authored runtime actions cannot claim trusted transport or policy provenance.
    let mut duplicate = runtime_http_spec();
    let duplicate_action = duplicate["actions"][0].clone();
    duplicate["actions"]
        .as_array_mut()
        .expect("actions should be an array")
        .push(duplicate_action);
    assert_validation_error(&duplicate, "definition.spec.actions[1].id");

    for action_id in ["1invalid", "way_too_long_connector_action"] {
        let mut invalid_id = runtime_http_spec();
        invalid_id["actions"][0]["id"] = json!(action_id);
        assert_validation_error(&invalid_id, "definition.spec.actions[0].id");
    }

    let mut mismatched = runtime_http_spec();
    mismatched["runtime"] = json!({"type": "built_in_managed", "provider": "crm/v1"});
    assert_validation_error(&mismatched, "definition.spec.actions[0].binding");

    for (field, value) in [
        ("action_class", json!("read")),
        ("risk_level", json!("low")),
        ("minimum_effect", json!("allow")),
    ] {
        let mut weakened = runtime_http_spec();
        weakened["actions"][0]["binding"]["contract"]["policy"][field] = value;
        assert_validation_error(
            &weakened,
            &format!("definition.spec.actions[0].binding.contract.policy.{field}"),
        );
    }

    let mut weakened_managed = runtime_managed_spec();
    weakened_managed["actions"][0]["binding"]["contract"]["minimum_effect"] = json!("allow");
    assert_validation_error(
        &weakened_managed,
        "definition.spec.actions[0].binding.contract.minimum_effect",
    );
}

#[test]
fn connection_qualified_tool_reference_is_deterministic_and_bounded() {
    // Pins: installed actions are connection-qualified and never collide by logical name alone.
    let connection_id = ConnectorConnectionId(
        Uuid::parse_str("018f8f1f-36a6-7c90-a7f8-2f2f57f5c111")
            .expect("fixture connection UUID should parse"),
    );
    let boundary_action = "Action_12345678901234567";
    assert_eq!(boundary_action.len(), 24);

    let reference = connection_action_tool_reference(connection_id, boundary_action)
        .expect("boundary action id should produce a reference");
    assert_eq!(
        reference,
        "conn__018f8f1f36a67c90a7f82f2f57f5c111__Action_12345678901234567"
    );
    assert_eq!(reference.len(), 64);
    assert!(connection_action_tool_reference(connection_id, "1bad").is_err());
    assert!(connection_action_tool_reference(connection_id, &"A".repeat(25)).is_err());
}

#[test]
fn runtime_connector_dto_cannot_carry_credential_material() {
    // Pins: credential plaintext has no serializable field in definition or operation DTOs.
    let mut spec = runtime_http_spec();
    spec["auth"][0]["secret"] = json!("must-not-decode");
    assert!(connector_document_from_spec(spec).is_err());

    let encoded = runtime_http_document()
        .to_json()
        .expect("runtime connector should serialize");
    for forbidden in ["secret", "token", "credential_value", "base_url"] {
        assert!(
            !encoded.contains(forbidden),
            "serialized runtime connector must omit {forbidden}"
        );
    }
}

#[test]
fn action_only_runtime_connector_preserves_exact_canonical_body_bytes() {
    // Pins: adding the optional source contract never serializes a null/default
    // field or changes the already-reviewed action-only artifact hash.
    let document = runtime_http_document();
    let ArtifactDefinition::Connector(definition) = &document.definition else {
        panic!("fixture should be a connector definition");
    };
    let encoded = serde_json::to_value(definition)
        .expect("action-only runtime definition should serialize canonically");

    assert_eq!(
        encoded,
        runtime_http_spec(),
        "the absent source field must not change action-only canonical bytes"
    );
    assert_eq!(
        canonical_hash_hex(canonical_hash(&document).expect("action-only connector should hash")),
        REVIEWED_CONNECTOR_CANONICAL_HASHES[1].1,
    );
}

fn legacy_document() -> ArtifactDocument {
    connector_document_from_spec(json!({
        "auth": {},
        "actions": [{
            "id": "ping",
            "description": "Ping the provider.",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"}
        }],
        "ui": {}
    }))
    .expect("legacy connector fixture should decode")
}

fn runtime_http_document() -> ArtifactDocument {
    connector_document_from_spec(runtime_http_spec()).expect("HTTP connector fixture should decode")
}

fn runtime_http_document_with(
    method: &str,
    auth: Value,
    credential_slot: Option<&str>,
) -> ArtifactDocument {
    let mut spec = runtime_http_spec();
    spec["auth"] = auth;
    spec["actions"][0]["binding"]["contract"]["method"] = json!(method);
    let contract = spec["actions"][0]["binding"]["contract"]
        .as_object_mut()
        .expect("HTTP contract fixture should be an object");
    match credential_slot {
        Some(slot) => {
            contract.insert("credential_slot".to_string(), json!(slot));
        }
        None => {
            contract.remove("credential_slot");
        }
    }
    connector_document_from_spec(spec).expect("HTTP connector matrix fixture should decode")
}

fn runtime_managed_document() -> ArtifactDocument {
    connector_document_from_spec(runtime_managed_spec())
        .expect("managed connector fixture should decode")
}

fn runtime_managed_spec() -> Value {
    let mut spec = runtime_http_spec();
    spec["display_name"] = json!("Managed CRM");
    spec["runtime"] = json!({"type": "built_in_managed", "provider": "crm/v1"});
    spec["auth"] = json!([{"type": "managed_oauth", "slot": "primary"}]);
    spec["actions"][0]["binding"] = json!({
        "type": "built_in_managed",
        "operation": "contacts.upsert",
        "contract": governed_policy()
    });
    spec
}

fn runtime_http_spec() -> Value {
    json!({
        "definition_version": "v1",
        "display_name": "Invoice API",
        "description": "Write invoice state through one tenant installation.",
        "runtime": {"type": "constrained_http"},
        "auth": [{"type": "bearer", "slot": "primary"}],
        "actions": [{
            "id": "GetInvoice",
            "description": "Fetch one invoice through a reviewed external action.",
            "binding": {
                "type": "http",
                "contract": {
                    "method": "GET",
                    "path_template": "/invoices/{invoice_id}",
                    "path_inputs": [{
                        "placeholder": "invoice_id",
                        "input_pointer": "/invoice_id"
                    }],
                    "query_inputs": [{
                        "parameter": "include_lines",
                        "input_pointer": "/include_lines"
                    }],
                    "credential_slot": "primary",
                    "response_pointer": "/data",
                    "max_request_bytes": 1024,
                    "max_response_bytes": 16384,
                    "connect_timeout_ms": 1000,
                    "total_timeout_ms": 5000,
                    "policy": governed_policy()
                }
            }
        }]
    })
}

fn governed_policy() -> Value {
    json!({
        "input_schema": {
            "type": "object",
            "properties": {
                "invoice_id": {"type": "string"},
                "include_lines": {"type": "boolean"}
            }
        },
        "output_schema": {"type": "object"},
        "data_classes": ["pii"],
        "action_class": "external_write",
        "risk_level": "high",
        "minimum_effect": "admin_review",
        "idempotency": "idempotent"
    })
}

fn set_input_schema(spec: &mut Value, schema: Value) {
    spec["actions"][0]["binding"]["contract"]["policy"]["input_schema"] = schema;
}

fn mixed_boolean_schema_at_depth(depth: usize, wrap_child: fn(Value) -> Value) -> Value {
    assert!(depth > 1, "fixture depth must include an object root");
    let mut schema = Value::Bool(true);
    for _ in 1..depth {
        schema = wrap_child(schema);
    }
    schema["type"] = json!("object");
    schema
}

fn connector_document_from_spec(spec: Value) -> Result<ArtifactDocument, serde_json::Error> {
    serde_json::from_value(json!({
        "api_version": "moa.artifact/v1",
        "kind": "connector",
        "metadata": {"name": "invoice-api"},
        "definition": {"type": "connector", "spec": spec}
    }))
}

fn assert_raw_duplicate_rejected(spec: &str, field: &str) {
    let raw = format!(
        r#"{{
            "api_version":"moa.artifact/v1",
            "kind":"connector",
            "metadata":{{"name":"duplicate-fixture"}},
            "definition":{{"type":"connector","spec":{spec}}}
        }}"#
    );
    let error = ArtifactDocument::from_json(&raw)
        .expect_err("duplicate connector field must fail before map collapse")
        .to_string();
    assert!(
        error.contains(&format!("duplicate connector definition field `{field}`")),
        "unexpected duplicate-field error: {error}"
    );
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

fn canonical_hash_hex(hash: [u8; 32]) -> String {
    hash.into_iter().map(|byte| format!("{byte:02x}")).collect()
}
