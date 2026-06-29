//! Integration coverage for atomic graph-memory write modes.

use std::sync::Arc;

use chrono::{Duration, Utc};
use moa_core::RlsContext;
use moa_core::TenantId;
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
    PostgresGraphStore,
};
use moa_memory_vector::{PgvectorStore, VectorQuery, VectorStore};
use moa_session::testing;
use serde_json::json;
use sqlx::{PgPool, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

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

fn tenant_scope(storage_partition_id: impl AsRef<str>) -> RlsContext {
    let storage_partition_id = storage_partition_id.as_ref();
    let tenant_id = Uuid::parse_str(storage_partition_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id)));
    RlsContext::tenant(tenant_id)
}

fn stable_uuid_from_label(label: &str) -> Uuid {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.as_bytes().iter().copied().enumerate() {
        let slot = index % 16;
        bytes[slot] = bytes[slot]
            .wrapping_mul(31)
            .wrapping_add(byte)
            .wrapping_add(index as u8);
        let mirror = (index * 7 + 3) % 16;
        bytes[mirror] ^= byte.rotate_left((index % 8) as u32);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; 1024];
    vector[index % 1024] = 1.0;
    vector
}

fn graph_store(pool: &PgPool, storage_partition_id: &str) -> PostgresGraphStore {
    let scope = tenant_scope(storage_partition_id);
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    PostgresGraphStore::scoped_for_app_role(pool.clone(), scope).with_vector_store(Arc::new(vector))
}

fn node_intent(
    storage_partition_id: &str,
    label: NodeLabel,
    name: &str,
    valid_from: chrono::DateTime<Utc>,
    embedding: Option<Vec<f32>>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "write_protocol" }),
        pii_class: PiiClass::None,
        confidence: Some(0.9),
        valid_from,
        embedding,
        embedding_model: Some("test-model".to_string()),
        embedding_model_version: Some(1),
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

fn edge_intent(
    storage_partition_id: &str,
    label: EdgeLabel,
    start_uid: Uuid,
    end_uid: Uuid,
    index: usize,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        start_uid,
        end_uid,
        properties: json!({ "kind": "test-edge", "index": index }),
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn scoped_conn<'a>(pool: &'a PgPool, storage_partition_id: &str) -> ScopedConn<'a> {
    let ctx = tenant_scope(storage_partition_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin scoped test transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

async fn workspace_version(pool: &PgPool, storage_partition_id: &str) -> i64 {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT changelog_version FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id)
    .fetch_one(conn.as_mut())
    .await
    .expect("read storage_partition_state version");
    conn.commit().await.expect("commit version read");
    version
}

async fn seed_workspace_embedder_state(pool: &PgPool, storage_partition_id: &str) {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, 'test-model', 1, 1024)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(storage_partition_id)
    .execute(conn.as_mut())
    .await
    .expect("seed workspace embedder state");
    conn.commit()
        .await
        .expect("commit workspace embedder state");
}

async fn vector_count(pool: &PgPool, storage_partition_id: &str, uid: Uuid) -> i64 {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
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
    storage_partition_id: &str,
    uid: Uuid,
) -> Option<chrono::DateTime<Utc>> {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
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

async fn edge_index_row(pool: &PgPool, storage_partition_id: &str, uid: Uuid) -> EdgeIndexRow {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let row = sqlx::query(
        r#"
        SELECT uid, label, start_uid, end_uid, storage_partition_id, user_id, scope, properties
        FROM moa.edge_index
        WHERE uid = $1
        "#,
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read edge_index row by uid");
    conn.commit().await.expect("commit edge_index row read");
    EdgeIndexRow {
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
    }
}

async fn assert_edge_index_row(
    pool: &PgPool,
    storage_partition_id: &str,
    uid: Uuid,
    label: EdgeLabel,
    start_uid: Uuid,
    end_uid: Uuid,
    properties: serde_json::Value,
) {
    let row = edge_index_row(pool, storage_partition_id, uid).await;
    assert_eq!(row.uid, uid);
    assert_eq!(row.label, label.as_str());
    assert_eq!(row.start_uid, start_uid);
    assert_eq!(row.end_uid, end_uid);
    assert_eq!(
        row.storage_partition_id.as_deref(),
        Some(storage_partition_id)
    );
    assert_eq!(row.user_id, None);
    assert_eq!(row.scope, "tenant");
    assert_eq!(row.properties, properties);
}

async fn assert_supersedes_edge_index_row(
    pool: &PgPool,
    storage_partition_id: &str,
    old_uid: Uuid,
    new_uid: Uuid,
) {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let edge_uid = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT uid
        FROM moa.edge_index
        WHERE label = $1
          AND start_uid = $2
          AND end_uid = $3
        "#,
    )
    .bind(EdgeLabel::Supersedes.as_str())
    .bind(new_uid)
    .bind(old_uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read SUPERSEDES edge uid");
    conn.commit().await.expect("commit edge read");
    assert_edge_index_row(
        pool,
        storage_partition_id,
        edge_uid,
        EdgeLabel::Supersedes,
        new_uid,
        old_uid,
        json!({}),
    )
    .await;
}

async fn incident_edge_count(pool: &PgPool, storage_partition_id: &str, uid: Uuid) -> i64 {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.edge_index WHERE start_uid = $1 OR end_uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("count incident edge_index rows");
    conn.commit().await.expect("commit incident edge count");
    count
}

async fn linked_supersede_rows(
    pool: &PgPool,
    storage_partition_id: &str,
    old_uid: Uuid,
    new_uid: Uuid,
) {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
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

async fn erase_payload_hash_exists(pool: &PgPool, storage_partition_id: &str, uid: Uuid) {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
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
    let storage_partition_id = Uuid::now_v7().to_string();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    seed_workspace_embedder_state(session_store.pool(), &storage_partition_id).await;

    let t0 = Utc::now() - Duration::minutes(5);
    let old = node_intent(
        &storage_partition_id,
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
        &storage_partition_id,
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
        workspace_version(session_store.pool(), &storage_partition_id).await,
        2
    );

    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    let matches = vector
        .knn(&VectorQuery {
            embedding: basis_vector(0),
            k: 1,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query created vector");
    assert_eq!(matches.first().map(|row| row.uid), Some(old_uid));

    let new = node_intent(
        &storage_partition_id,
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
        node_valid_to(session_store.pool(), &storage_partition_id, old_uid).await,
        Some(new.valid_from)
    );
    assert_eq!(
        node_valid_to(session_store.pool(), &storage_partition_id, new_uid).await,
        None
    );
    assert_eq!(
        vector_count(session_store.pool(), &storage_partition_id, old_uid).await,
        0
    );
    assert_eq!(
        vector_count(session_store.pool(), &storage_partition_id, new_uid).await,
        1
    );
    let historical_vector_matches = vector
        .knn(&VectorQuery {
            embedding: basis_vector(0),
            k: 5,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: Some(t0 + Duration::seconds(30)),
        })
        .await
        .expect("query old-window vector after supersession");
    assert!(
        historical_vector_matches
            .iter()
            .all(|row| row.uid != old_uid),
        "superseded pgvector row is deleted, so old uid should not be returned"
    );
    assert_supersedes_edge_index_row(
        session_store.pool(),
        &storage_partition_id,
        old_uid,
        new_uid,
    )
    .await;
    linked_supersede_rows(
        session_store.pool(),
        &storage_partition_id,
        old_uid,
        new_uid,
    )
    .await;
    assert_eq!(
        workspace_version(session_store.pool(), &storage_partition_id).await,
        4
    );

    let relates_edge = edge_intent(
        &storage_partition_id,
        EdgeLabel::RelatesTo,
        new_uid,
        target_uid,
        0,
    );
    let relates_edge_uid = relates_edge.uid;
    graph
        .create_edge(relates_edge.clone())
        .await
        .expect("create graph edge");
    assert_edge_index_row(
        session_store.pool(),
        &storage_partition_id,
        relates_edge_uid,
        EdgeLabel::RelatesTo,
        new_uid,
        target_uid,
        relates_edge.properties,
    )
    .await;
    assert_eq!(
        workspace_version(session_store.pool(), &storage_partition_id).await,
        5
    );

    graph
        .invalidate_node(new_uid, "write protocol invalidation")
        .await
        .expect("invalidate node");
    assert!(
        node_valid_to(session_store.pool(), &storage_partition_id, new_uid)
            .await
            .is_some()
    );
    assert_eq!(
        vector_count(session_store.pool(), &storage_partition_id, new_uid).await,
        0
    );
    assert_eq!(
        workspace_version(session_store.pool(), &storage_partition_id).await,
        6
    );
    assert_eq!(
        incident_edge_count(session_store.pool(), &storage_partition_id, new_uid).await,
        2
    );

    graph
        .hard_purge(new_uid, "redacted:test")
        .await
        .expect("hard purge node");
    assert_eq!(
        incident_edge_count(session_store.pool(), &storage_partition_id, new_uid).await,
        0
    );
    assert!(
        graph
            .get_node(new_uid)
            .await
            .expect("get purged node")
            .is_none()
    );
    erase_payload_hash_exists(session_store.pool(), &storage_partition_id, new_uid).await;
    assert_eq!(
        workspace_version(session_store.pool(), &storage_partition_id).await,
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
async fn write_protocol_creates_every_edge_label() {
    // Pins: every supported graph edge label is persisted in the relational sidecar.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    let now = Utc::now();
    let start_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Fact,
            "edge-label source",
            now,
            None,
        ))
        .await
        .expect("create source node");
    let end_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Entity,
            "edge-label target",
            now,
            None,
        ))
        .await
        .expect("create target node");
    let labels = EdgeLabel::ALL;

    for (index, label) in labels.iter().copied().enumerate() {
        let intent = edge_intent(&storage_partition_id, label, start_uid, end_uid, index);
        let expected_uid = intent.uid;
        let expected_properties = intent.properties.clone();
        let actual_uid = graph
            .create_edge(intent)
            .await
            .unwrap_or_else(|error| panic!("create {} edge: {error}", label.as_str()));
        assert_eq!(actual_uid, expected_uid);
        assert_edge_index_row(
            session_store.pool(),
            &storage_partition_id,
            expected_uid,
            label,
            start_uid,
            end_uid,
            expected_properties,
        )
        .await;
    }
    assert_eq!(
        workspace_version(session_store.pool(), &storage_partition_id).await,
        2 + labels.len() as i64
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn rollback_on_failure_removes_relational_sidecar_rows() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    seed_workspace_embedder_state(session_store.pool(), &storage_partition_id).await;
    let bad = node_intent(
        &storage_partition_id,
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

    let mut conn = scoped_conn(session_store.pool(), &storage_partition_id).await;
    let sidecar_count =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(conn.as_mut())
            .await
            .expect("count sidecar rows after rollback");
    assert_eq!(sidecar_count, 0);
    conn.commit().await.expect("commit rollback verification");
    assert!(
        graph
            .get_node(uid)
            .await
            .expect("get rolled-back node")
            .is_none()
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
