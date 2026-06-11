//! Integration coverage for graph-memory hybrid retrieval.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_core::{MemoryScope, ScopeContext, ScopedConn, SessionId, UserId, WorkspaceId};
use moa_memory_graph::{
    AgeGraphStore, EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
};
use moa_memory_ingest::{
    ExtractedFact, ExtractedFactScopeHint, IngestCtx, RrfPlusJudgeDetector, ScriptedFactExtractor,
    SessionTurn, fact_hash, fact_uid_from_hash, ingest_turn_direct_with_ctx,
};
use moa_memory_pii::{PiiClassifier, PiiError, PiiResult, PiiSpan};
use moa_memory_vector::{PgvectorStore, TurbopufferStore, VECTOR_DIMENSION};
use moa_session::testing;
use secrecy::SecretString;
use serde_json::json;
use sqlx::{PgPool, Postgres, QueryBuilder};
use tokio::sync::Mutex;
use uuid::Uuid;

use moa_brain::planning::{PlanningCtx, QueryPlanner, QueryRetrievalCtx};
use moa_brain::retrieval::{
    CachedHybridRetriever, HybridRetriever, RetrievalRequest, legs::lexical_leg,
};

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

#[tokio::test]
async fn query_retrieval_ctx_defaults_reranker_off_and_requires_explicit_opt_in() {
    // Pins: query retrieval callers do not enable reranking unless they explicitly opt in.
    let pool = PgPool::connect_lazy("postgres://unused")
        .expect("lazy pool construction should not connect");
    let workspace_id = WorkspaceId::new("reranker-default-workspace");
    let scope = MemoryScope::Workspace {
        workspace_id: workspace_id.clone(),
    };
    let scope_context = ScopeContext::workspace(workspace_id);
    let vector: Arc<dyn moa_memory_vector::VectorStore> = Arc::new(
        PgvectorStore::new_for_app_role(pool.clone(), scope_context.clone()),
    );
    let graph: Arc<dyn GraphStore> = Arc::new(
        AgeGraphStore::scoped_for_app_role(pool.clone(), scope_context)
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
        PiiClass::Restricted,
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

    async fn embed(&self, texts: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
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
    async fn classify(&self, _text: &str) -> Result<PiiResult, PiiError> {
        Ok(PiiResult {
            class: PiiClass::None,
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

fn graph_store(pool: &PgPool, workspace_id: &str) -> AgeGraphStore {
    let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    AgeGraphStore::scoped_for_app_role(pool.clone(), scope).with_vector_store(Arc::new(vector))
}

fn user_graph_store(pool: &PgPool, workspace_id: &str, user_id: &str) -> AgeGraphStore {
    let scope = ScopeContext::user(WorkspaceId::new(workspace_id), UserId::new(user_id));
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    AgeGraphStore::scoped_for_app_role(pool.clone(), scope).with_vector_store(Arc::new(vector))
}

fn scripted_user_fact(summary: &str) -> ExtractedFact {
    let mut fact = ExtractedFact {
        uid: Uuid::nil(),
        subject: "user".to_string(),
        predicate: "prefers".to_string(),
        object: summary.to_string(),
        summary: summary.to_string(),
        source_chunk: 0,
        scope_hint: ExtractedFactScopeHint::User,
        confidence: Some(0.92),
    };
    let hash = fact_hash(&fact).expect("scripted fact hashes");
    fact.uid = fact_uid_from_hash(&hash);
    fact
}

fn hit_summary(hit: &moa_brain::retrieval::RetrievalHit) -> Option<&str> {
    hit.node
        .properties_summary
        .as_ref()
        .and_then(|properties| properties.get("summary"))
        .and_then(serde_json::Value::as_str)
}

fn node_intent(
    workspace_id: &str,
    label: NodeLabel,
    name: &str,
    embedding: Option<Vec<f32>>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        name: name.to_string(),
        properties: json!({ "summary": name, "source": "hybrid_retrieval_test" }),
        pii_class: PiiClass::None,
        confidence: Some(0.9),
        valid_from: Utc::now(),
        embedding,
        embedding_model: Some("test-model".to_string()),
        embedding_model_version: Some(1),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn seed_filler_rows(pool: &PgPool, workspace_id: &str, prefix: &str, count: usize) {
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin filler seed transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");

    let mut builder = QueryBuilder::<Postgres>::new(
        "INSERT INTO moa.node_index (uid, label, workspace_id, name, pii_class, confidence) ",
    );
    builder.push_values(0..count, |mut row, index| {
        row.push_bind(Uuid::now_v7())
            .push_bind(NodeLabel::Fact.as_str())
            .push_bind(workspace_id)
            .push_bind(format!("{prefix} filler operational note {index}"))
            .push_bind(PiiClass::None.as_str())
            .push_bind(0.5_f64);
    });
    builder
        .build()
        .execute(conn.as_mut())
        .await
        .expect("insert filler rows");
    conn.commit().await.expect("commit filler seed transaction");
}

async fn delete_filler_rows(pool: &PgPool, workspace_id: &str, prefix: &str) {
    sqlx::query("DELETE FROM moa.node_index WHERE workspace_id = $1 AND name LIKE $2")
        .bind(workspace_id)
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .expect("delete filler rows");
}

async fn set_workspace_vector_backend(pool: &PgPool, workspace_id: &str, backend: &str) {
    let ctx = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin workspace_state transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    sqlx::query(
        r#"
        INSERT INTO moa.workspace_state (workspace_id, vector_backend, vector_backend_state)
        VALUES ($1, $2, 'steady')
        ON CONFLICT (workspace_id) DO UPDATE
            SET vector_backend = EXCLUDED.vector_backend,
                vector_backend_state = EXCLUDED.vector_backend_state
        "#,
    )
    .bind(workspace_id)
    .bind(backend)
    .execute(conn.as_mut())
    .await
    .expect("set workspace vector backend");
    conn.commit()
        .await
        .expect("commit workspace_state transaction");
}

#[tokio::test]
async fn hybrid_retrieval_e2e_returns_fused_annotated_results() {
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = format!("hybrid-retrieval-{}", Uuid::now_v7().simple());
    let prefix = format!("hybrid-e2e-{}", Uuid::now_v7().simple());
    let graph = graph_store(session_store.pool(), &workspace_id);

    seed_filler_rows(session_store.pool(), &workspace_id, &prefix, 1_000).await;

    let seed = node_intent(
        &workspace_id,
        NodeLabel::Entity,
        "auth service deployment entity",
        None,
    );
    let seed_uid = graph.create_node(seed).await.expect("create seed node");
    let exact_text = "auth service deployment provider is fly.io";
    let exact = node_intent(
        &workspace_id,
        NodeLabel::Fact,
        exact_text,
        Some(deterministic_vector(exact_text)),
    );
    let exact_uid = graph.create_node(exact).await.expect("create exact fact");
    let related_text = "auth service uses JWT access tokens";
    let related = node_intent(
        &workspace_id,
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
                properties: json!({ "source": "hybrid_retrieval_test" }),
                workspace_id: Some(workspace_id.clone()),
                user_id: None,
                scope: "workspace".to_string(),
                actor_id: Uuid::now_v7().to_string(),
                actor_kind: "system".to_string(),
            })
            .await
            .expect("create graph edge");
    }

    let scope = MemoryScope::Workspace {
        workspace_id: WorkspaceId::new(workspace_id.clone()),
    };
    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_id.clone())),
    );
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_assume_app_role(true);
    let request = RetrievalRequest {
        seeds: vec![seed_uid],
        query_text: exact_text.to_string(),
        query_embedding: deterministic_vector(exact_text),
        scope,
        label_filter: Some(vec![NodeLabel::Fact]),
        max_pii_class: PiiClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
    };
    let lexical_hits = lexical_leg(session_store.pool(), &request, true)
        .await
        .expect("lexical leg should retrieve exact fact");
    assert!(
        lexical_hits.iter().any(|hit| hit.uid == exact_uid),
        "{lexical_hits:?}"
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
    assert_eq!(exact_hit.node.scope, "workspace");

    let graph_only_hits = retriever
        .retrieve(RetrievalRequest {
            seeds: vec![seed_uid],
            query_text: String::new(),
            query_embedding: Vec::new(),
            scope: MemoryScope::Workspace {
                workspace_id: WorkspaceId::new(workspace_id.clone()),
            },
            label_filter: Some(vec![NodeLabel::Fact]),
            max_pii_class: PiiClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
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
            seeds: vec![seed_uid],
            query_text: String::new(),
            query_embedding: Vec::new(),
            scope: MemoryScope::Workspace {
                workspace_id: WorkspaceId::new(workspace_id.clone()),
            },
            label_filter: Some(vec![NodeLabel::Fact]),
            max_pii_class: PiiClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: true,
        })
        .await
        .expect("retrieve with graph expansion disabled");
    assert!(
        graph_disabled_hits.iter().all(|hit| hit.uid != related_uid),
        "{graph_disabled_hits:?}"
    );

    delete_filler_rows(session_store.pool(), &workspace_id, &prefix).await;
    let _ = graph.hard_purge(exact_uid, "redacted:hybrid-test").await;
    let _ = graph.hard_purge(related_uid, "redacted:hybrid-test").await;
    let _ = graph.hard_purge(seed_uid, "redacted:hybrid-test").await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
#[ignore = "requires Postgres test database"]
async fn user_scope_fact_invisible_to_other_user_at_any_k() {
    // Pins: user-scoped facts written by ingestion are structurally hidden from other users.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let pool = session_store.pool().clone();
    let workspace_id = format!("scope-isolation-{}", Uuid::now_v7().simple());
    let user_a = "user-scope-owner";
    let user_b = "user-scope-other";
    let workspace = WorkspaceId::new(workspace_id.clone());
    let workspace_scope = ScopeContext::workspace(workspace.clone());
    let ingest_vector = Arc::new(PgvectorStore::new_for_app_role(
        pool.clone(),
        workspace_scope.clone(),
    ));
    let ingest_graph = Arc::new(
        AgeGraphStore::scoped_for_app_role(pool.clone(), workspace_scope)
            .with_vector_store(ingest_vector.clone()),
    );
    let summary = "The user prefers the private green deployment dashboard";
    let fact = scripted_user_fact(summary);
    let ctx = IngestCtx::new(
        pool.clone(),
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
            workspace_id: workspace.clone(),
            user_id: UserId::new(user_a),
            session_id: SessionId::new(),
            turn_seq: 1,
            transcript: format!("user: {summary}"),
            dominant_pii_class: "none".to_string(),
            finalized_at: utc("2026-05-07T12:00:00Z"),
        },
    )
    .await
    .expect("ingest user-scoped fact");
    assert_eq!(report.inserted, 1);

    let owner_graph = user_graph_store(&pool, &workspace_id, user_a);
    let owner_scope = ScopeContext::user(workspace.clone(), UserId::new(user_a));
    let owner_vector = PgvectorStore::new_for_app_role(pool.clone(), owner_scope);
    let owner_hits =
        HybridRetriever::new(pool.clone(), Arc::new(owner_graph), Arc::new(owner_vector))
            .with_assume_app_role(true)
            .retrieve(RetrievalRequest {
                seeds: Vec::new(),
                query_text: summary.to_string(),
                query_embedding: deterministic_vector(summary),
                scope: MemoryScope::User {
                    workspace_id: workspace.clone(),
                    user_id: UserId::new(user_a),
                },
                label_filter: Some(vec![NodeLabel::Fact]),
                max_pii_class: PiiClass::Restricted,
                k_final: 25,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                disable_leg_timeouts: true,
                disable_graph_expansion: false,
            })
            .await
            .expect("owner retrieval succeeds");
    assert!(
        owner_hits
            .iter()
            .any(|hit| hit_summary(hit) == Some(summary)),
        "{owner_hits:?}"
    );

    let other_graph = user_graph_store(&pool, &workspace_id, user_b);
    let other_scope = ScopeContext::user(workspace.clone(), UserId::new(user_b));
    let other_vector = PgvectorStore::new_for_app_role(pool.clone(), other_scope);
    let other_hits =
        HybridRetriever::new(pool.clone(), Arc::new(other_graph), Arc::new(other_vector))
            .with_assume_app_role(true)
            .retrieve(RetrievalRequest {
                seeds: Vec::new(),
                query_text: summary.to_string(),
                query_embedding: deterministic_vector(summary),
                scope: MemoryScope::User {
                    workspace_id: workspace,
                    user_id: UserId::new(user_b),
                },
                label_filter: Some(vec![NodeLabel::Fact]),
                max_pii_class: PiiClass::Restricted,
                k_final: 25,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                disable_leg_timeouts: true,
                disable_graph_expansion: false,
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
    let workspace_id = format!("hybrid-temporal-{}", Uuid::now_v7().simple());
    let graph = graph_store(session_store.pool(), &workspace_id);

    let old_name = "temporal-asof legacy gateway owner";
    let mut old = node_intent(&workspace_id, NodeLabel::Fact, old_name, None);
    old.valid_from = utc("2026-02-01T00:00:00Z");
    let old_uid = graph.create_node(old).await.expect("create old fact");

    let mut replacement = node_intent(
        &workspace_id,
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
        ScopeContext::workspace(WorkspaceId::new(workspace_id.clone())),
    );
    let retriever = HybridRetriever::new(
        session_store.pool().clone(),
        Arc::new(graph.clone()),
        Arc::new(vector),
    )
    .with_assume_app_role(true);
    let scope = MemoryScope::Workspace {
        workspace_id: WorkspaceId::new(workspace_id.clone()),
    };
    let historical = RetrievalRequest {
        seeds: Vec::new(),
        query_text: old_name.to_string(),
        query_embedding: Vec::new(),
        scope: scope.clone(),
        label_filter: Some(vec![NodeLabel::Fact]),
        max_pii_class: PiiClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: None,
        as_of: Some(utc("2026-03-01T00:00:00Z")),
        ranking_reference_time: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
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
        seeds: Vec::new(),
        query_text: old_name.to_string(),
        query_embedding: Vec::new(),
        scope,
        label_filter: Some(vec![NodeLabel::Fact]),
        max_pii_class: PiiClass::Restricted,
        k_final: 5,
        use_reranker: false,
        strategy: None,
        as_of: None,
        ranking_reference_time: None,
        disable_leg_timeouts: false,
        disable_graph_expansion: false,
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
async fn temporal_turbopuffer_unsupported_as_of_falls_back_to_pgvector() {
    // Pins: temporal hybrid vector retrieval falls back to pgvector for Turbopuffer workspaces.
    let _guard = TEST_LOCK.lock().await;
    let (session_store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let workspace_id = format!("hybrid-tp-asof-{}", Uuid::now_v7().simple());
    let graph = graph_store(session_store.pool(), &workspace_id);
    let fact = "temporal turbopuffer fallback pgvector fact";
    let mut intent = node_intent(
        &workspace_id,
        NodeLabel::Fact,
        fact,
        Some(deterministic_vector(fact)),
    );
    intent.valid_from = utc("2026-02-01T00:00:00Z");
    let fact_uid = graph.create_node(intent).await.expect("create vector fact");
    set_workspace_vector_backend(session_store.pool(), &workspace_id, "turbopuffer").await;

    let vector = PgvectorStore::new_for_app_role(
        session_store.pool().clone(),
        ScopeContext::workspace(WorkspaceId::new(workspace_id.clone())),
    );
    let turbopuffer = TurbopufferStore::new(
        "http://127.0.0.1:9".to_string(),
        SecretString::from("unused-key"),
        "temporal-fallback",
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
            seeds: Vec::new(),
            query_text: String::new(),
            query_embedding: deterministic_vector(fact),
            scope: MemoryScope::Workspace {
                workspace_id: WorkspaceId::new(workspace_id.clone()),
            },
            label_filter: Some(vec![NodeLabel::Fact]),
            max_pii_class: PiiClass::Restricted,
            k_final: 5,
            use_reranker: false,
            strategy: None,
            as_of: Some(utc("2026-03-01T00:00:00Z")),
            ranking_reference_time: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
        })
        .await
        .expect("retrieve through pgvector fallback");

    assert_eq!(hits.first().map(|hit| hit.uid), Some(fact_uid));
    assert!(hits[0].legs.vector, "{hits:?}");

    let _ = graph
        .hard_purge(fact_uid, "redacted:hybrid-temporal-test")
        .await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
