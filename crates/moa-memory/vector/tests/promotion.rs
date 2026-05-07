//! Integration coverage for workspace-to-global node promotion.

use chrono::Utc;
use moa_core::{ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_vector::{
    Error, PgvectorStore, VECTOR_DIMENSION, VectorItem, VectorStore,
    promote_workspace_node_to_global,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_TEST_POSTGRES_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    vector[index % VECTOR_DIMENSION] = 1.0;
    vector
}

fn scope(workspace_id: &str) -> ScopeContext {
    ScopeContext::workspace(WorkspaceId::new(workspace_id))
}

fn vector_item(workspace_id: &str, uid: Uuid) -> VectorItem {
    VectorItem {
        uid,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        label: "Fact".to_string(),
        pii_class: "none".to_string(),
        embedding: basis_vector(0),
        embedding_model: "test-embedder".to_string(),
        embedding_model_version: 1,
        valid_to: None,
    }
}

async fn seed_workspace_fact(test_db: &TestDb, workspace_id: &str, uid: Uuid, content: &str) {
    let mut conn = scoped_conn(test_db.store().pool(), workspace_id).await;
    sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, workspace_id, name, pii_class, confidence, valid_from, properties_summary) \
         VALUES ($1, 'Fact', $2, $3, 'none', 0.9, $4, $5)",
    )
    .bind(uid)
    .bind(workspace_id)
    .bind(content)
    .bind(Utc::now())
    .bind(json!({ "content": content }))
    .execute(conn.as_mut())
    .await
    .expect("seed workspace node_index row for promotion");
    conn.commit().await.expect("commit workspace fact seed");

    let store =
        PgvectorStore::new_for_app_role(test_db.store().pool().clone(), scope(workspace_id));
    store
        .upsert(&[vector_item(workspace_id, uid)])
        .await
        .expect("seed workspace embedding for promotion");
}

async fn scoped_conn<'a>(pool: &'a PgPool, workspace_id: &str) -> ScopedConn<'a> {
    let mut conn = ScopedConn::begin(pool, &scope(workspace_id))
        .await
        .expect("begin scoped promotion transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

#[tokio::test]
async fn promote_workspace_node_to_global_creates_global_row_with_same_uid() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("promotion-create-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_workspace_fact(&test_db, &workspace_id, uid, "promoted fact").await;

    let report = promote_workspace_node_to_global(test_db.store().pool(), &workspace_id, uid)
        .await
        .expect("promote workspace node");

    let row = sqlx::query(
        "SELECT uid, scope, workspace_id, properties_summary->>'content' AS content \
         FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(test_db.store().pool())
    .await
    .expect("read promoted global row");
    assert_eq!(report.uid, uid);
    assert_eq!(row.try_get::<Uuid, _>("uid").expect("decode uid"), uid);
    assert_eq!(
        row.try_get::<String, _>("scope").expect("decode scope"),
        "global"
    );
    assert!(
        row.try_get::<Option<String>, _>("workspace_id")
            .expect("decode workspace")
            .is_none()
    );
    assert_eq!(
        row.try_get::<String, _>("content").expect("decode content"),
        "promoted fact"
    );
}

#[tokio::test]
async fn promote_workspace_node_to_global_invalidates_workspace_row() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("promotion-invalidate-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_workspace_fact(&test_db, &workspace_id, uid, "invalidated workspace fact").await;

    let report = promote_workspace_node_to_global(test_db.store().pool(), &workspace_id, uid)
        .await
        .expect("promote workspace node");

    let workspace_rows = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.node_index WHERE uid = $1 AND workspace_id = $2",
    )
    .bind(uid)
    .bind(&workspace_id)
    .fetch_one(test_db.store().pool())
    .await
    .expect("count remaining workspace rows");
    let global_valid_from = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
        "SELECT valid_from FROM moa.node_index WHERE uid = $1 AND workspace_id IS NULL",
    )
    .bind(uid)
    .fetch_one(test_db.store().pool())
    .await
    .expect("read global valid_from");

    assert_eq!(workspace_rows, 0);
    assert_eq!(global_valid_from, report.valid_from);
}

#[tokio::test]
async fn promote_workspace_node_to_global_preserves_lineage_chain_via_supersedes_edge() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("promotion-lineage-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_workspace_fact(&test_db, &workspace_id, uid, "lineage fact").await;

    promote_workspace_node_to_global(test_db.store().pool(), &workspace_id, uid)
        .await
        .expect("promote workspace node");

    let supersede_change = sqlx::query_scalar::<_, i64>(
        "SELECT change_id FROM moa.graph_changelog \
         WHERE workspace_id = $1 AND target_uid = $2 AND op = 'supersede'",
    )
    .bind(&workspace_id)
    .bind(uid)
    .fetch_one(test_db.store().pool())
    .await
    .expect("read promotion supersede changelog row");
    let linked_global_create = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE workspace_id IS NULL AND target_uid = $1 AND op = 'create' AND cause_change_id = $2",
    )
    .bind(uid)
    .bind(supersede_change)
    .fetch_one(test_db.store().pool())
    .await
    .expect("count linked global create changelog row");

    assert_eq!(linked_global_create, 1);
}

#[tokio::test]
async fn promote_workspace_node_to_global_with_existing_global_uid_collision_returns_typed_error() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("promotion-collision-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO moa.node_index (uid, label, workspace_id, name, pii_class, properties_summary) \
         VALUES ($1, 'Fact', NULL, 'global collision fact', 'none', $2)",
    )
    .bind(uid)
    .bind(json!({ "content": "already global" }))
    .execute(test_db.store().pool())
    .await
    .expect("seed global collision row");

    let error = promote_workspace_node_to_global(test_db.store().pool(), &workspace_id, uid)
        .await
        .expect_err("global UID collision must be typed");

    assert!(matches!(error, Error::PromotionUidCollision { uid: actual } if actual == uid));
}

#[tokio::test]
async fn promote_workspace_node_visible_in_other_workspaces_after_promotion() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_a = format!("promotion-visible-a-{}", Uuid::now_v7().simple());
    let workspace_b = format!("promotion-visible-b-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_workspace_fact(&test_db, &workspace_a, uid, "visible global fact").await;

    {
        let mut conn = scoped_conn(test_db.store().pool(), &workspace_b).await;
        let before = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.node_index WHERE uid = $1 AND valid_to IS NULL",
        )
        .bind(uid)
        .fetch_one(conn.as_mut())
        .await
        .expect("count visibility before promotion");
        conn.commit().await.expect("commit before visibility read");
        assert_eq!(before, 0);
    }

    promote_workspace_node_to_global(test_db.store().pool(), &workspace_a, uid)
        .await
        .expect("promote workspace node");

    let mut conn = scoped_conn(test_db.store().pool(), &workspace_b).await;
    let after = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.node_index WHERE uid = $1 AND scope = 'global' AND valid_to IS NULL",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("count visibility after promotion");
    conn.commit().await.expect("commit after visibility read");

    assert_eq!(after, 1);
}
