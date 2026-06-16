//! Integration tests for the pgvector `halfvec(1024)` graph-memory store.

use chrono::{DateTime, Utc};
use moa_core::{ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_vector::{PgvectorStore, VectorItem, VectorQuery, VectorStore};
use moa_session::testing;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn basis_vector(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; 1024];
    vector[index % 1024] = 1.0;
    vector
}

fn vector_item(uid: Uuid, workspace_id: &str, label: &str, embedding: Vec<f32>) -> VectorItem {
    VectorItem {
        uid,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        label: label.to_string(),
        pii_class: "none".to_string(),
        embedding,
        embedding_model: "test-model".to_string(),
        embedding_model_version: 1,
        valid_to: None,
    }
}

async fn set_app_role(conn: &mut sqlx::PgConnection) {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .expect("set moa_app role");
}

async fn insert_node_index_rows(pool: &PgPool, workspace_id: &str, items: &[VectorItem]) {
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin node_index seed transaction");
    set_app_role(conn.as_mut()).await;

    for item in items {
        sqlx::query(
            "INSERT INTO moa.node_index (uid, label, workspace_id, name, pii_class) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(item.uid)
        .bind(&item.label)
        .bind(workspace_id)
        .bind(format!("vector seed {}", item.uid))
        .bind(&item.pii_class)
        .execute(conn.as_mut())
        .await
        .expect("insert node_index seed row");
    }

    conn.commit()
        .await
        .expect("commit node_index seed transaction");
}

async fn insert_node_index_row_with_validity(
    pool: &PgPool,
    workspace_id: &str,
    item: &VectorItem,
    valid_from: DateTime<Utc>,
    valid_to: Option<DateTime<Utc>>,
) {
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin historical node_index seed transaction");
    set_app_role(conn.as_mut()).await;

    sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, workspace_id, name, pii_class, valid_from, valid_to) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(item.uid)
    .bind(&item.label)
    .bind(workspace_id)
    .bind(format!("historical vector seed {}", item.uid))
    .bind(&item.pii_class)
    .bind(valid_from)
    .bind(valid_to)
    .execute(conn.as_mut())
    .await
    .expect("insert historical node_index seed row");

    conn.commit()
        .await
        .expect("commit historical node_index seed transaction");
}

async fn delete_node_index_rows(pool: &PgPool, uids: &[Uuid]) {
    sqlx::query("DELETE FROM moa.node_index WHERE uid = ANY($1)")
        .bind(uids)
        .execute(pool)
        .await
        .expect("delete node_index seed rows");
}

#[tokio::test]
async fn pgvector_round_trip_returns_identical_seed_first() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = format!("vector-round-trip-{}", Uuid::now_v7().simple());
    let items: Vec<_> = (0..100)
        .map(|index| vector_item(Uuid::now_v7(), &workspace_id, "Fact", basis_vector(index)))
        .collect();
    let uids: Vec<_> = items.iter().map(|item| item.uid).collect();
    insert_node_index_rows(session_store.pool(), &workspace_id, &items).await;

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_id.clone())),
    );
    store.upsert(&items).await.expect("upsert vectors");

    let seed = &items[42];
    let matches = store
        .knn(&VectorQuery {
            workspace_id: Some(workspace_id.clone()),
            embedding: seed.embedding.clone(),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query KNN");
    assert_eq!(matches.len(), 10);
    assert_eq!(matches[0].uid, seed.uid);
    assert!(matches[0].score > 0.99, "score={}", matches[0].score);

    store.delete(&uids).await.expect("delete vectors");
    delete_node_index_rows(session_store.pool(), &uids).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn cross_tenant_knn_cannot_see_other_workspace_vectors() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_a = format!("vector-tenant-a-{}", Uuid::now_v7().simple());
    let workspace_b = format!("vector-tenant-b-{}", Uuid::now_v7().simple());
    let item_a = vector_item(Uuid::now_v7(), &workspace_a, "Fact", basis_vector(0));
    insert_node_index_rows(
        session_store.pool(),
        &workspace_a,
        std::slice::from_ref(&item_a),
    )
    .await;

    let store_a = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_a.clone())),
    );
    store_a
        .upsert(std::slice::from_ref(&item_a))
        .await
        .expect("upsert workspace A vector");

    let store_b = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_b.clone())),
    );
    let matches = store_b
        .knn(&VectorQuery {
            workspace_id: Some(workspace_b),
            embedding: item_a.embedding.clone(),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query workspace B KNN");
    assert!(matches.is_empty(), "{matches:?}");

    delete_node_index_rows(session_store.pool(), &[item_a.uid]).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn pgvector_as_of_filters_by_node_index_validity_window() {
    // Pins: pgvector historical queries use node_index valid_from/valid_to, not active rows only.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = format!("vector-as-of-{}", Uuid::now_v7().simple());
    let old_valid_from = utc("2026-02-01T00:00:00Z");
    let new_valid_from = utc("2026-04-01T00:00:00Z");
    let old = VectorItem {
        valid_to: Some(new_valid_from),
        ..vector_item(Uuid::now_v7(), &workspace_id, "Fact", basis_vector(0))
    };
    let new = vector_item(Uuid::now_v7(), &workspace_id, "Fact", basis_vector(1));

    insert_node_index_row_with_validity(
        session_store.pool(),
        &workspace_id,
        &old,
        old_valid_from,
        Some(new_valid_from),
    )
    .await;
    insert_node_index_row_with_validity(
        session_store.pool(),
        &workspace_id,
        &new,
        new_valid_from,
        None,
    )
    .await;

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_id.clone())),
    );
    store
        .upsert(&[old.clone(), new.clone()])
        .await
        .expect("upsert historical vectors");

    let matches = store
        .knn(&VectorQuery {
            workspace_id: Some(workspace_id.clone()),
            embedding: old.embedding.clone(),
            k: 5,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: "restricted".to_string(),
            include_global: false,
            as_of: Some(utc("2026-03-01T00:00:00Z")),
        })
        .await
        .expect("query historical KNN");

    assert_eq!(matches.first().map(|row| row.uid), Some(old.uid));

    store
        .delete(&[old.uid, new.uid])
        .await
        .expect("delete historical vectors");
    delete_node_index_rows(session_store.pool(), &[old.uid, new.uid]).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("test timestamp should be valid RFC3339")
        .with_timezone(&Utc)
}
