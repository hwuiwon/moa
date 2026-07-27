//! Tenant/contact RLS coverage for graph-memory writes.

use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{types::contact::ContactId, types::identifiers::TenantId};
use moa_db::ScopedConn;
use moa_memory_graph::{EdgeLabel, GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_session::testing;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, PartialEq)]
struct EdgeIndexRow {
    uid: Uuid,
    label: String,
    start_uid: Uuid,
    end_uid: Uuid,
    storage_partition_id: Option<String>,
    user_id: Option<String>,
    scope: String,
    properties: serde_json::Value,
}

fn node_intent(tenant_id: TenantId, contact_id: ContactId, name: &str) -> NodeWriteIntent {
    NodeWriteIntent {
        barrier: None,
        uid: Uuid::now_v7(),
        data_subject_id: contact_id.0,
        label: NodeLabel::Fact,
        storage_partition_id: Some(
            moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id).to_string(),
        ),
        contact_id: Some(contact_id.to_string()),
        scope: "contact".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "contact_write_db_memory" }),
        pii_class: SensitivityClass::None,
        confidence: Some(0.9),
        valid_from: moa_test_support::fixtures::pg_now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "user".to_string(),
    }
}

async fn set_app_role(conn: &mut sqlx::PgConnection) {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .expect("set app role");
}

async fn set_legacy_user_id(conn: &mut sqlx::PgConnection, contact_id: ContactId) {
    sqlx::query("SELECT pg_catalog.set_config('moa.user_id', $1, true)")
        .bind(contact_id.to_string())
        .execute(conn)
        .await
        .expect("set legacy user id GUC");
}

async fn insert_contact_edge_fixture(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> (Uuid, Uuid, Uuid) {
    let storage_partition_id =
        moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id).to_string();
    let start_uid = Uuid::now_v7();
    let end_uid = Uuid::now_v7();
    let edge_uid = Uuid::now_v7();
    let mut conn = ScopedConn::begin_contact(pool, tenant_id, contact_id)
        .await
        .expect("begin contact edge fixture");
    set_app_role(conn.as_mut()).await;
    set_legacy_user_id(conn.as_mut(), contact_id).await;

    for (uid, name) in [
        (start_uid, "contact edge start"),
        (end_uid, "contact edge end"),
    ] {
        sqlx::query(
            r#"
            INSERT INTO moa.node_index
                (uid, label, storage_partition_id, user_id, tenant_id, contact_id,
                 data_subject_id, name, pii_class, confidence, properties_summary)
            VALUES ($1, 'Fact', $2, $3, $4, $5, $6, $7, 'none', 0.9, $8)
            "#,
        )
        .bind(uid)
        .bind(&storage_partition_id)
        .bind(contact_id.to_string())
        .bind(tenant_id.0)
        .bind(contact_id.0)
        .bind(contact_id.0)
        .bind(name)
        .bind(json!({ "name": name, "source": "contact_edge_rls" }))
        .execute(conn.as_mut())
        .await
        .expect("insert contact-scoped node_index fixture");
    }

    sqlx::query(
        r#"
        INSERT INTO moa.edge_index
            (uid, label, start_uid, end_uid, storage_partition_id, user_id, tenant_id, contact_id,
             properties)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        "#,
    )
    .bind(edge_uid)
    .bind(EdgeLabel::RelatesTo.as_str())
    .bind(start_uid)
    .bind(end_uid)
    .bind(&storage_partition_id)
    .bind(contact_id.to_string())
    .bind(tenant_id.0)
    .bind(contact_id.0)
    .bind(json!({ "source": "contact_edge_rls" }))
    .execute(conn.as_mut())
    .await
    .expect("insert contact-scoped edge_index fixture");

    conn.commit().await.expect("commit contact edge fixture");
    (edge_uid, start_uid, end_uid)
}

async fn visible_edge_index_row(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    edge_uid: Uuid,
) -> Option<EdgeIndexRow> {
    let mut conn = ScopedConn::begin_contact(pool, tenant_id, contact_id)
        .await
        .expect("begin contact edge read");
    set_app_role(conn.as_mut()).await;
    set_legacy_user_id(conn.as_mut(), contact_id).await;
    let row = sqlx::query(
        r#"
        SELECT uid, label, start_uid, end_uid, storage_partition_id, user_id, scope, properties
        FROM moa.edge_index
        WHERE uid = $1
        "#,
    )
    .bind(edge_uid)
    .fetch_optional(conn.as_mut())
    .await
    .expect("read contact-scoped edge_index row");
    conn.commit().await.expect("commit contact edge read");
    row.map(|row| EdgeIndexRow {
        uid: row.try_get("uid").expect("decode edge uid"),
        label: row.try_get("label").expect("decode edge label"),
        start_uid: row.try_get("start_uid").expect("decode edge start"),
        end_uid: row.try_get("end_uid").expect("decode edge end"),
        storage_partition_id: row
            .try_get("storage_partition_id")
            .expect("decode edge storage partition"),
        user_id: row.try_get("user_id").expect("decode edge user id"),
        scope: row.try_get("scope").expect("decode edge scope"),
        properties: row.try_get("properties").expect("decode edge properties"),
    })
}

#[tokio::test]
async fn contact_scoped_graph_write_sets_contact_and_blocks_other_contact_db_memory() {
    // Pins: graph writes project tenant/contact IDs and contact RLS blocks forged contact rows.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated graph write store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_a = ContactId(Uuid::now_v7());
    let contact_b = ContactId(Uuid::now_v7());
    let graph = PostgresGraphStore::scoped_for_app_role(
        store.pool().clone(),
        RlsContext::contact(tenant_id, contact_a),
        super::test_kms(),
    );

    let uid = graph
        .create_node(node_intent(
            tenant_id,
            contact_a,
            "contact A private graph fact",
        ))
        .await
        .expect("contact-scoped graph write should succeed");

    let mut read_conn = ScopedConn::begin_contact(store.pool(), tenant_id, contact_a)
        .await
        .expect("begin contact read");
    set_app_role(read_conn.as_mut()).await;
    let visible = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.node_index WHERE uid = $1 AND contact_id = $2",
    )
    .bind(uid)
    .bind(contact_a.0)
    .fetch_one(read_conn.as_mut())
    .await
    .expect("count own contact graph row");
    read_conn.commit().await.expect("commit contact read");
    assert_eq!(visible, 1);

    let mut write_conn = ScopedConn::begin_contact(store.pool(), tenant_id, contact_a)
        .await
        .expect("begin forged contact write");
    set_app_role(write_conn.as_mut()).await;
    let forged = sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, tenant_id, contact_id, data_subject_id,
             name, pii_class, confidence, properties_summary)
        VALUES ($1, 'Fact', $2, $3, $4, $5, 'forged contact B fact', 'none', 0.9, $6)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id.to_string())
    .bind(tenant_id.0)
    .bind(contact_b.0)
    .bind(contact_b.0)
    .bind(json!({ "name": "forged contact B fact" }))
    .execute(write_conn.as_mut())
    .await;
    write_conn.rollback().await.expect("rollback forged write");
    assert!(
        forged.is_err(),
        "contact-scoped app-role write must not insert another contact's row"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("cleanup isolated graph write store");
}

#[tokio::test]
async fn contact_scoped_edge_index_rls_blocks_other_contacts_and_tenants_db_memory() {
    // Pins: direct edge_index reads cannot cross contact or tenant boundaries.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated graph edge store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let other_tenant_id = TenantId::from(Uuid::now_v7());
    let contact_a = ContactId(Uuid::now_v7());
    let contact_b = ContactId(Uuid::now_v7());
    let (edge_uid, start_uid, end_uid) =
        insert_contact_edge_fixture(store.pool(), tenant_id, contact_a).await;
    let expected_storage_partition_id =
        moa_core::types::identifiers::StoragePartitionId::for_tenant(tenant_id).to_string();
    let expected_user_id = contact_a.to_string();

    let own_row = visible_edge_index_row(store.pool(), tenant_id, contact_a, edge_uid)
        .await
        .expect("own contact should see edge_index row");
    assert_eq!(own_row.uid, edge_uid);
    assert_eq!(own_row.label, EdgeLabel::RelatesTo.as_str());
    assert_eq!(own_row.start_uid, start_uid);
    assert_eq!(own_row.end_uid, end_uid);
    assert_eq!(
        own_row.storage_partition_id.as_deref(),
        Some(expected_storage_partition_id.as_str())
    );
    assert_eq!(own_row.user_id.as_deref(), Some(expected_user_id.as_str()));
    assert_eq!(own_row.scope, "contact");
    assert_eq!(own_row.properties, json!({ "source": "contact_edge_rls" }));

    assert_eq!(
        visible_edge_index_row(store.pool(), tenant_id, contact_b, edge_uid).await,
        None,
        "same-tenant different contact must not see contact edge"
    );
    assert_eq!(
        visible_edge_index_row(store.pool(), other_tenant_id, contact_a, edge_uid).await,
        None,
        "different tenant must not see contact edge"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("cleanup isolated graph edge store");
}
