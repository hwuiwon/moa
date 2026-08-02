//! DB-backed SCIM group query batching and membership-bound coverage.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity, IdentityType};
use moa_core::types::identifiers::TenantId;
use moa_orchestrator::services::scim::groups::{
    ListQuery, create_group, delete_group, list_groups, patch_group, put_group,
};
use moa_orchestrator::services::scim::patch::{Operation, PatchOp};
use moa_orchestrator::services::scim::schema::{
    SCHEMA_GROUP, SCHEMA_PATCH, ScimError, ScimGroup, ScimGroupMember,
};
use moa_orchestrator::services::scim::{ScimResponseError, ScimState};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

use super::fga_mock::spawn_fga_mock;

const EXPECTED_GROUP_MEMBER_LIMIT: usize = 4096;

#[tokio::test]
async fn scim_group_list_empty_page_db() -> Result<()> {
    // Pins: listing a tenant with no groups returns an exact empty SCIM page.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let state = scim_state(pool, tenant_id).await?;

    let page = list_page(state, 200).await?;

    assert_eq!(page.total_results, 0);
    assert_eq!(page.items_per_page, 0);
    assert_eq!(page.start_index, 1);
    assert_eq!(page.resources.len(), 0);
    Ok(())
}

#[tokio::test]
async fn scim_group_list_keeps_batched_members_with_their_group_db() -> Result<()> {
    // Pins: one page-scoped member read groups multiple groups without cross-assignment
    // and preserves deterministic email ordering inside each group.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let alpha_user = insert_user(&pool, tenant_id, "alpha@example.com").await?;
    let zeta_user = insert_user(&pool, tenant_id, "zeta@example.com").await?;
    let engineering = insert_group(&pool, tenant_id, "engineering").await?;
    let support = insert_group(&pool, tenant_id, "support").await?;
    insert_memberships(
        &pool,
        &[
            (engineering, zeta_user),
            (engineering, alpha_user),
            (support, zeta_user),
        ],
    )
    .await?;

    let page = list_page(scim_state(pool, tenant_id).await?, 200).await?;
    let engineering_members = member_displays(&page.resources, "engineering");
    let support_members = member_displays(&page.resources, "support");

    assert_eq!(page.total_results, 2);
    assert_eq!(page.items_per_page, 2);
    assert_eq!(
        engineering_members,
        vec!["alpha@example.com", "zeta@example.com"]
    );
    assert_eq!(support_members, vec!["zeta@example.com"]);
    Ok(())
}

#[tokio::test]
async fn scim_group_list_returns_all_members_at_protocol_page_cap_db() -> Result<()> {
    // Pins: the protocol's maximum 200-group page is fully hydrated by the batched
    // member query rather than dropping groups or members at the boundary.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let member_id = insert_user(&pool, tenant_id, "page-member@example.com").await?;
    let group_ids = (0..200).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
    let display_names = (0..200)
        .map(|index| format!("page-group-{index:03}"))
        .collect::<Vec<_>>();
    insert_group_page(&pool, tenant_id, &group_ids, &display_names).await?;
    let memberships = group_ids
        .iter()
        .copied()
        .map(|group_id| (group_id, member_id))
        .collect::<Vec<_>>();
    insert_memberships(&pool, &memberships).await?;

    let page = list_page(scim_state(pool, tenant_id).await?, 200).await?;

    assert_eq!(page.total_results, 200);
    assert_eq!(page.items_per_page, 200);
    assert_eq!(page.resources.len(), 200);
    assert!(page.resources.iter().all(|group| {
        group.members.len() == 1
            && group.members[0].value == member_id.to_string()
            && group.members[0].display.as_deref() == Some("page-member@example.com")
    }));
    Ok(())
}

#[tokio::test]
async fn scim_group_create_deduplicates_repeated_member_rows_db() -> Result<()> {
    // Pins: repeated member references in one create request persist and return one
    // membership row under the table's set semantics.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let member_id = insert_user(&pool, tenant_id, "duplicate@example.com").await?;
    let state = scim_state(pool.clone(), tenant_id).await?;
    let member = scim_member(member_id);

    let (status, Json(group)) = create_group(
        State(state),
        scim_headers(),
        Json(scim_group(
            "duplicate-members",
            vec![member.clone(), member],
        )),
    )
    .await
    .map_err(|error| anyhow!("SCIM group create failed: {error:?}"))?;
    let group_id = parse_group_id(&group)?;
    let persisted_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scim_group_members WHERE group_id = $1")
            .bind(group_id)
            .fetch_one(&pool)
            .await?;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(group.members.len(), 1);
    assert_eq!(group.members[0].value, member_id.to_string());
    assert_eq!(persisted_count, 1);
    Ok(())
}

#[tokio::test]
async fn scim_group_put_exact_replay_is_a_database_noop_db() -> Result<()> {
    // Pins: replacing a group with its exact persisted state performs no group,
    // membership, authorization-outbox, signing-key, or security-event mutation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let member_id = insert_user(&pool, tenant_id, "replay@example.com").await?;
    let group_id = insert_group(&pool, tenant_id, "replay-group").await?;
    insert_memberships(&pool, &[(group_id, member_id)]).await?;
    let state = scim_state(pool.clone(), tenant_id).await?;
    let before = database_snapshot(&pool, group_id).await?;
    let signing_keys_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_signing_keys WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&pool)
            .await?;

    let _ = put_group(
        State(state),
        scim_headers(),
        Path(group_id),
        Json(scim_group("replay-group", vec![scim_member(member_id)])),
    )
    .await
    .map_err(|error| anyhow!("exact PUT replay failed: {error:?}"))?;

    assert_eq!(database_snapshot(&pool, group_id).await?, before);
    let signing_keys_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_signing_keys WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(signing_keys_after, signing_keys_before);
    Ok(())
}

#[tokio::test]
async fn scim_group_external_id_only_put_bumps_once_and_replay_is_a_noop_db() -> Result<()> {
    // Pins: externalId is real group metadata: changing only that field bumps the
    // version and emits one update audit, while replay preserves both exactly.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let member_id = insert_user(&pool, tenant_id, "external-id@example.com").await?;
    let group_id = insert_group(&pool, tenant_id, "external-id-group").await?;
    insert_memberships(&pool, &[(group_id, member_id)]).await?;
    let state = scim_state(pool.clone(), tenant_id).await?;
    let mut request = scim_group("external-id-group", vec![scim_member(member_id)]);
    request.external_id = Some("idp-group-42".to_string());

    let Json(updated) = put_group(
        State(state.clone()),
        scim_headers(),
        Path(group_id),
        Json(request.clone()),
    )
    .await
    .map_err(|error| anyhow!("external-id-only PUT failed: {error:?}"))?;
    assert_eq!(updated.external_id.as_deref(), Some("idp-group-42"));
    assert_eq!(
        updated
            .meta
            .as_ref()
            .expect("updated group should include metadata")
            .version,
        "W/\"2\""
    );
    let after_update = database_snapshot(&pool, group_id).await?;
    assert_eq!(after_update.external_id.as_deref(), Some("idp-group-42"));
    assert_eq!(after_update.version, 2);
    assert_eq!(after_update.security_event_count, 1);

    let Json(replayed) = put_group(State(state), scim_headers(), Path(group_id), Json(request))
        .await
        .map_err(|error| anyhow!("external-id PUT replay failed: {error:?}"))?;
    assert_eq!(
        replayed
            .meta
            .as_ref()
            .expect("replayed group should include metadata")
            .version,
        "W/\"2\""
    );
    assert_eq!(database_snapshot(&pool, group_id).await?, after_update);
    Ok(())
}

#[tokio::test]
async fn scim_group_patch_uses_set_algebra_and_replay_is_a_noop_db() -> Result<()> {
    // Pins: PATCH applies (current union additions) minus removals with removal
    // winning, emits only actual row changes, and an exact replay mutates nothing.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let alpha = insert_user(&pool, tenant_id, "patch-alpha@example.com").await?;
    let removed = insert_user(&pool, tenant_id, "patch-removed@example.com").await?;
    let added = insert_user(&pool, tenant_id, "patch-added@example.com").await?;
    let group_id = insert_group(&pool, tenant_id, "patch-set-group").await?;
    insert_memberships(&pool, &[(group_id, alpha), (group_id, removed)]).await?;
    let state = scim_state(pool.clone(), tenant_id).await?;
    let Json(group) = patch_group(
        State(state.clone()),
        scim_headers(),
        Path(group_id),
        Json(set_algebra_patch(removed, added)),
    )
    .await
    .map_err(|error| anyhow!("set-algebra PATCH failed: {error:?}"))?;
    assert_eq!(
        group
            .members
            .iter()
            .map(|member| member.value.as_str())
            .collect::<Vec<_>>(),
        [added.to_string(), alpha.to_string()]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    let after_change = database_snapshot(&pool, group_id).await?;
    assert_eq!(after_change.version, 2);
    assert_eq!(after_change.membership_count, 2);
    assert_eq!(after_change.outbox_count, 0);
    assert_eq!(after_change.security_event_count, 3);
    let signing_key_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_signing_keys WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(signing_key_count, 1);

    let _ = patch_group(
        State(state),
        scim_headers(),
        Path(group_id),
        Json(set_algebra_patch(removed, added)),
    )
    .await
    .map_err(|error| anyhow!("set-algebra PATCH replay failed: {error:?}"))?;
    assert_eq!(database_snapshot(&pool, group_id).await?, after_change);
    let signing_key_count_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM tenant_signing_keys WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&pool)
            .await?;
    assert_eq!(signing_key_count_after, signing_key_count);
    Ok(())
}

#[tokio::test]
async fn scim_group_role_rename_updates_retained_privileges_without_membership_churn_db()
-> Result<()> {
    // Pins: renaming a tenant role group revokes and grants retained-member
    // privileges without rewriting membership rows or mislabeling them as membership changes.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let alpha = insert_user(&pool, tenant_id, "role-alpha@example.com").await?;
    let beta = insert_user(&pool, tenant_id, "role-beta@example.com").await?;
    let admin_name = format!("tenant:{tenant_id}:admin");
    let operator_name = format!("tenant:{tenant_id}:operator");
    let ordinary_name = "ordinary-role-group";
    let state = scim_state(pool.clone(), tenant_id).await?;

    let (_, Json(created)) = create_group(
        State(state.clone()),
        scim_headers(),
        Json(scim_group(
            &admin_name,
            vec![scim_member(beta), scim_member(alpha)],
        )),
    )
    .await
    .map_err(|error| anyhow!("role group create failed: {error:?}"))?;
    let group_id = parse_group_id(&created)?;
    let memberships_before: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT user_id, added_at FROM scim_group_members WHERE group_id = $1 ORDER BY user_id",
    )
    .bind(group_id)
    .fetch_all(&pool)
    .await?;

    let Json(updated) = patch_group(
        State(state.clone()),
        scim_headers(),
        Path(group_id),
        Json(display_name_patch(&operator_name)),
    )
    .await
    .map_err(|error| anyhow!("role rename failed: {error:?}"))?;

    assert_eq!(updated.display_name, operator_name);
    let _ = patch_group(
        State(state.clone()),
        scim_headers(),
        Path(group_id),
        Json(display_name_patch(ordinary_name)),
    )
    .await
    .map_err(|error| anyhow!("operator-to-ordinary rename failed: {error:?}"))?;
    let Json(final_group) = patch_group(
        State(state),
        scim_headers(),
        Path(group_id),
        Json(display_name_patch(&admin_name)),
    )
    .await
    .map_err(|error| anyhow!("ordinary-to-admin rename failed: {error:?}"))?;
    assert_eq!(final_group.display_name, admin_name);
    let memberships_after: Vec<(Uuid, chrono::DateTime<chrono::Utc>)> = sqlx::query_as(
        "SELECT user_id, added_at FROM scim_group_members WHERE group_id = $1 ORDER BY user_id",
    )
    .bind(group_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(memberships_after, memberships_before);
    let tuples: Vec<(String, String, String, i64, String, Option<Uuid>)> = sqlx::query_as(
        r#"
        SELECT tuple_user, tuple_relation, op, generation, status, tenant_id
        FROM authz_outbox
        ORDER BY tuple_relation, tuple_user
        "#,
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(tuples.len(), 4);
    for user_id in [alpha, beta] {
        let user_wire = format!("operator:{user_id}");
        assert!(tuples.contains(&(
            user_wire.clone(),
            "admin".to_string(),
            "write".to_string(),
            3,
            "pending".to_string(),
            Some(tenant_id),
        )));
        assert!(tuples.contains(&(
            user_wire,
            "operator".to_string(),
            "delete".to_string(),
            2,
            "pending".to_string(),
            Some(tenant_id),
        )));
    }

    let group_target = format!("scim_group:{group_id}");
    let tenant_target = format!("tenant:{tenant_id}");
    let event_groups: Vec<(i32, i32, String, i64)> = sqlx::query_as(
        r#"
        SELECT class_uid, activity_id, target_resource_uid, COUNT(*)
        FROM security_events
        WHERE tenant_id = $1
        GROUP BY class_uid, activity_id, target_resource_uid
        ORDER BY class_uid, activity_id, target_resource_uid
        "#,
    )
    .bind(tenant_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        event_groups,
        vec![
            (3003, 1, group_target, 2),
            (3003, 1, tenant_target.clone(), 4),
            (3003, 2, tenant_target, 4),
            (3004, 1, format!("scim_group:{group_id}"), 1),
            (3004, 3, format!("scim_group:{group_id}"), 3),
        ]
    );
    let key_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT signing_key_id FROM security_events WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_all(&pool)
    .await?;
    assert_eq!(key_ids.len(), 1);
    Ok(())
}

#[tokio::test]
async fn scim_group_cross_tenant_member_rolls_back_group_tuple_key_and_audit_db() -> Result<()> {
    // Pins: set-wise user validation rejects an explicit cross-tenant member and
    // rolls back the group plus every transaction-coupled side effect.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let other_tenant = Uuid::new_v4();
    let local = insert_user(&pool, tenant_id, "local@example.com").await?;
    let foreign = insert_user(&pool, other_tenant, "foreign@example.com").await?;
    let state = scim_state(pool.clone(), tenant_id).await?;

    let error = create_group(
        State(state),
        scim_headers(),
        Json(scim_group(
            &format!("tenant:{tenant_id}:admin"),
            vec![scim_member(local), scim_member(foreign)],
        )),
    )
    .await
    .expect_err("cross-tenant member must fail group create");
    assert_invalid_value(error).await?;

    let residue: (i64, i64, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT COUNT(*) FROM scim_groups),
            (SELECT COUNT(*) FROM scim_group_members),
            (SELECT COUNT(*) FROM authz_outbox),
            (SELECT COUNT(*) FROM tenant_signing_keys WHERE tenant_id = $1),
            (SELECT COUNT(*) FROM security_events WHERE tenant_id = $1)
        "#,
    )
    .bind(tenant_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(residue, (0, 0, 0, 0, 0));
    Ok(())
}

#[tokio::test]
async fn scim_group_exact_member_cap_succeeds_and_final_cap_plus_one_rolls_back_db() -> Result<()> {
    // Pins: 4096 canonical members succeed in one bounded write, while a one-user
    // PATCH that would produce 4097 final members is rejected before any mutation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let member_ids = insert_users(&pool, tenant_id, EXPECTED_GROUP_MEMBER_LIMIT).await?;
    let state = scim_state(pool.clone(), tenant_id).await?;
    let (_, Json(created)) = create_group(
        State(state.clone()),
        scim_headers(),
        Json(scim_group(
            "exact-cap-group",
            member_ids.iter().copied().map(scim_member).collect(),
        )),
    )
    .await
    .map_err(|error| anyhow!("exact-cap group create failed: {error:?}"))?;
    let group_id = parse_group_id(&created)?;
    assert_eq!(created.members.len(), EXPECTED_GROUP_MEMBER_LIMIT);
    let after_exact_cap = database_snapshot(&pool, group_id).await?;
    assert_eq!(
        after_exact_cap.membership_count,
        EXPECTED_GROUP_MEMBER_LIMIT as i64
    );
    assert_eq!(after_exact_cap.outbox_count, 0);
    assert_eq!(
        after_exact_cap.security_event_count,
        (EXPECTED_GROUP_MEMBER_LIMIT + 1) as i64
    );

    let extra = insert_user(&pool, tenant_id, "exact-cap-extra@example.com").await?;
    assert_too_many(
        patch_group(
            State(state),
            scim_headers(),
            Path(group_id),
            Json(PatchOp {
                schemas: vec![SCHEMA_PATCH.to_string()],
                operations: vec![Operation {
                    op: "add".to_string(),
                    path: Some("members".to_string()),
                    value: Some(json!([scim_member(extra)])),
                }],
            }),
        )
        .await
        .expect_err("4097-member final group must be rejected"),
    )
    .await?;
    assert_eq!(database_snapshot(&pool, group_id).await?, after_exact_cap);
    Ok(())
}

#[tokio::test]
async fn scim_group_delete_batches_tuple_and_audit_state_atomically_db() -> Result<()> {
    // Pins: deleting a role group converges tuple state, cascades membership rows,
    // and emits exactly one removal plus one lifecycle event in the same transaction.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let member_id = insert_user(&pool, tenant_id, "delete-member@example.com").await?;
    let state = scim_state(pool.clone(), tenant_id).await?;
    let (_, Json(created)) = create_group(
        State(state.clone()),
        scim_headers(),
        Json(scim_group(
            &format!("tenant:{tenant_id}:admin"),
            vec![scim_member(member_id)],
        )),
    )
    .await
    .map_err(|error| anyhow!("delete fixture create failed: {error:?}"))?;
    let group_id = parse_group_id(&created)?;

    let status = delete_group(State(state), scim_headers(), Path(group_id))
        .await
        .map_err(|error| anyhow!("SCIM group delete failed: {error:?}"))?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let residue: (i64, i64, String, i64, i64, i64) = sqlx::query_as(
        r#"
        SELECT
            (SELECT COUNT(*) FROM scim_groups WHERE id = $1),
            (SELECT COUNT(*) FROM scim_group_members WHERE group_id = $1),
            (SELECT op FROM authz_outbox WHERE tuple_user = $2),
            (SELECT generation FROM authz_outbox WHERE tuple_user = $2),
            (SELECT COUNT(*) FROM tenant_signing_keys WHERE tenant_id = $3),
            (SELECT COUNT(*) FROM security_events WHERE tenant_id = $3)
        "#,
    )
    .bind(group_id)
    .bind(format!("operator:{member_id}"))
    .bind(tenant_id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(residue, (0, 0, "delete".to_string(), 2, 1, 4));
    Ok(())
}

#[tokio::test]
async fn scim_group_signing_failure_rolls_back_membership_and_version_db() -> Result<()> {
    // Pins: signing is inside the SCIM write transaction, so an invalid active key
    // rolls back the preceding set-based membership insert and group version bump.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let current = insert_user(&pool, tenant_id, "sign-current@example.com").await?;
    let added = insert_user(&pool, tenant_id, "sign-added@example.com").await?;
    let group_id = insert_group(&pool, tenant_id, "signing-rollback-group").await?;
    insert_memberships(&pool, &[(group_id, current)]).await?;
    sqlx::query(
        "INSERT INTO tenant_signing_keys (tenant_id, key_b64, active) VALUES ($1, $2, TRUE)",
    )
    .bind(tenant_id)
    .bind("not-valid-base64")
    .execute(&pool)
    .await?;
    let before = database_snapshot(&pool, group_id).await?;

    let error = patch_group(
        State(scim_state(pool.clone(), tenant_id).await?),
        scim_headers(),
        Path(group_id),
        Json(PatchOp {
            schemas: vec![SCHEMA_PATCH.to_string()],
            operations: vec![Operation {
                op: "add".to_string(),
                path: Some("members".to_string()),
                value: Some(json!([scim_member(added)])),
            }],
        }),
    )
    .await
    .expect_err("invalid signing key must fail the whole SCIM transaction");
    assert_eq!(
        error.into_response().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(database_snapshot(&pool, group_id).await?, before);
    Ok(())
}

#[tokio::test]
async fn scim_group_tenant_purge_fence_rolls_back_membership_group_and_tuple_state_db() -> Result<()>
{
    // Pins: the production authz-outbox tenant-purge fence rejects an ordinary SCIM role
    // write and rolls back membership plus the already-issued group update.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let current = insert_user(&pool, tenant_id, "fence-current@example.com").await?;
    let added = insert_user(&pool, tenant_id, "fence-added@example.com").await?;
    let group_id = insert_group(&pool, tenant_id, &format!("tenant:{tenant_id}:admin")).await?;
    insert_memberships(&pool, &[(group_id, current)]).await?;
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(format!("scim-fence-{tenant_id}"))
        .execute(&pool)
        .await?;
    let before = database_snapshot(&pool, group_id).await?;

    let error = patch_group(
        State(scim_state(pool.clone(), tenant_id).await?),
        scim_headers(),
        Path(group_id),
        Json(PatchOp {
            schemas: vec![SCHEMA_PATCH.to_string()],
            operations: vec![Operation {
                op: "add".to_string(),
                path: Some("members".to_string()),
                value: Some(json!([scim_member(added)])),
            }],
        }),
    )
    .await
    .expect_err("the tenant-purge fence must reject the SCIM tuple write");
    assert_eq!(
        error.into_response().status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(database_snapshot(&pool, group_id).await?, before);
    Ok(())
}

#[tokio::test]
async fn scim_group_writes_reject_over_cap_before_dml_db() -> Result<()> {
    // Pins: create, replace, and patch reject 4097 requested members without
    // changing groups, memberships, authorization outbox, or security audit rows.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_id = Uuid::new_v4();
    let group_id = insert_group(&pool, tenant_id, "bounded-group").await?;
    let state = scim_state(pool.clone(), tenant_id).await?;
    let members = vec![scim_member(Uuid::new_v4()); EXPECTED_GROUP_MEMBER_LIMIT + 1];
    let before = database_snapshot(&pool, group_id).await?;

    assert_too_many(
        create_group(
            State(state.clone()),
            scim_headers(),
            Json(scim_group("oversized-create", members.clone())),
        )
        .await
        .expect_err("over-cap create must fail"),
    )
    .await?;
    assert_too_many(
        put_group(
            State(state.clone()),
            scim_headers(),
            Path(group_id),
            Json(scim_group("oversized-replace", members.clone())),
        )
        .await
        .expect_err("over-cap replace must fail"),
    )
    .await?;
    assert_too_many(
        patch_group(
            State(state),
            scim_headers(),
            Path(group_id),
            Json(PatchOp {
                schemas: vec![SCHEMA_PATCH.to_string()],
                operations: vec![Operation {
                    op: "add".to_string(),
                    path: Some("members".to_string()),
                    value: Some(json!(members)),
                }],
            }),
        )
        .await
        .expect_err("over-cap patch must fail"),
    )
    .await?;

    assert_eq!(database_snapshot(&pool, group_id).await?, before);
    Ok(())
}

async fn list_page(
    state: ScimState,
    count: i64,
) -> Result<moa_orchestrator::services::scim::schema::ListResponse<ScimGroup>> {
    let Json(page) = list_groups(
        State(state),
        scim_headers(),
        Query(ListQuery {
            start_index: Some(1),
            count: Some(count),
            filter: None,
        }),
    )
    .await
    .map_err(|error| anyhow!("SCIM group list failed: {error:?}"))?;
    Ok(page)
}

fn member_displays<'a>(groups: &'a [ScimGroup], display_name: &str) -> Vec<&'a str> {
    groups
        .iter()
        .find(|group| group.display_name == display_name)
        .unwrap_or_else(|| panic!("group {display_name:?} should be in the page"))
        .members
        .iter()
        .map(|member| {
            member
                .display
                .as_deref()
                .expect("stored SCIM members should have an email display")
        })
        .collect()
}

fn scim_group(display_name: &str, members: Vec<ScimGroupMember>) -> ScimGroup {
    ScimGroup {
        schemas: vec![SCHEMA_GROUP.to_string()],
        id: None,
        external_id: None,
        display_name: display_name.to_string(),
        members,
        meta: None,
    }
}

fn scim_member(user_id: Uuid) -> ScimGroupMember {
    ScimGroupMember {
        value: user_id.to_string(),
        display: None,
    }
}

fn set_algebra_patch(removed: Uuid, added: Uuid) -> PatchOp {
    PatchOp {
        schemas: vec![SCHEMA_PATCH.to_string()],
        operations: vec![
            Operation {
                op: "add".to_string(),
                path: Some("members".to_string()),
                value: Some(json!([
                    scim_member(removed),
                    scim_member(added),
                    scim_member(added)
                ])),
            },
            Operation {
                op: "remove".to_string(),
                path: Some("members".to_string()),
                value: Some(json!([scim_member(removed), scim_member(removed)])),
            },
        ],
    }
}

fn display_name_patch(display_name: &str) -> PatchOp {
    PatchOp {
        schemas: vec![SCHEMA_PATCH.to_string()],
        operations: vec![Operation {
            op: "replace".to_string(),
            path: Some("displayName".to_string()),
            value: Some(json!(display_name)),
        }],
    }
}

fn parse_group_id(group: &ScimGroup) -> Result<Uuid> {
    let id = group
        .id
        .as_deref()
        .context("created group response should contain an id")?;
    Uuid::parse_str(id).context("created group id should be a UUID")
}

async fn assert_too_many(error: ScimResponseError) -> Result<()> {
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .context("read SCIM error response")?;
    let body: ScimError = serde_json::from_slice(&bytes).context("decode SCIM error response")?;
    assert_eq!(body.status, StatusCode::BAD_REQUEST.as_u16().to_string());
    assert_eq!(body.scim_type.as_deref(), Some("tooMany"));
    assert_eq!(
        body.detail,
        "group membership request exceeds the 4096 member limit"
    );
    Ok(())
}

async fn assert_invalid_value(error: ScimResponseError) -> Result<()> {
    let response = error.into_response();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .context("read invalid-value SCIM response")?;
    let body: ScimError =
        serde_json::from_slice(&bytes).context("decode invalid-value SCIM response")?;
    assert_eq!(body.status, StatusCode::BAD_REQUEST.as_u16().to_string());
    assert_eq!(body.scim_type.as_deref(), Some("invalidValue"));
    assert_eq!(body.detail, "group member user does not exist in tenant");
    Ok(())
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct DatabaseSnapshot {
    group_count: i64,
    membership_count: i64,
    outbox_count: i64,
    security_event_count: i64,
    external_id: Option<String>,
    display_name: String,
    version: i64,
}

async fn database_snapshot(pool: &PgPool, group_id: Uuid) -> Result<DatabaseSnapshot> {
    sqlx::query_as(
        r#"
        SELECT
            (SELECT COUNT(*) FROM scim_groups) AS group_count,
            (SELECT COUNT(*) FROM scim_group_members) AS membership_count,
            (SELECT COUNT(*) FROM authz_outbox) AS outbox_count,
            (SELECT COUNT(*) FROM security_events) AS security_event_count,
            external_id,
            display_name,
            version
        FROM scim_groups
        WHERE id = $1
        "#,
    )
    .bind(group_id)
    .fetch_one(pool)
    .await
    .context("load SCIM write database snapshot")
}

async fn insert_user(pool: &PgPool, tenant_id: Uuid, email: &str) -> Result<Uuid> {
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, tenant_id, email) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(tenant_id)
        .bind(email)
        .execute(pool)
        .await?;
    Ok(user_id)
}

async fn insert_users(pool: &PgPool, tenant_id: Uuid, count: usize) -> Result<Vec<Uuid>> {
    let user_ids = (0..count).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
    let emails = (0..count)
        .map(|index| format!("member-{index:04}@example.com"))
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        INSERT INTO users (id, tenant_id, email)
        SELECT input.user_id, $3, input.email
        FROM UNNEST($1::uuid[], $2::text[]) AS input(user_id, email)
        "#,
    )
    .bind(&user_ids)
    .bind(&emails)
    .bind(tenant_id)
    .execute(pool)
    .await?;
    Ok(user_ids)
}

async fn insert_group(pool: &PgPool, tenant_id: Uuid, display_name: &str) -> Result<Uuid> {
    let group_id = Uuid::new_v4();
    sqlx::query("INSERT INTO scim_groups (id, tenant_id, display_name) VALUES ($1, $2, $3)")
        .bind(group_id)
        .bind(tenant_id)
        .bind(display_name)
        .execute(pool)
        .await?;
    Ok(group_id)
}

async fn insert_group_page(
    pool: &PgPool,
    tenant_id: Uuid,
    group_ids: &[Uuid],
    display_names: &[String],
) -> Result<()> {
    sqlx::query(
        r#"
        INSERT INTO scim_groups (id, tenant_id, display_name)
        SELECT input.group_id, $3, input.display_name
        FROM UNNEST($1::uuid[], $2::text[]) AS input(group_id, display_name)
        "#,
    )
    .bind(group_ids)
    .bind(display_names)
    .bind(tenant_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn insert_memberships(pool: &PgPool, memberships: &[(Uuid, Uuid)]) -> Result<()> {
    let group_ids = memberships
        .iter()
        .map(|(group_id, _)| *group_id)
        .collect::<Vec<_>>();
    let user_ids = memberships
        .iter()
        .map(|(_, user_id)| *user_id)
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        INSERT INTO scim_group_members (group_id, user_id)
        SELECT input.group_id, input.user_id
        FROM UNNEST($1::uuid[], $2::uuid[]) AS input(group_id, user_id)
        "#,
    )
    .bind(&group_ids)
    .bind(&user_ids)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Clone)]
struct FixedAuth {
    identity: Identity,
}

#[async_trait]
impl AuthProvider for FixedAuth {
    async fn authenticate(&self, _credential: &Credential) -> Result<Identity, AuthError> {
        Ok(self.identity.clone())
    }

    fn name(&self) -> &'static str {
        "scim-groups-test"
    }
}

async fn scim_state(pool: PgPool, tenant_id: Uuid) -> Result<ScimState> {
    let (fga, _requests) = spawn_fga_mock(true).await?;
    Ok(ScimState::new(
        pool,
        Arc::new(FixedAuth {
            identity: Identity {
                identity_type: IdentityType::Operator,
                id: Uuid::new_v4(),
                tenant_id: TenantId::from(tenant_id),
                api_key_id: Some(Uuid::new_v4()),
                acting_on_behalf_of: None,
            },
        }),
        Some(fga),
        "https://moa.test/scim/v2".to_string(),
    ))
}

fn scim_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer test-token"));
    headers
}
