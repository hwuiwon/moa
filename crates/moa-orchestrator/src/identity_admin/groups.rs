//! SCIM group persistence and membership authorization mapping.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use moa_authz::enqueue_batch;
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use moa_ocsf::{ActorInput, ScimGroupAuditChange, emit_scim_group_changes_tx};
use uuid::Uuid;

use crate::services::scim::{ScimResponseError, map_db};

const MAX_GROUP_MEMBERS_PER_REQUEST: usize = 4096;

/// SCIM group list filter understood by the group repository.
#[derive(Debug)]
pub(crate) enum GroupFilter {
    /// Match group display name exactly.
    DisplayName(String),
    /// Match SCIM external id exactly.
    ExternalId(String),
}

/// Group row data used by SCIM response assembly.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct GroupRow {
    /// Group UUID.
    pub(crate) id: Uuid,
    /// SCIM external id.
    pub(crate) external_id: Option<String>,
    /// Group display name.
    pub(crate) display_name: String,
    /// Creation timestamp.
    pub(crate) created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub(crate) updated_at: DateTime<Utc>,
    /// Optimistic version used for SCIM ETags.
    pub(crate) version: i64,
}

/// Group member row data used by SCIM response assembly.
#[derive(Debug, Clone)]
pub(crate) struct GroupMemberRow {
    /// User UUID.
    pub(crate) user_id: Uuid,
    /// User email shown as SCIM member display text.
    pub(crate) email: String,
}

/// Group row plus member rows.
#[derive(Debug)]
pub(crate) struct GroupWithMembers {
    /// Group row.
    pub(crate) group: GroupRow,
    /// Current group members.
    pub(crate) members: Vec<GroupMemberRow>,
}

/// Data accepted by SCIM group create and replace operations.
#[derive(Debug, Clone)]
pub(crate) struct GroupWrite {
    /// Group display name.
    pub(crate) display_name: String,
    /// SCIM external id.
    pub(crate) external_id: Option<String>,
    /// Full replacement member set.
    pub(crate) members: Vec<Uuid>,
}

/// Data accepted by SCIM group PATCH.
#[derive(Debug, Clone)]
pub(crate) struct GroupPatch {
    /// Optional display-name replacement.
    pub(crate) display_name: Option<String>,
    /// Members to add.
    pub(crate) add_members: Vec<Uuid>,
    /// Members to remove.
    pub(crate) remove_members: Vec<Uuid>,
}

/// Fetch one page of groups with their members.
pub(crate) async fn fetch_groups_page(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    filter: Option<&GroupFilter>,
    start: i64,
    count: i64,
) -> Result<(i64, Vec<GroupWithMembers>), sqlx::Error> {
    let offset = start.saturating_sub(1);
    let (total, rows): (i64, Vec<GroupRow>) = match filter {
        None => {
            let total = sqlx::query_scalar("SELECT COUNT(*) FROM scim_groups WHERE tenant_id = $1")
                .bind(tenant_id)
                .fetch_one(pool)
                .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, display_name, created_at, updated_at, version
                FROM scim_groups
                WHERE tenant_id = $1
                ORDER BY created_at DESC, id
                LIMIT $2 OFFSET $3
                "#,
            )
            .bind(tenant_id)
            .bind(count)
            .bind(offset)
            .fetch_all(pool)
            .await?;
            (total, rows)
        }
        Some(GroupFilter::DisplayName(display_name)) => {
            let total = sqlx::query_scalar(
                "SELECT COUNT(*) FROM scim_groups WHERE tenant_id = $1 AND display_name = $2",
            )
            .bind(tenant_id)
            .bind(display_name)
            .fetch_one(pool)
            .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, display_name, created_at, updated_at, version
                FROM scim_groups
                WHERE tenant_id = $1 AND display_name = $2
                ORDER BY created_at DESC, id
                LIMIT $3 OFFSET $4
                "#,
            )
            .bind(tenant_id)
            .bind(display_name)
            .bind(count)
            .bind(offset)
            .fetch_all(pool)
            .await?;
            (total, rows)
        }
        Some(GroupFilter::ExternalId(external_id)) => {
            let total = sqlx::query_scalar(
                "SELECT COUNT(*) FROM scim_groups WHERE tenant_id = $1 AND external_id = $2",
            )
            .bind(tenant_id)
            .bind(external_id)
            .fetch_one(pool)
            .await?;
            let rows = sqlx::query_as(
                r#"
                SELECT id, external_id, display_name, created_at, updated_at, version
                FROM scim_groups
                WHERE tenant_id = $1 AND external_id = $2
                ORDER BY created_at DESC, id
                LIMIT $3 OFFSET $4
                "#,
            )
            .bind(tenant_id)
            .bind(external_id)
            .bind(count)
            .bind(offset)
            .fetch_all(pool)
            .await?;
            (total, rows)
        }
    };

    let group_ids = rows.iter().map(|group| group.id).collect::<Vec<_>>();
    let mut members_by_group = fetch_members_for_groups(pool, &group_ids).await?;
    let groups = rows
        .into_iter()
        .map(|group| {
            let members = members_by_group.remove(&group.id).unwrap_or_default();
            GroupWithMembers { group, members }
        })
        .collect();
    Ok((total, groups))
}

/// Fetch one group with members by id within a tenant.
pub(crate) async fn fetch_group_by_id(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    group_id: Uuid,
) -> Result<Option<GroupWithMembers>, ScimResponseError> {
    let row: Option<GroupRow> = sqlx::query_as(
        r#"
        SELECT id, external_id, display_name, created_at, updated_at, version
        FROM scim_groups
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(group_id)
    .bind(tenant_id)
    .fetch_optional(pool)
    .await
    .map_err(map_db)?;
    let Some(group) = row else {
        return Ok(None);
    };
    let members = fetch_members(pool, group_id).await.map_err(map_db)?;
    Ok(Some(GroupWithMembers { group, members }))
}

/// Create a SCIM group and all membership tuple/audit records atomically.
pub(crate) async fn create_group(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    actor: ActorInput,
    group: GroupWrite,
) -> Result<Uuid, ScimResponseError> {
    validate_requested_member_count(group.members.len())?;
    let target_members = normalize_member_ids(group.members);
    let new_tuples = authorization_tuples(tenant_id, &group.display_name, &target_members)?;
    let group_id = Uuid::new_v4();
    let mut tx = pool.begin().await.map_err(map_db)?;
    validate_users_in_tenant(&mut tx, tenant_id, &target_members).await?;
    sqlx::query(
        r#"
        INSERT INTO scim_groups (id, tenant_id, display_name, external_id)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(group_id)
    .bind(tenant_id)
    .bind(&group.display_name)
    .bind(group.external_id.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(map_db)?;

    insert_group_members(&mut tx, group_id, &target_members).await?;
    enqueue_tuple_difference(&mut tx, tenant_id, &BTreeSet::new(), &new_tuples).await?;

    let mut audit_changes = target_members
        .iter()
        .copied()
        .map(|user_id| ScimGroupAuditChange::MembershipAdded { group_id, user_id })
        .collect::<Vec<_>>();
    audit_changes.push(ScimGroupAuditChange::Created { group_id });
    emit_scim_group_changes_tx(&mut tx, tenant_id, actor, &audit_changes)
        .await
        .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)?;
    Ok(group_id)
}

/// Replace a SCIM group, membership set, tuple mappings, and audit records atomically.
pub(crate) async fn replace_group(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    group_id: Uuid,
    actor: ActorInput,
    group: GroupWrite,
) -> Result<(), ScimResponseError> {
    validate_requested_member_count(group.members.len())?;
    let target_members = normalize_member_ids(group.members);
    let mut tx = pool.begin().await.map_err(map_db)?;
    let existing = fetch_group_row_for_update(&mut tx, tenant_id, group_id).await?;
    let current_members = fetch_member_ids(&mut tx, group_id).await?;
    validate_users_in_tenant(&mut tx, tenant_id, &target_members).await?;
    apply_group_change(
        &mut tx,
        tenant_id,
        group_id,
        actor,
        GroupChange {
            existing: &existing,
            display_name: &group.display_name,
            external_id: group.external_id.as_deref(),
            current_members: &current_members,
            target_members: &target_members,
        },
    )
    .await?;

    tx.commit().await.map_err(map_db)
}

/// Patch a SCIM group, membership set, tuple mappings, and audit records atomically.
pub(crate) async fn patch_group(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    group_id: Uuid,
    actor: ActorInput,
    mutation: GroupPatch,
) -> Result<(), ScimResponseError> {
    validate_requested_member_count(
        mutation
            .add_members
            .len()
            .saturating_add(mutation.remove_members.len()),
    )?;
    let additions = normalize_member_ids(mutation.add_members);
    let removals = normalize_member_ids(mutation.remove_members);
    let requested_members = additions.union(&removals).copied().collect::<BTreeSet<_>>();
    let mut tx = pool.begin().await.map_err(map_db)?;
    let existing = fetch_group_row_for_update(&mut tx, tenant_id, group_id).await?;
    let current_members = fetch_member_ids(&mut tx, group_id).await?;
    validate_users_in_tenant(&mut tx, tenant_id, &requested_members).await?;
    let members_with_additions = current_members
        .union(&additions)
        .copied()
        .collect::<BTreeSet<_>>();
    let target_members = members_with_additions
        .difference(&removals)
        .copied()
        .collect::<BTreeSet<_>>();
    validate_requested_member_count(target_members.len())?;
    let display_name = mutation
        .display_name
        .as_deref()
        .unwrap_or(&existing.display_name)
        .trim()
        .to_string();
    apply_group_change(
        &mut tx,
        tenant_id,
        group_id,
        actor,
        GroupChange {
            existing: &existing,
            display_name: &display_name,
            external_id: existing.external_id.as_deref(),
            current_members: &current_members,
            target_members: &target_members,
        },
    )
    .await?;

    tx.commit().await.map_err(map_db)
}

/// Delete a SCIM group and all membership tuple/audit records atomically.
pub(crate) async fn delete_group(
    pool: &sqlx::PgPool,
    tenant_id: Uuid,
    group_id: Uuid,
    actor: ActorInput,
) -> Result<(), ScimResponseError> {
    let mut tx = pool.begin().await.map_err(map_db)?;
    let existing = fetch_group_row_for_update(&mut tx, tenant_id, group_id).await?;
    let members = fetch_member_ids(&mut tx, group_id).await?;
    let old_tuples = authorization_tuples(tenant_id, &existing.display_name, &members)?;
    enqueue_tuple_difference(&mut tx, tenant_id, &old_tuples, &BTreeSet::new()).await?;
    sqlx::query("DELETE FROM scim_groups WHERE id = $1 AND tenant_id = $2")
        .bind(group_id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;

    let mut audit_changes = members
        .iter()
        .copied()
        .map(|user_id| ScimGroupAuditChange::MembershipRemoved { group_id, user_id })
        .collect::<Vec<_>>();
    audit_changes.push(ScimGroupAuditChange::Deleted { group_id });
    emit_scim_group_changes_tx(&mut tx, tenant_id, actor, &audit_changes)
        .await
        .map_err(map_audit)?;

    tx.commit().await.map_err(map_db)
}

async fn fetch_group_row_for_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    group_id: Uuid,
) -> Result<GroupRow, ScimResponseError> {
    sqlx::query_as(
        r#"
        SELECT id, external_id, display_name, created_at, updated_at, version
        FROM scim_groups
        WHERE id = $1 AND tenant_id = $2
        FOR UPDATE
        "#,
    )
    .bind(group_id)
    .bind(tenant_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(map_db)?
    .ok_or_else(|| ScimResponseError::not_found("group not found"))
}

async fn fetch_members(
    pool: &sqlx::PgPool,
    group_id: Uuid,
) -> Result<Vec<GroupMemberRow>, sqlx::Error> {
    let mut members_by_group = fetch_members_for_groups(pool, &[group_id]).await?;
    Ok(members_by_group.remove(&group_id).unwrap_or_default())
}

async fn fetch_members_for_groups(
    pool: &sqlx::PgPool,
    group_ids: &[Uuid],
) -> Result<BTreeMap<Uuid, Vec<GroupMemberRow>>, sqlx::Error> {
    if group_ids.is_empty() {
        return Ok(BTreeMap::new());
    }

    let rows: Vec<(Uuid, Uuid, String)> = sqlx::query_as(
        r#"
        SELECT gm.group_id, u.id, u.email
        FROM scim_group_members gm
        JOIN users u ON u.id = gm.user_id
        WHERE gm.group_id = ANY($1)
        ORDER BY gm.group_id, u.email, u.id
        "#,
    )
    .bind(group_ids)
    .fetch_all(pool)
    .await?;

    let mut members_by_group = BTreeMap::<Uuid, Vec<GroupMemberRow>>::new();
    for (group_id, user_id, email) in rows {
        members_by_group
            .entry(group_id)
            .or_default()
            .push(GroupMemberRow { user_id, email });
    }
    Ok(members_by_group)
}

fn validate_requested_member_count(member_count: usize) -> Result<(), ScimResponseError> {
    if member_count > MAX_GROUP_MEMBERS_PER_REQUEST {
        return Err(ScimResponseError::bad_request(
            "tooMany",
            format!(
                "group membership request exceeds the {MAX_GROUP_MEMBERS_PER_REQUEST} member limit"
            ),
        ));
    }
    Ok(())
}

async fn fetch_member_ids(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_id: Uuid,
) -> Result<BTreeSet<Uuid>, ScimResponseError> {
    let members = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT user_id
        FROM scim_group_members
        WHERE group_id = $1
        ORDER BY user_id
        LIMIT $2
        "#,
    )
    .bind(group_id)
    .bind((MAX_GROUP_MEMBERS_PER_REQUEST + 1) as i64)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_db)?;
    validate_requested_member_count(members.len())?;
    Ok(members.into_iter().collect())
}

fn normalize_member_ids(members: Vec<Uuid>) -> BTreeSet<Uuid> {
    members.into_iter().collect()
}

async fn validate_users_in_tenant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    requested: &BTreeSet<Uuid>,
) -> Result<(), ScimResponseError> {
    if requested.is_empty() {
        return Ok(());
    }
    let requested = requested.iter().copied().collect::<Vec<_>>();
    let found = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM users
        WHERE tenant_id = $1
          AND id = ANY($2)
        ORDER BY id
        "#,
    )
    .bind(tenant_id)
    .bind(&requested)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_db)?;
    if found == requested {
        Ok(())
    } else {
        Err(ScimResponseError::bad_request(
            "invalidValue",
            "group member user does not exist in tenant",
        ))
    }
}

async fn insert_group_members(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_id: Uuid,
    members: &BTreeSet<Uuid>,
) -> Result<(), ScimResponseError> {
    if members.is_empty() {
        return Ok(());
    }
    let expected = members.iter().copied().collect::<Vec<_>>();
    let inserted = sqlx::query_scalar::<_, Uuid>(
        r#"
        INSERT INTO scim_group_members (group_id, user_id)
        SELECT $1, input.user_id
        FROM UNNEST($2::uuid[]) AS input(user_id)
        ORDER BY input.user_id
        RETURNING user_id
        "#,
    )
    .bind(group_id)
    .bind(&expected)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_db)?
    .into_iter()
    .collect::<BTreeSet<_>>();
    ensure_exact_member_change(&inserted, members, "insert")
}

async fn delete_group_members(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_id: Uuid,
    members: &BTreeSet<Uuid>,
) -> Result<(), ScimResponseError> {
    if members.is_empty() {
        return Ok(());
    }
    let expected = members.iter().copied().collect::<Vec<_>>();
    let deleted = sqlx::query_scalar::<_, Uuid>(
        r#"
        DELETE FROM scim_group_members
        WHERE group_id = $1
          AND user_id = ANY($2)
        RETURNING user_id
        "#,
    )
    .bind(group_id)
    .bind(&expected)
    .fetch_all(&mut **tx)
    .await
    .map_err(map_db)?
    .into_iter()
    .collect::<BTreeSet<_>>();
    ensure_exact_member_change(&deleted, members, "delete")
}

fn ensure_exact_member_change(
    actual: &BTreeSet<Uuid>,
    expected: &BTreeSet<Uuid>,
    operation: &str,
) -> Result<(), ScimResponseError> {
    if actual == expected {
        Ok(())
    } else {
        tracing::error!(
            operation,
            expected = expected.len(),
            actual = actual.len(),
            "SCIM membership set change did not affect its exact expected rows"
        );
        Err(ScimResponseError::internal(
            "group membership changed concurrently",
        ))
    }
}

fn authorization_tuples(
    tenant_id: Uuid,
    display_name: &str,
    members: &BTreeSet<Uuid>,
) -> Result<BTreeSet<TupleKey>, ScimResponseError> {
    let Some(relation) = group_target(tenant_id, display_name)? else {
        return Ok(BTreeSet::new());
    };
    Ok(members
        .iter()
        .copied()
        .map(|user_id| {
            TupleKey::new(
                UserType::Operator,
                user_id,
                relation,
                ObjectType::Tenant,
                tenant_id,
            )
        })
        .collect())
}

async fn enqueue_tuple_difference(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    old_tuples: &BTreeSet<TupleKey>,
    new_tuples: &BTreeSet<TupleKey>,
) -> Result<(), ScimResponseError> {
    let intents = old_tuples
        .difference(new_tuples)
        .map(|tuple| (TupleOp::Delete, *tuple))
        .chain(
            new_tuples
                .difference(old_tuples)
                .map(|tuple| (TupleOp::Write, *tuple)),
        )
        .collect::<Vec<_>>();
    enqueue_batch(tx, tenant_id, &intents)
        .await
        .map_err(map_outbox)
}

struct GroupChange<'a> {
    existing: &'a GroupRow,
    display_name: &'a str,
    external_id: Option<&'a str>,
    current_members: &'a BTreeSet<Uuid>,
    target_members: &'a BTreeSet<Uuid>,
}

async fn apply_group_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    group_id: Uuid,
    actor: ActorInput,
    change: GroupChange<'_>,
) -> Result<(), ScimResponseError> {
    let added_members = change
        .target_members
        .difference(change.current_members)
        .copied()
        .collect::<BTreeSet<_>>();
    let removed_members = change
        .current_members
        .difference(change.target_members)
        .copied()
        .collect::<BTreeSet<_>>();
    let old_tuples = authorization_tuples(
        tenant_id,
        &change.existing.display_name,
        change.current_members,
    )?;
    let new_tuples = authorization_tuples(tenant_id, change.display_name, change.target_members)?;
    let metadata_changed = change.existing.display_name != change.display_name
        || change.existing.external_id.as_deref() != change.external_id;
    if !metadata_changed && added_members.is_empty() && removed_members.is_empty() {
        return Ok(());
    }

    delete_group_members(tx, group_id, &removed_members).await?;
    insert_group_members(tx, group_id, &added_members).await?;
    let updated = sqlx::query(
        r#"
        UPDATE scim_groups
        SET display_name = $3,
            external_id = $4,
            updated_at = NOW(),
            version = version + 1
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(group_id)
    .bind(tenant_id)
    .bind(change.display_name)
    .bind(change.external_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db)?;
    if updated.rows_affected() != 1 {
        return Err(ScimResponseError::internal("group changed concurrently"));
    }
    enqueue_tuple_difference(tx, tenant_id, &old_tuples, &new_tuples).await?;

    let retained_members = change
        .current_members
        .intersection(change.target_members)
        .copied()
        .collect::<BTreeSet<_>>();
    let mut audit_changes = removed_members
        .iter()
        .copied()
        .map(|user_id| ScimGroupAuditChange::MembershipRemoved { group_id, user_id })
        .chain(
            added_members
                .iter()
                .copied()
                .map(|user_id| ScimGroupAuditChange::MembershipAdded { group_id, user_id }),
        )
        .collect::<Vec<_>>();
    audit_changes.extend(
        old_tuples
            .difference(&new_tuples)
            .filter(|tuple| retained_members.contains(&tuple.user_id))
            .map(|tuple| ScimGroupAuditChange::PrivilegeRevoked {
                group_id,
                user_id: tuple.user_id,
                relation: tuple.relation.to_string(),
                object: tuple.object_wire(),
            }),
    );
    audit_changes.extend(
        new_tuples
            .difference(&old_tuples)
            .filter(|tuple| retained_members.contains(&tuple.user_id))
            .map(|tuple| ScimGroupAuditChange::PrivilegeGranted {
                group_id,
                user_id: tuple.user_id,
                relation: tuple.relation.to_string(),
                object: tuple.object_wire(),
            }),
    );
    audit_changes.push(ScimGroupAuditChange::Updated { group_id });
    emit_scim_group_changes_tx(tx, tenant_id, actor, &audit_changes)
        .await
        .map_err(map_audit)?;
    Ok(())
}

fn group_target(
    tenant_id: Uuid,
    display_name: &str,
) -> Result<Option<Relation>, ScimResponseError> {
    let parts: Vec<&str> = display_name.split(':').collect();
    match parts.as_slice() {
        ["tenant", tenant, relation] if Uuid::parse_str(tenant).ok() == Some(tenant_id) => {
            if !matches!(*relation, "admin" | "operator") {
                return Err(ScimResponseError::bad_request(
                    "invalidValue",
                    "group display name maps to unsupported tenant relation",
                ));
            }
            Ok(Some(if *relation == "admin" {
                Relation::Admin
            } else {
                Relation::Operator
            }))
        }
        ["tenant", tenant, _] if Uuid::parse_str(tenant).is_ok() => {
            Err(ScimResponseError::bad_request(
                "invalidValue",
                "group display name tenant does not match request tenant",
            ))
        }
        _ => Ok(None),
    }
}

fn map_outbox(error: moa_authz::AuthzError) -> ScimResponseError {
    tracing::error!(error = %error, "SCIM group authorization queue error");
    ScimResponseError::internal("authorization queue error")
}

fn map_audit(error: moa_ocsf::EmitError) -> ScimResponseError {
    tracing::error!(error = %error, "SCIM group security audit failed");
    ScimResponseError::internal("security audit failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant_id() -> Uuid {
        Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa")
            .expect("fixture tenant UUID should parse")
    }

    #[test]
    fn tenant_admin_group_maps_to_schema_relation() {
        // Pins: SCIM tenant:<id>:admin groups emit only schema-backed tenant relations.
        let target = group_target(
            tenant_id(),
            "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:admin",
        )
        .expect("admin group should map")
        .expect("admin group should have a target");

        assert_eq!(target, Relation::Admin);
    }

    #[test]
    fn ordinary_group_does_not_emit_tenant_member_tuple() {
        // Pins: ordinary SCIM groups remain local product data and do not emit
        // undefined tenant#member OpenFGA tuples.
        let target = group_target(tenant_id(), "support-team").expect("ordinary group maps");

        assert!(target.is_none());
    }

    #[test]
    fn unsupported_tenant_relation_is_rejected() {
        // Pins: stale group names such as member do not survive as tuple aliases.
        use axum::{http::StatusCode, response::IntoResponse};

        let error = group_target(
            tenant_id(),
            "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:member",
        )
        .expect_err("unsupported relation should be rejected");

        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }
}
