//! Postgres repository coverage for connector lifecycle and authorization intent.

use moa_artifacts::connector::ConnectorDefinition;
use moa_authz_schema::MODEL_VERSION;
use moa_connectors::Error;
use moa_connectors::catalog::InstalledConnectorCatalogSource;
use moa_connectors::domain::{
    CompiledOperationContract, ConnectionDefinitionRef, ConnectionGeneration, ConnectionOrigin,
    ConnectionStatus, InstalledActionBinding, InstalledActionBindingId,
};
use moa_connectors::repository::{
    ConnectionActivation, ConnectionLifecycleRepository, NewConnectorConnection,
    PostgresConnectionRepository,
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
struct AuthzTupleRow {
    op: String,
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    model_version: i32,
    generation: i64,
    status: String,
    tenant_id: Uuid,
}

#[tokio::test]
async fn create_commits_exact_tenant_and_owner_outbox_tuples_db_memory() {
    // Pins: connection creation and both authorization intents are one atomic write,
    // with no guessed or extra relationship published for the new resource.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector repository test database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let owner_id = Uuid::new_v4();
    let creator_id = Uuid::new_v4();

    let created = repository
        .create(new_connection(
            tenant_id,
            connection_id,
            owner_id,
            Some(creator_id),
        ))
        .await
        .expect("valid connector connection should be created");

    assert_eq!(created.tenant_id, tenant_id);
    assert_eq!(created.connection_id, connection_id);
    assert_eq!(created.status, ConnectionStatus::PendingAuth);
    assert_eq!(created.generation, generation(1));
    assert_eq!(created.owner_identity_id, Some(owner_id));
    assert_eq!(created.created_by_identity_id, Some(creator_id));
    assert_eq!(created.non_secret_config, json!({"region": "us-east-1"}));

    let other_tenant = TenantId::new();
    assert!(
        repository
            .load(other_tenant, connection_id)
            .await
            .expect("cross-tenant lookup should fail closed as an empty RLS projection")
            .is_none()
    );
    assert!(
        repository
            .list(
                other_tenant,
                moa_connectors::repository::ConnectionListRequest {
                    after: None,
                    limit: 50,
                },
            )
            .await
            .expect("cross-tenant catalog metadata should remain hidden by RLS")
            .connections
            .is_empty()
    );

    assert_eq!(
        authz_rows(&pool, connection_id).await,
        vec![
            AuthzTupleRow {
                op: "write".to_string(),
                tuple_user: format!("operator:{owner_id}"),
                tuple_relation: "owner".to_string(),
                tuple_object: format!("connector_connection:{connection_id}"),
                model_version: MODEL_VERSION as i32,
                generation: 1,
                status: "pending".to_string(),
                tenant_id: tenant_id.0,
            },
            AuthzTupleRow {
                op: "write".to_string(),
                tuple_user: format!("tenant:{tenant_id}"),
                tuple_relation: "tenant".to_string(),
                tuple_object: format!("connector_connection:{connection_id}"),
                model_version: MODEL_VERSION as i32,
                generation: 1,
                status: "pending".to_string(),
                tenant_id: tenant_id.0,
            },
        ]
    );
}

#[tokio::test]
async fn activation_rolls_back_partial_bindings_and_lifecycle_fences_catalog_db_memory() {
    // Pins: activation is all-or-nothing, stale generations cannot mutate lifecycle,
    // suspension hides rather than destroys bindings, and deletion publishes exact inverses.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector lifecycle test database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let owner_id = Uuid::new_v4();
    repository
        .create(new_connection(tenant_id, connection_id, owner_id, None))
        .await
        .expect("connection fixture should be created");

    let definition = definition_fixture(&["invoice_create", "invoice_cancel"]);
    let duplicate_binding_id = InstalledActionBindingId(Uuid::new_v4());
    let invalid = ConnectionActivation {
        tenant_id,
        connection_id,
        expected_generation: generation(1),
        bindings: definition
            .actions
            .iter()
            .map(|action| {
                binding(
                    tenant_id,
                    connection_id,
                    generation(2),
                    duplicate_binding_id,
                    &definition,
                    action,
                )
            })
            .collect(),
    };
    let error = repository
        .activate(invalid)
        .await
        .expect_err("duplicate binding identity should fail after the first insert");
    assert!(matches!(error, Error::Storage(_)));

    let after_failed_activation = repository
        .load(tenant_id, connection_id)
        .await
        .expect("connection should remain readable after rolled-back activation")
        .expect("connection fixture should still exist");
    assert_eq!(
        after_failed_activation.status,
        ConnectionStatus::PendingAuth
    );
    assert_eq!(after_failed_activation.generation, generation(1));
    assert_eq!(binding_count(&pool, tenant_id, connection_id).await, 0);

    let bindings = definition
        .actions
        .iter()
        .map(|action| {
            binding(
                tenant_id,
                connection_id,
                generation(2),
                InstalledActionBindingId(Uuid::new_v4()),
                &definition,
                action,
            )
        })
        .collect::<Vec<_>>();
    let activated = repository
        .activate(ConnectionActivation {
            tenant_id,
            connection_id,
            expected_generation: generation(1),
            bindings: bindings.clone(),
        })
        .await
        .expect("valid binding replacement should activate atomically");
    assert_eq!(activated.status, ConnectionStatus::Active);
    assert_eq!(activated.generation, generation(2));
    assert_eq!(catalog_len(&repository, tenant_id, connection_id).await, 2);

    let stale = repository
        .transition(
            tenant_id,
            connection_id,
            generation(1),
            ConnectionStatus::Suspended,
        )
        .await
        .expect_err("stale lifecycle generation must fail without a write");
    assert!(matches!(
        stale,
        Error::GenerationConflict { expected, actual }
            if expected == generation(1) && actual == generation(2)
    ));
    assert_eq!(catalog_len(&repository, tenant_id, connection_id).await, 2);

    let suspended = repository
        .transition(
            tenant_id,
            connection_id,
            generation(2),
            ConnectionStatus::Suspended,
        )
        .await
        .expect("active connection should suspend");
    assert_eq!(suspended.status, ConnectionStatus::Suspended);
    assert_eq!(catalog_len(&repository, tenant_id, connection_id).await, 0);
    assert_eq!(
        enabled_binding_count(&pool, tenant_id, connection_id).await,
        2
    );

    repository
        .transition(
            tenant_id,
            connection_id,
            generation(2),
            ConnectionStatus::Active,
        )
        .await
        .expect("suspended connection should resume without recompiling bindings");
    assert_eq!(catalog_len(&repository, tenant_id, connection_id).await, 2);

    repository
        .transition(
            tenant_id,
            connection_id,
            generation(2),
            ConnectionStatus::Disconnecting,
        )
        .await
        .expect("active connection should enter teardown");
    assert_eq!(
        enabled_binding_count(&pool, tenant_id, connection_id).await,
        0
    );
    let deleted = repository
        .transition(
            tenant_id,
            connection_id,
            generation(2),
            ConnectionStatus::Deleted,
        )
        .await
        .expect("disconnecting connection should reach the retained terminal state");
    assert_eq!(deleted.status, ConnectionStatus::Deleted);
    assert_eq!(catalog_len(&repository, tenant_id, connection_id).await, 0);

    assert_eq!(
        authz_rows(&pool, connection_id).await,
        vec![
            AuthzTupleRow {
                op: "delete".to_string(),
                tuple_user: format!("operator:{owner_id}"),
                tuple_relation: "owner".to_string(),
                tuple_object: format!("connector_connection:{connection_id}"),
                model_version: MODEL_VERSION as i32,
                generation: 2,
                status: "pending".to_string(),
                tenant_id: tenant_id.0,
            },
            AuthzTupleRow {
                op: "delete".to_string(),
                tuple_user: format!("tenant:{tenant_id}"),
                tuple_relation: "tenant".to_string(),
                tuple_object: format!("connector_connection:{connection_id}"),
                model_version: MODEL_VERSION as i32,
                generation: 2,
                status: "pending".to_string(),
                tenant_id: tenant_id.0,
            },
        ]
    );
}

#[tokio::test]
async fn credential_generation_fence_is_cas_and_suspends_active_connection_db_memory() {
    // Pins: after a credential write, exactly one concurrent fence advances the
    // generation, active actions disappear immediately, and teardown states reject fencing.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap credential-generation fence test database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    repository
        .create(new_connection(
            tenant_id,
            connection_id,
            Uuid::new_v4(),
            None,
        ))
        .await
        .expect("active fence fixture should be created");
    let definition = definition_fixture(&["invoice_create"]);
    let action = definition
        .actions
        .first()
        .expect("fixture definition should contain one action");
    repository
        .activate(ConnectionActivation {
            tenant_id,
            connection_id,
            expected_generation: generation(1),
            bindings: vec![binding(
                tenant_id,
                connection_id,
                generation(2),
                InstalledActionBindingId(Uuid::new_v4()),
                &definition,
                action,
            )],
        })
        .await
        .expect("credential fence fixture should activate");

    let first = repository.clone();
    let second = repository.clone();
    let (left, right) = tokio::join!(
        first.advance_credential_generation(tenant_id, connection_id, generation(2)),
        second.advance_credential_generation(tenant_id, connection_id, generation(2)),
    );
    let (winner, loser) = match (left, right) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
        outcomes => panic!("exactly one credential fence should win, observed {outcomes:?}"),
    };
    assert_eq!(winner.generation, generation(3));
    assert_eq!(winner.status, ConnectionStatus::Suspended);
    assert!(matches!(
        loser,
        Error::GenerationConflict { expected, actual }
            if expected == generation(2) && actual == generation(3)
    ));
    assert_eq!(catalog_len(&repository, tenant_id, connection_id).await, 0);
    assert_eq!(
        enabled_binding_count(&pool, tenant_id, connection_id).await,
        0
    );
    let resume = repository
        .transition(
            tenant_id,
            connection_id,
            generation(3),
            ConnectionStatus::Active,
        )
        .await
        .expect_err("credential fence must prevent resuming stale bindings");
    assert!(matches!(resume, Error::InvalidContract { .. }));

    let still_suspended = repository
        .advance_credential_generation(tenant_id, connection_id, generation(3))
        .await
        .expect("a second credential write should advance an already-suspended connection");
    assert_eq!(still_suspended.generation, generation(4));
    assert_eq!(still_suspended.status, ConnectionStatus::Suspended);

    let pending_id = ConnectorConnectionId::new();
    repository
        .create(new_connection(tenant_id, pending_id, Uuid::new_v4(), None))
        .await
        .expect("pending fence fixture should be created");
    let pending = repository
        .advance_credential_generation(tenant_id, pending_id, generation(1))
        .await
        .expect("pending-auth connection should advance without changing lifecycle");
    assert_eq!(pending.generation, generation(2));
    assert_eq!(pending.status, ConnectionStatus::PendingAuth);
    repository
        .transition(
            tenant_id,
            pending_id,
            generation(2),
            ConnectionStatus::Disconnecting,
        )
        .await
        .expect("pending-auth fixture should enter disconnecting state");
    repository
        .transition(
            tenant_id,
            pending_id,
            generation(2),
            ConnectionStatus::Deleted,
        )
        .await
        .expect("disconnecting fixture should enter deleted state");
    let teardown = repository
        .advance_credential_generation(tenant_id, pending_id, generation(2))
        .await
        .expect_err("deleted connection must reject credential generation writes");
    assert!(matches!(teardown, Error::InvalidContract { .. }));
}

#[tokio::test]
async fn load_pinned_action_joins_exact_connection_and_binding_under_tenant_rls_db_memory() {
    // Pins: the final pre-send read returns the exact tenant connection generation,
    // immutable artifact revision, and action binding from one joined snapshot; a
    // credential fence leaves the stale binding visible but disabled so the executor
    // can reject its generation pin, while tenant RLS hides the row entirely.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap pinned connector action test database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let other_tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let artifact_uid = Uuid::new_v4();
    let revision_uid = Uuid::new_v4();
    let definition = definition_fixture(&["invoice_create"]);
    insert_connector_artifact_revision(&pool, tenant_id, artifact_uid, revision_uid, &definition)
        .await;
    let definition_ref = ConnectionDefinitionRef::Artifact {
        artifact_uid,
        revision_uid,
    };
    repository
        .create(NewConnectorConnection {
            connection_id,
            tenant_id,
            display_name: "Pinned billing account".to_string(),
            definition_ref: definition_ref.clone(),
            origin: Some(
                ConnectionOrigin::parse("https://billing.example.com")
                    .expect("fixture connector origin should be canonical"),
            ),
            non_secret_config: json!({"region": "eu-west-1"}),
            created_by_identity_id: Some(Uuid::new_v4()),
            owner_identity_id: Uuid::new_v4(),
        })
        .await
        .expect("artifact-backed connection fixture should be created");

    let action = definition
        .actions
        .first()
        .expect("fixture definition should contain one action");
    let binding_id = InstalledActionBindingId(Uuid::new_v4());
    let expected_binding = binding(
        tenant_id,
        connection_id,
        generation(2),
        binding_id,
        &definition,
        action,
    );
    let activated = repository
        .activate(ConnectionActivation {
            tenant_id,
            connection_id,
            expected_generation: generation(1),
            bindings: vec![expected_binding.clone()],
        })
        .await
        .expect("pinned action fixture should activate atomically");

    let pinned = repository
        .load_pinned_action(tenant_id, connection_id, binding_id)
        .await
        .expect("joined final pin read should succeed")
        .expect("exact tenant connection and binding should exist");
    assert_eq!(pinned.connection, activated);
    assert_eq!(pinned.connection.definition, definition_ref);
    assert_eq!(pinned.connection.generation, generation(2));
    assert_eq!(pinned.binding, expected_binding);
    assert_eq!(
        pinned.binding.governed_contract_revision,
        "billing/v1/invoice_create"
    );

    assert!(
        repository
            .load_pinned_action(other_tenant_id, connection_id, binding_id)
            .await
            .expect("cross-tenant final pin read should fail closed")
            .is_none(),
        "tenant RLS must hide both the connection and its binding"
    );
    assert!(
        repository
            .load_pinned_action(
                tenant_id,
                connection_id,
                InstalledActionBindingId(Uuid::new_v4()),
            )
            .await
            .expect("missing binding lookup should remain a successful read")
            .is_none(),
        "an unknown binding identity must not select a different action"
    );

    let fenced = repository
        .advance_credential_generation(tenant_id, connection_id, generation(2))
        .await
        .expect("credential invalidation should advance and suspend the connection");
    assert_eq!(fenced.generation, generation(3));
    assert_eq!(fenced.status, ConnectionStatus::Suspended);
    let stale_pin = repository
        .load_pinned_action(tenant_id, connection_id, binding_id)
        .await
        .expect("joined final pin read should expose authoritative invalidation state")
        .expect("immutable stale binding should remain available for final validation");
    assert_eq!(stale_pin.connection.generation, generation(3));
    assert_eq!(stale_pin.connection.status, ConnectionStatus::Suspended);
    assert_eq!(stale_pin.binding.connection_generation, generation(2));
    assert!(!stale_pin.binding.enabled);
}

async fn insert_connector_artifact_revision(
    pool: &PgPool,
    tenant_id: TenantId,
    artifact_uid: Uuid,
    revision_uid: Uuid,
    definition: &ConnectorDefinition,
) {
    sqlx::query(
        "INSERT INTO moa.artifact \
             (artifact_uid, tenant_id, storage_partition_id, kind, name) \
         VALUES ($1, $2, $2::TEXT, 'connector', $3)",
    )
    .bind(artifact_uid)
    .bind(tenant_id.0)
    .bind(format!("pinned-connector-{artifact_uid}"))
    .execute(pool)
    .await
    .expect("connector artifact fixture should be persisted");
    sqlx::query(
        "INSERT INTO moa.artifact_revision \
             (revision_uid, artifact_uid, tenant_id, storage_partition_id, definition, \
              canonical_hash, source_format, source_text, status, version) \
         VALUES ($1, $2, $3, $3::TEXT, $4, $5, 'json', $6, 'published', 1)",
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(tenant_id.0)
    .bind(serde_json::to_value(definition).expect("connector definition should serialize"))
    .bind(vec![7_u8; 32])
    .bind(serde_json::to_vec(definition).expect("connector source fixture should serialize"))
    .execute(pool)
    .await
    .expect("connector artifact revision fixture should be persisted");
}

fn new_connection(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    owner_identity_id: Uuid,
    created_by_identity_id: Option<Uuid>,
) -> NewConnectorConnection {
    NewConnectorConnection {
        connection_id,
        tenant_id,
        display_name: "Billing account".to_string(),
        definition_ref: ConnectionDefinitionRef::built_in("knowledge:nango", 1)
            .expect("fixture built-in definition should be valid"),
        origin: None,
        non_secret_config: json!({"region": "us-east-1"}),
        created_by_identity_id,
        owner_identity_id,
    }
}

fn definition_fixture(action_ids: &[&str]) -> ConnectorDefinition {
    let actions = action_ids
        .iter()
        .map(|action_id| {
            json!({
                "id": action_id,
                "description": "DB-memory connector fixture action.",
                "contract": {
                    "method": "POST",
                    "path_template": "/actions",
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
            })
        })
        .collect::<Vec<Value>>();
    serde_json::from_value(json!({
        "display_name": "Billing fixture",
        "auth": [{"type": "none"}],
        "actions": actions,
    }))
    .expect("fixture definition should match the runtime connector contract")
}

fn binding(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    connection_generation: ConnectionGeneration,
    binding_id: InstalledActionBindingId,
    definition: &ConnectorDefinition,
    action: &moa_artifacts::connector::RuntimeConnectorAction,
) -> InstalledActionBinding {
    let compiled_contract = CompiledOperationContract::compile(definition, action)
        .expect("fixture operation contract should compile");
    let contract_hash = compiled_contract
        .hash()
        .expect("fixture operation contract should hash");
    InstalledActionBinding {
        binding_id,
        tenant_id,
        connection_id,
        connection_generation,
        action_id: action.id.clone(),
        compiled_contract,
        contract_hash,
        governed_contract_revision: format!("billing/v1/{}", action.id),
        minimum_effect: moa_core::types::action_policy::ActionPolicyEffect::AdminReview,
        enabled: true,
    }
}

fn generation(value: u64) -> ConnectionGeneration {
    ConnectionGeneration::new(value).expect("fixture generation should be positive")
}

async fn catalog_len(
    repository: &PostgresConnectionRepository,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> usize {
    repository
        .candidates(tenant_id, &[connection_id])
        .await
        .expect("catalog candidates should load")
        .len()
}

async fn authz_rows(pool: &PgPool, connection_id: ConnectorConnectionId) -> Vec<AuthzTupleRow> {
    sqlx::query_as(
        "SELECT op, tuple_user, tuple_relation, tuple_object, model_version, generation, status, \
         tenant_id FROM authz_outbox WHERE tuple_object = $1 \
         ORDER BY tuple_relation, tuple_user",
    )
    .bind(format!("connector_connection:{connection_id}"))
    .fetch_all(pool)
    .await
    .expect("connector authorization tuples should load")
}

async fn binding_count(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM moa.connector_action_bindings \
         WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .fetch_one(pool)
    .await
    .expect("connector binding count should load")
}

async fn enabled_binding_count(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM moa.connector_action_bindings \
         WHERE tenant_id = $1 AND connection_uid = $2 AND enabled",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .fetch_one(pool)
    .await
    .expect("enabled connector binding count should load")
}
