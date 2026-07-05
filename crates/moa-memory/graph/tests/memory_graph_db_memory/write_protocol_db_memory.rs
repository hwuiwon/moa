//! Integration coverage for atomic graph-memory write modes.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use chrono::{Duration, Utc};
use moa_core::RlsContext;
use moa_core::TenantId;
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
    PostgresGraphStore,
};
use moa_memory_vector::{
    PgvectorStore, VectorItem, VectorPostCommitSync, VectorQuery, VectorStore,
};
use moa_session::testing;
use moa_test_support::fixtures::stable_uuid_from_label;
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

struct RecordingVectorStore {}

struct RecordingPostCommitSync {
    pool: PgPool,
    target_uid: Uuid,
    post_commit_calls: Arc<AtomicUsize>,
    visible_rows_at_sync: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl VectorStore for RecordingVectorStore {
    fn backend(&self) -> &'static str {
        "recording-test"
    }

    fn dimension(&self) -> usize {
        1024
    }

    async fn upsert(&self, _items: &[VectorItem]) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }

    async fn upsert_in_tx(
        &self,
        _conn: &mut sqlx::PgConnection,
        _items: &[VectorItem],
    ) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }

    async fn knn(
        &self,
        _query: &VectorQuery,
    ) -> Result<Vec<moa_memory_vector::VectorMatch>, moa_memory_vector::Error> {
        Ok(Vec::new())
    }

    async fn delete(&self, _uids: &[Uuid]) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }

    async fn delete_in_tx(
        &self,
        _conn: &mut sqlx::PgConnection,
        _uids: &[Uuid],
    ) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl VectorPostCommitSync for RecordingPostCommitSync {
    async fn sync_post_commit(&self) -> Result<(), moa_memory_vector::Error> {
        let visible_rows =
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE uid = $1")
                .bind(self.target_uid)
                .fetch_one(&self.pool)
                .await?;
        self.visible_rows_at_sync
            .store(visible_rows as usize, Ordering::SeqCst);
        self.post_commit_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn tenant_scope(storage_partition_id: impl AsRef<str>) -> RlsContext {
    let storage_partition_id = storage_partition_id.as_ref();
    let tenant_id = Uuid::parse_str(storage_partition_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id)));
    RlsContext::tenant(tenant_id)
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

async fn embedding_model_for_partition(pool: &PgPool, storage_partition_id: &str) -> String {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let embedding_model = sqlx::query_scalar::<_, String>(
        "SELECT embedding_model FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id)
    .fetch_one(conn.as_mut())
    .await
    .expect("read storage partition embedding model");
    conn.commit().await.expect("commit embedding model read");
    embedding_model
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

async fn node_exists(pool: &PgPool, storage_partition_id: &str, uid: Uuid) -> bool {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .fetch_one(conn.as_mut())
        .await
        .expect("count node rows");
    conn.commit().await.expect("commit node existence check");
    count > 0
}

#[tokio::test]
async fn in_conn_write_primitives_compose_in_one_transaction_db_memory() {
    // Pins: the *_in_conn write primitives batch a node create, an edge create, and
    // a supersede into one caller-owned transaction. An uncommitted transaction
    // leaves nothing behind, and a single commit persists all writes atomically.
    // Slow-path ingestion relies on this to apply a fact's writes in one tx.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, _database_url, _schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool();
    let storage_partition_id = Uuid::now_v7().to_string();
    let scope = tenant_scope(&storage_partition_id);
    let store = PostgresGraphStore::scoped_for_app_role(pool.clone(), scope);

    let t0 = Utc::now() - Duration::minutes(5);
    let fact = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        "batched fact",
        t0,
        None,
    );
    let entity = node_intent(
        &storage_partition_id,
        NodeLabel::Entity,
        "batched entity",
        t0,
        None,
    );
    let fact_uid = fact.uid;
    let entity_uid = entity.uid;
    let edge = edge_intent(
        &storage_partition_id,
        EdgeLabel::RelatesTo,
        entity_uid,
        fact_uid,
        0,
    );
    let edge_uid = edge.uid;
    let replacement = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        "batched fact v2",
        t0 + Duration::minutes(1),
        None,
    );
    let replacement_uid = replacement.uid;

    // Rollback path: writes in an uncommitted transaction persist nothing.
    {
        let mut conn = scoped_conn(pool, &storage_partition_id).await;
        moa_memory_graph::write::create_node_in_conn(&store, conn.as_mut(), fact.clone())
            .await
            .expect("create fact node in conn");
        moa_memory_graph::write::create_node_in_conn(&store, conn.as_mut(), entity.clone())
            .await
            .expect("create entity node in conn");
        moa_memory_graph::write::create_edge_in_conn(&store, conn.as_mut(), edge.clone())
            .await
            .expect("create edge in conn");
        // Dropped without commit -> transaction rolls back.
    }
    assert!(!node_exists(pool, &storage_partition_id, fact_uid).await);
    assert!(!node_exists(pool, &storage_partition_id, entity_uid).await);

    // Commit path: node + entity + edge + supersede persist atomically on one commit.
    let mut conn = scoped_conn(pool, &storage_partition_id).await;
    moa_memory_graph::write::create_node_in_conn(&store, conn.as_mut(), fact)
        .await
        .expect("create fact node");
    moa_memory_graph::write::create_node_in_conn(&store, conn.as_mut(), entity)
        .await
        .expect("create entity node");
    moa_memory_graph::write::create_edge_in_conn(&store, conn.as_mut(), edge)
        .await
        .expect("create edge");
    let new_uid = moa_memory_graph::write::supersede_node_in_conn(
        &store,
        conn.as_mut(),
        fact_uid,
        replacement,
    )
    .await
    .expect("supersede fact node in conn");
    assert_eq!(new_uid, replacement_uid);
    conn.commit().await.expect("commit batched writes");

    assert!(
        node_valid_to(pool, &storage_partition_id, fact_uid)
            .await
            .is_some(),
        "superseded fact is closed"
    );
    assert_eq!(
        node_valid_to(pool, &storage_partition_id, replacement_uid).await,
        None,
        "replacement fact is active"
    );
    assert!(node_exists(pool, &storage_partition_id, entity_uid).await);
    let edge_row = edge_index_row(pool, &storage_partition_id, edge_uid).await;
    assert_eq!(edge_row.start_uid, entity_uid);
    assert_eq!(edge_row.end_uid, fact_uid);
}

async fn changelog_node_create_count(pool: &PgPool, storage_partition_id: &str) -> i64 {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 AND op = 'create' AND target_kind = 'node'",
    )
    .bind(storage_partition_id)
    .fetch_one(conn.as_mut())
    .await
    .expect("count node create changelog rows");
    conn.commit().await.expect("commit changelog count");
    count
}

#[tokio::test]
async fn bulk_create_nodes_matches_looped_singles_including_changelog_db_memory() {
    // Pins: bulk_create_nodes writes the same node_index rows in input order, one
    // changelog row per node (so the storage-partition version bumps once per
    // node, exactly like a loop of create_node), and the same per-node vector
    // rows (item: bulk graph primitives).
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool();
    let batch_partition = Uuid::now_v7().to_string();
    let loop_partition = Uuid::now_v7().to_string();
    let batch_graph = graph_store(pool, &batch_partition);
    let loop_graph = graph_store(pool, &loop_partition);
    seed_workspace_embedder_state(pool, &batch_partition).await;
    seed_workspace_embedder_state(pool, &loop_partition).await;
    let now = Utc::now();

    let intents = |partition: &str| {
        vec![
            node_intent(
                partition,
                NodeLabel::Fact,
                "bulk parity 0",
                now,
                Some(basis_vector(0)),
            ),
            node_intent(
                partition,
                NodeLabel::Entity,
                "bulk parity 1",
                now,
                Some(basis_vector(1)),
            ),
            node_intent(partition, NodeLabel::Entity, "bulk parity 2", now, None),
        ]
    };
    let batch_intents = intents(&batch_partition);
    let loop_intents = intents(&loop_partition);
    let batch_uids = batch_intents
        .iter()
        .map(|intent| intent.uid)
        .collect::<Vec<_>>();

    let returned = batch_graph
        .bulk_create_nodes(batch_intents)
        .await
        .expect("bulk create nodes");
    assert_eq!(returned, batch_uids, "bulk returns uids in input order");
    for intent in loop_intents {
        loop_graph
            .create_node(intent)
            .await
            .expect("single create node");
    }

    // One storage-partition version bump per node in both paths.
    assert_eq!(workspace_version(pool, &batch_partition).await, 3);
    assert_eq!(workspace_version(pool, &loop_partition).await, 3);
    // Exactly one node-create changelog row per node in both paths.
    assert_eq!(changelog_node_create_count(pool, &batch_partition).await, 3);
    assert_eq!(changelog_node_create_count(pool, &loop_partition).await, 3);

    // Every bulk node is present and active.
    for uid in &batch_uids {
        assert!(node_exists(pool, &batch_partition, *uid).await);
        assert_eq!(node_valid_to(pool, &batch_partition, *uid).await, None);
    }
    // Vector rows follow the embeddings: the two embedded nodes get a row, the
    // third does not.
    assert_eq!(vector_count(pool, &batch_partition, batch_uids[0]).await, 1);
    assert_eq!(vector_count(pool, &batch_partition, batch_uids[1]).await, 1);
    assert_eq!(vector_count(pool, &batch_partition, batch_uids[2]).await, 0);

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
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
async fn graph_write_runs_vector_sync_after_commit_db_memory() {
    // Pins: external vector sync is a post-commit hook. The hook reads the node
    // through a separate connection and must see the committed graph row.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let intent = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        "post commit vector sync fact",
        Utc::now(),
        Some(basis_vector(0)),
    );
    let post_commit_calls = Arc::new(AtomicUsize::new(0));
    let visible_rows_at_sync = Arc::new(AtomicUsize::new(0));
    let vector = RecordingVectorStore {};
    let hook = RecordingPostCommitSync {
        pool: session_store.pool().clone(),
        target_uid: intent.uid,
        post_commit_calls: post_commit_calls.clone(),
        visible_rows_at_sync: visible_rows_at_sync.clone(),
    };
    let graph = PostgresGraphStore::scoped_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    )
    .with_vector_store(Arc::new(vector))
    .with_vector_post_commit_sync(Arc::new(hook));

    let uid = graph
        .create_node(intent)
        .await
        .expect("create node with vector post-commit hook");

    assert_eq!(post_commit_calls.load(Ordering::SeqCst), 1);
    assert_eq!(visible_rows_at_sync.load(Ordering::SeqCst), 1);
    assert_eq!(
        embedding_model_for_partition(session_store.pool(), &storage_partition_id).await,
        "test-model"
    );
    assert_eq!(
        node_valid_to(session_store.pool(), &storage_partition_id, uid).await,
        None
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
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
