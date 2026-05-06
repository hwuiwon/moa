//! Read-side smoke tests for `GraphStore`.

use chrono::Utc;
use moa_core::{ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, PiiClass, cypher};
use moa_session::testing;
use serde_json::json;
use sqlx::Row;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

async fn set_app_role(conn: &mut sqlx::PgConnection) {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .expect("set moa_app role");
}

#[tokio::test]
async fn cypher_template_create_uses_bound_params() {
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-template-{run_id}");
    let uid = format!("entity-{run_id}");
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id.clone()));
    let mut conn = ScopedConn::begin(store.pool(), &ctx)
        .await
        .expect("begin scoped template transaction");
    set_app_role(conn.as_mut()).await;

    let params = json!({
        "uid": uid,
        "workspace_id": workspace_id,
        "user_id": "",
        "scope": "workspace",
        "name": "template smoke",
        "pii_class": "none",
        "valid_from": Utc::now().to_rfc3339(),
        "created_at": Utc::now().to_rfc3339(),
        "properties": { "smoke": true }
    });
    let row = cypher::node::CREATE_ENTITY
        .execute(&params)
        .fetch_one(conn.as_mut())
        .await
        .expect("create AGE node through Cypher template");
    let rendered = row
        .try_get::<String, _>(0)
        .expect("decode cypher result text");
    assert!(rendered.contains("entity-"), "{rendered}");

    conn.rollback()
        .await
        .expect("rollback template smoke transaction");
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

async fn seed_node(pool: &sqlx::PgPool, workspace_id: &str, uid: Uuid, name: &str) {
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin scoped seed transaction");
    set_app_role(conn.as_mut()).await;
    sqlx::query(
        "INSERT INTO moa.node_index \
         (uid, label, workspace_id, name, pii_class, confidence) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(uid)
    .bind(NodeLabel::Fact.as_str())
    .bind(workspace_id)
    .bind(name)
    .bind(PiiClass::None.as_str())
    .bind(0.99_f64)
    .execute(conn.as_mut())
    .await
    .expect("insert node_index seed row");
    conn.commit().await.expect("commit seed transaction");
}

async fn delete_node(pool: &sqlx::PgPool, uid: Uuid) {
    sqlx::query("DELETE FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .execute(pool)
        .await
        .expect("delete seeded node_index row");
}

#[tokio::test]
async fn read_smoke_get_node_and_lookup_seeds() {
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-read-{run_id}");
    let uid = Uuid::now_v7();
    let name = format!("auth service graph smoke {run_id}");
    seed_node(store.pool(), &workspace_id, uid, &name).await;

    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_id.clone())),
    );
    let row = graph
        .get_node(uid)
        .await
        .expect("get node through graph store")
        .expect("seeded node is visible");
    assert_eq!(row.uid, uid);
    assert_eq!(row.label, NodeLabel::Fact);
    assert_eq!(row.workspace_id.as_deref(), Some(workspace_id.as_str()));

    let seeds = graph
        .lookup_seeds("auth", 10)
        .await
        .expect("lookup lexical seeds through graph store");
    assert!(seeds.iter().any(|seed| seed.uid == uid));

    delete_node(store.pool(), uid).await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
