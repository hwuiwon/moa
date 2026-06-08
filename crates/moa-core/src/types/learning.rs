//! Learning-log DTOs shared across MOA crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Tenant identifier used for team-level learning state.
pub type TenantId = String;

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
    /// Session or segment identifiers that contributed to the learning.
    pub source_refs: Vec<Uuid>,
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
