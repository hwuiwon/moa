//! Integration tests for the pgvector `halfvec(1024)` graph-memory store.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::types::memory::RlsContext;
use moa_core::{
    types::contact::ContactId, types::identifiers::TenantId, types::security::SensitivityClass,
};
use moa_db::ScopedConn;
use moa_memory_vector::{
    PROMOTION_OVERLAP_THRESHOLD, PgvectorStore, VectorItem, VectorMatch, VectorPartitionPromotion,
    VectorQuery, VectorStore,
};
use moa_session::testing;
use moa_test_support::fixtures::stable_uuid_from_label;
use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

/// Test-only target backend whose KNN never overlaps the source, so promotion
/// validation overlap is forced to zero regardless of the source's results.
struct DisjointVectorStore;

#[async_trait::async_trait]
impl VectorStore for DisjointVectorStore {
    fn backend(&self) -> &'static str {
        "disjoint-test"
    }

    fn dimension(&self) -> usize {
        1024
    }

    async fn upsert(&self, _items: &[VectorItem]) -> Result<(), moa_memory_vector::Error> {
        Ok(())
    }

    async fn knn(
        &self,
        _query: &VectorQuery,
    ) -> Result<Vec<VectorMatch>, moa_memory_vector::Error> {
        Ok(vec![VectorMatch {
            uid: Uuid::now_v7(),
            score: 1.0,
        }])
    }

    async fn delete(&self, _uids: &[Uuid]) -> Result<(), moa_memory_vector::Error> {
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

/// Builds a 1024-dim vector spanning the first two axes so that cosine distance
/// to `basis_vector(0)` is fully determined by the `x`/`y` ratio. Values are
/// small integers that are exact in `halfvec` (fp16), so the induced ordering is
/// deterministic across runs.
fn mixed_vector(x: f32, y: f32) -> Vec<f32> {
    let mut vector = vec![0.0; 1024];
    vector[0] = x;
    vector[1] = y;
    vector
}

fn vector_item(
    uid: Uuid,
    storage_partition_id: &str,
    label: &str,
    embedding: Vec<f32>,
) -> VectorItem {
    let _ = storage_partition_id;
    VectorItem {
        uid,
        user_id: None,
        label: label.to_string(),
        pii_class: SensitivityClass::None,
        embedding,
        embedding_model: "test-model".to_string(),
        embedding_model_version: 1,
        search_text: None,
        valid_to: None,
    }
}

async fn set_app_role(conn: &mut sqlx::PgConnection) {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .expect("set moa_app role");
}

async fn insert_node_index_rows(pool: &PgPool, storage_partition_id: &str, items: &[VectorItem]) {
    let ctx = tenant_scope(storage_partition_id);
    let data_subject_id = ctx.tenant_id().0;
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin node_index seed transaction");
    set_app_role(conn.as_mut()).await;

    for item in items {
        sqlx::query(
            "INSERT INTO moa.node_index (uid, label, storage_partition_id, data_subject_id, name, pii_class) \
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(item.uid)
        .bind(&item.label)
        .bind(storage_partition_id)
        .bind(data_subject_id)
        .bind(format!("vector seed {}", item.uid))
        .bind(item.pii_class.as_str())
        .execute(conn.as_mut())
        .await
        .expect("insert node_index seed row");
    }

    conn.commit()
        .await
        .expect("commit node_index seed transaction");
}

async fn insert_contact_node_index_rows(
    pool: &PgPool,
    storage_partition_id: &str,
    contact_id: ContactId,
    items: &[VectorItem],
) {
    let tenant_id =
        Uuid::parse_str(storage_partition_id).expect("test storage partition id should be a UUID");
    let ctx = RlsContext::contact(TenantId::from(tenant_id), contact_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin contact node_index seed transaction");
    set_app_role(conn.as_mut()).await;

    for item in items {
        sqlx::query(
            "INSERT INTO moa.node_index (uid, label, storage_partition_id, data_subject_id, user_id, contact_id, name, pii_class) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(item.uid)
        .bind(&item.label)
        .bind(storage_partition_id)
        .bind(contact_id.0)
        .bind(item.user_id.as_deref())
        .bind(contact_id.0)
        .bind(format!("contact vector seed {}", item.uid))
        .bind(item.pii_class.as_str())
        .execute(conn.as_mut())
        .await
        .expect("insert contact node_index seed row");
    }

    conn.commit()
        .await
        .expect("commit contact node_index seed transaction");
}

async fn set_workspace_embedder_state(pool: &PgPool, storage_partition_id: &str, model: &str) {
    let ctx = tenant_scope(storage_partition_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin storage_partition_state seed transaction");
    set_app_role(conn.as_mut()).await;

    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, 1, 1024)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_dimension = EXCLUDED.embedding_dimension
        "#,
    )
    .bind(storage_partition_id)
    .bind(model)
    .execute(conn.as_mut())
    .await
    .expect("seed workspace embedder state");

    conn.commit()
        .await
        .expect("commit storage_partition_state seed transaction");
}

async fn insert_node_index_row_with_validity(
    pool: &PgPool,
    storage_partition_id: &str,
    item: &VectorItem,
    valid_from: DateTime<Utc>,
    valid_to: Option<DateTime<Utc>>,
) {
    let ctx = tenant_scope(storage_partition_id);
    let data_subject_id = ctx.tenant_id().0;
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin historical node_index seed transaction");
    set_app_role(conn.as_mut()).await;

    sqlx::query(
        "INSERT INTO moa.node_index \
            (uid, label, storage_partition_id, data_subject_id, name, pii_class, valid_from, valid_to) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(item.uid)
    .bind(&item.label)
    .bind(storage_partition_id)
    .bind(data_subject_id)
    .bind(format!("historical vector seed {}", item.uid))
    .bind(item.pii_class.as_str())
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
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let items: Vec<_> = (0..100)
        .map(|index| {
            vector_item(
                Uuid::now_v7(),
                &storage_partition_id,
                "Fact",
                basis_vector(index),
            )
        })
        .collect();
    let uids: Vec<_> = items.iter().map(|item| item.uid).collect();
    insert_node_index_rows(session_store.pool(), &storage_partition_id, &items).await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    store.upsert(&items).await.expect("upsert vectors");

    let seed = &items[42];
    let matches = store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                seed.embedding.clone(),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
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
async fn knn_on_partition_without_vectors_returns_no_hits_instead_of_erroring() {
    // Pins: a graph-only partition has a state row but no vector model. Reads
    // answer with zero hits instead of comparing the query against a fabricated
    // legacy model.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let scope = tenant_scope(&storage_partition_id);
    let mut conn = ScopedConn::begin(session_store.pool(), &scope)
        .await
        .expect("begin graph-only partition transaction");
    set_app_role(conn.as_mut()).await;
    sqlx::query("INSERT INTO moa.storage_partition_state (storage_partition_id) VALUES ($1)")
        .bind(&storage_partition_id)
        .execute(conn.as_mut())
        .await
        .expect("seed graph-only partition state");
    conn.commit()
        .await
        .expect("commit graph-only partition state");

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    let matches = store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                basis_vector(0),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 10,
            label_filter: None,
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        })
        .await
        .expect("graph-only partition read must not error");
    assert!(
        matches.is_empty(),
        "expected zero hits from a graph-only partition, got {}",
        matches.len()
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn preseeded_partition_pins_embedder_model_for_vector_writes() {
    // Pins: direct vector writes require a preseeded partition embedder
    // identity and reject later writes from a different model instead of
    // silently mixing vector spaces.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let item = vector_item(
        Uuid::now_v7(),
        &storage_partition_id,
        "Fact",
        basis_vector(1),
    );
    insert_node_index_rows(
        session_store.pool(),
        &storage_partition_id,
        std::slice::from_ref(&item),
    )
    .await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    store
        .upsert(std::slice::from_ref(&item))
        .await
        .expect("write with matching preseeded embedder succeeds");

    let pinned: (String, i32) = sqlx::query_as(
        "SELECT embedding_model, embedding_dimension FROM moa.storage_partition_state \
         WHERE storage_partition_id = $1",
    )
    .bind(&storage_partition_id)
    .fetch_one(session_store.pool())
    .await
    .expect("embedder state row exists after first write");
    assert_eq!(pinned.0, "test-model");
    assert_eq!(pinned.1, 1024);

    let mut foreign = vector_item(
        Uuid::now_v7(),
        &storage_partition_id,
        "Fact",
        basis_vector(2),
    );
    foreign.embedding_model = "other-model".to_string();
    let mismatch = store.upsert(&[foreign]).await;
    assert!(
        matches!(
            mismatch,
            Err(moa_memory_vector::Error::EmbedderModelMismatch { .. })
        ),
        "a different model must be rejected against the pinned identity: {mismatch:?}"
    );

    delete_node_index_rows(session_store.pool(), &[item.uid]).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn same_dimension_query_from_different_model_is_rejected_db_memory() {
    // Pins: equal vector dimensions do not make different embedding models
    // compatible; KNN must reject a query before searching the partition.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    set_workspace_embedder_state(
        session_store.pool(),
        &storage_partition_id,
        "partition-model",
    )
    .await;
    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );

    let error = store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                basis_vector(0),
                "query-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 1,
            label_filter: None,
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        })
        .await
        .expect_err("a query from another model must fail closed");

    match error {
        moa_memory_vector::Error::EmbedderModelMismatch {
            storage_partition_id: actual_partition,
            configured_model,
            requested_model,
        } => {
            assert_eq!(actual_partition, storage_partition_id);
            assert_eq!(configured_model, "partition-model");
            assert_eq!(requested_model, "query-model");
        }
        other => panic!("expected EmbedderModelMismatch, got {other:?}"),
    }

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn cross_tenant_knn_cannot_see_other_workspace_vectors() {
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_a = Uuid::now_v7().to_string();
    let workspace_b = Uuid::now_v7().to_string();
    let item_a = vector_item(Uuid::now_v7(), &workspace_a, "Fact", basis_vector(0));
    insert_node_index_rows(
        session_store.pool(),
        &workspace_a,
        std::slice::from_ref(&item_a),
    )
    .await;
    set_workspace_embedder_state(session_store.pool(), &workspace_a, "test-model").await;
    set_workspace_embedder_state(session_store.pool(), &workspace_b, "test-model").await;

    let store_a = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(workspace_a.clone()),
    );
    store_a
        .upsert(std::slice::from_ref(&item_a))
        .await
        .expect("upsert workspace A vector");

    let store_b = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(workspace_b.clone()),
    );
    let matches = store_b
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                item_a.embedding.clone(),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
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
async fn control_plane_knn_can_validate_contact_workspace_vectors() {
    // Pins: storage-partition vector promotion validates contact-owned embeddings through control-plane RLS.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let tenant_id = TenantId::from(Uuid::parse_str(&storage_partition_id).expect("workspace uuid"));
    let contact_id = ContactId::new();
    let mut item = vector_item(
        Uuid::now_v7(),
        &storage_partition_id,
        "Fact",
        basis_vector(11),
    );
    item.user_id = Some(contact_id.to_string());
    insert_contact_node_index_rows(
        session_store.pool(),
        &storage_partition_id,
        contact_id,
        std::slice::from_ref(&item),
    )
    .await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let contact_store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        RlsContext::contact(tenant_id, contact_id),
    );
    contact_store
        .upsert(std::slice::from_ref(&item))
        .await
        .expect("upsert contact vector");

    let query = VectorQuery {
        embedding: moa_memory_vector::QueryEmbedding::new(
            item.embedding.clone(),
            "test-model".to_string(),
        )
        .expect("valid query embedding"),
        k: 10,
        label_filter: Some(vec!["Fact".to_string()]),
        max_pii_class: SensitivityClass::Restricted,
        include_global: false,
        as_of: None,
    };
    let tenant_store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        RlsContext::tenant(tenant_id),
    );
    let tenant_matches = tenant_store
        .knn(&query)
        .await
        .expect("tenant-scoped query should run");
    assert_eq!(
        tenant_matches,
        Vec::new(),
        "tenant-scoped pgvector must not see contact vectors"
    );

    let control_plane_store = PgvectorStore::new_for_control_plane(
        session_store.pool().clone(),
        RlsContext::tenant(tenant_id),
    );
    let control_plane_matches = control_plane_store
        .knn(&query)
        .await
        .expect("control-plane query should run");
    assert_eq!(
        control_plane_matches.first().map(|row| row.uid),
        Some(item.uid)
    );

    control_plane_store
        .delete(&[item.uid])
        .await
        .expect("delete contact vector");
    delete_node_index_rows(session_store.pool(), &[item.uid]).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn pgvector_as_of_filters_by_node_index_validity_window() {
    // Pins: pgvector historical queries use node_index valid_from/valid_to, not active rows only.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let old_valid_from = utc("2026-02-01T00:00:00Z");
    let new_valid_from = utc("2026-04-01T00:00:00Z");
    let old = VectorItem {
        valid_to: Some(new_valid_from),
        ..vector_item(
            Uuid::now_v7(),
            &storage_partition_id,
            "Fact",
            basis_vector(0),
        )
    };
    let new = vector_item(
        Uuid::now_v7(),
        &storage_partition_id,
        "Fact",
        basis_vector(1),
    );

    insert_node_index_row_with_validity(
        session_store.pool(),
        &storage_partition_id,
        &old,
        old_valid_from,
        Some(new_valid_from),
    )
    .await;
    insert_node_index_row_with_validity(
        session_store.pool(),
        &storage_partition_id,
        &new,
        new_valid_from,
        None,
    )
    .await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    store
        .upsert(&[old.clone(), new.clone()])
        .await
        .expect("upsert historical vectors");

    let matches = store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                old.embedding.clone(),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 5,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
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

#[tokio::test]
async fn pgvector_knn_excludes_pii_vectors_under_none_ceiling() {
    // Pins the privacy ceiling at pgvector_store.rs:169-179: a query with a lower
    // `max_pii_class` must exclude PII embeddings even when they are the nearest
    // neighbors. Restricted and PHI nodes cannot have embeddings, so this test
    // exercises the complete none/PII range that vector storage accepts.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();

    let none_item = VectorItem {
        pii_class: SensitivityClass::None,
        ..vector_item(
            Uuid::now_v7(),
            &storage_partition_id,
            "Fact",
            basis_vector(0),
        )
    };
    let pii_item = VectorItem {
        pii_class: SensitivityClass::Pii,
        ..vector_item(
            Uuid::now_v7(),
            &storage_partition_id,
            "Fact",
            basis_vector(0),
        )
    };
    let items = vec![none_item.clone(), pii_item.clone()];
    let uids: Vec<_> = items.iter().map(|item| item.uid).collect();

    insert_node_index_rows(session_store.pool(), &storage_partition_id, &items).await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    store
        .upsert(&items)
        .await
        .expect("upsert mixed-pii vectors");

    let query_at = |ceiling| VectorQuery {
        embedding: moa_memory_vector::QueryEmbedding::new(
            basis_vector(0),
            "test-model".to_string(),
        )
        .expect("valid query embedding"),
        k: 10,
        label_filter: Some(vec!["Fact".to_string()]),
        max_pii_class: ceiling,
        include_global: false,
        as_of: None,
    };

    let pii_ceiling: HashSet<Uuid> = store
        .knn(&query_at(SensitivityClass::Pii))
        .await
        .expect("pii-ceiling query")
        .into_iter()
        .map(|row| row.uid)
        .collect();
    assert!(
        pii_ceiling.contains(&none_item.uid),
        "none-class vector must pass a PII ceiling: {pii_ceiling:?}"
    );
    assert!(
        pii_ceiling.contains(&pii_item.uid),
        "PII vector must pass a PII ceiling: {pii_ceiling:?}"
    );

    let none_ceiling: HashSet<Uuid> = store
        .knn(&query_at(SensitivityClass::None))
        .await
        .expect("none-ceiling query")
        .into_iter()
        .map(|row| row.uid)
        .collect();
    assert_eq!(
        none_ceiling,
        HashSet::from([none_item.uid]),
        "only the non-PII vector survives a none ceiling"
    );

    store.delete(&uids).await.expect("delete vectors");
    delete_node_index_rows(session_store.pool(), &uids).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn promotion_validate_storage_partition_scores_real_backend_overlap_db_memory() {
    // Pins the production `validate_storage_partition` overlap path against real
    // Postgres: `fetch_validation_sample` reads the seeded `moa.embeddings` rows
    // and the method contrasts the pgvector source KNN with the promotion target
    // KNN per sampled row. Identical backends must validate (overlap 1.0 >=
    // threshold); a disjoint target must be rejected (overlap 0.0 < threshold).
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let items: Vec<_> = (0..5)
        .map(|index| {
            vector_item(
                Uuid::now_v7(),
                &storage_partition_id,
                "Fact",
                basis_vector(index),
            )
        })
        .collect();
    let uids: Vec<_> = items.iter().map(|item| item.uid).collect();
    insert_node_index_rows(session_store.pool(), &storage_partition_id, &items).await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let source: Arc<dyn VectorStore> = Arc::new(PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    ));
    source.upsert(&items).await.expect("upsert source vectors");

    // Identical source/target backends: every sampled query returns the same
    // top-K from both sides, so the average overlap is 1.0 and validation passes.
    let identical =
        VectorPartitionPromotion::new(session_store.pool().clone(), source.clone(), source.clone());
    let high_overlap = identical
        .validate_storage_partition(&storage_partition_id, 100)
        .await
        .expect("validate identical backends");
    assert!(
        (high_overlap - 1.0).abs() < f64::EPSILON,
        "identical pgvector backends must report full overlap, got {high_overlap}"
    );
    assert!(
        high_overlap >= PROMOTION_OVERLAP_THRESHOLD,
        "full overlap must clear the promotion threshold"
    );

    // Disjoint target backend: no sampled query overlaps, so validation reports
    // zero overlap and would reject the promotion.
    let target: Arc<dyn VectorStore> = Arc::new(DisjointVectorStore);
    let mismatched =
        VectorPartitionPromotion::new(session_store.pool().clone(), source.clone(), target);
    let low_overlap = mismatched
        .validate_storage_partition(&storage_partition_id, 100)
        .await
        .expect("validate disjoint backends");
    assert_eq!(
        low_overlap, 0.0,
        "a disjoint target backend must report zero overlap"
    );
    assert!(
        low_overlap < PROMOTION_OVERLAP_THRESHOLD,
        "zero overlap must fail the promotion threshold"
    );

    source.delete(&uids).await.expect("delete source vectors");
    delete_node_index_rows(session_store.pool(), &uids).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn pgvector_knn_returns_topk_in_strict_distance_order_db_memory() {
    // Pins: after the KNN rewrite (single-bound probe vector, HNSW iterative_scan,
    // subquery `LIMIT` + outer re-sort) the returned rows and their distance order
    // are unchanged. Five vectors with distinct, monotone cosine distances to the
    // probe must come back sorted by descending score, and a smaller `k` must yield
    // exactly the top-k prefix of that ordering.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();

    // Ordered nearest -> farthest to basis_vector(0): (1,0) sim 1.0, (3,1) sim
    // 0.949, (1,1) sim 0.707, (1,3) sim 0.316, (0,1) sim 0.0.
    let ordered: Vec<(f32, f32)> = vec![(1.0, 0.0), (3.0, 1.0), (1.0, 1.0), (1.0, 3.0), (0.0, 1.0)];
    let items: Vec<_> = ordered
        .iter()
        .map(|(x, y)| {
            vector_item(
                Uuid::now_v7(),
                &storage_partition_id,
                "Fact",
                mixed_vector(*x, *y),
            )
        })
        .collect();
    let expected_order: Vec<Uuid> = items.iter().map(|item| item.uid).collect();
    let uids: Vec<_> = expected_order.clone();
    insert_node_index_rows(session_store.pool(), &storage_partition_id, &items).await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    store.upsert(&items).await.expect("upsert ordered vectors");

    let probe = mixed_vector(1.0, 0.0);
    let full = store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                probe.clone(),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 5,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query full ordering");
    let full_uids: Vec<Uuid> = full.iter().map(|row| row.uid).collect();
    assert_eq!(
        full_uids, expected_order,
        "KNN must return rows in strict descending-score (ascending-distance) order"
    );
    for window in full.windows(2) {
        assert!(
            window[0].score > window[1].score,
            "scores must be strictly descending: {} !> {}",
            window[0].score,
            window[1].score
        );
    }

    // A smaller k must return exactly the top-k prefix of the same ordering.
    let topk = store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(probe, "test-model".to_string())
                .expect("valid query embedding"),
            k: 3,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query top-3");
    let topk_uids: Vec<Uuid> = topk.iter().map(|row| row.uid).collect();
    assert_eq!(
        topk_uids,
        expected_order[..3].to_vec(),
        "a smaller k must yield the top-k prefix of the full ordering"
    );

    store.delete(&uids).await.expect("delete vectors");
    delete_node_index_rows(session_store.pool(), &uids).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn pgvector_knn_excludes_soft_deleted_embedding_even_when_nearest_db_memory() {
    // Pins: the validity predicate survives the KNN rewrite. A soft-deleted
    // embedding (embedding.valid_to set) that is the exact nearest neighbor must
    // still be excluded from a default (as_of = None) query, and only the valid
    // farther row is returned. The subquery/outer-sort restructuring must not let
    // an invalid row leak in ahead of the WHERE filter.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();

    let valid_item = vector_item(
        Uuid::now_v7(),
        &storage_partition_id,
        "Fact",
        mixed_vector(1.0, 1.0),
    );
    let expired_item = VectorItem {
        valid_to: Some(utc("2020-01-01T00:00:00Z")),
        ..vector_item(
            Uuid::now_v7(),
            &storage_partition_id,
            "Fact",
            mixed_vector(1.0, 0.0),
        )
    };
    let items = vec![valid_item.clone(), expired_item.clone()];
    let uids: Vec<_> = items.iter().map(|item| item.uid).collect();
    insert_node_index_rows(session_store.pool(), &storage_partition_id, &items).await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    store.upsert(&items).await.expect("upsert soft-deleted mix");

    let matches = store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                mixed_vector(1.0, 0.0),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 5,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query with soft-deleted nearest neighbor");
    let match_uids: Vec<Uuid> = matches.iter().map(|row| row.uid).collect();
    assert_eq!(
        match_uids,
        vec![valid_item.uid],
        "soft-deleted nearest neighbor must be excluded; only the valid row survives"
    );

    store.delete(&uids).await.expect("delete vectors");
    delete_node_index_rows(session_store.pool(), &uids).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn pgvector_knn_hnsw_tuning_does_not_leak_past_transaction_db_memory() {
    // Pins: the per-query HNSW tuning is applied with SET LOCAL semantics
    // (set_config(..., true)) on the KNN's own ScopedConn transaction, so it is
    // discarded at commit and never persists onto the pooled connection. A
    // dedicated single-connection pool guarantees the follow-up SHOW runs on the
    // same backend that served the KNN; if the store had used session-level SET,
    // ef_search/iterative_scan would still read the tuned values here.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let item = vector_item(
        Uuid::now_v7(),
        &storage_partition_id,
        "Fact",
        mixed_vector(1.0, 0.0),
    );
    insert_node_index_rows(
        session_store.pool(),
        &storage_partition_id,
        std::slice::from_ref(&item),
    )
    .await;
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    // Seed through the shared store, then pin all KNN + verification traffic to one
    // physical connection so a leaked GUC would be observable.
    let seed_store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    seed_store
        .upsert(std::slice::from_ref(&item))
        .await
        .expect("upsert leak-test vector");

    let search_path = format!("\"{schema_name}\", public");
    let pinned_pool = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("connect single-connection leak-test pool");

    let pinned_store = PgvectorStore::new_for_app_role(
        pinned_pool.clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    // k = 10 -> ef_search floors at 100, so the tuned value differs from the 40
    // default; any leak would surface as "100" below.
    let matches = pinned_store
        .knn(&VectorQuery {
            embedding: moa_memory_vector::QueryEmbedding::new(
                mixed_vector(1.0, 0.0),
                "test-model".to_string(),
            )
            .expect("valid query embedding"),
            k: 10,
            label_filter: Some(vec!["Fact".to_string()]),
            max_pii_class: SensitivityClass::Restricted,
            include_global: false,
            as_of: None,
        })
        .await
        .expect("query on pinned connection");
    assert_eq!(matches.first().map(|row| row.uid), Some(item.uid));

    let ef_search: String = sqlx::query("SHOW hnsw.ef_search")
        .fetch_one(&pinned_pool)
        .await
        .expect("read hnsw.ef_search on pinned connection")
        .try_get(0)
        .expect("hnsw.ef_search value column");
    assert_eq!(
        ef_search, "40",
        "hnsw.ef_search must revert to the pgvector default after the KNN transaction commits"
    );

    let iterative_scan: String = sqlx::query("SHOW hnsw.iterative_scan")
        .fetch_one(&pinned_pool)
        .await
        .expect("read hnsw.iterative_scan on pinned connection")
        .try_get(0)
        .expect("hnsw.iterative_scan value column");
    assert_eq!(
        iterative_scan, "off",
        "hnsw.iterative_scan must revert to the pgvector default after the KNN transaction commits"
    );

    pinned_store
        .delete(std::slice::from_ref(&item.uid))
        .await
        .expect("delete leak-test vector");
    delete_node_index_rows(session_store.pool(), &[item.uid]).await;
    pinned_pool.close().await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

/// Seeds a 50-vector Matryoshka-friendly corpus. Cosine similarity to
/// `mixed_vector(10.0, 0.0)` strictly decreases with the dim-1 component, and all
/// signal lives in dims 0..2, so a 512-dim truncated prefix preserves the exact
/// nearest-neighbor order. Returns the items in decreasing-similarity order
/// (index 0 == nearest neighbor).
async fn seed_mrl_prefix_ordered_corpus(
    pool: &PgPool,
    storage_partition_id: &str,
) -> Vec<VectorItem> {
    let items: Vec<_> = (0..50)
        .map(|y| {
            vector_item(
                Uuid::now_v7(),
                storage_partition_id,
                "Fact",
                mixed_vector(10.0, y as f32),
            )
        })
        .collect();
    insert_node_index_rows(pool, storage_partition_id, &items).await;
    set_workspace_embedder_state(pool, storage_partition_id, "test-model").await;
    let store = PgvectorStore::new_for_app_role(pool.clone(), tenant_scope(storage_partition_id));
    store.upsert(&items).await.expect("upsert MRL corpus");
    items
}

fn topk_query(k: usize) -> VectorQuery {
    VectorQuery {
        embedding: moa_memory_vector::QueryEmbedding::new(
            mixed_vector(10.0, 0.0),
            "test-model".to_string(),
        )
        .expect("valid query embedding"),
        k,
        label_filter: Some(vec!["Fact".to_string()]),
        max_pii_class: SensitivityClass::Restricted,
        include_global: false,
        as_of: None,
    }
}

#[tokio::test]
async fn mrl_cascade_matches_exact_full_dim_topk_db_memory() {
    // Pins: the Matryoshka two-stage cascade (truncated 512-dim prefix shortlist +
    // exact full-dim rescore) returns the same top-k, in the same order and with the
    // same full-dim scores, as an exact full-dim scan on a 50-vector corpus whose
    // signal lives entirely in the prefix. Exercises the functional subvector index
    // expression, the nested-CTE shape, filter/bind ordering, and the rescore end to
    // end. Break either stage (e.g. drop the rescore or truncate the shortlist too
    // hard) and the order or scores diverge from the exact baseline.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();

    let items = seed_mrl_prefix_ordered_corpus(session_store.pool(), &storage_partition_id).await;
    let expected_order: Vec<Uuid> = items.iter().map(|item| item.uid).collect();

    let exact_store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    )
    .with_exact_search(true);
    let exact = exact_store
        .knn(&topk_query(10))
        .await
        .expect("exact full-dim knn");
    let exact_uids: Vec<Uuid> = exact.iter().map(|row| row.uid).collect();
    assert_eq!(
        exact_uids,
        expected_order[..10].to_vec(),
        "exact full-dim search must return the constructed nearest-neighbor order"
    );

    let cascade_store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    )
    .with_mrl_shortlist(Some(512));
    let cascade = cascade_store
        .knn(&topk_query(10))
        .await
        .expect("mrl cascade knn");
    let cascade_uids: Vec<Uuid> = cascade.iter().map(|row| row.uid).collect();
    assert_eq!(
        cascade_uids, exact_uids,
        "MRL cascade top-k must equal the exact full-dim top-k"
    );
    for (cascade_row, exact_row) in cascade.iter().zip(exact.iter()) {
        assert!(
            (cascade_row.score - exact_row.score).abs() < 1e-4,
            "cascade rescores by full-dim distance, so scores match exact: {} vs {}",
            cascade_row.score,
            exact_row.score
        );
    }

    exact_store
        .delete(&expected_order)
        .await
        .expect("delete vectors");
    delete_node_index_rows(session_store.pool(), &expected_order).await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn mrl_disabled_matches_exact_full_dim_topk_db_memory() {
    // Pins: with the cascade disabled (mrl_shortlist_dims = None, the default) the KNN
    // path is unchanged -- it returns the same top-k as an exact full-dim scan on the
    // same corpus, exactly as before the MRL cascade was added. This is the "disabled
    // path unchanged" guard: a store built without `with_mrl_shortlist` must never take
    // the two-stage branch.
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();

    let items = seed_mrl_prefix_ordered_corpus(session_store.pool(), &storage_partition_id).await;
    let expected_order: Vec<Uuid> = items.iter().map(|item| item.uid).collect();

    let exact_store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    )
    .with_exact_search(true);
    let exact_uids: Vec<Uuid> = exact_store
        .knn(&topk_query(10))
        .await
        .expect("exact full-dim knn")
        .into_iter()
        .map(|row| row.uid)
        .collect();

    let disabled_store = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(storage_partition_id.clone()),
    );
    let disabled_uids: Vec<Uuid> = disabled_store
        .knn(&topk_query(10))
        .await
        .expect("cascade-off knn")
        .into_iter()
        .map(|row| row.uid)
        .collect();
    assert_eq!(
        disabled_uids,
        expected_order[..10].to_vec(),
        "cascade-off KNN must return the exact nearest-neighbor order"
    );
    assert_eq!(
        disabled_uids, exact_uids,
        "cascade-off KNN must match exact full-dim search (unchanged behavior)"
    );

    exact_store
        .delete(&expected_order)
        .await
        .expect("delete vectors");
    delete_node_index_rows(session_store.pool(), &expected_order).await;
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
