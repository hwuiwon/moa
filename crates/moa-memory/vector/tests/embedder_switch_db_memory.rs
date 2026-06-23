//! Integration coverage for workspace embedder switching guards.

use moa_core::{ScopeContext, ScopedConn, TenantId};
use moa_memory_vector::{
    Error, PgvectorStore, VECTOR_DIMENSION, VectorItem, VectorQuery, VectorStore,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use sqlx::{PgPool, Row};
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn tenant_scope(workspace_id: impl AsRef<str>) -> ScopeContext {
    let workspace_id = workspace_id.as_ref();
    let tenant_id = Uuid::parse_str(workspace_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(workspace_id)));
    ScopeContext::tenant(tenant_id)
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

async fn configured_test_db() -> Option<TestDb> {
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
    tenant_scope(workspace_id)
}

fn store(test_db: &TestDb, workspace_id: &str) -> PgvectorStore {
    PgvectorStore::new_for_app_role(test_db.store().pool().clone(), scope(workspace_id))
}

fn item(
    uid: Uuid,
    workspace_id: &str,
    embedding: Vec<f32>,
    model: &str,
    version: i32,
) -> VectorItem {
    VectorItem {
        uid,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        label: "Fact".to_string(),
        pii_class: "none".to_string(),
        embedding,
        embedding_model: model.to_string(),
        embedding_model_version: version,
        valid_to: None,
    }
}

async fn scoped_conn<'a>(pool: &'a PgPool, workspace_id: &str) -> ScopedConn<'a> {
    let mut conn = ScopedConn::begin(pool, &scope(workspace_id))
        .await
        .expect("begin scoped vector transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

async fn seed_node_index(pool: &PgPool, workspace_id: &str, uid: Uuid) {
    let mut conn = scoped_conn(pool, workspace_id).await;
    sqlx::query(
        "INSERT INTO moa.node_index (uid, label, workspace_id, name, pii_class) \
         VALUES ($1, 'Fact', $2, $3, 'none')",
    )
    .bind(uid)
    .bind(workspace_id)
    .bind(format!("embedder switch {uid}"))
    .execute(conn.as_mut())
    .await
    .expect("seed node index row");
    conn.commit().await.expect("commit node seed");
}

async fn set_embedder_state(
    pool: &PgPool,
    workspace_id: &str,
    model: &str,
    dimension: i32,
    state: &str,
) {
    let mut conn = scoped_conn(pool, workspace_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.workspace_state
            (workspace_id, embedding_model, embedding_model_version, embedding_dimension, reembed_state)
        VALUES ($1, $2, 1, $3, $4)
        ON CONFLICT (workspace_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = EXCLUDED.reembed_state
        "#,
    )
    .bind(workspace_id)
    .bind(model)
    .bind(dimension)
    .bind(state)
    .execute(conn.as_mut())
    .await
    .expect("set workspace embedder state");
    conn.commit().await.expect("commit embedder state");
}

fn query(workspace_id: &str, embedding: Vec<f32>) -> VectorQuery {
    VectorQuery {
        workspace_id: Some(workspace_id.to_string()),
        embedding,
        k: 5,
        label_filter: Some(vec!["Fact".to_string()]),
        max_pii_class: "restricted".to_string(),
        include_global: false,
        as_of: None,
    }
}

#[tokio::test]
async fn switching_embedder_dimensions_blocks_knn_until_reembedded() {
    // Pins: KNN rejects a workspace whose configured embedder dimension no longer matches pgvector.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("embedder-mismatch-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_node_index(test_db.store().pool(), &workspace_id, uid).await;
    set_embedder_state(
        test_db.store().pool(),
        &workspace_id,
        "cohere-embed-v4",
        1024,
        "steady",
    )
    .await;
    let store = store(&test_db, &workspace_id);
    store
        .upsert(&[item(
            uid,
            &workspace_id,
            basis_vector(0),
            "cohere-embed-v4",
            1,
        )])
        .await
        .expect("seed vector");

    set_embedder_state(
        test_db.store().pool(),
        &workspace_id,
        "gemini-embedding-2",
        768,
        "steady",
    )
    .await;
    let error = store
        .knn(&query(&workspace_id, basis_vector(0)))
        .await
        .expect_err("dimension switch must block KNN");

    assert!(matches!(
        error,
        Error::EmbedderMismatch {
            configured_dimension: 768,
            required_dimension: VECTOR_DIMENSION,
            ..
        }
    ));
}

#[tokio::test]
async fn reembed_workspace_with_new_embedder_overwrites_existing_vectors_atomically() {
    // Pins: after explicit state migration, same-model writes can replace old vectors atomically.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("embedder-reembed-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_node_index(test_db.store().pool(), &workspace_id, uid).await;
    set_embedder_state(
        test_db.store().pool(),
        &workspace_id,
        "cohere-embed-v4",
        1024,
        "steady",
    )
    .await;
    let store = store(&test_db, &workspace_id);
    store
        .upsert(&[item(
            uid,
            &workspace_id,
            basis_vector(0),
            "cohere-embed-v4",
            1,
        )])
        .await
        .expect("seed old vector");

    set_embedder_state(
        test_db.store().pool(),
        &workspace_id,
        "replacement-embedder",
        1024,
        "steady",
    )
    .await;
    store
        .upsert(&[item(
            uid,
            &workspace_id,
            basis_vector(7),
            "replacement-embedder",
            2,
        )])
        .await
        .expect("overwrite vector with replacement embedder");

    let matches = store
        .knn(&query(&workspace_id, basis_vector(7)))
        .await
        .expect("KNN succeeds after reembed state is steady");
    assert_eq!(matches.first().map(|hit| hit.uid), Some(uid));

    let mut conn = scoped_conn(test_db.store().pool(), &workspace_id).await;
    let row = sqlx::query(
        "SELECT embedding_model, embedding_model_version FROM moa.embeddings WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read overwritten embedding metadata");
    conn.commit().await.expect("commit embedding metadata read");
    assert_eq!(
        row.try_get::<String, _>("embedding_model")
            .expect("decode model"),
        "replacement-embedder"
    );
    assert_eq!(
        row.try_get::<i32, _>("embedding_model_version")
            .expect("decode model version"),
        2
    );
}

#[tokio::test]
async fn reembed_in_progress_state_blocks_concurrent_knn_queries_until_complete() {
    // Pins: re-embedding state blocks reads even when matching vectors already exist.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("embedder-progress-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_node_index(test_db.store().pool(), &workspace_id, uid).await;
    set_embedder_state(
        test_db.store().pool(),
        &workspace_id,
        "cohere-embed-v4",
        1024,
        "steady",
    )
    .await;
    let store = store(&test_db, &workspace_id);
    store
        .upsert(&[item(
            uid,
            &workspace_id,
            basis_vector(0),
            "cohere-embed-v4",
            1,
        )])
        .await
        .expect("seed vector");
    set_embedder_state(
        test_db.store().pool(),
        &workspace_id,
        "cohere-embed-v4",
        1024,
        "in_progress",
    )
    .await;

    let error = store
        .knn(&query(&workspace_id, basis_vector(0)))
        .await
        .expect_err("in-progress reembed must block KNN");
    assert!(matches!(error, Error::ReembedInProgress { .. }));
}

#[tokio::test]
async fn configured_workspace_embedder_allows_same_model_vector_write() {
    // Pins: embedding writes succeed only after explicit workspace embedder state exists.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("embedder-same-model-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_node_index(test_db.store().pool(), &workspace_id, uid).await;
    set_embedder_state(
        test_db.store().pool(),
        &workspace_id,
        "cohere-embed-v4",
        1024,
        "steady",
    )
    .await;

    let store = store(&test_db, &workspace_id);
    store
        .upsert(&[item(
            uid,
            &workspace_id,
            basis_vector(0),
            "cohere-embed-v4",
            1,
        )])
        .await
        .expect("upsert with configured embedder");

    let mut conn = scoped_conn(test_db.store().pool(), &workspace_id).await;
    let row = sqlx::query(
        "SELECT embedding_model, embedding_model_version FROM moa.embeddings WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("read inserted embedding");
    conn.commit().await.expect("commit embedding read");

    assert_eq!(
        row.try_get::<String, _>("embedding_model")
            .expect("decode embedding model"),
        "cohere-embed-v4"
    );
    assert_eq!(
        row.try_get::<i32, _>("embedding_model_version")
            .expect("decode embedding model version"),
        1
    );
}

#[tokio::test]
async fn missing_workspace_embedder_state_rejects_vector_write() {
    // Pins: direct vector writes fail closed when workspace_state has not been explicitly seeded.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("embedder-missing-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_node_index(test_db.store().pool(), &workspace_id, uid).await;

    let store = store(&test_db, &workspace_id);
    let error = store
        .upsert(&[item(
            uid,
            &workspace_id,
            basis_vector(0),
            "cohere-embed-v4",
            1,
        )])
        .await
        .expect_err("missing workspace_state must reject vector writes");
    assert!(matches!(error, Error::WorkspaceEmbedderStateMissing { .. }));

    let mut conn = scoped_conn(test_db.store().pool(), &workspace_id).await;
    let row_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.embeddings WHERE workspace_id = $1 AND uid = $2",
    )
    .bind(&workspace_id)
    .bind(uid)
    .fetch_one(conn.as_mut())
    .await
    .expect("count rejected embedding rows");
    conn.commit().await.expect("commit rejected embedding read");
    assert_eq!(row_count, 0);
}

#[tokio::test]
async fn configured_workspace_embedder_rejects_mixed_model_vector_write() {
    // Pins: a workspace cannot mix vector spaces during embedding writes.
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("embedder-mixed-{}", Uuid::now_v7().simple());
    let uid = Uuid::now_v7();
    seed_node_index(test_db.store().pool(), &workspace_id, uid).await;
    set_embedder_state(
        test_db.store().pool(),
        &workspace_id,
        "cohere-embed-v4",
        1024,
        "steady",
    )
    .await;

    let store = store(&test_db, &workspace_id);
    let error = store
        .upsert(&[item(
            uid,
            &workspace_id,
            basis_vector(0),
            "replacement-embedder",
            1,
        )])
        .await
        .expect_err("mixed embedder model must reject vector writes");
    assert!(matches!(
        error,
        Error::EmbedderModelMismatch {
            configured_model,
            requested_model,
            ..
        } if configured_model == "cohere-embed-v4" && requested_model == "replacement-embedder"
    ));
}
