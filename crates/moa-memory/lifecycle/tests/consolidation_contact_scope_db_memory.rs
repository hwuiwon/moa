//! Postgres-backed checks for contact-local memory consolidation.

use chrono::{Duration, TimeZone, Utc};
use moa_core::{ContactId, StoragePartitionId, TenantId};
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeLabel, NodeWriteIntent, PiiClass};
use moa_memory_lifecycle::merge_duplicates;
use moa_memory_types::ScopeContext;
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn configured_test_db() -> Option<TestDb> {
    std::env::var_os("MOA_DATABASE_URL")?;
    Some(
        bootstrap_test_db()
            .await
            .expect("bootstrap Postgres test database"),
    )
}

#[tokio::test]
#[ignore = "requires MOA_DATABASE_URL and a reachable Postgres instance"]
async fn duplicate_merge_keeps_contact_collisions_separate_db_memory() {
    // Pins: exact duplicate consolidation keys by tenant/contact ownership.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let tenant_id = TenantId::from(Uuid::now_v7());
    let contact_a = ContactId(Uuid::now_v7());
    let contact_b = ContactId(Uuid::now_v7());
    let base = Utc
        .with_ymd_and_hms(2026, 6, 23, 0, 0, 0)
        .single()
        .expect("fixed timestamp");

    let a1 = create_contact_fact(
        test_db.store().pool(),
        tenant_id,
        contact_a,
        "contact-a duplicate one",
        "same-hash",
        base,
    )
    .await;
    let a2 = create_contact_fact(
        test_db.store().pool(),
        tenant_id,
        contact_a,
        "contact-a duplicate two",
        "same-hash",
        base + Duration::seconds(1),
    )
    .await;
    let b1 = create_contact_fact(
        test_db.store().pool(),
        tenant_id,
        contact_b,
        "contact-b same hash",
        "same-hash",
        base + Duration::seconds(2),
    )
    .await;

    let stats = merge_duplicates(
        test_db.store().pool(),
        &tenant_id,
        base + Duration::hours(1),
    )
    .await
    .expect("merge duplicates");

    assert_eq!(stats.merged, 1);
    assert_eq!(stats.duplicates_remaining, 0);
    assert_active(test_db.store().pool(), b1, true).await;
    assert_active(test_db.store().pool(), a1, true).await;
    assert_active(test_db.store().pool(), a2, false).await;
}

async fn create_contact_fact(
    pool: &PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    name: &str,
    fact_hash: &str,
    valid_from: chrono::DateTime<Utc>,
) -> Uuid {
    let graph = AgeGraphStore::scoped_for_app_role(
        pool.clone(),
        ScopeContext::contact(tenant_id, contact_id),
    );
    let uid = Uuid::now_v7();
    graph
        .create_node(NodeWriteIntent {
            uid,
            label: NodeLabel::Fact,
            storage_partition_id: Some(StoragePartitionId::for_tenant(tenant_id).to_string()),
            contact_id: Some(contact_id.to_string()),
            scope: "contact".to_string(),
            name: name.to_string(),
            properties: json!({
                "summary": name,
                "subject": "contact preference",
                "predicate": "prefers",
                "object": name,
                "fact_hash": fact_hash,
                "source": "consolidation_contact_scope_db_memory",
            }),
            pii_class: PiiClass::None,
            confidence: Some(0.9),
            valid_from,
            embedding: None,
            embedding_model: None,
            embedding_model_version: None,
            actor_id: contact_id.to_string(),
            actor_kind: "contact".to_string(),
        })
        .await
        .expect("seed contact fact");
    uid
}

async fn assert_active(pool: &PgPool, uid: Uuid, expected: bool) {
    let active =
        sqlx::query_scalar::<_, bool>("SELECT valid_to IS NULL FROM moa.node_index WHERE uid = $1")
            .bind(uid)
            .fetch_one(pool)
            .await
            .expect("read node active state");
    assert_eq!(active, expected, "unexpected active state for {uid}");
}
