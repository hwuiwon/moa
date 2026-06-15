//! Shared wire DTOs for the cloud orchestrator HTTP surface.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    Attachment, CheckpointHandle, CheckpointInfo, Event, EventRange, IdempotencyClass, MemoryScope,
    SegmentAssessment, SegmentCompletion, SegmentId, SessionFilter, SessionId, SessionMeta,
    SessionStatus, TaskSegment, ToolDefinition, UserId, WorkspaceId,
};

/// Input accepted by one `TurnExecution` workflow run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunTurnRequest {
    /// Session that owns the turn.
    pub session_id: String,
    /// Stable turn identifier and workflow key.
    pub turn_id: String,
    /// User message that initiated the turn.
    pub user_message: String,
    /// User message attachments that initiated the turn.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
}

/// Input accepted by one `SubAgentTurnExecution` workflow run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunSubAgentTurnRequest {
    /// Sub-agent object key whose queued messages should be processed.
    pub sub_agent_id: String,
    /// Stable turn identifier and workflow key.
    pub turn_id: String,
}

/// Durable lifecycle phase for one turn workflow.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub enum TurnPhase {
    /// Workflow has not started visible work.
    #[default]
    Pending,
    /// Workflow is compiling context and request state.
    Compiling,
    /// Workflow is producing model output.
    Streaming,
    /// Workflow is executing tools.
    Tooling,
    /// Workflow is persisting turn output.
    Persisting,
    /// Workflow completed successfully.
    Completed,
    /// Workflow was cancelled.
    Cancelled,
    /// Workflow failed.
    Failed,
}

/// Terminal outcome returned by one turn workflow.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TurnOutcome {
    /// Stable turn identifier.
    pub turn_id: String,
    /// Terminal outcome kind.
    pub kind: TurnOutcomeKind,
    /// Human-readable outcome message.
    pub message: String,
}

/// Terminal outcome category for a turn workflow.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum TurnOutcomeKind {
    /// The turn body completed.
    Completed,
    /// The cancel awakeable resolved before the body completed.
    Cancelled,
    /// The turn body failed.
    Failed,
}

/// Read-only progress projection for one turn workflow.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TurnProgress {
    /// Stable turn identifier.
    pub turn_id: String,
    /// Current durable phase.
    pub phase: TurnPhase,
    /// Whether a cancel signal has been recorded.
    pub cancel_requested: bool,
    /// Optional cancel reason recorded by `request_cancel`.
    pub cancel_reason: Option<String>,
}

/// Request for starting a turn through the durable `TurnExecution` workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartTurnRequest {
    /// User message text that initiates the turn.
    pub user_message: String,
    /// Attachments included with the user message.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
}

/// Response returned by `Session/start_turn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartTurnResponse {
    /// Turn ID when a workflow was started immediately.
    pub turn_id: Option<String>,
    /// Whether the request was queued behind an already-active turn.
    pub queued: bool,
}

/// Request for queueing a message behind the active `TurnExecution` workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueMessageRequest {
    /// User message text to enqueue or start immediately.
    pub user_message: String,
    /// Attachments included with the user message.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
}

/// Response returned by `Session/queue_message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueMessageResponse {
    /// Whether the message was queued behind an active turn.
    pub queued: bool,
    /// Turn ID when the message started a workflow immediately.
    pub started_turn_id: Option<String>,
}

/// Response returned by `Session/request_cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResponse {
    /// Whether a cancel signal was forwarded to an active turn.
    pub cancelled: bool,
    /// Human-readable cancel forwarding result.
    pub reason: String,
}

/// Message queued behind an active turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingMessage {
    /// Durable time the message was accepted by the Session VO.
    pub queued_at: DateTime<Utc>,
    /// User message text to run later.
    pub user_message: String,
    /// Attachments included with the queued message.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
}

/// Read-only projection of the additive `TurnExecution` session state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// Session object key.
    pub session_id: String,
    /// Currently active `TurnExecution` workflow ID, if any.
    pub active_turn_id: Option<String>,
    /// Number of messages waiting behind the active turn.
    pub pending_message_count: u64,
    /// Last outcome delivered by `TurnExecution`.
    pub last_outcome: Option<TurnOutcome>,
}

/// Request payload for `SessionStore/append_event`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendEventRequest {
    /// Session receiving the event.
    pub session_id: SessionId,
    /// Event payload to append to the durable log.
    pub event: Event,
}

/// Request payload for `SessionStore/get_events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GetEventsRequest {
    /// Session whose event log should be read.
    pub session_id: SessionId,
    /// Range and filter options for the event query.
    pub range: EventRange,
}

/// Request payload for `SessionStore/update_status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatusRequest {
    /// Session whose lifecycle state should be updated.
    pub session_id: SessionId,
    /// New session lifecycle state.
    pub status: crate::SessionStatus,
}

/// Request payload for `SessionStore/search_events`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchEventsRequest {
    /// Full-text search query.
    pub query: String,
    /// Additional event-search scoping and limits.
    pub filter: crate::EventFilter,
}

/// Request payload for `SessionStore/init_session_vo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitSessionVoRequest {
    /// Session object key that should be initialized.
    pub session_id: SessionId,
    /// Session metadata mirrored into Restate object state.
    pub meta: SessionMeta,
}

/// Request payload for `SessionStore/create_segment`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSegmentRequest {
    /// Segment metadata to persist.
    pub segment: TaskSegment,
}

/// Request payload for `SessionStore/complete_segment`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteSegmentRequest {
    /// Segment identifier to complete.
    pub segment_id: SegmentId,
    /// Completion counters and end timestamp.
    pub update: SegmentCompletion,
}

/// Request payload for `SessionStore/update_segment_assessment`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateSegmentAssessmentRequest {
    /// Segment identifier to update.
    pub segment_id: SegmentId,
    /// Full assessment outcome and evidence.
    pub assessment: SegmentAssessment,
}

/// Request payload for `SessionStore/get_segment_baseline`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GetSegmentBaselineRequest {
    /// Tenant/workspace identifier.
    pub tenant_id: String,
}

/// Request payload for `SessionStore/list_skill_resolution_rates`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSkillResolutionRatesRequest {
    /// Tenant/workspace identifier.
    pub tenant_id: String,
}

/// Request payload for recording active-segment tool usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSegmentToolUseRequest {
    /// Session whose active segment receives the tool usage.
    pub session_id: SessionId,
    /// Tool name to record.
    pub tool_name: String,
}

/// Request payload for recording active-segment skill usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSegmentSkillActivationRequest {
    /// Session whose active segment receives the skill activation.
    pub session_id: SessionId,
    /// Skill name to record.
    pub skill_name: String,
}

/// Request payload for recording active-segment turn usage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSegmentTurnUsageRequest {
    /// Session whose active segment receives the turn usage.
    pub session_id: SessionId,
    /// Token cost to add for the turn.
    pub token_cost: u64,
}

/// Request payload for `SessionStore/list_sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSessionsRequest {
    /// Session summary filter.
    pub filter: SessionFilter,
}

/// Request payload for `SessionStore/workspace_cost_since`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCostSinceRequest {
    /// Workspace whose spend should be aggregated.
    pub workspace_id: WorkspaceId,
    /// Inclusive lower-bound timestamp for the spend query.
    pub since: DateTime<Utc>,
}

/// Request payload for reading analytics for one session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsRequest {
    /// Session whose analytics summary should be read.
    pub session_id: SessionId,
}

/// Response payload containing one session analytics summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatsResponse {
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace that owns the session.
    pub workspace_id: WorkspaceId,
    /// User that owns the session.
    pub user_id: UserId,
    /// Current persisted session status.
    pub status: SessionStatus,
    /// Number of completed assistant turns.
    pub turn_count: u64,
    /// Total event count for the session.
    pub event_count: u64,
    /// Total input tokens across cached and uncached paths.
    pub total_input_tokens: u64,
    /// Total output tokens.
    pub total_output_tokens: u64,
    /// Total session cost in cents.
    pub total_cost_cents: u64,
    /// Total main-loop cost in cents.
    pub main_cost_cents: u64,
    /// Total auxiliary-tier cost in cents.
    pub auxiliary_cost_cents: u64,
    /// Fraction of input tokens served from cache.
    pub cache_hit_rate: f64,
    /// Session wall-clock duration in seconds.
    pub duration_seconds: f64,
    /// Number of tool calls recorded for the session.
    pub tool_call_count: u64,
    /// Number of error events recorded for the session.
    pub error_count: u64,
}

/// Request payload for reading workspace analytics over a recent window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceStatsRequest {
    /// Workspace whose rollup should be read.
    pub workspace_id: WorkspaceId,
    /// Number of whole days included in the rollup window.
    pub days: u32,
}

/// Response payload containing workspace analytics over a recent window.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceStatsResponse {
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// Number of whole days included in the rollup window.
    pub days: u32,
    /// Session count across the window.
    pub session_count: u64,
    /// Turn count across the window.
    pub turn_count: u64,
    /// Total input tokens across the window.
    pub total_input_tokens: u64,
    /// Cache-read input tokens across the window.
    pub total_cache_read_tokens: u64,
    /// Total output tokens across the window.
    pub total_output_tokens: u64,
    /// Total cost in cents across the window.
    pub total_cost_cents: u64,
    /// Weighted cache-hit rate for the window.
    pub cache_hit_rate: f64,
}

/// Request payload for reading per-tool analytics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStatsRequest {
    /// Optional workspace filter for the per-tool rollup.
    pub workspace_id: Option<WorkspaceId>,
}

/// Response payload containing per-tool analytics rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStatsResponse {
    /// Workspace filter used for this response, if one was requested.
    pub workspace_id: Option<WorkspaceId>,
    /// Per-tool analytics rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<ToolStatsRow>,
}

/// One per-tool analytics row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolStatsRow {
    /// Stable tool name.
    pub tool_name: String,
    /// Number of completed calls for the tool.
    pub call_count: u64,
    /// Fraction of calls that succeeded.
    pub success_rate: f64,
    /// Mean duration in milliseconds.
    pub avg_duration_ms: f64,
    /// Median duration in milliseconds.
    pub p50_ms: f64,
    /// P95 duration in milliseconds.
    pub p95_ms: f64,
}

/// Request payload for reading workspace cache analytics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheStatsRequest {
    /// Workspace whose cache rollup should be read.
    pub workspace_id: WorkspaceId,
    /// Number of whole days included in the cache window.
    pub days: u32,
}

/// Response payload containing workspace cache analytics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheStatsResponse {
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// Number of whole days included in the cache window.
    pub days: u32,
    /// Weighted cache-hit rate for the window.
    pub cache_hit_rate: f64,
    /// Cache-read input tokens across the window.
    pub total_cache_read_tokens: u64,
    /// Total input tokens across the window.
    pub total_input_tokens: u64,
    /// Total output tokens across the window.
    pub total_output_tokens: u64,
    /// Total cost in cents across the window.
    pub total_cost_cents: u64,
    /// Estimated cache savings in cents when pricing history can support it.
    pub estimated_savings_cents: Option<u64>,
    /// Daily cache trend rows ordered by day.
    #[serde(default)]
    pub daily: Vec<CacheDailyMetricRow>,
}

/// One daily workspace cache trend point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheDailyMetricRow {
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// UTC day bucket.
    pub day: DateTime<Utc>,
    /// Session count on the day.
    pub session_count: u64,
    /// Turn count on the day.
    pub turn_count: u64,
    /// Total input tokens on the day.
    pub total_input_tokens: u64,
    /// Total cache-read tokens on the day.
    pub total_cache_read_tokens: u64,
    /// Total output tokens on the day.
    pub total_output_tokens: u64,
    /// Total cost in cents on the day.
    pub total_cost_cents: u64,
    /// Average cache-hit rate on the day.
    pub avg_cache_hit_rate: f64,
}

/// Request payload for graph-memory search.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchRequest {
    /// Workspace whose memory should be searched.
    pub workspace_id: WorkspaceId,
    /// Optional user scope for user-personal memory reads.
    pub user_id: Option<UserId>,
    /// Search query text.
    pub query: String,
    /// Maximum number of hits to return.
    pub limit: u32,
    /// Optional graph labels to include.
    #[serde(default)]
    pub label_filter: Vec<String>,
    /// Optional maximum PII class accepted by the caller.
    pub max_pii_class: Option<String>,
    /// Whether the retrieval service should apply reranking.
    #[serde(default)]
    pub use_reranker: bool,
}

/// Response payload containing graph-memory search hits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemorySearchResponse {
    /// Query text that produced these hits.
    pub query: String,
    /// Memory hits ordered by rank.
    #[serde(default)]
    pub hits: Vec<MemoryHit>,
}

/// One graph-memory hit returned to API renderers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryHit {
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Graph node label.
    pub label: String,
    /// Human-readable graph node name.
    pub name: String,
    /// Retrieval score assigned to the hit.
    pub score: f64,
    /// Short text snippet for table display.
    pub snippet: String,
    /// Retrieval legs that contributed to this hit.
    #[serde(default)]
    pub legs: Vec<String>,
    /// Optional server-side node summary or properties used for richer renderers.
    #[serde(default)]
    pub properties: Option<Value>,
}

/// Request payload for showing one graph-memory node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryShowRequest {
    /// Workspace used to authorize and scope the node lookup.
    pub workspace_id: WorkspaceId,
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Neighbor traversal depth requested by the caller.
    #[serde(default)]
    pub neighbor_depth: u32,
}

/// Response payload containing one graph-memory node and immediate context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryShowResponse {
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Graph node label.
    pub label: String,
    /// Human-readable graph node name.
    pub name: String,
    /// Persisted memory scope label.
    pub scope: String,
    /// Timestamp when this node version became valid.
    pub valid_from: DateTime<Utc>,
    /// Timestamp when this node version was superseded, if any.
    pub valid_to: Option<DateTime<Utc>>,
    /// Node properties prepared for display.
    pub properties: Value,
    /// Neighboring nodes returned with the node.
    #[serde(default)]
    pub neighbors: Vec<MemoryNeighbor>,
}

/// One neighboring graph-memory node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryNeighbor {
    /// Stable graph node UID.
    pub uid: Uuid,
    /// Graph node label.
    pub label: String,
    /// Human-readable graph node name.
    pub name: String,
    /// Optional relationship label connecting the neighbor.
    pub relationship: Option<String>,
}

/// One document supplied to graph-memory ingestion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIngestDocument {
    /// Human-readable source name for the document.
    pub source_name: String,
    /// Source document content to ingest.
    pub content: String,
    /// Optional logical source path or URI for audit trails.
    pub source_uri: Option<String>,
    /// Additional caller-supplied ingestion metadata.
    #[serde(default)]
    pub metadata: Value,
}

/// Request payload for graph-memory ingestion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIngestRequest {
    /// Workspace receiving the ingested documents.
    pub workspace_id: WorkspaceId,
    /// User associated with the ingestion request, if any.
    pub user_id: Option<UserId>,
    /// Documents to ingest.
    #[serde(default)]
    pub documents: Vec<MemoryIngestDocument>,
}

/// Response payload containing graph-memory ingestion results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIngestResponse {
    /// Workspace that received the ingested documents.
    pub workspace_id: WorkspaceId,
    /// Per-document ingestion results.
    #[serde(default)]
    pub results: Vec<MemoryIngestResult>,
}

/// Per-document graph-memory ingestion result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryIngestResult {
    /// Human-readable source name for the document.
    pub source_name: String,
    /// Number of graph nodes inserted.
    pub inserted: u64,
    /// Number of graph nodes superseded.
    pub superseded: u64,
    /// Number of graph nodes skipped.
    pub skipped: u64,
    /// Number of graph nodes that failed ingestion.
    pub failed: u64,
    /// Number of graph edges inserted.
    pub edges: u64,
    /// Number of contradictions detected.
    pub contradictions: u64,
    /// Whether this document produced dead-letter work.
    pub dead_lettered: bool,
}

/// Request payload for detailed memory retrieval debugging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRetrieveDebugRequest {
    /// Workspace whose memory should be searched.
    pub workspace_id: WorkspaceId,
    /// Optional user scope for user-personal memory reads.
    pub user_id: Option<UserId>,
    /// Search query text.
    pub query: String,
    /// Maximum number of hits to return.
    pub limit: u32,
    /// Whether the server should skip durable lineage flushing.
    #[serde(default)]
    pub no_flush_wait: bool,
}

/// Response payload for detailed memory retrieval debugging.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRetrieveDebugResponse {
    /// Query text that produced these hits.
    pub query: String,
    /// Whether lineage capture was enabled during retrieval.
    pub lineage_enabled: bool,
    /// Whether durable lineage flushing was skipped.
    pub no_flush_wait: bool,
    /// Turn identifier for the debug lineage record, when one was emitted.
    pub lineage_turn: Option<Uuid>,
    /// Seed node UIDs used by the hybrid retrieval request.
    #[serde(default)]
    pub seed_uids: Vec<Uuid>,
    /// Memory hits ordered by rank.
    #[serde(default)]
    pub hits: Vec<MemoryHit>,
    /// Additional backend-specific retrieval diagnostics.
    #[serde(default)]
    pub diagnostics: Value,
}

/// Request payload for explaining lineage for one session or turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageExplainRequest {
    /// Workspace containing the session or turn to explain.
    pub workspace_id: WorkspaceId,
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
    /// Workspace associated with the lineage record, when available.
    pub workspace_id: Option<WorkspaceId>,
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

/// Request payload for querying lineage records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageQueryRequest {
    /// Read-only SQL query using the logical `lineage` source.
    pub sql: String,
    /// Whether to query the cold object tier instead of the hot store.
    #[serde(default)]
    pub cold: bool,
    /// Postgres interval for hot-tier time filtering.
    pub since: String,
    /// Workspace filter for authorization and query scoping.
    pub workspace_id: WorkspaceId,
}

/// Response payload containing dynamic lineage query rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageQueryResponse {
    /// Query rows as a JSON array or backend-specific report value.
    pub rows: Value,
}

/// Request payload for exporting a lineage DSAR bundle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageExportRequest {
    /// Workspace whose lineage records should be exported.
    pub workspace_id: WorkspaceId,
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
    /// Workspace whose lineage window should be verified.
    pub workspace_id: WorkspaceId,
    /// `hot`, an audit root UUID, or an audit root object URI.
    pub window: String,
    /// Postgres interval for hot-window verification.
    pub since: String,
}

/// Response payload describing lineage verification results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageVerifyResponse {
    /// Workspace whose lineage window was verified.
    pub workspace_id: WorkspaceId,
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
    /// Workspace containing the subject pseudonym.
    pub workspace_id: WorkspaceId,
    /// Hex-encoded subject pseudonym.
    pub subject: String,
}

/// Response payload for a lineage erase request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineageEraseResponse {
    /// Workspace containing the erased subject pseudonym.
    pub workspace_id: WorkspaceId,
    /// Number of matching subjects scheduled for erasure.
    pub subjects: u64,
    /// Erasure status label.
    pub status: String,
}

/// Request payload for exporting privacy data for one subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyExportRequest {
    /// Optional workspace filter for the export.
    pub workspace_id: Option<WorkspaceId>,
    /// Subject user identifier for the data export.
    pub subject_user_id: UserId,
    /// Administrative reason recorded in the audit trail.
    pub reason: String,
    /// Signed platform-admin approval token.
    pub approval_token: String,
    /// Optional armored PGP recipient key for encrypting the archive.
    pub pgp_recipient: Option<String>,
}

/// Response payload describing a privacy export archive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyExportResponse {
    /// Subject user identifier exported.
    pub subject_user_id: UserId,
    /// Workspace filter applied to the export.
    pub workspace_id: Option<WorkspaceId>,
    /// URI where the archive can be fetched.
    pub archive_uri: String,
    /// Number of files included in the archive.
    pub file_count: u64,
    /// Per-section exported row counts.
    #[serde(default)]
    pub counts: BTreeMap<String, u64>,
    /// Optional manifest or signature details.
    #[serde(default)]
    pub manifest: Value,
    /// Optional base64-encoded archive bytes for API file output.
    pub archive_base64: Option<String>,
}

/// Request payload for erasing privacy data for one subject.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyEraseRequest {
    /// Workspace containing the subject data to erase.
    pub workspace_id: WorkspaceId,
    /// Subject user identifier for the erasure request.
    pub subject_user_id: UserId,
    /// Administrative reason recorded in the audit trail.
    pub reason: String,
    /// Whether to list candidates without writing graph or changelog rows.
    #[serde(default)]
    pub dry_run: bool,
    /// Signed platform-admin approval token.
    pub approval_token: String,
}

/// Response payload for a privacy erase request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PrivacyEraseResponse {
    /// Workspace containing the subject data.
    pub workspace_id: WorkspaceId,
    /// Subject user identifier erased.
    pub subject_user_id: UserId,
    /// Number of candidate memory nodes found.
    pub candidate_count: u64,
    /// Number of memory nodes erased.
    pub erased_count: u64,
    /// Number of PII vault rows erased.
    pub pii_vault_erased: u64,
    /// Whether the request was a dry run.
    pub dry_run: bool,
    /// Sample erase candidates for dry-run output.
    #[serde(default)]
    pub sample: Vec<Value>,
}

/// Request payload for exporting workspace skills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillExportRequest {
    /// Workspace whose visible skills should be exported.
    pub workspace_id: WorkspaceId,
}

/// Response payload containing exported skill packages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillExportResponse {
    /// Workspace whose skills were exported.
    pub workspace_id: WorkspaceId,
    /// Exported skill packages.
    #[serde(default)]
    pub packages: Vec<SkillPackageDocument>,
}

/// Skill package supplied to skill import or returned by export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillPackageDocument {
    /// Optional stable skill name parsed from `SKILL.md`.
    pub name: Option<String>,
    /// Optional one-line skill description parsed from `SKILL.md`.
    pub description: Option<String>,
    /// Files contained in this skill package.
    #[serde(default)]
    pub files: Vec<SkillPackageDocumentFile>,
    /// Optional logical source path or URI.
    pub source_uri: Option<String>,
    /// Additional skill metadata parsed by the server.
    #[serde(default)]
    pub metadata: Value,
}

/// One file in a skill package import or export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillPackageDocumentFile {
    /// POSIX relative path inside the skill package.
    pub path: String,
    /// Base64-encoded file content.
    pub content_base64: String,
    /// Optional media type hint.
    pub content_type: Option<String>,
    /// Whether the file should be executable in a sandbox.
    #[serde(default)]
    pub executable: bool,
}

/// Request payload for importing skill packages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillImportRequest {
    /// Workspace used for authorization and workspace/user scoped imports.
    pub workspace_id: WorkspaceId,
    /// Scope where imported skills should be written.
    pub scope: MemoryScope,
    /// Skill packages to import.
    #[serde(default)]
    pub packages: Vec<SkillPackageDocument>,
}

/// Response payload for importing skill packages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillImportResponse {
    /// Scope where skills were imported.
    pub scope: MemoryScope,
    /// Number of skill packages imported.
    pub imported: u64,
}

/// Request payload for listing skills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillListRequest {
    /// Workspace whose visible skills should be listed.
    pub workspace_id: WorkspaceId,
}

/// Response payload containing listed skills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillListResponse {
    /// Listed skills ordered for API display.
    #[serde(default)]
    pub skills: Vec<SkillSummary>,
}

/// Summary of one visible skill version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillSummary {
    /// Stable row identifier for this skill version.
    pub skill_uid: Uuid,
    /// Scope where this skill is visible.
    pub scope: MemoryScope,
    /// Integer row-level skill version.
    pub version: i32,
    /// Skill name.
    pub name: String,
    /// Skill description.
    pub description: String,
    /// Tags associated with the skill.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Hex-encoded SHA-256 digest of the full package tree.
    pub package_hash: String,
    /// Hex-encoded SHA-256 digest of the required `SKILL.md`.
    pub skill_md_hash: String,
    /// Number of files in the package.
    pub file_count: i32,
    /// Total package size in bytes.
    pub total_size_bytes: i64,
    /// Timestamp when this skill version was created.
    pub created_at: DateTime<Utc>,
    /// Timestamp when this skill version was last updated.
    pub updated_at: DateTime<Utc>,
}

/// Request payload for bootstrapping global skills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillBootstrapGlobalRequest {
    /// Authored global skill packages to import.
    #[serde(default)]
    pub packages: Vec<SkillPackageDocument>,
}

/// Response payload for bootstrapping global skills.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillBootstrapGlobalResponse {
    /// Number of global skill documents imported.
    pub imported: u64,
}

/// Request payload for planning an eval suite run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalPlanRequest {
    /// Workspace scope used for authorization and eval execution.
    pub workspace_id: WorkspaceId,
    /// Raw suite document supplied by the API caller.
    pub suite_document: String,
    /// Logical suite source path or URI.
    pub suite_source: Option<String>,
    /// Raw agent configuration documents supplied by the API caller.
    #[serde(default)]
    pub config_documents: Vec<String>,
    /// Logical config source paths or URIs.
    #[serde(default)]
    pub config_sources: Vec<String>,
}

/// Response payload describing an eval execution plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalPlanResponse {
    /// Suite name that would be executed.
    pub suite_name: String,
    /// Agent config names included in the run.
    #[serde(default)]
    pub configs: Vec<String>,
    /// Test case names included in the run.
    #[serde(default)]
    pub cases: Vec<String>,
    /// Total `(config, case)` executions.
    pub total_runs: u64,
    /// Coarse minimum estimated dollar cost.
    pub estimated_min_cost_dollars: f64,
    /// Coarse maximum estimated dollar cost.
    pub estimated_max_cost_dollars: f64,
}

/// One eval suite document supplied for hosted listing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSuiteListDocument {
    /// Logical suite source path or URI.
    pub source: Option<String>,
    /// Raw suite TOML document.
    pub body: String,
}

/// Request payload for listing eval suite summaries from supplied documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSuiteListRequest {
    /// Workspace scope used for authorization.
    pub workspace_id: WorkspaceId,
    /// Suite documents to parse and summarize.
    #[serde(default)]
    pub documents: Vec<EvalSuiteListDocument>,
}

/// Hosted eval suite summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSuiteSummary {
    /// Logical suite source path or URI.
    pub source: Option<String>,
    /// Stable suite name.
    pub name: String,
    /// Number of cases in the suite.
    pub cases: u64,
    /// Optional suite description.
    pub description: Option<String>,
    /// Suite tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Response payload for listing eval suite summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalSuiteListResponse {
    /// Workspace scope used for authorization.
    pub workspace_id: WorkspaceId,
    /// Parsed suite summaries ordered like the request documents.
    #[serde(default)]
    pub suites: Vec<EvalSuiteSummary>,
}

/// Request payload for running an eval suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunRequest {
    /// Workspace scope used for authorization and eval execution.
    pub workspace_id: WorkspaceId,
    /// Raw suite document supplied by the API caller.
    pub suite_document: String,
    /// Logical suite source path or URI.
    pub suite_source: Option<String>,
    /// Raw agent configuration documents supplied by the API caller.
    #[serde(default)]
    pub config_documents: Vec<String>,
    /// Logical config source paths or URIs.
    #[serde(default)]
    pub config_sources: Vec<String>,
    /// Report sink specs such as `terminal`, `json:<path>`, or `langfuse`.
    #[serde(default)]
    pub reports: Vec<String>,
    /// Maximum concurrent eval executions.
    pub parallel: u32,
    /// Whether CI exit-code semantics should be applied.
    #[serde(default)]
    pub ci: bool,
    /// Evaluator names to run.
    #[serde(default)]
    pub evaluators: Vec<String>,
    /// Maximum allowed per-run cost in dollars.
    pub max_cost_dollars: Option<f64>,
    /// Maximum allowed per-run latency in milliseconds.
    pub max_latency_ms: Option<u64>,
    /// Maximum allowed tokens per run.
    pub max_tokens: Option<u64>,
    /// Maximum allowed tool calls per run.
    pub max_tool_calls: Option<u64>,
    /// Maximum allowed turns per run.
    pub max_turns: Option<u64>,
    /// Whether per-case response and score comments should be included.
    #[serde(default)]
    pub verbose: bool,
}

/// Response payload for an eval suite run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunResponse {
    /// Workspace scope used for authorization and eval execution.
    pub workspace_id: WorkspaceId,
    /// Server-assigned eval run identifier.
    pub run_id: Uuid,
    /// Current run lifecycle status.
    pub status: EvalRunStatus,
    /// Suite name that was executed.
    pub suite_name: String,
    /// Process exit code recommended for automation.
    pub exit_code: i32,
    /// Aggregate run summary.
    pub summary: Value,
    /// Per-case eval results.
    #[serde(default)]
    pub results: Vec<Value>,
    /// Terminal error when the hosted eval failed before producing case results.
    pub error: Option<String>,
}

/// Durable server-side lifecycle status for an eval run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvalRunStatus {
    /// The run has been accepted but has not started visible work.
    #[default]
    Pending,
    /// The run is executing on the hosted orchestrator.
    Running,
    /// The run completed and contains terminal results.
    Completed,
    /// The run failed before producing terminal results.
    Failed,
}

/// Request payload for polling an eval run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRunStatusRequest {
    /// Workspace scope used for authorization and run-result filtering.
    pub workspace_id: WorkspaceId,
    /// Server-assigned eval run identifier.
    pub run_id: Uuid,
}

/// Response payload for polling an eval run status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRunStatusResponse {
    /// Workspace scope that owns this run.
    pub workspace_id: WorkspaceId,
    /// Server-assigned eval run identifier.
    pub run_id: Uuid,
    /// Current run lifecycle status.
    pub status: EvalRunStatus,
    /// Suite name when known.
    pub suite_name: Option<String>,
    /// Process exit code recommended for automation once terminal.
    pub exit_code: Option<i32>,
    /// Aggregate run summary once terminal.
    pub summary: Option<Value>,
    /// Per-case eval results once terminal.
    #[serde(default)]
    pub results: Vec<Value>,
    /// Terminal error when the hosted eval failed before producing case results.
    pub error: Option<String>,
}

/// Request payload for registering an eval dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalDatasetRegisterRequest {
    /// Workspace scope used for authorization and dataset item ownership.
    pub workspace_id: WorkspaceId,
    /// Dataset name.
    pub name: String,
    /// Raw JSONL dataset content.
    pub jsonl: String,
    /// Logical source path or URI for the dataset.
    pub source_uri: Option<String>,
}

/// Response payload for registering an eval dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalDatasetRegisterResponse {
    /// Workspace scope that owns the registered dataset items.
    pub workspace_id: WorkspaceId,
    /// Registered dataset identifier.
    pub dataset_id: Uuid,
    /// Dataset name.
    pub name: String,
    /// Number of dataset items registered.
    pub items: u64,
}

/// Request payload for listing eval datasets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalDatasetListRequest {
    /// Workspace scope used for authorization and dataset filtering.
    pub workspace_id: WorkspaceId,
}

/// Workspace-scoped eval dataset summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalDatasetSummary {
    /// Workspace that has items in this dataset.
    pub workspace_id: WorkspaceId,
    /// Dataset identifier.
    pub dataset_id: Uuid,
    /// Dataset name.
    pub name: String,
    /// Number of items visible in this workspace.
    pub items: u64,
    /// Logical source path or URI for the dataset.
    pub source_uri: Option<String>,
}

/// Response payload for listing eval datasets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalDatasetListResponse {
    /// Workspace scope used to filter dataset item counts.
    pub workspace_id: WorkspaceId,
    /// Dataset summaries ordered for API display.
    #[serde(default)]
    pub datasets: Vec<EvalDatasetSummary>,
}

/// Request payload for replaying an eval dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReplayRequest {
    /// Workspace scope used for authorization and dataset item filtering.
    pub workspace_id: WorkspaceId,
    /// Dataset identifier.
    pub dataset_id: Uuid,
    /// Optional replay run identifier.
    pub run_id: Option<Uuid>,
    /// Maximum dataset items to replay.
    pub limit: Option<u64>,
    /// Optional embedder label for the run.
    pub embedder: Option<String>,
    /// Optional model label for the run.
    pub model: Option<String>,
}

/// Response payload for replaying an eval dataset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReplayResponse {
    /// Workspace scope used for dataset item filtering.
    pub workspace_id: WorkspaceId,
    /// Replay run identifier.
    pub run_id: Uuid,
    /// Dataset identifier.
    pub dataset_id: Uuid,
    /// Number of dataset items processed.
    pub items: u64,
    /// Number of score rows emitted.
    pub scores: u64,
}

/// Request payload for reading eval score summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScoresRequest {
    /// Workspace scope used for authorization and score filtering.
    pub workspace_id: WorkspaceId,
    /// Replay run identifier.
    pub run_id: Uuid,
}

/// Workspace-scoped eval score summary row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScoreSummaryRow {
    /// Score name.
    pub name: String,
    /// Score value type.
    pub value_type: String,
    /// Number of rows summarized.
    pub n: u64,
    /// Numeric mean or boolean true-rate.
    pub mean_or_rate: f64,
}

/// Response payload containing eval score summaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalScoresResponse {
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
    /// Replay run identifier.
    pub run_id: Uuid,
    /// Score summary rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<EvalScoreSummaryRow>,
}

/// Request payload for comparing two eval replay runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCompareRequest {
    /// Workspace scope used for authorization and score filtering.
    pub workspace_id: WorkspaceId,
    /// Baseline replay run identifier.
    pub base_run: Uuid,
    /// New replay run identifier.
    pub new_run: Uuid,
}

/// Workspace-scoped eval run comparison row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCompareRow {
    /// Score name.
    pub name: String,
    /// Baseline numeric mean.
    pub base_mean: Option<f64>,
    /// New numeric mean.
    pub new_mean: Option<f64>,
    /// New mean minus baseline mean, treating missing means as zero.
    pub delta: f64,
}

/// Response payload containing eval run comparison rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalCompareResponse {
    /// Workspace scope used for score filtering.
    pub workspace_id: WorkspaceId,
    /// Baseline replay run identifier.
    pub base_run: Uuid,
    /// New replay run identifier.
    pub new_run: Uuid,
    /// Comparison rows ordered for API display.
    #[serde(default)]
    pub rows: Vec<EvalCompareRow>,
}

/// Request payload for promoting a workspace vector backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorPromoteRequest {
    /// Workspace to promote.
    pub workspace_id: WorkspaceId,
    /// Target vector backend.
    pub target_backend: String,
    /// Percentage of vectors to sample during validation.
    pub validate_percent: u32,
    /// Number of hours to dual-read both backends after cutover.
    pub dual_read_hours: u32,
}

/// Response payload describing a vector promotion or update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorPromotionResponse {
    /// Workspace whose vector backend was updated.
    pub workspace_id: WorkspaceId,
    /// Number of vectors copied to the target backend.
    pub copied_vectors: u64,
    /// Average top-K overlap observed during validation.
    pub validation_overlap: f64,
    /// Active vector backend after the operation.
    pub vector_backend: String,
    /// Active vector backend state after the operation.
    pub vector_backend_state: String,
    /// Dual-read window in hours, when relevant.
    pub dual_read_hours: Option<u32>,
}

/// Request payload for rolling back or finalizing a vector promotion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VectorPromotionUpdateRequest {
    /// Workspace whose promotion state should be updated.
    pub workspace_id: WorkspaceId,
    /// Promotion update action such as `rollback` or `finalize`.
    pub action: String,
}

/// Request payload for creating a checkpoint branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointCreateRequest {
    /// Human-readable checkpoint label.
    pub label: String,
    /// Optional session associated with the checkpoint.
    pub session_id: Option<SessionId>,
}

/// Response payload for creating a checkpoint branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointCreateResponse {
    /// Created checkpoint handle.
    pub handle: CheckpointHandle,
}

/// Response payload for listing checkpoint branches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointListResponse {
    /// Active checkpoint branches ordered for API display.
    #[serde(default)]
    pub checkpoints: Vec<CheckpointInfo>,
}

/// Request payload for rolling back to a checkpoint branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRollbackRequest {
    /// Neon checkpoint branch identifier.
    pub id: String,
}

/// Response payload for rolling back to a checkpoint branch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRollbackResponse {
    /// Checkpoint selected for rollback.
    pub handle: CheckpointHandle,
    /// Database URL selected after rollback.
    pub database_url: String,
}

/// Response payload for deleting expired checkpoint branches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointCleanupResponse {
    /// Number of expired checkpoints deleted.
    pub deleted_expired_checkpoints: u64,
}

/// Public metadata returned by `ToolExecutor/list_tools`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDescriptor {
    /// Stable tool name.
    pub name: String,
    /// Human-readable tool description.
    pub description: String,
    /// JSON schema for the tool input.
    pub schema: serde_json::Value,
    /// Declared retry/idempotency contract for the tool.
    pub idempotency_class: IdempotencyClass,
    /// Whether the tool requires approval by default.
    pub requires_approval: bool,
}

/// Builds the public descriptor for one registered tool definition.
pub fn tool_descriptor(definition: ToolDefinition) -> ToolDescriptor {
    let requires_approval = definition.requires_approval();
    ToolDescriptor {
        name: definition.name,
        description: definition.description,
        schema: definition.schema,
        idempotency_class: definition.idempotency_class,
        requires_approval,
    }
}
