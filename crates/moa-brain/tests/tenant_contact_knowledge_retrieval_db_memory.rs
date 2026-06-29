//! Integration coverage for tenant knowledge plus contact-memory retrieval.

use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use chrono::Utc;
use moa_brain::pipeline::memory::GraphMemoryRetriever;
use moa_brain::planning::{PlannedQuery, Strategy};
use moa_brain::retrieval::{CachedHybridRetriever, HybridRetriever, RetrievalRequest};
use moa_core::RlsContext;
use moa_core::{
    Channel, ContactId, ContactRef, ContactVerificationState, ContextMessage, ContextProcessor,
    LineageHandle, ModelCapabilities, ModelId, SessionId, SessionMeta, TenantId, TokenPricing,
    ToolCallFormat, traits::EmbeddingProvider,
};
use moa_db::ScopedConn;
use moa_lineage_core::{LineageEvent, RetrievalLineage};
use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PiiClass, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{
    PgvectorStore, PromotionOptions, VECTOR_DIMENSION, VectorPartitionPromotion, VectorStore,
};
use moa_session::testing;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn mock_tenant_and_contact_retrieval() {
    // Pins: tenant KB chunks and the current contact's memory are retrieved together.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let tenant_id = TenantId::new();
    let contact_id = ContactId::new();
    let other_contact_id = ContactId::new();
    let tenant_scope = RlsContext::tenant(tenant_id);
    let contact_scope = RlsContext::contact(tenant_id, contact_id);
    let other_contact_scope = RlsContext::contact(tenant_id, other_contact_id);
    seed_storage_partition_embedder_state(&pool, tenant_id)
        .await
        .expect("seed tenant vector embedder state");
    let tenant_graph = graph_store(pool.clone(), tenant_scope.clone());
    let contact_graph = graph_store(pool.clone(), contact_scope);
    let other_contact_graph = graph_store(pool.clone(), other_contact_scope);

    let chunk_graph_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Chunk,
            "pto runbook answer",
            json!({ "summary": "tenant node summary should be replaced by hydrated chunk text" }),
        ))
        .await
        .expect("create tenant chunk graph node");
    seed_knowledge_chunk(&pool, tenant_id, chunk_graph_uid)
        .await
        .expect("seed tenant knowledge rows");
    let contact_fact_uid = contact_graph
        .create_node(node_intent(
            tenant_id,
            Some(contact_id),
            NodeLabel::Fact,
            "contact deployment preference answer",
            json!({ "summary": "current contact prefers blue deployment windows" }),
        ))
        .await
        .expect("create current contact fact");
    let other_contact_fact_uid = other_contact_graph
        .create_node(node_intent(
            tenant_id,
            Some(other_contact_id),
            NodeLabel::Fact,
            "contact deployment preference answer",
            json!({ "summary": "other contact private memory should not appear" }),
        ))
        .await
        .expect("create other contact fact");

    let session = contact_session(tenant_id, contact_id);
    let mut ctx = moa_core::WorkingContext::new(&session, capabilities());
    ctx.append_message(ContextMessage::user(
        "Find the pto runbook answer and contact deployment preference answer",
    ));
    let contact_lineage = Arc::new(CapturedLineage::default());
    let retriever = GraphMemoryRetriever::new(pool.clone(), Some(Arc::new(TestEmbedder)))
        .with_assume_app_role(true)
        .with_lineage(contact_lineage.clone())
        .with_result_limit(6);

    let output = retriever
        .process(&mut ctx)
        .await
        .expect("graph memory retrieval should assemble context");

    assert!(
        output
            .items_included
            .iter()
            .any(|item| item == &format!("graph:Chunk:{chunk_graph_uid}")),
        "{:?}",
        output.items_included
    );
    assert!(
        output
            .items_included
            .iter()
            .any(|item| item == &format!("graph:Fact:{contact_fact_uid}")),
        "{:?}",
        output.items_included
    );
    assert!(
        output
            .items_included
            .iter()
            .all(|item| item != &format!("graph:Fact:{other_contact_fact_uid}")),
        "{:?}",
        output.items_included
    );
    let memory_message = ctx
        .messages
        .first()
        .expect("memory reminder should be inserted");
    assert!(memory_message.content.contains("<knowledge_context>"));
    assert!(memory_message.content.contains("<tenant_knowledge>"));
    assert!(memory_message.content.contains("<user_memory>"));
    let tenant_section = section_between(
        &memory_message.content,
        "<tenant_knowledge>",
        "</tenant_knowledge>",
    );
    let user_section = section_between(&memory_message.content, "<user_memory>", "</user_memory>");
    assert!(tenant_section.contains("Hydrated tenant PTO policy chunk text"));
    assert!(tenant_section.contains("source_uri=https://example.test/pto-runbook"));
    assert!(!tenant_section.contains("tenant node summary should be replaced"));
    assert!(user_section.contains("current contact prefers blue deployment windows"));
    assert!(
        !memory_message
            .content
            .contains("other contact private memory")
    );
    assert_eq!(memory_message.source_refs.len(), 2);
    assert!(
        memory_message.source_refs.iter().any(|source| source
            .label
            .as_deref()
            .is_some_and(|label| label.starts_with("tenant_knowledge:Chunk:"))),
        "{:?}",
        memory_message.source_refs
    );
    assert!(
        memory_message.source_refs.iter().any(|source| source
            .label
            .as_deref()
            .is_some_and(|label| label.starts_with("user_memory:Fact:"))),
        "{:?}",
        memory_message.source_refs
    );

    let contact_traces = contact_lineage.retrieval_events();
    assert_eq!(contact_traces.len(), 1);
    let contact_trace = &contact_traces[0];
    assert_eq!(
        contact_trace.query_original,
        "Find the pto runbook answer and contact deployment preference answer"
    );
    assert_eq!(
        contact_trace.searched_scopes,
        vec![
            format!("tenant:{tenant_id}:tenant_knowledge"),
            format!("contact:{tenant_id}:{contact_id}:user_memory"),
        ]
    );
    assert_eq!(contact_trace.selected_hits.len(), 2);
    let tenant_hit = contact_trace
        .selected_hits
        .iter()
        .find(|hit| hit.source_tier == "tenant_knowledge")
        .expect("tenant KB hit should be selected");
    assert_eq!(tenant_hit.graph_node_uid, chunk_graph_uid);
    assert_eq!(
        tenant_hit.source_uri.as_deref(),
        Some("https://example.test/pto-runbook")
    );
    assert_eq!(tenant_hit.citation["object_type"], json!("document"));
    assert!(tenant_hit.prompt_included);
    let contact_hit = contact_trace
        .selected_hits
        .iter()
        .find(|hit| hit.source_tier == "user_memory")
        .expect("current contact memory should be selected");
    assert_eq!(contact_hit.fact_uid, Some(contact_fact_uid));
    assert_eq!(
        contact_trace
            .fusion_scores
            .iter()
            .filter(|hit| hit.vector_contribution > 0.0)
            .count(),
        2
    );
    assert_eq!(
        contact_trace
            .fusion_scores
            .iter()
            .filter(|hit| hit.lexical_contribution > 0.0)
            .count(),
        2
    );
    assert!(contact_trace.timings.total_ms > 0);

    let tenant_session = tenant_only_session(tenant_id);
    let mut tenant_ctx = moa_core::WorkingContext::new(&tenant_session, capabilities());
    tenant_ctx.append_message(ContextMessage::user("Find the pto runbook answer"));
    let tenant_lineage = Arc::new(CapturedLineage::default());
    let tenant_retriever = GraphMemoryRetriever::new(pool.clone(), Some(Arc::new(TestEmbedder)))
        .with_assume_app_role(true)
        .with_lineage(tenant_lineage.clone())
        .with_result_limit(6);
    let tenant_output = tenant_retriever
        .process(&mut tenant_ctx)
        .await
        .expect("tenant-only graph retrieval should assemble context");
    assert_eq!(
        tenant_output.items_included,
        vec![format!("graph:Chunk:{chunk_graph_uid}")]
    );
    let tenant_memory_message = tenant_ctx
        .messages
        .first()
        .expect("tenant-only memory reminder should be inserted");
    let tenant_only_section = section_between(
        &tenant_memory_message.content,
        "<tenant_knowledge>",
        "</tenant_knowledge>",
    );
    let tenant_only_user_section = section_between(
        &tenant_memory_message.content,
        "<user_memory>",
        "</user_memory>",
    );
    assert!(tenant_only_section.contains("Hydrated tenant PTO policy chunk text"));
    assert!(!tenant_only_user_section.contains("current contact prefers"));
    assert!(
        !tenant_memory_message
            .content
            .contains("other contact private memory")
    );
    let tenant_traces = tenant_lineage.retrieval_events();
    assert_eq!(tenant_traces.len(), 1);
    assert_eq!(
        tenant_traces[0].searched_scopes,
        vec![format!("tenant:{tenant_id}:tenant_knowledge")]
    );
    assert_eq!(tenant_traces[0].selected_hits.len(), 1);
    assert_eq!(
        tenant_traces[0].selected_hits[0].source_tier,
        "tenant_knowledge"
    );

    let _ = tenant_graph
        .hard_purge(chunk_graph_uid, "redacted:tenant-contact-knowledge-test")
        .await;
    let _ = contact_graph
        .hard_purge(contact_fact_uid, "redacted:tenant-contact-knowledge-test")
        .await;
    let _ = other_contact_graph
        .hard_purge(
            other_contact_fact_uid,
            "redacted:tenant-contact-knowledge-test",
        )
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn retrieval_cache_invalidates_when_knowledge_object_deactivates() {
    // Pins: knowledge object visibility changes bump the tenant cache version,
    // so a warmed retrieval cache cannot keep returning hydrated deleted-object
    // chunks.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let tenant_id = TenantId::new();
    seed_storage_partition_embedder_state(&pool, tenant_id)
        .await
        .expect("seed tenant vector embedder state");
    let tenant_graph = graph_store(pool.clone(), RlsContext::tenant(tenant_id));
    let chunk_graph_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Chunk,
            "cache object deactivation pto answer",
            json!({ "summary": "cache object deactivation pto answer" }),
        ))
        .await
        .expect("create tenant chunk graph node");
    let knowledge = seed_knowledge_chunk_with_text(
        &pool,
        tenant_id,
        chunk_graph_uid,
        "cache-object",
        "Hydrated cache object deactivation text.",
    )
    .await
    .expect("seed tenant knowledge rows");
    let cached = cached_retriever(&pool, tenant_id, &tenant_graph);
    let planned = planned_chunk_query(tenant_id, "cache object deactivation pto answer");
    let request = tenant_chunk_request(tenant_id, "cache object deactivation pto answer");

    let first = cached
        .retrieve(&planned, request.clone())
        .await
        .expect("warm cache retrieval should succeed");
    assert_eq!(
        knowledge_text(&first, chunk_graph_uid),
        Some("Hydrated cache object deactivation text."),
        "{first:?}"
    );
    let before_version = storage_partition_version(&pool, tenant_id).await;

    sqlx::query(
        "UPDATE moa.knowledge_objects SET status = 'deleted', updated_at = now() WHERE object_uid = $1",
    )
    .bind(knowledge.object_uid)
    .execute(&pool)
    .await
    .expect("deactivate knowledge object");

    let after_version = storage_partition_version(&pool, tenant_id).await;
    assert!(
        after_version > before_version,
        "object deactivation must bump retrieval cache version: before={before_version}, after={after_version}"
    );
    let second = cached
        .retrieve(&planned, request)
        .await
        .expect("post-deactivation retrieval should succeed");
    assert_eq!(
        knowledge_text(&second, chunk_graph_uid),
        None,
        "stale cache hit would keep returning hydrated deleted-object text: {second:?}"
    );

    let _ = tenant_graph
        .hard_purge(chunk_graph_uid, "redacted:retrieval-cache-object-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn retrieval_cache_invalidates_when_knowledge_chunk_tombstones() {
    // Pins: chunk-level visibility changes bump the tenant cache version, so a
    // warmed retrieval cache cannot keep returning hydrated tombstoned chunks.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let tenant_id = TenantId::new();
    seed_storage_partition_embedder_state(&pool, tenant_id)
        .await
        .expect("seed tenant vector embedder state");
    let tenant_graph = graph_store(pool.clone(), RlsContext::tenant(tenant_id));
    let chunk_graph_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Chunk,
            "cache chunk tombstone pto answer",
            json!({ "summary": "cache chunk tombstone pto answer" }),
        ))
        .await
        .expect("create tenant chunk graph node");
    let knowledge = seed_knowledge_chunk_with_text(
        &pool,
        tenant_id,
        chunk_graph_uid,
        "cache-chunk",
        "Hydrated cache chunk tombstone text.",
    )
    .await
    .expect("seed tenant knowledge rows");
    let cached = cached_retriever(&pool, tenant_id, &tenant_graph);
    let planned = planned_chunk_query(tenant_id, "cache chunk tombstone pto answer");
    let request = tenant_chunk_request(tenant_id, "cache chunk tombstone pto answer");

    let first = cached
        .retrieve(&planned, request.clone())
        .await
        .expect("warm cache retrieval should succeed");
    assert_eq!(
        knowledge_text(&first, chunk_graph_uid),
        Some("Hydrated cache chunk tombstone text."),
        "{first:?}"
    );
    let before_version = storage_partition_version(&pool, tenant_id).await;

    sqlx::query(
        r#"
        UPDATE moa.knowledge_chunks
        SET metadata = jsonb_set(metadata, '{active}', 'false'::jsonb, true),
            updated_at = now()
        WHERE chunk_uid = $1
        "#,
    )
    .bind(knowledge.chunk_uid)
    .execute(&pool)
    .await
    .expect("tombstone knowledge chunk");

    let after_version = storage_partition_version(&pool, tenant_id).await;
    assert!(
        after_version > before_version,
        "chunk tombstone must bump retrieval cache version: before={before_version}, after={after_version}"
    );
    let second = cached
        .retrieve(&planned, request)
        .await
        .expect("post-tombstone retrieval should succeed");
    assert_eq!(
        knowledge_text(&second, chunk_graph_uid),
        None,
        "stale cache hit would keep returning hydrated tombstoned chunk text: {second:?}"
    );

    let _ = tenant_graph
        .hard_purge(chunk_graph_uid, "redacted:retrieval-cache-chunk-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn retrieval_cache_invalidates_when_graph_node_invalidates() {
    // Pins: graph node invalidation already flows through graph_changelog and
    // the tenant cache version, so a warmed retrieval cache cannot keep
    // returning an invalidated graph node.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let tenant_id = TenantId::new();
    seed_storage_partition_embedder_state(&pool, tenant_id)
        .await
        .expect("seed tenant vector embedder state");
    let tenant_graph = graph_store(pool.clone(), RlsContext::tenant(tenant_id));
    let chunk_graph_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Chunk,
            "cache graph invalidation pto answer",
            json!({ "summary": "cache graph invalidation pto answer" }),
        ))
        .await
        .expect("create tenant chunk graph node");
    seed_knowledge_chunk_with_text(
        &pool,
        tenant_id,
        chunk_graph_uid,
        "cache-graph",
        "Hydrated cache graph invalidation text.",
    )
    .await
    .expect("seed tenant knowledge rows");
    let cached = cached_retriever(&pool, tenant_id, &tenant_graph);
    let planned = planned_chunk_query(tenant_id, "cache graph invalidation pto answer");
    let request = tenant_chunk_request(tenant_id, "cache graph invalidation pto answer");

    let first = cached
        .retrieve(&planned, request.clone())
        .await
        .expect("warm cache retrieval should succeed");
    assert_eq!(
        knowledge_text(&first, chunk_graph_uid),
        Some("Hydrated cache graph invalidation text."),
        "{first:?}"
    );
    let before_version = storage_partition_version(&pool, tenant_id).await;

    tenant_graph
        .invalidate_node(chunk_graph_uid, "retrieval_cache_audit")
        .await
        .expect("invalidate graph chunk node");

    let after_version = storage_partition_version(&pool, tenant_id).await;
    assert!(
        after_version > before_version,
        "graph invalidation must bump retrieval cache version: before={before_version}, after={after_version}"
    );
    let second = cached
        .retrieve(&planned, request)
        .await
        .expect("post-graph-invalidation retrieval should succeed");
    assert!(
        second.iter().all(|hit| hit.uid != chunk_graph_uid),
        "stale cache hit would keep returning invalidated node: {second:?}"
    );

    let _ = tenant_graph
        .hard_purge(chunk_graph_uid, "redacted:retrieval-cache-graph-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn vector_promotion_bumps_retrieval_cache_version() {
    // Pins: vector backend promotion is a real freshness boundary for cached
    // hybrid retrieval because vector candidate generation changes by backend
    // state even when graph rows are unchanged.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let tenant_id = TenantId::new();
    seed_storage_partition_embedder_state(&pool, tenant_id)
        .await
        .expect("seed tenant vector embedder state");
    let tenant_graph = graph_store(pool.clone(), RlsContext::tenant(tenant_id));
    let chunk_graph_uid = tenant_graph
        .create_node(node_intent(
            tenant_id,
            None,
            NodeLabel::Chunk,
            "cache vector promotion pto answer",
            json!({ "summary": "cache vector promotion pto answer" }),
        ))
        .await
        .expect("create tenant chunk graph node");
    seed_knowledge_chunk_with_text(
        &pool,
        tenant_id,
        chunk_graph_uid,
        "cache-vector",
        "Hydrated cache vector promotion text.",
    )
    .await
    .expect("seed tenant knowledge rows");
    let cached = cached_retriever(&pool, tenant_id, &tenant_graph);
    let planned = planned_chunk_query(tenant_id, "cache vector promotion pto answer");
    let request = tenant_chunk_request(tenant_id, "cache vector promotion pto answer");

    let first = cached
        .retrieve(&planned, request.clone())
        .await
        .expect("warm cache retrieval should succeed");
    assert_eq!(
        knowledge_text(&first, chunk_graph_uid),
        Some("Hydrated cache vector promotion text."),
        "{first:?}"
    );
    let before_version = storage_partition_version(&pool, tenant_id).await;

    let source: Arc<dyn VectorStore> = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        RlsContext::tenant(tenant_id),
    ));
    let promotion = VectorPartitionPromotion::new(pool.clone(), source.clone(), source);
    promotion
        .promote(PromotionOptions {
            storage_partition_id: tenant_id.to_string(),
            target_backend: "turbopuffer".to_string(),
            validate_percent: 100,
            dual_read_hours: 1,
        })
        .await
        .expect("promote vector backend");

    let after_version = storage_partition_version(&pool, tenant_id).await;
    assert!(
        after_version > before_version,
        "vector promotion must bump retrieval cache version: before={before_version}, after={after_version}"
    );
    let second = cached
        .retrieve(&planned, request)
        .await
        .expect("post-promotion retrieval should succeed");
    assert_eq!(
        knowledge_text(&second, chunk_graph_uid),
        Some("Hydrated cache vector promotion text."),
        "{second:?}"
    );

    let _ = tenant_graph
        .hard_purge(chunk_graph_uid, "redacted:retrieval-cache-vector-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

fn graph_store(pool: PgPool, scope: RlsContext) -> PostgresGraphStore {
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    PostgresGraphStore::scoped_for_app_role(pool, scope).with_vector_store(vector)
}

async fn seed_storage_partition_embedder_state(
    pool: &PgPool,
    tenant_id: TenantId,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state (
            storage_partition_id,
            embedding_model,
            embedding_model_version,
            embedding_dimension,
            reembed_state
        )
        VALUES ($1, 'embed-v4.0', 1, $2, 'steady')
        ON CONFLICT (storage_partition_id) DO UPDATE
        SET embedding_model = EXCLUDED.embedding_model,
            embedding_model_version = EXCLUDED.embedding_model_version,
            embedding_dimension = EXCLUDED.embedding_dimension,
            reembed_state = EXCLUDED.reembed_state
        "#,
    )
    .bind(tenant_id.to_string())
    .bind(VECTOR_DIMENSION as i32)
    .execute(pool)
    .await
    .map(|_| ())
}

fn node_intent(
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    label: NodeLabel,
    name: &str,
    properties: serde_json::Value,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        storage_partition_id: Some(tenant_id.to_string()),
        contact_id: contact_id.map(|id| id.to_string()),
        scope: if contact_id.is_some() {
            "contact".to_string()
        } else {
            "tenant".to_string()
        },
        name: name.to_string(),
        properties,
        pii_class: PiiClass::None,
        confidence: Some(0.95),
        valid_from: Utc::now(),
        embedding: Some(test_embedding(name)),
        embedding_model: Some("embed-v4.0".to_string()),
        embedding_model_version: Some(1),
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

#[derive(Debug, Default)]
struct TestEmbedder;

#[async_trait]
impl EmbeddingProvider for TestEmbedder {
    fn model_id(&self) -> &str {
        "embed-v4.0"
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|input| test_embedding(input)).collect())
    }
}

fn test_embedding(input: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    for (index, byte) in input.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

#[derive(Debug, Default)]
struct CapturedLineage {
    events: StdMutex<Vec<Value>>,
}

impl CapturedLineage {
    fn retrieval_events(&self) -> Vec<RetrievalLineage> {
        self.events
            .lock()
            .expect("captured lineage mutex should not be poisoned")
            .iter()
            .filter_map(
                |event| match serde_json::from_value::<LineageEvent>(event.clone()) {
                    Ok(LineageEvent::Retrieval(retrieval)) => Some(retrieval),
                    Ok(_) | Err(_) => None,
                },
            )
            .collect()
    }
}

impl LineageHandle for CapturedLineage {
    fn record(&self, evt_json: Value) {
        self.events
            .lock()
            .expect("captured lineage mutex should not be poisoned")
            .push(evt_json);
    }
}

#[derive(Debug, Clone, Copy)]
struct KnowledgeSeed {
    object_uid: Uuid,
    chunk_uid: Uuid,
}

async fn seed_knowledge_chunk(
    pool: &PgPool,
    tenant_id: TenantId,
    graph_node_uid: Uuid,
) -> sqlx::Result<KnowledgeSeed> {
    seed_knowledge_chunk_with_text(
        pool,
        tenant_id,
        graph_node_uid,
        "pto-runbook",
        "Hydrated tenant PTO policy chunk text. The runbook answer is owner approval.",
    )
    .await
}

async fn seed_knowledge_chunk_with_text(
    pool: &PgPool,
    tenant_id: TenantId,
    graph_node_uid: Uuid,
    external_id: &str,
    text: &str,
) -> sqlx::Result<KnowledgeSeed> {
    let connection_uid = Uuid::now_v7();
    let object_uid = Uuid::now_v7();
    let version_uid = Uuid::now_v7();
    let chunk_uid = Uuid::now_v7();
    let storage_partition_id = tenant_id.to_string();
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_connections (
            connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
            provider_connection_id, connector, credential_ref, status, metadata
        )
        VALUES ($1, $2, $3, 'merge', 'test-config', $4, 'drive',
                'vault://tenant-contact-test', 'active', '{}'::jsonb)
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(format!("acct-tenant-contact-{external_id}"))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_objects (
            object_uid, tenant_id, storage_partition_id, connection_id, object_type,
            external_object_id, title, change_token, source_uri, status, metadata
        )
        VALUES ($1, $2, $3, $4, 'document', $5, 'PTO Runbook',
                'etag-1', $6, 'active', '{}'::jsonb)
        "#,
    )
    .bind(object_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(connection_uid)
    .bind(external_id)
    .bind(format!("https://example.test/{external_id}"))
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_document_versions (
            document_version_uid, tenant_id, storage_partition_id, object_id,
            parser_provider, parser_job_id, content_hash, metadata
        )
        VALUES ($1, $2, $3, $4, 'native', 'native-job-1', 'content-hash-1', '{}'::jsonb)
        "#,
    )
    .bind(version_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(object_uid)
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_chunks (
            chunk_uid, tenant_id, storage_partition_id, document_version_id, graph_node_uid,
            chunk_hash, block_hashes, heading_path, text, ordinal, token_count, metadata
        )
        VALUES ($1, $2, $3, $4, $5, $6, ARRAY[$7]::text[],
                ARRAY['People', 'PTO']::text[],
                $8,
                0, 13, '{}'::jsonb)
        "#,
    )
    .bind(chunk_uid)
    .bind(tenant_id.0)
    .bind(&storage_partition_id)
    .bind(version_uid)
    .bind(graph_node_uid)
    .bind(format!("chunk-hash-{external_id}"))
    .bind(format!("block-hash-{external_id}"))
    .bind(text)
    .execute(pool)
    .await?;
    Ok(KnowledgeSeed {
        object_uid,
        chunk_uid,
    })
}

fn cached_retriever(
    pool: &PgPool,
    tenant_id: TenantId,
    graph: &PostgresGraphStore,
) -> CachedHybridRetriever {
    let vector: Arc<dyn VectorStore> = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        RlsContext::tenant(tenant_id),
    ));
    let hybrid = Arc::new(
        HybridRetriever::new(pool.clone(), Arc::new(graph.clone()), vector)
            .with_assume_app_role(true),
    );
    CachedHybridRetriever::new_for_app_role(hybrid, pool.clone())
}

fn planned_chunk_query(tenant_id: TenantId, _query: &str) -> PlannedQuery {
    let scope = tenant_memory_scope(tenant_id);
    let scope_ancestors = scope.ancestors();
    PlannedQuery {
        strategy: Strategy::Both,
        seeds: Vec::new(),
        label_hint: Some(vec![NodeLabel::Chunk]),
        scope,
        scope_ancestors,
        temporal_filter: None,
    }
}

fn tenant_chunk_request(tenant_id: TenantId, query: &str) -> RetrievalRequest {
    RetrievalRequest {
        seeds: Vec::new(),
        query_text: query.to_string(),
        query_embedding: test_embedding(query),
        scope: tenant_memory_scope(tenant_id),
        label_filter: Some(vec![NodeLabel::Chunk]),
        max_pii_class: PiiClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: Some(Strategy::Both),
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
    }
}

fn tenant_memory_scope(tenant_id: TenantId) -> MemoryScope {
    MemoryScope::Tenant { tenant_id }
}

fn knowledge_text(
    hits: &[moa_brain::retrieval::RetrievalHit],
    graph_node_uid: Uuid,
) -> Option<&str> {
    hits.iter()
        .find(|hit| hit.uid == graph_node_uid)
        .and_then(|hit| hit.knowledge_chunk.as_ref())
        .map(|chunk| chunk.text.as_str())
}

async fn storage_partition_version(pool: &PgPool, tenant_id: TenantId) -> i64 {
    let mut conn = ScopedConn::begin(pool, &RlsContext::tenant(tenant_id))
        .await
        .expect("begin cache version read");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for cache version read");
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT changelog_version FROM moa.storage_partition_state WHERE tenant_id = $1",
    )
    .bind(tenant_id.0)
    .fetch_optional(conn.as_mut())
    .await
    .expect("read storage partition version")
    .unwrap_or(0);
    conn.commit().await.expect("commit cache version read");
    version
}

fn contact_session(tenant_id: TenantId, contact_id: ContactId) -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        tenant_id,
        channel: Channel::Chat,
        model: ModelId::new("mock"),
        contact: Some(ContactRef {
            contact_id,
            tenant_id,
            state: ContactVerificationState::Verified,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: serde_json::Value::Null,
            agent_ids: Vec::new(),
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        }),
        ..SessionMeta::default()
    }
}

fn tenant_only_session(tenant_id: TenantId) -> SessionMeta {
    SessionMeta {
        id: SessionId::new(),
        tenant_id,
        channel: Channel::Chat,
        model: ModelId::new("mock"),
        contact: None,
        ..SessionMeta::default()
    }
}

fn capabilities() -> ModelCapabilities {
    ModelCapabilities {
        model_id: ModelId::new("mock"),
        context_window: 32_000,
        max_output: 1_024,
        supports_tools: true,
        supports_vision: false,
        supports_prefix_caching: false,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::OpenAiCompatible,
        pricing: TokenPricing {
            input_per_mtok: 1.0,
            output_per_mtok: 1.0,
            cached_input_per_mtok: None,
            cache_write_5m_per_mtok: None,
            cache_write_1h_per_mtok: None,
        },
        native_tools: Vec::new(),
    }
}

fn section_between<'a>(content: &'a str, start: &str, end: &str) -> &'a str {
    let start_index = content
        .find(start)
        .expect("section start marker should exist")
        + start.len();
    let end_index = content[start_index..]
        .find(end)
        .expect("section end marker should exist")
        + start_index;
    &content[start_index..end_index]
}
