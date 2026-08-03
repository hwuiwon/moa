//! Postgres replay-ledger coverage for connector action invocations.

use moa_artifacts::connector::ConnectorDefinition;
use moa_connectors::Error;
use moa_connectors::domain::{
    CompiledOperationContract, ConnectionDefinitionRef, ConnectionGeneration, ConnectionStatus,
    ConnectorInvocationId, ConnectorInvocationState, ConnectorInvocationTerminal,
    InstalledActionBinding, InstalledActionBindingId, OperationContractHash,
};
use moa_connectors::repository::{
    ConnectionActivation, ConnectionLifecycleRepository, ConnectorInvocationRepository,
    InvocationReservation, InvocationReservationRequest, NewConnectorConnection,
    PostgresConnectionRepository,
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn terminal_same_hash_replays_and_changed_pins_or_hash_conflict_db_memory() {
    // Pins: a terminal tool-call replay returns the exact durable result, while
    // any request-hash or installed-binding pin drift fails without redispatch.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector invocation replay database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool);
    let tenant_id = TenantId::new();
    let (connection_id, binding_id) = active_binding(&repository, tenant_id, "primary").await;
    let (other_connection_id, other_binding_id) =
        active_binding(&repository, tenant_id, "secondary").await;
    let invocation_id = ConnectorInvocationId(Uuid::new_v4());
    let request = reservation_request(
        invocation_id,
        tenant_id,
        connection_id,
        binding_id,
        "tool-call-terminal-replay",
        OperationContractHash::from_bytes([1; 32]),
    );

    let reserved = repository
        .reserve_invocation(request.clone())
        .await
        .expect("fresh replay key should reserve");
    let InvocationReservation::Reserved(started) = reserved else {
        panic!("fresh replay key must return Reserved, observed {reserved:?}");
    };
    assert_eq!(started.invocation_id, invocation_id);
    assert_eq!(started.state, ConnectorInvocationState::Reserved);
    assert_eq!(started.connection_id, connection_id);
    assert_eq!(started.binding_id, binding_id);
    assert_eq!(started.connection_generation, generation(2));
    assert_eq!(started.completed_at, None);

    let in_flight = repository
        .reserve_invocation(InvocationReservationRequest {
            invocation_id: ConnectorInvocationId(Uuid::new_v4()),
            ..request.clone()
        })
        .await
        .expect("same started request should not create a second invocation");
    assert!(matches!(
        in_flight,
        InvocationReservation::InFlight(ref record) if record == &started
    ));

    let transmitting = repository
        .mark_transmitting(tenant_id, invocation_id)
        .await
        .expect("reserved invocation should transfer to transport once");
    assert_eq!(transmitting.state, ConnectorInvocationState::Transmitting);
    assert_eq!(transmitting.completed_at, None);

    let terminal = ConnectorInvocationTerminal::Succeeded {
        output_metadata: json!({"remote_id": "invoice-42", "status": "accepted"}),
    };
    let finished = repository
        .finish_invocation(tenant_id, invocation_id, terminal.clone())
        .await
        .expect("started invocation should commit one terminal result");
    assert_eq!(finished.state, ConnectorInvocationState::Succeeded);
    assert_eq!(finished.error_metadata, None);
    assert_eq!(
        finished.output_metadata,
        Some(json!({"remote_id": "invoice-42", "status": "accepted"}))
    );
    assert!(finished.completed_at.is_some());

    let replay = repository
        .reserve_invocation(InvocationReservationRequest {
            invocation_id: ConnectorInvocationId(Uuid::new_v4()),
            ..request.clone()
        })
        .await
        .expect("same terminal request should replay");
    assert!(matches!(
        replay,
        InvocationReservation::Replay(ref record) if record == &finished
    ));
    assert_eq!(
        repository
            .finish_invocation(tenant_id, invocation_id, terminal)
            .await
            .expect("identical terminal completion should be idempotent"),
        finished
    );

    let changed_hash = repository
        .reserve_invocation(InvocationReservationRequest {
            invocation_id: ConnectorInvocationId(Uuid::new_v4()),
            request_hash: OperationContractHash::from_bytes([2; 32]),
            ..request.clone()
        })
        .await
        .expect_err("same tool call with a different canonical request must conflict");
    assert!(matches!(
        changed_hash,
        Error::InvocationConflict { ref tool_call_id }
            if tool_call_id == "tool-call-terminal-replay"
    ));

    let changed_pins = repository
        .reserve_invocation(InvocationReservationRequest {
            invocation_id: ConnectorInvocationId(Uuid::new_v4()),
            connection_id: other_connection_id,
            binding_id: other_binding_id,
            ..request
        })
        .await
        .expect_err("same tool call with a different installed binding must conflict");
    assert!(matches!(
        changed_pins,
        Error::InvocationConflict { ref tool_call_id }
            if tool_call_id == "tool-call-terminal-replay"
    ));
}

#[tokio::test]
async fn post_send_unknown_outcome_is_terminal_and_never_auto_retries_db_memory() {
    // Pins: uncertainty after possible transmission is a sticky terminal replay,
    // not a failed-before-send result that another attempt may transmit again.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector unknown-outcome database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool);
    let tenant_id = TenantId::new();
    let (connection_id, binding_id) = active_binding(&repository, tenant_id, "unknown").await;
    let invocation_id = ConnectorInvocationId(Uuid::new_v4());
    let request = reservation_request(
        invocation_id,
        tenant_id,
        connection_id,
        binding_id,
        "tool-call-unknown-outcome",
        OperationContractHash::from_bytes([9; 32]),
    );
    assert!(matches!(
        repository
            .reserve_invocation(request.clone())
            .await
            .expect("fresh replay key should reserve"),
        InvocationReservation::Reserved(_)
    ));
    let transmitting = repository
        .mark_transmitting(tenant_id, invocation_id)
        .await
        .expect("transport should own the invocation before possible transmission");
    assert_eq!(transmitting.state, ConnectorInvocationState::Transmitting);

    let uncertainty = json!({"class": "connection_lost_after_send", "retry_safe": false});
    let unknown = repository
        .finish_invocation(
            tenant_id,
            invocation_id,
            ConnectorInvocationTerminal::UnknownOutcome {
                error_metadata: uncertainty.clone(),
            },
        )
        .await
        .expect("post-send uncertainty should persist as terminal");
    assert_eq!(unknown.state, ConnectorInvocationState::UnknownOutcome);
    assert_eq!(unknown.error_metadata, Some(uncertainty));
    assert_eq!(unknown.output_metadata, None);
    assert!(unknown.completed_at.is_some());

    let replay = repository
        .reserve_invocation(InvocationReservationRequest {
            invocation_id: ConnectorInvocationId(Uuid::new_v4()),
            ..request
        })
        .await
        .expect("unknown outcome should be returned from the replay ledger");
    assert!(matches!(
        replay,
        InvocationReservation::Replay(ref record) if record == &unknown
    ));

    let downgrade = repository
        .finish_invocation(
            tenant_id,
            invocation_id,
            ConnectorInvocationTerminal::FailedBeforeSend {
                error_metadata: json!({"class": "safe_to_retry"}),
            },
        )
        .await
        .expect_err("unknown post-send outcome must not be reclassified as retry-safe");
    assert!(matches!(
        downgrade,
        Error::InvocationStateConflict {
            from: ConnectorInvocationState::UnknownOutcome,
            to: ConnectorInvocationState::FailedBeforeSend,
            ..
        }
    ));
}

#[tokio::test]
async fn suspension_between_reservation_and_transport_blocks_transmission_db_memory() {
    // Pins: transport ownership atomically rechecks active lifecycle and the
    // current enabled binding, so a suspension after reservation prevents send.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector pre-transmission fence database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool);
    let tenant_id = TenantId::new();
    let (connection_id, binding_id) = active_binding(&repository, tenant_id, "suspend").await;
    let invocation_id = ConnectorInvocationId(Uuid::new_v4());
    let request = reservation_request(
        invocation_id,
        tenant_id,
        connection_id,
        binding_id,
        "tool-call-suspended-before-send",
        OperationContractHash::from_bytes([7; 32]),
    );
    assert!(matches!(
        repository
            .reserve_invocation(request.clone())
            .await
            .expect("active binding should reserve before suspension"),
        InvocationReservation::Reserved(_)
    ));
    repository
        .transition(
            tenant_id,
            connection_id,
            generation(2),
            ConnectionStatus::Suspended,
        )
        .await
        .expect("active connection should suspend before transport ownership");

    let blocked = repository
        .mark_transmitting(tenant_id, invocation_id)
        .await
        .expect_err("suspended connection must not transfer invocation to transport");
    assert!(matches!(
        blocked,
        Error::InvocationStateConflict {
            from: ConnectorInvocationState::Reserved,
            to: ConnectorInvocationState::Transmitting,
            ..
        }
    ));
    assert!(matches!(
        repository
            .reserve_invocation(InvocationReservationRequest {
                invocation_id: ConnectorInvocationId(Uuid::new_v4()),
                ..request
            })
            .await
            .expect("blocked invocation should remain a durable in-flight reservation"),
        InvocationReservation::InFlight(record)
            if record.state == ConnectorInvocationState::Reserved
    ));
}

async fn active_binding(
    repository: &PostgresConnectionRepository,
    tenant_id: TenantId,
    suffix: &str,
) -> (ConnectorConnectionId, InstalledActionBindingId) {
    let connection_id = ConnectorConnectionId::new();
    repository
        .create(NewConnectorConnection {
            connection_id,
            tenant_id,
            display_name: format!("Billing {suffix}"),
            definition_ref: ConnectionDefinitionRef::built_in("knowledge:nango", 1)
                .expect("fixture built-in definition should be valid"),
            origin: None,
            non_secret_config: json!({}),
            created_by_identity_id: None,
            owner_identity_id: Uuid::new_v4(),
        })
        .await
        .expect("connection fixture should be created");
    let definition: ConnectorDefinition = serde_json::from_value(json!({
        "display_name": "Billing replay fixture",
        "auth": [{"type": "none"}],
        "actions": [{
            "id": "invoice_create",
            "description": "Create one invoice.",
            "contract": {
                "method": "POST",
                "path_template": "/invoices",
                "max_request_bytes": 1024,
                "max_response_bytes": 1024,
                "connect_timeout_ms": 1000,
                "total_timeout_ms": 2000,
                "policy": {
                    "input_schema": {"type": "object"},
                    "output_schema": {"type": "object"},
                    "data_classes": [],
                    "idempotency": "idempotent"
                }
            }
        }]
    }))
    .expect("fixture definition should match the runtime connector contract");
    let action = definition
        .actions
        .first()
        .expect("fixture definition should have one action");
    let compiled_contract = CompiledOperationContract::compile(&definition, action)
        .expect("fixture operation contract should compile");
    let contract_hash = compiled_contract
        .hash()
        .expect("fixture operation contract should hash");
    let binding_id = InstalledActionBindingId(Uuid::new_v4());
    repository
        .activate(ConnectionActivation {
            tenant_id,
            connection_id,
            expected_generation: generation(1),
            bindings: vec![InstalledActionBinding {
                binding_id,
                tenant_id,
                connection_id,
                connection_generation: generation(2),
                action_id: action.id.clone(),
                compiled_contract,
                contract_hash,
                governed_contract_revision: format!("billing/{suffix}/invoice.create"),
                minimum_effect: moa_core::types::action_policy::ActionPolicyEffect::AdminReview,
                enabled: true,
            }],
        })
        .await
        .expect("connection fixture should activate");
    (connection_id, binding_id)
}

fn reservation_request(
    invocation_id: ConnectorInvocationId,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    binding_id: InstalledActionBindingId,
    tool_call_id: &str,
    request_hash: OperationContractHash,
) -> InvocationReservationRequest {
    InvocationReservationRequest {
        invocation_id,
        tenant_id,
        connection_id,
        binding_id,
        connection_generation: generation(2),
        tool_call_id: tool_call_id.to_string(),
        request_hash,
        upstream_idempotency_key: Some(format!("idempotency-{tool_call_id}")),
    }
}

fn generation(value: u64) -> ConnectionGeneration {
    ConnectionGeneration::new(value).expect("fixture generation should be positive")
}
