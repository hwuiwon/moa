//! Contact token-grant persistence operations.

use chrono::{DateTime, Utc};
use moa_core::{
    types::contact::ContactId, types::contact::ContactTokenClaims,
    types::identifiers::StoragePartitionId,
};
use uuid::Uuid;

use crate::{Error, Result};

/// Persists a contact token grant for later revocation checks.
pub async fn create_contact_token_grant(
    pool: sqlx::PgPool,
    claims: &ContactTokenClaims,
    contact_id: ContactId,
    expires_at: DateTime<Utc>,
    issued_by_actor_type: &'static str,
    issued_by_actor_id: Option<Uuid>,
) -> Result<()> {
    let session_ids = claims
        .session_ids
        .iter()
        .map(|session_id| session_id.0)
        .collect::<Vec<_>>();
    sqlx::query(
        r#"
        INSERT INTO contact_token_grants
            (id, token_jti, tenant_id, storage_partition_id, contact_id, state, scopes, permissions,
             agent_ids, session_ids, issued_by_actor_type, issued_by_actor_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
        ON CONFLICT (token_jti) DO NOTHING
        "#,
    )
    .bind(Uuid::now_v7())
    .bind(&claims.jti)
    .bind(claims.tenant_id.0)
    .bind(StoragePartitionId::for_tenant(claims.tenant_id).as_str())
    .bind(contact_id.0)
    .bind(claims.state.as_str())
    .bind(&claims.scopes)
    .bind(&claims.permissions)
    .bind(&claims.agent_ids)
    .bind(&session_ids)
    .bind(issued_by_actor_type)
    .bind(issued_by_actor_id)
    .bind(expires_at)
    .execute(&pool)
    .await
    .map_err(|error| Error::database("insert contact token grant", error))?;
    Ok(())
}

/// Verifies that a contact token grant is active and unexpired.
pub async fn ensure_contact_token_grant_active(
    pool: &sqlx::PgPool,
    claims: &ContactTokenClaims,
    contact_id: ContactId,
) -> Result<()> {
    let active = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM contact_token_grants
            WHERE token_jti = $1
              AND tenant_id = $2
              AND contact_id = $3
              AND state = $4
              AND revoked_at IS NULL
              AND expires_at > NOW()
        )
        "#,
    )
    .bind(&claims.jti)
    .bind(claims.tenant_id.0)
    .bind(contact_id.0)
    .bind(claims.state.as_str())
    .fetch_one(pool)
    .await
    .map_err(|error| Error::database("check contact token grant", error))?;
    if active {
        Ok(())
    } else {
        Err(Error::terminal(401, "contact token grant is not active"))
    }
}
