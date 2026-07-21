//! Memory-owned privacy erasure helpers.

use moa_core::types::memory::RlsContext;
use moa_core::{
    types::contact::ContactId, types::identifiers::StoragePartitionId, types::identifiers::TenantId,
};
use moa_db::ScopedConn;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// Result type returned by memory erasure helpers.
pub type Result<T> = std::result::Result<T, ErasureError>;

/// Errors returned by memory erasure helpers.
#[derive(Debug, thiserror::Error)]
pub enum ErasureError {
    /// Graph-memory operation failed.
    #[error("graph erasure: {0}")]
    Graph(#[from] moa_memory_graph::Error),
    /// Scoped transaction setup failed.
    #[error("scoped erasure transaction: {0}")]
    Scope(#[from] moa_core::error::MoaError),
    /// SQL operation failed.
    #[error("erasure sql: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Cryptographic erasure (crypto-shred) of a subject KEK failed.
    #[error("crypto-shred: {0}")]
    Crypto(#[from] moa_crypto::Error),
}

/// One graph-memory candidate selected for privacy erasure.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct EraseCandidate {
    /// Candidate node UID.
    pub uid: Uuid,
    /// Candidate graph label.
    pub label: String,
    /// Candidate node name.
    pub name: String,
    /// Candidate PII class.
    pub pii_class: String,
}

/// Audit metadata required for a scoped graph erasure.
#[derive(Debug, Clone)]
pub struct GraphErasureAudit {
    /// Tenant containing subject data.
    pub tenant_id: TenantId,
    /// Subject user UUID.
    pub subject_user: Uuid,
    /// Subject user id as stored in text columns.
    pub subject_user_id: String,
    /// Administrative reason for the erasure.
    pub reason: String,
    /// Approving administrator subject identifier.
    pub approver_id: String,
    /// Unique approval-token identifier consumed for this erasure.
    pub approval_token_jti: String,
}

/// Enumerates all graph-memory versions attributable to one tenant-bound subject user.
pub async fn enumerate_erase_candidates(
    pool: &PgPool,
    tenant_id: TenantId,
    subject_user_id: &str,
) -> Result<Vec<EraseCandidate>> {
    let subject_id = contact_id_from_subject(subject_user_id)?.0;
    let mut tx = begin_app_scoped_tx(pool, tenant_id, subject_user_id).await?;
    let rows = sqlx::query_as::<_, EraseCandidate>(
        r#"
        SELECT uid, label, name, pii_class
        FROM moa.node_index
        WHERE tenant_id = $1
          AND data_subject_id = $2
        ORDER BY uid
        "#,
    )
    .bind(tenant_id.0)
    .bind(subject_id)
    .fetch_all(tx.as_mut())
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Hard-purges selected graph-memory candidates and writes the summary erase changelog.
pub async fn hard_purge_erase_candidates(
    pool: &PgPool,
    audit: &GraphErasureAudit,
    _candidates: &[EraseCandidate],
) -> Result<usize> {
    let subject_id = contact_id_from_subject(&audit.subject_user_id)?.0;
    if subject_id != audit.subject_user {
        return Err(ErasureError::Scope(
            moa_core::error::MoaError::ValidationError(
                "privacy erasure audit subject UUID does not match subject_user_id".to_string(),
            ),
        ));
    }
    let mut tx = begin_app_scoped_tx(pool, audit.tenant_id, &audit.subject_user_id).await?;
    let result: Value = sqlx::query_scalar("SELECT moa.erase_memory_data_subject($1, $2, $3)")
        .bind(audit.tenant_id.0)
        .bind(subject_id)
        .bind(erase_audit_metadata(audit))
        .fetch_one(tx.as_mut())
        .await?;
    tx.commit().await?;
    let erased = result
        .get("nodes_deleted")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ErasureError::Scope(moa_core::error::MoaError::StorageError(
                "privacy erasure function returned no nodes_deleted count".to_string(),
            ))
        })?;
    usize::try_from(erased).map_err(|error| {
        ErasureError::Scope(moa_core::error::MoaError::StorageError(format!(
            "privacy erasure node count overflow: {error}"
        )))
    })
}

/// Cryptographically erases one data subject's key-encryption key as erasure
/// defense-in-depth.
///
/// This complements [`hard_purge_erase_candidates`]. Hard-purge deletes the live
/// graph rows, but a restricted/PHI node's sealed content can also survive in
/// backups, read replicas, or WAL that a `DELETE` cannot reach. Destroying the
/// subject's per-subject KEK ([`moa_crypto::crypto_shred_subject`]) makes every
/// record sealed under it permanently unrecoverable everywhere, without touching
/// any ciphertext. It is idempotent — shredding an already-shredded or
/// never-sealed subject succeeds — and leaves every other subject in the tenant
/// decrypting normally.
///
/// The KEK is keyed by `(tenant_id, subject_id)` exactly as the write path sealed
/// it, where `subject_id` is the contact/data-subject UUID.
pub async fn crypto_shred_erased_subject(
    pool: &PgPool,
    kms: &dyn moa_crypto::KeyManagementProvider,
    tenant_id: TenantId,
    subject_id: Uuid,
    operation_id: &str,
) -> Result<()> {
    // The fence is rechecked immediately before the irreversible KMS call. The
    // transaction advisory guard stays held until the provider confirms shred,
    // so a concurrent hold cannot cross this boundary on another replica.
    let guard = crate::legal_hold::begin_destruction_stage_guard(
        pool,
        tenant_id,
        &[subject_id],
        operation_id,
    )
    .await
    .map_err(|error| {
        ErasureError::Scope(moa_core::error::MoaError::StorageError(error.to_string()))
    })?;
    moa_crypto::crypto_shred_subject(kms, tenant_id.0, subject_id).await?;
    guard.finish().await.map_err(|error| {
        ErasureError::Scope(moa_core::error::MoaError::StorageError(error.to_string()))
    })?;
    Ok(())
}

/// Deletes the subject's standing memory-digest rows during privacy erasure.
///
/// A contact whose active facts are all erased forms zero digest groups, so the
/// lifecycle rebuild never reaches that identity and its rendered digest text
/// would otherwise survive and be re-injected into prompts. This closes the
/// contact-scoped digest directly, mirroring the full-tenant purge.
pub async fn delete_subject_digests(
    pool: &PgPool,
    tenant_id: TenantId,
    subject_user_id: &str,
) -> Result<u64> {
    let contact_id = contact_id_from_subject(subject_user_id)?;
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let mut tx = begin_app_scoped_tx(pool, tenant_id, subject_user_id).await?;
    let deleted = sqlx::query(
        r#"
        DELETE FROM moa.memory_digests
        WHERE storage_partition_id = $1
          AND contact_id = $2
        "#,
    )
    .bind(storage_partition_id)
    .bind(contact_id.0)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(deleted)
}

/// Deletes the subject's retrieval-lineage rows during privacy erasure.
///
/// Retrieval-lineage rows carry attributable subject/session/UID/rank/time
/// provenance and their `uid` has no foreign key to `node_index`, so purging
/// graph nodes never removes them. They belong in erasure closure.
pub async fn delete_subject_retrieval_lineage(
    pool: &PgPool,
    tenant_id: TenantId,
    subject_user_id: &str,
) -> Result<u64> {
    let contact_id = contact_id_from_subject(subject_user_id)?;
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let mut tx = begin_app_scoped_tx(pool, tenant_id, subject_user_id).await?;
    let deleted = sqlx::query(
        r#"
        DELETE FROM moa.retrieval_lineage
        WHERE storage_partition_id = $1
          AND contact_id = $2
        "#,
    )
    .bind(storage_partition_id)
    .bind(contact_id.0)
    .execute(tx.as_mut())
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(deleted)
}

/// Begins a transaction scoped as `moa_app` for one tenant-bound contact subject.
pub async fn begin_app_scoped_tx<'a>(
    pool: &'a PgPool,
    tenant_id: TenantId,
    subject_user_id: &str,
) -> Result<ScopedConn<'a>> {
    let scope = contact_scope_from_subject(tenant_id, subject_user_id)?;
    let tx = ScopedConn::begin_as_app(pool, &scope, true).await?;
    Ok(tx)
}

fn contact_scope_from_subject(tenant_id: TenantId, subject_user_id: &str) -> Result<RlsContext> {
    Ok(RlsContext::contact(
        tenant_id,
        contact_id_from_subject(subject_user_id)?,
    ))
}

fn contact_id_from_subject(subject_user_id: &str) -> Result<ContactId> {
    moa_wire::privacy::ParsedPrivacySubjectId::parse_str(subject_user_id)
        .map(moa_wire::privacy::ParsedPrivacySubjectId::contact_id)
        .map_err(|error| {
            ErasureError::Scope(moa_core::error::MoaError::ValidationError(format!(
                "privacy erasure subject_user_id must be a contact UUID or contact:<UUID> for contact-scoped memory: {error}"
            )))
        })
}

fn erase_audit_metadata(audit: &GraphErasureAudit) -> Value {
    json!({
        "reason": audit.reason.as_str(),
        "approver_id": audit.approver_id.as_str(),
        "approval_token_jti": audit.approval_token_jti.as_str(),
        "subject_user_id": audit.subject_user_id.as_str(),
        "tenant_id": audit.tenant_id.to_string(),
        "op": "erase",
    })
}
