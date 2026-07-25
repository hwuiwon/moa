//! Postgres knowledge contact-group persistence operations.

use super::row_mapping::*;
use super::*;

/// Serializes same-group writers behind a transaction-scoped advisory lock.
///
/// Contact groups are cross-object by construction: concurrent page records
/// legitimately derive the byte-identical group and race their writes. The
/// group table carries a second unique index
/// (`knowledge_contact_groups_name_uniq`) beside the `group_uid` arbiter, and
/// PostgreSQL only routes arbiter-index conflicts into `DO UPDATE` — a
/// concurrent identical insert first detected on the name index raises a plain
/// `23505`. The same concurrency also drives unordered same-group membership
/// updates toward `40P01`. Taking one per-group lock before either write makes
/// the second writer wait for the first commit, after which its arbiter check
/// sees the committed row and takes the update path.
async fn lock_contact_group(conn: &mut moa_db::ScopedConn<'_>, group_uid: Uuid) -> Result<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('knowledge_contact_group:' || $1::text, 0))",
    )
    .bind(group_uid)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    Ok(())
}

pub(super) async fn upsert_contact_group(
    repository: &PostgresKnowledgeRepository,
    group: ContactGroup,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    lock_contact_group(&mut conn, group.group_uid).await?;
    sqlx::query(
        r#"
        INSERT INTO moa.knowledge_contact_groups (
            group_uid, tenant_id, storage_partition_id, group_kind,
            normalized_name, display_name, source_connection_id, metadata
        )
        VALUES (
            $1,
            $2,
            $3,
            'derived',
            $4,
            $5,
            (
                SELECT connection_uid
                FROM moa.knowledge_connections
                WHERE connection_uid = $6
                  AND tenant_id = $2
            ),
            $7
        )
        ON CONFLICT (group_uid)
        DO UPDATE SET
            display_name = EXCLUDED.display_name,
            source_connection_id = EXCLUDED.source_connection_id,
            metadata = EXCLUDED.metadata,
            updated_at = now()
        "#,
    )
    .bind(group.group_uid)
    .bind(group.tenant_id.0)
    .bind(storage_partition_id(group.tenant_id))
    .bind(group.group_key)
    .bind(group.display_name)
    .bind(source_connection_id(&group.metadata))
    .bind(group.metadata)
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn replace_contact_group_memberships(
    repository: &PostgresKnowledgeRepository,
    group_uid: Uuid,
    memberships: Vec<ContactGroupMembership>,
) -> Result<()> {
    let mut conn = repository.begin().await?;
    lock_contact_group(&mut conn, group_uid).await?;
    let mut memberships_by_contact = BTreeMap::new();
    for mut membership in memberships {
        membership.evidence.sort_unstable();
        membership.evidence.dedup();
        memberships_by_contact.insert(membership.contact_id.0, membership);
    }
    let active_contact_ids = memberships_by_contact.keys().copied().collect::<Vec<_>>();
    if active_contact_ids.is_empty() {
        sqlx::query(
            r#"
            UPDATE moa.knowledge_contact_group_memberships
            SET active = FALSE, updated_at = now()
            WHERE group_id = $1
              AND active = TRUE
            "#,
        )
        .bind(group_uid)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
    } else {
        sqlx::query(
            r#"
            UPDATE moa.knowledge_contact_group_memberships
            SET active = FALSE, updated_at = now()
            WHERE group_id = $1
              AND active = TRUE
              AND NOT (contact_id = ANY($2::UUID[]))
            "#,
        )
        .bind(group_uid)
        .bind(&active_contact_ids)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
    }
    for membership in memberships_by_contact.into_values() {
        sqlx::query(
            r#"
            INSERT INTO moa.knowledge_contact_group_memberships (
                tenant_id, storage_partition_id, group_id, contact_id,
                active, evidence_ids, metadata
            )
            SELECT tenant_id, storage_partition_id, group_uid, $2, TRUE, $3, $4
            FROM moa.knowledge_contact_groups
            WHERE group_uid = $1
            ON CONFLICT (tenant_id, group_id, contact_id) WHERE active = TRUE
            DO UPDATE SET
                evidence_ids = EXCLUDED.evidence_ids,
                metadata = EXCLUDED.metadata,
                updated_at = now()
            WHERE moa.knowledge_contact_group_memberships.evidence_ids
                    IS DISTINCT FROM EXCLUDED.evidence_ids
               OR moa.knowledge_contact_group_memberships.metadata
                    IS DISTINCT FROM EXCLUDED.metadata
            "#,
        )
        .bind(group_uid)
        .bind(membership.contact_id.0)
        .bind(membership.evidence)
        .bind(membership.metadata)
        .execute(conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
    }
    conn.commit().await.map_err(map_moa_error)
}

pub(super) async fn contact_group_targets(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    group_key: &str,
) -> Result<Option<ContactGroupTarget>> {
    let mut conn = repository.begin().await?;
    let group = sqlx::query(
        r#"
        SELECT group_uid, tenant_id, normalized_name, display_name, metadata
        FROM moa.knowledge_contact_groups
        WHERE tenant_id = $1
          AND group_kind = 'derived'
          AND normalized_name = $2
        "#,
    )
    .bind(tenant_id.0)
    .bind(group_key)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    let Some(group_row) = group else {
        conn.commit().await.map_err(map_moa_error)?;
        return Ok(None);
    };
    let group = contact_group_from_row(&group_row)?;
    let rows = sqlx::query(
        r#"
        SELECT contact_id, evidence_ids, metadata
        FROM moa.knowledge_contact_group_memberships
        WHERE tenant_id = $1
          AND group_id = $2
          AND active = TRUE
        ORDER BY contact_id ASC
        "#,
    )
    .bind(tenant_id.0)
    .bind(group.group_uid)
    .fetch_all(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    let members = rows
        .iter()
        .map(contact_group_target_member_from_row)
        .collect::<Result<Vec<_>>>()?;
    Ok(Some(ContactGroupTarget::from_active_members(
        group, members,
    )))
}
