//! Intent taxonomy and learning-log DTOs shared across MOA crates.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Tenant identifier used for team-level learning state.
pub type TenantId = String;

/// Tenant-scoped intent definition used for classification and admin review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantIntent {
    /// Stable intent identifier.
    pub id: Uuid,
    /// Tenant that owns this taxonomy entry.
    pub tenant_id: TenantId,
    /// Short human-readable intent label.
    pub label: String,
    /// Optional one-sentence description.
    pub description: Option<String>,
    /// Lifecycle status for the intent.
    pub status: IntentStatus,
    /// Source from which the intent entered the tenant taxonomy.
    pub source: IntentSource,
    /// Optional global catalog reference when adopted from the curated library.
    pub catalog_ref: Option<Uuid>,
    /// Representative user queries for the intent.
    pub example_queries: Vec<String>,
    /// Optional centroid embedding used for nearest-centroid classification.
    pub embedding: Option<Vec<f32>>,
    /// Number of segments currently assigned to the intent.
    pub segment_count: u32,
    /// Tenant-level resolution rate for segments assigned to the intent.
    pub resolution_rate: Option<f64>,
}

/// Intent lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus {
    /// Candidate discovered by the learning pipeline and awaiting admin review.
    Proposed,
    /// Confirmed intent used for ongoing classification.
    Active,
    /// Retired intent retained for history and auditability.
    Deprecated,
}

/// Origin of a tenant intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentSource {
    /// Automatically discovered from tenant conversation patterns.
    Discovered,
    /// Manually created by a tenant admin.
    Manual,
    /// Adopted from the global platform catalog.
    Catalog,
}

/// Platform-curated intent available for tenant opt-in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogIntent {
    /// Stable catalog identifier.
    pub id: Uuid,
    /// Short canonical label.
    pub label: String,
    /// Catalog description.
    pub description: String,
    /// Optional broad catalog category.
    pub category: Option<String>,
    /// Representative user queries for the catalog entry.
    pub example_queries: Vec<String>,
    /// Optional catalog centroid embedding copied into tenant adoptions.
    pub embedding: Option<Vec<f32>>,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
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
