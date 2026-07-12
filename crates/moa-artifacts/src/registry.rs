//! Postgres-backed artifact registry with MOA three-tier visibility.

use std::{collections::BTreeMap, str::FromStr};

use chrono::{DateTime, Utc};
use moa_core::types::memory::RlsContext;
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
    types::contact::ContactId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::UserId,
};
use moa_db::ScopedConn;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::canonical::canonical_hash;
use crate::document::{ArtifactDocument, ArtifactKind, ArtifactStatus};
use crate::validation::ValidationReport;

mod revisions;
mod runs;
mod skill_embeddings;

pub use revisions::{RollbackApplication, insert_published_revision};
pub use skill_embeddings::{MissingSkillEmbedding, NewSkillEmbedding, SkillEmbeddingNeighbor};

/// Maximum size, in bytes, accepted for a single stored artifact package file.
///
/// Artifact files are skill/agent package assets (instructions, configs, small
/// scripts), so a 10 MiB ceiling rejects abusive uploads long before the
/// `i64` byte-count conversion could overflow.
pub const MAX_FILE_SIZE_BYTES: usize = 10 * 1024 * 1024;
const DEFAULT_RUN_PAGE_LIMIT: usize = 50;
const MAX_RUN_PAGE_LIMIT: usize = 200;

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
            ActionRuleScope::Contact {
                tenant_id,
                contact_id,
            } => Self {
                tenant_id: Some(tenant_id.0),
                storage_partition_id: Some(StoragePartitionId::for_tenant(*tenant_id).to_string()),
                user_id: Some(contact_id.to_string()),
            },
        }
    }
}

fn artifact_scope_context(scope: &ActionRuleScope) -> RlsContext {
    match scope {
        ActionRuleScope::Tenant { tenant_id } => RlsContext::tenant(*tenant_id),
        ActionRuleScope::Contact {
            tenant_id,
            contact_id,
        } => RlsContext::contact(*tenant_id, *contact_id),
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

/// Procedure run status persisted for artifacts.
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

impl FromStr for ArtifactRunStatus {
    type Err = MoaError;

    fn from_str(value: &str) -> Result<Self> {
        runs::run_status_from_str(value)
    }
}

/// Procedure node-run status persisted for artifacts.
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

/// New procedure run row.
#[derive(Clone, Debug, PartialEq)]
pub struct NewArtifactRun {
    /// Referenced artifact UID, if already resolved.
    pub artifact_uid: Option<Uuid>,
    /// Referenced revision UID, if already resolved.
    pub revision_uid: Option<Uuid>,
    /// Session associated with this procedure run, when the run was started from a session.
    pub session_id: Option<SessionId>,
    /// Procedure reference string, pointing at the skill artifact that carries the procedure.
    pub procedure_ref: String,
    /// Initial run status.
    pub status: ArtifactRunStatus,
    /// Current node ID.
    pub current_node_id: Option<String>,
    /// Input payload.
    pub input: Value,
    /// Mutable procedure state.
    pub state: Value,
    /// Output payload.
    pub output: Option<Value>,
    /// Error text.
    pub error: Option<String>,
    /// Optional idempotency key.
    pub idempotency_key: Option<String>,
}

/// Stored procedure run row.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactRun {
    /// Run row identifier.
    pub run_uid: Uuid,
    /// Referenced artifact UID, if the run was started from a resolved artifact.
    pub artifact_uid: Option<Uuid>,
    /// Referenced revision UID, if the run was started from a resolved artifact revision.
    pub revision_uid: Option<Uuid>,
    /// Session associated with this procedure run, when present.
    pub session_id: Option<SessionId>,
    /// Procedure reference string, pointing at the skill artifact that carries the procedure.
    pub procedure_ref: String,
    /// Current status.
    pub status: ArtifactRunStatus,
    /// Current node ID.
    pub current_node_id: Option<String>,
    /// Input payload.
    pub input: Value,
    /// Mutable procedure state.
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

/// Keyset cursor for listing procedure runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactRunListCursor {
    /// Last seen run start timestamp.
    pub started_at: DateTime<Utc>,
    /// Last seen run identifier at that timestamp.
    pub run_uid: Uuid,
}

/// Request for listing visible procedure runs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArtifactRunListRequest {
    /// Optional status filter.
    pub status: Option<ArtifactRunStatus>,
    /// Maximum number of rows to return.
    pub limit: Option<usize>,
    /// Cursor returned by a previous page.
    pub cursor: Option<ArtifactRunListCursor>,
}

/// One page of visible procedure runs.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactRunPage {
    /// Runs in descending start order.
    pub runs: Vec<ArtifactRun>,
    /// Cursor for the next page when more rows are available.
    pub next_cursor: Option<ArtifactRunListCursor>,
}

/// Registry patch payload for mutable procedure run fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArtifactRunUpdate {
    /// Replacement status when present.
    pub status: Option<ArtifactRunStatus>,
    /// Replacement current node ID when present, including clearing it to `NULL`.
    pub current_node_id: Option<Option<String>>,
    /// Replacement procedure state when present.
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
    /// Procedure node ID.
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

/// Stored procedure node-run row.
#[derive(Clone, Debug, PartialEq)]
pub struct ArtifactNodeRun {
    /// Node-run row identifier.
    pub node_run_uid: Uuid,
    /// Parent run identifier.
    pub run_uid: Uuid,
    /// Procedure node ID.
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

/// Registry patch payload for mutable procedure node-run fields.
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
#[derive(Clone)]
pub struct ArtifactRegistry {
    pool: PgPool,
}

impl ArtifactRegistry {
    /// Creates an artifact registry backed by a Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
