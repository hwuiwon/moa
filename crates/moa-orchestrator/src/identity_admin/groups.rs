//! SCIM group persistence and membership authorization mapping.

use chrono::{DateTime, Utc};
use moa_authz::enqueue_raw;
use moa_authz_schema::TupleOp;
use moa_ocsf::ActorInput;
use uuid::Uuid;

use crate::services::scim::{ScimResponseError, map_db};

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
                ORDER BY created_at DESC
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
                ORDER BY created_at DESC
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
                ORDER BY created_at DESC
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

    let mut groups = Vec::with_capacity(rows.len());
    for group in rows {
        let members = fetch_members(pool, group.id).await?;
        groups.push(GroupWithMembers { group, members });
    }
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
    let group_id = Uuid::new_v4();
    let mut tx = pool.begin().await.map_err(map_db)?;
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

    for user_id in group.members {
        add_group_member(
            &mut tx,
            tenant_id,
            group_id,
            &group.display_name,
            user_id,
            actor.clone(),
        )
        .await?;
    }

    moa_ocsf::emit_scim_group_created_tx(&mut tx, tenant_id, actor, group_id)
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
    let mut tx = pool.begin().await.map_err(map_db)?;
    let existing = fetch_group_row_for_update(&mut tx, tenant_id, group_id).await?;
    let current_members = fetch_member_ids(&mut tx, group_id).await?;

    for user_id in &current_members {
        enqueue_group_mapping(
            &mut tx,
            TupleOp::Delete,
            tenant_id,
            &existing.display_name,
            *user_id,
        )
        .await?;
        moa_ocsf::emit_group_membership_removed_tx(
            &mut tx,
            tenant_id,
            actor.clone(),
            group_id,
            *user_id,
        )
        .await
        .map_err(map_audit)?;
    }
    sqlx::query("DELETE FROM scim_group_members WHERE group_id = $1")
        .bind(group_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;

    sqlx::query(
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
    .bind(&group.display_name)
    .bind(group.external_id.as_deref())
    .execute(&mut *tx)
    .await
    .map_err(map_db)?;

    for user_id in group.members {
        add_group_member(
            &mut tx,
            tenant_id,
            group_id,
            &group.display_name,
            user_id,
            actor.clone(),
        )
        .await?;
    }

    moa_ocsf::emit_scim_group_updated_tx(&mut tx, tenant_id, actor, group_id)
        .await
        .map_err(map_audit)?;

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
    let mut tx = pool.begin().await.map_err(map_db)?;
    let existing = fetch_group_row_for_update(&mut tx, tenant_id, group_id).await?;
    let display_name = mutation
        .display_name
        .as_deref()
        .unwrap_or(&existing.display_name)
        .trim()
        .to_string();

    if mutation.display_name.is_some() && display_name != existing.display_name {
        let members = fetch_member_ids(&mut tx, group_id).await?;
        for user_id in &members {
            enqueue_group_mapping(
                &mut tx,
                TupleOp::Delete,
                tenant_id,
                &existing.display_name,
                *user_id,
            )
            .await?;
            moa_ocsf::emit_group_membership_removed_tx(
                &mut tx,
                tenant_id,
                actor.clone(),
                group_id,
                *user_id,
            )
            .await
            .map_err(map_audit)?;
            enqueue_group_mapping(&mut tx, TupleOp::Write, tenant_id, &display_name, *user_id)
                .await?;
            moa_ocsf::emit_group_membership_added_tx(
                &mut tx,
                tenant_id,
                actor.clone(),
                group_id,
                *user_id,
            )
            .await
            .map_err(map_audit)?;
        }
        sqlx::query(
            "UPDATE scim_groups SET display_name = $3, updated_at = NOW(), version = version + 1 WHERE id = $1 AND tenant_id = $2",
        )
        .bind(group_id)
        .bind(tenant_id)
        .bind(&display_name)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;
    }

    for user_id in mutation.add_members {
        add_group_member(
            &mut tx,
            tenant_id,
            group_id,
            &display_name,
            user_id,
            actor.clone(),
        )
        .await?;
    }
    for user_id in mutation.remove_members {
        remove_group_member(
            &mut tx,
            tenant_id,
            group_id,
            &display_name,
            user_id,
            actor.clone(),
        )
        .await?;
    }

    moa_ocsf::emit_scim_group_updated_tx(&mut tx, tenant_id, actor, group_id)
        .await
        .map_err(map_audit)?;

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
    for user_id in members {
        enqueue_group_mapping(
            &mut tx,
            TupleOp::Delete,
            tenant_id,
            &existing.display_name,
            user_id,
        )
        .await?;
        moa_ocsf::emit_group_membership_removed_tx(
            &mut tx,
            tenant_id,
            actor.clone(),
            group_id,
            user_id,
        )
        .await
        .map_err(map_audit)?;
    }
    sqlx::query("DELETE FROM scim_groups WHERE id = $1 AND tenant_id = $2")
        .bind(group_id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(map_db)?;

    moa_ocsf::emit_scim_group_deleted_tx(&mut tx, tenant_id, actor, group_id)
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
    let rows: Vec<(Uuid, String)> = sqlx::query_as(
        r#"
        SELECT u.id, u.email
        FROM scim_group_members gm
        JOIN users u ON u.id = gm.user_id
        WHERE gm.group_id = $1
        ORDER BY u.email
        "#,
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(user_id, email)| GroupMemberRow { user_id, email })
        .collect())
}

async fn fetch_member_ids(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_id: Uuid,
) -> Result<Vec<Uuid>, ScimResponseError> {
    let rows: Vec<(Uuid,)> =
        sqlx::query_as("SELECT user_id FROM scim_group_members WHERE group_id = $1")
            .bind(group_id)
            .fetch_all(&mut **tx)
            .await
            .map_err(map_db)?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

async fn add_group_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    group_id: Uuid,
    display_name: &str,
    user_id: Uuid,
    actor: ActorInput,
) -> Result<(), ScimResponseError> {
    ensure_user_in_tenant(tx, tenant_id, user_id).await?;
    sqlx::query(
        "INSERT INTO scim_group_members (group_id, user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(group_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await
    .map_err(map_db)?;
    enqueue_group_mapping(tx, TupleOp::Write, tenant_id, display_name, user_id).await?;
    moa_ocsf::emit_group_membership_added_tx(tx, tenant_id, actor, group_id, user_id)
        .await
        .map_err(map_audit)?;
    Ok(())
}

async fn remove_group_member(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    group_id: Uuid,
    display_name: &str,
    user_id: Uuid,
    actor: ActorInput,
) -> Result<(), ScimResponseError> {
    sqlx::query("DELETE FROM scim_group_members WHERE group_id = $1 AND user_id = $2")
        .bind(group_id)
        .bind(user_id)
        .execute(&mut **tx)
        .await
        .map_err(map_db)?;
    enqueue_group_mapping(tx, TupleOp::Delete, tenant_id, display_name, user_id).await?;
    moa_ocsf::emit_group_membership_removed_tx(tx, tenant_id, actor, group_id, user_id)
        .await
        .map_err(map_audit)?;
    Ok(())
}

async fn ensure_user_in_tenant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: Uuid,
    user_id: Uuid,
) -> Result<(), ScimResponseError> {
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = $1 AND tenant_id = $2)")
            .bind(user_id)
            .bind(tenant_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(map_db)?;
    if exists {
        Ok(())
    } else {
        Err(ScimResponseError::bad_request(
            "invalidValue",
            "group member user does not exist in tenant",
        ))
    }
}

async fn enqueue_group_mapping(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    op: TupleOp,
    tenant_id: Uuid,
    display_name: &str,
    user_id: Uuid,
) -> Result<(), ScimResponseError> {
    let targets = group_targets(tenant_id, display_name)?;
    for target in targets {
        enqueue_raw(
            &mut **tx,
            op,
            &format!("user:{user_id}"),
            &target.relation,
            &target.object,
            Some(tenant_id),
        )
        .await
        .map_err(map_outbox)?;
    }
    Ok(())
}

#[derive(Debug)]
struct GroupTarget {
    relation: String,
    object: String,
}

fn group_targets(
    tenant_id: Uuid,
    display_name: &str,
) -> Result<Vec<GroupTarget>, ScimResponseError> {
    let parts: Vec<&str> = display_name.split(':').collect();
    match parts.as_slice() {
        ["tenant", tenant, relation] if Uuid::parse_str(tenant).ok() == Some(tenant_id) => {
            if !matches!(*relation, "admin" | "operator") {
                return Err(ScimResponseError::bad_request(
                    "invalidValue",
                    "group display name maps to unsupported tenant relation",
                ));
            }
            Ok(vec![GroupTarget {
                relation: (*relation).to_string(),
                object: format!("tenant:{tenant}"),
            }])
        }
        ["tenant", tenant, _] if Uuid::parse_str(tenant).is_ok() => {
            Err(ScimResponseError::bad_request(
                "invalidValue",
                "group display name tenant does not match request tenant",
            ))
        }
        _ => Ok(Vec::new()),
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
        let targets = group_targets(
            tenant_id(),
            "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:admin",
        )
        .expect("admin group should map");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].relation.as_str(), "admin");
        assert_eq!(
            targets[0].object.as_str(),
            "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"
        );
    }

    #[test]
    fn ordinary_group_does_not_emit_tenant_member_tuple() {
        // Pins: ordinary SCIM groups remain local product data and do not emit
        // undefined tenant#member OpenFGA tuples.
        let targets = group_targets(tenant_id(), "support-team").expect("ordinary group maps");

        assert_eq!(targets.len(), 0);
    }

    #[test]
    fn unsupported_tenant_relation_is_rejected() {
        // Pins: stale group names such as member do not survive as tuple aliases.
        use axum::{http::StatusCode, response::IntoResponse};

        let error = group_targets(
            tenant_id(),
            "tenant:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:member",
        )
        .expect_err("unsupported relation should be rejected");

        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }
}
