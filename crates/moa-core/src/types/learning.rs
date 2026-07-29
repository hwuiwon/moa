//! Learning-log DTOs shared across MOA crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::identifiers::{SegmentId, SessionId, TenantId};

/// One typed provenance reference standing behind a learning-log entry.
///
/// The column this replaces was a bare `UUID[]` documented as "session or
/// segment identifiers", which meant nothing could tell the two apart, and in
/// practice it also carried candidate and experience ids. An erasure walking
/// that array had to guess which table to look in; a guess is not a derivation
/// chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LearningLogSourceRef {
    /// The reviewed learning candidate this entry records the outcome of.
    Candidate {
        /// Candidate the entry was derived from.
        candidate_id: Uuid,
    },
    /// An experience record that motivated the learning.
    Experience {
        /// Experience the entry was derived from.
        experience_id: Uuid,
    },
    /// A session that contributed to the learning.
    Session {
        /// Session the entry was derived from.
        session_id: SessionId,
    },
    /// One assessed task segment that contributed to the learning.
    TaskSegment {
        /// Segment the entry was derived from.
        segment_id: SegmentId,
    },
    /// An artifact revision the learning applies to.
    ArtifactRevision {
        /// Revision the entry references.
        revision_uid: Uuid,
    },
}

impl LearningLogSourceRef {
    /// Returns the stable `source_kind` discriminator persisted alongside the reference.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Candidate { .. } => "candidate",
            Self::Experience { .. } => "experience",
            Self::Session { .. } => "session",
            Self::TaskSegment { .. } => "task_segment",
            Self::ArtifactRevision { .. } => "artifact_revision",
        }
    }
}

/// Append-only learning-log entry for learned patterns and derived updates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LearningEntry {
    /// Stable learning entry identifier.
    pub id: Uuid,
    /// Tenant scope for the learning.
    pub tenant_id: TenantId,
    /// Machine-readable learning type.
    pub learning_type: String,
    /// Identifier of the learned target.
    pub target_id: String,
    /// Optional human-readable target label.
    pub target_label: Option<String>,
    /// Structured payload containing full learning details.
    pub payload: serde_json::Value,
    /// Confidence score from 0.0 to 1.0, when available.
    pub confidence: Option<f64>,
    /// Typed sources that contributed to the learning.
    ///
    /// Committed in the same transaction as the entry, so an append can never
    /// leave an entry standing with no traceable derivation.
    #[serde(default)]
    pub sources: Vec<LearningLogSourceRef>,
    /// Actor that recorded the learning.
    pub actor: String,
    /// Time from which this learning version is valid.
    pub valid_from: DateTime<Utc>,
    /// Time at which this learning version was superseded or rolled back.
    pub valid_to: Option<DateTime<Utc>>,
    /// Optional batch identifier for grouped rollback.
    pub batch_id: Option<Uuid>,
    /// Monotonic target version.
    pub version: i32,
}
