//! Integration coverage for concurrent graph-memory writers.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use moa_core::RlsContext;
use moa_core::TenantId;
use moa_db::ScopedConn;
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::{Config as ProptestConfig, TestRunner};
use serde_json::json;
use sqlx::{PgPool, Row};
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

fn stable_uuid_from_label(label: &str) -> Uuid {
    let mut bytes = [0_u8; 16];
    for (index, byte) in label.as_bytes().iter().copied().enumerate() {
        let slot = index % 16;
        bytes[slot] = bytes[slot]
            .wrapping_mul(31)
            .wrapping_add(byte)
            .wrapping_add(index as u8);
        let mirror = (index * 7 + 3) % 16;
        bytes[mirror] ^= byte.rotate_left((index % 8) as u32);
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

#[derive(Debug, Clone)]
struct ChangelogEdge {
    change_id: i64,
    cause_change_id: Option<i64>,
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

fn graph_store(test_db: &TestDb, storage_partition_id: &str) -> AgeGraphStore {
    AgeGraphStore::scoped_for_app_role(test_db.store().pool().clone(), scope(storage_partition_id))
}

fn node_intent(
    storage_partition_id: &str,
    uid: Uuid,
    name: impl Into<String>,
    valid_from: DateTime<Utc>,
    value: impl Into<String>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid,
        label: NodeLabel::Fact,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.into(),
        properties: json!({ "value": value.into() }),
        pii_class: PiiClass::None,
        confidence: Some(0.9),
        valid_from,
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn scoped_conn<'a>(pool: &'a PgPool, storage_partition_id: &str) -> ScopedConn<'a> {
    let mut conn = ScopedConn::begin(pool, &scope(storage_partition_id))
        .await
        .expect("begin scoped graph transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role");
    conn
}

async fn changelog_version(pool: &PgPool, storage_partition_id: &str) -> i64 {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let version = sqlx::query_scalar::<_, i64>(
        "SELECT changelog_version FROM moa.storage_partition_state WHERE storage_partition_id = $1",
    )
    .bind(storage_partition_id)
    .fetch_one(conn.as_mut())
    .await
    .expect("read changelog version");
    conn.commit().await.expect("commit version read");
    version
}

async fn active_nodes_named(
    pool: &PgPool,
    storage_partition_id: &str,
    name_prefix: &str,
) -> Vec<(Uuid, DateTime<Utc>)> {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query(
        "SELECT uid, valid_from FROM moa.node_index \
         WHERE name LIKE $1 AND valid_to IS NULL ORDER BY valid_from, uid",
    )
    .bind(format!("{name_prefix}%"))
    .fetch_all(conn.as_mut())
    .await
    .expect("read active graph nodes");
    conn.commit().await.expect("commit active node read");
    rows.into_iter()
        .map(|row| {
            (
                row.try_get::<Uuid, _>("uid").expect("decode uid"),
                row.try_get::<DateTime<Utc>, _>("valid_from")
                    .expect("decode valid_from"),
            )
        })
        .collect()
}

async fn changelog_edges(pool: &PgPool, storage_partition_id: &str) -> Vec<ChangelogEdge> {
    let mut conn = scoped_conn(pool, storage_partition_id).await;
    let rows = sqlx::query(
        "SELECT change_id, cause_change_id FROM moa.graph_changelog \
         WHERE storage_partition_id = $1 ORDER BY change_id",
    )
    .bind(storage_partition_id)
    .fetch_all(conn.as_mut())
    .await
    .expect("read changelog edges");
    conn.commit().await.expect("commit changelog edge read");
    rows.into_iter()
        .map(|row| ChangelogEdge {
            change_id: row.try_get("change_id").expect("decode change_id"),
            cause_change_id: row
                .try_get("cause_change_id")
                .expect("decode cause_change_id"),
        })
        .collect()
}

fn assert_changelog_forms_dag(edges: &[ChangelogEdge]) {
    let ids = edges
        .iter()
        .map(|edge| edge.change_id)
        .collect::<HashSet<_>>();
    for edge in edges {
        if let Some(cause) = edge.cause_change_id {
            assert!(
                ids.contains(&cause),
                "cause_change_id {cause} must resolve to an existing changelog row"
            );
            assert!(
                cause < edge.change_id,
                "cause_change_id {cause} must point backward from {}",
                edge.change_id
            );
        }
    }
}

async fn create_seed(
    graph: &AgeGraphStore,
    storage_partition_id: &str,
    name: &str,
    t0: DateTime<Utc>,
) -> Uuid {
    let uid = Uuid::now_v7();
    graph
        .create_node(node_intent(storage_partition_id, uid, name, t0, "seed"))
        .await
        .expect("create concurrent writer seed");
    uid
}

#[tokio::test]
async fn concurrent_supersedes_of_same_node_serialize_with_monotonic_changelog_versions() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = format!("concurrent-chain-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let t0 = Utc::now();
    let old_uid = create_seed(&graph, &storage_partition_id, "chain node", t0).await;

    let mut tasks = Vec::new();
    for index in 0..10 {
        let graph = graph.clone();
        let storage_partition_id = storage_partition_id.clone();
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(index * 5)).await;
            graph
                .supersede_node(
                    old_uid,
                    node_intent(
                        &storage_partition_id,
                        Uuid::now_v7(),
                        format!("chain node {index}"),
                        t0 + Duration::seconds(i64::try_from(index).expect("index fits") + 1),
                        format!("value {index}"),
                    ),
                )
                .await
        }));
    }

    for task in tasks {
        task.await
            .expect("join concurrent supersede task")
            .expect("concurrent supersede should serialize");
    }

    assert_eq!(
        changelog_version(test_db.store().pool(), &storage_partition_id).await,
        21
    );
    let active =
        active_nodes_named(test_db.store().pool(), &storage_partition_id, "chain node").await;
    assert_eq!(active.len(), 1);
    assert!(active[0].1 > t0);
    assert_changelog_forms_dag(
        &changelog_edges(test_db.store().pool(), &storage_partition_id).await,
    );
}

#[tokio::test]
async fn concurrent_writes_to_different_nodes_in_same_workspace_do_not_interfere() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = format!("concurrent-different-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let t0 = Utc::now();

    let mut tasks = Vec::new();
    for index in 0..10 {
        let graph = graph.clone();
        let storage_partition_id = storage_partition_id.clone();
        tasks.push(tokio::spawn(async move {
            let uid = Uuid::now_v7();
            graph
                .create_node(node_intent(
                    &storage_partition_id,
                    uid,
                    format!("different node {index}"),
                    t0 + Duration::seconds(i64::from(index)),
                    format!("value {index}"),
                ))
                .await
                .map(|_| uid)
        }));
    }

    let mut uids = Vec::new();
    for task in tasks {
        uids.push(
            task.await
                .expect("join concurrent create task")
                .expect("concurrent create should succeed"),
        );
    }

    let mut conn = scoped_conn(test_db.store().pool(), &storage_partition_id).await;
    for uid in uids {
        let count = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM moa.graph_changelog WHERE target_uid = $1 AND op = 'create'",
        )
        .bind(uid)
        .fetch_one(conn.as_mut())
        .await
        .expect("count changelog rows for independent node");
        assert_eq!(count, 1);
    }
    conn.commit().await.expect("commit independent node read");
}

#[tokio::test]
async fn concurrent_writes_to_same_node_across_workspaces_isolate_via_rls() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let workspace_a = format!("concurrent-rls-a-{}", Uuid::now_v7().simple());
    let workspace_b = format!("concurrent-rls-b-{}", Uuid::now_v7().simple());
    let graph_a = graph_store(&test_db, &workspace_a);
    let graph_b = graph_store(&test_db, &workspace_b);
    let t0 = Utc::now();
    let logical_name = "shared logical node";

    let task_a = tokio::spawn({
        let workspace_a = workspace_a.clone();
        async move {
            graph_a
                .create_node(node_intent(
                    &workspace_a,
                    Uuid::now_v7(),
                    logical_name,
                    t0,
                    "workspace-a",
                ))
                .await
        }
    });
    let task_b = tokio::spawn({
        let workspace_b = workspace_b.clone();
        async move {
            graph_b
                .create_node(node_intent(
                    &workspace_b,
                    Uuid::now_v7(),
                    logical_name,
                    t0,
                    "workspace-b",
                ))
                .await
        }
    });
    task_a
        .await
        .expect("join workspace A write")
        .expect("workspace A write succeeds");
    task_b
        .await
        .expect("join workspace B write")
        .expect("workspace B write succeeds");

    for storage_partition_id in [&workspace_a, &workspace_b] {
        let mut conn = scoped_conn(test_db.store().pool(), storage_partition_id).await;
        let visible = sqlx::query(
            "SELECT storage_partition_id, properties_summary->>'value' AS value \
             FROM moa.node_index WHERE name = $1 AND valid_to IS NULL",
        )
        .bind(logical_name)
        .fetch_all(conn.as_mut())
        .await
        .expect("read visible logical node rows");
        conn.commit().await.expect("commit RLS read");
        assert_eq!(visible.len(), 1);
        assert_eq!(
            visible[0]
                .try_get::<String, _>("storage_partition_id")
                .expect("decode workspace id"),
            *storage_partition_id
        );
    }
}

#[tokio::test]
async fn concurrent_supersede_with_contradicting_facts_chooses_one_deterministically() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let storage_partition_id = format!("concurrent-conflict-{}", Uuid::now_v7().simple());
    let graph = graph_store(&test_db, &storage_partition_id);
    let t0 = Utc::now();
    let old_uid = create_seed(&graph, &storage_partition_id, "conflicting node", t0).await;

    let first = tokio::spawn({
        let graph = graph.clone();
        let storage_partition_id = storage_partition_id.clone();
        async move {
            graph
                .supersede_node(
                    old_uid,
                    node_intent(
                        &storage_partition_id,
                        Uuid::now_v7(),
                        "conflicting node first",
                        t0 + Duration::seconds(1),
                        "value-one",
                    ),
                )
                .await
        }
    });
    let second = tokio::spawn({
        let graph = graph.clone();
        let storage_partition_id = storage_partition_id.clone();
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            graph
                .supersede_node(
                    old_uid,
                    node_intent(
                        &storage_partition_id,
                        Uuid::now_v7(),
                        "conflicting node second",
                        t0 + Duration::seconds(2),
                        "value-two",
                    ),
                )
                .await
        }
    });

    first
        .await
        .expect("join first conflicting writer")
        .expect("first conflicting write succeeds");
    second
        .await
        .expect("join second conflicting writer")
        .expect("second conflicting write supersedes first");

    let active = active_nodes_named(
        test_db.store().pool(),
        &storage_partition_id,
        "conflicting node",
    )
    .await;
    assert_eq!(active.len(), 1);
    let mut conn = scoped_conn(test_db.store().pool(), &storage_partition_id).await;
    let value = sqlx::query_scalar::<_, String>(
        "SELECT properties_summary->>'value' FROM moa.node_index WHERE uid = $1",
    )
    .bind(active[0].0)
    .fetch_one(conn.as_mut())
    .await
    .expect("read winning conflicting value");
    conn.commit().await.expect("commit conflicting value read");
    assert_eq!(value, "value-two");
}

#[tokio::test]
async fn proptest_arbitrary_concurrent_supersedes_yield_valid_dag() {
    let _guard = TEST_LOCK.lock().await;
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let mut runner = TestRunner::new(ProptestConfig::with_cases(10));
    let strategy = proptest::collection::vec(1usize..=10, 1..=5);

    for case_index in 0..10 {
        let writes_per_node = strategy
            .new_tree(&mut runner)
            .expect("generate proptest concurrent shape")
            .current();
        run_concurrent_case(&test_db, case_index, &writes_per_node).await;
    }
}

async fn run_concurrent_case(test_db: &TestDb, case_index: usize, writes_per_node: &[usize]) {
    let storage_partition_id = format!("concurrent-prop-{case_index}-{}", Uuid::now_v7().simple());
    let graph = graph_store(test_db, &storage_partition_id);
    let t0 = Utc::now();
    let mut seed_by_node = HashMap::new();

    for node_index in 0..writes_per_node.len() {
        let seed = create_seed(
            &graph,
            &storage_partition_id,
            &format!("prop node {case_index}-{node_index}"),
            t0,
        )
        .await;
        seed_by_node.insert(node_index, seed);
    }

    let mut tasks = Vec::new();
    for (node_index, write_count) in writes_per_node.iter().copied().enumerate() {
        for write_index in 0..write_count {
            let graph = graph.clone();
            let storage_partition_id = storage_partition_id.clone();
            let old_uid = seed_by_node[&node_index];
            tasks.push(tokio::spawn(async move {
                let delay = (node_index * 100 + write_index) as u64;
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                graph
                    .supersede_node(
                        old_uid,
                        node_intent(
                            &storage_partition_id,
                            Uuid::now_v7(),
                            format!("prop node {case_index}-{node_index}-{write_index}"),
                            t0 + Duration::seconds(
                                i64::try_from(write_index).expect("write index fits") + 1,
                            ),
                            format!("value {write_index}"),
                        ),
                    )
                    .await
            }));
        }
    }

    for task in tasks {
        task.await
            .expect("join generated supersede")
            .expect("generated supersede should serialize");
    }

    assert_changelog_forms_dag(
        &changelog_edges(test_db.store().pool(), &storage_partition_id).await,
    );
    for node_index in 0..writes_per_node.len() {
        let active = active_nodes_named(
            test_db.store().pool(),
            &storage_partition_id,
            &format!("prop node {case_index}-{node_index}"),
        )
        .await;
        assert_eq!(
            active.len(),
            1,
            "exactly one active row should remain for generated node {node_index}"
        );
    }
}
