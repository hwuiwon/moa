//! Postgres-backed artifact registry with MOA three-tier visibility.

use chrono::{DateTime, Utc};
use moa_core::types::memory::RlsContext;
use moa_core::{
    error::MoaError, error::Result, types::action_policy::ActionRuleScope,
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

mod contributions;
#[cfg(feature = "test-support")]
pub(crate) mod release;
#[cfg(not(feature = "test-support"))]
mod release;
mod revisions;
mod serving;
mod skill_embeddings;

pub use contributions::{
    NewRevisionContribution, NewSuiteContribution, RevisionContributionKind,
    StoredSuiteContribution, SuiteContributionKind,
};
pub use release::{
    CandidateSubjectInputs, CandidateSubmission, DecisionOutcome, RecordDecision, ReleaseCandidate,
    ReleaseRepository, SubmitCandidate,
};
pub use revisions::RollbackApplication;
pub use serving::{ActivationProvenance, ServingPointer};
pub use skill_embeddings::{
    MissingSkillEmbedding, NamedSkillEmbeddingNeighbor, NewSkillEmbedding, SkillEmbeddingNeighbor,
};

/// Maximum size, in bytes, accepted for a single stored artifact package file.
///
/// Artifact files are skill/agent package assets (instructions, configs, small
/// scripts), so a 10 MiB ceiling rejects abusive uploads long before the
/// `i64` byte-count conversion could overflow.
pub const MAX_FILE_SIZE_BYTES: usize = 10 * 1024 * 1024;

/// Maximum number of package files accepted for one artifact revision.
///
/// The cap bounds both request validation and the parameter count of the
/// set-based artifact-file insert.
pub const MAX_FILES_PER_REVISION: usize = 128;

/// Maximum combined package-file bytes accepted for one artifact revision.
///
/// This 64 MiB request ceiling applies in addition to
/// [`MAX_FILE_SIZE_BYTES`], so many individually valid files cannot make one
/// revision transaction grow without bound.
pub const MAX_TOTAL_FILE_SIZE_BYTES: usize = 64 * 1024 * 1024;

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

    /// Returns the pool this registry reads and writes through.
    ///
    /// The release repository and the artifact registry are separate surfaces over
    /// the same pool; callers that hold one and need the other use this instead of
    /// threading a second handle everywhere.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

fn map_sqlx_error(error: sqlx::Error) -> MoaError {
    MoaError::StorageError(error.to_string())
}
