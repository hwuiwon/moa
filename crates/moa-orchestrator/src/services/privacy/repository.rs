//! SQL-backed privacy subject resolution and export read models.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use moa_core::{
    types::contact::ContactId, types::identifiers::StoragePartitionId, types::identifiers::UserId,
};
use moa_wire::privacy::{ParsedPrivacySubjectId, contact_privacy_subject_string};
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::Value;
use sqlx::{PgPool, Row};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use super::approval::{ApprovalClaims, ensure_jti_inserted};
use super::context::{PrivacyExportContext, PrivacySubject};

/// Contact-link expansion policy for privacy subject resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ContactLinkedSubjectPolicy {
    /// Resolve only the requested subject.
    SpecifiedOnly,
    /// Include verified contacts that were merged into the requested contact.
    IncludeVerifiedLinks,
}

/// Effective privacy subject kind after resolving a user/contact subject id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PrivacySubjectKind {
    /// Ordinary user subject.
    User,
    /// Contact subject.
    Contact,
}

/// Resolved privacy subjects and effective storage partition.
#[derive(Debug, Clone)]
pub(super) struct ResolvedPrivacySubjects {
    /// Subject kind used for erasure-scope validation.
    pub(super) kind: PrivacySubjectKind,
    /// Storage partition that should constrain reads, if any.
    pub(super) effective_storage_partition: Option<String>,
    /// Primary and linked subjects included in the operation.
    pub(super) subjects: Vec<PrivacySubject>,
}

/// Records an approval-token JTI and rejects token replay.
///
/// Used by the single-shot export path. Erasure uses [`claim_erasure_job`],
/// which owns the JTI through a durable, resumable job instead of a single-use
/// ledger insert.
pub(super) async fn consume_approval_jti(
    pool: &PgPool,
    tenant_id: Uuid,
    claims: &ApprovalClaims,
) -> Result<(), HandlerError> {
    let expires_at = jti_expires_at(claims)?;
    let inserted = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO moa.audit_jti_used
            (jti, tenant_id, op, subject_user_id, approver_id, approval_claims, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (jti) DO NOTHING
        RETURNING jti
        "#,
    )
    .bind(&claims.jti)
    .bind(tenant_id)
    .bind(&claims.op)
    .bind(&claims.subject_user_id)
    .bind(&claims.sub)
    .bind(serde_json::to_value(claims).map_err(handler_error)?)
    .bind(expires_at)
    .fetch_optional(pool)
    .await
    .map_err(handler_error)?;
    ensure_jti_inserted(inserted.as_deref())
}

fn jti_expires_at(claims: &ApprovalClaims) -> Result<chrono::DateTime<Utc>, HandlerError> {
    Utc.timestamp_opt(claims.exp, 0).single().ok_or_else(|| {
        TerminalError::new_with_code(400, "approval token exp is out of range").into()
    })
}

/// Ordered stages of a durable, resumable erasure job.
///
/// A resumed job jumps straight to the persisted stage and runs the remaining
/// stages in order, so each stage runs at most once per completed job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ErasureJobStage {
    /// PII vault subject keys not yet erased.
    Vault,
    /// Graph-memory candidates not yet purged.
    Graph,
    /// Standing memory-digest rows not yet deleted.
    Digest,
    /// Retrieval-lineage rows not yet deleted.
    Lineage,
    /// All stages complete.
    Done,
}

impl ErasureJobStage {
    fn as_str(self) -> &'static str {
        match self {
            Self::Vault => "vault",
            Self::Graph => "graph",
            Self::Digest => "digest",
            Self::Lineage => "lineage",
            Self::Done => "done",
        }
    }

    fn parse(raw: &str) -> Result<Self, HandlerError> {
        match raw {
            "vault" => Ok(Self::Vault),
            "graph" => Ok(Self::Graph),
            "digest" => Ok(Self::Digest),
            "lineage" => Ok(Self::Lineage),
            "done" => Ok(Self::Done),
            other => Err(TerminalError::new(format!("unknown erasure job stage: {other}")).into()),
        }
    }
}

/// Accumulated per-store progress persisted on a durable erasure job.
#[derive(Debug, Clone, Copy)]
pub(super) struct ErasureJobProgress {
    /// Stage the job should run next.
    pub(super) stage: ErasureJobStage,
    /// PII vault rows erased so far.
    pub(super) pii_vault_erased: u64,
    /// Graph-memory nodes purged so far.
    pub(super) graph_erased: u64,
    /// Standing memory-digest rows deleted so far.
    pub(super) digest_deleted: u64,
    /// Retrieval-lineage rows deleted so far.
    pub(super) lineage_deleted: u64,
}

/// A durable erasure job after claiming or resuming ownership of its JTI.
#[derive(Debug, Clone, Copy)]
pub(super) struct ClaimedErasureJob {
    /// True when this call inserted the job (rather than resuming an existing one).
    pub(super) fresh: bool,
    /// Whether the job already reached the completed terminal state.
    pub(super) completed: bool,
    /// Original candidate count enumerated when the job was first claimed.
    pub(super) candidate_count: u64,
    /// Resumable progress, seeded from the persisted counters.
    pub(super) progress: ErasureJobProgress,
}

#[derive(Debug, sqlx::FromRow)]
struct ErasureJobRow {
    request_fingerprint: String,
    status: String,
    stage: String,
    candidate_count: i64,
    pii_vault_erased: i64,
    graph_erased: i64,
    digest_deleted: i64,
    lineage_deleted: i64,
}

impl ErasureJobRow {
    fn into_claimed(self, fresh: bool) -> Result<ClaimedErasureJob, HandlerError> {
        Ok(ClaimedErasureJob {
            fresh,
            completed: self.status == "completed",
            candidate_count: nonneg_u64(self.candidate_count),
            progress: ErasureJobProgress {
                stage: ErasureJobStage::parse(&self.stage)?,
                pii_vault_erased: nonneg_u64(self.pii_vault_erased),
                graph_erased: nonneg_u64(self.graph_erased),
                digest_deleted: nonneg_u64(self.digest_deleted),
                lineage_deleted: nonneg_u64(self.lineage_deleted),
            },
        })
    }
}

fn nonneg_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn u64_to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

const ERASURE_JOB_RETURNING: &str = "jti, request_fingerprint, status, stage, candidate_count, \
     pii_vault_erased, graph_erased, digest_deleted, lineage_deleted";

/// Atomically binds an approval JTI to one idempotent, resumable erasure job.
///
/// A fresh request inserts the job and owns the JTI. A replay of the *same*
/// request (matching `request_fingerprint`) resumes from the persisted stage
/// instead of failing on the already-consumed token. A reuse of the token for a
/// *different* request is rejected so approval tokens never become generally
/// reusable. The audit-JTI ledger is written idempotently in the same
/// transaction so both export and erase share one consumed-token record.
pub(super) async fn claim_erasure_job(
    pool: &PgPool,
    claims: &ApprovalClaims,
    request_fingerprint: &str,
    tenant_id: Uuid,
    candidate_count: u64,
) -> Result<ClaimedErasureJob, HandlerError> {
    let expires_at = jti_expires_at(claims)?;
    let claims_json = serde_json::to_value(claims).map_err(handler_error)?;
    let mut tx = pool.begin().await.map_err(handler_error)?;

    sqlx::query(
        r#"
        INSERT INTO moa.audit_jti_used
            (jti, tenant_id, op, subject_user_id, approver_id, approval_claims, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (jti) DO NOTHING
        "#,
    )
    .bind(&claims.jti)
    .bind(tenant_id)
    .bind(&claims.op)
    .bind(&claims.subject_user_id)
    .bind(&claims.sub)
    .bind(&claims_json)
    .bind(expires_at)
    .execute(tx.as_mut())
    .await
    .map_err(handler_error)?;

    let inserted = sqlx::query_as::<_, ErasureJobRow>(&format!(
        r#"
        INSERT INTO moa.erasure_jobs
            (jti, tenant_id, subject_user_id, request_fingerprint, approver_id,
             approval_claims, candidate_count)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        ON CONFLICT (jti) DO NOTHING
        RETURNING {ERASURE_JOB_RETURNING}
        "#
    ))
    .bind(&claims.jti)
    .bind(tenant_id)
    .bind(&claims.subject_user_id)
    .bind(request_fingerprint)
    .bind(&claims.sub)
    .bind(&claims_json)
    .bind(u64_to_i64(candidate_count))
    .fetch_optional(tx.as_mut())
    .await
    .map_err(handler_error)?;

    let claimed = if let Some(row) = inserted {
        row.into_claimed(true)?
    } else {
        let existing = sqlx::query_as::<_, ErasureJobRow>(&format!(
            "SELECT {ERASURE_JOB_RETURNING} FROM moa.erasure_jobs WHERE jti = $1"
        ))
        .bind(&claims.jti)
        .fetch_one(tx.as_mut())
        .await
        .map_err(handler_error)?;
        if existing.request_fingerprint != request_fingerprint {
            return Err(TerminalError::new_with_code(
                409,
                "approval token replayed for a different erasure request",
            )
            .into());
        }
        existing.into_claimed(false)?
    };

    tx.commit().await.map_err(handler_error)?;
    Ok(claimed)
}

/// Persists resumable per-store progress for an in-flight erasure job.
pub(super) async fn save_erasure_job_progress(
    pool: &PgPool,
    jti: &str,
    progress: &ErasureJobProgress,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        UPDATE moa.erasure_jobs
        SET stage = $2,
            pii_vault_erased = $3,
            graph_erased = $4,
            digest_deleted = $5,
            lineage_deleted = $6,
            updated_at = now()
        WHERE jti = $1
        "#,
    )
    .bind(jti)
    .bind(progress.stage.as_str())
    .bind(u64_to_i64(progress.pii_vault_erased))
    .bind(u64_to_i64(progress.graph_erased))
    .bind(u64_to_i64(progress.digest_deleted))
    .bind(u64_to_i64(progress.lineage_deleted))
    .execute(pool)
    .await
    .map_err(handler_error)?;
    Ok(())
}

/// Marks an erasure job completed after every store reached its erased state.
pub(super) async fn complete_erasure_job(
    pool: &PgPool,
    jti: &str,
    progress: &ErasureJobProgress,
) -> Result<(), HandlerError> {
    sqlx::query(
        r#"
        UPDATE moa.erasure_jobs
        SET status = 'completed',
            stage = 'done',
            pii_vault_erased = $2,
            graph_erased = $3,
            digest_deleted = $4,
            lineage_deleted = $5,
            completed_at = now(),
            updated_at = now()
        WHERE jti = $1
        "#,
    )
    .bind(jti)
    .bind(u64_to_i64(progress.pii_vault_erased))
    .bind(u64_to_i64(progress.graph_erased))
    .bind(u64_to_i64(progress.digest_deleted))
    .bind(u64_to_i64(progress.lineage_deleted))
    .execute(pool)
    .await
    .map_err(handler_error)?;
    Ok(())
}

pub(super) async fn resolve_privacy_subjects(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: Option<&StoragePartitionId>,
    requested_subject_user_id: &UserId,
    linked_policy: ContactLinkedSubjectPolicy,
) -> Result<ResolvedPrivacySubjects, HandlerError> {
    let parsed = parse_privacy_subject_id(requested_subject_user_id)?;
    let contact_row =
        load_privacy_contact_row(pool, tenant_id, storage_partition_id, parsed.uuid).await?;
    let Some(contact_row) = contact_row else {
        if parsed.is_contact() {
            return Err(TerminalError::new_with_code(404, "contact not found").into());
        }
        return Ok(ResolvedPrivacySubjects {
            kind: PrivacySubjectKind::User,
            effective_storage_partition: storage_partition_id.map(ToString::to_string),
            subjects: vec![PrivacySubject::primary(
                requested_subject_user_id.to_string(),
                parsed.uuid,
            )],
        });
    };

    let mut subjects = vec![PrivacySubject::primary(
        contact_privacy_subject_string(ContactId(contact_row.id)),
        contact_row.id,
    )];
    if contact_row.state == "verified"
        && linked_policy == ContactLinkedSubjectPolicy::IncludeVerifiedLinks
    {
        let linked = load_linked_contact_ids(
            pool,
            tenant_id,
            &contact_row.storage_partition_id,
            contact_row.id,
        )
        .await?;
        subjects.extend(linked.into_iter().map(PrivacySubject::linked_contact));
    }

    Ok(ResolvedPrivacySubjects {
        kind: PrivacySubjectKind::Contact,
        effective_storage_partition: Some(contact_row.storage_partition_id),
        subjects,
    })
}

pub(super) fn parse_privacy_subject_id(
    subject_user_id: &UserId,
) -> Result<ParsedPrivacySubjectId, HandlerError> {
    ParsedPrivacySubjectId::parse(subject_user_id)
        .map_err(|error| TerminalError::new_with_code(400, error.to_string()).into())
}

#[derive(Debug, Clone)]
struct PrivacyContactRow {
    id: Uuid,
    storage_partition_id: String,
    state: String,
}

async fn load_privacy_contact_row(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: Option<&StoragePartitionId>,
    contact_id: Uuid,
) -> Result<Option<PrivacyContactRow>, HandlerError> {
    let row = sqlx::query(
        r#"
        SELECT id, storage_partition_id, state
        FROM contacts
        WHERE id = $1
          AND tenant_id = $2
          AND ($3::text IS NULL OR storage_partition_id = $3)
        "#,
    )
    .bind(contact_id)
    .bind(tenant_id)
    .bind(storage_partition_id.map(StoragePartitionId::as_str))
    .fetch_optional(pool)
    .await
    .map_err(|error| db_handler_error("load privacy contact subject", error))?;
    row.map(|row| {
        Ok(PrivacyContactRow {
            id: row
                .try_get::<Uuid, _>("id")
                .map_err(|error| db_handler_error("read privacy contact id", error))?,
            storage_partition_id: row.try_get::<String, _>("storage_partition_id").map_err(
                |error| db_handler_error("read privacy contact storage partition", error),
            )?,
            state: row
                .try_get::<String, _>("state")
                .map_err(|error| db_handler_error("read privacy contact state", error))?,
        })
    })
    .transpose()
}

async fn load_linked_contact_ids(
    pool: &PgPool,
    tenant_id: Uuid,
    storage_partition_id: &str,
    contact_id: Uuid,
) -> Result<Vec<Uuid>, HandlerError> {
    sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT id
        FROM contacts
        WHERE canonical_contact_id = $1
          AND tenant_id = $2
          AND storage_partition_id = $3
        ORDER BY merged_at NULLS LAST, updated_at DESC, id
        "#,
    )
    .bind(contact_id)
    .bind(tenant_id)
    .bind(storage_partition_id)
    .fetch_all(pool)
    .await
    .map_err(|error| db_handler_error("load linked privacy contacts", error))
}

/// Collects privacy export data sections before README, audit, changelog, and manifest generation.
pub async fn collect_privacy_export_data_sections(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<BTreeMap<&'static str, usize>, HandlerError> {
    let mut counts = BTreeMap::new();
    counts.insert("facts", collect_facts(ctx, export_dir).await?);
    counts.insert("entities", collect_entities(ctx, export_dir).await?);
    counts.insert(
        "relationships",
        collect_relationships(ctx, export_dir).await?,
    );
    counts.insert("embeddings", collect_embeddings(ctx, export_dir).await?);
    counts.insert("skills", collect_skills(ctx, export_dir).await?);
    Ok(counts)
}

async fn collect_facts(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    collect_nodes(
        ctx,
        export_dir.join("facts.jsonl"),
        &["Fact", "Lesson", "Decision", "Incident"],
    )
    .await
}

async fn collect_entities(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    collect_nodes(
        ctx,
        export_dir.join("entities.jsonl"),
        &["Entity", "Concept", "Source"],
    )
    .await
}

async fn collect_nodes(
    ctx: &PrivacyExportContext,
    path: PathBuf,
    labels: &[&str],
) -> Result<usize, HandlerError> {
    let label_filter = labels
        .iter()
        .map(|label| (*label).to_string())
        .collect::<Vec<_>>();
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'uid', uid,
                    'label', label,
                    'storage_partition_id', storage_partition_id,
                    'user_id', user_id,
                    'scope', scope,
                    'name', name,
                    'properties_summary', properties_summary,
                    'pii_class', pii_class,
                    'confidence', confidence,
                    'valid_from', valid_from,
                    'valid_to', valid_to,
                    'created_at', created_at,
                    'last_accessed_at', last_accessed_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $4
                )
                FROM moa.node_index
                WHERE valid_to IS NULL
                  AND label = ANY($3)
                  AND ($1::text IS NULL OR storage_partition_id = $1)
                  AND (
                      user_id = $2
                      OR properties_summary->>'user_id' = $2
                      OR properties_summary::text LIKE ('%' || $2 || '%')
                  )
                ORDER BY storage_partition_id NULLS FIRST, label, name, uid
                "#,
            )
            .bind(ctx.storage_partition.as_deref())
            .bind(&subject.user_id)
            .bind(label_filter.clone())
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(path, &rows).await
}

async fn collect_relationships(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'change_id', change_id,
                    'storage_partition_id', storage_partition_id,
                    'user_id', user_id,
                    'scope', scope,
                    'actor_id', actor_id,
                    'actor_kind', actor_kind,
                    'op', op,
                    'target_kind', target_kind,
                    'target_label', target_label,
                    'target_uid', target_uid,
                    'payload', payload,
                    'pii_class', pii_class,
                    'audit_metadata', audit_metadata,
                    'cause_change_id', cause_change_id,
                    'created_at', created_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $3
                )
                FROM moa.graph_changelog
                WHERE target_kind = 'edge'
                  AND ($1::text IS NULL OR storage_partition_id = $1)
                  AND (
                      user_id = $2
                      OR actor_id = $2
                      OR payload::text LIKE ('%' || $2 || '%')
                      OR audit_metadata->>'subject_user_id' = $2
                  )
                ORDER BY created_at, change_id
                "#,
            )
            .bind(ctx.storage_partition.as_deref())
            .bind(&subject.user_id)
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(export_dir.join("relationships.jsonl"), &rows).await
}

async fn collect_embeddings(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'uid', e.uid,
                    'storage_partition_id', e.storage_partition_id,
                    'user_id', e.user_id,
                    'scope', e.scope,
                    'label', e.label,
                    'pii_class', e.pii_class,
                    'embedding_model', e.embedding_model,
                    'embedding_model_version', e.embedding_model_version,
                    'embedding', (e.embedding::text)::jsonb,
                    'valid_to', e.valid_to,
                    'created_at', e.created_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $3
                )
                FROM moa.embeddings e
                JOIN moa.node_index n ON n.uid = e.uid
                WHERE e.valid_to IS NULL
                  AND n.valid_to IS NULL
                  AND ($1::text IS NULL OR e.storage_partition_id = $1)
                  AND (
                      e.user_id = $2
                      OR n.user_id = $2
                      OR n.properties_summary->>'user_id' = $2
                      OR n.properties_summary::text LIKE ('%' || $2 || '%')
                  )
                ORDER BY e.storage_partition_id NULLS FIRST, e.label, e.uid
                "#,
            )
            .bind(ctx.storage_partition.as_deref())
            .bind(&subject.user_id)
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(export_dir.join("embeddings.jsonl"), &rows).await
}

async fn collect_skills(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'artifact_uid', a.artifact_uid,
                    'revision_uid', r.revision_uid,
                    'storage_partition_id', a.storage_partition_id,
                    'user_id', a.user_id,
                    'scope', a.scope,
                    'name', a.name,
                    'description', a.description,
                    'tags', a.tags,
                    'definition', r.definition,
                    'canonical_hash_hex', encode(r.canonical_hash, 'hex'),
                    'source_format', r.source_format,
                    'source_text_base64', encode(r.source_text, 'base64'),
                    'status', r.status,
                    'version', r.version,
                    'published_at', r.published_at,
                    'valid_to', r.valid_to,
                    'created_at', r.created_at,
                    'updated_at', r.updated_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $3,
                    'files', COALESCE((
                        SELECT jsonb_agg(jsonb_build_object(
                            'path', f.path,
                            'content_base64', encode(f.content, 'base64'),
                            'content_sha256_hex', encode(f.content_sha256, 'hex'),
                            'content_type', f.content_type,
                            'executable', f.executable,
                            'file_size_bytes', f.file_size_bytes
                        ) ORDER BY f.path)
                        FROM moa.artifact_file f
                        WHERE f.revision_uid = r.revision_uid
                    ), '[]'::jsonb)
                )
                FROM moa.artifact a
                JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
                WHERE a.valid_to IS NULL
                  AND r.valid_to IS NULL
                  AND a.kind = 'skill'
                  AND r.status = 'published'
                  AND ($1::text IS NULL OR a.storage_partition_id = $1)
                  AND (
                      a.user_id = $2
                      OR a.description LIKE ('%' || $2 || '%')
                      OR r.definition::text LIKE ('%' || $2 || '%')
                      OR encode(r.source_text, 'escape') LIKE ('%' || $2 || '%')
                      OR EXISTS (
                          SELECT 1
                          FROM moa.artifact_file f
                          WHERE f.revision_uid = r.revision_uid
                            AND encode(f.content, 'escape') LIKE ('%' || $2 || '%')
                      )
                  )
                ORDER BY a.storage_partition_id NULLS FIRST, a.scope, a.name, r.version
                "#,
            )
            .bind(ctx.storage_partition.as_deref())
            .bind(&subject.user_id)
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(export_dir.join("skills.jsonl"), &rows).await
}

pub(super) async fn collect_changelog(
    ctx: &PrivacyExportContext,
    export_dir: &Path,
) -> Result<usize, HandlerError> {
    let mut tx = begin_audited_read(&ctx.pool).await?;
    let mut rows = Vec::new();
    for subject in &ctx.subjects {
        rows.extend(
            sqlx::query_scalar::<_, Value>(
                r#"
                SELECT jsonb_build_object(
                    'change_id', change_id,
                    'storage_partition_id', storage_partition_id,
                    'user_id', user_id,
                    'scope', scope,
                    'actor_id', actor_id,
                    'actor_kind', actor_kind,
                    'op', op,
                    'target_kind', target_kind,
                    'target_label', target_label,
                    'target_uid', target_uid,
                    'payload', payload,
                    'redaction_marker', redaction_marker,
                    'pii_class', pii_class,
                    'audit_metadata', audit_metadata,
                    'cause_change_id', cause_change_id,
                    'created_at', created_at,
                    'privacy_subject_user_id', $2,
                    'privacy_subject_provenance', $3
                )
                FROM moa.graph_changelog
                WHERE ($1::text IS NULL OR storage_partition_id = $1)
                  AND (
                      user_id = $2
                      OR actor_id = $2
                      OR target_uid::text = $2
                      OR payload::text LIKE ('%' || $2 || '%')
                      OR audit_metadata->>'subject_user_id' = $2
                  )
                ORDER BY created_at, change_id
                "#,
            )
            .bind(ctx.storage_partition.as_deref())
            .bind(&subject.user_id)
            .bind(subject.provenance.as_str())
            .fetch_all(&mut *tx)
            .await
            .map_err(handler_error)?,
        );
    }
    tx.commit().await.map_err(handler_error)?;
    write_jsonl(export_dir.join("changelog.jsonl"), &rows).await
}

async fn begin_audited_read(
    pool: &PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, HandlerError> {
    let mut tx = pool.begin().await.map_err(handler_error)?;
    sqlx::query("SET LOCAL ROLE moa_auditor")
        .execute(&mut *tx)
        .await
        .map_err(handler_error)?;
    Ok(tx)
}

async fn write_jsonl(path: PathBuf, rows: &[Value]) -> Result<usize, HandlerError> {
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(handler_error)?;
    for row in rows {
        file.write_all(
            serde_json::to_string(row)
                .map_err(handler_error)?
                .as_bytes(),
        )
        .await
        .map_err(handler_error)?;
        file.write_all(b"\n").await.map_err(handler_error)?;
    }
    file.flush().await.map_err(handler_error)?;
    Ok(rows.len())
}

fn handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

fn db_handler_error(context: &'static str, error: sqlx::Error) -> HandlerError {
    TerminalError::new(format!("{context}: {error}")).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn erasure_job_stage_round_trips_and_orders() {
        // Pins: stage serialization is stable so a resumed job reads back the exact
        // stage it persisted, and unknown stages fail closed instead of defaulting.
        for stage in [
            ErasureJobStage::Vault,
            ErasureJobStage::Graph,
            ErasureJobStage::Digest,
            ErasureJobStage::Lineage,
            ErasureJobStage::Done,
        ] {
            assert_eq!(
                ErasureJobStage::parse(stage.as_str()).expect("round-trip"),
                stage
            );
        }
        assert!(ErasureJobStage::parse("unknown").is_err());
    }
}
