//! Apache AGE smoke coverage for MOA's scoped Postgres transaction path.

use moa_core::{MemoryScope, ScopeContext, ScopedConn, WorkspaceId};
use moa_session::testing;
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

    let create = format!(
        "SELECT * FROM cypher('moa_graph', $$ \
         CREATE (n:Entity {{uid: '{uid}', workspace_id: '{workspace_id}', scope: 'workspace'}}) \
         RETURN n $$) AS (n agtype)"
    );
    sqlx::query(&create)
        .execute(conn.as_mut())
        .await
        .expect("create AGE vertex");

    let select = format!(
        "SELECT uid::text FROM cypher('moa_graph', $$ \
         MATCH (n:Entity {{uid: '{uid}'}}) RETURN n.uid $$) AS (uid agtype)"
    );
    let stored_uid = sqlx::query_scalar::<_, String>(&select)
        .fetch_one(conn.as_mut())
        .await
        .expect("read AGE vertex property");
    assert!(stored_uid.contains(&uid));

    conn.commit().await.expect("commit AGE transaction");
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated test schema");
}
