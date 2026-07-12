//! Bitemporal edge-validity coverage for graph traversal and the write protocol.

use chrono::{DateTime, Duration, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, GraphWalkScoring, NodeLabel, NodeWriteIntent, PiiClass,
    PostgresGraphStore,
};
use moa_session::testing;
use moa_test_support::fixtures::stable_uuid_from_label;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

fn tenant_scope(storage_partition_id: &str) -> RlsContext {
    let tenant_id = Uuid::parse_str(storage_partition_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id)));
    RlsContext::tenant(tenant_id)
}

fn utc(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .expect("test timestamp should be valid RFC3339")
        .with_timezone(&Utc)
}

fn node_intent(
    storage_partition_id: &str,
    label: NodeLabel,
    name: &str,
    valid_from: DateTime<Utc>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "edge_validity" }),
        pii_class: PiiClass::None,
        confidence: Some(0.99),
        valid_from,
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

fn edge_intent(
    storage_partition_id: &str,
    start_uid: Uuid,
    end_uid: Uuid,
    valid_from: DateTime<Utc>,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: Uuid::now_v7(),
        label: EdgeLabel::RelatesTo,
        start_uid,
        end_uid,
        valid_from,
        properties: json!({ "source": "edge_validity" }),
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

#[tokio::test]
async fn future_edge_is_invisible_to_as_of_walks_in_the_past() {
    // Pins: an edge created with a later valid_from does not leak into as-of
    // expansion at an earlier instant, even when both endpoints were valid then.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let graph = PostgresGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let node_valid_from = utc("2026-01-01T00:00:00Z");
    let edge_valid_from = utc("2026-06-01T00:00:00Z");
    let seed_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Entity,
            "future edge seed entity",
            node_valid_from,
        ))
        .await
        .expect("create seed node");
    let target_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Fact,
            "future edge target fact",
            node_valid_from,
        ))
        .await
        .expect("create target node");
    graph
        .create_edge(edge_intent(
            &storage_partition_id,
            seed_uid,
            target_uid,
            edge_valid_from,
        ))
        .await
        .expect("create edge");

    let before_edge = graph
        .expand_seeds(
            &[seed_uid],
            2,
            Some(edge_valid_from - Duration::days(30)),
            &GraphWalkScoring::default(),
        )
        .await
        .expect("expand before the edge existed");
    assert!(
        before_edge.iter().all(|hit| hit.uid != target_uid),
        "edge must be invisible before its valid_from: {before_edge:?}"
    );

    let after_edge = graph
        .expand_seeds(
            &[seed_uid],
            2,
            Some(edge_valid_from + Duration::minutes(5)),
            &GraphWalkScoring::default(),
        )
        .await
        .expect("expand after the edge became valid");
    assert!(
        after_edge.iter().any(|hit| hit.uid == target_uid),
        "edge must be visible after its valid_from: {after_edge:?}"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}

#[tokio::test]
async fn node_supersession_closes_incident_edges_at_the_supersession_instant() {
    // Pins: superseding a node transactionally closes its still-active edges at
    // the replacement's valid_from, so relationships die with the node version
    // they described instead of outliving it.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let graph = PostgresGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(&storage_partition_id),
    );
    let old_valid_from = utc("2026-02-01T00:00:00Z");
    let new_valid_from = utc("2026-04-01T00:00:00Z");
    let old_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Fact,
            "supersession edge-close fact",
            old_valid_from,
        ))
        .await
        .expect("create old node");
    let neighbor_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Entity,
            "supersession edge-close neighbor",
            old_valid_from,
        ))
        .await
        .expect("create neighbor node");
    let edge = edge_intent(&storage_partition_id, old_uid, neighbor_uid, old_valid_from);
    let edge_uid = edge.uid;
    graph.create_edge(edge).await.expect("create edge");

    let mut replacement = node_intent(
        &storage_partition_id,
        NodeLabel::Fact,
        "supersession edge-close fact",
        new_valid_from,
    );
    replacement.valid_from = new_valid_from;
    graph
        .supersede_node(old_uid, replacement)
        .await
        .expect("supersede old node");

    let row = sqlx::query("SELECT valid_to FROM moa.edge_index WHERE uid = $1")
        .bind(edge_uid)
        .fetch_one(store.pool())
        .await
        .expect("read closed edge row");
    assert_eq!(
        row.get::<Option<DateTime<Utc>>, _>("valid_to"),
        Some(new_valid_from),
        "incident edge must close exactly at the supersession instant"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
