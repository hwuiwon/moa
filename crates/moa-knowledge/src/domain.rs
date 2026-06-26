//! Domain types for tenant knowledge connections, parsing, blocks, and chunks.

use chrono::{DateTime, Utc};
use moa_core::{ContactId, TenantId};
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Linked-account provider identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkedProviderKind {
    /// Nango linked-account provider.
    Nango,
    /// Merge linked-account provider.
    Merge,
}

impl LinkedProviderKind {
    /// Returns the stable provider identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Nango => "nango",
            Self::Merge => "merge",
        }
    }
}

/// Parser provider identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParserKind {
    /// Local native parser backed by deterministic MOA parsing and liteparse.
    Native,
    /// LlamaParse cloud parser.
    LlamaParse,
    /// Unstructured partitioning parser.
    Unstructured,
    /// Reducto parser.
    Reducto,
}

impl ParserKind {
    /// Returns the stable parser identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::LlamaParse => "llamaparse",
            Self::Unstructured => "unstructured",
            Self::Reducto => "reducto",
        }
    }
}

/// One linked external account for one tenant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnection {
    /// Tenant-owned connection identifier.
    pub connection_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Linked-account provider.
    pub provider: String,
    /// Provider connector identifier.
    pub connector: String,
    /// Provider account identifier.
    pub provider_account_id: String,
    /// Credential vault reference, never raw credentials.
    pub credential_ref: String,
    /// Current connection status.
    pub status: ConnectionStatus,
    /// Safe provider metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Last successful sync timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_synced_at: Option<DateTime<Utc>>,
}

/// Connection lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionStatus {
    /// Link flow has been created but not completed.
    Pending,
    /// Connection can sync records.
    Active,
    /// Connection is disabled.
    Disabled,
    /// Provider reported a recoverable or terminal error.
    Error,
}

impl ConnectionStatus {
    /// Returns the stable database status identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Disabled => "disabled",
            Self::Error => "error",
        }
    }
}

/// One local sync and ingestion attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncRun {
    /// Sync-run identifier.
    pub sync_run_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Linked connection.
    pub connection_uid: Uuid,
    /// Parser selected for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser: Option<String>,
    /// Current run status.
    pub status: SyncRunStatus,
    /// Number of source records observed.
    pub records_seen: u64,
    /// Number of records ingested.
    pub records_ingested: u64,
    /// Number of records failed.
    pub records_failed: u64,
    /// Run start time.
    pub started_at: DateTime<Utc>,
    /// Run finish time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// Counter update accumulated while processing one sync run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KnowledgeSyncCounters {
    /// Number of source records observed.
    pub records_seen: u64,
    /// Number of records whose content changed.
    pub records_changed: u64,
    /// Number of provider-deleted records.
    pub records_deleted: u64,
    /// Number of records successfully ingested.
    pub records_ingested: u64,
    /// Number of records that failed ingestion.
    pub records_failed: u64,
    /// Number of objects parsed.
    pub objects_parsed: u64,
    /// Number of chunks embedded.
    pub chunks_embedded: u64,
    /// Number of graph nodes upserted.
    pub graph_nodes_upserted: u64,
    /// Number of graph edges upserted.
    pub graph_edges_upserted: u64,
}

/// Sync-run status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRunStatus {
    /// Accepted but not yet doing provider work.
    Pending,
    /// Provider and ingestion work is running.
    Running,
    /// Run completed successfully.
    Completed,
    /// Run completed with some failed records.
    PartialFailure,
    /// Run failed.
    Failed,
}

impl SyncRunStatus {
    /// Returns the stable database status identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::PartialFailure => "partial_failure",
            Self::Failed => "failed",
        }
    }
}

/// Source-side object such as a file, page, ticket, message, or CRM record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObject {
    /// Tenant-owned object identifier.
    pub object_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Owning connection.
    pub connection_uid: Uuid,
    /// Source object type.
    pub object_type: String,
    /// Provider source identifier.
    pub source_id: String,
    /// Optional stable parent object identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_source_id: Option<String>,
    /// Source URI when safe to keep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Renderer-safe title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Provider change token or etag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_token: Option<String>,
    /// Safe source metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Current ingestion status.
    pub status: ObjectStatus,
    /// Source update time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_updated_at: Option<DateTime<Utc>>,
    /// Soft deletion timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,
}

/// Knowledge object status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectStatus {
    /// Object was observed but not ingested.
    Pending,
    /// Object has active parsed content.
    Active,
    /// Object was deleted at the provider.
    Deleted,
    /// Object failed ingestion.
    Error,
}

impl ObjectStatus {
    /// Returns the stable database status identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Deleted => "deleted",
            Self::Error => "error",
        }
    }
}

/// One immutable parsed content version for a source object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentVersion {
    /// Version identifier.
    pub version_uid: Uuid,
    /// Owning object.
    pub object_uid: Uuid,
    /// Parser identifier.
    pub parser: String,
    /// Parser job identifier when supplied by an external parser.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser_job_id: Option<String>,
    /// Content hash for this version.
    pub content_hash: String,
    /// Parser metadata safe for inspection.
    #[serde(default)]
    pub metadata: Value,
    /// Version creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Input accepted by a document parser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParseInput {
    /// Object being parsed.
    pub object: KnowledgeObject,
    /// Optional file name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// Optional MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Optional source URL or presigned URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Optional raw bytes for local parsing or upload-style APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<Vec<u8>>,
    /// Optional already-normalized text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Parser options.
    #[serde(default)]
    pub options: Value,
}

/// Parser output normalized across native and external parsers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParsedDocument {
    /// Parser identifier.
    pub parser: String,
    /// Parser job identifier when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser_job_id: Option<String>,
    /// Rendered full text or markdown.
    pub text: String,
    /// Structured document elements.
    #[serde(default)]
    pub elements: Vec<DocumentElement>,
    /// Safe parser metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Parser output unit such as heading, paragraph, table, field, or block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentElement {
    /// Stable parser element identifier.
    pub element_id: String,
    /// Normalized element kind.
    pub kind: DocumentElementKind,
    /// Element text.
    pub text: String,
    /// Heading path active for this element.
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Document-order ordinal.
    pub ordinal: u32,
    /// 1-based page number when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_number: Option<u32>,
    /// Layout metadata when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<ElementLayout>,
    /// Safe parser-specific metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Normalized parser element kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentElementKind {
    /// Heading or title.
    Heading,
    /// Paragraph or narrative text.
    Paragraph,
    /// List item.
    ListItem,
    /// Table or table row.
    Table,
    /// Message-like source object.
    Message,
    /// Field or record attribute.
    Field,
    /// Attachment reference.
    Attachment,
    /// Figure or caption.
    Figure,
    /// Page boundary or page-level text.
    Page,
    /// Parser chunk structure.
    ParserChunk,
    /// Unknown but text-bearing element.
    Other,
}

/// Layout metadata for visual citations.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ElementLayout {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Width.
    pub width: f32,
    /// Height.
    pub height: f32,
    /// Page width when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_width: Option<f32>,
    /// Page height when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_height: Option<f32>,
    /// OCR or parser confidence when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

/// Normalized atomic knowledge unit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeBlock {
    /// Block identifier.
    pub block_uid: Uuid,
    /// Owning document version.
    pub version_uid: Uuid,
    /// Source element identifier.
    pub element_id: String,
    /// Deterministic block content hash.
    pub block_hash: String,
    /// Normalized text used for hashing and chunking.
    pub normalized_text: String,
    /// Heading path used for citation rendering.
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Document-order ordinal.
    pub ordinal: u32,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Retrieval-sized group of consecutive knowledge blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeChunk {
    /// Chunk identifier.
    pub chunk_uid: Uuid,
    /// Owning document version.
    pub version_uid: Uuid,
    /// Graph node UID written for this chunk, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_node_uid: Option<Uuid>,
    /// Deterministic chunk content hash.
    pub chunk_hash: String,
    /// Ordered source block hashes.
    pub block_hashes: Vec<String>,
    /// Chunk text.
    pub text: String,
    /// Heading path for rendering.
    #[serde(default)]
    pub heading_path: Vec<String>,
    /// Chunk ordinal.
    pub ordinal: u32,
    /// Approximate token count.
    pub token_count: usize,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Linked connection plus latest sync-run status for service projections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeConnectionProjection {
    /// Linked connection.
    pub connection: KnowledgeConnection,
    /// Most recent sync-run status, when one exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sync_status: Option<SyncRunStatus>,
}

/// Source object plus parser and graph counters for service projections.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectProjection {
    /// Source object.
    pub object: KnowledgeObject,
    /// Latest parser that produced content for the object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser: Option<String>,
    /// Current parser status for the object.
    pub parser_status: String,
    /// Current chunk count.
    pub chunk_count: u64,
    /// Current graph node count.
    pub graph_node_count: u64,
}

/// Object inspection projection assembled from object, version, chunks, and steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeObjectInspection {
    /// Source object.
    pub object: KnowledgeObject,
    /// Latest parsed document version, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<DocumentVersion>,
    /// Current chunks for the latest version.
    #[serde(default)]
    pub chunks: Vec<KnowledgeChunk>,
    /// Ordered object ingestion timeline.
    #[serde(default)]
    pub steps: Vec<KnowledgeIngestionStep>,
}

/// Stored provider webhook event used for idempotent delivery handling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeProviderEventRecord {
    /// Tenant-owned provider-event row identifier.
    pub provider_event_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Optional linked connection associated with the event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_uid: Option<Uuid>,
    /// Linked-account provider that emitted the event.
    pub provider: String,
    /// Provider event identifier used for idempotency.
    pub provider_event_id: String,
    /// Provider event type.
    pub event_type: String,
    /// Local event status.
    pub status: String,
    /// Redacted provider payload.
    #[serde(default)]
    pub payload: Value,
    /// Whether this delivery duplicated an already recorded event.
    pub duplicate: bool,
}

/// Request to create a provider link token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateLinkTokenRequest {
    /// Tenant that will own the connection.
    pub tenant_id: TenantId,
    /// Connector identifier.
    pub connector: String,
    /// Optional caller-facing account reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_account_id: Option<String>,
    /// Optional redirect URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_url: Option<String>,
}

/// Provider link token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkToken {
    /// Provider identifier.
    pub provider: String,
    /// Short-lived token.
    pub token: String,
    /// Optional hosted link URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
    /// Token expiration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Request to exchange a provider public token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExchangePublicTokenRequest {
    /// Tenant that owns the link.
    pub tenant_id: TenantId,
    /// Token returned by provider-hosted UI.
    pub public_token: String,
}

/// Linked account returned by a provider after token exchange.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkedAccount {
    /// Provider identifier.
    pub provider: String,
    /// Provider connector.
    pub connector: String,
    /// Provider account identifier.
    pub provider_account_id: String,
    /// Credential vault reference or provider token reference.
    pub credential_ref: String,
    /// Safe account metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Request to trigger a provider sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggerSyncRequest {
    /// Connection to sync.
    pub connection: KnowledgeConnection,
    /// Provider model or collection to sync.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Provider sync trigger acknowledgement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriggeredSync {
    /// Provider identifier.
    pub provider: String,
    /// Provider sync identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_sync_id: Option<String>,
    /// Provider status.
    pub status: String,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Request to list changed provider records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListChangedRecordsRequest {
    /// Connection to inspect.
    pub connection: KnowledgeConnection,
    /// Provider cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Maximum records to return.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Page of normalized provider records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordPage {
    /// Normalized records.
    #[serde(default)]
    pub records: Vec<ProviderRecord>,
    /// Cursor for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// Provider record before normalization into a knowledge object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRecord {
    /// Provider source identifier.
    pub source_id: String,
    /// Source object type.
    pub object_type: String,
    /// Optional title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Optional URI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// Optional change token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_token: Option<String>,
    /// Whether the provider reports this record as deleted.
    #[serde(default)]
    pub deleted: bool,
    /// Source update timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_updated_at: Option<DateTime<Utc>>,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
    /// Raw record payload kept in memory for normalization only.
    #[serde(default)]
    pub payload: Value,
}

/// Verified provider webhook event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookEvent {
    /// Provider identifier.
    pub provider: String,
    /// Event identifier.
    pub event_id: String,
    /// Event type.
    pub event_type: String,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Ingestion step status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestionStepStatus {
    /// Step has started.
    Started,
    /// Step completed.
    Completed,
    /// Step failed.
    Failed,
    /// Step was skipped.
    Skipped,
}

impl IngestionStepStatus {
    /// Returns the stable database status identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Ingestion step row safe for storage and traces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeIngestionStep {
    /// Step identifier.
    pub step_uid: Uuid,
    /// Sync run.
    pub sync_run_uid: Uuid,
    /// Optional object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_uid: Option<Uuid>,
    /// Step name.
    pub step: String,
    /// Step status.
    pub status: IngestionStepStatus,
    /// Start timestamp.
    pub started_at: DateTime<Utc>,
    /// End timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// Duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Safe counters.
    #[serde(default)]
    pub counters: Value,
    /// Safe summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Retry count.
    pub retry_count: u32,
    /// Typed error code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Derived contact group grounded in tenant knowledge evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactGroup {
    /// Contact-group identifier.
    pub group_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Stable group key.
    pub group_key: String,
    /// Display name.
    pub display_name: String,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Contact-group membership derived from source evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactGroupMembership {
    /// Owning group.
    pub group_uid: Uuid,
    /// Contact in the group.
    pub contact_id: ContactId,
    /// Evidence object or chunk identifiers.
    #[serde(default)]
    pub evidence: Vec<Uuid>,
    /// Safe metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Provider webhook verification input.
#[derive(Debug, Clone)]
pub struct WebhookVerification {
    /// Webhook headers.
    pub headers: HeaderMap,
    /// Webhook body bytes.
    pub body: bytes::Bytes,
}
