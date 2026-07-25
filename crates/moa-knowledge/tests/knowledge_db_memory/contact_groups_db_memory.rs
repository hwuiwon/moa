//! DB integration coverage for derived contact-group memberships.

use chrono::{DateTime, Utc};
use moa_contacts::{
    domain::{hash_contact_point_from_env, normalize_contact_point},
    repository::resolve_verified_contact_ids,
};
use moa_core::types::memory::RlsContext;
use moa_core::{
    types::contact::ContactId, types::contact::ContactPointKind,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use moa_knowledge::{
    contact_groups::{
        contact_group_member_contact_points,
        derive_contact_groups_from_object_with_resolved_members,
    },
    domain::{ContactGroup, KnowledgeObject, ObjectStatus},
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
};
use moa_test_support::postgres;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const TEST_HASH_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

fn repository(db: &postgres::TestDb, tenant_id: TenantId) -> PostgresKnowledgeRepository {
    PostgresKnowledgeRepository::scoped_for_app_role(
        db.store().pool().clone(),
        RlsContext::tenant(tenant_id),
    )
}

fn object(tenant_id: TenantId, connection_uid: Uuid) -> KnowledgeObject {
    KnowledgeObject {
        object_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid,
        object_type: "crm_account".to_string(),
        source_id: "crm-account-acct1".to_string(),
        parent_source_id: None,
        source_uri: None,
        title: Some("Acme".to_string()),
        change_token: Some("etag-acct1".to_string()),
        metadata: json!({
            "crm": {
                "account": {
                    "id": "acct1",
                    "name": "Acme"
                },
                "members": [
                    { "email": "member-a@example.invalid" },
                    { "email": "member-b@example.invalid" }
                ]
            }
        }),
        status: ObjectStatus::Active,
        source_updated_at: Some(Utc::now()),
        deleted_at: None,
    }
}

/// Builds a CRM-account object whose derived group carries a single member.
///
/// The `connection_uid` drives the derived `group_key`, so reusing the same
/// value across tenants yields an identical group_key (only the tenant-scoped
/// group_uid differs).
fn object_with_member(
    tenant_id: TenantId,
    connection_uid: Uuid,
    member_email: &str,
) -> KnowledgeObject {
    KnowledgeObject {
        object_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid,
        object_type: "crm_account".to_string(),
        source_id: "crm-account-acct1".to_string(),
        parent_source_id: None,
        source_uri: None,
        title: Some("Acme".to_string()),
        change_token: Some("etag-acct1".to_string()),
        metadata: json!({
            "crm": {
                "account": { "id": "acct1", "name": "Acme" },
                "members": [ { "email": member_email } ]
            }
        }),
        status: ObjectStatus::Active,
        source_updated_at: Some(Utc::now()),
        deleted_at: None,
    }
}

#[tokio::test]
async fn contact_group_sync_resolves_verified_members_and_deactivates_absent_members_db_knowledge()
{
    // Pins: derived contact-group sync persists only verified contacts and exposes active targets.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let pool = db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let repo = repository(&db, tenant_id);
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let hash_key_env = format!(
        "MOA_TEST_CONTACT_POINT_HASH_KEY_{}",
        Uuid::now_v7().simple()
    );
    // SAFETY: this test writes a unique process env key that no other test reads.
    unsafe {
        std::env::set_var(&hash_key_env, TEST_HASH_KEY_HEX);
    }

    let verified_contact_id = insert_contact_with_point(
        pool,
        tenant_id,
        &storage_partition_id,
        &hash_key_env,
        ContactPointKind::Email,
        "member-a@example.invalid",
        true,
    )
    .await;
    let unverified_contact_id = insert_contact_with_point(
        pool,
        tenant_id,
        &storage_partition_id,
        &hash_key_env,
        ContactPointKind::Email,
        "member-b@example.invalid",
        false,
    )
    .await;
    let object = object(tenant_id, Uuid::now_v7());
    let member_points = contact_group_member_contact_points(&object);
    assert_eq!(
        member_points.len(),
        2,
        "setup should derive both source member identities"
    );
    // `insert_contact_with_point` hashes via the env-named key; the resolver takes
    // the hex key directly, so pass the same key material explicitly here.
    let resolved_contact_ids =
        resolve_verified_contact_ids(pool, tenant_id, TEST_HASH_KEY_HEX, &member_points)
            .await
            .expect("resolver should return verified contacts");
    assert_eq!(
        resolved_contact_ids,
        vec![verified_contact_id],
        "only the verified contact point should resolve"
    );

    let delta =
        derive_contact_groups_from_object_with_resolved_members(&object, &resolved_contact_ids);
    assert_eq!(delta.groups.len(), 1, "one CRM account group should derive");
    assert_eq!(
        delta.memberships.len(),
        1,
        "only one resolved membership should derive"
    );
    let group = delta
        .groups
        .first()
        .expect("derived delta should include a group")
        .clone();
    repo.upsert_contact_group(group.clone())
        .await
        .expect("upsert derived contact group");
    repo.replace_contact_group_memberships(group.group_uid, delta.memberships.clone())
        .await
        .expect("persist derived memberships");
    assert_eq!(
        membership_counts(pool, group.group_uid).await,
        (1, 1),
        "first sync should create one active membership row"
    );
    assert_eq!(
        active_membership_count_for_contact(pool, group.group_uid, unverified_contact_id).await,
        0,
        "unverified contact should not become an active member"
    );
    let target = repo
        .contact_group_targets(tenant_id, &group.group_key)
        .await
        .expect("load contact group targets")
        .expect("group should resolve through the repository targeting API");
    assert_eq!(target.group.group_uid, group.group_uid);
    assert_eq!(
        target.group.group_key,
        format!("crm:{}:account:acct1", object.connection_uid)
    );
    assert_eq!(target.group.display_name, "Acme");
    assert_eq!(
        target
            .members
            .iter()
            .map(|member| member.contact_id)
            .collect::<Vec<_>>(),
        vec![verified_contact_id],
        "targeting API should expose only active verified members"
    );
    assert_eq!(target.members[0].evidence, vec![object.object_uid]);
    assert_eq!(
        target
            .active_graph_memberships
            .iter()
            .map(|membership| membership.contact_id)
            .collect::<Vec<_>>(),
        vec![verified_contact_id],
        "active graph membership projection should be derived from active SQL rows"
    );
    assert_eq!(target.active_graph_memberships[0].edge_label, "MEMBER_OF");
    assert_eq!(
        target.active_graph_memberships[0].evidence,
        vec![object.object_uid]
    );
    assert!(
        !target.group.metadata.to_string().contains('@'),
        "target group metadata should not expose member contact points"
    );
    assert!(
        !target.members[0].metadata.to_string().contains('@'),
        "target member metadata should not expose member contact points"
    );

    repo.upsert_contact_group(group.clone())
        .await
        .expect("repeat group upsert");
    repo.replace_contact_group_memberships(group.group_uid, delta.memberships)
        .await
        .expect("repeat membership sync");
    assert_eq!(
        membership_counts(pool, group.group_uid).await,
        (1, 1),
        "repeating the same sync must not add inactive churn"
    );

    let empty_delta = derive_contact_groups_from_object_with_resolved_members(&object, &[]);
    repo.replace_contact_group_memberships(group.group_uid, empty_delta.memberships)
        .await
        .expect("sync without resolved members");
    assert_eq!(
        membership_counts(pool, group.group_uid).await,
        (1, 0),
        "missing resolved members should deactivate the existing active row"
    );
    let target_after_removal = repo
        .contact_group_targets(tenant_id, &group.group_key)
        .await
        .expect("load contact group targets after removal")
        .expect("group should remain targetable after members are removed");
    assert_eq!(
        target_after_removal.members,
        Vec::new(),
        "targeting projection should not return inactive membership rows"
    );
    assert_eq!(
        target_after_removal.active_graph_memberships,
        Vec::new(),
        "active graph membership projection should drop removed members"
    );
    assert!(
        !target_after_removal
            .active_graph_memberships
            .iter()
            .any(|membership| membership.contact_id == verified_contact_id),
        "removed member must not remain in the active MEMBER_OF projection"
    );

    let group_row = sqlx::query_as::<_, (Uuid, String, String, Value)>(
        r#"
        SELECT group_uid, normalized_name, display_name, metadata
        FROM moa.knowledge_contact_groups
        WHERE tenant_id = $1
          AND normalized_name = $2
          AND display_name = $3
        "#,
    )
    .bind(tenant_id.0)
    .bind(&group.group_key)
    .bind("Acme")
    .fetch_one(pool)
    .await
    .expect("group should be targetable by tenant, group key, and name");
    assert_eq!(group_row.0, group.group_uid, "group UID should be stable");
    assert_eq!(group_row.1, group.group_key);
    assert_eq!(group_row.2, "Acme");
    assert!(
        !group_row.3.to_string().contains('@'),
        "persisted group metadata should not expose member contact points"
    );
}

#[tokio::test]
async fn contact_group_targets_isolate_members_across_tenants_for_same_group_key_db_knowledge() {
    // Pins: two tenants whose derived groups share an identical group_key cannot
    // read each other's group members through contact_group_targets; RLS scopes
    // the targeting API to the connection's own tenant.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let pool = db.store().pool();
    let tenant_a = TenantId::from(Uuid::now_v7());
    let tenant_b = TenantId::from(Uuid::now_v7());
    let repo_a = repository(&db, tenant_a);
    let repo_b = repository(&db, tenant_b);
    // A shared connection_uid makes the derived group_key identical across
    // tenants, so only tenant_id distinguishes the two groups and their members.
    let shared_connection_uid = Uuid::now_v7();
    let hash_key_env = format!(
        "MOA_TEST_CONTACT_POINT_HASH_KEY_{}",
        Uuid::now_v7().simple()
    );
    // SAFETY: this test writes a unique process env key that no other test reads.
    unsafe {
        std::env::set_var(&hash_key_env, TEST_HASH_KEY_HEX);
    }

    let member_a_email = "tenant-a-member@example.invalid";
    let member_b_email = "tenant-b-member@example.invalid";
    let (group_uid_a, group_key_a, contact_a) = persist_tenant_group(
        pool,
        &repo_a,
        tenant_a,
        shared_connection_uid,
        &hash_key_env,
        member_a_email,
    )
    .await;
    let (group_uid_b, group_key_b, contact_b) = persist_tenant_group(
        pool,
        &repo_b,
        tenant_b,
        shared_connection_uid,
        &hash_key_env,
        member_b_email,
    )
    .await;

    assert_eq!(
        group_key_a, group_key_b,
        "both tenants should derive an identical group_key from the shared connection + account"
    );
    assert_ne!(
        group_uid_a, group_uid_b,
        "group_uid must remain tenant-scoped even when the group_key matches"
    );

    // Each tenant sees only its own verified member through the targeting API.
    let target_a = repo_a
        .contact_group_targets(tenant_a, &group_key_a)
        .await
        .expect("load tenant A targets")
        .expect("tenant A group should resolve");
    assert_eq!(
        target_a
            .members
            .iter()
            .map(|member| member.contact_id)
            .collect::<Vec<_>>(),
        vec![contact_a],
        "tenant A should see only its own member"
    );
    assert!(
        !target_a
            .members
            .iter()
            .any(|member| member.contact_id == contact_b),
        "tenant A must not see tenant B's member"
    );

    let target_b = repo_b
        .contact_group_targets(tenant_b, &group_key_b)
        .await
        .expect("load tenant B targets")
        .expect("tenant B group should resolve");
    assert_eq!(
        target_b
            .members
            .iter()
            .map(|member| member.contact_id)
            .collect::<Vec<_>>(),
        vec![contact_b],
        "tenant B should see only its own member"
    );
    assert!(
        !target_b
            .members
            .iter()
            .any(|member| member.contact_id == contact_a),
        "tenant B must not see tenant A's member"
    );

    // Crux of the RLS proof: querying the OTHER tenant's group (correct tenant_id
    // and group_key) from this scope must resolve to nothing, because RLS hides
    // the other tenant's rows from this connection.
    assert!(
        repo_b
            .contact_group_targets(tenant_a, &group_key_a)
            .await
            .expect("tenant B scope query for tenant A group")
            .is_none(),
        "tenant B scope must not resolve tenant A's derived group even with its tenant_id + group_key"
    );
    assert!(
        repo_a
            .contact_group_targets(tenant_b, &group_key_b)
            .await
            .expect("tenant A scope query for tenant B group")
            .is_none(),
        "tenant A scope must not resolve tenant B's derived group even with its tenant_id + group_key"
    );
}

/// Persists a single-member derived contact group for one tenant.
///
/// Inserts a verified contact for `member_email`, derives the CRM-account group
/// from a shared-connection object, and writes the group plus its memberships
/// through the tenant-scoped repository. Returns the derived
/// `(group_uid, group_key, verified_contact_id)`.
async fn persist_tenant_group(
    pool: &PgPool,
    repo: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    connection_uid: Uuid,
    hash_key_env: &str,
    member_email: &str,
) -> (Uuid, String, ContactId) {
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let contact_id = insert_contact_with_point(
        pool,
        tenant_id,
        &storage_partition_id,
        hash_key_env,
        ContactPointKind::Email,
        member_email,
        true,
    )
    .await;
    let object = object_with_member(tenant_id, connection_uid, member_email);
    let member_points = contact_group_member_contact_points(&object);
    // `insert_contact_with_point` hashes via the env-named key; the resolver takes
    // the hex key directly, so pass the same key material explicitly here.
    let resolved = resolve_verified_contact_ids(pool, tenant_id, TEST_HASH_KEY_HEX, &member_points)
        .await
        .expect("resolve verified contacts");
    assert_eq!(
        resolved,
        vec![contact_id],
        "only the tenant's own verified member should resolve"
    );
    let delta = derive_contact_groups_from_object_with_resolved_members(&object, &resolved);
    let group = delta
        .groups
        .first()
        .expect("derived delta should include a group")
        .clone();
    let group_key = group.group_key.clone();
    repo.upsert_contact_group(group.clone())
        .await
        .expect("upsert derived contact group");
    repo.replace_contact_group_memberships(group.group_uid, delta.memberships)
        .await
        .expect("persist derived memberships");
    (group.group_uid, group_key, contact_id)
}

async fn insert_contact_with_point(
    pool: &PgPool,
    tenant_id: TenantId,
    storage_partition_id: &str,
    hash_key_env: &str,
    kind: ContactPointKind,
    value: &str,
    verified: bool,
) -> ContactId {
    let contact_id = ContactId::new();
    let state = if verified { "verified" } else { "unverified" };
    sqlx::query(
        r#"
        INSERT INTO contacts (id, tenant_id, storage_partition_id, contact_id, state)
        VALUES ($1, $2, $3, $1, $4)
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .bind(state)
    .execute(pool)
    .await
    .expect("insert contact fixture");

    let normalized = normalize_contact_point(kind, value).expect("normalize contact point fixture");
    let normalized_hash = hash_contact_point_from_env(tenant_id, kind, &normalized, hash_key_env)
        .expect("hash contact point fixture");
    let verified_at: Option<DateTime<Utc>> = verified.then(Utc::now);
    sqlx::query(
        r#"
        INSERT INTO contact_points (
            id, contact_id, tenant_id, storage_partition_id, kind,
            normalized_hash, display_value, verified, verified_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, NULL, $7, $8)
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(storage_partition_id)
    .bind(kind.as_str())
    .bind(normalized_hash)
    .bind(verified)
    .bind(verified_at)
    .execute(pool)
    .await
    .expect("insert contact point fixture");
    contact_id
}

async fn membership_counts(pool: &PgPool, group_uid: Uuid) -> (i64, i64) {
    sqlx::query_as::<_, (i64, i64)>(
        r#"
        SELECT COUNT(*)::BIGINT,
               COUNT(*) FILTER (WHERE active)::BIGINT
        FROM moa.knowledge_contact_group_memberships
        WHERE group_id = $1
        "#,
    )
    .bind(group_uid)
    .fetch_one(pool)
    .await
    .expect("load membership counts")
}

async fn active_membership_count_for_contact(
    pool: &PgPool,
    group_uid: Uuid,
    contact_id: ContactId,
) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT COUNT(*)::BIGINT
        FROM moa.knowledge_contact_group_memberships
        WHERE group_id = $1
          AND contact_id = $2
          AND active = TRUE
        "#,
    )
    .bind(group_uid)
    .bind(contact_id.0)
    .fetch_one(pool)
    .await
    .expect("load contact membership count")
}

/// Builds one derived contact group shaped exactly like ingestion materialization output.
fn derived_group(tenant_id: TenantId, connection_uid: Uuid, source_group_id: &str) -> ContactGroup {
    let group_key = format!("merge:{connection_uid}:account:{source_group_id}");
    ContactGroup {
        group_uid: Uuid::now_v7(),
        tenant_id,
        group_key: group_key.clone(),
        display_name: "Acme".to_string(),
        metadata: json!({ "source_provider": "merge" }),
    }
}

/// Builds a tenant-scoped repository that owns its pool clone for spawned tasks.
fn owned_repository(pool: &PgPool, tenant_id: TenantId) -> PostgresKnowledgeRepository {
    PostgresKnowledgeRepository::scoped_for_app_role(pool.clone(), RlsContext::tenant(tenant_id))
}

#[tokio::test]
async fn concurrent_identical_group_upserts_converge_without_duplicate_key_db_knowledge() {
    // Pins: contact groups are cross-object, so page-concurrent records derive
    // the byte-identical group (same group_uid AND same
    // knowledge_contact_groups_name_uniq key) and race their upserts. Every
    // writer must succeed and the table must converge to one row; without
    // per-group serialization a second writer that passes the group_uid
    // arbiter pre-check concurrently raises 23505 on the name index, which
    // never takes the DO UPDATE path.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let pool = db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let group = derived_group(tenant_id, Uuid::now_v7(), "acct-race");

    const WRITERS: usize = 8;
    const ROUNDS: usize = 5;
    for _ in 0..ROUNDS {
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(WRITERS));
        let mut handles = Vec::with_capacity(WRITERS);
        for _ in 0..WRITERS {
            let barrier = barrier.clone();
            let repository = owned_repository(pool, tenant_id);
            let group = group.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                repository.upsert_contact_group(group).await
            }));
        }
        for handle in handles {
            handle
                .await
                .expect("upsert task should not panic")
                .expect("concurrent identical group upsert should succeed");
        }
    }

    let row_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM moa.knowledge_contact_groups WHERE group_uid = $1",
    )
    .bind(group.group_uid)
    .fetch_one(pool)
    .await
    .expect("count group rows");
    assert_eq!(row_count, 1, "identical upserts must converge to one row");
}

#[tokio::test]
async fn identical_group_upsert_waits_for_the_group_advisory_lock_db_knowledge() {
    // Pins: the repository serializes same-group writers by taking
    // pg_advisory_xact_lock on the group identity before writing, so a
    // same-group upsert cannot reach its INSERT while another transaction
    // holds that group's lock. Removing the production lock lets the upsert
    // complete while the lock is held, which fails this test.
    let db = postgres::bootstrap_test_db()
        .await
        .expect("bootstrap isolated knowledge DB");
    let pool = db.store().pool();
    let tenant_id = TenantId::from(Uuid::now_v7());
    let group = derived_group(tenant_id, Uuid::now_v7(), "acct-lock");

    let mut lock_tx = pool.begin().await.expect("open lock transaction");
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('knowledge_contact_group:' || $1::text, 0))",
    )
    .bind(group.group_uid)
    .execute(&mut *lock_tx)
    .await
    .expect("hold the group advisory lock");

    let repository = owned_repository(pool, tenant_id);
    let contended_group = group.clone();
    let mut handle =
        tokio::spawn(async move { repository.upsert_contact_group(contended_group).await });

    let blocked = tokio::time::timeout(std::time::Duration::from_millis(300), &mut handle).await;
    assert!(
        blocked.is_err(),
        "same-group upsert must wait for the held group advisory lock"
    );

    lock_tx.rollback().await.expect("release the group lock");
    handle
        .await
        .expect("upsert task should not panic")
        .expect("upsert should complete once the group lock is released");

    let row_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM moa.knowledge_contact_groups WHERE group_uid = $1",
    )
    .bind(group.group_uid)
    .fetch_one(pool)
    .await
    .expect("count group rows");
    assert_eq!(row_count, 1, "released upsert must persist exactly one row");
}
