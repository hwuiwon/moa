//! Memory-owned privacy erasure helpers.

use moa_core::{ContactId, MoaError, ScopeContext, ScopedConn, TenantId};
use moa_memory_graph::{
    AgeGraphStore, ChangelogRecord, write::hard_purge_with_audit, write_and_bump,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const CONTACT_SUBJECT_PREFIX: &str = "contact:";
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
    /// Workspace containing subject data.
    pub workspace_id: String,
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

/// Enumerates active graph-memory nodes attributable to one workspace-bound subject user.
pub async fn enumerate_erase_candidates(
    pool: &PgPool,
    workspace_id: &str,
    subject_user_id: &str,
) -> Result<Vec<EraseCandidate>> {
    let mut tx = begin_app_scoped_tx(pool, workspace_id, subject_user_id).await?;
    let rows = sqlx::query_as::<_, EraseCandidate>(
        r#"
        SELECT uid, label, name, pii_class
        FROM moa.node_index
        WHERE workspace_id = $1
          AND valid_to IS NULL
          AND (
              user_id = $2
              OR properties_summary->>'user_id' = $2
          )
        ORDER BY uid
        "#,
    )
    .bind(workspace_id)
    .bind(subject_user_id)
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

    let graph = erase_graph_store(pool, &audit.workspace_id, &audit.subject_user_id)?;
    let redaction_marker = format!("erase:{}", audit.approval_token_jti);
    let mut erased_count = 0usize;
    for chunk in candidates.chunks(ERASE_CHUNK_SIZE) {
        for candidate in chunk {
            hard_purge_with_audit(
                &graph,
                candidate.uid,
                &redaction_marker,
                Some(erase_audit_metadata(audit)),
            )
            .await?;
            erased_count = erased_count.saturating_add(1);
        }
    }
    emit_erase_summary(pool, audit, erased_count).await?;
    Ok(erased_count)
}

/// Begins a transaction scoped as `moa_app` for one tenant-bound contact subject.
pub async fn begin_app_scoped_tx<'a>(
    pool: &'a PgPool,
    workspace_id: &str,
    subject_user_id: &str,
) -> Result<ScopedConn<'a>> {
    let scope = contact_scope_from_subject(workspace_id, subject_user_id)?;
    let mut tx = ScopedConn::begin(pool, &scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(tx.as_mut())
        .await?;
    Ok(tx)
}

fn erase_graph_store(
    pool: &PgPool,
    workspace_id: &str,
    subject_user_id: &str,
) -> Result<AgeGraphStore> {
    let scope = contact_scope_from_subject(workspace_id, subject_user_id)?;
    Ok(AgeGraphStore::scoped_for_app_role(pool.clone(), scope))
}

fn contact_scope_from_subject(workspace_id: &str, subject_user_id: &str) -> Result<ScopeContext> {
    let tenant_id = Uuid::parse_str(workspace_id).map(TenantId::from).map_err(|error| {
        ErasureError::Scope(MoaError::ValidationError(format!(
            "privacy erasure workspace_id must be a tenant UUID for tenant-scoped memory: {error}"
        )))
    })?;
    let contact_subject = subject_user_id
        .strip_prefix(CONTACT_SUBJECT_PREFIX)
        .unwrap_or(subject_user_id);
    let contact_id = Uuid::parse_str(contact_subject)
        .map(ContactId)
        .map_err(|error| {
            ErasureError::Scope(MoaError::ValidationError(format!(
                "privacy erasure subject_user_id must be a contact UUID or contact:<UUID> for contact-scoped memory: {error}"
            )))
        })?;
    Ok(ScopeContext::contact(tenant_id, contact_id))
}

fn erase_audit_metadata(audit: &GraphErasureAudit) -> Value {
    json!({
        "reason": audit.reason.as_str(),
        "approver_id": audit.approver_id.as_str(),
        "approval_token_jti": audit.approval_token_jti.as_str(),
        "subject_user_id": audit.subject_user_id.as_str(),
        "workspace_id": audit.workspace_id.as_str(),
        "op": "erase",
    })
}

async fn emit_erase_summary(
    pool: &PgPool,
    audit: &GraphErasureAudit,
    erased_count: usize,
) -> Result<()> {
    let mut tx = begin_app_scoped_tx(pool, &audit.workspace_id, &audit.subject_user_id).await?;
    write_and_bump(
        tx.as_mut(),
        ChangelogRecord {
            workspace_id: Some(audit.workspace_id.clone()),
            user_id: None,
            scope: "tenant".to_string(),
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
                "workspace_id": audit.workspace_id.as_str(),
                "op": "erase",
            })),
            cause_change_id: None,
        },
    )
    .await?;
    tx.commit().await?;
    Ok(())
}
