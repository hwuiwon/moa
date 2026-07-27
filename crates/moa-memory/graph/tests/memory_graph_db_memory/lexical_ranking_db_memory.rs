//! Integration coverage for weighted lexical seed ranking.

use chrono::{DateTime, Duration, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_memory_graph::{GraphStore, LexicalStore, NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_test_support::fixtures::stable_uuid_from_label;
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn tenant_scope(storage_partition_id: impl AsRef<str>) -> RlsContext {
    let storage_partition_id = storage_partition_id.as_ref();
    let tenant_id = Uuid::parse_str(storage_partition_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id)));
    RlsContext::tenant(tenant_id)
}

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

fn scope(storage_partition_id: &str) -> RlsContext {
    tenant_scope(storage_partition_id)
}

fn graph_store(test_db: &TestDb, storage_partition_id: &str) -> PostgresGraphStore {
    PostgresGraphStore::scoped_for_app_role(
        test_db.store().pool().clone(),
        scope(storage_partition_id),
        super::test_kms(),
    )
}

fn lexical_store(test_db: &TestDb, storage_partition_id: &str) -> LexicalStore {
    LexicalStore::scoped_for_app_role(test_db.store().pool().clone(), scope(storage_partition_id))
}

fn fact(
    storage_partition_id: &str,
    uid: Uuid,
    valid_from: DateTime<Utc>,
    confidence: f64,
    reference_count: i64,
) -> NodeWriteIntent {
    NodeWriteIntent {
        barrier: None,
        uid,
        data_subject_id: scope(storage_partition_id).tenant_id().0,
        label: NodeLabel::Fact,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: "ranking alpha memory".to_string(),
        properties: json!({
            "summary": format!("ranking alpha memory {uid}"),
            "reference_count": reference_count,
        }),
        pii_class: SensitivityClass::None,
        confidence: Some(confidence),
        valid_from,
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn insert_fact(
    graph: &PostgresGraphStore,
    storage_partition_id: &str,
    valid_from: DateTime<Utc>,
    confidence: f64,
    reference_count: i64,
) -> Uuid {
    let uid = Uuid::now_v7();
    graph
        .create_node(fact(
            storage_partition_id,
            uid,
            valid_from,
            confidence,
            reference_count,
        ))
        .await
        .expect("insert lexical ranking fact");
    uid
}

async fn lookup_uids(test_db: &TestDb, storage_partition_id: &str, top_k: i64) -> Vec<Uuid> {
    lexical_store(test_db, storage_partition_id)
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
    let storage_partition_id = format!("lexical-ranking-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let now = moa_test_support::fixtures::pg_now();

    let medium = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::days(1),
        0.7,
        2,
    )
    .await;
    let recent_low = insert_fact(&graph, &storage_partition_id, now, 0.1, 0).await;
    let old_high = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::days(4),
        1.0,
        100,
    )
    .await;
    let balanced = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::hours(12),
        0.8,
        10,
    )
    .await;
    let low = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::days(2),
        0.4,
        50,
    )
    .await;

    assert_eq!(
        lookup_uids(&test_db, &storage_partition_id, 10).await,
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
    let storage_partition_id = format!("lexical-recency-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let now = moa_test_support::fixtures::pg_now();

    let old = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::days(5),
        0.7,
        3,
    )
    .await;
    let recent = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::hours(1),
        0.7,
        3,
    )
    .await;
    let middle = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::days(2),
        0.7,
        3,
    )
    .await;

    assert_eq!(
        lookup_uids(&test_db, &storage_partition_id, 10).await,
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
    let storage_partition_id = format!("lexical-confidence-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let valid_from = moa_test_support::fixtures::pg_now() - Duration::days(1);

    let low = insert_fact(&graph, &storage_partition_id, valid_from, 0.2, 8).await;
    let high = insert_fact(&graph, &storage_partition_id, valid_from, 0.9, 8).await;
    let middle = insert_fact(&graph, &storage_partition_id, valid_from, 0.5, 8).await;

    assert_eq!(
        lookup_uids(&test_db, &storage_partition_id, 10).await,
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
    let storage_partition_id = format!("lexical-refs-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let valid_from = moa_test_support::fixtures::pg_now() - Duration::days(1);

    let few = insert_fact(&graph, &storage_partition_id, valid_from, 0.7, 1).await;
    let many = insert_fact(&graph, &storage_partition_id, valid_from, 0.7, 30).await;
    let some = insert_fact(&graph, &storage_partition_id, valid_from, 0.7, 10).await;

    assert_eq!(
        lookup_uids(&test_db, &storage_partition_id, 10).await,
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
    let storage_partition_id = format!("lexical-topk-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let now = moa_test_support::fixtures::pg_now();
    let mut expected = Vec::new();

    for offset in (0..20).rev() {
        let uid = insert_fact(
            &graph,
            &storage_partition_id,
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

    let actual = lookup_uids(&test_db, &storage_partition_id, 5).await;
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
    let storage_partition_id = format!("lexical-validto-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let now = moa_test_support::fixtures::pg_now();

    let keep_a = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::minutes(1),
        0.7,
        0,
    )
    .await;
    let drop_a = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::minutes(2),
        0.7,
        0,
    )
    .await;
    let keep_b = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::minutes(3),
        0.7,
        0,
    )
    .await;
    let drop_b = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::minutes(4),
        0.7,
        0,
    )
    .await;
    let keep_c = insert_fact(
        &graph,
        &storage_partition_id,
        now - Duration::minutes(5),
        0.7,
        0,
    )
    .await;

    graph
        .invalidate_node(drop_a, "lexical invalidated")
        .await
        .expect("invalidate first lexical row");
    graph
        .invalidate_node(drop_b, "lexical invalidated")
        .await
        .expect("invalidate second lexical row");

    assert_eq!(
        lookup_uids(&test_db, &storage_partition_id, 10).await,
        vec![keep_a, keep_b, keep_c]
    );
}

async fn insert_named_fact(
    graph: &PostgresGraphStore,
    storage_partition_id: &str,
    name: &str,
    valid_from: DateTime<Utc>,
    confidence: f64,
    reference_count: i64,
) -> Uuid {
    let uid = Uuid::now_v7();
    graph
        .create_node(NodeWriteIntent {
            barrier: None,
            uid,
            data_subject_id: scope(storage_partition_id).tenant_id().0,
            label: NodeLabel::Fact,
            storage_partition_id: Some(storage_partition_id.to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: name.to_string(),
            properties: json!({ "summary": name, "reference_count": reference_count }),
            pii_class: SensitivityClass::None,
            confidence: Some(confidence),
            valid_from,
            embedding: None,
            embedding_model: None,
            embedding_model_version: None,
            embedding_text: None,
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("insert named lexical fact");
    uid
}

/// Pins: batched seed lookup returns exact per-name parity with N single lookups,
/// preserves input order, keeps an empty slot for a name that matches nothing, and
/// applies the same recency/confidence/reference ranking within each name (the two
/// `alpha` facts are ordered by recency over confidence, so a divergent batch
/// ranking would flip them).
#[tokio::test]
async fn lookup_seeds_batch_matches_single_name_lookups() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = format!("lexical-batch-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let now = moa_test_support::fixtures::pg_now();

    // "alpha" matches two facts whose ranking hinges on the recency weight: the
    // fresh-but-low-quality fact must outrank the old-but-high-quality one, so a
    // batch query with a different recency coefficient would reorder them.
    insert_named_fact(
        &graph,
        &storage_partition_id,
        "alpha search service",
        now,
        0.1,
        0,
    )
    .await;
    insert_named_fact(
        &graph,
        &storage_partition_id,
        "alpha search platform",
        now - Duration::days(10),
        1.0,
        100,
    )
    .await;
    insert_named_fact(
        &graph,
        &storage_partition_id,
        "beta cache backend",
        now,
        0.8,
        1,
    )
    .await;
    insert_named_fact(
        &graph,
        &storage_partition_id,
        "gamma queue worker",
        now,
        0.8,
        1,
    )
    .await;

    let names = ["alpha", "beta", "gamma", "delta"];
    let batched = graph
        .lookup_seeds_batch(&names, 10, None)
        .await
        .expect("batched seed lookup");

    let mut singles = Vec::with_capacity(names.len());
    for name in names {
        singles.push(
            graph
                .lookup_seeds(name, 10, None)
                .await
                .expect("single seed lookup"),
        );
    }

    assert_eq!(
        batched, singles,
        "batched lookup must match N single-name lookups per name and in order"
    );
    assert_eq!(batched.len(), names.len());
    assert_eq!(
        batched[0].len(),
        2,
        "alpha matches both alpha-prefixed facts"
    );
    assert_eq!(batched[1].len(), 1);
    assert_eq!(batched[2].len(), 1);
    assert!(
        batched[3].is_empty(),
        "delta matches nothing but keeps its ordinal slot"
    );
}
