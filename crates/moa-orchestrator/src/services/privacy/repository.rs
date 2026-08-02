//! SQL-backed privacy subject resolution and export read models.

use std::collections::BTreeMap;
use std::path::Path;

use chrono::{TimeZone, Utc};
use futures_util::TryStreamExt;
use moa_core::{
    types::contact::ContactId, types::identifiers::StoragePartitionId, types::identifiers::UserId,
};
use moa_wire::privacy::{ParsedPrivacySubjectId, contact_privacy_subject_string};
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::Value;
use sqlx::{PgConnection, PgPool, Row};
use tokio::io::{AsyncWriteExt, BufWriter};
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
    /// Learning derived from the subject's data not yet erased.
    ///
    /// First, deliberately: the closure walk needs the source memories to still
    /// exist in order to find what was derived from them.
    Learning,
    /// Artifact-side dispositions not yet recorded.
    Artifacts,
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
            Self::Learning => "learning",
            Self::Artifacts => "artifacts",
            Self::Vault => "vault",
            Self::Graph => "graph",
            Self::Digest => "digest",
            Self::Lineage => "lineage",
            Self::Done => "done",
        }
    }

    fn parse(raw: &str) -> Result<Self, HandlerError> {
        match raw {
            "learning" => Ok(Self::Learning),
            "artifacts" => Ok(Self::Artifacts),
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
    /// Learning candidates and learning-log entries erased so far.
    pub(super) learning_erased: u64,
    /// Artifact revisions and suite contributions erased or invalidated so far.
    pub(super) artifact_erased: u64,
    /// Durable per-record dispositions written so far.
    pub(super) decisions_recorded: u64,
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
    learning_erased: i64,
    artifact_erased: i64,
    decisions_recorded: i64,
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
                learning_erased: nonneg_u64(self.learning_erased),
                artifact_erased: nonneg_u64(self.artifact_erased),
                decisions_recorded: nonneg_u64(self.decisions_recorded),
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
     learning_erased, artifact_erased, decisions_recorded, \
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
            learning_erased = $7,
            artifact_erased = $8,
            decisions_recorded = $9,
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
    .bind(u64_to_i64(progress.learning_erased))
    .bind(u64_to_i64(progress.artifact_erased))
    .bind(u64_to_i64(progress.decisions_recorded))
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
            learning_erased = $6,
            artifact_erased = $7,
            decisions_recorded = $8,
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
    .bind(u64_to_i64(progress.learning_erased))
    .bind(u64_to_i64(progress.artifact_erased))
    .bind(u64_to_i64(progress.decisions_recorded))
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

const MAX_PRIVACY_EXPORT_SUBJECTS: usize = 1_000;

const EXPORT_SUBJECTS_CTE: &str = r#"
WITH subjects AS MATERIALIZED (
    SELECT subject.user_id,
           subject.target_uid,
           subject.provenance,
           subject.ordinality
    FROM unnest($3::text[], $4::uuid[], $5::text[])
         WITH ORDINALITY AS subject(user_id, target_uid, provenance, ordinality)
)
"#;

const EXPORT_LEARNING_CLOSURE_CTES: &str = r#"
WITH RECURSIVE subjects AS MATERIALIZED (
    SELECT subject.user_id,
           subject.target_uid,
           subject.provenance,
           subject.ordinality
    FROM unnest($3::text[], $4::uuid[], $5::text[])
         WITH ORDINALITY AS subject(user_id, target_uid, provenance, ordinality)
),
subject_sessions AS MATERIALIZED (
    SELECT DISTINCT subject.ordinality AS subject_ordinal, session.id
    FROM subjects AS subject
    JOIN sessions AS session
      ON session.tenant_id = $1
     AND (session.user_id = subject.user_id OR session.contact_id = subject.target_uid)
),
subject_experiences AS MATERIALIZED (
    SELECT DISTINCT subject.ordinality AS subject_ordinal,
           experience.id,
           experience.session_id
    FROM subjects AS subject
    JOIN experience_records AS experience
      ON experience.tenant_id = $1::text
     AND experience.user_id = subject.user_id
),
subject_attributions AS MATERIALIZED (
    SELECT DISTINCT experience.subject_ordinal, attribution.id
    FROM subject_experiences AS experience
    JOIN experience_attributions AS attribution
      ON attribution.tenant_id = $1::text
     AND attribution.experience_id = experience.id
),
subject_segments AS MATERIALIZED (
    SELECT DISTINCT session.subject_ordinal, segment.id
    FROM subject_sessions AS session
    JOIN task_segments AS segment
      ON segment.tenant_id = $1::text
     AND segment.session_id = session.id
),
subject_candidate_anchors(subject_ordinal, source_kind, privacy_anchor_id) AS MATERIALIZED (
    SELECT subject.ordinality, 'contact'::text, subject.target_uid
    FROM subjects AS subject
    UNION ALL
    SELECT session.subject_ordinal, 'session'::text, session.id
    FROM subject_sessions AS session
    UNION ALL
    SELECT session.subject_ordinal, 'event'::text, session.id
    FROM subject_sessions AS session
    UNION ALL
    SELECT segment.subject_ordinal, 'task_segment'::text, segment.id
    FROM subject_segments AS segment
    UNION ALL
    SELECT experience.subject_ordinal, 'experience'::text, experience.id
    FROM subject_experiences AS experience
    UNION ALL
    SELECT attribution.subject_ordinal, 'attribution'::text, attribution.id
    FROM subject_attributions AS attribution
),
derived(subject_ordinal, kind, id) AS (
    SELECT DISTINCT anchor.subject_ordinal, 'candidate'::text, source.candidate_id
    FROM subject_candidate_anchors AS anchor
    JOIN learning_candidate_source AS source
      ON source.tenant_id = $1::text
     AND source.source_kind = anchor.source_kind
     AND source.privacy_anchor_id = anchor.privacy_anchor_id
    UNION
    SELECT parent.subject_ordinal, next.kind, next.id
    FROM derived AS parent
    CROSS JOIN LATERAL (
        SELECT 'candidate'::text AS kind, source.candidate_id AS id
        FROM learning_candidate_source AS source
        WHERE source.tenant_id = $1::text
          AND source.source_kind = CASE parent.kind
              WHEN 'candidate' THEN 'promotion_candidate'
              WHEN 'revision' THEN 'artifact_revision'
          END
          AND source.privacy_anchor_id = parent.id
        UNION
        SELECT 'revision'::text, contribution.revision_uid
        FROM moa.artifact_revision_contribution AS contribution
        WHERE parent.kind = 'candidate'
          AND contribution.tenant_id = $1::text
          AND contribution.candidate_id = parent.id
    ) AS next
),
subject_learning_anchors(subject_ordinal, source_kind, privacy_anchor_id) AS MATERIALIZED (
    SELECT derived.subject_ordinal, 'candidate'::text, derived.id
    FROM derived
    WHERE derived.kind = 'candidate'
    UNION ALL
    SELECT derived.subject_ordinal, 'artifact_revision'::text, derived.id
    FROM derived
    WHERE derived.kind = 'revision'
    UNION ALL
    SELECT session.subject_ordinal, 'session'::text, session.id
    FROM subject_sessions AS session
    UNION ALL
    SELECT experience.subject_ordinal, 'experience'::text, experience.id
    FROM subject_experiences AS experience
    UNION ALL
    SELECT segment.subject_ordinal, 'task_segment'::text, segment.id
    FROM subject_segments AS segment
),
subject_learning AS MATERIALIZED (
    SELECT DISTINCT anchor.subject_ordinal, source.learning_id
    FROM subject_learning_anchors AS anchor
    JOIN learning_log_source AS source
      ON source.tenant_id = $1::text
     AND source.source_kind = anchor.source_kind
     AND source.privacy_anchor_id = anchor.privacy_anchor_id
)
"#;

/// Starts the one read-only, repeatable-read snapshot used by a privacy export.
///
/// The isolation statement is deliberately the first statement after `BEGIN`.
pub async fn begin_privacy_export_snapshot(
    pool: &PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, HandlerError> {
    let mut tx = pool.begin().await.map_err(handler_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(tx.as_mut())
        .await
        .map_err(handler_error)?;
    sqlx::query("SET LOCAL statement_timeout = '30s'")
        .execute(tx.as_mut())
        .await
        .map_err(handler_error)?;
    sqlx::query("SET LOCAL idle_in_transaction_session_timeout = '30s'")
        .execute(tx.as_mut())
        .await
        .map_err(handler_error)?;
    sqlx::query("SET LOCAL ROLE moa_auditor")
        .execute(tx.as_mut())
        .await
        .map_err(handler_error)?;
    Ok(tx)
}

/// Resolves the primary subject and verified linked contacts inside the export snapshot.
pub(super) async fn resolve_privacy_export_subjects(
    conn: &mut PgConnection,
    tenant_id: Uuid,
    storage_partition_id: Option<&StoragePartitionId>,
    requested_subject_user_id: &UserId,
) -> Result<ResolvedPrivacySubjects, HandlerError> {
    let parsed = parse_privacy_subject_id(requested_subject_user_id)?;
    let mut rows = sqlx::query_as::<_, (Uuid, String, bool)>(
        r#"
        WITH requested AS MATERIALIZED (
            SELECT id, storage_partition_id, state
            FROM contacts
            WHERE id = $1
              AND tenant_id = $2
              AND ($3::text IS NULL OR storage_partition_id = $3)
        ),
        resolved AS (
            SELECT requested.id,
                   requested.storage_partition_id,
                   TRUE AS is_primary,
                   0::bigint AS ordinal
            FROM requested
            UNION ALL
            SELECT linked.id,
                   linked.storage_partition_id,
                   FALSE,
                   row_number() OVER (
                       ORDER BY linked.merged_at NULLS LAST, linked.updated_at DESC, linked.id
                   )
            FROM requested
            JOIN contacts AS linked
              ON requested.state = 'verified'
             AND linked.canonical_contact_id = requested.id
             AND linked.tenant_id = $2
             AND linked.storage_partition_id = requested.storage_partition_id
        )
        SELECT id, storage_partition_id, is_primary
        FROM resolved
        ORDER BY ordinal, id
        LIMIT 1001
        "#,
    )
    .bind(parsed.uuid)
    .bind(tenant_id)
    .bind(storage_partition_id.map(StoragePartitionId::as_str))
    .fetch(&mut *conn);
    let mut resolved_rows = Vec::with_capacity(MAX_PRIVACY_EXPORT_SUBJECTS.saturating_add(1));
    while let Some(row) = rows
        .try_next()
        .await
        .map_err(|error| db_handler_error("resolve privacy export subjects", error))?
    {
        resolved_rows.push(row);
    }
    drop(rows);

    if resolved_rows.len() > MAX_PRIVACY_EXPORT_SUBJECTS {
        return Err(TerminalError::new_with_code(
            400,
            "privacy export subject expansion exceeds the 1000-subject limit",
        )
        .into());
    }
    let Some((_, effective_storage_partition, _)) = resolved_rows.first() else {
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

    let subjects = resolved_rows
        .iter()
        .map(|(id, _, is_primary)| {
            if *is_primary {
                PrivacySubject::primary(contact_privacy_subject_string(ContactId(*id)), *id)
            } else {
                PrivacySubject::linked_contact(*id)
            }
        })
        .collect();
    Ok(ResolvedPrivacySubjects {
        kind: PrivacySubjectKind::Contact,
        effective_storage_partition: Some(effective_storage_partition.clone()),
        subjects,
    })
}

#[derive(Debug)]
struct ExportSubjectRelation {
    user_ids: Vec<String>,
    target_uids: Vec<Uuid>,
    provenances: Vec<String>,
}

impl ExportSubjectRelation {
    fn from_subjects(subjects: &[PrivacySubject]) -> Self {
        Self {
            user_ids: subjects
                .iter()
                .map(|subject| subject.user_id.clone())
                .collect(),
            target_uids: subjects.iter().map(|subject| subject.target_uid).collect(),
            provenances: subjects
                .iter()
                .map(|subject| subject.provenance.as_str().to_string())
                .collect(),
        }
    }
}

/// Streams every typed privacy export section inside the caller's snapshot.
pub async fn collect_privacy_export_data_sections(
    ctx: &PrivacyExportContext,
    conn: &mut PgConnection,
    export_dir: &Path,
) -> Result<BTreeMap<&'static str, usize>, HandlerError> {
    let subjects = ExportSubjectRelation::from_subjects(&ctx.subjects);
    let mut counts = BTreeMap::new();

    for (name, labels) in [
        ("facts", "'Fact', 'Lesson', 'Decision', 'Incident'"),
        ("entities", "'Entity', 'Concept', 'Source'"),
    ] {
        let query = format!(
            "{EXPORT_SUBJECTS_CTE}
             SELECT to_jsonb(node) || jsonb_build_object(
                 'privacy_subject_user_id', subject.user_id,
                 'privacy_subject_provenance', subject.provenance
             )
             FROM moa.node_index AS node
             JOIN subjects AS subject ON subject.target_uid = node.data_subject_id
             WHERE node.tenant_id = $1
               AND node.valid_to IS NULL
               AND node.label IN ({labels})
             ORDER BY subject.ordinality, node.label, node.name, node.uid"
        );
        counts.insert(
            name,
            stream_subject_query(
                ctx,
                conn,
                &subjects,
                &export_dir.join(format!("{name}.jsonl")),
                &query,
            )
            .await?,
        );
    }

    let relationships = format!(
        "{EXPORT_SUBJECTS_CTE},
         matched AS (
             SELECT DISTINCT ON (edge.uid)
                    edge.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    subject.ordinality AS privacy_subject_ordinal
             FROM moa.edge_index AS edge
             JOIN subjects AS subject
               ON edge.user_id = subject.user_id
               OR edge.contact_id = subject.target_uid
               OR EXISTS (
                   SELECT 1
                   FROM moa.node_index AS endpoint
                   WHERE endpoint.uid IN (edge.start_uid, edge.end_uid)
                     AND endpoint.tenant_id = $1
                     AND endpoint.data_subject_id = subject.target_uid
               )
             WHERE edge.tenant_id = $1
             ORDER BY edge.uid, subject.ordinality
         )
         SELECT to_jsonb(matched) - 'privacy_subject_ordinal'
         FROM matched
         ORDER BY privacy_subject_ordinal, created_at, uid"
    );
    counts.insert(
        "relationships",
        stream_subject_query(
            ctx,
            conn,
            &subjects,
            &export_dir.join("relationships.jsonl"),
            &relationships,
        )
        .await?,
    );

    let embeddings = format!(
        "{EXPORT_SUBJECTS_CTE},
         matched AS (
             SELECT DISTINCT ON (embedding.storage_partition_id, embedding.uid)
                    embedding.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    subject.ordinality AS privacy_subject_ordinal
             FROM moa.embeddings AS embedding
             JOIN moa.node_index AS node
               ON node.uid = embedding.uid AND node.tenant_id = embedding.tenant_id
             JOIN subjects AS subject
               ON embedding.user_id = subject.user_id
               OR embedding.contact_id = subject.target_uid
               OR node.data_subject_id = subject.target_uid
             WHERE embedding.tenant_id = $1
               AND embedding.valid_to IS NULL
               AND node.valid_to IS NULL
             ORDER BY embedding.storage_partition_id, embedding.uid, subject.ordinality
         )
         SELECT (to_jsonb(matched) - 'embedding' - 'privacy_subject_ordinal')
                || jsonb_build_object('embedding', (matched.embedding::text)::jsonb)
         FROM matched
         ORDER BY privacy_subject_ordinal, label, uid"
    );
    counts.insert(
        "embeddings",
        stream_subject_query(
            ctx,
            conn,
            &subjects,
            &export_dir.join("embeddings.jsonl"),
            &embeddings,
        )
        .await?,
    );

    stream_learning_sections(ctx, conn, &subjects, export_dir, &mut counts).await?;
    stream_erasure_decisions(ctx, conn, &subjects, export_dir, &mut counts).await?;
    stream_changelog(ctx, conn, &subjects, export_dir, &mut counts).await?;
    Ok(counts)
}

async fn stream_learning_sections(
    ctx: &PrivacyExportContext,
    conn: &mut PgConnection,
    subjects: &ExportSubjectRelation,
    export_dir: &Path,
    counts: &mut BTreeMap<&'static str, usize>,
) -> Result<(), HandlerError> {
    let candidates = format!(
        "{EXPORT_LEARNING_CLOSURE_CTES},
         matched AS (
             SELECT DISTINCT ON (candidate.id)
                    candidate.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    derived.subject_ordinal
             FROM derived
             JOIN learning_candidates AS candidate
               ON derived.kind = 'candidate'
              AND candidate.id = derived.id
              AND candidate.tenant_id = $1::text
             JOIN subjects AS subject ON subject.ordinality = derived.subject_ordinal
             ORDER BY candidate.id, derived.subject_ordinal
         )
         SELECT (to_jsonb(matched) - 'subject_ordinal')
                || jsonb_build_object(
                    'sources', COALESCE((
                        SELECT jsonb_agg(to_jsonb(source) ORDER BY source.source_kind, source.id)
                        FROM learning_candidate_source AS source
                        WHERE source.candidate_id = matched.id
                    ), '[]'::jsonb)
                )
         FROM matched
         ORDER BY subject_ordinal, created_at, id"
    );
    counts.insert(
        "learning_candidates",
        stream_subject_query(
            ctx,
            conn,
            subjects,
            &export_dir.join("learning_candidates.jsonl"),
            &candidates,
        )
        .await?,
    );

    let entries = format!(
        "{EXPORT_LEARNING_CLOSURE_CTES},
         matched AS (
             SELECT DISTINCT ON (entry.id)
                    entry.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    reached.subject_ordinal
             FROM subject_learning AS reached
             JOIN learning_log AS entry
               ON entry.id = reached.learning_id AND entry.tenant_id = $1::text
             JOIN subjects AS subject ON subject.ordinality = reached.subject_ordinal
             ORDER BY entry.id, reached.subject_ordinal
         )
         SELECT (to_jsonb(matched) - 'subject_ordinal')
                || jsonb_build_object(
                    'sources', COALESCE((
                        SELECT jsonb_agg(to_jsonb(source) ORDER BY source.source_kind, source.id)
                        FROM learning_log_source AS source
                        WHERE source.learning_id = matched.id
                    ), '[]'::jsonb)
                )
         FROM matched
         ORDER BY subject_ordinal, valid_from, id"
    );
    counts.insert(
        "learning_entries",
        stream_subject_query(
            ctx,
            conn,
            subjects,
            &export_dir.join("learning_entries.jsonl"),
            &entries,
        )
        .await?,
    );

    let decisions = format!(
        "{EXPORT_LEARNING_CLOSURE_CTES},
         matched AS (
             SELECT DISTINCT ON (decision.id)
                    decision.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    derived.subject_ordinal
             FROM derived
             JOIN learning_candidate_decision AS decision
               ON derived.kind = 'candidate'
              AND decision.candidate_id = derived.id
              AND decision.tenant_id = $1::text
             JOIN subjects AS subject ON subject.ordinality = derived.subject_ordinal
             ORDER BY decision.id, derived.subject_ordinal
         )
         SELECT to_jsonb(matched) - 'subject_ordinal'
         FROM matched
         ORDER BY subject_ordinal, decided_at, id"
    );
    counts.insert(
        "learning_decisions",
        stream_subject_query(
            ctx,
            conn,
            subjects,
            &export_dir.join("learning_decisions.jsonl"),
            &decisions,
        )
        .await?,
    );

    let revision_contributions = format!(
        "{EXPORT_LEARNING_CLOSURE_CTES},
         matched AS (
             SELECT DISTINCT ON (contribution.contribution_uid)
                    contribution.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    derived.subject_ordinal
             FROM derived
             JOIN moa.artifact_revision_contribution AS contribution
               ON contribution.tenant_id = $1::text
              AND (
                  (derived.kind = 'candidate' AND contribution.candidate_id = derived.id)
                  OR (derived.kind = 'revision' AND contribution.revision_uid = derived.id)
              )
             JOIN subjects AS subject ON subject.ordinality = derived.subject_ordinal
             ORDER BY contribution.contribution_uid, derived.subject_ordinal
         )
         SELECT to_jsonb(matched) - 'subject_ordinal'
         FROM matched
         ORDER BY subject_ordinal, created_at, contribution_uid"
    );
    counts.insert(
        "artifact_revision_contributions",
        stream_subject_query(
            ctx,
            conn,
            subjects,
            &export_dir.join("artifact_revision_contributions.jsonl"),
            &revision_contributions,
        )
        .await?,
    );

    let suite_contributions = format!(
        "{EXPORT_LEARNING_CLOSURE_CTES},
         matched AS (
             SELECT DISTINCT ON (suite.contribution_uid)
                    suite.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    subject.ordinality AS subject_ordinal
             FROM moa.artifact_suite_contribution AS suite
             JOIN subjects AS subject
               ON EXISTS (
                   SELECT 1 FROM derived
                   WHERE derived.subject_ordinal = subject.ordinality
                     AND derived.kind = 'candidate'
                     AND derived.id = suite.candidate_id
               )
               OR EXISTS (
                   SELECT 1 FROM subject_sessions
                   WHERE subject_ordinal = subject.ordinality
                     AND id = suite.source_session_id
               )
               OR EXISTS (
                   SELECT 1 FROM subject_experiences
                   WHERE subject_ordinal = subject.ordinality
                     AND id = suite.source_experience_id
               )
             WHERE suite.tenant_id = $1::text
             ORDER BY suite.contribution_uid, subject.ordinality
         )
         SELECT to_jsonb(matched) - 'subject_ordinal'
         FROM matched
         ORDER BY subject_ordinal, created_at, contribution_uid"
    );
    counts.insert(
        "artifact_suite_contributions",
        stream_subject_query(
            ctx,
            conn,
            subjects,
            &export_dir.join("artifact_suite_contributions.jsonl"),
            &suite_contributions,
        )
        .await?,
    );

    let skills = format!(
        "{EXPORT_LEARNING_CLOSURE_CTES},
         matched AS (
             SELECT DISTINCT ON (revision.revision_uid)
                    artifact.artifact_uid,
                    revision.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    subject.ordinality AS subject_ordinal
             FROM moa.artifact AS artifact
             JOIN moa.artifact_revision AS revision
               ON revision.artifact_uid = artifact.artifact_uid
             JOIN subjects AS subject
               ON artifact.user_id = subject.user_id
               OR EXISTS (
                   SELECT 1 FROM derived
                   WHERE derived.subject_ordinal = subject.ordinality
                     AND derived.kind = 'revision'
                     AND derived.id = revision.revision_uid
               )
             WHERE artifact.kind = 'skill'
               AND artifact.storage_partition_id = $2
             ORDER BY revision.revision_uid, subject.ordinality
         )
         SELECT (to_jsonb(matched) - 'subject_ordinal')
                || jsonb_build_object(
                    'files', COALESCE((
                        SELECT jsonb_agg(
                            jsonb_build_object(
                                'path', file.path,
                                'content_base64', encode(file.content, 'base64'),
                                'content_sha256_hex', encode(file.content_sha256, 'hex'),
                                'content_type', file.content_type,
                                'executable', file.executable,
                                'file_size_bytes', file.file_size_bytes
                            ) ORDER BY file.path
                        )
                        FROM moa.artifact_file AS file
                        WHERE file.revision_uid = matched.revision_uid
                    ), '[]'::jsonb)
                )
         FROM matched
         ORDER BY subject_ordinal, created_at, revision_uid"
    );
    counts.insert(
        "skills",
        stream_subject_query(
            ctx,
            conn,
            subjects,
            &export_dir.join("skills.jsonl"),
            &skills,
        )
        .await?,
    );
    Ok(())
}

async fn stream_erasure_decisions(
    ctx: &PrivacyExportContext,
    conn: &mut PgConnection,
    subjects: &ExportSubjectRelation,
    export_dir: &Path,
    counts: &mut BTreeMap<&'static str, usize>,
) -> Result<(), HandlerError> {
    let query = format!(
        "{EXPORT_SUBJECTS_CTE}
         SELECT to_jsonb(decision)
                || jsonb_build_object('privacy_subject_provenance', subject.provenance)
         FROM moa.privacy_erasure_record_decision AS decision
         JOIN subjects AS subject ON subject.user_id = decision.subject_user_id
         WHERE decision.tenant_id = $1
         ORDER BY subject.ordinality, decision.decided_at, decision.decision_uid"
    );
    counts.insert(
        "erasure_decisions",
        stream_subject_query(
            ctx,
            conn,
            subjects,
            &export_dir.join("erasure_decisions.jsonl"),
            &query,
        )
        .await?,
    );
    Ok(())
}

async fn stream_changelog(
    ctx: &PrivacyExportContext,
    conn: &mut PgConnection,
    subjects: &ExportSubjectRelation,
    export_dir: &Path,
    counts: &mut BTreeMap<&'static str, usize>,
) -> Result<(), HandlerError> {
    let query = format!(
        "{EXPORT_SUBJECTS_CTE},
         matched AS (
             SELECT DISTINCT ON (changelog.change_id, changelog.created_at)
                    changelog.*,
                    subject.user_id AS privacy_subject_user_id,
                    subject.provenance AS privacy_subject_provenance,
                    subject.ordinality AS subject_ordinal
             FROM moa.graph_changelog AS changelog
             JOIN subjects AS subject
               ON changelog.user_id = subject.user_id
               OR changelog.actor_id = subject.user_id
               OR changelog.target_uid = subject.target_uid
               OR changelog.contact_id = subject.target_uid
               OR changelog.payload ->> 'subject_user_id' = subject.user_id
               OR changelog.audit_metadata ->> 'subject_user_id' = subject.user_id
               OR EXISTS (
                   SELECT 1
                   FROM jsonb_array_elements(
                       CASE
                           WHEN jsonb_typeof(changelog.payload -> 'subjects') = 'array'
                           THEN changelog.payload -> 'subjects'
                           ELSE '[]'::jsonb
                       END
                   ) AS audit_subject
                   WHERE audit_subject ->> 'user_id' = subject.user_id
                      OR audit_subject ->> 'target_uid' = subject.target_uid::text
               )
               OR EXISTS (
                   SELECT 1
                   FROM jsonb_array_elements(
                       CASE
                           WHEN jsonb_typeof(changelog.audit_metadata -> 'subjects') = 'array'
                           THEN changelog.audit_metadata -> 'subjects'
                           ELSE '[]'::jsonb
                       END
                   ) AS audit_subject
                   WHERE audit_subject ->> 'user_id' = subject.user_id
                      OR audit_subject ->> 'target_uid' = subject.target_uid::text
               )
             WHERE changelog.tenant_id = $1
             ORDER BY changelog.change_id, changelog.created_at, subject.ordinality
         )
         SELECT to_jsonb(matched) - 'subject_ordinal'
         FROM matched
         ORDER BY subject_ordinal, created_at, change_id"
    );
    counts.insert(
        "changelog",
        stream_subject_query(
            ctx,
            conn,
            subjects,
            &export_dir.join("changelog.jsonl"),
            &query,
        )
        .await?,
    );
    Ok(())
}

async fn stream_subject_query(
    ctx: &PrivacyExportContext,
    conn: &mut PgConnection,
    subjects: &ExportSubjectRelation,
    path: &Path,
    query: &str,
) -> Result<usize, HandlerError> {
    let file = tokio::fs::File::create(path).await.map_err(handler_error)?;
    let mut writer = BufWriter::new(file);
    let mut rows = sqlx::query_scalar::<_, Value>(query)
        .bind(ctx.tenant_id.0)
        .bind(ctx.storage_partition.as_deref())
        .bind(&subjects.user_ids)
        .bind(&subjects.target_uids)
        .bind(&subjects.provenances)
        .fetch(&mut *conn);
    let mut count = 0usize;
    while let Some(row) = rows.try_next().await.map_err(handler_error)? {
        let bytes = serde_json::to_vec(&row).map_err(handler_error)?;
        writer.write_all(&bytes).await.map_err(handler_error)?;
        writer.write_all(b"\n").await.map_err(handler_error)?;
        count = count.saturating_add(1);
    }
    drop(rows);
    writer.flush().await.map_err(handler_error)?;
    Ok(count)
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
            ErasureJobStage::Learning,
            ErasureJobStage::Artifacts,
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
