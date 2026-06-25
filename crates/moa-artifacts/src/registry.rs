//! Postgres-backed artifact registry with MOA three-tier visibility.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use moa_core::{ActionRuleScope, MoaError, Result, SessionId, StoragePartitionId, UserId};
use moa_db::ScopedConn;
use moa_memory_types::ScopeContext;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::canonical::canonical_hash;
use crate::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use crate::validation::{ValidationReport, validate_for_status};

/// Artifact storage columns derived from artifact inheritance scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactScopeParts {
    /// Tenant override owner used by RLS for tenant-owned artifacts.
    pub tenant_id: Option<Uuid>,
    /// Storage column used to route tenant-owned artifacts.
    pub storage_partition_id: Option<String>,
    /// User ownership column; canonical artifact defaults do not use users.
    pub user_id: Option<String>,
}

impl ArtifactScopeParts {
    /// Converts an artifact inheritance scope into database column values.
    #[must_use]
    pub fn from_scope(scope: &ActionRuleScope) -> Self {
        match scope {
            ActionRuleScope::Tenant { tenant_id } => Self {
                tenant_id: Some(tenant_id.0),
                storage_partition_id: Some(StoragePartitionId::for_tenant(*tenant_id).to_string()),
                user_id: None,
            },
        }
    }
}

fn artifact_scope_context(scope: &ActionRuleScope) -> ScopeContext {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => ScopeContext::tenant(*tenant_id),
    }
}

/// Stored artifact revision loaded from Postgres.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredArtifactRevision {
    /// Artifact row identifier.
    pub artifact_uid: Uuid,
    /// Revision row identifier.
    pub revision_uid: Uuid,
    /// Storage partition owning tenant-scoped artifacts.
    pub storage_partition_id: Option<StoragePartitionId>,
    /// User owning user scoped artifacts.
    pub user_id: Option<UserId>,
    /// Generated SQL scope tier.
    pub scope: String,
    /// Artifact kind.
    pub kind: ArtifactKind,
    /// Artifact name.
    pub name: String,
    /// Artifact description.
    pub description: String,
    /// Artifact tags.
    pub tags: Vec<String>,
    /// Canonical artifact document.
    pub document: ArtifactDocument,
    /// SHA-256 hash over the canonical document.
    pub canonical_hash: Vec<u8>,
    /// Original source format: `json` or `yaml`.
    pub source_format: String,
    /// Original submitted source bytes.
    pub source_text: Vec<u8>,
    /// Revision status.
    pub status: ArtifactStatus,
    /// Stored validation report.
    pub validation_report: Value,
    /// Monotonic artifact-local revision version.
    pub version: i32,
    /// Publication timestamp.
    pub published_at: Option<DateTime<Utc>>,
    /// Invalidation timestamp.
    pub valid_to: Option<DateTime<Utc>>,
    /// Row creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Row update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Lightweight visible artifact list entry.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactSummary {
    /// Artifact row identifier.
    pub artifact_uid: Uuid,
    /// Latest visible revision row identifier.
    pub revision_uid: Uuid,
    /// Generated SQL scope tier.
    pub scope: String,
    /// Artifact kind.
    pub kind: ArtifactKind,
    /// Artifact name.
    pub name: String,
    /// Artifact description.
    pub description: String,
    /// Artifact tags.
    pub tags: Vec<String>,
    /// Latest revision status.
    pub status: ArtifactStatus,
    /// Latest revision version.
    pub version: i32,
    /// Latest update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// File to store with an artifact revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewArtifactFile {
    /// Package-relative path.
    pub path: String,
    /// File bytes.
    pub content: Vec<u8>,
    /// Optional content type.
    pub content_type: Option<String>,
    /// Whether this file is executable.
    pub executable: bool,
}

impl NewArtifactFile {
    /// Builds a new artifact file from a relative path and bytes.
    #[must_use]
    pub fn new(path: impl Into<String>, content: Vec<u8>) -> Self {
        Self {
            path: path.into(),
            content,
            content_type: None,
            executable: false,
        }
    }
}

/// Draft artifact revision payload to insert.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NewArtifactDraft<'a> {
    /// Canonical artifact document.
    pub document: &'a ArtifactDocument,
    /// Original source format: `json` or `yaml`.
    pub source_format: &'a str,
    /// Original submitted source bytes.
    pub source_text: &'a [u8],
    /// Optional package files stored with the revision.
    pub files: &'a [NewArtifactFile],
}

/// Published artifact revision payload to insert inside an existing transaction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NewPublishedArtifactRevision<'a> {
    /// Canonical artifact document.
    pub document: &'a ArtifactDocument,
    /// Original source format: `json` or `yaml`.
    pub source_format: &'a str,
    /// Original submitted source bytes.
    pub source_text: &'a [u8],
    /// Optional package files stored with the revision.
    pub files: &'a [NewArtifactFile],
    /// Optional caller-owned artifact-local version.
    pub version: Option<i32>,
}

/// Stored artifact file row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactFile {
    /// File row identifier.
    pub file_uid: Uuid,
    /// Package-relative path.
    pub path: String,
    /// File bytes.
    pub content: Vec<u8>,
    /// SHA-256 file digest.
    pub content_sha256: Vec<u8>,
    /// Optional content type.
    pub content_type: Option<String>,
    /// Whether this file is executable.
    pub executable: bool,
    /// File size in bytes.
    pub file_size_bytes: i64,
}

/// Workflow run status persisted for artifacts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactRunStatus {
    /// Run has been created but not started.
    Queued,
    /// Run is actively executing.
    Running,
    /// Run is pending tenant-admin action review.
    PendingReview,
    /// Run completed successfully.
    Completed,
    /// Run failed.
    Failed,
    /// Run was cancelled.
    Cancelled,
}

impl ArtifactRunStatus {
    /// Returns the lowercase database label for this status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::PendingReview => "pending_review",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Workflow node-run status persisted for artifacts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactNodeRunStatus {
    /// Node run has been created but not started.
    Queued,
    /// Node run is actively executing.
    Running,
    /// Node run is pending tenant-admin action review.
    PendingReview,
    /// Node run completed successfully.
    Completed,
    /// Node run failed.
    Failed,
    /// Node run was cancelled.
    Cancelled,
    /// Node run was skipped.
    Skipped,
}

impl ArtifactNodeRunStatus {
    /// Returns the lowercase database label for this status.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::PendingReview => "pending_review",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }
}

/// New workflow run row.
#[derive(Clone, Debug, PartialEq)]
pub struct NewArtifactRun {
    /// Referenced artifact UID, if already resolved.
    pub artifact_uid: Option<Uuid>,
    /// Referenced revision UID, if already resolved.
    pub revision_uid: Option<Uuid>,
    /// Session associated with this workflow run, when the run was started from a session.
    pub session_id: Option<SessionId>,
    /// Workflow reference string.
    pub workflow_ref: String,
    /// Initial run status.
    pub status: ArtifactRunStatus,
    /// Current node ID.
    pub current_node_id: Option<String>,
    /// Input payload.
    pub input: Value,
    /// Mutable workflow state.
    pub state: Value,
    /// Output payload.
    pub output: Option<Value>,
    /// Error text.
    pub error: Option<String>,
    /// Optional idempotency key.
    pub idempotency_key: Option<String>,
}

/// Stored workflow run row.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactRun {
    /// Run row identifier.
    pub run_uid: Uuid,
    /// Referenced artifact UID, if the run was started from a resolved artifact.
    pub artifact_uid: Option<Uuid>,
    /// Referenced revision UID, if the run was started from a resolved artifact revision.
    pub revision_uid: Option<Uuid>,
    /// Session associated with this workflow run, when present.
    pub session_id: Option<SessionId>,
    /// Workflow reference string.
    pub workflow_ref: String,
    /// Current status.
    pub status: ArtifactRunStatus,
    /// Current node ID.
    pub current_node_id: Option<String>,
    /// Input payload.
    pub input: Value,
    /// Mutable workflow state.
    pub state: Value,
    /// Output payload.
    pub output: Option<Value>,
    /// Error text.
    pub error: Option<String>,
    /// Run start timestamp.
    pub started_at: DateTime<Utc>,
    /// Run completion timestamp.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Registry patch payload for mutable workflow run fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtifactRunUpdate {
    /// Replacement status when present.
    pub status: Option<ArtifactRunStatus>,
    /// Replacement current node ID when present, including clearing it to `NULL`.
    pub current_node_id: Option<Option<String>>,
    /// Replacement workflow state when present.
    pub state: Option<Value>,
    /// Replacement output when present, including clearing it to `NULL`.
    pub output: Option<Option<Value>>,
    /// Replacement error when present, including clearing it to `NULL`.
    pub error: Option<Option<String>>,
    /// Replacement completion timestamp when present, including clearing it to `NULL`.
    pub completed_at: Option<Option<DateTime<Utc>>>,
}

/// New node-run row.
#[derive(Clone, Debug, PartialEq)]
pub struct NewArtifactNodeRun {
    /// Parent run identifier.
    pub run_uid: Uuid,
    /// Workflow node ID.
    pub node_id: String,
    /// Node status.
    pub status: ArtifactNodeRunStatus,
    /// Node input payload.
    pub input: Value,
    /// Node output payload.
    pub output: Option<Value>,
    /// Error text.
    pub error: Option<String>,
    /// Optional completion timestamp.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Stored workflow node-run row.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactNodeRun {
    /// Node-run row identifier.
    pub node_run_uid: Uuid,
    /// Parent run identifier.
    pub run_uid: Uuid,
    /// Workflow node ID.
    pub node_id: String,
    /// Node status.
    pub status: ArtifactNodeRunStatus,
    /// Node input payload.
    pub input: Value,
    /// Node output payload.
    pub output: Option<Value>,
    /// Error text.
    pub error: Option<String>,
    /// Node-run start timestamp.
    pub started_at: DateTime<Utc>,
    /// Node-run completion timestamp.
    pub completed_at: Option<DateTime<Utc>>,
}

/// Registry patch payload for mutable workflow node-run fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtifactNodeRunUpdate {
    /// Replacement status when present.
    pub status: Option<ArtifactNodeRunStatus>,
    /// Replacement output when present, including clearing it to `NULL`.
    pub output: Option<Option<Value>>,
    /// Replacement error when present, including clearing it to `NULL`.
    pub error: Option<Option<String>>,
    /// Replacement completion timestamp when present, including clearing it to `NULL`.
    pub completed_at: Option<Option<DateTime<Utc>>>,
}

/// Postgres-backed canonical artifact registry.
pub struct ArtifactRegistry {
    pool: PgPool,
}

impl ArtifactRegistry {
    /// Creates an artifact registry backed by a Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Creates a new draft revision and stores optional package files.
    pub async fn create_draft(
        &self,
        scope: &ActionRuleScope,
        draft: NewArtifactDraft<'_>,
    ) -> Result<StoredArtifactRevision> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let stored = Self::create_draft_in_tx(conn.as_mut(), scope, draft).await?;
        conn.commit().await?;
        Ok(stored)
    }

    /// Creates a new draft revision using the caller's open transaction.
    ///
    /// The caller owns commit or rollback and should apply matching MOA scope GUCs before calling
    /// this method when row-level security is relevant.
    pub async fn create_draft_in_tx(
        conn: &mut PgConnection,
        scope: &ActionRuleScope,
        draft: NewArtifactDraft<'_>,
    ) -> Result<StoredArtifactRevision> {
        validate_source_format(draft.source_format)?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let artifact_uid = ensure_artifact(conn, &parts, draft.document).await?;
        let version = next_revision_version(conn, artifact_uid).await?;
        let revision_uid = Uuid::now_v7();
        let definition = serde_json::to_value(draft.document)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        let canonical_hash = canonical_hash(draft.document)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?
            .to_vec();
        let validation_report =
            serde_json::to_value(validate_for_status(draft.document, ArtifactStatus::Draft))
                .map_err(|error| MoaError::SerializationError(error.to_string()))?;

        sqlx::query(
            r#"
            INSERT INTO moa.artifact_revision (
                revision_uid, artifact_uid, tenant_id, storage_partition_id, user_id, definition,
                canonical_hash, source_format, source_text, status,
                validation_report, version
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'draft', $10, $11)
            "#,
        )
        .bind(revision_uid)
        .bind(artifact_uid)
        .bind(parts.tenant_id)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(definition)
        .bind(&canonical_hash)
        .bind(draft.source_format)
        .bind(draft.source_text)
        .bind(validation_report)
        .bind(version)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        sqlx::query(
            "UPDATE moa.artifact SET latest_revision_uid = $1, updated_at = now() WHERE artifact_uid = $2",
        )
        .bind(revision_uid)
        .bind(artifact_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        insert_files(conn, &parts, artifact_uid, revision_uid, draft.files).await?;
        load_revision_by_uid(conn, revision_uid).await
    }

    /// Marks a draft revision as published without invalidating older published revisions.
    pub async fn publish_revision(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
        report: &ValidationReport,
    ) -> Result<StoredArtifactRevision> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let stored = Self::publish_revision_in_tx(conn.as_mut(), revision_uid, report).await?;
        conn.commit().await?;
        Ok(stored)
    }

    /// Marks a draft revision as published using the caller's open transaction.
    ///
    /// The caller owns commit or rollback and should apply matching MOA scope GUCs before calling
    /// this method when row-level security is relevant.
    pub async fn publish_revision_in_tx(
        conn: &mut PgConnection,
        revision_uid: Uuid,
        report: &ValidationReport,
    ) -> Result<StoredArtifactRevision> {
        let artifact_uid = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT artifact_uid
            FROM moa.artifact_revision
            WHERE revision_uid = $1
              AND valid_to IS NULL
            FOR UPDATE
            "#,
        )
        .bind(revision_uid)
        .fetch_one(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        let validation_report = serde_json::to_value(report)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?;
        sqlx::query(
            r#"
            UPDATE moa.artifact_revision
            SET status = 'published',
                validation_report = $2,
                published_at = COALESCE(published_at, now()),
                updated_at = now()
            WHERE revision_uid = $1
              AND valid_to IS NULL
        "#,
        )
        .bind(revision_uid)
        .bind(validation_report)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        sqlx::query(
            "UPDATE moa.artifact SET latest_revision_uid = $1, updated_at = now() WHERE artifact_uid = $2",
        )
        .bind(revision_uid)
        .bind(artifact_uid)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;

        load_revision_by_uid(conn, revision_uid).await
    }

    /// Loads the most specific visible artifact revision by kind and name.
    pub async fn load_visible(
        &self,
        scope: &ActionRuleScope,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<Option<StoredArtifactRevision>> {
        load_visible_with_status(&self.pool, scope, kind, name, None).await
    }

    /// Loads the most specific visible published artifact revision by kind and name.
    pub async fn load_visible_published(
        &self,
        scope: &ActionRuleScope,
        kind: ArtifactKind,
        name: &str,
    ) -> Result<Option<StoredArtifactRevision>> {
        load_visible_with_status(
            &self.pool,
            scope,
            kind,
            name,
            Some(ArtifactStatus::Published),
        )
        .await
    }

    /// Loads a visible artifact revision by revision id.
    pub async fn load_revision(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
    ) -> Result<Option<StoredArtifactRevision>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            SELECT a.artifact_uid, r.revision_uid, a.storage_partition_id, a.user_id, a.scope,
                   a.kind, a.name, a.description, a.tags, r.definition,
                   r.canonical_hash, r.source_format, r.source_text, r.status,
                   r.validation_report, r.version, r.published_at, r.valid_to,
                   r.created_at, r.updated_at
            FROM moa.artifact a
            JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
            WHERE a.valid_to IS NULL
              AND r.revision_uid = $3
              AND r.valid_to IS NULL
              AND a.storage_partition_id = $1
              AND (a.user_id IS NULL OR a.user_id = $2)
            LIMIT 1
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(revision_uid)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(revision_from_row).transpose()
    }

    /// Lists active artifact revisions visible from the provided scope.
    pub async fn list_visible(
        &self,
        scope: &ActionRuleScope,
        kind: Option<ArtifactKind>,
        status: Option<ArtifactStatus>,
    ) -> Result<Vec<ArtifactSummary>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let rows = sqlx::query(
            r#"
            SELECT DISTINCT ON (a.kind, a.name)
                   a.artifact_uid, r.revision_uid, a.scope, a.kind, a.name,
                   a.description, a.tags, r.status, r.version, r.updated_at
            FROM moa.artifact a
            JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
            WHERE a.valid_to IS NULL
              AND r.valid_to IS NULL
              AND ($3::TEXT IS NULL OR a.kind = $3)
              AND ($4::TEXT IS NULL OR r.status = $4)
              AND a.storage_partition_id = $1
              AND a.user_id IS NULL
            ORDER BY
              a.kind ASC,
              a.name ASC,
              r.version DESC
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(kind.as_ref().map(ToString::to_string))
        .bind(status.as_ref().map(ToString::to_string))
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(summary_from_row).collect()
    }

    /// Loads files attached to a visible revision.
    pub async fn load_files(
        &self,
        scope: &ActionRuleScope,
        revision_uid: Uuid,
    ) -> Result<Vec<ArtifactFile>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let files = load_files(conn.as_mut(), scope, revision_uid).await?;
        conn.commit().await?;
        Ok(files)
    }

    /// Appends a workflow run row.
    pub async fn append_run(
        &self,
        scope: &ActionRuleScope,
        run: NewArtifactRun,
    ) -> Result<ArtifactRun> {
        let parts = ArtifactScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let run_uid = Uuid::now_v7();
        let row = sqlx::query(
            r#"
            INSERT INTO moa.artifact_run (
                run_uid, artifact_uid, revision_uid, tenant_id, storage_partition_id, user_id, session_id,
                workflow_ref, status, current_node_id, input, state, output,
                error, idempotency_key
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
            ON CONFLICT (
                coalesce(storage_partition_id, ''),
                coalesce(user_id, ''),
                workflow_ref,
                idempotency_key
            )
            WHERE idempotency_key IS NOT NULL
            DO UPDATE SET updated_at = moa.artifact_run.updated_at
            RETURNING run_uid, artifact_uid, revision_uid, session_id, workflow_ref, status,
                      current_node_id, input, state, output, error, started_at, completed_at
            "#,
        )
        .bind(run_uid)
        .bind(run.artifact_uid)
        .bind(run.revision_uid)
        .bind(parts.tenant_id)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run.session_id.map(|session_id| session_id.0))
        .bind(&run.workflow_ref)
        .bind(run.status.as_str())
        .bind(run.current_node_id.as_deref())
        .bind(run.input)
        .bind(run.state)
        .bind(run.output)
        .bind(run.error)
        .bind(run.idempotency_key)
        .fetch_one(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        run_from_row(&row)
    }

    /// Loads a visible workflow run by id.
    pub async fn load_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
    ) -> Result<Option<ArtifactRun>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            SELECT run_uid, artifact_uid, revision_uid, session_id, workflow_ref, status,
                   current_node_id, input, state, output, error, started_at, completed_at
            FROM moa.artifact_run
            WHERE run_uid = $3
              AND storage_partition_id = $1
              AND user_id IS NULL
            LIMIT 1
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Updates mutable fields for a visible workflow run and returns the full projection.
    pub async fn update_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        update: ArtifactRunUpdate,
    ) -> Result<Option<ArtifactRun>> {
        let ArtifactRunUpdate {
            status,
            current_node_id,
            state,
            output,
            error,
            completed_at,
        } = update;
        let status_value = status.as_ref().map(ArtifactRunStatus::as_str);
        let status_present = status_value.is_some();
        let current_node_id_present = current_node_id.is_some();
        let current_node_id_value = current_node_id.as_ref().and_then(|value| value.as_deref());
        let state_present = state.is_some();
        let output_present = output.is_some();
        let output_value = output.unwrap_or(None);
        let error_present = error.is_some();
        let error_value = error.unwrap_or(None);
        let completed_at_present = completed_at.is_some();
        let completed_at_value = completed_at.unwrap_or(None);

        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            UPDATE moa.artifact_run
            SET status = CASE WHEN $2 THEN $3::TEXT ELSE status END,
                current_node_id = CASE WHEN $4 THEN $5::TEXT ELSE current_node_id END,
                state = CASE WHEN $6 THEN $7::JSONB ELSE state END,
                output = CASE WHEN $8 THEN $9::JSONB ELSE output END,
                error = CASE WHEN $10 THEN $11::TEXT ELSE error END,
                completed_at = CASE WHEN $12 THEN $13::TIMESTAMPTZ ELSE completed_at END,
                updated_at = now()
            WHERE run_uid = $14
              AND storage_partition_id = $1
              AND user_id IS NULL
            RETURNING run_uid, artifact_uid, revision_uid, session_id, workflow_ref, status,
                      current_node_id, input, state, output, error, started_at, completed_at
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(status_present)
        .bind(status_value)
        .bind(current_node_id_present)
        .bind(current_node_id_value)
        .bind(state_present)
        .bind(state)
        .bind(output_present)
        .bind(output_value)
        .bind(error_present)
        .bind(error_value)
        .bind(completed_at_present)
        .bind(completed_at_value)
        .bind(run_uid)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Marks a visible workflow run as cancelled.
    pub async fn cancel_run(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
        reason: Option<String>,
    ) -> Result<Option<ArtifactRun>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            UPDATE moa.artifact_run
            SET status = 'cancelled',
                error = COALESCE($4, error),
                completed_at = COALESCE(completed_at, now()),
                updated_at = now()
            WHERE run_uid = $3
              AND status NOT IN ('completed', 'failed', 'cancelled')
              AND storage_partition_id = $1
              AND user_id IS NULL
            RETURNING run_uid, artifact_uid, revision_uid, session_id, workflow_ref, status,
                      current_node_id, input, state, output, error, started_at, completed_at
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(run_uid)
        .bind(reason)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(run_from_row).transpose()
    }

    /// Appends a workflow node-run row.
    pub async fn append_node_run(
        &self,
        scope: &ActionRuleScope,
        node_run: NewArtifactNodeRun,
    ) -> Result<Uuid> {
        let parts = ArtifactScopeParts::from_scope(scope);
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        if let Some(existing_uid) = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT node_run_uid
            FROM moa.artifact_node_run
            WHERE run_uid = $2
              AND node_id = $3
              AND storage_partition_id = $1
              AND user_id IS NULL
            ORDER BY started_at ASC, node_run_uid ASC
            LIMIT 1
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(node_run.run_uid)
        .bind(&node_run.node_id)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?
        {
            conn.commit().await?;
            return Ok(existing_uid);
        }
        let node_run_uid = Uuid::now_v7();
        sqlx::query(
            r#"
            INSERT INTO moa.artifact_node_run (
                node_run_uid, run_uid, tenant_id, storage_partition_id, user_id, node_id, status,
                input, output, error, completed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
            "#,
        )
        .bind(node_run_uid)
        .bind(node_run.run_uid)
        .bind(parts.tenant_id)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(&node_run.node_id)
        .bind(node_run.status.as_str())
        .bind(node_run.input)
        .bind(node_run.output)
        .bind(node_run.error)
        .bind(node_run.completed_at)
        .execute(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        Ok(node_run_uid)
    }

    /// Appends missing node-run rows for one workflow run in a single transaction.
    pub async fn append_node_runs(
        &self,
        scope: &ActionRuleScope,
        node_runs: Vec<NewArtifactNodeRun>,
    ) -> Result<Vec<Uuid>> {
        let Some(run_uid) = node_runs.first().map(|node_run| node_run.run_uid) else {
            return Ok(Vec::new());
        };
        if node_runs.iter().any(|node_run| node_run.run_uid != run_uid) {
            return Err(MoaError::ValidationError(
                "append_node_runs requires rows from one workflow run".to_string(),
            ));
        }

        let parts = ArtifactScopeParts::from_scope(scope);
        let node_ids = node_runs
            .iter()
            .map(|node_run| node_run.node_id.clone())
            .collect::<Vec<_>>();
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let existing_rows = sqlx::query(
            r#"
            SELECT node_id, node_run_uid
            FROM moa.artifact_node_run
            WHERE run_uid = $2
              AND node_id = ANY($3)
              AND storage_partition_id = $1
              AND user_id IS NULL
            ORDER BY started_at ASC, node_run_uid ASC
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(run_uid)
        .bind(&node_ids)
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        let mut known_uids = BTreeMap::new();
        for row in existing_rows {
            let node_id: String = row.get("node_id");
            known_uids
                .entry(node_id)
                .or_insert_with(|| row.get::<Uuid, _>("node_run_uid"));
        }

        let mut appended = Vec::with_capacity(node_runs.len());
        for node_run in node_runs {
            if let Some(existing_uid) = known_uids.get(&node_run.node_id) {
                appended.push(*existing_uid);
                continue;
            }

            let node_run_uid = Uuid::now_v7();
            sqlx::query(
                r#"
                INSERT INTO moa.artifact_node_run (
                    node_run_uid, run_uid, tenant_id, storage_partition_id, user_id, node_id, status,
                    input, output, error, completed_at
                )
                VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                "#,
            )
            .bind(node_run_uid)
            .bind(node_run.run_uid)
            .bind(parts.tenant_id)
            .bind(parts.storage_partition_id.as_deref())
            .bind(parts.user_id.as_deref())
            .bind(&node_run.node_id)
            .bind(node_run.status.as_str())
            .bind(node_run.input)
            .bind(node_run.output)
            .bind(node_run.error)
            .bind(node_run.completed_at)
            .execute(&mut *conn.as_mut())
            .await
            .map_err(map_sqlx_error)?;
            known_uids.insert(node_run.node_id, node_run_uid);
            appended.push(node_run_uid);
        }
        conn.commit().await?;
        Ok(appended)
    }

    /// Updates mutable fields for a visible workflow node run.
    pub async fn update_node_run(
        &self,
        scope: &ActionRuleScope,
        node_run_uid: Uuid,
        update: ArtifactNodeRunUpdate,
    ) -> Result<Option<ArtifactNodeRun>> {
        let ArtifactNodeRunUpdate {
            status,
            output,
            error,
            completed_at,
        } = update;
        let status_value = status.as_ref().map(ArtifactNodeRunStatus::as_str);
        let status_present = status_value.is_some();
        let output_present = output.is_some();
        let output_value = output.unwrap_or(None);
        let error_present = error.is_some();
        let error_value = error.unwrap_or(None);
        let completed_at_present = completed_at.is_some();
        let completed_at_value = completed_at.unwrap_or(None);

        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let row = sqlx::query(
            r#"
            UPDATE moa.artifact_node_run
            SET status = CASE WHEN $2 THEN $3::TEXT ELSE status END,
                output = CASE WHEN $4 THEN $5::JSONB ELSE output END,
                error = CASE WHEN $6 THEN $7::TEXT ELSE error END,
                completed_at = CASE WHEN $8 THEN $9::TIMESTAMPTZ ELSE completed_at END,
                updated_at = now()
            WHERE node_run_uid = $10
              AND storage_partition_id = $1
              AND user_id IS NULL
            RETURNING node_run_uid, run_uid, node_id, status, input, output, error,
                      started_at, completed_at
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(status_present)
        .bind(status_value)
        .bind(output_present)
        .bind(output_value)
        .bind(error_present)
        .bind(error_value)
        .bind(completed_at_present)
        .bind(completed_at_value)
        .bind(node_run_uid)
        .fetch_optional(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        row.as_ref().map(node_run_from_row).transpose()
    }

    /// Lists visible node runs for a workflow run in start order.
    pub async fn list_node_runs(
        &self,
        scope: &ActionRuleScope,
        run_uid: Uuid,
    ) -> Result<Vec<ArtifactNodeRun>> {
        let mut conn = ScopedConn::begin(&self.pool, &artifact_scope_context(scope)).await?;
        let parts = ArtifactScopeParts::from_scope(scope);
        let rows = sqlx::query(
            r#"
            SELECT node_run_uid, run_uid, node_id, status, input, output, error,
                   started_at, completed_at
            FROM moa.artifact_node_run
            WHERE run_uid = $2
              AND storage_partition_id = $1
              AND user_id IS NULL
            ORDER BY started_at ASC, node_run_uid ASC
            "#,
        )
        .bind(parts.storage_partition_id.as_deref())
        .bind(run_uid)
        .fetch_all(&mut *conn.as_mut())
        .await
        .map_err(map_sqlx_error)?;
        conn.commit().await?;
        rows.iter().map(node_run_from_row).collect()
    }
}

/// Inserts a published artifact revision inside an existing transaction.
pub async fn insert_published_revision(
    conn: &mut PgConnection,
    parts: &ArtifactScopeParts,
    revision: NewPublishedArtifactRevision<'_>,
) -> Result<Uuid> {
    validate_source_format(revision.source_format)?;
    let artifact_uid = ensure_artifact(conn, parts, revision.document).await?;

    let version = match revision.version {
        Some(version) => version,
        None => next_revision_version(conn, artifact_uid).await?,
    };
    let revision_uid = Uuid::now_v7();
    let definition = serde_json::to_value(revision.document)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?;
    let canonical_hash = canonical_hash(revision.document)
        .map_err(|error| MoaError::SerializationError(error.to_string()))?
        .to_vec();
    let validation_report = serde_json::to_value(validate_for_status(
        revision.document,
        ArtifactStatus::Published,
    ))
    .map_err(|error| MoaError::SerializationError(error.to_string()))?;

    sqlx::query(
        r#"
        INSERT INTO moa.artifact_revision (
            revision_uid, artifact_uid, tenant_id, storage_partition_id, user_id, definition,
            canonical_hash, source_format, source_text, status, validation_report,
            version, published_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'published', $10, $11, now())
        "#,
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .bind(parts.tenant_id)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(definition)
    .bind(canonical_hash)
    .bind(revision.source_format)
    .bind(revision.source_text)
    .bind(validation_report)
    .bind(version)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        "UPDATE moa.artifact SET latest_revision_uid = $1, updated_at = now() WHERE artifact_uid = $2",
    )
    .bind(revision_uid)
    .bind(artifact_uid)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    insert_files(conn, parts, artifact_uid, revision_uid, revision.files).await?;
    Ok(revision_uid)
}

async fn load_visible_with_status(
    pool: &PgPool,
    scope: &ActionRuleScope,
    kind: ArtifactKind,
    name: &str,
    status: Option<ArtifactStatus>,
) -> Result<Option<StoredArtifactRevision>> {
    let mut conn = ScopedConn::begin(pool, &artifact_scope_context(scope)).await?;
    let parts = ArtifactScopeParts::from_scope(scope);
    let row = sqlx::query(
        r#"
        SELECT a.artifact_uid, r.revision_uid, a.storage_partition_id, a.user_id, a.scope,
               a.kind, a.name, a.description, a.tags, r.definition,
               r.canonical_hash, r.source_format, r.source_text, r.status,
               r.validation_report, r.version, r.published_at, r.valid_to,
               r.created_at, r.updated_at
        FROM moa.artifact a
        JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
        WHERE a.valid_to IS NULL
          AND r.valid_to IS NULL
          AND a.kind = $3
          AND a.name = $4
          AND ($5::TEXT IS NULL OR r.status = $5)
          AND a.storage_partition_id = $1
          AND a.user_id IS NULL
        ORDER BY
          r.version DESC
        LIMIT 1
        "#,
    )
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(kind.to_string())
    .bind(name)
    .bind(status.as_ref().map(ToString::to_string))
    .fetch_optional(&mut *conn.as_mut())
    .await
    .map_err(map_sqlx_error)?;
    conn.commit().await?;
    row.as_ref().map(revision_from_row).transpose()
}

async fn ensure_artifact(
    conn: &mut PgConnection,
    parts: &ArtifactScopeParts,
    document: &ArtifactDocument,
) -> Result<Uuid> {
    let active = sqlx::query_scalar::<_, Uuid>(
        r#"
        SELECT artifact_uid
        FROM moa.artifact
        WHERE valid_to IS NULL
          AND storage_partition_id IS NOT DISTINCT FROM $1
          AND user_id IS NOT DISTINCT FROM $2
          AND kind = $3
          AND name = $4
        FOR UPDATE
        "#,
    )
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(document.kind.to_string())
    .bind(&document.metadata.name)
    .fetch_optional(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    if let Some(artifact_uid) = active {
        sqlx::query(
            r#"
            UPDATE moa.artifact
            SET description = $2, tags = $3, updated_at = now()
            WHERE artifact_uid = $1
            "#,
        )
        .bind(artifact_uid)
        .bind(&document.metadata.description)
        .bind(&document.metadata.tags)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
        return Ok(artifact_uid);
    }

    let artifact_uid = Uuid::now_v7();
    sqlx::query(
        r#"
        INSERT INTO moa.artifact (
            artifact_uid, tenant_id, storage_partition_id, user_id, kind, name, description, tags
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        "#,
    )
    .bind(artifact_uid)
    .bind(parts.tenant_id)
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(document.kind.to_string())
    .bind(&document.metadata.name)
    .bind(&document.metadata.description)
    .bind(&document.metadata.tags)
    .execute(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    Ok(artifact_uid)
}

async fn next_revision_version(conn: &mut PgConnection, artifact_uid: Uuid) -> Result<i32> {
    let version = sqlx::query_scalar::<_, Option<i32>>(
        "SELECT max(version) FROM moa.artifact_revision WHERE artifact_uid = $1",
    )
    .bind(artifact_uid)
    .fetch_one(&mut *conn)
    .await
    .map_err(map_sqlx_error)?
    .unwrap_or(0)
    .saturating_add(1);
    Ok(version)
}

async fn insert_files(
    conn: &mut PgConnection,
    parts: &ArtifactScopeParts,
    artifact_uid: Uuid,
    revision_uid: Uuid,
    files: &[NewArtifactFile],
) -> Result<()> {
    for file in files {
        let digest = Sha256::digest(&file.content).to_vec();
        let file_size_bytes = i64::try_from(file.content.len()).map_err(|_| {
            MoaError::ValidationError(format!("artifact file {} is too large", file.path))
        })?;
        sqlx::query(
            r#"
            INSERT INTO moa.artifact_file (
                file_uid, artifact_uid, revision_uid, tenant_id, storage_partition_id, user_id,
                path, content, content_sha256, content_type, executable,
                file_size_bytes
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(Uuid::now_v7())
        .bind(artifact_uid)
        .bind(revision_uid)
        .bind(parts.tenant_id)
        .bind(parts.storage_partition_id.as_deref())
        .bind(parts.user_id.as_deref())
        .bind(&file.path)
        .bind(&file.content)
        .bind(digest)
        .bind(file.content_type.as_deref())
        .bind(file.executable)
        .bind(file_size_bytes)
        .execute(&mut *conn)
        .await
        .map_err(map_sqlx_error)?;
    }
    Ok(())
}

async fn load_revision_by_uid(
    conn: &mut PgConnection,
    revision_uid: Uuid,
) -> Result<StoredArtifactRevision> {
    let row = sqlx::query(
        r#"
        SELECT a.artifact_uid, r.revision_uid, a.storage_partition_id, a.user_id, a.scope,
               a.kind, a.name, a.description, a.tags, r.definition,
               r.canonical_hash, r.source_format, r.source_text, r.status,
               r.validation_report, r.version, r.published_at, r.valid_to,
               r.created_at, r.updated_at
        FROM moa.artifact a
        JOIN moa.artifact_revision r ON r.artifact_uid = a.artifact_uid
        WHERE r.revision_uid = $1
        "#,
    )
    .bind(revision_uid)
    .fetch_one(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;
    revision_from_row(&row)
}

async fn load_files(
    conn: &mut PgConnection,
    scope: &ActionRuleScope,
    revision_uid: Uuid,
) -> Result<Vec<ArtifactFile>> {
    let parts = ArtifactScopeParts::from_scope(scope);
    let rows = sqlx::query(
        r#"
        SELECT f.file_uid, f.path, f.content, f.content_sha256, f.content_type,
               f.executable, f.file_size_bytes
        FROM moa.artifact_file f
        JOIN moa.artifact a ON a.artifact_uid = f.artifact_uid
        WHERE f.revision_uid = $3
          AND a.storage_partition_id = $1
          AND a.user_id IS NULL
        ORDER BY f.path ASC
        "#,
    )
    .bind(parts.storage_partition_id.as_deref())
    .bind(parts.user_id.as_deref())
    .bind(revision_uid)
    .fetch_all(&mut *conn)
    .await
    .map_err(map_sqlx_error)?;

    rows.iter().map(file_from_row).collect()
}

fn revision_from_row(row: &sqlx::postgres::PgRow) -> Result<StoredArtifactRevision> {
    let kind_text: String = row.try_get("kind").map_err(map_sqlx_error)?;
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    let definition: Value = row.try_get("definition").map_err(map_sqlx_error)?;
    Ok(StoredArtifactRevision {
        artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
        revision_uid: row.try_get("revision_uid").map_err(map_sqlx_error)?,
        storage_partition_id: row
            .try_get::<Option<String>, _>("storage_partition_id")
            .map_err(map_sqlx_error)?
            .map(StoragePartitionId::new),
        user_id: row
            .try_get::<Option<String>, _>("user_id")
            .map_err(map_sqlx_error)?
            .map(UserId::new),
        scope: row.try_get("scope").map_err(map_sqlx_error)?,
        kind: kind_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        name: row.try_get("name").map_err(map_sqlx_error)?,
        description: row.try_get("description").map_err(map_sqlx_error)?,
        tags: row
            .try_get::<Option<Vec<String>>, _>("tags")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        document: serde_json::from_value(definition)
            .map_err(|error| MoaError::SerializationError(error.to_string()))?,
        canonical_hash: row.try_get("canonical_hash").map_err(map_sqlx_error)?,
        source_format: row.try_get("source_format").map_err(map_sqlx_error)?,
        source_text: row.try_get("source_text").map_err(map_sqlx_error)?,
        status: status_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        validation_report: row.try_get("validation_report").map_err(map_sqlx_error)?,
        version: row.try_get("version").map_err(map_sqlx_error)?,
        published_at: row.try_get("published_at").map_err(map_sqlx_error)?,
        valid_to: row.try_get("valid_to").map_err(map_sqlx_error)?,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

fn summary_from_row(row: &sqlx::postgres::PgRow) -> Result<ArtifactSummary> {
    let kind_text: String = row.try_get("kind").map_err(map_sqlx_error)?;
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    Ok(ArtifactSummary {
        artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
        revision_uid: row.try_get("revision_uid").map_err(map_sqlx_error)?,
        scope: row.try_get("scope").map_err(map_sqlx_error)?,
        kind: kind_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        name: row.try_get("name").map_err(map_sqlx_error)?,
        description: row.try_get("description").map_err(map_sqlx_error)?,
        tags: row
            .try_get::<Option<Vec<String>>, _>("tags")
            .map_err(map_sqlx_error)?
            .unwrap_or_default(),
        status: status_text
            .parse()
            .map_err(|error: crate::Error| MoaError::StorageError(error.to_string()))?,
        version: row.try_get("version").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

fn file_from_row(row: &sqlx::postgres::PgRow) -> Result<ArtifactFile> {
    Ok(ArtifactFile {
        file_uid: row.try_get("file_uid").map_err(map_sqlx_error)?,
        path: row.try_get("path").map_err(map_sqlx_error)?,
        content: row.try_get("content").map_err(map_sqlx_error)?,
        content_sha256: row.try_get("content_sha256").map_err(map_sqlx_error)?,
        content_type: row.try_get("content_type").map_err(map_sqlx_error)?,
        executable: row.try_get("executable").map_err(map_sqlx_error)?,
        file_size_bytes: row.try_get("file_size_bytes").map_err(map_sqlx_error)?,
    })
}

fn run_from_row(row: &sqlx::postgres::PgRow) -> Result<ArtifactRun> {
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    let session_id = row
        .try_get::<Option<Uuid>, _>("session_id")
        .map_err(map_sqlx_error)?
        .map(SessionId);
    Ok(ArtifactRun {
        run_uid: row.try_get("run_uid").map_err(map_sqlx_error)?,
        artifact_uid: row.try_get("artifact_uid").map_err(map_sqlx_error)?,
        revision_uid: row.try_get("revision_uid").map_err(map_sqlx_error)?,
        session_id,
        workflow_ref: row.try_get("workflow_ref").map_err(map_sqlx_error)?,
        status: run_status_from_str(&status_text)?,
        current_node_id: row.try_get("current_node_id").map_err(map_sqlx_error)?,
        input: row.try_get("input").map_err(map_sqlx_error)?,
        state: row.try_get("state").map_err(map_sqlx_error)?,
        output: row.try_get("output").map_err(map_sqlx_error)?,
        error: row.try_get("error").map_err(map_sqlx_error)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        completed_at: row.try_get("completed_at").map_err(map_sqlx_error)?,
    })
}

fn node_run_from_row(row: &sqlx::postgres::PgRow) -> Result<ArtifactNodeRun> {
    let status_text: String = row.try_get("status").map_err(map_sqlx_error)?;
    Ok(ArtifactNodeRun {
        node_run_uid: row.try_get("node_run_uid").map_err(map_sqlx_error)?,
        run_uid: row.try_get("run_uid").map_err(map_sqlx_error)?,
        node_id: row.try_get("node_id").map_err(map_sqlx_error)?,
        status: node_run_status_from_str(&status_text)?,
        input: row.try_get("input").map_err(map_sqlx_error)?,
        output: row.try_get("output").map_err(map_sqlx_error)?,
        error: row.try_get("error").map_err(map_sqlx_error)?,
        started_at: row.try_get("started_at").map_err(map_sqlx_error)?,
        completed_at: row.try_get("completed_at").map_err(map_sqlx_error)?,
    })
}

fn run_status_from_str(value: &str) -> Result<ArtifactRunStatus> {
    match value {
        "queued" => Ok(ArtifactRunStatus::Queued),
        "running" => Ok(ArtifactRunStatus::Running),
        "pending_review" => Ok(ArtifactRunStatus::PendingReview),
        "completed" => Ok(ArtifactRunStatus::Completed),
        "failed" => Ok(ArtifactRunStatus::Failed),
        "cancelled" => Ok(ArtifactRunStatus::Cancelled),
        _ => Err(MoaError::StorageError(format!(
            "unknown artifact run status: {value}"
        ))),
    }
}

fn node_run_status_from_str(value: &str) -> Result<ArtifactNodeRunStatus> {
    match value {
        "queued" => Ok(ArtifactNodeRunStatus::Queued),
        "running" => Ok(ArtifactNodeRunStatus::Running),
        "pending_review" => Ok(ArtifactNodeRunStatus::PendingReview),
        "completed" => Ok(ArtifactNodeRunStatus::Completed),
        "failed" => Ok(ArtifactNodeRunStatus::Failed),
        "cancelled" => Ok(ArtifactNodeRunStatus::Cancelled),
        "skipped" => Ok(ArtifactNodeRunStatus::Skipped),
        _ => Err(MoaError::StorageError(format!(
            "unknown artifact node run status: {value}"
        ))),
    }
}

fn validate_source_format(source_format: &str) -> Result<()> {
    if matches!(source_format, "json" | "yaml") {
        return Ok(());
    }
    Err(MoaError::ValidationError(format!(
        "unsupported artifact source format: {source_format}"
    )))
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
