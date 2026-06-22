//! Read-side smoke tests for `GraphStore`.

use chrono::{DateTime, Duration, Utc};
use moa_core::{ScopeContext, ScopedConn, TenantId};
use moa_memory_graph::{
    AgeGraphStore, EdgeLabel, EdgeWriteIntent, GraphStore, NodeLabel, NodeWriteIntent, PiiClass,
    cypher,
};
use moa_session::testing;
use serde_json::json;
use sqlx::Row;
use tokio::sync::Mutex;
use uuid::Uuid;

static TEST_LOCK: Mutex<()> = Mutex::const_new(());

fn tenant_scope(workspace_id: impl AsRef<str>) -> ScopeContext {
    let workspace_id = workspace_id.as_ref();
    let tenant_id = Uuid::parse_str(workspace_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(workspace_id)));
    ScopeContext::tenant(tenant_id)
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

async fn set_app_role(conn: &mut sqlx::PgConnection) {
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn)
        .await
        .expect("set moa_app role");
}

#[tokio::test]
async fn cypher_template_create_uses_bound_params() {
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-template-{run_id}");
    let uid = format!("entity-{run_id}");
    let ctx = tenant_scope(workspace_id.clone());
    let mut conn = ScopedConn::begin(store.pool(), &ctx)
        .await
        .expect("begin scoped template transaction");
    set_app_role(conn.as_mut()).await;

    let params = json!({
        "uid": uid,
        "workspace_id": workspace_id,
        "user_id": "",
        "scope": "workspace",
        "name": "template smoke",
        "pii_class": "none",
        "valid_from": Utc::now().to_rfc3339(),
        "created_at": Utc::now().to_rfc3339(),
        "properties": { "smoke": true }
    });
    let row = cypher::node::CREATE_ENTITY
        .execute(&params)
        .fetch_one(conn.as_mut())
        .await
        .expect("create AGE node through Cypher template");
    let rendered = row
        .try_get::<String, _>(0)
        .expect("decode cypher result text");
    assert!(rendered.contains("entity-"), "{rendered}");

    conn.rollback()
        .await
        .expect("rollback template smoke transaction");
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

async fn seed_node(pool: &sqlx::PgPool, workspace_id: &str, uid: Uuid, name: &str) {
    let ctx = tenant_scope(workspace_id);
    let mut conn = ScopedConn::begin(pool, &ctx)
        .await
        .expect("begin scoped seed transaction");
    set_app_role(conn.as_mut()).await;
    sqlx::query(
        "INSERT INTO moa.node_index \
         (uid, label, workspace_id, name, pii_class, confidence) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(uid)
    .bind(NodeLabel::Fact.as_str())
    .bind(workspace_id)
    .bind(name)
    .bind(PiiClass::None.as_str())
    .bind(0.99_f64)
    .execute(conn.as_mut())
    .await
    .expect("insert node_index seed row");
    conn.commit().await.expect("commit seed transaction");
}

async fn delete_node(pool: &sqlx::PgPool, uid: Uuid) {
    sqlx::query("DELETE FROM moa.node_index WHERE uid = $1")
        .bind(uid)
        .execute(pool)
        .await
        .expect("delete seeded node_index row");
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("test timestamp should be valid RFC3339")
        .with_timezone(&Utc)
}

fn node_intent(
    workspace_id: &str,
    label: NodeLabel,
    uid: Uuid,
    name: &str,
    valid_from: DateTime<Utc>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid,
        label,
        workspace_id: Some(workspace_id.to_string()),
        user_id: None,
        scope: "workspace".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "read_smoke" }),
        pii_class: PiiClass::None,
        confidence: Some(0.99),
        valid_from,
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

async fn create_superseded_neighbor_case(
    graph: &AgeGraphStore,
    workspace_id: &str,
    run_id: &str,
) -> (Uuid, Uuid, Uuid, DateTime<Utc>, DateTime<Utc>) {
    let old_valid_from = utc("2026-02-01T00:00:00Z");
    let new_valid_from = utc("2026-04-01T00:00:00Z");
    let old_uid = Uuid::now_v7();
    let old = node_intent(
        workspace_id,
        NodeLabel::Fact,
        old_uid,
        &format!("temporal neighbor fact {run_id}"),
        old_valid_from,
    );
    graph.create_node(old).await.expect("create old graph node");
    let new_uid = Uuid::now_v7();
    let new = node_intent(
        workspace_id,
        NodeLabel::Fact,
        new_uid,
        &format!("temporal neighbor fact {run_id}"),
        new_valid_from,
    );
    graph
        .supersede_node(old_uid, new)
        .await
        .expect("supersede old graph node");

    let target_uid = Uuid::now_v7();
    let target = node_intent(
        workspace_id,
        NodeLabel::Entity,
        target_uid,
        &format!("active neighbor entity {run_id}"),
        new_valid_from,
    );
    graph
        .create_node(target)
        .await
        .expect("create active neighbor node");
    graph
        .create_edge(EdgeWriteIntent {
            uid: Uuid::now_v7(),
            label: EdgeLabel::RelatesTo,
            start_uid: new_uid,
            end_uid: target_uid,
            properties: json!({ "kind": "read_smoke" }),
            workspace_id: Some(workspace_id.to_string()),
            user_id: None,
            scope: "workspace".to_string(),
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("create active neighbor edge");

    (old_uid, new_uid, target_uid, old_valid_from, new_valid_from)
}

#[tokio::test]
async fn read_smoke_get_node_and_lookup_seeds() {
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-read-{run_id}");
    let uid = Uuid::now_v7();
    let name = format!("auth service graph smoke {run_id}");
    seed_node(store.pool(), &workspace_id, uid, &name).await;

    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(workspace_id.clone()),
    );
    let row = graph
        .get_node(uid)
        .await
        .expect("get node through graph store")
        .expect("seeded node is visible");
    assert_eq!(row.uid, uid);
    assert_eq!(row.label, NodeLabel::Fact);
    assert_eq!(row.workspace_id.as_deref(), Some(workspace_id.as_str()));

    let seeds = graph
        .lookup_seeds("auth", 10, None)
        .await
        .expect("lookup lexical seeds through graph store");
    assert!(seeds.iter().any(|seed| seed.uid == uid));

    delete_node(store.pool(), uid).await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn lookup_seeds_prefers_exact_name_over_same_shape_siblings() {
    // Pins: planner seed lookup anchors code-like entity names before recency-ranked siblings.
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-seed-exact-{run_id}");
    let exact_uid = Uuid::now_v7();
    let sibling_uid = Uuid::now_v7();
    seed_node(
        store.pool(),
        &workspace_id,
        sibling_uid,
        "audit-shipper-dep-0-4-0",
    )
    .await;
    seed_node(
        store.pool(),
        &workspace_id,
        exact_uid,
        "audit-shipper-dep-0-0-0",
    )
    .await;

    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(workspace_id.clone()),
    );
    let seeds = graph
        .lookup_seeds("audit-shipper-dep-0-0-0", 10, None)
        .await
        .expect("lookup exact code-like seed");

    assert_eq!(seeds.first().map(|row| row.uid), Some(exact_uid));

    delete_node(store.pool(), exact_uid).await;
    delete_node(store.pool(), sibling_uid).await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn graph_neighbors_with_no_as_of_returns_active_rows_only() {
    // Pins: non-temporal graph traversal emits active sidecar rows and excludes superseded rows.
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-neighbor-active-{run_id}");
    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(workspace_id.clone()),
    );
    let (old_uid, new_uid, target_uid, _, _) =
        create_superseded_neighbor_case(&graph, &workspace_id, &run_id).await;

    let neighbors = graph
        .neighbors(new_uid, 1, None, None)
        .await
        .expect("read active neighbors");
    let neighbor_uids = neighbors.iter().map(|row| row.uid).collect::<Vec<_>>();

    assert_eq!(neighbor_uids, vec![target_uid]);
    assert!(!neighbor_uids.contains(&old_uid));

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn graph_neighbors_as_of_returns_superseded_node_inside_window() {
    // Pins: graph traversal can emit a superseded row when `as_of` falls inside its validity window.
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-neighbor-as-of-{run_id}");
    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(workspace_id.clone()),
    );
    let (old_uid, new_uid, _, old_valid_from, _) =
        create_superseded_neighbor_case(&graph, &workspace_id, &run_id).await;

    let neighbors = graph
        .neighbors(
            new_uid,
            1,
            None,
            Some(old_valid_from + Duration::minutes(5)),
        )
        .await
        .expect("read historical neighbors");
    let neighbor_uids = neighbors.iter().map(|row| row.uid).collect::<Vec<_>>();

    assert_eq!(neighbor_uids, vec![old_uid]);

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn lookup_seeds_as_of_includes_invalidated_node_inside_window() {
    // Pins: seed lookup uses the same bitemporal predicate as retrieval hydration.
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-seed-as-of-{run_id}");
    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(workspace_id.clone()),
    );
    let (old_uid, new_uid, _, old_valid_from, _) =
        create_superseded_neighbor_case(&graph, &workspace_id, &run_id).await;
    let query = format!("temporal neighbor fact {run_id}");

    let historical = graph
        .lookup_seeds(&query, 10, Some(old_valid_from + Duration::minutes(5)))
        .await
        .expect("lookup historical seeds");
    let active = graph
        .lookup_seeds(&query, 10, None)
        .await
        .expect("lookup active seeds");

    assert_eq!(
        historical.iter().map(|row| row.uid).collect::<Vec<_>>(),
        vec![old_uid]
    );
    assert_eq!(
        active.iter().map(|row| row.uid).collect::<Vec<_>>(),
        vec![new_uid]
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn expand_seeds_returns_two_hop_fact_via_shared_entity() {
    // Pins: batched graph expansion returns hop count and edge labels across Fact -> Entity -> Fact.
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-expand-two-hop-{run_id}");
    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(workspace_id.clone()),
    );
    let valid_from = utc("2026-02-01T00:00:00Z");
    let source_uid = Uuid::now_v7();
    let entity_uid = Uuid::now_v7();
    let related_uid = Uuid::now_v7();
    graph
        .create_node(node_intent(
            &workspace_id,
            NodeLabel::Fact,
            source_uid,
            &format!("source fact {run_id}"),
            valid_from,
        ))
        .await
        .expect("create source fact");
    graph
        .create_node(node_intent(
            &workspace_id,
            NodeLabel::Entity,
            entity_uid,
            &format!("shared library {run_id}"),
            valid_from,
        ))
        .await
        .expect("create shared entity");
    graph
        .create_node(node_intent(
            &workspace_id,
            NodeLabel::Fact,
            related_uid,
            &format!("related owner fact {run_id}"),
            valid_from,
        ))
        .await
        .expect("create related fact");
    create_edge(&graph, &workspace_id, source_uid, entity_uid, "object").await;
    create_edge(&graph, &workspace_id, entity_uid, related_uid, "subject").await;

    let hits = graph
        .expand_seeds(&[source_uid], 3, None)
        .await
        .expect("expand source fact");
    let related = hits
        .iter()
        .find(|hit| hit.uid == related_uid)
        .expect("two-hop fact should be expanded");

    assert_eq!(related.seed, source_uid);
    assert_eq!(related.label, NodeLabel::Fact);
    assert_eq!(related.hop, 2);
    assert_eq!(
        related.edges,
        vec![EdgeLabel::RelatesTo, EdgeLabel::RelatesTo]
    );

    let _ = graph
        .hard_purge(related_uid, "redacted:expand-two-hop")
        .await;
    let _ = graph
        .hard_purge(entity_uid, "redacted:expand-two-hop")
        .await;
    let _ = graph
        .hard_purge(source_uid, "redacted:expand-two-hop")
        .await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn expand_seeds_respects_as_of_validity_at_every_hop() {
    // Pins: an invalid intermediate node cannot extend present-time expansion.
    let _guard = TEST_LOCK.lock().await;
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let run_id = Uuid::now_v7().simple().to_string();
    let workspace_id = format!("graph-expand-validity-{run_id}");
    let graph = AgeGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(workspace_id.clone()),
    );
    let old_valid_from = utc("2026-02-01T00:00:00Z");
    let new_valid_from = utc("2026-04-01T00:00:00Z");
    let seed_uid = Uuid::now_v7();
    let old_entity_uid = Uuid::now_v7();
    let target_uid = Uuid::now_v7();
    graph
        .create_node(node_intent(
            &workspace_id,
            NodeLabel::Fact,
            seed_uid,
            &format!("seed fact {run_id}"),
            old_valid_from,
        ))
        .await
        .expect("create seed fact");
    graph
        .create_node(node_intent(
            &workspace_id,
            NodeLabel::Entity,
            old_entity_uid,
            &format!("old intermediate entity {run_id}"),
            old_valid_from,
        ))
        .await
        .expect("create old intermediate");
    graph
        .create_node(node_intent(
            &workspace_id,
            NodeLabel::Fact,
            target_uid,
            &format!("target fact {run_id}"),
            old_valid_from,
        ))
        .await
        .expect("create target fact");
    create_edge(&graph, &workspace_id, seed_uid, old_entity_uid, "object").await;
    create_edge(&graph, &workspace_id, old_entity_uid, target_uid, "subject").await;
    let mut replacement = node_intent(
        &workspace_id,
        NodeLabel::Entity,
        Uuid::now_v7(),
        &format!("new intermediate entity {run_id}"),
        new_valid_from,
    );
    replacement.valid_from = new_valid_from;
    let new_entity_uid = graph
        .supersede_node(old_entity_uid, replacement)
        .await
        .expect("supersede intermediate entity");

    let current_hits = graph
        .expand_seeds(&[seed_uid], 3, None)
        .await
        .expect("expand present-time paths");
    let historical_hits = graph
        .expand_seeds(&[seed_uid], 3, Some(old_valid_from + Duration::minutes(5)))
        .await
        .expect("expand historical paths");

    assert!(
        current_hits.iter().all(|hit| hit.uid != target_uid),
        "{current_hits:?}"
    );
    assert!(
        historical_hits.iter().any(|hit| hit.uid == target_uid),
        "{historical_hits:?}"
    );

    let _ = graph
        .hard_purge(new_entity_uid, "redacted:expand-validity")
        .await;
    let _ = graph
        .hard_purge(target_uid, "redacted:expand-validity")
        .await;
    let _ = graph
        .hard_purge(old_entity_uid, "redacted:expand-validity")
        .await;
    let _ = graph.hard_purge(seed_uid, "redacted:expand-validity").await;
    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

async fn create_edge(
    graph: &AgeGraphStore,
    workspace_id: &str,
    start_uid: Uuid,
    end_uid: Uuid,
    role: &str,
) {
    graph
        .create_edge(EdgeWriteIntent {
            uid: Uuid::now_v7(),
            label: EdgeLabel::RelatesTo,
            start_uid,
            end_uid,
            properties: json!({ "role": role, "source": "read_smoke_expand" }),
            workspace_id: Some(workspace_id.to_string()),
            user_id: None,
            scope: "workspace".to_string(),
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
        .expect("create expansion edge");
}
