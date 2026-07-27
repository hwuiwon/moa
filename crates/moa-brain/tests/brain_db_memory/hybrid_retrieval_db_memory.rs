//! Integration coverage for graph-memory hybrid retrieval.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{
    types::contact::ContactId, types::identifiers::SessionId, types::identifiers::TenantId,
};
use moa_db::ScopedConn;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore,
};
use moa_memory_ingest::{
    ExtractedFact, ExtractedFactScopeHint, IngestCtx, RrfPlusJudgeDetector, ScriptedFactExtractor,
    SessionTurn, fact_hash, fact_uid_from_hash, ingest_turn_direct_with_ctx,
};
use moa_memory_pii::{Error, PiiClassifier, PiiResult, PiiSpan};
use moa_memory_types::{FactCategory, FactEdgeLabel, MemoryScope};
use moa_memory_vector::{PgvectorStore, TurbopufferStore, VECTOR_DIMENSION};
use moa_session::testing;
use secrecy::SecretString;
use serde_json::json;
use sqlx::{PgPool, Postgres, QueryBuilder};
use tokio::sync::Mutex;
use uuid::Uuid;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use moa_retrieval::planning::{PlanningCtx, QueryPlanner, QueryRetrievalCtx};
use moa_retrieval::retrieval::{
    CachedHybridRetriever, HybridRetriever, RetrievalRequest,
    legs::{lexical_leg, vector_leg},
};

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn tenant_id_from_storage_partition_id(storage_partition_id: &str) -> TenantId {
    Uuid::parse_str(storage_partition_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id)))
}

fn contact_id_from_user_id(user_id: &str) -> ContactId {
    Uuid::parse_str(user_id)
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id)))
}

use moa_test_support::fixtures::stable_uuid_from_label;

fn test_storage_partition_id() -> String {
    Uuid::now_v7().to_string()
}

fn tenant_scope(storage_partition_id: &str) -> RlsContext {
    RlsContext::tenant(tenant_id_from_storage_partition_id(storage_partition_id))
}

fn contact_scope(storage_partition_id: &str, user_id: &str) -> RlsContext {
    RlsContext::contact(
        tenant_id_from_storage_partition_id(storage_partition_id),
        contact_id_from_user_id(user_id),
    )
}

fn tenant_memory_scope(storage_partition_id: &str) -> MemoryScope {
    MemoryScope::Tenant {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
    }
}

fn contact_memory_scope(storage_partition_id: &str, user_id: &str) -> MemoryScope {
    MemoryScope::Contact {
        tenant_id: tenant_id_from_storage_partition_id(storage_partition_id),
        contact_id: contact_id_from_user_id(user_id),
    }
}

#[tokio::test]
async fn query_retrieval_ctx_defaults_reranker_off_and_requires_explicit_opt_in() {
    // Pins: query retrieval callers do not enable reranking unless they explicitly opt in.
    let pool = PgPool::connect_lazy("postgres://unused")
        .expect("lazy pool construction should not connect");
    let storage_partition_id = "reranker-default-workspace";
    let scope = tenant_memory_scope(storage_partition_id);
    let scope_context = tenant_scope(storage_partition_id);
    let vector: Arc<PgvectorStore> = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        scope_context.clone(),
    ));
    let graph: Arc<dyn GraphStore> = Arc::new(
        PostgresGraphStore::scoped_for_app_role(pool.clone(), scope_context, super::test_kms())
            .with_vector_store(vector.clone()),
    );
    let hybrid = Arc::new(
        HybridRetriever::new(pool.clone(), graph.clone(), vector).with_assume_app_role(true),
    );
    let cached = CachedHybridRetriever::new_for_app_role(hybrid, pool);
    let planner = QueryPlanner::new();
    let planning = PlanningCtx::new(scope, graph);
    let embedder = RerankerDefaultEmbedder;

    let ctx = QueryRetrievalCtx::new(
        &planner,
        &planning,
        &embedder,
        &cached,
        SensitivityClass::Restricted,
    );

    assert!(!ctx.use_reranker);
    assert!(ctx.with_reranker(true).use_reranker);
}

#[derive(Debug)]
struct RerankerDefaultEmbedder;

#[async_trait]
impl moa_core::traits::EmbeddingProvider for RerankerDefaultEmbedder {
    fn model_id(&self) -> &str {
        "reranker-default-test"
    }

    fn model_version(&self) -> i32 {
        1
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    async fn embed(&self, texts: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| deterministic_vector(text))
            .collect())
    }
}

#[derive(Debug, Clone)]
struct FixedPiiClassifier;

#[async_trait]
impl PiiClassifier for FixedPiiClassifier {
    async fn classify(&self, _text: &str) -> Result<PiiResult, Error> {
        Ok(PiiResult {
            class: SensitivityClass::None,
            spans: Vec::<PiiSpan>::new(),
            model_version: "hybrid-retrieval-test".to_string(),
            abstained: false,
        })
    }
}

fn deterministic_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    for (index, byte) in text.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("test timestamp should be valid RFC3339")
        .with_timezone(&Utc)
}

fn graph_store(pool: &PgPool, storage_partition_id: &str) -> PostgresGraphStore {
    let scope = tenant_scope(storage_partition_id);
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    PostgresGraphStore::scoped_for_app_role(pool.clone(), scope, super::test_kms())
        .with_vector_store(Arc::new(vector))
}

fn user_graph_store(
    pool: &PgPool,
    storage_partition_id: &str,
    user_id: &str,
) -> PostgresGraphStore {
    let scope = contact_scope(storage_partition_id, user_id);
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    PostgresGraphStore::scoped_for_app_role(pool.clone(), scope, super::test_kms())
        .with_vector_store(Arc::new(vector))
}

fn scripted_user_fact(summary: &str) -> ExtractedFact {
    let mut fact = ExtractedFact {
        uid: Uuid::nil(),
        subject: "user".to_string(),
        predicate: "prefers".to_string(),
        object: summary.to_string(),
        summary: summary.to_string(),
        source_chunk: 0,
        scope_hint: ExtractedFactScopeHint::Contact,
        confidence: Some(0.92),
        event_time: None,
        category: FactCategory::Other,
        edge_label: FactEdgeLabel::RelatesTo,
        functional: false,
    };
    let hash = fact_hash(&fact).expect("scripted fact hashes");
    fact.uid = fact_uid_from_hash(&hash);
    fact
}

fn hit_summary(hit: &moa_retrieval::retrieval::RetrievalHit) -> Option<&str> {
    hit.node
        .properties_summary
        .as_ref()
        .and_then(|properties| properties.get("summary"))
        .and_then(serde_json::Value::as_str)
}

fn node_intent(
    storage_partition_id: &str,
    label: NodeLabel,
    name: &str,
    embedding: Option<Vec<f32>>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        barrier: None,
        uid: Uuid::now_v7(),
        data_subject_id: Uuid::parse_str(storage_partition_id)
            .expect("storage partition fixture should be a tenant UUID"),
        label,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "summary": name, "source": "hybrid_retrieval_test" }),
        pii_class: SensitivityClass::None,
        confidence: Some(0.9),
        valid_from: moa_test_support::fixtures::pg_now(),
        embedding,
        embedding_model: Some("test-model".to_string()),
        embedding_model_version: Some(1),
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn seed_filler_rows(pool: &PgPool, storage_partition_id: &str, prefix: &str, count: usize) {
    let data_subject_id = Uuid::parse_str(storage_partition_id)
        .expect("storage partition fixture should be a tenant UUID");
    let ctx = tenant_scope(storage_partition_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin filler seed transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");

    let mut builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO moa.node_index (uid, label, storage_partition_id, data_subject_id, name, pii_class, confidence) ",
    );
    builder.push_values(0..count, |mut row, index| {
        row.push_bind(Uuid::now_v7())
            .push_bind(NodeLabel::Fact.as_str())
            .push_bind(storage_partition_id)
            .push_bind(data_subject_id)
            .push_bind(format!("{prefix} filler operational note {index}"))
            .push_bind(SensitivityClass::None.as_str())
            .push_bind(0.5_f64);
    });
    builder
        .build()
        .execute(conn.as_mut())
        .await
        .expect("insert filler rows");
    conn.commit().await.expect("commit filler seed transaction");
}

async fn delete_filler_rows(pool: &PgPool, storage_partition_id: &str, prefix: &str) {
    sqlx::query("DELETE FROM moa.node_index WHERE storage_partition_id = $1 AND name LIKE $2")
        .bind(storage_partition_id)
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .expect("delete filler rows");
}

async fn set_storage_partition_vector_backend(
    pool: &PgPool,
    storage_partition_id: &str,
    backend: &str,
) {
    let ctx = tenant_scope(storage_partition_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin storage_partition_state transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state (storage_partition_id, vector_backend, vector_backend_state)
        VALUES ($1, $2, 'steady')
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET vector_backend = EXCLUDED.vector_backend,
                vector_backend_state = EXCLUDED.vector_backend_state
        "#,
    )
    .bind(storage_partition_id)
    .bind(backend)
    .execute(conn.as_mut())
    .await
    .expect("set storage-partition vector backend");
    conn.commit()
        .await
        .expect("commit storage_partition_state transaction");
}

async fn set_storage_partition_vector_backend_state(
    pool: &PgPool,
    storage_partition_id: &str,
    backend: &str,
    backend_state: &str,
    dual_read_until: Option<DateTime<Utc>>,
) {
    let ctx = tenant_scope(storage_partition_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin storage_partition_state transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, vector_backend, vector_backend_state, dual_read_until)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET vector_backend = EXCLUDED.vector_backend,
                vector_backend_state = EXCLUDED.vector_backend_state,
                dual_read_until = EXCLUDED.dual_read_until
        "#,
    )
    .bind(storage_partition_id)
    .bind(backend)
    .bind(backend_state)
    .bind(dual_read_until)
    .execute(conn.as_mut())
    .await
    .expect("set storage-partition vector backend state");
    conn.commit()
        .await
        .expect("commit storage_partition_state transaction");
}

async fn set_workspace_embedder_state(pool: &PgPool, storage_partition_id: &str, model: &str) {
    let ctx = tenant_scope(storage_partition_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin workspace embedder transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, 1, $3)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(storage_partition_id)
    .bind(model)
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .expect("set workspace embedder state");
    conn.commit()
        .await
        .expect("commit workspace embedder transaction");
}

async fn seed_knowledge_document_chunks(
    pool: &PgPool,
    storage_partition_id: &str,
    object_slug: &str,
    title: &str,
    source_uri: &str,
    chunks: &[(Uuid, String)],
) {
    let tenant_id = tenant_id_from_storage_partition_id(storage_partition_id);
    let connection_uid = Uuid::now_v7();
    let object_uid = Uuid::now_v7();
    let version_uid = Uuid::now_v7();

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_connections (
            connection_uid, tenant_id, storage_partition_id, provider, provider_config_key,
            provider_connection_id, connector, credential_ref, status, metadata
        )
        VALUES ($1, $2, $3, 'merge', $4, $5, 'drive',
                'vault://hybrid-duplicate-test', 'active', '{}'::jsonb)
        "#,
    )
    .bind(connection_uid)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .bind(format!("duplicate-crowding-{object_slug}"))
    .bind(format!("acct-{object_slug}"))
    .execute(pool)
    .await
    .expect("insert duplicate-crowding knowledge connection");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_objects (
            object_uid, tenant_id, storage_partition_id, connection_id, object_type,
            external_object_id, title, change_token, source_uri, status, metadata
        )
        VALUES ($1, $2, $3, $4, 'document', $5, $6,
                'etag-1', $7, 'active', '{}'::jsonb)
        "#,
    )
    .bind(object_uid)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .bind(connection_uid)
    .bind(object_slug)
    .bind(title)
    .bind(source_uri)
    .execute(pool)
    .await
    .expect("insert duplicate-crowding knowledge object");

    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_document_versions (
            document_version_uid, tenant_id, storage_partition_id, object_id,
            parser_provider, parser_job_id, content_hash, metadata
        )
        VALUES ($1, $2, $3, $4, 'native', 'native-job-duplicate-crowding', $5, '{}'::jsonb)
        "#,
    )
    .bind(version_uid)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .bind(object_uid)
    .bind(format!("content-{object_slug}"))
    .execute(pool)
    .await
    .expect("insert duplicate-crowding document version");

    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        INSERT INTO moa.knowledge_chunks (
            chunk_uid, tenant_id, storage_partition_id, document_version_id, graph_node_uid,
            chunk_hash, block_hashes, heading_path, text, ordinal, token_count, metadata
        )
        "#,
    );
    builder.push_values(
        chunks.iter().enumerate(),
        |mut row, (index, (graph_uid, text))| {
            // One chunk row is one graph occurrence: the seeded graph node uid IS
            // the chunk uid, which is what storage enforces.
            row.push_bind(*graph_uid)
                .push_bind(tenant_id.0)
                .push_bind(storage_partition_id)
                .push_bind(version_uid)
                .push_bind(*graph_uid)
                // Content identity follows the text, so two documents carrying the
                // same paragraph really do carry the same content hash.
                .push_bind(format!("chunk-hash-{text}"))
                .push_bind(vec![format!("block-{object_slug}-{index}")])
                .push_bind(vec!["Diagnostics".to_string(), title.to_string()])
                .push_bind(text)
                .push_bind(i32::try_from(index).expect("test chunk ordinal fits in i32"))
                .push_bind(i32::try_from(text.len() / 4).expect("test token count fits in i32"))
                .push_bind(json!({}));
        },
    );
    builder
        .build()
        .execute(pool)
        .await
        .expect("insert duplicate-crowding knowledge chunks");
}

#[tokio::test]
async fn hybrid_retrieval_db_memory_returns_fused_annotated_results() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let prefix = format!("hybrid-e2e-{}", Uuid::now_v7().simple());
    let graph = graph_store(session_store.pool(), &storage_partition_id);

    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;
    seed_filler_rows(session_store.pool(), &storage_partition_id, &prefix, 1_000).await;

    let seed = node_intent(
        &storage_partition_id,
        NodeLabel::Entity,
        "auth service deployment entity",
        None,
    );
    let seed_uid = graph.create_node(seed).await.expect("create seed node");
    let exact_text = "auth service deployment provider is railway";
    let exact = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        exact_text,
        Some(deterministic_vector(exact_text)),
    );
    let exact_uid = graph.create_node(exact).await.expect("create exact fact");
    let related_text = "auth service uses JWT access tokens";
    let related = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        related_text,
        Some(deterministic_vector(related_text)),
    );
    let related_uid = graph
        .create_node(related)
        .await
        .expect("create related fact");
    for end_uid in [exact_uid, related_uid] {
        graph
            .create_edge(EdgeWriteIntent {
                uid: Uuid::now_v7(),
                label: EdgeLabel::RelatesTo,
                start_uid: seed_uid,
                end_uid,
                valid_from: moa_test_support::fixtures::pg_now(),
                properties: json!({ "source": "hybrid_retrieval_test" }),
                storage_partition_id: Some(storage_partition_id.clone()),
                contact_id: None,
                scope: "tenant".to_string(),
                actor_id: Uuid::now_v7().to_string(),
                actor_kind: "system".to_string(),
            })
            .await
            .expect("create graph edge");
    }

    let scope = tenant_memory_scope(&storage_partition_id);
    let vector: Arc<PgvectorStore> = Arc::new(PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    ));
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        vector.clone(),
    )
    .with_assume_app_role(true);
    let request = RetrievalRequest {
        cleared_barriers: Default::default(),
        seeds: vec![seed_uid],
        query_text: exact_text.to_string(),
        query_embedding: deterministic_vector(exact_text),
        scope,
        label_filter: Some(vec![NodeLabel::Fact]),
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
    };
    let lexical_hits = lexical_leg(session_store.pool(), &request, true)
        .await
        .expect("lexical leg should retrieve exact fact");
    assert!(
        lexical_hits.iter().any(|hit| hit.uid == exact_uid),
        "{lexical_hits:?}"
    );
    let vector_hits = vector_leg(vector.as_ref(), &request)
        .await
        .expect("vector leg should retrieve exact fact");
    assert!(
        vector_hits.iter().any(|hit| hit.uid == exact_uid),
        "{vector_hits:?}"
    );

    let hits = retriever
        .retrieve(request)
        .await
        .expect("retrieve hybrid hits");

    assert!(!hits.is_empty());
    assert!(hits.len() <= 5);
    let exact_hit = hits
        .iter()
        .find(|hit| hit.uid == exact_uid)
        .expect("exact fact should be retrieved");
    assert!(exact_hit.legs.graph, "{exact_hit:?}");
    assert!(exact_hit.legs.vector, "{exact_hit:?}");
    assert_eq!(exact_hit.node.scope, "tenant");

    let graph_only_hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: vec![seed_uid],
            query_text: String::new(),
            query_embedding: Vec::new(),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Fact]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve graph-only entity seed hits");
    let graph_only_fact = graph_only_hits
        .iter()
        .find(|hit| hit.uid == related_uid)
        .expect("entity seed should reach related fact through graph leg");
    assert!(graph_only_fact.legs.graph, "{graph_only_fact:?}");
    assert!(!graph_only_fact.legs.vector, "{graph_only_fact:?}");
    assert!(!graph_only_fact.legs.lexical, "{graph_only_fact:?}");

    let graph_disabled_hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: vec![seed_uid],
            query_text: String::new(),
            query_embedding: Vec::new(),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Fact]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve with graph expansion disabled");
    assert!(
        graph_disabled_hits.iter().all(|hit| hit.uid != related_uid),
        "{graph_disabled_hits:?}"
    );

    delete_filler_rows(session_store.pool(), &storage_partition_id, &prefix).await;
    let _ = graph.hard_purge(exact_uid, "redacted:hybrid-test").await;
    let _ = graph.hard_purge(related_uid, "redacted:hybrid-test").await;
    let _ = graph.hard_purge(seed_uid, "redacted:hybrid-test").await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn barrier_cleared_agent_retrieves_node_uncleared_fails_closed_db_memory() {
    // Pins: the agent's cleared-barrier set on a `RetrievalRequest` threads
    // through the scoped retrieval legs' `moa.cleared_barriers` GUC end to end, so
    // a barriered node is retrieved only when the request is cleared for its tag.
    // An empty clearance (fail closed) and a non-matching clearance both hide it,
    // while a NULL-barrier sibling stays visible throughout. This proves the
    // agent-policy -> request -> GUC threading over the production `retrieve`
    // path; lower-level tests already prove the underlying RLS at the SQL level. Mutation
    // check: revert the final `hydrate_nodes` clearances in `hybrid.rs` back to
    // `&[]` and the cleared-agent assertion below fails.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let barriered_text = "deal alpha restricted acquisition memo";
    let mut barriered = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        barriered_text,
        Some(deterministic_vector(barriered_text)),
    );
    barriered.barrier = Some(
        moa_core::types::memory::InformationBarrierId::parse("deal-alpha").expect("valid barrier"),
    );
    let barriered_uid = graph
        .create_node(barriered)
        .await
        .expect("create barriered node");

    let public_text = "deal alpha public roster note";
    let public = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        public_text,
        Some(deterministic_vector(barriered_text)),
    );
    let public_uid = graph.create_node(public).await.expect("create public node");

    let scope = tenant_memory_scope(&storage_partition_id);
    let vector: Arc<PgvectorStore> = Arc::new(PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    ));
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        vector,
    )
    .with_assume_app_role(true);

    let make_request = |cleared: Vec<&str>| RetrievalRequest {
        cleared_barriers: cleared
            .into_iter()
            .map(|barrier| {
                moa_core::types::memory::InformationBarrierId::parse(barrier)
                    .expect("valid barrier")
            })
            .collect(),
        seeds: Vec::new(),
        query_text: barriered_text.to_string(),
        query_embedding: deterministic_vector(barriered_text),
        scope: scope.clone(),
        label_filter: Some(vec![NodeLabel::Fact]),
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 10,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
    };

    let cleared_hits = retriever
        .retrieve(make_request(vec!["deal-alpha"]))
        .await
        .expect("cleared agent retrieval");
    assert!(
        cleared_hits.iter().any(|hit| hit.uid == barriered_uid),
        "an agent cleared for deal-alpha must retrieve its barriered node: {cleared_hits:?}"
    );
    assert!(
        cleared_hits.iter().any(|hit| hit.uid == public_uid),
        "the null-barrier sibling must be visible to the cleared agent: {cleared_hits:?}"
    );

    let uncleared_hits = retriever
        .retrieve(make_request(Vec::new()))
        .await
        .expect("uncleared agent retrieval");
    assert!(
        uncleared_hits.iter().all(|hit| hit.uid != barriered_uid),
        "empty clearance must hide the barriered node (fail closed): {uncleared_hits:?}"
    );
    assert!(
        uncleared_hits.iter().any(|hit| hit.uid == public_uid),
        "the null-barrier sibling must stay visible under empty clearance: {uncleared_hits:?}"
    );

    let wrong_hits = retriever
        .retrieve(make_request(vec!["deal-beta"]))
        .await
        .expect("non-matching clearance retrieval");
    assert!(
        wrong_hits.iter().all(|hit| hit.uid != barriered_uid),
        "a non-matching clearance must not reveal the barriered node: {wrong_hits:?}"
    );

    let _ = graph
        .hard_purge(barriered_uid, "redacted:barrier-test")
        .await;
    let _ = graph.hard_purge(public_uid, "redacted:barrier-test").await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn duplicate_crowding_keeps_distinct_supporting_knowledge_chunk() {
    // Pins: duplicate-heavy tenant knowledge from one document must not crowd
    // a distinct supporting chunk out of the final context window.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(&pool, &storage_partition_id);
    let query = "duplicate crowding diagnostic primary mitigation escalation owner";
    let duplicate_text = "duplicate crowding diagnostic primary mitigation rerank cap";
    let distinct_text = "duplicate crowding diagnostic escalation owner retrieval team";
    let reference_time = utc("2026-06-01T00:00:00Z");

    set_workspace_embedder_state(&pool, &storage_partition_id, "test-model").await;

    let mut duplicate_chunks = Vec::new();
    let mut duplicate_uids = Vec::new();
    for index in 0..5 {
        let text = format!("{duplicate_text} duplicate paragraph {}", index + 1);
        let mut intent = node_intent(
            &storage_partition_id,
            NodeLabel::Chunk,
            &text,
            Some(deterministic_vector(query)),
        );
        intent.valid_from = reference_time - chrono::Duration::days(1);
        let uid = graph
            .create_node(intent)
            .await
            .expect("create duplicate chunk node");
        duplicate_uids.push(uid);
        duplicate_chunks.push((uid, text));
    }
    seed_knowledge_document_chunks(
        &pool,
        &storage_partition_id,
        "duplicate-diagnostic-source",
        "Duplicate Diagnostic Source",
        "https://example.test/duplicate-diagnostic",
        &duplicate_chunks,
    )
    .await;

    let mut distinct = node_intent(
        &storage_partition_id,
        NodeLabel::Chunk,
        distinct_text,
        Some(deterministic_vector(query)),
    );
    distinct.valid_from = reference_time - chrono::Duration::days(180);
    let distinct_uid = graph
        .create_node(distinct)
        .await
        .expect("create distinct supporting chunk node");
    seed_knowledge_document_chunks(
        &pool,
        &storage_partition_id,
        "distinct-supporting-source",
        "Distinct Supporting Source",
        "https://example.test/distinct-supporting",
        &[(distinct_uid, distinct_text.to_string())],
    )
    .await;

    let vector = PgvectorStore::new_for_app_role(pool.clone(), tenant_scope(&storage_partition_id));
    let retriever = HybridRetriever::new(pool.clone(), Arc::new(graph.clone()), Arc::new(vector))
        .with_assume_app_role(true);
    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: query.to_string(),
            query_embedding: deterministic_vector(query),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 3,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: Some(reference_time),
            lineage: None,
            disable_leg_timeouts: true,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve duplicate-heavy knowledge chunks");

    assert_eq!(hits.len(), 3, "{hits:#?}");
    let duplicate_count = hits
        .iter()
        .filter(|hit| duplicate_uids.contains(&hit.uid))
        .count();
    assert!(
        hits.iter().any(|hit| hit.uid == distinct_uid),
        "distinct supporting chunk was crowded out; duplicate_count={duplicate_count}; hits={hits:#?}"
    );
    assert!(
        duplicate_count <= 2,
        "one duplicate source exceeded the final-hit cap: {hits:#?}"
    );

    for uid in duplicate_uids.into_iter().chain([distinct_uid]) {
        let _ = graph
            .hard_purge(uid, "redacted:hybrid-duplicate-crowding-test")
            .await;
    }
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn identical_text_in_two_documents_hydrates_each_source_occurrence_db_memory() {
    // Pins: byte-identical text in two documents is two occurrences with their own
    // graph uids, and ONE retrieval hydrates both against their own document
    // version, source uri, and source title. Collapsing hydration candidates by
    // graph uid — the newest-version `DISTINCT ON` this task removed — would serve
    // one document's text under the other document's citation.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(&pool, &storage_partition_id);
    let shared_text = "occurrence hydration reimbursement requires manager approval";
    let query = shared_text;

    set_workspace_embedder_state(&pool, &storage_partition_id, "test-model").await;

    let mut occurrences = Vec::new();
    for (slug, title, source_uri) in [
        (
            "occurrence-handbook",
            "Employee Handbook",
            "https://example.test/handbook",
        ),
        (
            "occurrence-finance-policy",
            "Finance Policy",
            "https://example.test/finance-policy",
        ),
    ] {
        let uid = graph
            .create_node(node_intent(
                &storage_partition_id,
                NodeLabel::Chunk,
                shared_text,
                Some(deterministic_vector(query)),
            ))
            .await
            .expect("create occurrence chunk node");
        seed_knowledge_document_chunks(
            &pool,
            &storage_partition_id,
            slug,
            title,
            source_uri,
            &[(uid, shared_text.to_string())],
        )
        .await;
        occurrences.push((uid, title, source_uri));
    }

    let vector = PgvectorStore::new_for_app_role(pool.clone(), tenant_scope(&storage_partition_id));
    let retriever = HybridRetriever::new(pool.clone(), Arc::new(graph.clone()), Arc::new(vector))
        .with_assume_app_role(true);
    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: query.to_string(),
            query_embedding: deterministic_vector(query),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: true,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve both occurrences of the shared paragraph");

    let mut hydrated = Vec::new();
    for (uid, title, source_uri) in &occurrences {
        let hit = hits
            .iter()
            .find(|hit| hit.uid == *uid)
            .unwrap_or_else(|| panic!("occurrence {uid} must be retrievable: {hits:#?}"));
        let chunk = hit
            .knowledge_chunk
            .as_ref()
            .unwrap_or_else(|| panic!("occurrence {uid} must hydrate: {hit:#?}"));
        assert_eq!(chunk.chunk_uid, *uid, "hydration keeps occurrence identity");
        assert_eq!(chunk.text, shared_text);
        assert_eq!(chunk.source_title.as_deref(), Some(*title));
        assert_eq!(chunk.source_uri.as_deref(), Some(*source_uri));
        hydrated.push((chunk.object_uid, chunk.document_version_uid));
    }
    assert_eq!(hydrated.len(), 2);
    assert_ne!(
        hydrated[0].0, hydrated[1].0,
        "each occurrence must cite its own source object"
    );
    assert_ne!(
        hydrated[0].1, hydrated[1].1,
        "each occurrence must cite its own document version"
    );

    for (uid, _, _) in occurrences {
        let _ = graph
            .hard_purge(uid, "redacted:hybrid-occurrence-hydration-test")
            .await;
    }
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn parent_document_retrieval_hydrates_ordinal_adjacent_neighbors() {
    // Pins: a matched middle chunk hydrates its ordinal ±1 siblings from the same
    // document version into `context_window`, in ascending ordinal order, and
    // never includes the matched chunk itself.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(&pool, &storage_partition_id);
    set_workspace_embedder_state(&pool, &storage_partition_id, "test-model").await;

    let query = "parent document retrieval middle chunk escalation owner";
    let before_text = "Parent document intro overview of the runbook.".to_string();
    let matched_text = format!("{query} detailed mitigation steps.");
    let after_text = "Parent document appendix with related references.".to_string();

    let before_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Chunk,
            &before_text,
            Some(deterministic_vector(&before_text)),
        ))
        .await
        .expect("create before chunk node");
    let matched_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Chunk,
            &matched_text,
            Some(deterministic_vector(query)),
        ))
        .await
        .expect("create matched chunk node");
    let after_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Chunk,
            &after_text,
            Some(deterministic_vector(&after_text)),
        ))
        .await
        .expect("create after chunk node");

    // One document version carrying ordinals 0, 1, 2 in insertion order.
    seed_knowledge_document_chunks(
        &pool,
        &storage_partition_id,
        "parent-document-source",
        "Parent Document Source",
        "https://example.test/parent-document",
        &[
            (before_uid, before_text.clone()),
            (matched_uid, matched_text.clone()),
            (after_uid, after_text.clone()),
        ],
    )
    .await;

    let vector = PgvectorStore::new_for_app_role(pool.clone(), tenant_scope(&storage_partition_id));
    let retriever = HybridRetriever::new(pool.clone(), Arc::new(graph.clone()), Arc::new(vector))
        .with_assume_app_role(true);
    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: query.to_string(),
            query_embedding: deterministic_vector(query),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: true,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve parent-document chunks");

    let matched_hit = hits
        .iter()
        .find(|hit| hit.uid == matched_uid)
        .expect("matched middle chunk retrieved");
    let chunk = matched_hit
        .knowledge_chunk
        .as_ref()
        .expect("matched chunk hydrated");
    assert_eq!(chunk.ordinal, 1);
    let window_ordinals = chunk
        .context_window
        .iter()
        .map(|part| part.ordinal)
        .collect::<Vec<_>>();
    assert_eq!(window_ordinals, vec![0, 2], "{:#?}", chunk.context_window);
    assert_eq!(chunk.context_window[0].text, before_text);
    assert_eq!(chunk.context_window[1].text, after_text);
    assert!(
        chunk
            .context_window
            .iter()
            .all(|part| part.text != matched_text),
        "the matched chunk must not appear in its own context window"
    );

    for uid in [before_uid, matched_uid, after_uid] {
        let _ = graph.hard_purge(uid, "redacted:parent-document-test").await;
    }
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
#[ignore = "requires Postgres test database"]
async fn reinforced_fact_survives_consolidation_while_idle_one_off_expires_from_retrieval() {
    // Pins: the full freshness loop across production paths — a fact restated in
    // a later turn is reinforced at write time (confidence capped, decay anchor
    // reset); after months of idleness a one-off fact decays to the floor and
    // is bitemporally expired by consolidation; live retrieval then surfaces
    // the reinforced fact but not the expired one, while the expired row stays
    // readable as history.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let storage_partition_id = test_storage_partition_id();
    let tenant_id = tenant_id_from_storage_partition_id(&storage_partition_id);
    let user = "freshness-user";
    let workspace_scope = tenant_scope(&storage_partition_id);

    let spo_fact = |subject: &str, predicate: &str, object: &str| {
        let mut fact = ExtractedFact {
            uid: Uuid::nil(),
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            summary: format!("{subject} {predicate} {object}"),
            source_chunk: 0,
            scope_hint: ExtractedFactScopeHint::Contact,
            confidence: Some(0.92),
            event_time: None,
            category: FactCategory::Other,
            edge_label: FactEdgeLabel::RelatesTo,
            functional: false,
        };
        let hash = fact_hash(&fact).expect("scripted fact hashes");
        fact.uid = fact_uid_from_hash(&hash);
        fact
    };
    // Distinct subjects and predicates so the contradiction detector routes
    // both as inserts rather than superseding one with the other.
    let retained = spo_fact("workspace-home", "defaults_to", "dashboards overview");
    let one_off = spo_fact("quarterly-report", "generated_by", "legacy reporting tool");
    set_workspace_embedder_state(&pool, &storage_partition_id, "reranker-default-test").await;

    let ingest_ctx = |facts: Vec<ExtractedFact>| {
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            pool.clone(),
            workspace_scope.clone(),
        ));
        let graph = Arc::new(
            PostgresGraphStore::scoped_for_app_role(
                pool.clone(),
                workspace_scope.clone(),
                super::test_kms(),
            )
            .with_vector_store(vector.clone()),
        );
        IngestCtx::new(
            pool.clone(),
            super::test_kms(),
            graph,
            vector,
            Arc::new(RerankerDefaultEmbedder),
            Arc::new(FixedPiiClassifier),
            Arc::new(RrfPlusJudgeDetector::default()),
        )
        .with_extractor(Arc::new(ScriptedFactExtractor::new(facts)))
    };
    let turn = |turn_seq: u64, finalized_at: &str| SessionTurn {
        tenant_id,
        contact_id: Some(contact_id_from_user_id(user)),
        session_id: SessionId::new(),
        turn_seq,
        transcript: "conversational transcript".to_string(),
        dominant_pii_class: "none".to_string(),
        finalized_at: utc(finalized_at),
        barrier: None,
    };

    let first = ingest_turn_direct_with_ctx(
        ingest_ctx(vec![retained.clone(), one_off.clone()]),
        turn(1, "2025-06-01T12:00:00Z"),
    )
    .await
    .expect("initial turn ingests");
    assert_eq!(first.inserted, 2, "unexpected report: {first:?}");

    // A later turn restates the retained fact verbatim: the detector routes it
    // as a duplicate and reinforcement confirms the survivor.
    let second = ingest_turn_direct_with_ctx(
        ingest_ctx(vec![retained.clone()]),
        turn(2, "2025-09-01T12:00:00Z"),
    )
    .await
    .expect("restating turn ingests");
    assert_eq!(second.reinforced, 1);
    assert_eq!(second.inserted, 0);

    // Simulate two idle years for the one-off fact; the retained fact keeps
    // the fresh access stamp reinforcement just gave it.
    let now = moa_test_support::fixtures::pg_now();
    let aged = sqlx::query(
        "UPDATE moa.node_index SET last_accessed_at = $2 \
         WHERE tenant_id = $1 AND label = 'Fact' AND name = 'quarterly-report'",
    )
    .bind(tenant_id.0)
    .bind(now - chrono::Duration::days(720))
    .execute(&pool)
    .await
    .expect("age one-off fact");
    assert_eq!(aged.rows_affected(), 1, "one-off fact row should age");

    let outcome = moa_memory_lifecycle::consolidate_tenant(
        &pool,
        super::test_kms(),
        tenant_id,
        moa_memory_lifecycle::ConsolidationOptions::default(),
        now,
        None,
    )
    .await
    .expect("consolidation pass runs");
    assert_eq!(outcome.decayed, 1, "only the idle one-off decays");
    assert_eq!(outcome.at_floor, 1, "720 idle days bottom out at the floor");
    assert_eq!(outcome.expired_idle, 1, "floor-bound idle fact expires");

    // Live retrieval: the reinforced fact answers, the expired one is gone.
    let contact_graph = user_graph_store(&pool, &storage_partition_id, user);
    let contact_vector =
        PgvectorStore::new_for_app_role(pool.clone(), contact_scope(&storage_partition_id, user));
    let retriever = HybridRetriever::new(
        pool.clone(),
        Arc::new(contact_graph),
        Arc::new(contact_vector),
    )
    .with_assume_app_role(true);
    let request = |query: &str| RetrievalRequest {
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: query.to_string(),
        query_embedding: deterministic_vector(query),
        scope: contact_memory_scope(&storage_partition_id, user),
        label_filter: Some(vec![NodeLabel::Fact]),
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 25,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: true,
        disable_graph_expansion: false,
        window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
    };
    let retained_hits = retriever
        .retrieve(request(&retained.summary))
        .await
        .expect("retained retrieval succeeds");
    assert!(
        retained_hits
            .iter()
            .any(|hit| hit_summary(hit) == Some(retained.summary.as_str())),
        "reinforced fact must stay retrievable: {retained_hits:?}"
    );
    let expired_hits = retriever
        .retrieve(request(&one_off.summary))
        .await
        .expect("expired retrieval succeeds");
    assert!(
        expired_hits
            .iter()
            .all(|hit| hit_summary(hit) != Some(one_off.summary.as_str())),
        "expired fact must not reach live retrieval: {expired_hits:?}"
    );

    // Reinforcement capped confidence and expiry preserved history.
    let (retained_confidence, retained_valid_to): (Option<f64>, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT confidence, valid_to FROM moa.node_index \
             WHERE tenant_id = $1 AND label = 'Fact' AND name = 'workspace-home'",
        )
        .bind(tenant_id.0)
        .fetch_one(&pool)
        .await
        .expect("read retained fact");
    let (expired_valid_to, expired_reason): (Option<DateTime<Utc>>, Option<String>) =
        sqlx::query_as(
            "SELECT valid_to, invalidated_reason FROM moa.node_index \
             WHERE tenant_id = $1 AND label = 'Fact' AND name = 'quarterly-report'",
        )
        .bind(tenant_id.0)
        .fetch_one(&pool)
        .await
        .expect("read expired fact");
    assert_eq!(retained_valid_to, None, "reinforced fact stays active");
    assert!(
        (retained_confidence.expect("confidence set") - 0.95).abs() < 1e-9,
        "reinforcement steps 0.92 to the 0.95 cap"
    );
    assert_eq!(
        expired_valid_to,
        Some(now),
        "expiry closes at the pass instant"
    );
    assert_eq!(
        expired_reason.as_deref(),
        Some(moa_memory_lifecycle::EXPIRED_IDLE_REASON)
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
#[ignore = "requires Postgres test database"]
async fn user_scope_fact_invisible_to_other_user_at_any_k() {
    // Pins: contact-scoped facts written by ingestion are structurally hidden from other contacts.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let storage_partition_id = test_storage_partition_id();
    let user_a = "user-scope-owner";
    let user_b = "user-scope-other";
    let workspace_scope = tenant_scope(&storage_partition_id);
    let ingest_vector = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        workspace_scope.clone(),
    ));
    let ingest_graph = Arc::new(
        PostgresGraphStore::scoped_for_app_role(pool.clone(), workspace_scope, super::test_kms())
            .with_vector_store(ingest_vector.clone()),
    );
    let summary = "The user prefers the private green deployment dashboard";
    let fact = scripted_user_fact(summary);
    let ctx = IngestCtx::new(
        pool.clone(),
        super::test_kms(),
        ingest_graph,
        ingest_vector,
        Arc::new(RerankerDefaultEmbedder),
        Arc::new(FixedPiiClassifier),
        Arc::new(RrfPlusJudgeDetector::default()),
    )
    .with_extractor(Arc::new(ScriptedFactExtractor::new(vec![fact.clone()])));
    let report = ingest_turn_direct_with_ctx(
        ctx,
        SessionTurn {
            tenant_id: tenant_id_from_storage_partition_id(&storage_partition_id),
            contact_id: Some(contact_id_from_user_id(user_a)),
            session_id: SessionId::new(),
            turn_seq: 1,
            transcript: format!("user: {summary}"),
            dominant_pii_class: "none".to_string(),
            finalized_at: utc("2026-05-07T12:00:00Z"),
            barrier: None,
        },
    )
    .await
    .expect("ingest contact-scoped fact");
    assert_eq!(report.inserted, 1);

    let owner_graph = user_graph_store(&pool, &storage_partition_id, user_a);
    let owner_scope = contact_scope(&storage_partition_id, user_a);
    let owner_vector = PgvectorStore::new_for_app_role(pool.clone(), owner_scope);
    let owner_hits =
        HybridRetriever::new(pool.clone(), Arc::new(owner_graph), Arc::new(owner_vector))
            .with_assume_app_role(true)
            .retrieve(RetrievalRequest {
                cleared_barriers: Default::default(),
                seeds: Vec::new(),
                query_text: summary.to_string(),
                query_embedding: deterministic_vector(summary),
                scope: contact_memory_scope(&storage_partition_id, user_a),
                label_filter: Some(vec![NodeLabel::Fact]),
                label_boost: None,
                max_pii_class: SensitivityClass::Restricted,
                k_final: 25,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: true,
                disable_graph_expansion: false,
                window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
            })
            .await
            .expect("owner retrieval succeeds");
    assert!(
        owner_hits
            .iter()
            .any(|hit| hit_summary(hit) == Some(summary)),
        "{owner_hits:?}"
    );

    let other_graph = user_graph_store(&pool, &storage_partition_id, user_b);
    let other_scope = contact_scope(&storage_partition_id, user_b);
    let other_vector = PgvectorStore::new_for_app_role(pool.clone(), other_scope);
    let other_hits =
        HybridRetriever::new(pool.clone(), Arc::new(other_graph), Arc::new(other_vector))
            .with_assume_app_role(true)
            .retrieve(RetrievalRequest {
                cleared_barriers: Default::default(),
                seeds: Vec::new(),
                query_text: summary.to_string(),
                query_embedding: deterministic_vector(summary),
                scope: contact_memory_scope(&storage_partition_id, user_b),
                label_filter: Some(vec![NodeLabel::Fact]),
                label_boost: None,
                max_pii_class: SensitivityClass::Restricted,
                k_final: 25,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: true,
                disable_graph_expansion: false,
                window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
            })
            .await
            .expect("other-user retrieval succeeds");
    assert!(
        other_hits
            .iter()
            .all(|hit| hit_summary(hit) != Some(summary)),
        "{other_hits:?}"
    );

    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn temporal_retrieval_returns_superseded_node_as_of_valid_window() {
    // Pins: hybrid retrieval hydrates superseded sidecar rows when `as_of` falls inside validity.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);

    let old_name = "temporal-asof retired gateway owner";
    let mut old = node_intent(&storage_partition_id, NodeLabel::Fact, old_name, None);
    old.valid_from = utc("2026-02-01T00:00:00Z");
    let old_uid = graph.create_node(old).await.expect("create old fact");

    let mut replacement = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        "temporal-asof replacement gateway owner",
        None,
    );
    replacement.valid_from = utc("2026-04-01T00:00:00Z");
    let replacement_uid = graph
        .supersede_node(old_uid, replacement.clone())
        .await
        .expect("supersede old fact");

    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_assume_app_role(true);
    let scope = tenant_memory_scope(&storage_partition_id);
    let historical = RetrievalRequest {
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: old_name.to_string(),
        query_embedding: Vec::new(),
        scope: scope.clone(),
        label_filter: Some(vec![NodeLabel::Fact]),
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: None,
        as_of: Some(utc("2026-03-01T00:00:00Z")),
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
    };

    let lexical_hits = lexical_leg(session_store.pool(), &historical, true)
        .await
        .expect("lexical leg should retrieve historical fact");
    assert_eq!(lexical_hits.first().map(|hit| hit.uid), Some(old_uid));

    let hits = retriever
        .retrieve(historical)
        .await
        .expect("retrieve historical hit");
    assert_eq!(hits.first().map(|hit| hit.uid), Some(old_uid));
    assert_eq!(hits[0].node.valid_to, Some(replacement.valid_from));

    let current = RetrievalRequest {
        cleared_barriers: Default::default(),
        seeds: Vec::new(),
        query_text: old_name.to_string(),
        query_embedding: Vec::new(),
        scope,
        label_filter: Some(vec![NodeLabel::Fact]),
        label_boost: None,
        max_pii_class: SensitivityClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        lineage: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
        window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
    };
    let current_hits = retriever
        .retrieve(current)
        .await
        .expect("retrieve current hits");
    assert!(
        current_hits.iter().all(|hit| hit.uid != old_uid),
        "{current_hits:?}"
    );

    let _ = graph
        .hard_purge(replacement_uid, "redacted:hybrid-temporal-test")
        .await;
    let _ = graph
        .hard_purge(old_uid, "redacted:hybrid-temporal-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn temporal_turbopuffer_as_of_uses_pgvector_without_calling_turbopuffer() {
    // Pins: temporal hybrid vector retrieval routes directly to pgvector for
    // Turbopuffer workspaces because external projections are current-read only.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;
    let fact = "temporal turbopuffer fallback pgvector fact";
    let mut intent = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        fact,
        Some(deterministic_vector(fact)),
    );
    intent.valid_from = utc("2026-02-01T00:00:00Z");
    let fact_uid = graph.create_node(intent).await.expect("create vector fact");
    set_storage_partition_vector_backend(
        session_store.pool(),
        &storage_partition_id,
        "turbopuffer",
    )
    .await;

    let server = MockServer::start().await;
    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let turbopuffer = TurbopufferStore::new(
        server.uri(),
        SecretString::from("unused-key"),
        "temporal-as-of",
        false,
    )
    .expect("build Turbopuffer store");
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_turbopuffer(Some(Arc::new(turbopuffer)))
    .with_assume_app_role(true);

    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: String::new(),
            query_embedding: deterministic_vector(fact),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Fact]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: Some(utc("2026-03-01T00:00:00Z")),
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve through pgvector fallback");

    assert_eq!(hits.first().map(|hit| hit.uid), Some(fact_uid));
    assert!(hits[0].legs.vector, "{hits:?}");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(
        requests.len(),
        0,
        "as_of vector reads must not call Turbopuffer"
    );

    let _ = graph
        .hard_purge(fact_uid, "redacted:hybrid-temporal-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn temporal_dual_read_as_of_uses_pgvector_without_calling_turbopuffer() {
    // Pins: promotion dual-read is current-read only; historical reads stay on
    // the pgvector source and do not fan out to external projections.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;
    let fact = "temporal dual read pgvector source fact";
    let mut intent = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        fact,
        Some(deterministic_vector(fact)),
    );
    intent.valid_from = utc("2026-02-01T00:00:00Z");
    let fact_uid = graph.create_node(intent).await.expect("create vector fact");
    set_storage_partition_vector_backend_state(
        session_store.pool(),
        &storage_partition_id,
        "turbopuffer",
        "dual_read",
        Some(moa_test_support::fixtures::pg_now() + chrono::Duration::hours(1)),
    )
    .await;

    let server = MockServer::start().await;
    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let turbopuffer = TurbopufferStore::new(
        server.uri(),
        SecretString::from("unused-key"),
        "temporal-dual-read",
        false,
    )
    .expect("build Turbopuffer store");
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_turbopuffer(Some(Arc::new(turbopuffer)))
    .with_assume_app_role(true);

    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: String::new(),
            query_embedding: deterministic_vector(fact),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Fact]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: Some(utc("2026-03-01T00:00:00Z")),
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve through pgvector source");

    assert_eq!(hits.first().map(|hit| hit.uid), Some(fact_uid));
    assert!(hits[0].legs.vector, "{hits:?}");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(
        requests.len(),
        0,
        "dual-read as_of vector reads must not call Turbopuffer"
    );

    let _ = graph
        .hard_purge(fact_uid, "redacted:hybrid-temporal-dual-read-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn turbopuffer_backend_uses_bm25_for_lexical_candidates_db_memory() {
    // Pins: active Turbopuffer storage partitions use the Turbopuffer BM25 leg
    // for lexical candidates when the request has no historical as_of filter.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;
    let fact = "turbopuffer bm25 exact identifier abc-123";
    let fact_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Chunk,
            fact,
            None,
        ))
        .await
        .expect("create lexical fact");
    set_storage_partition_vector_backend(
        session_store.pool(),
        &storage_partition_id,
        "turbopuffer",
    )
    .await;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("BM25"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rows": [{ "id": fact_uid.to_string(), "$score": 5.0 }]
        })))
        .mount(&server)
        .await;

    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let turbopuffer = TurbopufferStore::new(
        server.uri(),
        SecretString::from("unused-key"),
        "bm25-routing",
        false,
    )
    .expect("build Turbopuffer store");
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_turbopuffer(Some(Arc::new(turbopuffer)))
    .with_assume_app_role(true);

    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "abc-123".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve through Turbopuffer BM25");

    assert_eq!(hits.first().map(|hit| hit.uid), Some(fact_uid));
    assert!(hits[0].legs.lexical, "{hits:?}");
    assert!(!hits[0].legs.vector, "{hits:?}");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("BM25 body is JSON");
    assert_eq!(body["rank_by"], json!(["content", "BM25", "abc-123"]));

    let _ = graph
        .hard_purge(fact_uid, "redacted:hybrid-bm25-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn turbopuffer_backend_keeps_postgres_lexical_for_fact_candidates_db_memory() {
    // Pins: BM25 is dark for non-chunk rows, so Turbopuffer-backed tenants keep
    // using Postgres lexical for fact/entity style graph memory.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;
    let fact = "turbopuffer fact exact identifier fact-456";
    let fact_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Fact,
            fact,
            None,
        ))
        .await
        .expect("create fact node");
    set_storage_partition_vector_backend(
        session_store.pool(),
        &storage_partition_id,
        "turbopuffer",
    )
    .await;

    let server = MockServer::start().await;
    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let turbopuffer = TurbopufferStore::new(
        server.uri(),
        SecretString::from("unused-key"),
        "bm25-fact-postgres",
        false,
    )
    .expect("build Turbopuffer store");
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_turbopuffer(Some(Arc::new(turbopuffer)))
    .with_assume_app_role(true);

    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "fact-456".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Fact]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve fact through Postgres lexical");

    assert_eq!(hits.first().map(|hit| hit.uid), Some(fact_uid));
    assert!(hits[0].legs.lexical, "{hits:?}");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 0, "fact lexical must not call BM25");

    let _ = graph
        .hard_purge(fact_uid, "redacted:hybrid-bm25-fact-postgres-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn turbopuffer_bm25_error_falls_back_to_postgres_lexical_db_memory() {
    // Pins: BM25 provider errors fail open to the existing Postgres lexical leg
    // without losing lexical attribution.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;
    let fact = "fallback postgres lexical token xyz-987";
    let fact_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Chunk,
            fact,
            None,
        ))
        .await
        .expect("create lexical fact");
    set_storage_partition_vector_backend(
        session_store.pool(),
        &storage_partition_id,
        "turbopuffer",
    )
    .await;

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_string_contains("BM25"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "status": "error",
            "error": "bm25 unavailable"
        })))
        .mount(&server)
        .await;

    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let turbopuffer = TurbopufferStore::new(
        server.uri(),
        SecretString::from("unused-key"),
        "bm25-fallback",
        false,
    )
    .expect("build Turbopuffer store");
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_turbopuffer(Some(Arc::new(turbopuffer)))
    .with_assume_app_role(true);

    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "xyz-987".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve through Postgres lexical fallback");

    assert_eq!(hits.first().map(|hit| hit.uid), Some(fact_uid));
    assert!(hits[0].legs.lexical, "{hits:?}");
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 1);

    let _ = graph
        .hard_purge(fact_uid, "redacted:hybrid-bm25-fallback-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn lexical_prefix_fallback_matches_word_prefix_when_primary_misses_db_memory() {
    // Pins: the sargable lexical fallback (name_tsv prefix FTS) still recovers a
    // node whose stored word extends a short query term — e.g. query "auth"
    // matches the stored lexeme "authentication" — which the exact/stemmed
    // primary leg misses. This is the recall the previous non-sargable
    // LIKE '%term%' scan provided for word-prefix cases.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    let node_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Chunk,
            "authentication service",
            None,
        ))
        .await
        .expect("create prefix-fallback fact");

    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_assume_app_role(true);

    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "auth".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Chunk]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve through sargable prefix fallback");

    let hit = hits
        .iter()
        .find(|hit| hit.uid == node_uid)
        .expect("word-prefix node should be recovered by the prefix fallback");
    assert!(
        hit.legs.lexical,
        "hit must be attributed to the lexical leg"
    );

    let _ = graph
        .hard_purge(node_uid, "redacted:prefix-fallback-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn lexical_fallback_matches_structured_predicate_in_properties_db_memory() {
    // Pins: the sargable fallback still recovers structured-predicate facts whose
    // match lives in `properties_summary` (not the name) via the new
    // `properties_tsv` GIN column — the recall the LIKE scan over
    // name || properties_summary previously provided.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = test_storage_partition_id();
    let graph = graph_store(session_store.pool(), &storage_partition_id);
    set_workspace_embedder_state(session_store.pool(), &storage_partition_id, "test-model").await;

    // Name deliberately carries none of the query terms; the match must come from
    // the structured predicate stored in properties_summary.
    let node_uid = graph
        .create_node(NodeWriteIntent {
            barrier: None,
            uid: Uuid::now_v7(),
            data_subject_id: Uuid::parse_str(&storage_partition_id)
                .expect("storage partition fixture should be a tenant UUID"),
            label: NodeLabel::Fact,
            storage_partition_id: Some(storage_partition_id.clone()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: "unrelated node title".to_string(),
            properties: json!({
                "summary": "private repository preference",
                "predicate": "private_repository",
                "source": "structured_predicate_test"
            }),
            pii_class: SensitivityClass::None,
            confidence: Some(0.9),
            valid_from: moa_test_support::fixtures::pg_now(),
            embedding: None,
            embedding_model: Some("test-model".to_string()),
            embedding_model_version: Some(1),
            embedding_text: None,
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("create structured-predicate fact");

    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_assume_app_role(true);

    let hits = retriever
        .retrieve(RetrievalRequest {
            cleared_barriers: Default::default(),
            seeds: Vec::new(),
            query_text: "Which private work repository should you use for me?".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_memory_scope(&storage_partition_id),
            label_filter: Some(vec![NodeLabel::Fact]),
            label_boost: None,
            max_pii_class: SensitivityClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: true,
            window_policy: moa_retrieval::retrieval::EvidenceWindowPolicy::default(),
        })
        .await
        .expect("retrieve through properties_tsv fallback");

    let hit = hits
        .iter()
        .find(|hit| hit.uid == node_uid)
        .expect("structured-predicate fact should be recovered via properties_tsv");
    assert!(
        hit.legs.lexical,
        "hit must be attributed to the lexical leg"
    );

    let _ = graph
        .hard_purge(node_uid, "redacted:properties-fallback-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
