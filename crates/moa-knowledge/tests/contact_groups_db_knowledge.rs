//! DB integration coverage for derived contact-group memberships.

use chrono::{DateTime, Utc};
use moa_contacts::{
    domain::{hash_contact_point_from_env, normalize_contact_point},
    repository::resolve_verified_contact_ids,
};
use moa_core::{ContactId, ContactPointKind, StoragePartitionId, TenantId};
use moa_knowledge::{
    contact_groups::{
        contact_group_member_contact_points,
        derive_contact_groups_from_object_with_resolved_members,
    },
    domain::{KnowledgeObject, ObjectStatus},
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
};
use moa_memory_types::ScopeContext;
use moa_test_support::postgres;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const TEST_HASH_KEY_HEX: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

fn repository(db: &postgres::TestDb, tenant_id: TenantId) -> PostgresKnowledgeRepository {
    PostgresKnowledgeRepository::scoped_for_app_role(
        db.store().pool().clone(),
        ScopeContext::tenant(tenant_id),
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
    let resolved_contact_ids =
        resolve_verified_contact_ids(pool, tenant_id, &hash_key_env, &member_points)
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
