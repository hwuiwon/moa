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
    parent_created_by_claim, credential_expected_generation,
    credential_ownership, candidate_credential_ref, previous_vault_credential_ref,
    state, sync_run_uid,
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
            state
        )
        SELECT $1, $2, $3, $4, $5, 'reserved'
        WHERE $4::UUID IS NOT NULL
        ON CONFLICT DO NOTHING
        RETURNING {CLAIM_COLUMNS}
        "#
    ))
    .bind(claim.tenant_id.0)
    .bind(&claim.operation_id)
    .bind(&claim.request_hash)
    .bind(claim.owner_identity_id)
    .bind(claim.connection_uid)
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
    if let Some(row) = existing {
        let existing = link_claim_from_row(&row)?;
        conn.commit().await.map_err(map_moa_error)?;
        // The hash covers the operation's selector and the connection it claims, so
        // a reused id whose inputs changed is a conflict rather than a resume that
        // would silently adopt another link's connection.
        if existing.request_hash != claim.request_hash
            || existing.connection_uid != claim.connection_uid
        {
            return Ok(LinkClaimReservation::Conflict);
        }
        return Ok(LinkClaimReservation::Existing(existing));
    }

    // The partial unique fence permits only one non-terminal link per
    // connection. Inspect it after the insert loses so callers get a typed
    // serialization result instead of a storage error. Terminal claims do not
    // match this predicate and therefore never block a later relink.
    let connection_busy = sqlx::query_scalar::<_, bool>(
        r#"
        SELECT EXISTS (
            SELECT 1
            FROM moa.knowledge_link_claims
            WHERE tenant_id = $1
              AND connection_uid = $2
              AND state NOT IN ('finalized', 'compensated')
        )
        "#,
    )
    .bind(claim.tenant_id.0)
    .bind(claim.connection_uid)
    .fetch_one(conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await.map_err(map_moa_error)?;

    if connection_busy {
        return Ok(LinkClaimReservation::ConnectionBusy);
    }
    if claim.owner_identity_id.is_none() {
        return Ok(LinkClaimReservation::OwnerRequired);
    }
    Err(Error::Repository(
        "knowledge link claim vanished between insert and read".to_string(),
    ))
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
    let (
        parent_created_by_claim,
        credential_expected_generation,
        credential_ownership,
        candidate_credential_ref,
        previous_vault_credential_ref,
        sync_run_uid,
    ) = match &transition {
        LinkClaimTransition::ParentClaimed {
            parent_created_by_claim,
            credential_expected_generation,
        } => (
            Some(*parent_created_by_claim),
            Some(i64::try_from(*credential_expected_generation).map_err(map_int_error)?),
            None,
            None,
            None,
            None,
        ),
        LinkClaimTransition::CredentialWritten {
            credential_ownership,
            candidate_credential_ref,
            previous_vault_credential_ref,
        } => (
            None,
            None,
            Some(credential_ownership.as_str()),
            candidate_credential_ref.clone(),
            previous_vault_credential_ref.clone(),
            None,
        ),
        LinkClaimTransition::SyncRunClaimed { sync_run_uid }
        | LinkClaimTransition::Finalized { sync_run_uid } => {
            (None, None, None, None, None, Some(*sync_run_uid))
        }
        LinkClaimTransition::Compensating | LinkClaimTransition::Compensated => {
            (None, None, None, None, None, None)
        }
    };

    let mut conn = repository.begin().await?;
    // Compare-and-swap on the source state: a claim that already moved on is not
    // updated, so a replayed or concurrent link observes the loss instead of
    // rewriting a newer state.
    let row = sqlx::query(&format!(
        r#"
        UPDATE moa.knowledge_link_claims
        SET state = $3,
            parent_created_by_claim = COALESCE($4, parent_created_by_claim),
            credential_expected_generation = COALESCE($5, credential_expected_generation),
            credential_ownership = COALESCE($6, credential_ownership),
            candidate_credential_ref = COALESCE($7, candidate_credential_ref),
            previous_vault_credential_ref = COALESCE($8, previous_vault_credential_ref),
            sync_run_uid = COALESCE($9, sync_run_uid),
            updated_at = now()
        WHERE tenant_id = $1
          AND operation_id = $2
          AND state = ANY($10::TEXT[])
        RETURNING {CLAIM_COLUMNS}
        "#
    ))
    .bind(tenant_id.0)
    .bind(operation_id)
    .bind(transition.target_state().as_str())
    .bind(parent_created_by_claim)
    .bind(credential_expected_generation)
    .bind(credential_ownership)
    .bind(candidate_credential_ref)
    .bind(previous_vault_credential_ref)
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
