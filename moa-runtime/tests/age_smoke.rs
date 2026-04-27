//! Apache AGE smoke coverage for MOA's scoped Postgres transaction path.

use moa_core::{MemoryScope, ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_graph::cypher;
use moa_session::testing;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

#[tokio::test]
async fn age_round_trip() {
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store with AGE migrations");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = WorkspaceId::new(format!("age-smoke-{run_id}"));
    let uid = format!("entity-{run_id}");
    let ctx = ScopeContext::new(MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    });
    let mut conn = ScopedConn::begin(store.pool(), &ctx)
        .await
        .expect("begin scoped AGE transaction");

    let params = json!({
        "uid": uid,
        "workspace_id": workspace_id.to_string(),
        "user_id": "",
        "scope": "workspace",
        "name": "age smoke entity",
        "pii_class": "none",
    });
    cypher::node::CREATE_ENTITY
        .execute(&params)
        .execute(conn.as_mut())
        .await
        .expect("create AGE vertex through template");

    let stored_uid = cypher::node::GET_ENTITY_UID
        .execute(&json!({ "uid": uid }))
        .fetch_one(conn.as_mut())
        .await
        .expect("read AGE vertex property through template")
        .try_get::<String, _>(0)
        .expect("decode AGE uid text");
    assert!(stored_uid.contains(&uid));

    conn.commit().await.expect("commit AGE transaction");
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated test schema");
}
