//! Postgres coverage for direct connector `Use` desired-state relationships.

use moa_authz_schema::MODEL_VERSION;
use moa_connectors::Error;
use moa_connectors::domain::{ConnectionDefinitionRef, ConnectionGeneration, ConnectionStatus};
use moa_connectors::repository::{
    ConnectionRepository, ConnectionUseGrantRepository, ConnectionUseRequest, ConnectorUseSubject,
    NewConnectorConnection, PostgresConnectionRepository,
};
use moa_core::types::identifiers::{ConnectorConnectionId, TenantId};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

#[derive(Debug, Eq, PartialEq, sqlx::FromRow)]
struct UseTupleRow {
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
async fn operator_agent_and_contact_use_grants_converge_idempotently_db_memory() {
    // Pins: the closed subject variants produce exact direct `Use` tuples, while
    // same-op replays are no-ops and write/delete/write converges by generation.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector direct-Use convergence database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    create_connection(&repository, tenant_id, connection_id).await;
    let operator_id = insert_operator(&pool, tenant_id, true).await;
    let agent_id = insert_agent(&pool, tenant_id, "active").await;
    let contact_id = insert_contact(&pool, tenant_id, "verified", None).await;
    let requests = use_requests(tenant_id, connection_id, operator_id, agent_id, contact_id);

    for request in &requests {
        repository
            .grant_use(*request)
            .await
            .expect("active same-tenant subject should receive direct Use");
        repository
            .grant_use(*request)
            .await
            .expect("same direct Use grant should replay idempotently");
    }

    assert_eq!(
        grant_rows(&pool, tenant_id, connection_id).await,
        vec![
            ("agent".to_string(), agent_id),
            ("contact".to_string(), contact_id),
            ("operator".to_string(), operator_id),
        ]
    );
    assert_eq!(
        use_tuple_rows(&pool, connection_id).await,
        expected_use_rows(
            tenant_id,
            connection_id,
            "write",
            1,
            operator_id,
            agent_id,
            contact_id,
        )
    );

    for request in &requests {
        repository
            .revoke_use(*request)
            .await
            .expect("existing direct Use should revoke");
        repository
            .revoke_use(*request)
            .await
            .expect("same direct Use revoke should replay idempotently");
    }
    assert!(grant_rows(&pool, tenant_id, connection_id).await.is_empty());
    assert_eq!(
        use_tuple_rows(&pool, connection_id).await,
        expected_use_rows(
            tenant_id,
            connection_id,
            "delete",
            2,
            operator_id,
            agent_id,
            contact_id,
        )
    );

    for request in &requests {
        repository
            .grant_use(*request)
            .await
            .expect("revoked relationship should converge back to direct Use");
        repository
            .grant_use(*request)
            .await
            .expect("same restored grant should remain a no-op");
    }
    assert_eq!(
        use_tuple_rows(&pool, connection_id).await,
        expected_use_rows(
            tenant_id,
            connection_id,
            "write",
            3,
            operator_id,
            agent_id,
            contact_id,
        )
    );
}

#[tokio::test]
async fn grant_rejects_inactive_cross_tenant_and_teardown_subjects_db_memory() {
    // Pins: a grant cannot cross either tenant boundary, target an inactive
    // subject, or attach new authority after connector teardown begins.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector direct-Use validation database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    create_connection(&repository, tenant_id, connection_id).await;

    let inactive_operator = insert_operator(&pool, tenant_id, false).await;
    let inactive_agent = insert_agent(&pool, tenant_id, "suspended").await;
    let canonical_contact = insert_contact(&pool, tenant_id, "verified", None).await;
    let merged_contact = insert_contact(&pool, tenant_id, "merged", Some(canonical_contact)).await;
    for subject in [
        ConnectorUseSubject::Operator {
            id: inactive_operator,
        },
        ConnectorUseSubject::Agent { id: inactive_agent },
        ConnectorUseSubject::Contact { id: merged_contact },
    ] {
        let error = repository
            .grant_use(ConnectionUseRequest {
                tenant_id,
                connection_id,
                subject,
            })
            .await
            .expect_err("inactive same-tenant subject must not receive direct Use");
        assert!(
            matches!(
                error,
                Error::UseGrantSubjectInactive {
                    subject_kind,
                    subject_id,
                } if subject_kind == subject_kind_for(subject) && subject_id == subject_id_for(subject)
            ),
            "expected an exact inactive-subject error, observed {error:?}"
        );
    }

    let other_tenant = TenantId::new();
    let other_operator = insert_operator(&pool, other_tenant, true).await;
    let cross_subject = ConnectorUseSubject::Operator { id: other_operator };
    let cross_subject_error = repository
        .grant_use(ConnectionUseRequest {
            tenant_id,
            connection_id,
            subject: cross_subject,
        })
        .await
        .expect_err("other-tenant subject must remain invisible to validation");
    assert!(matches!(
        cross_subject_error,
        Error::UseGrantSubjectNotFound {
            subject_kind: "operator",
            subject_id,
        } if subject_id == other_operator
    ));

    let other_connection = ConnectorConnectionId::new();
    create_connection(&repository, other_tenant, other_connection).await;
    let cross_connection_error = repository
        .grant_use(ConnectionUseRequest {
            tenant_id,
            connection_id: other_connection,
            subject: ConnectorUseSubject::Operator {
                id: inactive_operator,
            },
        })
        .await
        .expect_err("other-tenant connection must remain hidden by RLS");
    assert!(matches!(
        cross_connection_error,
        Error::ConnectionNotFound { connection_id } if connection_id == other_connection
    ));

    let active_operator = insert_operator(&pool, tenant_id, true).await;
    let deleted_connection = ConnectorConnectionId::new();
    create_connection(&repository, tenant_id, deleted_connection).await;
    repository
        .transition(
            tenant_id,
            deleted_connection,
            generation(1),
            ConnectionStatus::Deleted,
        )
        .await
        .expect("pending fixture should enter retained deleted state");
    assert_teardown_rejects_grant(
        &repository,
        tenant_id,
        deleted_connection,
        ConnectionStatus::Deleted,
        active_operator,
    )
    .await;

    let disconnecting_connection = ConnectorConnectionId::new();
    create_connection(&repository, tenant_id, disconnecting_connection).await;
    sqlx::query(
        "UPDATE moa.connector_connections SET lifecycle_status = 'disconnecting' \
         WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(disconnecting_connection.0)
    .execute(&pool)
    .await
    .expect("fixture connection should enter disconnecting state");
    assert_teardown_rejects_grant(
        &repository,
        tenant_id,
        disconnecting_connection,
        ConnectionStatus::Disconnecting,
        active_operator,
    )
    .await;

    assert!(grant_rows(&pool, tenant_id, connection_id).await.is_empty());
}

#[tokio::test]
async fn revoke_accepts_inactive_same_tenant_but_rejects_foreign_subject_db_memory() {
    // Pins: cleanup remains possible after a principal is disabled, but revoke
    // still proves the subject belongs to the connection tenant.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector direct-Use revoke validation database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    create_connection(&repository, tenant_id, connection_id).await;
    let operator_id = insert_operator(&pool, tenant_id, true).await;
    let request = ConnectionUseRequest {
        tenant_id,
        connection_id,
        subject: ConnectorUseSubject::Operator { id: operator_id },
    };
    repository
        .grant_use(request)
        .await
        .expect("active operator should receive fixture direct Use");
    sqlx::query("UPDATE public.users SET active = FALSE WHERE tenant_id = $1 AND id = $2")
        .bind(tenant_id.0)
        .bind(operator_id)
        .execute(&pool)
        .await
        .expect("fixture operator should become inactive");
    repository
        .revoke_use(request)
        .await
        .expect("inactive same-tenant operator must remain revocable");
    assert!(grant_rows(&pool, tenant_id, connection_id).await.is_empty());
    assert_eq!(
        use_tuple_rows(&pool, connection_id).await,
        vec![UseTupleRow {
            op: "delete".to_string(),
            tuple_user: format!("operator:{operator_id}"),
            tuple_relation: "use".to_string(),
            tuple_object: format!("connector_connection:{connection_id}"),
            model_version: MODEL_VERSION as i32,
            generation: 2,
            status: "pending".to_string(),
            tenant_id: tenant_id.0,
        }]
    );

    let other_tenant = TenantId::new();
    let other_operator = insert_operator(&pool, other_tenant, true).await;
    let error = repository
        .revoke_use(ConnectionUseRequest {
            tenant_id,
            connection_id,
            subject: ConnectorUseSubject::Operator { id: other_operator },
        })
        .await
        .expect_err("other-tenant subject must not be accepted by revoke");
    assert!(matches!(
        error,
        Error::UseGrantSubjectNotFound {
            subject_kind: "operator",
            subject_id,
        } if subject_id == other_operator
    ));
}

#[tokio::test]
async fn deleted_transition_inverts_all_direct_use_grants_atomically_db_memory() {
    // Pins: retained deletion removes the complete local desired-state registry
    // and atomically publishes tenant, owner, and deterministically loaded Use inverses.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector direct-Use deletion database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let owner_id = Uuid::new_v4();
    repository
        .create(new_connection(tenant_id, connection_id, owner_id))
        .await
        .expect("deletion fixture connection should be created");
    let operator_id = insert_operator(&pool, tenant_id, true).await;
    let agent_id = insert_agent(&pool, tenant_id, "active").await;
    let contact_id = insert_contact(&pool, tenant_id, "verified", None).await;
    for request in use_requests(tenant_id, connection_id, operator_id, agent_id, contact_id) {
        repository
            .grant_use(request)
            .await
            .expect("deletion fixture direct Use should be registered");
    }

    let deleted = repository
        .transition(
            tenant_id,
            connection_id,
            generation(1),
            ConnectionStatus::Deleted,
        )
        .await
        .expect("pending connection should delete with exact relationship inverses");
    assert_eq!(deleted.status, ConnectionStatus::Deleted);
    assert!(grant_rows(&pool, tenant_id, connection_id).await.is_empty());

    let all_rows: Vec<UseTupleRow> = sqlx::query_as(
        "SELECT op, tuple_user, tuple_relation, tuple_object, model_version, generation, \
                status, tenant_id FROM authz_outbox WHERE tuple_object = $1 \
         ORDER BY tuple_relation, tuple_user",
    )
    .bind(format!("connector_connection:{connection_id}"))
    .fetch_all(&pool)
    .await
    .expect("all connector authorization inverses should load");
    assert_eq!(
        all_rows,
        vec![
            expected_tuple(
                tenant_id,
                connection_id,
                "delete",
                format!("operator:{owner_id}"),
                "owner",
                2,
            ),
            expected_tuple(
                tenant_id,
                connection_id,
                "delete",
                format!("tenant:{tenant_id}"),
                "tenant",
                2,
            ),
            expected_tuple(
                tenant_id,
                connection_id,
                "delete",
                format!("agent:{agent_id}"),
                "use",
                2,
            ),
            expected_tuple(
                tenant_id,
                connection_id,
                "delete",
                format!("contact:{contact_id}"),
                "use",
                2,
            ),
            expected_tuple(
                tenant_id,
                connection_id,
                "delete",
                format!("operator:{operator_id}"),
                "use",
                2,
            ),
        ]
    );
}

#[tokio::test]
async fn outbox_failure_rolls_back_direct_use_registry_db_memory() {
    // Pins: registry desired state never commits unless the matching OpenFGA
    // desired-state enqueue commits in the same transaction.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector direct-Use rollback database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    create_connection(&repository, tenant_id, connection_id).await;
    let operator_id = insert_operator(&pool, tenant_id, true).await;
    install_use_outbox_rejection(&pool).await;

    let error = repository
        .grant_use(ConnectionUseRequest {
            tenant_id,
            connection_id,
            subject: ConnectorUseSubject::Operator { id: operator_id },
        })
        .await
        .expect_err("outbox failure must fail the complete direct Use transaction");
    assert!(
        matches!(error, Error::Authorization(_)),
        "expected typed authorization enqueue failure, observed {error:?}"
    );
    assert!(grant_rows(&pool, tenant_id, connection_id).await.is_empty());
    assert!(use_tuple_rows(&pool, connection_id).await.is_empty());
    let connection_tuple_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM authz_outbox WHERE tuple_object = $1")
            .bind(format!("connector_connection:{connection_id}"))
            .fetch_one(&pool)
            .await
            .expect("connection tuple count should load after failed grant");
    assert_eq!(connection_tuple_count, 2);
}

#[tokio::test]
async fn inverse_outbox_failure_rolls_back_deletion_and_registry_cleanup_db_memory() {
    // Pins: a failed direct-Use inverse rolls back the lifecycle edge, registry
    // removal, and earlier tenant/owner inverse upserts as one atomic deletion.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector direct-Use inverse rollback database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    let owner_id = Uuid::new_v4();
    repository
        .create(new_connection(tenant_id, connection_id, owner_id))
        .await
        .expect("inverse rollback fixture connection should be created");
    let operator_id = insert_operator(&pool, tenant_id, true).await;
    repository
        .grant_use(ConnectionUseRequest {
            tenant_id,
            connection_id,
            subject: ConnectorUseSubject::Operator { id: operator_id },
        })
        .await
        .expect("inverse rollback fixture direct Use should be registered");
    install_use_outbox_rejection(&pool).await;

    let error = repository
        .transition(
            tenant_id,
            connection_id,
            generation(1),
            ConnectionStatus::Deleted,
        )
        .await
        .expect_err("failed Use inverse must abort the complete deletion transaction");
    assert!(
        matches!(error, Error::Authorization(_)),
        "expected typed authorization enqueue failure, observed {error:?}"
    );
    let retained = repository
        .load(tenant_id, connection_id)
        .await
        .expect("connection should remain readable after rolled-back deletion")
        .expect("rolled-back deletion must retain the connection");
    assert_eq!(retained.status, ConnectionStatus::PendingAuth);
    assert_eq!(
        grant_rows(&pool, tenant_id, connection_id).await,
        vec![("operator".to_string(), operator_id)]
    );
    let all_rows: Vec<UseTupleRow> = sqlx::query_as(
        "SELECT op, tuple_user, tuple_relation, tuple_object, model_version, generation, \
                status, tenant_id FROM authz_outbox WHERE tuple_object = $1 \
         ORDER BY tuple_relation, tuple_user",
    )
    .bind(format!("connector_connection:{connection_id}"))
    .fetch_all(&pool)
    .await
    .expect("rolled-back authorization desired state should load");
    assert_eq!(
        all_rows,
        vec![
            expected_tuple(
                tenant_id,
                connection_id,
                "write",
                format!("operator:{owner_id}"),
                "owner",
                1,
            ),
            expected_tuple(
                tenant_id,
                connection_id,
                "write",
                format!("tenant:{tenant_id}"),
                "tenant",
                1,
            ),
            expected_tuple(
                tenant_id,
                connection_id,
                "write",
                format!("operator:{operator_id}"),
                "use",
                1,
            ),
        ]
    );
}

#[tokio::test]
async fn physical_connection_and_subject_deletion_require_grant_cleanup_db_memory() {
    // Pins: database foreign keys prevent bypassing inverse-tuple cleanup by
    // physically deleting either side while a registry relationship survives.
    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("bootstrap connector direct-Use foreign-key database");
    let pool = test_db.store().pool().clone();
    let repository = PostgresConnectionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let connection_id = ConnectorConnectionId::new();
    create_connection(&repository, tenant_id, connection_id).await;
    let operator_id = insert_operator(&pool, tenant_id, true).await;
    let agent_id = insert_agent(&pool, tenant_id, "active").await;
    let contact_id = insert_contact(&pool, tenant_id, "verified", None).await;
    for request in use_requests(tenant_id, connection_id, operator_id, agent_id, contact_id) {
        repository
            .grant_use(request)
            .await
            .expect("foreign-key fixture direct Use should be registered");
    }

    let connection_error = sqlx::query(
        "DELETE FROM moa.connector_connections WHERE tenant_id = $1 AND connection_uid = $2",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .execute(&pool)
    .await
    .expect_err("physical connection deletion must wait for inverse cleanup");
    assert_eq!(
        constraint_name(&connection_error),
        Some("connector_connection_use_grants_connection_fk")
    );

    for (table, subject_id, expected_constraint) in [
        (
            "users",
            operator_id,
            "connector_connection_use_grants_operator_fk",
        ),
        (
            "agents",
            agent_id,
            "connector_connection_use_grants_agent_fk",
        ),
        (
            "contacts",
            contact_id,
            "connector_connection_use_grants_contact_fk",
        ),
    ] {
        let statement = format!("DELETE FROM public.{table} WHERE id = $1");
        let error = sqlx::query(&statement)
            .bind(subject_id)
            .execute(&pool)
            .await
            .expect_err("physical subject deletion must wait for inverse cleanup");
        assert_eq!(constraint_name(&error), Some(expected_constraint));
    }
    assert_eq!(grant_rows(&pool, tenant_id, connection_id).await.len(), 3);
}

async fn create_connection(
    repository: &PostgresConnectionRepository,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) {
    repository
        .create(new_connection(tenant_id, connection_id, Uuid::new_v4()))
        .await
        .expect("connector direct-Use fixture connection should be created");
}

fn new_connection(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    owner_identity_id: Uuid,
) -> NewConnectorConnection {
    NewConnectorConnection {
        connection_id,
        tenant_id,
        display_name: "Direct Use fixture".to_string(),
        definition_ref: ConnectionDefinitionRef::built_in("direct-use-fixture", 1)
            .expect("fixture built-in definition should be valid"),
        non_secret_config: json!({}),
        created_by_identity_id: None,
        owner_identity_id,
    }
}

async fn insert_operator(pool: &PgPool, tenant_id: TenantId, active: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO public.users (id, tenant_id, email, active) VALUES ($1, $2, $3, $4)")
        .bind(id)
        .bind(tenant_id.0)
        .bind(format!("connector-use-{id}@example.test"))
        .bind(active)
        .execute(pool)
        .await
        .expect("operator fixture should insert");
    id
}

async fn insert_agent(pool: &PgPool, tenant_id: TenantId, status: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO public.agents (id, tenant_id, display_name, status) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(tenant_id.0)
    .bind(format!("connector-use-agent-{id}"))
    .bind(status)
    .execute(pool)
    .await
    .expect("agent fixture should insert");
    id
}

async fn insert_contact(
    pool: &PgPool,
    tenant_id: TenantId,
    state: &str,
    canonical_contact_id: Option<Uuid>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO public.contacts \
         (id, contact_id, tenant_id, storage_partition_id, state, canonical_contact_id) \
         VALUES ($1, $1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(tenant_id.0)
    .bind(tenant_id.to_string())
    .bind(state)
    .bind(canonical_contact_id)
    .execute(pool)
    .await
    .expect("contact fixture should insert");
    id
}

async fn install_use_outbox_rejection(pool: &PgPool) {
    sqlx::query(
        "CREATE FUNCTION reject_test_connector_use_outbox() RETURNS TRIGGER \
         LANGUAGE plpgsql AS $$ BEGIN \
             IF NEW.tuple_relation = 'use' THEN \
                 RAISE EXCEPTION 'test direct Use outbox rejection'; \
             END IF; \
             RETURN NEW; \
         END $$",
    )
    .execute(pool)
    .await
    .expect("test-only outbox rejection function should install");
    sqlx::query(
        "CREATE TRIGGER reject_test_connector_use_outbox \
         BEFORE INSERT OR UPDATE ON authz_outbox FOR EACH ROW \
         EXECUTE FUNCTION reject_test_connector_use_outbox()",
    )
    .execute(pool)
    .await
    .expect("test-only outbox rejection trigger should install");
}

fn use_requests(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    operator_id: Uuid,
    agent_id: Uuid,
    contact_id: Uuid,
) -> [ConnectionUseRequest; 3] {
    [
        ConnectionUseRequest {
            tenant_id,
            connection_id,
            subject: ConnectorUseSubject::Operator { id: operator_id },
        },
        ConnectionUseRequest {
            tenant_id,
            connection_id,
            subject: ConnectorUseSubject::Agent { id: agent_id },
        },
        ConnectionUseRequest {
            tenant_id,
            connection_id,
            subject: ConnectorUseSubject::Contact { id: contact_id },
        },
    ]
}

async fn assert_teardown_rejects_grant(
    repository: &PostgresConnectionRepository,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    status: ConnectionStatus,
    operator_id: Uuid,
) {
    let error = repository
        .grant_use(ConnectionUseRequest {
            tenant_id,
            connection_id,
            subject: ConnectorUseSubject::Operator { id: operator_id },
        })
        .await
        .expect_err("teardown connection must reject a new direct Use grant");
    assert!(matches!(
        error,
        Error::UseGrantConnectionUnavailable {
            connection_id: actual_connection,
            status: actual_status,
        } if actual_connection == connection_id && actual_status == status
    ));
}

fn subject_kind_for(subject: ConnectorUseSubject) -> &'static str {
    match subject {
        ConnectorUseSubject::Operator { .. } => "operator",
        ConnectorUseSubject::Agent { .. } => "agent",
        ConnectorUseSubject::Contact { .. } => "contact",
    }
}

fn subject_id_for(subject: ConnectorUseSubject) -> Uuid {
    match subject {
        ConnectorUseSubject::Operator { id }
        | ConnectorUseSubject::Agent { id }
        | ConnectorUseSubject::Contact { id } => id,
    }
}

async fn grant_rows(
    pool: &PgPool,
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
) -> Vec<(String, Uuid)> {
    sqlx::query_as(
        "SELECT subject_kind, subject_id FROM moa.connector_connection_use_grants \
         WHERE tenant_id = $1 AND connection_uid = $2 ORDER BY subject_kind, subject_id",
    )
    .bind(tenant_id.0)
    .bind(connection_id.0)
    .fetch_all(pool)
    .await
    .expect("direct Use registry rows should load")
}

async fn use_tuple_rows(pool: &PgPool, connection_id: ConnectorConnectionId) -> Vec<UseTupleRow> {
    sqlx::query_as(
        "SELECT op, tuple_user, tuple_relation, tuple_object, model_version, generation, status, \
                tenant_id FROM authz_outbox WHERE tuple_object = $1 AND tuple_relation = 'use' \
         ORDER BY tuple_user",
    )
    .bind(format!("connector_connection:{connection_id}"))
    .fetch_all(pool)
    .await
    .expect("direct Use authorization rows should load")
}

fn expected_use_rows(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    op: &str,
    generation: i64,
    operator_id: Uuid,
    agent_id: Uuid,
    contact_id: Uuid,
) -> Vec<UseTupleRow> {
    vec![
        expected_tuple(
            tenant_id,
            connection_id,
            op,
            format!("agent:{agent_id}"),
            "use",
            generation,
        ),
        expected_tuple(
            tenant_id,
            connection_id,
            op,
            format!("contact:{contact_id}"),
            "use",
            generation,
        ),
        expected_tuple(
            tenant_id,
            connection_id,
            op,
            format!("operator:{operator_id}"),
            "use",
            generation,
        ),
    ]
}

fn expected_tuple(
    tenant_id: TenantId,
    connection_id: ConnectorConnectionId,
    op: &str,
    tuple_user: String,
    relation: &str,
    generation: i64,
) -> UseTupleRow {
    UseTupleRow {
        op: op.to_string(),
        tuple_user,
        tuple_relation: relation.to_string(),
        tuple_object: format!("connector_connection:{connection_id}"),
        model_version: MODEL_VERSION as i32,
        generation,
        status: "pending".to_string(),
        tenant_id: tenant_id.0,
    }
}

fn constraint_name(error: &sqlx::Error) -> Option<&str> {
    error
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::constraint)
}

fn generation(value: u64) -> ConnectionGeneration {
    ConnectionGeneration::new(value).expect("fixture generation should be positive")
}
