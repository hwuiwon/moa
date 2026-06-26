//! Tenant knowledge-base service wire DTOs.

use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Request payload for creating a linked-account token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeCreateLinkTokenRequest {
    /// Tenant that will own the linked connection.
    pub tenant_id: TenantId,
    /// Linked-account provider identifier, such as `nango` or `merge`.
    pub provider: String,
    /// Connector identifier within the linked-account provider.
    pub connector: String,
    /// Optional caller-facing account or contact reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_account_id: Option<String>,
}

/// Response payload containing a linked-account token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeCreateLinkTokenResponse {
    /// Provider that issued the token.
    pub provider: String,
    /// Short-lived link token or hosted-link URL token.
    pub link_token: String,
    /// Optional provider-specific hosted link URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
    /// Expiration timestamp for the link token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Request payload for exchanging a provider link token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeExchangeTokenRequest {
    /// Tenant that will own the linked connection.
    pub tenant_id: TenantId,
    /// Linked-account provider identifier.
    pub provider: String,
    /// Token or code returned by the provider link flow.
    pub exchange_token: String,
}

/// Response payload for a completed linked-account token exchange.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeExchangeTokenResponse {
    /// Tenant-owned connection identifier.
    pub connection_uid: Uuid,
    /// Provider that owns the connection.
    pub provider: String,
    /// Provider connector identifier.
    pub connector: String,
    /// Provider account identifier.
    pub provider_account_id: String,
}

/// Request payload for starting a tenant knowledge sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncRequest {
    /// Tenant that owns the connection.
    pub tenant_id: TenantId,
    /// Tenant-owned connection identifier.
    pub connection_uid: Uuid,
    /// Optional parser override for this sync run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser: Option<String>,
    /// Optional maximum records to process in this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_records: Option<u32>,
}

/// Response payload for a started tenant knowledge sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncResponse {
    /// Tenant-owned sync-run identifier.
    pub sync_run_uid: Uuid,
    /// Current sync-run status.
    pub status: String,
    /// Timestamp when the sync run was accepted.
    pub started_at: DateTime<Utc>,
}

/// Request payload for reading sync-run status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncStatusRequest {
    /// Tenant that owns the sync run.
    pub tenant_id: TenantId,
    /// Tenant-owned sync-run identifier.
    pub sync_run_uid: Uuid,
}

/// Response payload for sync-run status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncStatusResponse {
    /// Tenant-owned sync-run identifier.
    pub sync_run_uid: Uuid,
    /// Current sync-run status.
    pub status: String,
    /// Number of source records observed.
    pub records_seen: u64,
    /// Number of source records ingested.
    pub records_ingested: u64,
    /// Number of source records that failed.
    pub records_failed: u64,
    /// Ordered sync-run step summaries.
    #[serde(default)]
    pub steps: Vec<KnowledgeSyncStepView>,
    /// Timestamp when the sync run started.
    pub started_at: DateTime<Utc>,
    /// Timestamp when the sync run finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// Request payload for reading sync-run events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncEventsRequest {
    /// Tenant that owns the sync run.
    pub tenant_id: TenantId,
    /// Tenant-owned sync-run identifier.
    pub sync_run_uid: Uuid,
    /// Optional object filter for per-object ingestion timelines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_uid: Option<Uuid>,
    /// Optional pagination cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Maximum events to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Response payload containing sync-run event rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncEventsResponse {
    /// Sync event rows ordered by creation time.
    #[serde(default)]
    pub events: Vec<KnowledgeSyncStepView>,
    /// Cursor for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Renderer-facing view of one knowledge sync step or event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncStepView {
    /// Stable step or event identifier.
    pub step_uid: Uuid,
    /// Step kind, such as `list`, `fetch`, `parse`, `chunk`, or `graph_write`.
    pub step: String,
    /// Current step status.
    pub status: String,
    /// Optional source object identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_uid: Option<Uuid>,
    /// Short renderer-safe preview or status detail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Redacted structured step metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Timestamp when the step row was created.
    pub created_at: DateTime<Utc>,
}

/// Tenant knowledge connection summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnectionSummary {
    /// Tenant-owned connection identifier.
    pub connection_uid: Uuid,
    /// Linked-account provider identifier.
    pub provider: String,
    /// Provider connector identifier.
    pub connector: String,
    /// Provider account identifier.
    pub provider_account_id: String,
    /// Current connection status.
    pub status: String,
    /// Most recent sync-run status, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_status: Option<String>,
    /// Timestamp of the last successful sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// Request payload for listing tenant knowledge connections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnectionListRequest {
    /// Tenant that owns the linked connections.
    pub tenant_id: TenantId,
    /// Optional linked-account provider filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
}

/// Response payload containing tenant knowledge connection summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnectionListResponse {
    /// Linked connection summaries ordered by recent update time.
    #[serde(default)]
    pub connections: Vec<KnowledgeConnectionSummary>,
}

/// Request payload for listing tenant knowledge source objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectListRequest {
    /// Tenant that owns the objects.
    pub tenant_id: TenantId,
    /// Optional connection filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_uid: Option<Uuid>,
    /// Optional source object kind filter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_type: Option<String>,
    /// Optional pagination cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Maximum objects to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Response payload containing tenant knowledge source objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectListResponse {
    /// Object summaries ordered by update time.
    #[serde(default)]
    pub objects: Vec<Value>,
    /// Cursor for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Request payload for inspecting one tenant knowledge object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectInspectRequest {
    /// Tenant that owns the object.
    pub tenant_id: TenantId,
    /// Tenant-owned knowledge object identifier.
    pub object_uid: Uuid,
}

/// Response payload for inspecting one tenant knowledge object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectInspectResponse {
    /// Tenant-owned knowledge object identifier.
    pub object_uid: Uuid,
    /// Source object kind.
    pub object_type: String,
    /// Provider source identifier.
    pub source_id: String,
    /// Current ingestion status for this object.
    pub status: String,
    /// Renderer-safe object preview.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// Latest document version identifier, when parsed content exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_uid: Option<Uuid>,
    /// Parser that produced the latest document version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser: Option<String>,
    /// Safe parser metadata for the latest document version.
    #[serde(default)]
    pub parser_metadata: Value,
    /// Unique heading paths observed in current chunks.
    #[serde(default)]
    pub heading_paths: Vec<Vec<String>>,
    /// Current chunk summaries for inspection and citation rendering.
    #[serde(default)]
    pub chunks: Vec<KnowledgeObjectChunkInspectView>,
    /// Graph node UIDs written for current chunks.
    #[serde(default)]
    pub graph_node_uids: Vec<Uuid>,
    /// Safe citation metadata assembled from chunk metadata.
    #[serde(default)]
    pub citation_metadata: Value,
    /// Redacted source metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Sync and ingestion steps for this object.
    #[serde(default)]
    pub steps: Vec<KnowledgeSyncStepView>,
}

/// Renderer-safe summary of one tenant knowledge chunk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectChunkInspectView {
    /// Tenant-owned chunk identifier.
    pub chunk_uid: Uuid,
    /// Chunk ordinal within the document version.
    pub ordinal: u32,
    /// Stable chunk content hash.
    pub chunk_hash: String,
    /// Heading path active for the chunk.
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Approximate token count.
    pub token_count: usize,
    /// Bounded safe text preview for this chunk.
    pub preview: String,
    /// Graph node UID written for this chunk, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_node_uid: Option<Uuid>,
    /// Safe chunk metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Provider webhook payload accepted by the knowledge service.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeProviderWebhookRequest {
    /// Linked-account provider that emitted the webhook.
    pub provider: String,
    /// Provider event identifier for idempotency.
    pub event_id: String,
    /// Raw provider event type.
    pub event_type: String,
    /// Redacted provider payload.
    #[serde(default)]
    pub payload: Value,
    /// Provider HTTP headers forwarded for signature verification.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Base64-encoded raw webhook body. When absent, `payload` is serialized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
}

/// Response payload for a processed provider webhook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeProviderWebhookResponse {
    /// Provider that emitted the verified event.
    pub provider: String,
    /// Provider event id used for idempotency.
    pub event_id: String,
    /// Stored event status.
    pub status: String,
    /// Whether this delivery was a duplicate of a previously recorded event.
    pub duplicate: bool,
    /// Local sync run touched by this event, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_run_uid: Option<Uuid>,
    /// Whether ingestion was enqueued for this delivery.
    pub ingestion_enqueued: bool,
}

/// Request payload for reading a tenant knowledge query trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeQueryTraceRequest {
    /// Tenant that owns the query trace.
    pub tenant_id: TenantId,
    /// Stable query trace identifier.
    pub trace_uid: Uuid,
}

/// Response payload containing a tenant knowledge query trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeQueryTraceResponse {
    /// Stable query trace identifier.
    pub trace_uid: Uuid,
    /// Original caller query.
    pub original_query: String,
    /// Retrieval query after query rewriting, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_query: Option<String>,
    /// Searched source scopes.
    #[serde(default)]
    pub searched_scopes: Vec<String>,
    /// Per-stage trace records.
    #[serde(default)]
    pub stages: Vec<KnowledgeQueryTraceStage>,
    /// Selected trace hits.
    #[serde(default)]
    pub hits: Vec<KnowledgeQueryTraceHit>,
    /// Timestamp when the trace was captured.
    pub created_at: DateTime<Utc>,
}

/// One stage in a tenant knowledge query trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeQueryTraceStage {
    /// Stage name, such as `graph`, `vector`, `lexical`, or `reranker`.
    pub stage: String,
    /// Number of candidates produced by this stage.
    pub candidate_count: u32,
    /// Stage latency in milliseconds.
    pub latency_ms: u64,
    /// Redacted stage metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// One selected hit in a tenant knowledge query trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeQueryTraceHit {
    /// Stable graph node or chunk identifier.
    pub uid: Uuid,
    /// Source tier, such as tenant knowledge or contact memory.
    pub source_tier: String,
    /// Hit label or object type.
    pub label: String,
    /// Renderer-safe hit title.
    pub title: String,
    /// Short renderer-safe snippet.
    pub snippet: String,
    /// Retrieval score assigned to the hit.
    pub score: f64,
    /// Citation metadata for the hit.
    #[serde(default)]
    pub citation: Value,
}
