//! Canonical installed constrained-HTTP operation contract behavior.

use moa_artifacts::connector::{
    ConnectorDefinition, HttpMethod, HttpOperationContract, RuntimeConnectorAction,
    RuntimeConnectorAuthRequirement, RuntimeOperationPolicy,
};
use moa_connectors::Error;
use moa_connectors::domain::{
    CompiledOperationContract, ConnectionGeneration, InstalledActionBinding,
    InstalledActionBindingId, OperationContractHash,
};
use moa_core::types::action_policy::ActionPolicyEffect;
use moa_core::types::credentials::CredentialSlotName;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use moa_core::types::security::SensitivityClass;
use moa_core::types::tools::IdempotencyClass;
use serde_json::{Map, Value, json};
use uuid::Uuid;

fn action(input_schema: Value) -> RuntimeConnectorAction {
    RuntimeConnectorAction {
        id: "create_invoice".to_string(),
        description: "Create one invoice".to_string(),
        contract: HttpOperationContract {
            method: HttpMethod::Post,
            path_template: "/invoices".to_string(),
            path_inputs: Vec::new(),
            query_inputs: Vec::new(),
            body_input: None,
            credential_slot: Some(CredentialSlotName::PRIMARY),
            upstream_idempotency_header: None,
            response_pointer: None,
            max_request_bytes: 1024,
            max_response_bytes: 1024,
            connect_timeout_ms: 1_000,
            total_timeout_ms: 2_000,
            policy: RuntimeOperationPolicy {
                input_schema,
                output_schema: json!({"type": "object"}),
                data_classes: vec![SensitivityClass::Pii],
                idempotency: IdempotencyClass::NonIdempotent,
            },
        },
    }
}

fn definition() -> ConnectorDefinition {
    ConnectorDefinition {
        display_name: "Billing".to_string(),
        description: String::new(),
        auth: vec![RuntimeConnectorAuthRequirement::Bearer {
            slot: CredentialSlotName::PRIMARY,
        }],
        actions: Vec::new(),
    }
}

fn same_schema_with_property_order(property_names: [&str; 2]) -> Value {
    let mut properties = Map::new();
    for name in property_names {
        properties.insert(name.to_string(), json!({"type": "string"}));
    }
    let mut schema = Map::new();
    schema.insert("type".to_string(), Value::String("object".to_string()));
    schema.insert("properties".to_string(), Value::Object(properties));
    Value::Object(schema)
}

#[test]
fn compiled_operation_hash_ignores_json_map_insertion_order_offline() {
    // Pins: activation persists one deterministic contract hash independent of
    // serde_json map construction order or process scheduling.
    let definition = definition();
    let left = CompiledOperationContract::compile(
        &definition,
        &action(same_schema_with_property_order(["customer", "amount"])),
    )
    .expect("valid HTTP action should compile");
    let right = CompiledOperationContract::compile(
        &definition,
        &action(same_schema_with_property_order(["amount", "customer"])),
    )
    .expect("equivalent HTTP action should compile");

    assert_eq!(
        left.canonical_bytes()
            .expect("contract should canonicalize"),
        right
            .canonical_bytes()
            .expect("contract should canonicalize")
    );
    assert_eq!(
        left.hash().expect("contract should hash"),
        right.hash().expect("contract should hash")
    );
}

#[test]
fn installed_binding_rejects_identity_hash_and_policy_drift_offline() {
    // Pins: a binding cannot weaken the platform-fixed admin-review floor or
    // drift from the immutable compiled HTTP payload.
    let compiled =
        CompiledOperationContract::compile(&definition(), &action(json!({"type": "object"})))
            .expect("valid HTTP action should compile");
    let correct_hash = compiled.hash().expect("compiled contract should hash");
    let mut binding = InstalledActionBinding {
        binding_id: InstalledActionBindingId(Uuid::from_u128(11)),
        tenant_id: TenantId(Uuid::from_u128(12)),
        connection_id: ConnectorConnectionId(Uuid::from_u128(13)),
        connection_generation: ConnectionGeneration::new(3).expect("positive generation"),
        action_id: "create_invoice".to_string(),
        compiled_contract: compiled,
        contract_hash: correct_hash,
        governed_contract_revision: "connector-action:v1:billing:create_invoice".to_string(),
        minimum_effect: ActionPolicyEffect::AdminReview,
        enabled: true,
    };
    binding
        .validate()
        .expect("matching binding should validate");

    binding.contract_hash = OperationContractHash::from_bytes([7; 32]);
    assert!(matches!(
        binding.validate(),
        Err(Error::ContractHashMismatch { .. })
    ));
    binding.contract_hash = correct_hash;
    binding.minimum_effect = ActionPolicyEffect::Allow;
    assert!(matches!(
        binding.validate(),
        Err(Error::InvalidContract { .. })
    ));
}

#[test]
fn compiled_http_contract_requires_its_selected_credential_slot_offline() {
    // Pins: activation cannot persist a selected credential slot that was not declared.
    let action = action(json!({"type": "object"}));
    let mut definition = definition();
    definition.auth = vec![RuntimeConnectorAuthRequirement::None];
    assert!(matches!(
        CompiledOperationContract::compile(&definition, &action),
        Err(Error::CredentialSlotMissing { ref slot }) if slot == &CredentialSlotName::PRIMARY
    ));

    definition.auth = vec![RuntimeConnectorAuthRequirement::Bearer {
        slot: CredentialSlotName::PRIMARY,
    }];
    CompiledOperationContract::compile(&definition, &action)
        .expect("declared HTTP credential slot should compile");
}
