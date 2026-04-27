//! Integration coverage for atomic graph-memory write modes.

use std::sync::Arc;

use chrono::{Duration, Utc};
use moa_core::{ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_graph::{
    AgeGraphStore, EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
    cypher,
};
use moa_memory_vector::{PgvectorStore, VectorQuery, VectorStore};
use moa_session::testing;
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn basis_vector(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; 1024];
    vector[index % 1024] = 1.0;
    vector
}

fn graph_store(pool: &PgPool, workspace_id: &str) -> AgeGraphStore {
    let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    AgeGraphStore::scoped_for_app_role(pool.clone(), scope).with_vector_store(Arc::new(vector))
}

fn node_intent(
    workspace_id: &str,
    label: NodeLabel,
    name: &str,
    valid_from: chrono::DateTime<Utc>,
    embedding: Option<Vec<f32>>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "write_protocol" }),
        pii_class: PiiClass::None,
        confidence: Some(0.9),
        valid_from,
        embedding,
        embedding_model: Some("test-model".to_string()),
        embedding_model_version: Some(1),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn scoped_conn<'a>(pool: &'a PgPool, workspace_id: &str) -> ScopedConn<'a> {
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin scoped test transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

async fn workspace_version(pool: &PgPool, workspace_id: &str) -> i64 {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT changelog_version FROM moa.workspace_state WHERE workspace_id = $1",
    )
    .bind(workspace_id)
    .fetch_one(conn.as_mut())
    .await
    .expect("read workspace_state version");
    conn.commit().await.expect("commit version read");
    version
}

async fn vector_count(pool: &PgPool, workspace_id: &str, uid: Uuid) -> i64 {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.embeddings WHERE uid = $1")
        .bind(uid)
        .fetch_one(conn.as_mut())
        .await
        .expect("count vector rows");
    conn.commit().await.expect("commit vector count");
    count
}

async fn node_valid_to(
    pool: &PgPool,
    workspace_id: &str,
    uid: Uuid,
) -> Option<chrono::DateTime<Utc>> {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let valid_to = sqlx::query_scalar::<_, Option<chrono::DateTime<Utc>>>(
        "SELECT valid_to FROM moa.node_index WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read node valid_to");
    conn.commit().await.expect("commit valid_to read");
    valid_to
}

async fn supersedes_edge_exists(pool: &PgPool, workspace_id: &str, old_uid: Uuid, new_uid: Uuid) {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let row = cypher::edge::SUPERSEDES_EXISTS
        .execute(&json!({
            "old_uid": old_uid.to_string(),
            "new_uid": new_uid.to_string(),
        }))
        .fetch_optional(conn.as_mut())
        .await
        .expect("query SUPERSEDES edge");
    assert!(row.is_some());
    conn.commit().await.expect("commit edge read");
}

async fn linked_supersede_rows(pool: &PgPool, workspace_id: &str, old_uid: Uuid, new_uid: Uuid) {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let old_change = sqlx::query_scalar::<_, i64>(
        "SELECT change_id FROM moa.graph_changelog \
         WHERE target_uid = $1 AND op = 'supersede'",
    )
    .bind(old_uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read old supersede changelog row");
    let linked = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE target_uid = $1 AND op = 'create' AND cause_change_id = $2",
    )
    .bind(new_uid)
    .bind(old_change)
    .fetch_one(conn.as_mut())
    .await
    .expect("read linked create changelog row");
    assert_eq!(linked, 1);
    conn.commit().await.expect("commit changelog read");
}

async fn erase_payload_hash_exists(pool: &PgPool, workspace_id: &str, uid: Uuid) {
    let mut conn = scoped_conn(pool, workspace_id).await;
    let payload = sqlx::query(
        "SELECT payload FROM moa.graph_changelog WHERE target_uid = $1 AND op = 'erase'",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read erase changelog")
    .try_get::<serde_json::Value, _>("payload")
    .expect("decode erase payload");
    assert!(payload.get("properties_hash").is_some(), "{payload}");
    conn.commit().await.expect("commit erase payload read");
}

#[tokio::test]
async fn write_protocol_exercises_create_supersede_edge_invalidate_and_purge() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = format!("write-protocol-{}", Uuid::now_v7().simple());
    let graph = graph_store(session_store.pool(), &workspace_id);

    let t0 = Utc::now() - Duration::minutes(5);
    let old = node_intent(
        &workspace_id,
        NodeLabel::Fact,
        "old write protocol fact",
        t0,
        Some(basis_vector(0)),
    );
    let old_uid = graph
        .create_node(old.clone())
        .await
        .expect("create old node");
    let target = node_intent(
        &workspace_id,
        NodeLabel::Entity,
        "target write protocol entity",
        t0,
        None,
    );
    let target_uid = graph
        .create_node(target.clone())
        .await
        .expect("create target node");
    assert_eq!(
        workspace_version(session_store.pool(), &workspace_id).await,
        2
    );

    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_id.clone())),
    );
    let matches = vector
        .knn(&VectorQuery {
            embedding: basis_vector(0),
            k: 1,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
        })
        .await
        .expect("query created vector");
    assert_eq!(matches.first().map(|row| row.uid), Some(old_uid));

    let new = node_intent(
        &workspace_id,
        NodeLabel::Fact,
        "new write protocol fact",
        t0 + Duration::minutes(1),
        Some(basis_vector(1)),
    );
    let new_uid = graph
        .supersede_node(old_uid, new.clone())
        .await
        .expect("supersede node");
    assert_eq!(
        node_valid_to(session_store.pool(), &workspace_id, old_uid).await,
        Some(new.valid_from)
    );
    assert_eq!(
        node_valid_to(session_store.pool(), &workspace_id, new_uid).await,
        None
    );
    assert_eq!(
        vector_count(session_store.pool(), &workspace_id, old_uid).await,
        0
    );
    assert_eq!(
        vector_count(session_store.pool(), &workspace_id, new_uid).await,
        1
    );
    supersedes_edge_exists(session_store.pool(), &workspace_id, old_uid, new_uid).await;
    linked_supersede_rows(session_store.pool(), &workspace_id, old_uid, new_uid).await;
    assert_eq!(
        workspace_version(session_store.pool(), &workspace_id).await,
        4
    );

    let edge = EdgeWriteIntent {
        uid: Uuid::now_v7(),
        label: EdgeLabel::RelatesTo,
        start_uid: new_uid,
        end_uid: target_uid,
        properties: json!({ "kind": "test-edge" }),
        workspace_id: Some(workspace_id.clone()),
        user_id: None,
        scope: "workspace".to_string(),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    };
    graph.create_edge(edge).await.expect("create graph edge");
    assert_eq!(
        workspace_version(session_store.pool(), &workspace_id).await,
        5
    );

    graph
        .invalidate_node(new_uid, "write protocol invalidation")
        .await
        .expect("invalidate node");
    assert!(
        node_valid_to(session_store.pool(), &workspace_id, new_uid)
            .await
            .is_some()
    );
    assert_eq!(
        vector_count(session_store.pool(), &workspace_id, new_uid).await,
        0
    );
    assert_eq!(
        workspace_version(session_store.pool(), &workspace_id).await,
        6
    );

    graph
        .hard_purge(new_uid, "redacted:test")
        .await
        .expect("hard purge node");
    assert!(
        graph
            .get_node(new_uid)
            .await
            .expect("get purged node")
            .is_none()
    );
    erase_payload_hash_exists(session_store.pool(), &workspace_id, new_uid).await;
    assert_eq!(
        workspace_version(session_store.pool(), &workspace_id).await,
        7
    );

    let _ = graph.hard_purge(old_uid, "redacted:cleanup").await;
    let _ = graph.hard_purge(target_uid, "redacted:cleanup").await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn rollback_on_failure_removes_age_and_sidecar_rows() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = format!("write-rollback-{}", Uuid::now_v7().simple());
    let graph = graph_store(session_store.pool(), &workspace_id);
    let bad = node_intent(
        &workspace_id,
        NodeLabel::Entity,
        "bad vector rollback",
        Utc::now(),
        Some(vec![1.0]),
    );
    let uid = bad.uid;

    graph
        .create_node(bad)
        .await
        .expect_err("bad vector dimension should fail create_node");

    let mut conn = scoped_conn(session_store.pool(), &workspace_id).await;
    let sidecar_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(conn.as_mut())
            .await
            .expect("count sidecar rows after rollback");
    assert_eq!(sidecar_count, 0);
    let age_row = cypher::node::GET_ENTITY_UID
        .execute(&json!({ "uid": uid.to_string() }))
        .fetch_optional(conn.as_mut())
        .await
        .expect("query AGE row after rollback");
    assert!(age_row.is_none());
    conn.commit().await.expect("commit rollback verification");

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
