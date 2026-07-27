//! Scored-traversal coverage: in-walk pruning and path-score output.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_memory_graph::{
    EdgeLabel, EdgeWriteIntent, GraphStore, GraphWalkScoring, NodeLabel, NodeWriteIntent,
    PostgresGraphStore,
};
use moa_session::testing;
use moa_test_support::fixtures::stable_uuid_from_label;
use serde_json::json;
use uuid::Uuid;

fn tenant_scope(storage_partition_id: &str) -> RlsContext {
    let tenant_id = Uuid::parse_str(storage_partition_id)
        .map(TenantId::from)
        .unwrap_or_else(|_| TenantId::from(stable_uuid_from_label(storage_partition_id)));
    RlsContext::tenant(tenant_id)
}

fn node_intent(
    storage_partition_id: &str,
    label: NodeLabel,
    name: &str,
    valid_from: DateTime<Utc>,
) -> NodeWriteIntent {
    NodeWriteIntent {
        barrier: None,
        uid: Uuid::now_v7(),
        data_subject_id: tenant_scope(storage_partition_id).tenant_id().0,
        label,
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "scored_walk" }),
        pii_class: SensitivityClass::None,
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
    label: EdgeLabel,
    start_uid: Uuid,
    end_uid: Uuid,
    valid_from: DateTime<Utc>,
) -> EdgeWriteIntent {
    EdgeWriteIntent {
        uid: Uuid::now_v7(),
        label,
        start_uid,
        end_uid,
        valid_from,
        properties: json!({ "source": "scored_walk" }),
        storage_partition_id: Some(storage_partition_id.to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "system".to_string(),
    }
}

#[tokio::test]
async fn zero_prior_hub_fanout_is_pruned_in_walk_and_deep_paths_survive_with_scores() {
    // Pins: hub fan-out behind zero-prior edges is pruned inside the recursive
    // CTE instead of flooding the row limit, so a two-hop target behind a
    // semantic path is still reached; each hit carries the walk's
    // decay^hop path score.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated Postgres store");
    let storage_partition_id = Uuid::now_v7().to_string();
    let graph = PostgresGraphStore::scoped_for_app_role(
        store.pool().clone(),
        tenant_scope(&storage_partition_id),
        super::test_kms(),
    );
    let now = moa_test_support::fixtures::pg_now();
    let seed_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Entity,
            "scored walk hub seed",
            now,
        ))
        .await
        .expect("create seed");

    // 220 hub neighbors behind CONTRADICTS edges: prior 0.0 in the default
    // scoring, so every branch scores below the prune threshold. Before the
    // scored walk these rows alone exceeded the 200-row output limit for a
    // single-seed expansion.
    let mut contradicted = Vec::new();
    for index in 0..220 {
        let uid = graph
            .create_node(node_intent(
                &storage_partition_id,
                NodeLabel::Fact,
                &format!("contradicted hub fact {index}"),
                now,
            ))
            .await
            .expect("create hub fact");
        graph
            .create_edge(edge_intent(
                &storage_partition_id,
                EdgeLabel::Contradicts,
                seed_uid,
                uid,
                now,
            ))
            .await
            .expect("create hub edge");
        contradicted.push(uid);
    }

    let bridge_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Entity,
            "scored walk bridge entity",
            now,
        ))
        .await
        .expect("create bridge");
    let target_uid = graph
        .create_node(node_intent(
            &storage_partition_id,
            NodeLabel::Fact,
            "scored walk two-hop target fact",
            now,
        ))
        .await
        .expect("create target");
    graph
        .create_edge(edge_intent(
            &storage_partition_id,
            EdgeLabel::RelatesTo,
            seed_uid,
            bridge_uid,
            now,
        ))
        .await
        .expect("create seed->bridge edge");
    graph
        .create_edge(edge_intent(
            &storage_partition_id,
            EdgeLabel::RelatesTo,
            bridge_uid,
            target_uid,
            now,
        ))
        .await
        .expect("create bridge->target edge");

    let scoring = GraphWalkScoring::default();
    let hits = graph
        .expand_seeds(&[seed_uid], 3, None, &scoring)
        .await
        .expect("expand scored walk");

    let target = hits
        .iter()
        .find(|hit| hit.uid == target_uid)
        .expect("two-hop target must survive hub fan-out");
    assert_eq!(target.hop, 2);
    let expected_two_hop = scoring.decay * scoring.decay;
    assert!(
        (target.path_score - expected_two_hop).abs() < 1e-9,
        "two-hop path score must be decay^2: {}",
        target.path_score
    );
    let bridge = hits
        .iter()
        .find(|hit| hit.uid == bridge_uid)
        .expect("one-hop bridge is reachable");
    assert!(
        (bridge.path_score - scoring.decay).abs() < 1e-9,
        "one-hop path score must be decay: {}",
        bridge.path_score
    );
    assert!(
        hits.iter().all(|hit| !contradicted.contains(&hit.uid)),
        "zero-prior contradicts branches must be pruned inside the walk"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("drop isolated schema");
}
