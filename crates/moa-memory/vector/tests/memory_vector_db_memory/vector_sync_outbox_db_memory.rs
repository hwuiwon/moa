//! Integration coverage for external vector-backend sync outbox behavior.

use moa_core::{MoaConfig, RlsContext, TenantId};
use moa_db::ScopedConn;
use moa_memory_vector::{VECTOR_DIMENSION, VectorItem, VectorStoreFactory, VectorSyncReport};
use moa_session::testing;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tenant_scope(storage_partition_id: &str) -> RlsContext {
    let tenant_id =
        Uuid::parse_str(storage_partition_id).expect("test storage partition id is a UUID");
    RlsContext::tenant(TenantId::from(tenant_id))
}

fn basis_vector(index: usize) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    vector[index % VECTOR_DIMENSION] = 1.0;
    vector
}

fn vector_item(uid: Uuid, embedding_model: &str) -> VectorItem {
    VectorItem {
        uid,
        user_id: None,
        label: "Fact".to_string(),
        pii_class: "none".to_string(),
        embedding: basis_vector(0),
        embedding_model: embedding_model.to_string(),
        embedding_model_version: 1,
        search_text: None,
        valid_to: None,
    }
}

fn chunk_vector_item(uid: Uuid, embedding_model: &str) -> VectorItem {
    let mut item = vector_item(uid, embedding_model);
    item.label = "Chunk".to_string();
    item
}

async fn scoped_conn<'a>(pool: &'a PgPool, storage_partition_id: &str) -> ScopedConn<'a> {
    let ctx = tenant_scope(storage_partition_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin scoped vector-sync transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

async fn seed_storage_partition_state(
    pool: &PgPool,
    storage_partition_id: &str,
    vector_backend: &str,
    embedding_model: &str,
) {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, vector_backend, embedding_model, embedding_model_version,
             embedding_dimension)
        VALUES ($1, $2, $3, 1, 1024)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET vector_backend = EXCLUDED.vector_backend,
                vector_backend_state = 'steady',
                embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(storage_partition_id)
    .bind(vector_backend)
    .bind(embedding_model)
    .execute(conn.as_mut())
    .await
    .expect("seed storage partition vector state");
    conn.commit()
        .await
        .expect("commit storage partition vector state");
}

async fn insert_node_index_row(pool: &PgPool, storage_partition_id: &str, item: &VectorItem) {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.node_index (uid, label, storage_partition_id, name, pii_class)
        VALUES ($1, $2, $3, $4, $5)
        "#,
    )
    .bind(item.uid)
    .bind(&item.label)
    .bind(storage_partition_id)
    .bind(format!("vector sync {}", item.uid))
    .bind(&item.pii_class)
    .execute(conn.as_mut())
    .await
    .expect("insert node_index row");
    conn.commit().await.expect("commit node_index row");
}

async fn insert_knowledge_chunk(
    pool: &PgPool,
    storage_partition_id: &str,
    graph_node_uid: Uuid,
    text: &str,
) {
    let tenant_id =
        Uuid::parse_str(storage_partition_id).expect("test storage partition id is a tenant UUID");
    let connection_uid = Uuid::now_v7();
    let object_uid = Uuid::now_v7();
    let version_uid = Uuid::now_v7();
    let chunk_uid = Uuid::now_v7();
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_connections (
            connection_uid, tenant_id, storage_partition_id, provider,
            provider_config_key, provider_connection_id, connector,
            credential_ref, status
        )
        VALUES ($1, $2, $3, 'test-provider', 'default', $4, 'test-connector',
                'test-credential', 'active')
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id)
    .bind(storage_partition_id)
    .bind(format!("account-{connection_uid}"))
    .execute(conn.as_mut())
    .await
    .expect("insert knowledge connection");
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_objects (
            object_uid, tenant_id, storage_partition_id, connection_id,
            object_type, external_object_id, title, status, metadata
        )
        VALUES ($1, $2, $3, $4, 'article', 'article-1', 'Article 1', 'active', '{}')
        "#,
    )
    .bind(object_uid)
    .bind(tenant_id)
    .bind(storage_partition_id)
    .bind(connection_uid)
    .execute(conn.as_mut())
    .await
    .expect("insert knowledge object");
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_document_versions (
            document_version_uid, tenant_id, storage_partition_id, object_id,
            parser_provider, content_hash, metadata
        )
        VALUES ($1, $2, $3, $4, 'native', 'content-hash-1', '{}')
        "#,
    )
    .bind(version_uid)
    .bind(tenant_id)
    .bind(storage_partition_id)
    .bind(object_uid)
    .execute(conn.as_mut())
    .await
    .expect("insert knowledge document version");
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_chunks (
            chunk_uid, tenant_id, storage_partition_id, document_version_id,
            graph_node_uid, chunk_hash, block_hashes, text, ordinal, token_count, metadata
        )
        VALUES ($1, $2, $3, $4, $5, 'chunk-hash-1', ARRAY['block-hash-1']::TEXT[],
                $6, 0, 12, '{}')
        "#,
    )
    .bind(chunk_uid)
    .bind(tenant_id)
    .bind(storage_partition_id)
    .bind(version_uid)
    .bind(graph_node_uid)
    .bind(text)
    .execute(conn.as_mut())
    .await
    .expect("insert knowledge chunk");
    conn.commit().await.expect("commit knowledge chunk");
}

async fn pending_outbox_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM moa.vector_sync_outbox WHERE processed_at IS NULL",
    )
    .fetch_one(pool)
    .await
    .expect("count pending vector sync rows")
}

async fn outbox_counts_for_partition(pool: &PgPool, storage_partition_id: &str) -> (i64, i64) {
    sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT count(*) FILTER (WHERE processed_at IS NULL) AS pending,
               count(*) FILTER (WHERE processed_at IS NOT NULL) AS processed
          FROM moa.vector_sync_outbox
         WHERE storage_partition_id = $1
        "#,
    )
    .bind(storage_partition_id)
    .fetch_one(pool)
    .await
    .expect("count partition vector sync rows")
}

#[tokio::test]
async fn transactional_graph_vector_store_enqueues_only_external_backend_rows_db_memory() {
    // Pins: graph transactions still write pgvector, but they queue external sync
    // only for partitions whose durable vector backend is not pgvector.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = store.pool().clone();
    let embedding_model = "test-embed";
    let pgvector_partition = Uuid::now_v7().to_string();
    let external_partition = Uuid::now_v7().to_string();
    let pgvector_item = vector_item(Uuid::now_v7(), embedding_model);
    let external_item = vector_item(Uuid::now_v7(), embedding_model);

    seed_storage_partition_state(&pool, &pgvector_partition, "pgvector", embedding_model).await;
    seed_storage_partition_state(&pool, &external_partition, "turbopuffer", embedding_model).await;
    insert_node_index_row(&pool, &pgvector_partition, &pgvector_item).await;
    insert_node_index_row(&pool, &external_partition, &external_item).await;

    let factory = VectorStoreFactory::default();
    let pgvector_store = factory
        .transactional_graph_backend(pool.clone(), tenant_scope(&pgvector_partition), true)
        .vector_store();
    let mut conn = scoped_conn(&pool, &pgvector_partition).await;
    pgvector_store
        .upsert_in_tx(conn.as_mut(), std::slice::from_ref(&pgvector_item))
        .await
        .expect("pgvector partition upsert");
    conn.commit().await.expect("commit pgvector upsert");

    let external_store = factory
        .transactional_graph_backend(pool.clone(), tenant_scope(&external_partition), true)
        .vector_store();
    let mut conn = scoped_conn(&pool, &external_partition).await;
    external_store
        .upsert_in_tx(conn.as_mut(), std::slice::from_ref(&external_item))
        .await
        .expect("external partition source upsert");
    conn.commit().await.expect("commit external source upsert");

    let rows = sqlx::query_as::<_, (String, Uuid, String)>(
        r#"
        SELECT storage_partition_id, uid, op
          FROM moa.vector_sync_outbox
         ORDER BY sync_id
        "#,
    )
    .fetch_all(&pool)
    .await
    .expect("read vector sync rows");
    assert_eq!(
        rows,
        vec![(
            external_partition.clone(),
            external_item.uid,
            "upsert".to_string()
        )]
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn post_commit_sync_drains_only_current_storage_partition_db_memory() {
    // Pins: graph-write post-commit sync cannot drain unrelated tenants' queued
    // rows; the background/global drain path remains responsible for those.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = store.pool().clone();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 3,
            "rows_upserted": 3
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let embedding_model = "test-embed";
    let partition_a = Uuid::now_v7().to_string();
    let partition_b = Uuid::now_v7().to_string();
    let item_a = vector_item(Uuid::now_v7(), embedding_model);
    let item_b = vector_item(Uuid::now_v7(), embedding_model);
    seed_storage_partition_state(&pool, &partition_a, "turbopuffer", embedding_model).await;
    seed_storage_partition_state(&pool, &partition_b, "turbopuffer", embedding_model).await;
    insert_node_index_row(&pool, &partition_a, &item_a).await;
    insert_node_index_row(&pool, &partition_b, &item_b).await;

    let mut config = MoaConfig::default();
    config.memory.vector.turbopuffer.api_key = "test-key".to_string();
    config.memory.vector.turbopuffer.base_url = Some(server.uri());
    config.memory.vector.turbopuffer.environment = Some("test".to_string());
    let factory = VectorStoreFactory::from_config(&config);
    let vector_a =
        factory.transactional_graph_backend(pool.clone(), tenant_scope(&partition_a), true);
    let vector_b =
        factory.transactional_graph_backend(pool.clone(), tenant_scope(&partition_b), true);

    let mut conn = scoped_conn(&pool, &partition_a).await;
    vector_a
        .vector_store()
        .upsert_in_tx(conn.as_mut(), std::slice::from_ref(&item_a))
        .await
        .expect("partition A source upsert");
    conn.commit()
        .await
        .expect("commit partition A source upsert");
    let mut conn = scoped_conn(&pool, &partition_b).await;
    vector_b
        .vector_store()
        .upsert_in_tx(conn.as_mut(), std::slice::from_ref(&item_b))
        .await
        .expect("partition B source upsert");
    conn.commit()
        .await
        .expect("commit partition B source upsert");

    vector_a
        .sync_post_commit()
        .await
        .expect("drain partition A post-commit rows");

    assert_eq!(
        outbox_counts_for_partition(&pool, &partition_a).await,
        (0, 1)
    );
    assert_eq!(
        outbox_counts_for_partition(&pool, &partition_b).await,
        (1, 0)
    );
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 1);
    assert!(
        String::from_utf8_lossy(&requests[0].body).contains(&item_a.uid.to_string()),
        "post-commit drain should only send partition A row"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn vector_sync_outbox_drains_turbopuffer_upsert_and_delete_db_memory() {
    // Pins: queued rows are applied to Turbopuffer after commit and marked
    // processed, while the source row is re-read from committed pgvector state.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = store.pool().clone();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 1,
            "rows_upserted": 1
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("deletes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 3
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    let storage_partition_id = Uuid::now_v7().to_string();
    let embedding_model = "test-embed";
    let items = vec![
        chunk_vector_item(Uuid::now_v7(), embedding_model),
        vector_item(Uuid::now_v7(), embedding_model),
        vector_item(Uuid::now_v7(), embedding_model),
    ];
    seed_storage_partition_state(&pool, &storage_partition_id, "turbopuffer", embedding_model)
        .await;
    for item in &items {
        insert_node_index_row(&pool, &storage_partition_id, item).await;
    }
    insert_knowledge_chunk(
        &pool,
        &storage_partition_id,
        items[0].uid,
        "deployment runbook abc-123",
    )
    .await;

    let mut config = MoaConfig::default();
    config.memory.vector.turbopuffer.api_key = "test-key".to_string();
    config.memory.vector.turbopuffer.base_url = Some(server.uri());
    config.memory.vector.turbopuffer.environment = Some("test".to_string());
    let factory = VectorStoreFactory::from_config(&config);
    let vector = factory
        .transactional_graph_backend(pool.clone(), tenant_scope(&storage_partition_id), true)
        .vector_store();

    let mut conn = scoped_conn(&pool, &storage_partition_id).await;
    vector
        .upsert_in_tx(conn.as_mut(), &items)
        .await
        .expect("source upsert queues external sync");
    conn.commit().await.expect("commit source upsert");

    let report = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("drain upsert sync");
    assert_eq!(
        report,
        VectorSyncReport {
            attempted: 3,
            succeeded: 3,
            skipped: 0,
            failed: 0,
        }
    );
    assert_eq!(pending_outbox_count(&pool).await, 0);

    let mut conn = scoped_conn(&pool, &storage_partition_id).await;
    let uids = items.iter().map(|item| item.uid).collect::<Vec<_>>();
    vector
        .delete_in_tx(conn.as_mut(), &uids)
        .await
        .expect("source delete queues external sync");
    conn.commit().await.expect("commit source delete");

    let report = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("drain delete sync");
    assert_eq!(
        report,
        VectorSyncReport {
            attempted: 3,
            succeeded: 3,
            skipped: 0,
            failed: 0,
        }
    );
    assert_eq!(pending_outbox_count(&pool).await, 0);

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 2);
    let bodies = requests
        .iter()
        .map(|request| String::from_utf8_lossy(&request.body).to_string())
        .collect::<Vec<_>>();
    let upsert_body = bodies
        .iter()
        .find(|body| body.contains("upsert_rows"))
        .expect("drain should batch committed pgvector rows into one Turbopuffer upsert");
    let delete_body = bodies
        .iter()
        .find(|body| body.contains("deletes"))
        .expect("drain should batch deletes into one Turbopuffer request");
    for uid in &uids {
        assert!(
            upsert_body.contains(&uid.to_string()),
            "batched upsert body missing {uid}: {upsert_body}"
        );
        assert!(
            delete_body.contains(&uid.to_string()),
            "batched delete body missing {uid}: {delete_body}"
        );
    }
    assert!(
        upsert_body.contains("\"schema\""),
        "chunk text projection should configure Turbopuffer BM25 schema: {upsert_body}"
    );
    assert!(
        upsert_body.contains("deployment runbook abc-123"),
        "chunk text should be preserved across pgvector source reload: {upsert_body}"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
