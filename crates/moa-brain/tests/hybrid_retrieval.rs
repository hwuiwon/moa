//! Integration coverage for graph-memory hybrid retrieval.

use std::sync::Arc;

use chrono::Utc;
use moa_core::{MemoryScope, ScopeContext, ScopedConn, WorkspaceId};
use moa_memory_graph::{
    AgeGraphStore, EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
};
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use moa_session::testing;
use serde_json::json;
use sqlx::{PgPool, Postgres, QueryBuilder};
use tokio::sync::Mutex;
use uuid::Uuid;

use moa_brain::retrieval::{HybridRetriever, RetrievalRequest, legs::lexical_leg};

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn deterministic_vector(text: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    for (index, byte) in text.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

fn graph_store(pool: &PgPool, workspace_id: &str) -> AgeGraphStore {
    let scope = ScopeContext::workspace(WorkspaceId::new(workspace_id));
    let vector = PgvectorStore::new_for_app_role(pool.clone(), scope.clone());
    AgeGraphStore::scoped_for_app_role(pool.clone(), scope).with_vector_store(Arc::new(vector))
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

    delete_filler_rows(session_store.pool(), &workspace_id, &prefix).await;
    let _ = graph.hard_purge(exact_uid, "redacted:hybrid-test").await;
    let _ = graph.hard_purge(related_uid, "redacted:hybrid-test").await;
    let _ = graph.hard_purge(seed_uid, "redacted:hybrid-test").await;
    drop(session_store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
