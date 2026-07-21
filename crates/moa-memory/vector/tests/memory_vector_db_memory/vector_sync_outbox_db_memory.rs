//! Integration coverage for external vector-backend sync outbox behavior.

use std::{sync::Arc, time::Duration};

use moa_core::{
    config::MoaConfig,
    types::{identifiers::TenantId, memory::RlsContext, security::SensitivityClass},
};
use moa_db::ScopedConn;
use moa_memory_pii::legal_hold::start_destruction;
use moa_memory_vector::{
    VECTOR_DIMENSION, VectorItem, VectorStoreFactory, VectorSyncReport,
    sync::{has_active_vector_sync_claims, has_active_vector_sync_claims_in_tx},
};
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
        pii_class: SensitivityClass::None,
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
    let tenant_id =
        Uuid::parse_str(storage_partition_id).expect("test storage partition id is a tenant UUID");
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, name, pii_class, data_subject_id)
        VALUES ($1, $2, $3, $4, $5, $6)
        "#,
    )
    .bind(item.uid)
    .bind(&item.label)
    .bind(storage_partition_id)
    .bind(format!("vector sync {}", item.uid))
    .bind(item.pii_class.as_str())
    .bind(tenant_id)
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
    // Queue-only (F10): the graph write enqueues the outbox row and returns; it is
    // not synchronously drained, so the row stays pending for the background cron.
    assert_eq!(
        pending_outbox_count(&pool).await,
        1,
        "graph writes must leave the enqueued vector-sync row pending, not drain it inline"
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
async fn tenant_fence_keeps_expired_pre_fence_claim_from_reupserting_db_memory() {
    // Pins: a lease claimed before tenant destruction remains visible to the purge waiter, but
    // once that lease expires the durable tenant fence prevents every pod from reclaiming and
    // applying the stale upsert after external-vector deletion has quiesced.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = store.pool().clone();
    let server = MockServer::start().await;
    let (partition, item, factory) = setup_pending_upsert(&pool, server.uri()).await;
    let tenant_id = Uuid::parse_str(&partition).expect("partition should be a tenant UUID");
    let pre_fence_claim_token = Uuid::now_v7();

    let claimed = sqlx::query_as::<_, (Uuid, bool, i32)>(
        r#"
        UPDATE moa.vector_sync_outbox
           SET claim_token = $2,
               claim_expires_at = now() + INTERVAL '5 minutes',
               processing_started_at = now(),
               attempts = 1
         WHERE uid = $1
        RETURNING claim_token, claim_expires_at > now(), attempts
        "#,
    )
    .bind(item.uid)
    .bind(pre_fence_claim_token)
    .fetch_one(&pool)
    .await
    .expect("simulate a pre-fence vector-sync claim");
    assert_eq!(claimed, (pre_fence_claim_token, true, 1));

    let mut fence_conn = scoped_conn(&pool, &partition).await;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('moa:destruction:tenant:' || $1::text, 0))",
    )
    .bind(tenant_id)
    .execute(fence_conn.as_mut())
    .await
    .expect("lock tenant destruction boundary");
    sqlx::query(
        r#"
        INSERT INTO moa.destruction_operation_fence
            (tenant_id, subject_id, operation_id, operation_kind)
        VALUES ($1, NULL, $2, 'tenant.purge')
        "#,
    )
    .bind(tenant_id)
    .bind(format!("purge-{tenant_id}"))
    .execute(fence_conn.as_mut())
    .await
    .expect("persist tenant-wide destruction fence");
    fence_conn
        .commit()
        .await
        .expect("commit tenant-wide destruction fence");

    let visible_lease = sqlx::query_as::<_, (Option<Uuid>, bool, bool)>(
        r#"
        SELECT claim_token,
               claim_expires_at > now(),
               processed_at IS NULL
          FROM moa.vector_sync_outbox
         WHERE uid = $1
        "#,
    )
    .bind(item.uid)
    .fetch_one(&pool)
    .await
    .expect("read pre-fence lease after fence commit");
    assert_eq!(
        visible_lease,
        (Some(pre_fence_claim_token), true, true),
        "purge must be able to observe and wait for the pre-fence lease"
    );
    assert!(
        has_active_vector_sync_claims(&pool, &partition)
            .await
            .expect("check active claim through pool helper"),
        "pool quiescence check must see the pre-fence lease"
    );
    let mut quiescence_conn = scoped_conn(&pool, &partition).await;
    assert!(
        has_active_vector_sync_claims_in_tx(quiescence_conn.as_mut(), &partition)
            .await
            .expect("check active claim in purge transaction"),
        "same-transaction quiescence check must see the pre-fence lease"
    );
    quiescence_conn
        .commit()
        .await
        .expect("commit quiescence check");

    sqlx::query("UPDATE moa.vector_sync_outbox SET claim_expires_at = NULL WHERE uid = $1")
        .bind(item.uid)
        .execute(&pool)
        .await
        .expect("simulate malformed claim without an expiry");
    assert!(
        has_active_vector_sync_claims(&pool, &partition)
            .await
            .expect("check no-expiry claim through pool helper"),
        "a claimed row without an expiry must fail closed as active"
    );

    sqlx::query(
        "UPDATE moa.vector_sync_outbox SET claim_expires_at = now() - INTERVAL '1 second' WHERE uid = $1",
    )
    .bind(item.uid)
    .execute(&pool)
    .await
    .expect("expire pre-fence lease to simulate quiescence");
    assert!(
        !has_active_vector_sync_claims(&pool, &partition)
            .await
            .expect("check expired claim through pool helper"),
        "an expired claim is quiescent"
    );

    let report = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("fenced drain should complete without claiming work");
    assert_eq!(report, VectorSyncReport::default());
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(
        requests.len(),
        0,
        "an expired pre-fence claim must not upsert after destruction quiesces"
    );

    let final_claim = sqlx::query_as::<_, (Option<Uuid>, i32, bool)>(
        r#"
        SELECT claim_token, attempts, processed_at IS NULL
          FROM moa.vector_sync_outbox
         WHERE uid = $1
        "#,
    )
    .bind(item.uid)
    .fetch_one(&pool)
    .await
    .expect("read fenced vector-sync claim");
    assert_eq!(
        final_claim,
        (Some(pre_fence_claim_token), 1, true),
        "the claimant must leave the pre-fence lease durable for purge cleanup"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn tenant_fence_waits_for_remote_upsert_and_claim_settlement_db_memory() {
    // Pins: the vector drainer holds the shared tenant destruction lock across remote I/O and
    // durable claim settlement, so a concurrent purge fence cannot commit before the upsert and
    // then let that pre-fence upsert land after purge deletion.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = store.pool().clone();
    let server = MockServer::start().await;
    let remote_started = Arc::new(tokio::sync::Notify::new());
    let responder_started = remote_started.clone();
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(move |_: &wiremock::Request| {
            responder_started.notify_one();
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(json!({ "rows_affected": 1, "rows_upserted": 1 }))
        })
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("deletes"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "rows_affected": 1 })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let (partition, item, factory) = setup_pending_upsert(&pool, server.uri()).await;
    let tenant_id = Uuid::parse_str(&partition).expect("partition should be a tenant UUID");

    let drain_pool = pool.clone();
    let drain_factory = factory.clone();
    let drain_task =
        tokio::spawn(async move { drain_factory.drain_external_sync(&drain_pool, 10).await });

    tokio::time::timeout(Duration::from_secs(2), remote_started.notified())
        .await
        .expect("vector drainer should reach remote I/O while holding the destruction lock");

    let fence_pool = pool.clone();
    let mut fence_task = tokio::spawn(async move {
        start_destruction(
            &fence_pool,
            TenantId::from(tenant_id),
            &[],
            &format!("remote-order-{tenant_id}"),
            "tenant.purge",
        )
        .await
        .expect("start destruction after remote upsert settles");
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut fence_task)
            .await
            .is_err(),
        "tenant purge fence must wait while remote upsert and claim settlement hold the lock"
    );

    let report = drain_task
        .await
        .expect("join vector drain")
        .expect("complete vector drain");
    assert_eq!(
        report,
        VectorSyncReport {
            attempted: 1,
            succeeded: 1,
            skipped: 0,
            failed: 0,
            dead_lettered: 0,
        }
    );
    fence_task.await.expect("join tenant destruction fence");

    factory
        .turbopuffer()
        .expect("test factory should configure Turbopuffer")
        .delete_in_storage_partition(&partition, &[item.uid])
        .await
        .expect("purge-side external delete must run after destruction wins the lock");

    let state = sqlx::query_as::<_, (bool, bool)>(
        r#"
        SELECT outbox.processed_at IS NOT NULL,
               EXISTS (
                   SELECT 1 FROM moa.destruction_operation_fence
                    WHERE tenant_id = $2 AND subject_id IS NULL
               )
          FROM moa.vector_sync_outbox AS outbox
         WHERE outbox.uid = $1
        "#,
    )
    .bind(item.uid)
    .bind(tenant_id)
    .fetch_one(&pool)
    .await
    .expect("read settled claim and destruction fence");
    assert_eq!(
        state,
        (true, true),
        "claim settlement must precede the durable purge fence"
    );

    let retry = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("post-fence drain should not claim work");
    assert_eq!(retry, VectorSyncReport::default());
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(
        requests.len(),
        2,
        "one upsert must precede one purge delete"
    );
    let request_bodies = requests
        .iter()
        .map(|request| String::from_utf8_lossy(&request.body))
        .collect::<Vec<_>>();
    assert!(
        request_bodies[0].contains("upsert_rows"),
        "the pre-fence request must be the upsert"
    );
    assert!(
        request_bodies[1].contains("deletes"),
        "the purge delete must be the last external mutation"
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
            dead_lettered: 0,
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
            dead_lettered: 0,
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

/// Seeds a Turbopuffer partition with one committed pgvector row and its queued
/// external upsert, returning the partition, item, and a factory pointed at the
/// mock backend.
async fn setup_pending_upsert(
    pool: &PgPool,
    server_uri: String,
) -> (String, VectorItem, VectorStoreFactory) {
    let embedding_model = "test-embed";
    let partition = Uuid::now_v7().to_string();
    let item = vector_item(Uuid::now_v7(), embedding_model);
    seed_storage_partition_state(pool, &partition, "turbopuffer", embedding_model).await;
    insert_node_index_row(pool, &partition, &item).await;

    let mut config = MoaConfig::default();
    config.memory.vector.turbopuffer.api_key = "test-key".to_string();
    config.memory.vector.turbopuffer.base_url = Some(server_uri);
    config.memory.vector.turbopuffer.environment = Some("test".to_string());
    let factory = VectorStoreFactory::from_config(&config);
    let vector = factory
        .transactional_graph_backend(pool.clone(), tenant_scope(&partition), true)
        .vector_store();
    let mut conn = scoped_conn(pool, &partition).await;
    vector
        .upsert_in_tx(conn.as_mut(), std::slice::from_ref(&item))
        .await
        .expect("queue external upsert");
    conn.commit().await.expect("commit source upsert");
    (partition, item, factory)
}

/// Returns `(attempts, dead_lettered, backed_off, has_error)` for one outbox row.
async fn outbox_row_state(pool: &PgPool, uid: Uuid) -> (i32, bool, bool, bool) {
    sqlx::query_as::<_, (i32, bool, bool, bool)>(
        r#"
        SELECT attempts,
               dead_lettered_at IS NOT NULL AS dead_lettered,
               available_at > now() AS backed_off,
               last_error IS NOT NULL AS has_error
          FROM moa.vector_sync_outbox
         WHERE uid = $1
        "#,
    )
    .bind(uid)
    .fetch_one(pool)
    .await
    .expect("read vector sync outbox row state")
}

/// Returns the remaining backoff window, in seconds, for one outbox row.
async fn seconds_until_available(pool: &PgPool, uid: Uuid) -> f64 {
    sqlx::query_scalar::<_, f64>(
        "SELECT EXTRACT(EPOCH FROM (available_at - now()))::float8
           FROM moa.vector_sync_outbox WHERE uid = $1",
    )
    .bind(uid)
    .fetch_one(pool)
    .await
    .expect("read available_at delta")
}

/// Simulates the backoff window elapsing so the next drain can reclaim the row.
async fn make_available_now(pool: &PgPool, uid: Uuid) {
    sqlx::query("UPDATE moa.vector_sync_outbox SET available_at = now() WHERE uid = $1")
        .bind(uid)
        .execute(pool)
        .await
        .expect("reset available_at");
}

#[tokio::test]
async fn permanent_failure_dead_letters_and_is_excluded_from_claims_db_memory() {
    // Pins (F25): a permanent (4xx) external failure quarantines the row on the
    // first attempt and the claim predicate never reclaims it, so poison jobs
    // cannot consume the drainer forever.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = store.pool().clone();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(400).set_body_string("schema mismatch"))
        .mount(&server)
        .await;
    let (_partition, item, factory) = setup_pending_upsert(&pool, server.uri()).await;

    let report = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("drain applies the permanent failure");
    assert_eq!(report.attempted, 1);
    assert_eq!(report.failed, 1);
    assert_eq!(report.dead_lettered, 1);
    assert_eq!(report.succeeded, 0);

    let (_attempts, dead_lettered, _backed_off, has_error) =
        outbox_row_state(&pool, item.uid).await;
    assert!(dead_lettered, "a permanent failure must quarantine the row");
    assert!(has_error, "the quarantined row retains its last error");

    let second = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("second drain");
    assert_eq!(
        second.attempted, 0,
        "dead-lettered rows must be excluded from later claims"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn transient_failure_backs_off_exponentially_then_succeeds_db_memory() {
    // Pins (F25): a transient (5xx) failure is not quarantined; each retry backs
    // off with a growing delay (not a flat 30s), and the row succeeds once the
    // backend recovers.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = store.pool().clone();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream unavailable"))
        .mount(&server)
        .await;
    let (_partition, item, factory) = setup_pending_upsert(&pool, server.uri()).await;

    let first = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("first drain fails transiently");
    assert_eq!(first.failed, 1);
    assert_eq!(first.dead_lettered, 0);
    let (attempts, dead_lettered, backed_off, has_error) = outbox_row_state(&pool, item.uid).await;
    assert!(!dead_lettered, "a transient failure must not quarantine");
    assert_eq!(attempts, 1);
    assert!(backed_off, "a transient failure schedules a future retry");
    assert!(has_error);
    let backoff_after_first = seconds_until_available(&pool, item.uid).await;

    make_available_now(&pool, item.uid).await;
    let second = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("second drain fails transiently again");
    assert_eq!(second.failed, 1);
    assert_eq!(second.dead_lettered, 0);
    let backoff_after_second = seconds_until_available(&pool, item.uid).await;
    assert!(
        backoff_after_second > backoff_after_first + 20.0,
        "retry backoff must grow exponentially: {backoff_after_first}s then {backoff_after_second}s"
    );

    // Backend recovers; after the backoff window the row drains successfully.
    server.reset().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 1,
            "rows_upserted": 1
        })))
        .mount(&server)
        .await;
    make_available_now(&pool, item.uid).await;
    let third = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("recovered drain succeeds");
    assert_eq!(third.succeeded, 1);
    assert_eq!(pending_outbox_count(&pool).await, 0);

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn redrive_returns_dead_lettered_rows_to_pending_db_memory() {
    // Pins (F25): after remediation an operator redrive clears the quarantine and
    // makes the rows immediately eligible for the drainer again.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = store.pool().clone();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;
    let (partition, item, factory) = setup_pending_upsert(&pool, server.uri()).await;

    let report = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("drain quarantines the row");
    assert_eq!(report.dead_lettered, 1);
    let (_a, dead_lettered, _b, _c) = outbox_row_state(&pool, item.uid).await;
    assert!(dead_lettered);

    let redriven = factory
        .redrive_dead_lettered_external_sync(&pool, Some(&partition))
        .await
        .expect("redrive quarantined rows");
    assert_eq!(redriven, 1, "the one dead-lettered row is re-queued");
    let (attempts, dead_lettered, backed_off, _has_error) = outbox_row_state(&pool, item.uid).await;
    assert!(!dead_lettered, "redrive clears the quarantine");
    assert_eq!(attempts, 0, "redrive resets the attempt counter");
    assert!(!backed_off, "redriven rows are immediately eligible");

    // The recovered backend now delivers the redriven row.
    server.reset().await;
    Mock::given(method("POST"))
        .and(body_string_contains("upsert_rows"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows_affected": 1,
            "rows_upserted": 1
        })))
        .mount(&server)
        .await;
    let after = factory
        .drain_external_sync(&pool, 10)
        .await
        .expect("post-redrive drain");
    assert_eq!(after.succeeded, 1);
    assert_eq!(pending_outbox_count(&pool).await, 0);

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
