//! Postgres coverage for replay-safe managed knowledge parents.

use moa_authz_schema::MODEL_VERSION;
use moa_connectors::Error;
use moa_connectors::domain::{
    ConnectionGeneration, ConnectionStatus, ManagedParentDefinition, ManagedParentDeleteOutcome,
    ManagedParentPreservationReason,
};
use moa_connectors::repository::{
    ConnectionActivation, ConnectionLifecycleRepository, ManagedParentRepository,
    NewConnectorConnection, PostgresConnectionRepository,
};
use moa_connectors::service::{
    ManagedParentActivationRequest, ManagedParentClaimRequest, ManagedParentDeleteRequest,
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use serde_json::json;
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
async fn managed_parent_claim_replays_owned_bit_and_refuses_ownerless_creation_db_memory() {
    // Pins: the parent, claim ownership bit, and authorization intents commit
    // atomically, and an ownerless service actor may resume but never insert.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap managed-parent claim database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let owner_id = Uuid::new_v4();
    let request = claim_request(
        tenant_id,
        connection_id,
        "link-operation-1",
        &"a".repeat(64),
        ManagedParentDefinition::KnowledgeMerge,
        Some(owner_id),
    );

    let claimed = repository
        .claim_managed_parent(request.clone())
        .await
        .expect("owned first claim should create the managed parent");
    assert!(claimed.parent_created_by_claim);
    assert_eq!(claimed.connection.connection_id, connection_id);
    assert_eq!(claimed.connection.tenant_id, tenant_id);
    assert_eq!(claimed.connection.owner_identity_id, Some(owner_id));
    assert_eq!(claimed.connection.status, ConnectionStatus::PendingAuth);
    assert_eq!(claimed.connection.generation, generation(1));
    assert_eq!(
        managed_claim_row(&pool, tenant_id, "link-operation-1").await,
        ("a".repeat(64), connection_id.0, true,)
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

    let mut replay = request.clone();
    replay.owner_identity_id = None;
    let replayed = repository
        .claim_managed_parent(replay)
        .await
        .expect("ownerless service replay should return the durable ownership bit");
    assert!(replayed.parent_created_by_claim);
    assert_eq!(authz_rows(&pool, connection_id).await.len(), 2);

    let mut conflict = request;
    conflict.request_hash = "b".repeat(64);
    assert!(matches!(
        repository.claim_managed_parent(conflict).await,
        Err(Error::ManagedParentClaimConflict { connection_id: actual })
            if actual == connection_id
    ));

    let ownerless_id = ConnectorConnectionId::new();
    let ownerless = claim_request(
        tenant_id,
        ownerless_id,
        "ownerless-new-operation",
        &"c".repeat(64),
        ManagedParentDefinition::KnowledgeNango,
        None,
    );
    assert!(matches!(
        repository.claim_managed_parent(ownerless).await,
        Err(Error::ManagedParentOwnerRequired { connection_id: actual })
            if actual == ownerless_id
    ));
    assert!(
        repository
            .load(tenant_id, ownerless_id)
            .await
            .expect("ownerless refusal should leave the repository readable")
            .is_none()
    );
    assert_eq!(
        managed_claim_count(&pool, tenant_id, "ownerless-new-operation").await,
        0
    );
}

#[tokio::test]
async fn managed_parent_existing_compatibility_is_exact_and_never_claims_creation_db_memory() {
    // Pins: a new operation can attach ownerlessly only to an exact existing
    // parent, and neither config drift nor an arbitrary built-in is accepted.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap managed-parent compatibility database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    repository
        .create(existing_managed_parent(
            tenant_id,
            connection_id,
            ManagedParentDefinition::KnowledgeNango,
            "",
            "",
            "",
        ))
        .await
        .expect("exact pre-existing managed parent should be seeded");

    let first_inactive = repository
        .claim_managed_parent(claim_request(
            tenant_id,
            connection_id,
            "attach-existing",
            &"d".repeat(64),
            ManagedParentDefinition::KnowledgeNango,
            None,
        ))
        .await
        .expect_err("a first claim must preserve a pre-existing inactive lifecycle");
    assert!(matches!(
        first_inactive,
        Error::ManagedParentMismatch {
            connection_id: actual,
            field: "lifecycle_status",
        } if actual == connection_id
    ));
    assert_eq!(
        managed_claim_count(&pool, tenant_id, "attach-existing").await,
        0
    );
    set_connection_active(&pool, tenant_id, connection_id).await;

    let exact_request = claim_request(
        tenant_id,
        connection_id,
        "attach-existing",
        &"d".repeat(64),
        ManagedParentDefinition::KnowledgeNango,
        None,
    );
    let claim = repository
        .claim_managed_parent(exact_request.clone())
        .await
        .expect("ownerless operation may attach to an exact existing parent");
    assert!(!claim.parent_created_by_claim);
    assert_eq!(claim.connection.display_name, "Renamed by operator");
    repository
        .transition(
            tenant_id,
            connection_id,
            generation(1),
            ConnectionStatus::Suspended,
        )
        .await
        .expect("active pre-existing parent should suspend after its claim commits");
    let suspended_replay = repository
        .claim_managed_parent(exact_request)
        .await
        .expect("the exact ledger replay may resume its now-suspended parent");
    assert_eq!(
        suspended_replay.connection.status,
        ConnectionStatus::Suspended
    );
    assert!(!suspended_replay.parent_created_by_claim);
    assert!(matches!(
        repository
            .delete_managed_parent_if_unused(delete_request(
                tenant_id,
                connection_id,
                "attach-existing",
                &"d".repeat(64),
            ))
            .await
            .expect("pre-existing compensation should be a typed preservation"),
        ManagedParentDeleteOutcome::Preserved {
            reason: ManagedParentPreservationReason::PreExisting,
            ..
        }
    ));

    let mismatch_id = ConnectorConnectionId::new();
    repository
        .create(existing_managed_parent(
            tenant_id,
            mismatch_id,
            ManagedParentDefinition::KnowledgeMerge,
            "merge-config",
            "different-account",
            "ats",
        ))
        .await
        .expect("mismatch fixture should be seeded");
    set_connection_active(&pool, tenant_id, mismatch_id).await;
    let mismatch = repository
        .claim_managed_parent(claim_request(
            tenant_id,
            mismatch_id,
            "mismatched-account",
            &"e".repeat(64),
            ManagedParentDefinition::KnowledgeMerge,
            None,
        ))
        .await
        .expect_err("knowledge metadata on a generic parent must fail closed");
    assert!(matches!(
        mismatch,
        Error::ManagedParentMismatch {
            connection_id: actual,
            field: "non_secret_config",
        } if actual == mismatch_id
    ));

    let wrong_definition_id = ConnectorConnectionId::new();
    repository
        .create(existing_managed_parent(
            tenant_id,
            wrong_definition_id,
            ManagedParentDefinition::KnowledgeMerge,
            "",
            "",
            "",
        ))
        .await
        .expect("wrong managed definition fixture should be seeded");
    set_connection_active(&pool, tenant_id, wrong_definition_id).await;
    assert!(matches!(
        repository
            .claim_managed_parent(claim_request(
                tenant_id,
                wrong_definition_id,
                "wrong-managed-definition",
                &"f".repeat(64),
                ManagedParentDefinition::KnowledgeNango,
                None,
            ))
            .await,
        Err(Error::ManagedParentMismatch {
            connection_id: actual,
            field: "definition",
        }) if actual == wrong_definition_id
    ));
}

#[tokio::test]
async fn managed_parent_activation_and_compensation_preserve_generation_and_dependents_db_memory() {
    // Pins: knowledge-only activation creates no fake binding and preserves the
    // credential fence, while compensation deletes only claim-created parents
    // that have no current capability dependency.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap managed-parent lifecycle database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let operation_id = "managed-lifecycle";
    let request_hash = "1".repeat(64);
    repository
        .claim_managed_parent(claim_request(
            tenant_id,
            connection_id,
            operation_id,
            &request_hash,
            ManagedParentDefinition::KnowledgeNango,
            Some(Uuid::new_v4()),
        ))
        .await
        .expect("lifecycle fixture should claim a new parent");

    let activated = repository
        .activate_managed_knowledge_parent(ManagedParentActivationRequest {
            tenant_id,
            connection_id,
            expected_generation: generation(1),
            definition: ManagedParentDefinition::KnowledgeNango,
        })
        .await
        .expect("knowledge-only parent should activate without action bindings");
    assert_eq!(activated.status, ConnectionStatus::Active);
    assert_eq!(activated.generation, generation(1));
    assert_eq!(binding_count(&pool, tenant_id, connection_id).await, 0);
    repository
        .transition(
            tenant_id,
            connection_id,
            generation(1),
            ConnectionStatus::Suspended,
        )
        .await
        .expect("managed parent should suspend under the same fence");
    let reactivated = repository
        .activate_managed_knowledge_parent(ManagedParentActivationRequest {
            tenant_id,
            connection_id,
            expected_generation: generation(1),
            definition: ManagedParentDefinition::KnowledgeNango,
        })
        .await
        .expect("suspended knowledge-only parent should reactivate without a new generation");
    assert_eq!(reactivated.status, ConnectionStatus::Active);
    assert_eq!(reactivated.generation, generation(1));
    assert!(matches!(
        repository
            .activate(ConnectionActivation {
                tenant_id,
                connection_id,
                expected_generation: generation(1),
                bindings: vec![],
            })
            .await,
        Err(Error::InvalidContract { .. })
    ));

    let deleted = repository
        .delete_managed_parent_if_unused(delete_request(
            tenant_id,
            connection_id,
            operation_id,
            &request_hash,
        ))
        .await
        .expect("unused claim-created parent should compensate");
    assert!(matches!(
        deleted,
        ManagedParentDeleteOutcome::Deleted(ref parent)
            if parent.status == ConnectionStatus::Deleted
                && parent.generation == generation(1)
    ));
    assert!(matches!(
        repository
            .delete_managed_parent_if_unused(delete_request(
                tenant_id,
                connection_id,
                operation_id,
                &request_hash,
            ))
            .await
            .expect("deletion replay should be idempotent"),
        ManagedParentDeleteOutcome::AlreadyDeleted(ref parent)
            if parent.status == ConnectionStatus::Deleted
    ));
    assert_eq!(
        authz_rows(&pool, connection_id)
            .await
            .into_iter()
            .map(|row| (row.op, row.tuple_relation, row.generation))
            .collect::<Vec<_>>(),
        vec![
            ("delete".to_string(), "owner".to_string(), 2),
            ("delete".to_string(), "tenant".to_string(), 2),
        ]
    );

    let dependent_id = ConnectorConnectionId::new();
    let dependent_hash = "2".repeat(64);
    repository
        .claim_managed_parent(claim_request(
            tenant_id,
            dependent_id,
            "dependent-parent",
            &dependent_hash,
            ManagedParentDefinition::KnowledgeNango,
            Some(Uuid::new_v4()),
        ))
        .await
        .expect("dependent fixture should claim a new parent");
    insert_action_dependent(&pool, tenant_id, dependent_id).await;
    assert!(matches!(
        repository
            .activate_managed_knowledge_parent(ManagedParentActivationRequest {
                tenant_id,
                connection_id: dependent_id,
                expected_generation: generation(1),
                definition: ManagedParentDefinition::KnowledgeNango,
            })
            .await,
        Err(Error::ManagedParentActionDependents { connection_id: actual })
            if actual == dependent_id
    ));
    assert!(matches!(
        repository
            .delete_managed_parent_if_unused(delete_request(
                tenant_id,
                dependent_id,
                "dependent-parent",
                &dependent_hash,
            ))
            .await
            .expect("dependent parent should be preserved"),
        ManagedParentDeleteOutcome::Preserved {
            reason: ManagedParentPreservationReason::DependentCapability,
            ..
        }
    ));
    assert_eq!(
        repository
            .load(tenant_id, dependent_id)
            .await
            .expect("dependent parent should remain readable")
            .expect("dependent parent should remain present")
            .status,
        ConnectionStatus::PendingAuth
    );

    let inflight_id = ConnectorConnectionId::new();
    let inflight_hash = "3".repeat(64);
    repository
        .claim_managed_parent(claim_request(
            tenant_id,
            inflight_id,
            "creator-with-inflight-peer",
            &inflight_hash,
            ManagedParentDefinition::KnowledgeNango,
            Some(Uuid::new_v4()),
        ))
        .await
        .expect("in-flight claim fixture should create its parent");
    insert_inflight_knowledge_claim(&pool, tenant_id, inflight_id, "other-link-operation").await;
    assert!(matches!(
        repository
            .delete_managed_parent_if_unused(delete_request(
                tenant_id,
                inflight_id,
                "creator-with-inflight-peer",
                &inflight_hash,
            ))
            .await
            .expect("another in-flight link must preserve the shared parent"),
        ManagedParentDeleteOutcome::Preserved {
            reason: ManagedParentPreservationReason::DependentCapability,
            ..
        }
    ));
}

fn claim_request(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    operation_id: &str,
    request_hash: &str,
    definition: ManagedParentDefinition,
    owner_identity_id: Option<Uuid>,
) -> ManagedParentClaimRequest {
    let display_name = match definition {
        ManagedParentDefinition::KnowledgeNango => "github",
        ManagedParentDefinition::KnowledgeMerge => "ats",
    };
    ManagedParentClaimRequest {
        tenant_id,
        operation_id: operation_id.to_string(),
        request_hash: request_hash.to_string(),
        connection_id,
        definition,
        display_name: display_name.to_string(),
        owner_identity_id,
    }
}

fn existing_managed_parent(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    definition: ManagedParentDefinition,
    provider_config_key: &str,
    provider_connection_id: &str,
    connector: &str,
) -> NewConnectorConnection {
    NewConnectorConnection {
        connection_id,
        tenant_id,
        display_name: "Renamed by operator".to_string(),
        definition_ref: definition.definition_ref(),
        origin: None,
        non_secret_config: if provider_config_key.is_empty()
            && provider_connection_id.is_empty()
            && connector.is_empty()
        {
            json!({})
        } else {
            json!({
                "provider_config_key": provider_config_key,
                "provider_connection_id": provider_connection_id,
                "connector": connector,
            })
        },
        created_by_identity_id: None,
        owner_identity_id: Uuid::new_v4(),
    }
}

fn delete_request(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    operation_id: &str,
    request_hash: &str,
) -> ManagedParentDeleteRequest {
    ManagedParentDeleteRequest {
        tenant_id,
        operation_id: operation_id.to_string(),
        request_hash: request_hash.to_string(),
        connection_id,
    }
}

fn generation(value: u64) -> ConnectionGeneration {
    ConnectionGeneration::new(value).expect("fixture generation should be positive")
}

async fn managed_claim_row(
    pool: &PgPool,
    tenant_id: TenantId,
    operation_id: &str,
) -> (String, Uuid, bool) {
    sqlx::query_as(
        "SELECT request_hash, connection_uid, parent_created_by_claim \
         FROM moa.connector_managed_parent_claims \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id.0)
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .expect("managed-parent claim should load")
}

async fn managed_claim_count(pool: &PgPool, tenant_id: TenantId, operation_id: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM moa.connector_managed_parent_claims \
         WHERE tenant_id = $1 AND operation_id = $2",
    )
    .bind(tenant_id.0)
    .bind(operation_id)
    .fetch_one(pool)
    .await
    .expect("managed-parent claim count should load")
}

async fn authz_rows(pool: &PgPool, connection_id: ConnectorConnectionId) -> Vec<AuthzTupleRow> {
    sqlx::query_as(
        "SELECT op, tuple_user, tuple_relation, tuple_object, model_version, generation, status, \
         tenant_id FROM authz_outbox WHERE tuple_object = $1 \
         ORDER BY tuple_relation, op, tuple_user",
    )
    .bind(format!("connector_connection:{connection_id}"))
    .fetch_all(pool)
    .await
    .expect("managed-parent authorization tuples should load")
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
    .expect("managed-parent binding count should load")
}

async fn insert_action_dependent(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) {
    sqlx::query(
        "INSERT INTO moa.connector_action_bindings (binding_uid, tenant_id, connection_uid, \
         action_id, connection_generation, compiled_contract, contract_hash, \
         governed_contract_revision, minimum_effect, enabled) \
         VALUES ($1,$2,$3,'dependent_action',1,'{}'::JSONB,$4,'dependent/v1','allow',FALSE)",
    )
    .bind(Uuid::new_v4())
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .bind("0".repeat(64))
    .execute(pool)
    .await
    .expect("action-dependent fixture should insert");
}

async fn insert_inflight_knowledge_claim(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    operation_id: &str,
) {
    sqlx::query(
        "INSERT INTO moa.knowledge_link_claims (tenant_id, operation_id, request_hash, \
         owner_identity_id, connection_uid, parent_created_by_claim, state) \
         VALUES ($1,$2,$3,$4,$5,FALSE,'reserved')",
    )
    .bind(tenant_id.0)
    .bind(operation_id)
    .bind("9".repeat(64))
    .bind(Uuid::new_v4())
    .bind(connection_id.0)
    .execute(pool)
    .await
    .expect("in-flight knowledge-link claim should insert");
}

async fn set_connection_active(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) {
    sqlx::query(
        "UPDATE moa.connector_connections SET lifecycle_status = 'active' \
         WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .execute(pool)
    .await
    .expect("pre-existing managed-parent fixture should become active");
}
