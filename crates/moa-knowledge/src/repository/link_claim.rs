//! Postgres persistence for operation-fenced knowledge link claims.
//!
//! Every statement here runs through the repository's tenant-scoped `moa_app`
//! connection, so the forced row-level-security policy on
//! `moa.knowledge_link_claims` decides visibility: a missing or wrong
//! `moa.tenant_id` sees no claim at all rather than another tenant's.

use super::row_mapping::*;
use super::*;

const CLAIM_COLUMNS: &str = r#"
    tenant_id, operation_id, request_hash, owner_identity_id, connection_uid,
    previous_credential_ref, candidate_credential_ref, state, sync_run_uid,
    created_at, updated_at
"#;

pub(super) async fn reserve_link_claim(
    repository: &PostgresKnowledgeRepository,
    claim: NewLinkClaim,
) -> Result<LinkClaimReservation> {
    let mut conn = repository.begin().await?;
    let inserted = sqlx::query(&format!(
        r#"
        INSERT INTO moa.knowledge_link_claims (
            tenant_id, operation_id, request_hash, owner_identity_id, connection_uid,
            previous_credential_ref, state
        )
        VALUES ($1, $2, $3, $4, $5, $6, 'reserved')
        ON CONFLICT (tenant_id, operation_id) DO NOTHING
        RETURNING {CLAIM_COLUMNS}
        "#
    ))
    .bind(claim.tenant_id.0)
    .bind(&claim.operation_id)
    .bind(&claim.request_hash)
    .bind(claim.owner_identity_id)
    .bind(claim.connection_uid)
    .bind(claim.previous_credential_ref.as_deref())
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;

    if let Some(row) = inserted {
        let reserved = link_claim_from_row(&row)?;
        conn.commit().await.map_err(map_moa_error)?;
        return Ok(LinkClaimReservation::Reserved(reserved));
    }

    let existing = sqlx::query(&format!(
        r#"
        SELECT {CLAIM_COLUMNS}
        FROM moa.knowledge_link_claims
        WHERE tenant_id = $1 AND operation_id = $2
        "#
    ))
    .bind(claim.tenant_id.0)
    .bind(&claim.operation_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;

    let row = existing.ok_or_else(|| {
        Error::Repository("knowledge link claim vanished between insert and read".to_string())
    })?;
    let existing = link_claim_from_row(&row)?;
    // The hash covers the operation's selector and the connection it claims, so
    // a reused id whose inputs changed is a conflict rather than a resume that
    // would silently adopt another link's connection.
    if existing.request_hash != claim.request_hash
        || existing.connection_uid != claim.connection_uid
    {
        return Ok(LinkClaimReservation::Conflict);
    }
    Ok(LinkClaimReservation::Existing(existing))
}

pub(super) async fn advance_link_claim(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    operation_id: &str,
    transition: LinkClaimTransition,
) -> Result<Option<LinkClaim>> {
    let permitted = transition
        .permitted_source_states()
        .iter()
        .map(|state| state.as_str())
        .collect::<Vec<_>>();
    let (candidate_credential_ref, sync_run_uid) = match &transition {
        LinkClaimTransition::CredentialWritten {
            candidate_credential_ref,
        } => (Some(candidate_credential_ref.clone()), None),
        LinkClaimTransition::SyncRunClaimed { sync_run_uid }
        | LinkClaimTransition::Finalized { sync_run_uid } => (None, Some(*sync_run_uid)),
        LinkClaimTransition::Compensating | LinkClaimTransition::Compensated => (None, None),
    };

    let mut conn = repository.begin().await?;
    // Compare-and-swap on the source state: a claim that already moved on is not
    // updated, so a replayed or concurrent link observes the loss instead of
    // rewriting a newer state.
    let row = sqlx::query(&format!(
        r#"
        UPDATE moa.knowledge_link_claims
        SET state = $3,
            candidate_credential_ref = COALESCE($4, candidate_credential_ref),
            sync_run_uid = COALESCE($5, sync_run_uid),
            updated_at = now()
        WHERE tenant_id = $1
          AND operation_id = $2
          AND state = ANY($6::TEXT[])
        RETURNING {CLAIM_COLUMNS}
        "#
    ))
    .bind(tenant_id.0)
    .bind(operation_id)
    .bind(transition.target_state().as_str())
    .bind(candidate_credential_ref)
    .bind(sync_run_uid)
    .bind(permitted)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;

    row.as_ref().map(link_claim_from_row).transpose()
}

pub(super) async fn get_link_claim(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    operation_id: &str,
) -> Result<Option<LinkClaim>> {
    let mut conn = repository.begin().await?;
    let row = sqlx::query(&format!(
        r#"
        SELECT {CLAIM_COLUMNS}
        FROM moa.knowledge_link_claims
        WHERE tenant_id = $1 AND operation_id = $2
        "#
    ))
    .bind(tenant_id.0)
    .bind(operation_id)
    .fetch_optional(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;
    row.as_ref().map(link_claim_from_row).transpose()
}

pub(super) async fn purge_tenant_link_claims(
    repository: &PostgresKnowledgeRepository,
    limit: u32,
) -> Result<u64> {
    let mut conn = repository.begin().await?;
    // Forced RLS already pins this to the scoped tenant; the explicit predicate
    // keeps the intent readable and survives a policy edit.
    let removed = sqlx::query(
        r#"
        DELETE FROM moa.knowledge_link_claims
        WHERE (tenant_id, operation_id) IN (
            SELECT tenant_id, operation_id
            FROM moa.knowledge_link_claims
            WHERE tenant_id = $1
            ORDER BY created_at
            LIMIT $2
        )
        "#,
    )
    .bind(repository.scoped_tenant_id().0)
    .bind(i64::from(limit.max(1)))
    .execute(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?
    .rows_affected();
    conn.commit().await.map_err(map_moa_error)?;
    Ok(removed)
}
