//! Postgres-backed checks for embedding-blocked entity resolution: the blocking
//! self-join, the shared-neighbour structural gate, and same-scope merges.

use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use moa_core::{ContactId, StoragePartitionId, TenantId};
use moa_memory_graph::NodeIndexRow;
use moa_memory_ingest::{EntityMergeVerifier, Result as IngestResult};
use moa_memory_lifecycle::{EntityResolutionOptions, resolve_entity_duplicates};
use moa_test_support::postgres::{TestDb, bootstrap_test_db};
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

/// Merge verifier that accepts every gated pair.
struct AlwaysMergeVerifier;

#[async_trait]
impl EntityMergeVerifier for AlwaysMergeVerifier {
    async fn should_merge(&self, _mention: &str, _candidate: &NodeIndexRow) -> IngestResult<bool> {
        Ok(true)
    }
}

/// Merge verifier that fails the test if it is ever consulted.
struct NeverCalledVerifier;

#[async_trait]
impl EntityMergeVerifier for NeverCalledVerifier {
    async fn should_merge(&self, _mention: &str, _candidate: &NodeIndexRow) -> IngestResult<bool> {
        panic!("merge verifier must not be reached for a pair that fails the structural gate")
    }
}

#[tokio::test]
async fn shared_neighbor_near_duplicate_entities_merge_into_older_canonical_db_memory() {
    // Pins: two same-scope entities with near-identical embeddings AND a shared
    // active neighbour merge under an always-yes verifier; the newer entity is
    // superseded into the older canonical one, whose validity stays open.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let base = fixed_instant();

    // Canonical (older) and duplicate (newer) entities with near-identical vectors.
    let canonical = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "checkout service",
        near_identical_embedding(0.0),
        base,
    )
    .await;
    let duplicate = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "the checkout service",
        near_identical_embedding(0.02),
        base + Duration::seconds(1),
    )
    .await;
    // A neighbour both entities point at satisfies the structural gate.
    let neighbor = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "payments platform",
        near_identical_embedding(5.0),
        base,
    )
    .await;
    seed_edge(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        canonical,
        neighbor,
        base,
    )
    .await;
    let duplicate_edge = seed_edge(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        duplicate,
        neighbor,
        base,
    )
    .await;

    let stats = resolve_entity_duplicates(
        pool,
        &tenant_id,
        &AlwaysMergeVerifier,
        base - Duration::days(1),
        base + Duration::hours(1),
        &EntityResolutionOptions::default(),
    )
    .await
    .expect("resolve entity duplicates");

    assert_eq!(
        stats.pairs_adjudicated, 1,
        "one gated pair reached the verifier"
    );
    assert_eq!(stats.entities_merged, 1, "the verifier accepted one merge");

    assert_active(pool, canonical, true).await;
    assert_active(pool, duplicate, false).await;
    // Supersession records the older entity as the canonical replacement.
    assert!(
        supersedes_edge_exists(pool, canonical, duplicate).await,
        "a SUPERSEDES edge from the canonical to the duplicate must exist"
    );
    // Node supersession closes the superseded entity's incident edges in-tx.
    assert!(
        edge_closed(pool, duplicate_edge).await,
        "the duplicate's incident edge must be closed by supersession"
    );
}

#[tokio::test]
async fn near_duplicate_entities_without_shared_neighbor_never_reach_verifier_db_memory() {
    // Pins: near-identical embeddings alone do not merge; a pair whose endpoints
    // share no active neighbour is filtered before the verifier is consulted.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let base = fixed_instant();

    let first = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "checkout service",
        near_identical_embedding(0.0),
        base,
    )
    .await;
    let second = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "the checkout service",
        near_identical_embedding(0.02),
        base + Duration::seconds(1),
    )
    .await;
    // Each entity has a neighbour, but not the same one: no shared neighbour.
    let neighbor_a = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "team a",
        near_identical_embedding(5.0),
        base,
    )
    .await;
    let neighbor_b = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "team b",
        near_identical_embedding(6.0),
        base,
    )
    .await;
    seed_edge(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        first,
        neighbor_a,
        base,
    )
    .await;
    seed_edge(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        second,
        neighbor_b,
        base,
    )
    .await;

    let stats = resolve_entity_duplicates(
        pool,
        &tenant_id,
        &NeverCalledVerifier,
        base - Duration::days(1),
        base + Duration::hours(1),
        &EntityResolutionOptions::default(),
    )
    .await
    .expect("resolve entity duplicates without a shared neighbour");

    assert_eq!(
        stats.pairs_adjudicated, 0,
        "no pair should reach the verifier"
    );
    assert_eq!(stats.entities_merged, 0);
    assert_active(pool, first, true).await;
    assert_active(pool, second, true).await;
}

#[tokio::test]
async fn cross_scope_near_duplicate_entities_are_never_proposed_db_memory() {
    // Pins: blocking only pairs entities in the same (storage_partition, contact)
    // scope; a tenant-scoped and a contact-scoped near-duplicate sharing a
    // neighbour are never proposed, so the verifier is never called.
    let Some(test_db) = configured_test_db().await else {
        return;
    };
    let pool = test_db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
    let contact_id = ContactId(Uuid::now_v7());
    let base = fixed_instant();

    let tenant_entity = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "checkout service",
        near_identical_embedding(0.0),
        base,
    )
    .await;
    let contact_entity = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        Some(contact_id),
        "the checkout service",
        near_identical_embedding(0.02),
        base + Duration::seconds(1),
    )
    .await;
    // A shared neighbour would satisfy the structural gate; only the scope
    // boundary keeps this pair from ever being proposed.
    let neighbor = seed_entity(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        "payments platform",
        near_identical_embedding(5.0),
        base,
    )
    .await;
    seed_edge(
        pool,
        &storage_partition_id,
        tenant_id,
        None,
        tenant_entity,
        neighbor,
        base,
    )
    .await;
    seed_edge(
        pool,
        &storage_partition_id,
        tenant_id,
        Some(contact_id),
        contact_entity,
        neighbor,
        base,
    )
    .await;

    let stats = resolve_entity_duplicates(
        pool,
        &tenant_id,
        &NeverCalledVerifier,
        base - Duration::days(1),
        base + Duration::hours(1),
        &EntityResolutionOptions::default(),
    )
    .await
    .expect("resolve entity duplicates across scopes");

    assert_eq!(
        stats.pairs_adjudicated, 0,
        "no cross-scope pair should be proposed"
    );
    assert_eq!(stats.entities_merged, 0);
    assert_active(pool, tenant_entity, true).await;
    assert_active(pool, contact_entity, true).await;
}

fn fixed_instant() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 7, 0, 0, 0)
        .single()
        .expect("fixed timestamp")
}

/// Builds a 1024-dim vector pointing almost entirely along axis 0, with a small
/// `perturbation` on axis 1 so two seeds stay within cosine distance 0.15 while
/// a large `perturbation` yields an unrelated direction for neighbour nodes.
fn near_identical_embedding(perturbation: f32) -> Vec<f32> {
    let mut vector = vec![0.0_f32; 1024];
    vector[0] = 1.0;
    vector[1] = perturbation;
    vector
}

fn embedding_literal(vector: &[f32]) -> String {
    let mut literal = String::from("[");
    for (index, value) in vector.iter().enumerate() {
        if index > 0 {
            literal.push(',');
        }
        literal.push_str(&value.to_string());
    }
    literal.push(']');
    literal
}

#[allow(clippy::too_many_arguments)]
async fn seed_entity(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    name: &str,
    embedding: Vec<f32>,
    valid_from: DateTime<Utc>,
) -> Uuid {
    let uid = Uuid::now_v7();
    let user_id = contact_id.map(|contact| contact.0.to_string());
    let contact_uuid = contact_id.map(|contact| contact.0);
    sqlx::query(
        r#"
        INSERT INTO moa.node_index
            (uid, label, storage_partition_id, tenant_id, user_id, contact_id, name,
             pii_class, confidence, valid_from, last_accessed_at, properties_summary)
        VALUES ($1, 'Entity', $2, $3, $4, $5, $6, 'none', 0.9, $7, $7, '{}'::jsonb)
        "#,
    )
    .bind(uid)
    .bind(storage_partition_id.as_str())
    .bind(tenant_id.0)
    .bind(user_id.as_deref())
    .bind(contact_uuid)
    .bind(name)
    .bind(valid_from)
    .execute(pool)
    .await
    .expect("seed entity node");

    sqlx::query(
        r#"
        INSERT INTO moa.embeddings
            (uid, storage_partition_id, user_id, label, pii_class, embedding,
             embedding_model, embedding_model_version)
        VALUES ($1, $2, $3, 'Entity', 'none', $4::public.halfvec, 'test-embed', 1)
        "#,
    )
    .bind(uid)
    .bind(storage_partition_id.as_str())
    .bind(user_id.as_deref())
    .bind(embedding_literal(&embedding))
    .execute(pool)
    .await
    .expect("seed entity embedding");
    uid
}

#[allow(clippy::too_many_arguments)]
async fn seed_edge(
    pool: &PgPool,
    storage_partition_id: &StoragePartitionId,
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    start_uid: Uuid,
    end_uid: Uuid,
    valid_from: DateTime<Utc>,
) -> Uuid {
    let uid = Uuid::now_v7();
    let user_id = contact_id.map(|contact| contact.0.to_string());
    let contact_uuid = contact_id.map(|contact| contact.0);
    sqlx::query(
        r#"
        INSERT INTO moa.edge_index
            (uid, label, start_uid, end_uid, storage_partition_id, user_id, tenant_id,
             contact_id, properties, valid_from)
        VALUES ($1, 'RELATES_TO', $2, $3, $4, $5, $6, $7, '{}'::jsonb, $8)
        "#,
    )
    .bind(uid)
    .bind(start_uid)
    .bind(end_uid)
    .bind(storage_partition_id.as_str())
    .bind(user_id.as_deref())
    .bind(tenant_id.0)
    .bind(contact_uuid)
    .bind(valid_from)
    .execute(pool)
    .await
    .expect("seed edge");
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

async fn supersedes_edge_exists(pool: &PgPool, replacement: Uuid, old: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1 FROM moa.edge_index
            WHERE label = 'SUPERSEDES'
              AND start_uid = $1
              AND end_uid = $2
        )
        "#,
    )
    .bind(replacement)
    .bind(old)
    .fetch_one(pool)
    .await
    .expect("read supersedes edge")
}

async fn edge_closed(pool: &PgPool, uid: Uuid) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT valid_to IS NOT NULL FROM moa.edge_index WHERE uid = $1")
        .bind(uid)
        .fetch_one(pool)
        .await
        .expect("read edge validity")
}
