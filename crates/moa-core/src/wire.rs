//! Shared wire DTOs for the cloud orchestrator HTTP surface.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    Attachment, Event, EventRange, IdempotencyClass, ResolutionScore, SegmentCompletion, SegmentId,
    SessionFilter, SessionId, SessionMeta, TaskSegment, ToolDefinition, WorkspaceId,
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

/// Request payload for `SessionStore/update_segment_resolution`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateSegmentResolutionRequest {
    /// Segment identifier to update.
    pub segment_id: SegmentId,
    /// Resolution label.
    pub resolution: String,
    /// Resolution confidence.
    pub confidence: f64,
}

/// Request payload for `SessionStore/update_segment_resolution_score`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateSegmentResolutionScoreRequest {
    /// Segment identifier to update.
    pub segment_id: SegmentId,
    /// Full resolution score and signal breakdown.
    pub score: ResolutionScore,
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
