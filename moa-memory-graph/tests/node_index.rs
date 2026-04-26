//! Integration coverage for the `moa.node_index` sidecar table.

use moa_core::{ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_graph::{NodeLabel, PiiClass, bump_last_accessed, lookup_seed_by_name};
use moa_session::testing;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

async fn set_app_role(conn: &mut sqlx::PgConnection) {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .expect("set moa_app role");
}

async fn insert_workspace_rows(
    pool: &PgPool,
    workspace_id: &str,
    prefix: &str,
    label: NodeLabel,
    count: usize,
) -> Vec<Uuid> {
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin scoped node_index insert transaction");
    set_app_role(conn.as_mut()).await;

    let mut uids = Vec::with_capacity(count);
    for index in 0..count {
        let uid = Uuid::now_v7();
        let name = format!("{prefix} auth service {workspace_id} {index}");
        sqlx::query(
            "INSERT INTO moa.node_index \
             (uid, label, workspace_id, name, pii_class, confidence) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(uid)
        .bind(label.as_str())
        .bind(workspace_id)
        .bind(name)
        .bind(PiiClass::None.as_str())
        .bind(0.91_f64)
        .execute(conn.as_mut())
        .await
        .expect("insert node_index row");
        uids.push(uid);
    }

    conn.commit()
        .await
        .expect("commit node_index insert transaction");
    uids
}

async fn delete_prefixed_rows(pool: &PgPool, prefix: &str) {
    sqlx::query("DELETE FROM moa.node_index WHERE name LIKE $1")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .expect("delete test node_index rows");
}

#[tokio::test]
async fn node_index_rls_scopes_seed_lookup_and_bump() {
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let prefix = format!("node-index-{}", Uuid::now_v7().simple());
    let workspace_a = format!("{prefix}-a");
    let workspace_b = format!("{prefix}-b");
    let workspace_c = format!("{prefix}-c");

    let workspace_a_uids =
        insert_workspace_rows(store.pool(), &workspace_a, &prefix, NodeLabel::Fact, 40).await;
    insert_workspace_rows(store.pool(), &workspace_b, &prefix, NodeLabel::Fact, 30).await;
    insert_workspace_rows(store.pool(), &workspace_c, &prefix, NodeLabel::Entity, 30).await;

    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_a.clone()));
    let mut conn = ScopedConn::begin(store.pool(), &ctx)
        .await
        .expect("begin scoped node_index read transaction");
    set_app_role(conn.as_mut()).await;

    let visible_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE name LIKE $1")
            .bind(format!("{prefix}%"))
            .fetch_one(conn.as_mut())
            .await
            .expect("count visible node_index rows");
    assert_eq!(visible_count, 40);

    let seeds = lookup_seed_by_name(conn.as_mut(), "auth service", 10)
        .await
        .expect("lookup node_index seeds");
    assert!(!seeds.is_empty());
    assert!(seeds.iter().all(|row| row.scope == "workspace"));
    assert!(
        seeds
            .iter()
            .all(|row| row.workspace_id.as_deref() == Some(workspace_a.as_str()))
    );

    bump_last_accessed(conn.as_mut(), &workspace_a_uids[..2])
        .await
        .expect("bump node_index last_accessed_at");
    conn.commit()
        .await
        .expect("commit scoped node_index read transaction");

    delete_prefixed_rows(store.pool(), &prefix).await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn node_index_workspace_scope_query_uses_partial_index() {
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let prefix = format!("node-index-explain-{}", Uuid::now_v7().simple());
    let target_workspace = format!("{prefix}-target");
    let other_workspace = format!("{prefix}-other");
    insert_workspace_rows(
        store.pool(),
        &target_workspace,
        &prefix,
        NodeLabel::Fact,
        10,
    )
    .await;
    insert_workspace_rows(
        store.pool(),
        &other_workspace,
        &prefix,
        NodeLabel::Fact,
        500,
    )
    .await;
    sqlx::query("ANALYZE moa.node_index")
        .execute(store.pool())
        .await
        .expect("analyze node_index");

    let mut tx = store.pool().begin().await.expect("begin explain tx");
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *tx)
        .await
        .expect("disable seqscan for deterministic plan");

    let plan = sqlx::query_scalar::<_, String>(&format!(
        "EXPLAIN SELECT * FROM moa.node_index \
         WHERE workspace_id = '{target_workspace}' \
           AND scope = 'workspace' \
           AND label = 'Fact' \
           AND valid_to IS NULL"
    ))
    .fetch_all(&mut *tx)
    .await
    .expect("explain node_index workspace lookup")
    .join("\n");
    assert!(plan.contains("node_index_ws_scope_label"), "{plan}");

    tx.rollback().await.expect("rollback explain tx");
    delete_prefixed_rows(store.pool(), &prefix).await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
