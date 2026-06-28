//! Lineage administration wire DTOs.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Request payload for explaining lineage for one session or turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageExplainRequest {
    /// Tenant containing the session or turn to explain.
    pub tenant_id: TenantId,
    /// Session or turn identifier to explain.
    pub id: Uuid,
}

/// Response payload containing lineage records for one session or turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageExplainResponse {
    /// Identifier that was explained.
    pub id: Uuid,
    /// Lineage records ordered by timestamp and kind.
    #[serde(default)]
    pub records: Vec<LineageRecordView>,
}

/// Transport-safe view of one lineage record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageRecordView {
    /// Turn identifier associated with the lineage record.
    pub turn_id: Uuid,
    /// Session identifier associated with the lineage record, when available.
    pub session_id: Option<SessionId>,
    /// Tenant associated with the lineage record, when available.
    pub tenant_id: Option<TenantId>,
    /// User associated with the lineage record, when available.
    pub user_id: Option<UserId>,
    /// Timestamp when the lineage record was captured.
    pub ts: DateTime<Utc>,
    /// Numeric lineage record kind.
    pub record_kind: i16,
    /// Raw lineage payload.
    pub payload: Value,
    /// Optional renderer-ready one-line summary.
    pub summary: Option<String>,
}

/// Request payload for querying hot lineage records with typed filters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineageQueryRequest {
    /// Tenant filter for authorization and query scoping.
    pub tenant_id: TenantId,
    /// Optional typed filters applied to `analytics.turn_lineage`.
    #[serde(default)]
    pub filters: LineageQueryFilters,
    /// Timestamp order for returned records.
    #[serde(default)]
    pub order: LineageQueryOrder,
    /// Maximum number of rows to return. The edge clamps this to a bounded range.
    #[serde(default = "default_lineage_query_limit")]
    pub limit: u32,
}

/// Optional filters supported by the direct hot lineage query route.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LineageQueryFilters {
    /// Optional turn identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    /// Optional session identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// Optional lineage user identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<UserId>,
    /// Optional numeric lineage record kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_kind: Option<i16>,
    /// Optional inclusive lower timestamp bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_time: Option<DateTime<Utc>>,
    /// Optional inclusive upper timestamp bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_time: Option<DateTime<Utc>>,
}

/// Supported timestamp ordering for lineage query rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageQueryOrder {
    /// Newest records first.
    #[default]
    TimestampDesc,
    /// Oldest records first.
    TimestampAsc,
}

/// Response payload containing typed lineage query rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageQueryResponse {
    /// Query rows ordered by timestamp and record kind.
    #[serde(default)]
    pub rows: Vec<LineageRecordView>,
}

fn default_lineage_query_limit() -> u32 {
    100
}

/// Request payload for exporting a lineage DSAR bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageExportRequest {
    /// Tenant whose lineage records should be exported.
    pub tenant_id: TenantId,
    /// Subject pseudonym or natural identifier to search for.
    pub subject: String,
}

/// Response payload describing an exported lineage DSAR bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageExportResponse {
    /// URI where the exported bundle can be fetched.
    pub bundle_uri: String,
    /// Number of lineage records included in the bundle.
    pub record_count: u64,
    /// Hash of the exported subject pseudonym.
    pub subject_hash: String,
    /// Optional base64-encoded archive for transports that inline small bundles.
    pub archive_base64: Option<String>,
}

/// Request payload for verifying lineage integrity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageVerifyRequest {
    /// Tenant whose lineage window should be verified.
    pub tenant_id: TenantId,
    /// `hot`, an audit root UUID, or an audit root object URI.
    pub window: String,
    /// Postgres interval for hot-window verification.
    pub since: String,
}

/// Response payload describing lineage verification results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageVerifyResponse {
    /// Tenant whose lineage window was verified.
    pub tenant_id: TenantId,
    /// Number of records verified.
    pub records: u64,
    /// Whether the verification checked an audit root.
    pub root_checked: bool,
    /// Verification status label.
    pub status: String,
    /// Audit root identifier when one was checked.
    pub root_id: Option<Uuid>,
}

/// Request payload for erasing lineage subject keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageEraseRequest {
    /// Tenant containing the subject pseudonym.
    pub tenant_id: TenantId,
    /// Hex-encoded subject pseudonym.
    pub subject: String,
}

/// Response payload for a lineage erase request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageEraseResponse {
    /// Tenant containing the erased subject pseudonym.
    pub tenant_id: TenantId,
    /// Number of matching subjects scheduled for erasure.
    pub subjects: u64,
    /// Erasure status label.
    pub status: String,
}
