//! Request payloads for the Restate session-store service.

use super::*;

/// Request payload for `SessionStore/append_event`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AppendEventRequest {
    /// Session receiving the event.
    pub session_id: SessionId,
    /// Event payload to append to the durable log.
    pub event: Event,
}

/// Request payload for `SessionStore/get_events`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct GetEventsRequest {
    /// Session whose event log should be read.
    pub session_id: SessionId,
    /// Range and filter options for the event query.
    pub range: EventRange,
}

/// Request payload for `SessionStore/update_status`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UpdateStatusRequest {
    /// Session whose lifecycle state should be updated.
    pub session_id: SessionId,
    /// New session lifecycle state.
    pub status: SessionStatus,
}

/// Request payload for `SessionStore/search_events`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SearchEventsRequest {
    /// Full-text search query.
    pub query: String,
    /// Additional event-search scoping and limits.
    pub filter: EventFilter,
}

/// Request payload for `SessionStore/init_session_vo`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InitSessionVoRequest {
    /// Session object key that should be initialized.
    pub session_id: SessionId,
    /// Session metadata mirrored into Restate object state.
    pub meta: SessionMeta,
}

/// Request payload for `SessionStore/create_segment`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CreateSegmentRequest {
    /// Segment metadata to persist.
    pub segment: TaskSegment,
}

/// Request payload for `SessionStore/complete_segment`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompleteSegmentRequest {
    /// Segment identifier to complete.
    pub segment_id: SegmentId,
    /// Completion counters and end timestamp.
    pub update: SegmentCompletion,
}

/// Request payload for `SessionStore/update_segment_resolution`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UpdateSegmentResolutionRequest {
    /// Segment identifier to update.
    pub segment_id: SegmentId,
    /// Resolution label.
    pub resolution: String,
    /// Resolution confidence.
    pub confidence: f64,
}

/// Request payload for `SessionStore/update_segment_resolution_score`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UpdateSegmentResolutionScoreRequest {
    /// Segment identifier to update.
    pub segment_id: SegmentId,
    /// Full resolution score and signal breakdown.
    pub score: ResolutionScore,
}

/// Request payload for `SessionStore/get_segment_baseline`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct GetSegmentBaselineRequest {
    /// Tenant/workspace identifier.
    pub tenant_id: String,
    /// Optional intent label.
    pub intent_label: Option<String>,
}

/// Request payload for `SessionStore/list_skill_resolution_rates`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ListSkillResolutionRatesRequest {
    /// Tenant/workspace identifier.
    pub tenant_id: String,
    /// Optional intent label.
    pub intent_label: Option<String>,
}

/// Request payload for recording active-segment tool usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordSegmentToolUseRequest {
    /// Session whose active segment receives the tool usage.
    pub session_id: SessionId,
    /// Tool name to record.
    pub tool_name: String,
}

/// Request payload for recording active-segment skill usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordSegmentSkillActivationRequest {
    /// Session whose active segment receives the skill activation.
    pub session_id: SessionId,
    /// Skill name to record.
    pub skill_name: String,
}

/// Request payload for recording active-segment turn usage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordSegmentTurnUsageRequest {
    /// Session whose active segment receives the turn usage.
    pub session_id: SessionId,
    /// Token cost to add for the turn.
    pub token_cost: u64,
}
