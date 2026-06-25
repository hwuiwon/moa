//! Tenant/contact RLS coverage for graph-memory writes.

use chrono::Utc;
use moa_core::{ContactId, TenantId};
use moa_db::ScopedConn;
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_memory_types::ScopeContext;
use moa_session::testing;
use serde_json::json;
use uuid::Uuid;

fn node_intent(tenant_id: TenantId, name: &str) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label: NodeLabel::Fact,
        storage_partition_id: Some(tenant_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "contact_write_db_memory" }),
        pii_class: PiiClass::None,
        confidence: Some(0.9),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "user".to_string(),
    }
}

#[tokio::test]
#[ignore]
async fn contact_scoped_graph_write_sets_contact_and_blocks_other_contact_db_memory() {
    // Pins: graph writes project tenant/contact IDs and contact RLS blocks forged contact rows.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated graph write store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_a = ContactId(Uuid::now_v7());
    let contact_b = ContactId(Uuid::now_v7());
    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        ScopeContext::contact(tenant_id, contact_a),
    );

    let uid = graph
        .create_node(node_intent(tenant_id, "contact A private graph fact"))
        .await
        .expect("contact-scoped graph write should succeed");

    let mut read_conn = ScopedConn::begin_contact(store.pool(), tenant_id, contact_a)
        .await
        .expect("begin contact read");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(read_conn.as_mut())
        .await
        .expect("set app role");
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
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(write_conn.as_mut())
        .await
        .expect("set app role");
    let forged = sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, tenant_id, contact_id, name, pii_class, confidence, properties_summary)
        VALUES ($1, 'Fact', $2, $3, $4, 'forged contact B fact', 'none', 0.9, $5)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(tenant_id.to_string())
    .bind(tenant_id.0)
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
