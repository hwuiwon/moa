//! Memory-owned privacy erasure helpers.

use moa_core::RlsContext;
use moa_core::{ContactId, StoragePartitionId, TenantId};
use moa_db::ScopedConn;
use moa_memory_graph::{
    ChangelogRecord, PostgresGraphStore, write::hard_purge_with_audit, write_and_bump,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const ERASE_CHUNK_SIZE: usize = 1000;

/// Result type returned by memory erasure helpers.
pub type Result<T> = std::result::Result<T, ErasureError>;

/// Errors returned by memory erasure helpers.
#[derive(Debug, thiserror::Error)]
pub enum ErasureError {
    /// Graph-memory operation failed.
    #[error("graph erasure: {0}")]
    Graph(#[from] moa_memory_graph::GraphError),
    /// Scoped transaction setup failed.
    #[error("scoped erasure transaction: {0}")]
    Scope(#[from] moa_core::MoaError),
    /// SQL operation failed.
    #[error("erasure sql: {0}")]
    Sqlx(#[from] sqlx::Error),
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
    let storage_partition_id = StoragePartitionId::for_tenant(tenant_id).to_string();
    let contact_user_id = contact_id_from_subject(subject_user_id)?.to_string();
    let mut tx = begin_app_scoped_tx(pool, tenant_id, subject_user_id).await?;
    let rows = sqlx::query_as::<_, EraseCandidate>(
        r#"
        SELECT uid, label, name, pii_class
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND (
              user_id = $2
              OR properties_summary->>'user_id' = $2
              OR user_id = $3
              OR properties_summary->>'user_id' = $3
          )
        ORDER BY uid
        "#,
    )
    .bind(storage_partition_id)
    .bind(subject_user_id)
    .bind(contact_user_id)
    .fetch_all(tx.as_mut())
    .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Hard-purges selected graph-memory candidates and writes the summary erase changelog.
pub async fn hard_purge_erase_candidates(
    pool: &PgPool,
    audit: &GraphErasureAudit,
    candidates: &[EraseCandidate],
) -> Result<usize> {
    if candidates.is_empty() {
        return Ok(0);
    }

    let graph = erase_graph_store(pool, audit.tenant_id, &audit.subject_user_id)?;
    let redaction_marker = format!("erase:{}", audit.approval_token_jti);
    let mut erased_count = 0usize;
    for chunk in candidates.chunks(ERASE_CHUNK_SIZE) {
        for candidate in chunk {
            match hard_purge_with_audit(
                &graph,
                candidate.uid,
                &redaction_marker,
                Some(erase_audit_metadata(audit)),
            )
            .await
            {
                Ok(()) => {}
                // Idempotent erasure: a node already absent — because a
                // concurrent purge removed it or a resumed job re-enumerated
                // remaining candidates — is completed progress, not a failure.
                // Restart-after-partial-progress must not terminate on the first
                // already-purged node.
                Err(moa_memory_graph::GraphError::NotFound(_)) => {
                    tracing::debug!(
                        uid = %candidate.uid,
                        "erase candidate already absent; treating as purged"
                    );
                }
                Err(error) => return Err(error.into()),
            }
            erased_count = erased_count.saturating_add(1);
        }
    }
    emit_erase_summary(pool, audit, erased_count).await?;
    Ok(erased_count)
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

fn erase_graph_store(
    pool: &PgPool,
    tenant_id: TenantId,
    subject_user_id: &str,
) -> Result<PostgresGraphStore> {
    let scope = contact_scope_from_subject(tenant_id, subject_user_id)?;
    Ok(PostgresGraphStore::scoped_for_app_role(pool.clone(), scope))
}

fn contact_scope_from_subject(tenant_id: TenantId, subject_user_id: &str) -> Result<RlsContext> {
    Ok(RlsContext::contact(
        tenant_id,
        contact_id_from_subject(subject_user_id)?,
    ))
}

fn contact_id_from_subject(subject_user_id: &str) -> Result<ContactId> {
    moa_core::wire::privacy::ParsedPrivacySubjectId::parse_str(subject_user_id)
        .map(moa_core::wire::privacy::ParsedPrivacySubjectId::contact_id)
        .map_err(|error| {
            ErasureError::Scope(moa_core::MoaError::ValidationError(format!(
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

async fn emit_erase_summary(
    pool: &PgPool,
    audit: &GraphErasureAudit,
    erased_count: usize,
) -> Result<()> {
    let storage_partition_id = StoragePartitionId::for_tenant(audit.tenant_id).to_string();
    let contact_id = contact_id_from_subject(&audit.subject_user_id)?;
    let mut tx = begin_app_scoped_tx(pool, audit.tenant_id, &audit.subject_user_id).await?;
    write_and_bump(
        tx.as_mut(),
        ChangelogRecord {
            storage_partition_id: Some(storage_partition_id),
            contact_id: Some(contact_id.to_string()),
            scope: "contact".to_string(),
            actor_id: Some(audit.approver_id.clone()),
            actor_kind: "admin".to_string(),
            op: "erase".to_string(),
            target_kind: "contact".to_string(),
            target_label: "User".to_string(),
            target_uid: audit.subject_user,
            payload: json!({
                "reason": audit.reason.as_str(),
                "subject_user_id": audit.subject_user_id.as_str(),
                "erased_count": erased_count,
            }),
            redaction_marker: None,
            pii_class: "phi".to_string(),
            audit_metadata: Some(json!({
                "approver_id": audit.approver_id.as_str(),
                "approval_token_jti": audit.approval_token_jti.as_str(),
                "subject_user_id": audit.subject_user_id.as_str(),
                "tenant_id": audit.tenant_id.to_string(),
                "op": "erase",
            })),
            cause_change_id: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
