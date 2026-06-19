//! Integration coverage for weighted lexical seed ranking.

use chrono::{DateTime, Duration, Utc};
use moa_core::{ScopeContext, WorkspaceId};
use moa_memory_graph::{
    AgeGraphStore, GraphStore, LexicalStore, NodeLabel, NodeWriteIntent, PiiClass,
};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

fn scope(workspace_id: &str) -> ScopeContext {
    ScopeContext::workspace(WorkspaceId::new(workspace_id))
}

fn graph_store(test_db: &TestDb, workspace_id: &str) -> AgeGraphStore {
    AgeGraphStore::scoped_for_app_role(test_db.store().pool().clone(), scope(workspace_id))
}

fn lexical_store(test_db: &TestDb, workspace_id: &str) -> LexicalStore {
    LexicalStore::scoped_for_app_role(test_db.store().pool().clone(), scope(workspace_id))
}

fn fact(
    workspace_id: &str,
    uid: Uuid,
    valid_from: DateTime<Utc>,
    confidence: f64,
    reference_count: i64,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid,
        label: NodeLabel::Fact,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        name: "ranking alpha memory".to_string(),
        properties: json!({
            "summary": format!("ranking alpha memory {uid}"),
            "reference_count": reference_count,
        }),
        pii_class: PiiClass::None,
        confidence: Some(confidence),
        valid_from,
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn insert_fact(
    graph: &AgeGraphStore,
    workspace_id: &str,
    valid_from: DateTime<Utc>,
    confidence: f64,
    reference_count: i64,
) -> Uuid {
    let uid = Uuid::now_v7();
    graph
        .create_node(fact(
            workspace_id,
            uid,
            valid_from,
            confidence,
            reference_count,
        ))
        .await
        .expect("insert lexical ranking fact");
    uid
}

async fn lookup_uids(test_db: &TestDb, workspace_id: &str, top_k: i64) -> Vec<Uuid> {
    lexical_store(test_db, workspace_id)
        .lookup_seeds("ranking alpha", top_k)
        .await
        .expect("lookup lexical ranking seeds")
        .into_iter()
        .map(|row| row.uid)
        .collect()
}

/// Pins `0.55 * recency_decay + 0.35 * confidence + 0.10 * normalized_reference_count`.
#[tokio::test]
async fn lexical_search_orders_results_by_combined_recency_confidence_reference_score() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("lexical-ranking-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &workspace_id);
    let now = Utc::now();

    let medium = insert_fact(&graph, &workspace_id, now - Duration::days(1), 0.7, 2).await;
    let recent_low = insert_fact(&graph, &workspace_id, now, 0.1, 0).await;
    let old_high = insert_fact(&graph, &workspace_id, now - Duration::days(4), 1.0, 100).await;
    let balanced = insert_fact(&graph, &workspace_id, now - Duration::hours(12), 0.8, 10).await;
    let low = insert_fact(&graph, &workspace_id, now - Duration::days(2), 0.4, 50).await;

    assert_eq!(
        lookup_uids(&test_db, &workspace_id, 10).await,
        vec![balanced, recent_low, old_high, medium, low]
    );
}

/// Pins the recency term in `0.55 * recency_decay + 0.35 * confidence + 0.10 * normalized_reference_count`.
#[tokio::test]
async fn lexical_search_recency_weight_dominates_when_confidence_and_refs_equal() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("lexical-recency-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &workspace_id);
    let now = Utc::now();

    let old = insert_fact(&graph, &workspace_id, now - Duration::days(5), 0.7, 3).await;
    let recent = insert_fact(&graph, &workspace_id, now - Duration::hours(1), 0.7, 3).await;
    let middle = insert_fact(&graph, &workspace_id, now - Duration::days(2), 0.7, 3).await;

    assert_eq!(
        lookup_uids(&test_db, &workspace_id, 10).await,
        vec![recent, middle, old]
    );
}

/// Pins the confidence term in `0.55 * recency_decay + 0.35 * confidence + 0.10 * normalized_reference_count`.
#[tokio::test]
async fn lexical_search_confidence_weight_dominates_when_recency_and_refs_equal() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("lexical-confidence-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &workspace_id);
    let valid_from = Utc::now() - Duration::days(1);

    let low = insert_fact(&graph, &workspace_id, valid_from, 0.2, 8).await;
    let high = insert_fact(&graph, &workspace_id, valid_from, 0.9, 8).await;
    let middle = insert_fact(&graph, &workspace_id, valid_from, 0.5, 8).await;

    assert_eq!(
        lookup_uids(&test_db, &workspace_id, 10).await,
        vec![high, middle, low]
    );
}

/// Pins the reference-count tie breaker in `0.55 * recency_decay + 0.35 * confidence + 0.10 * normalized_reference_count`.
#[tokio::test]
async fn lexical_search_reference_count_breaks_ties_when_recency_and_confidence_equal() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("lexical-refs-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &workspace_id);
    let valid_from = Utc::now() - Duration::days(1);

    let few = insert_fact(&graph, &workspace_id, valid_from, 0.7, 1).await;
    let many = insert_fact(&graph, &workspace_id, valid_from, 0.7, 30).await;
    let some = insert_fact(&graph, &workspace_id, valid_from, 0.7, 10).await;

    assert_eq!(
        lookup_uids(&test_db, &workspace_id, 10).await,
        vec![many, some, few]
    );
}

/// Pins top-K truncation after applying `0.55 * recency_decay + 0.35 * confidence + 0.10 * normalized_reference_count`.
#[tokio::test]
async fn lexical_search_returns_top_k_when_more_than_k_match() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("lexical-topk-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &workspace_id);
    let now = Utc::now();
    let mut expected = Vec::new();

    for offset in (0..20).rev() {
        let uid = insert_fact(
            &graph,
            &workspace_id,
            now - Duration::minutes(offset),
            0.5,
            0,
        )
        .await;
        if offset < 5 {
            expected.push(uid);
        }
    }
    expected.reverse();

    let actual = lookup_uids(&test_db, &workspace_id, 5).await;
    assert_eq!(actual.len(), 5);
    assert_eq!(actual, expected);
}

/// Pins the `valid_to IS NULL` filter before weighted lexical ranking.
#[tokio::test]
async fn lexical_search_excludes_invalidated_nodes_with_valid_to_set() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_id = format!("lexical-validto-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &workspace_id);
    let now = Utc::now();

    let keep_a = insert_fact(&graph, &workspace_id, now - Duration::minutes(1), 0.7, 0).await;
    let drop_a = insert_fact(&graph, &workspace_id, now - Duration::minutes(2), 0.7, 0).await;
    let keep_b = insert_fact(&graph, &workspace_id, now - Duration::minutes(3), 0.7, 0).await;
    let drop_b = insert_fact(&graph, &workspace_id, now - Duration::minutes(4), 0.7, 0).await;
    let keep_c = insert_fact(&graph, &workspace_id, now - Duration::minutes(5), 0.7, 0).await;

    graph
        .invalidate_node(drop_a, "lexical invalidated")
        .await
        .expect("invalidate first lexical row");
    graph
        .invalidate_node(drop_b, "lexical invalidated")
        .await
        .expect("invalidate second lexical row");

    assert_eq!(
        lookup_uids(&test_db, &workspace_id, 10).await,
        vec![keep_a, keep_b, keep_c]
    );
}
