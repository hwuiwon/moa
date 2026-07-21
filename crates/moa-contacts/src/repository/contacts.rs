//! Contact and contact-point persistence operations.

use chrono::{DateTime, Utc};
use moa_core::{
    types::contact::ContactId, types::contact::ContactPointId, types::contact::ContactPointInput,
    types::contact::ContactPointRef, types::contact::ContactRef,
    types::contact::ContactTokenIssueRequest, types::contact::ContactVerificationState,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::session::SessionMeta,
};
use uuid::Uuid;

use crate::domain::{
    hash_contact_point_with_key_hex, normalize_contact_point, parse_contact_state,
};
use crate::{Error, Result};

use super::row_mapping::RowExt as _;

/// Issues a contact row and any unverified contact points in one transaction.
pub async fn issue_contact(
    pool: sqlx::PgPool,
    contact_point_hash_key_hex: &str,
    tenant_id: TenantId,
    request: ContactTokenIssueRequest,
) -> Result<(ContactRef, Vec<ContactPointRef>)> {
    let contact_id = ContactId::new();
    let state = if request.contact_points.is_empty() {
        ContactVerificationState::Anonymous
    } else {
        ContactVerificationState::Unverified
    };
    let mut transaction = pool
        .begin()
        .await
        .map_err(|error| Error::database("begin contact issuance", error))?;
    sqlx::query(
        r#"
        INSERT INTO contacts (
            id, tenant_id, storage_partition_id, contact_id, state, display_name, profile, metadata
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
    .bind(contact_id.0)
    .bind(state.as_str())
    .bind(request.display_name.as_deref())
    .bind(&request.profile)
    .bind(&request.metadata)
    .execute(&mut *transaction)
    .await
    .map_err(|error| Error::database("insert contact", error))?;

    let mut contact_points = Vec::with_capacity(request.contact_points.len());
    for point in request.contact_points {
        let contact_point = insert_contact_point(
            &mut transaction,
            contact_point_hash_key_hex,
            tenant_id,
            contact_id,
            point,
            false,
        )
        .await?;
        contact_points.push(contact_point);
    }

    transaction
        .commit()
        .await
        .map_err(|error| Error::database("commit contact issuance", error))?;

    Ok((
        ContactRef {
            contact_id,
            tenant_id,
            state,
            canonical_contact_id: None,
            linked_contact_ids: Vec::new(),
            scopes: Vec::new(),
            permissions: request.permissions,
            agent_ids: request.agent_ids,
            session_ids: Vec::new(),
            verified_contact_point_ids: Vec::new(),
        },
        contact_points,
    ))
}

/// Loads the persisted contact projection.
pub async fn load_contact_ref(
    pool: sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> Result<ContactRef> {
    let row = sqlx::query(
        r#"
        SELECT id, tenant_id, state, canonical_contact_id
        FROM contacts
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .fetch_optional(&pool)
    .await
    .map_err(|error| Error::database("load contact", error))?
    .ok_or_else(|| Error::terminal(404, "contact not found"))?;
    let state = row.col::<String>("state")?;
    Ok(ContactRef {
        contact_id,
        tenant_id,
        state: parse_contact_state(&state)?,
        canonical_contact_id: row
            .col::<Option<Uuid>>("canonical_contact_id")?
            .map(ContactId),
        linked_contact_ids: Vec::new(),
        scopes: Vec::new(),
        permissions: serde_json::Value::Null,
        agent_ids: Vec::new(),
        session_ids: Vec::new(),
        verified_contact_point_ids: Vec::new(),
    })
}

/// Resolves contact-point inputs to existing verified contacts in one tenant.
///
/// Plaintext contact-point values are normalized and hashed locally, then only
/// the keyed hashes are sent to storage.
pub async fn resolve_verified_contact_ids(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    contact_point_hash_key_hex: &str,
    contact_points: &[ContactPointInput],
) -> Result<Vec<ContactId>> {
    let mut contact_ids = Vec::new();
    for point in contact_points {
        let normalized = normalize_contact_point(point.kind, &point.value)?;
        let normalized_hash = hash_contact_point_with_key_hex(
            tenant_id,
            point.kind,
            &normalized,
            contact_point_hash_key_hex,
        )?;
        let rows = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT DISTINCT contact_id
            FROM contact_points
            WHERE tenant_id = $1
              AND storage_partition_id = $2
              AND kind = $3
              AND normalized_hash = $4
              AND verified = TRUE
            ORDER BY contact_id
            "#,
        )
        .bind(tenant_id.0)
        .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
        .bind(point.kind.as_str())
        .bind(&normalized_hash)
        .fetch_all(pool)
        .await
        .map_err(|error| Error::database("resolve verified contact point", error))?;
        for row in rows {
            let contact_id = ContactId(row);
            if !contact_ids.contains(&contact_id) {
                contact_ids.push(contact_id);
            }
        }
    }
    Ok(contact_ids)
}

/// Returns the contact id a session is promoted from, when promotion is allowed.
pub async fn promoted_from_contact(
    pool: &sqlx::PgPool,
    meta: &SessionMeta,
    contact: &ContactRef,
    tenant_id: TenantId,
) -> Result<Option<ContactId>> {
    let Some(current) = meta.contact.as_ref() else {
        return Err(Error::terminal(403, "session has no contact binding"));
    };
    if current.tenant_id != contact.tenant_id || current.tenant_id != tenant_id {
        return Err(Error::terminal(403, "session contact boundary mismatch"));
    }
    if current.contact_id == contact.contact_id {
        return Ok(None);
    }
    if contact_is_merged_into(pool, tenant_id, current.contact_id, contact.contact_id).await? {
        return Ok(Some(current.contact_id));
    }
    Err(Error::terminal(
        403,
        "session contact is not linked to verified contact",
    ))
}

pub(super) async fn ensure_contact_in_tenant(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    tenant_id: TenantId,
    contact_id: ContactId,
) -> Result<()> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM contacts WHERE id = $1 AND tenant_id = $2)",
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| Error::database("check contact workspace", error))?;
    if exists {
        Ok(())
    } else {
        Err(Error::terminal(404, "contact not found"))
    }
}

pub(super) async fn insert_contact_point(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    contact_point_hash_key_hex: &str,
    tenant_id: TenantId,
    contact_id: ContactId,
    point: ContactPointInput,
    verified: bool,
) -> Result<ContactPointRef> {
    let normalized = normalize_contact_point(point.kind, &point.value)?;
    let normalized_hash = hash_contact_point_with_key_hex(
        tenant_id,
        point.kind,
        &normalized,
        contact_point_hash_key_hex,
    )?;
    let point_id = ContactPointId::new();
    let verified_at = verified.then(Utc::now);
    let row = sqlx::query(
        r#"
        INSERT INTO contact_points
            (id, contact_id, tenant_id, storage_partition_id, kind, normalized_hash, display_value, verified, verified_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
        ON CONFLICT (tenant_id, storage_partition_id, contact_id, kind, normalized_hash)
        DO UPDATE SET
            display_value = COALESCE(EXCLUDED.display_value, contact_points.display_value),
            verified = contact_points.verified OR EXCLUDED.verified,
            verified_at = COALESCE(contact_points.verified_at, EXCLUDED.verified_at),
            updated_at = NOW()
        RETURNING id, display_value, verified, verified_at
        "#,
    )
    .bind(point_id.0)
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(StoragePartitionId::for_tenant(tenant_id).as_str())
    .bind(point.kind.as_str())
    .bind(&normalized_hash)
    .bind(point.display_value.as_deref())
    .bind(verified)
    .bind(verified_at)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| Error::database("upsert contact point", error))?;
    Ok(ContactPointRef {
        id: ContactPointId(row.col::<Uuid>("id")?),
        kind: point.kind,
        display_value: row.col::<Option<String>>("display_value")?,
        verified: row.col::<bool>("verified")?,
        verified_at: row.col::<Option<DateTime<Utc>>>("verified_at")?,
    })
}

async fn contact_is_merged_into(
    pool: &sqlx::PgPool,
    tenant_id: TenantId,
    contact_id: ContactId,
    canonical_contact_id: ContactId,
) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM contacts
            WHERE id = $1
              AND tenant_id = $2
              AND canonical_contact_id = $3
              AND state = 'merged'
        )
        "#,
    )
    .bind(contact_id.0)
    .bind(tenant_id.0)
    .bind(canonical_contact_id.0)
    .fetch_one(pool)
    .await
    .map_err(|error| Error::database("check promoted contact linkage", error))
}
