//! SQL-backed privacy subject resolution and export read models.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{TimeZone, Utc};
use moa_core::{StoragePartitionId, UserId};
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::Value;
use sqlx::{PgPool, Row};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use super::{
    ApprovalClaims, CONTACT_SUBJECT_PREFIX, PrivacyExportContext, PrivacySubject,
    ensure_jti_inserted,
};

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
pub(super) async fn consume_approval_jti(
    pool: &PgPool,
    claims: &ApprovalClaims,
) -> Result<(), HandlerError> {
    let expires_at = Utc
        .timestamp_opt(claims.exp, 0)
        .single()
        .ok_or_else(|| TerminalError::new_with_code(400, "approval token exp is out of range"))?;
    let inserted = sqlx::query_scalar::<_, String>(
        r#"
        INSERT INTO moa.audit_jti_used
            (jti, op, subject_user_id, approver_id, approval_claims, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (jti) DO NOTHING
        RETURNING jti
        "#,
    )
    .bind(&claims.jti)
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
        if parsed.is_contact_prefixed {
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
        format!("{CONTACT_SUBJECT_PREFIX}{}", contact_row.id),
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

/// Parsed privacy subject id with contact-prefix metadata.
#[derive(Debug, Clone, Copy)]
pub(super) struct ParsedPrivacySubjectId {
    /// Parsed UUID value.
    pub(super) uuid: Uuid,
    is_contact_prefixed: bool,
}

pub(super) fn parse_privacy_subject_id(
    subject_user_id: &UserId,
) -> Result<ParsedPrivacySubjectId, HandlerError> {
    let raw = subject_user_id.as_str();
    let (value, is_contact_prefixed) = raw
        .strip_prefix(CONTACT_SUBJECT_PREFIX)
        .map_or((raw, false), |value| (value, true));
    let uuid = Uuid::parse_str(value).map_err(|error| {
        TerminalError::new_with_code(
            400,
            format!("subject_user_id must be a UUID-backed user id: {error}"),
        )
    })?;
    Ok(ParsedPrivacySubjectId {
        uuid,
        is_contact_prefixed,
    })
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
