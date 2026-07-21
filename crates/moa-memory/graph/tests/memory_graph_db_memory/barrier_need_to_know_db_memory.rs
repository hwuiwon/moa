//! Information-barrier / need-to-know retrieval RLS coverage.
//!
//! Pins the database-enforced guarantee that a barriered `moa.node_index` row is
//! retrievable only by a caller whose `moa.cleared_barriers` clearance set
//! contains the row's `barrier` tag, that an unset/empty clearance fails closed
//! (barriered rows hidden), and that NULL-barrier rows are never affected. The
//! clearance travels the production path: `RlsContext::with_cleared_barriers` ->
//! `ScopedConn` GUC -> the `rd_barrier_need_to_know` RESTRICTIVE policy.

use chrono::Utc;
use moa_core::types::identifiers::{StoragePartitionId, TenantId};
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_db::ScopedConn;
use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_session::testing;
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// Builds a tenant-scoped node intent carrying an optional information-barrier tag.
fn barrier_node_intent(tenant_id: TenantId, name: &str, barrier: Option<&str>) -> NodeWriteIntent {
    NodeWriteIntent {
        uid: Uuid::now_v7(),
        data_subject_id: tenant_id.0,
        label: NodeLabel::Fact,
        storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
        contact_id: None,
        scope: "tenant".to_string(),
        name: name.to_string(),
        properties: json!({ "name": name, "source": "barrier_need_to_know_db_memory" }),
        pii_class: SensitivityClass::None,
        barrier: barrier.map(|value| {
            moa_core::types::memory::InformationBarrierId::parse(value).expect("valid barrier")
        }),
        confidence: Some(0.9),
        valid_from: Utc::now(),
        embedding: None,
        embedding_model: None,
        embedding_model_version: None,
        embedding_text: None,
        actor_id: Uuid::now_v7().to_string(),
        actor_kind: "user".to_string(),
    }
}

/// Returns which of `candidate_uids` are SELECT-able as `moa_app` under the given
/// cleared-barrier set, exercising the exact RLS path production retrieval uses.
async fn visible_uids(
    pool: &PgPool,
    tenant_id: TenantId,
    cleared: &[&str],
    candidate_uids: &[Uuid],
) -> Vec<Uuid> {
    let ctx = RlsContext::tenant(tenant_id).with_cleared_barriers(
        cleared
            .iter()
            .map(|tag| {
                moa_core::types::memory::InformationBarrierId::parse(*tag).expect("valid barrier")
            })
            .collect(),
    );
    let mut conn = ScopedConn::begin_as_app(pool, &ctx, true)
        .await
        .expect("begin app-role barrier read");
    let rows = sqlx::query_scalar::<_, Uuid>(
        "SELECT uid FROM moa.node_index WHERE uid = ANY($1) ORDER BY uid",
    )
    .bind(candidate_uids)
    .fetch_all(conn.as_mut())
    .await
    .expect("select barrier-gated node uids");
    conn.commit().await.expect("commit barrier read");
    rows
}

#[tokio::test]
async fn barrier_hidden_without_clearance_and_visible_with_clearance_db_memory() {
    // Pins: a barriered node is returned only when the caller's cleared set holds
    // its tag; an empty or non-matching clearance hides it (fail closed). A NULL
    // barrier sibling stays visible throughout. Mutation check: relax
    // `rd_barrier_need_to_know` USING to `true` (or drop it) and the empty/
    // non-matching assertions below start returning the barriered uid, failing.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated barrier store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let graph = PostgresGraphStore::scoped_for_app_role(
        store.pool().clone(),
        RlsContext::tenant(tenant_id),
        super::test_kms(),
    );

    let barriered = graph
        .create_node(barrier_node_intent(
            tenant_id,
            "deal alpha restricted memo",
            Some("deal-alpha"),
        ))
        .await
        .expect("write barriered node");
    let unrestricted = graph
        .create_node(barrier_node_intent(tenant_id, "public roster", None))
        .await
        .expect("write null-barrier node");
    let candidates = [barriered, unrestricted];

    let cleared = visible_uids(store.pool(), tenant_id, &["deal-alpha"], &candidates).await;
    assert!(
        cleared.contains(&barriered) && cleared.contains(&unrestricted),
        "clearance for the tag must reveal the barriered node alongside the unrestricted one"
    );

    let no_clearance = visible_uids(store.pool(), tenant_id, &[], &candidates).await;
    assert_eq!(
        no_clearance,
        vec![unrestricted],
        "empty clearance must hide the barriered node and keep the null-barrier node (fail closed)"
    );

    let wrong_clearance = visible_uids(store.pool(), tenant_id, &["deal-beta"], &candidates).await;
    assert_eq!(
        wrong_clearance,
        vec![unrestricted],
        "a non-matching clearance must not reveal the barriered node"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("cleanup isolated barrier store");
}

#[tokio::test]
async fn partial_barrier_clearance_returns_only_cleared_tag_db_memory() {
    // Pins: with two distinctly barriered nodes and clearance for exactly one
    // tag, only the cleared node is retrievable -- desk-A clearance never spills
    // into desk-B's segregated memory.
    let (store, database_url, schema_name) = testing::create_isolated_test_store()
        .await
        .expect("create isolated partial-barrier store");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let graph = PostgresGraphStore::scoped_for_app_role(
        store.pool().clone(),
        RlsContext::tenant(tenant_id),
        super::test_kms(),
    );

    let alpha = graph
        .create_node(barrier_node_intent(
            tenant_id,
            "alpha wall memo",
            Some("deal-alpha"),
        ))
        .await
        .expect("write deal-alpha node");
    let beta = graph
        .create_node(barrier_node_intent(
            tenant_id,
            "beta wall memo",
            Some("deal-beta"),
        ))
        .await
        .expect("write deal-beta node");
    let candidates = [alpha, beta];

    let cleared = visible_uids(store.pool(), tenant_id, &["deal-alpha"], &candidates).await;
    assert_eq!(
        cleared,
        vec![alpha],
        "clearance for deal-alpha must return only the deal-alpha node, not deal-beta"
    );

    drop(store);
    testing::cleanup_test_schema(&database_url, &schema_name)
        .await
        .expect("cleanup isolated partial-barrier store");
}
