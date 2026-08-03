//! Offline installed connector catalog visibility and integrity coverage.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use moa_artifacts::connector::ConnectorDefinition;
use moa_connectors::catalog::{
    ConnectorUseAuthorizer, GovernedInstalledConnectorCatalog, InstalledConnectorCatalog,
    InstalledConnectorCatalogQuery, InstalledConnectorCatalogSource,
};
use moa_connectors::domain::{
    CompiledOperationContract, ConnectionDefinitionRef, ConnectionGeneration, ConnectionHealth,
    ConnectionStatus, ConnectorConnection, InstalledActionBinding, InstalledActionBindingId,
};
use moa_connectors::executor::{ConnectorActionInvocation, InstalledConnectorActionPin};
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::action_policy::ActionPolicyEffect;
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId, ToolCallId};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
struct InMemoryInstalledConnectorCatalogSource {
    candidates: Vec<(ConnectorConnection, InstalledActionBinding)>,
    read_count: Arc<AtomicUsize>,
}

#[async_trait]
impl InstalledConnectorCatalogSource for InMemoryInstalledConnectorCatalogSource {
    async fn candidates(
        &self,
        tenant_id: TenantId,
        connection_ids: &[ConnectorConnectionId],
    ) -> moa_connectors::Result<Vec<(ConnectorConnection, InstalledActionBinding)>> {
        self.read_count.fetch_add(1, Ordering::SeqCst);
        let selected = connection_ids.iter().copied().collect::<HashSet<_>>();
        Ok(self
            .candidates
            .iter()
            .filter(|(connection, _)| {
                connection.tenant_id == tenant_id && selected.contains(&connection.connection_id)
            })
            .cloned()
            .collect())
    }
}

struct AllowListConnectorUseAuthorizer {
    allowed: HashSet<ConnectorConnectionId>,
}

#[async_trait]
impl ConnectorUseAuthorizer for AllowListConnectorUseAuthorizer {
    async fn require_use_batch(
        &self,
        _caller: &Identity,
        connection_ids: &[ConnectorConnectionId],
    ) -> moa_connectors::Result<()> {
        if connection_ids.iter().all(|id| self.allowed.contains(id)) {
            Ok(())
        } else {
            Err(moa_connectors::Error::CatalogInvariant {
                message: "fixture delegated connector use denied".to_string(),
            })
        }
    }
}

#[tokio::test]
async fn catalog_exposes_only_active_selected_bindings_offline() {
    // Pins: lifecycle and delegated connection authorization gate catalog visibility,
    // while an independent ready health observation cannot revive a suspension.
    let tenant_id = TenantId::new();
    let active_ready_id = ConnectorConnectionId::new();
    let active_second_id = ConnectorConnectionId::new();
    let suspended_ready_id = ConnectorConnectionId::new();
    let active_unauthorized_id = ConnectorConnectionId::new();
    let (catalog, _) = governed_catalog(
        vec![
            candidate(
                tenant_id,
                active_second_id,
                "active_second",
                ConnectionStatus::Active,
                ConnectionHealth::Ready,
            ),
            candidate(
                tenant_id,
                suspended_ready_id,
                "suspended",
                ConnectionStatus::Suspended,
                ConnectionHealth::Ready,
            ),
            candidate(
                tenant_id,
                active_unauthorized_id,
                "unauthorized",
                ConnectionStatus::Active,
                ConnectionHealth::Ready,
            ),
            candidate(
                tenant_id,
                active_ready_id,
                "active_ready",
                ConnectionStatus::Active,
                ConnectionHealth::Ready,
            ),
        ],
        [active_ready_id, active_second_id, suspended_ready_id],
    );

    let snapshot = catalog
        .snapshot(InstalledConnectorCatalogQuery::new(
            identity(tenant_id),
            [active_ready_id, active_second_id, suspended_ready_id],
        ))
        .await
        .expect("valid in-memory candidates should produce a catalog snapshot");
    let visible = snapshot
        .actions()
        .iter()
        .map(|action| (action.connection_id(), action.binding().action_id.as_str()))
        .collect::<Vec<_>>();

    let mut expected = vec![
        (active_ready_id, "active_ready"),
        (active_second_id, "active_second"),
    ];
    expected.sort_by_key(|(connection_id, action_id)| (connection_id.0, *action_id));
    assert_eq!(snapshot.tenant_id(), tenant_id);
    assert_eq!(snapshot.len(), 2);
    assert_eq!(visible, expected);
}

#[tokio::test]
async fn catalog_rejects_stale_binding_generation_instead_of_serving_partial_snapshot_offline() {
    // Pins: a stale compiled binding fails the whole immutable publication closed;
    // it is never silently mixed with a current-generation action.
    let tenant_id = TenantId::new();
    let current_id = ConnectorConnectionId::new();
    let stale_id = ConnectorConnectionId::new();
    let current = candidate(
        tenant_id,
        current_id,
        "current",
        ConnectionStatus::Active,
        ConnectionHealth::Ready,
    );
    let (stale_connection, mut stale_binding) = candidate(
        tenant_id,
        stale_id,
        "stale",
        ConnectionStatus::Active,
        ConnectionHealth::Ready,
    );
    stale_binding.connection_generation =
        ConnectionGeneration::new(2).expect("fixture generation should be positive");
    let (catalog, _) = governed_catalog(
        vec![current, (stale_connection, stale_binding)],
        [current_id, stale_id],
    );

    let error = catalog
        .snapshot(InstalledConnectorCatalogQuery::new(
            identity(tenant_id),
            [current_id, stale_id],
        ))
        .await
        .expect_err("stale generation must fail the complete snapshot");

    assert!(matches!(
        error,
        moa_connectors::Error::CatalogInvariant { .. }
    ));
}

#[tokio::test]
async fn catalog_with_no_requested_connections_is_empty_offline() {
    // Pins: an empty selected-connection set exposes zero installed actions.
    let tenant_id = TenantId::new();
    let (catalog, _) = governed_catalog(
        vec![candidate(
            tenant_id,
            ConnectorConnectionId::new(),
            "hidden",
            ConnectionStatus::Active,
            ConnectionHealth::Ready,
        )],
        [],
    );

    let snapshot = catalog
        .snapshot(InstalledConnectorCatalogQuery::new(identity(tenant_id), []))
        .await
        .expect("an empty authorization set should produce an empty snapshot");

    assert!(snapshot.is_empty());
    assert_eq!(snapshot.actions(), []);
}

#[test]
fn catalog_query_derives_tenant_from_authenticated_caller_offline() {
    // Pins: catalog callers cannot supply a tenant independently from their
    // authenticated identity.
    let tenant_id = TenantId::new();
    let caller = identity(tenant_id);
    let query = InstalledConnectorCatalogQuery::new(caller.clone(), []);

    assert_eq!(query.caller, caller);
    assert_eq!(query.tenant_id(), tenant_id);
}

#[tokio::test]
async fn connector_invocation_derives_tenant_and_redacts_input_from_debug_offline() {
    // Pins: durable dispatch derives tenant from the authenticated caller and
    // cannot leak model input through routine Debug formatting.
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let (catalog, _) = governed_catalog(
        vec![candidate(
            tenant_id,
            connection_id,
            "debug_safe",
            ConnectionStatus::Active,
            ConnectionHealth::Ready,
        )],
        [connection_id],
    );
    let snapshot = catalog
        .snapshot(InstalledConnectorCatalogQuery::new(
            identity(tenant_id),
            [connection_id],
        ))
        .await
        .expect("fixture should produce one visible action");
    let action = snapshot
        .actions()
        .first()
        .expect("fixture should expose one action");
    let invocation = ConnectorActionInvocation {
        caller: identity(tenant_id),
        tool_call_id: ToolCallId::new(),
        action: InstalledConnectorActionPin::from(action),
        input: json!({"customer_note": "debug-canary-credential"}),
        cancellation_token: CancellationToken::new(),
    };

    let debug = format!("{invocation:?}");
    assert_eq!(invocation.tenant_id(), tenant_id);
    assert!(!debug.contains("debug-canary-credential"));
    assert!(debug.contains("<connector action input>"));
}

#[tokio::test]
async fn catalog_denial_happens_before_protected_candidate_read_offline() {
    // Pins: requested connection IDs are not authorization proof; one denied
    // delegated-use check prevents every protected source read.
    let tenant_id = TenantId::new();
    let denied_connection_id = ConnectorConnectionId::new();
    let (catalog, read_count) = governed_catalog(
        vec![candidate(
            tenant_id,
            denied_connection_id,
            "denied",
            ConnectionStatus::Active,
            ConnectionHealth::Ready,
        )],
        [],
    );

    let error = catalog
        .snapshot(InstalledConnectorCatalogQuery::new(
            identity(tenant_id),
            [denied_connection_id],
        ))
        .await
        .expect_err("delegated-use denial must fail the catalog read");

    assert!(matches!(
        error,
        moa_connectors::Error::CatalogInvariant { .. }
    ));
    assert_eq!(read_count.load(Ordering::SeqCst), 0);
}

fn candidate(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    action_id: &str,
    status: ConnectionStatus,
    health: ConnectionHealth,
) -> (ConnectorConnection, InstalledActionBinding) {
    let now = Utc::now();
    let generation = ConnectionGeneration::new(1).expect("fixture generation should be positive");
    let definition: ConnectorDefinition = serde_json::from_value(definition_fixture(action_id))
        .expect("fixture definition should match the runtime V1 contract");
    let action = definition
        .actions
        .first()
        .expect("fixture definition should declare one action");
    let compiled_contract = CompiledOperationContract::compile(&definition, action)
        .expect("fixture contract should compile");
    let contract_hash = compiled_contract
        .hash()
        .expect("fixture contract should hash canonically");
    let connection = ConnectorConnection {
        connection_id,
        tenant_id,
        display_name: format!("Account {action_id}"),
        definition: ConnectionDefinitionRef::Artifact {
            artifact_uid: Uuid::new_v4(),
            revision_uid: Uuid::new_v4(),
        },
        origin: Some("https://api.example.test".parse().expect("fixture origin")),
        non_secret_config: json!({}),
        generation,
        status,
        health,
        health_reason: None,
        created_by_identity_id: None,
        owner_identity_id: None,
        created_at: now,
        updated_at: now,
    };
    let binding = InstalledActionBinding {
        binding_id: InstalledActionBindingId(Uuid::new_v4()),
        tenant_id,
        connection_id,
        connection_generation: generation,
        action_id: action_id.to_string(),
        compiled_contract,
        contract_hash,
        governed_contract_revision: "connector-action/v1".to_string(),
        minimum_effect: ActionPolicyEffect::AdminReview,
        enabled: true,
    };
    (connection, binding)
}

fn definition_fixture(action_id: &str) -> Value {
    json!({
        "definition_version": "v1",
        "display_name": "Offline HTTP fixture",
        "auth": [{"type": "none"}],
        "actions": [{
            "id": action_id,
            "description": "Offline catalog fixture.",
            "contract": {
                "method": "GET",
                "path_template": "/fixture",
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
    })
}

fn identity(tenant_id: TenantId) -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::new_v4(),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn governed_catalog(
    candidates: Vec<(ConnectorConnection, InstalledActionBinding)>,
    allowed: impl IntoIterator<Item = ConnectorConnectionId>,
) -> (GovernedInstalledConnectorCatalog, Arc<AtomicUsize>) {
    let read_count = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(InMemoryInstalledConnectorCatalogSource {
        candidates,
        read_count: Arc::clone(&read_count),
    });
    let authorizer = Arc::new(AllowListConnectorUseAuthorizer {
        allowed: allowed.into_iter().collect(),
    });
    (
        GovernedInstalledConnectorCatalog::new(source, authorizer),
        read_count,
    )
}
