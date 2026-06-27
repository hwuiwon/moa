//! Turn workflow and session progress wire DTOs.

use crate::traits::Identity;
use crate::*;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

const DEFAULT_SESSION_PROGRESS_EVENT_LIMIT: usize = 100;
const MAX_SESSION_PROGRESS_EVENT_LIMIT: usize = 500;

/// Input accepted by one `TurnExecution` workflow run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunTurnRequest {
    /// Session that owns the turn.
    pub session_id: String,
    /// Stable turn identifier and workflow key.
    pub turn_id: String,
    /// Trusted identity admitted by the Session VO for this turn.
    pub identity: Identity,
    /// Agent-facing contact admitted by the Session VO for this turn.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// User message that initiated the turn.
    pub user_message: String,
    /// User message attachments that initiated the turn.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional turn-iteration cap for this request.
    #[serde(default)]
    pub max_turns: Option<u32>,
}

/// Input accepted by one `SubAgentTurnExecution` workflow run.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct RunSubAgentTurnRequest {
    /// Sub-agent object key whose queued messages should be processed.
    pub sub_agent_id: String,
    /// Stable turn identifier and workflow key.
    pub turn_id: String,
    /// Optional turn-iteration cap for this child turn workflow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
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

/// Deterministic complexity class selected for one turn.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnComplexityClass {
    /// The request is underspecified enough that the agent should ask first.
    Clarification,
    /// The request should normally finish in one model pass without tools.
    Simple,
    /// The request is normal interactive work with bounded model and tool loops.
    #[default]
    Standard,
    /// The request is broad or workflow-shaped and may need the global hard cap.
    Complex,
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
    /// Deterministic complexity class selected for the turn.
    pub complexity_class: TurnComplexityClass,
    /// Current model-loop iteration, starting at `0` before the first call.
    pub iteration: u32,
    /// Effective model-loop cap for this turn, when bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Tool calls issued so far during this turn.
    pub tool_calls: u32,
    /// Effective tool-call cap for this turn, when bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Elapsed turn runtime in milliseconds.
    pub elapsed_ms: u64,
    /// Last durable progress summary emitted for this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_progress_summary: Option<String>,
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
    /// Agent-facing contact for this message, defaulting to the session contact.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// Optional turn-iteration cap for this request.
    #[serde(default)]
    pub max_turns: Option<u32>,
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
    /// Agent-facing contact for this message, defaulting to the session contact.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// Optional turn-iteration cap for this request.
    #[serde(default)]
    pub max_turns: Option<u32>,
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
    /// Trusted identity admitted by the Session VO for this queued turn.
    pub identity: Identity,
    /// Agent-facing contact admitted by the Session VO for this queued turn.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// User message text to run later.
    pub user_message: String,
    /// Attachments included with the queued message.
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    /// Optional per-turn model override.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional turn-iteration cap for this queued request.
    #[serde(default)]
    pub max_turns: Option<u32>,
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

/// Request payload for `Session/progress`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionProgressRequest {
    /// Event range to include alongside hot workflow progress.
    #[serde(default = "default_session_progress_event_range")]
    pub event_range: EventRange,
}

impl Default for SessionProgressRequest {
    fn default() -> Self {
        Self {
            event_range: default_session_progress_event_range(),
        }
    }
}

impl SessionProgressRequest {
    /// Returns a bounded event range for progress polling.
    #[must_use]
    pub fn normalized_event_range(&self) -> EventRange {
        let mut range = self.event_range.clone();
        range.limit = Some(
            range
                .limit
                .unwrap_or(DEFAULT_SESSION_PROGRESS_EVENT_LIMIT)
                .min(MAX_SESSION_PROGRESS_EVENT_LIMIT),
        );
        range
    }
}

/// Combined session progress projection for polling clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionProgress {
    /// Current Session VO lifecycle snapshot.
    pub snapshot: SessionSnapshot,
    /// Active turn workflow progress, when a turn is currently running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_turn_progress: Option<TurnProgress>,
    /// Durable event history matching the requested range.
    pub events: Vec<EventRecord>,
}

fn default_session_progress_event_range() -> EventRange {
    EventRange::recent(DEFAULT_SESSION_PROGRESS_EVENT_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_projection_round_trips_additive_fields() {
        // Pins: turn progress exposes responsiveness state without a separate taxonomy.
        let progress = TurnProgress {
            turn_id: "turn-123".to_string(),
            phase: TurnPhase::Tooling,
            complexity_class: TurnComplexityClass::Standard,
            iteration: 2,
            max_turns: Some(6),
            tool_calls: 3,
            max_tool_calls: Some(10),
            elapsed_ms: 12_500,
            last_progress_summary: Some("Running tool: bash".to_string()),
            cancel_requested: false,
            cancel_reason: None,
        };

        let json = serde_json::to_string(&progress).expect("serialize turn progress");
        assert!(json.contains("\"complexity_class\":\"Standard\""));
        assert!(json.contains("\"iteration\":2"));
        assert!(json.contains("\"max_turns\":6"));
        assert!(json.contains("\"tool_calls\":3"));
        assert!(json.contains("\"max_tool_calls\":10"));
        assert!(json.contains("\"elapsed_ms\":12500"));
        assert!(json.contains("\"last_progress_summary\":\"Running tool: bash\""));

        let decoded: TurnProgress = serde_json::from_str(&json).expect("deserialize turn progress");
        assert_eq!(decoded, progress);
    }

    #[test]
    fn session_progress_request_defaults_to_bounded_recent_events() {
        // Pins: Session/progress does not accidentally become an unbounded event-history endpoint.
        let decoded: SessionProgressRequest =
            serde_json::from_str("{}").expect("deserialize default session progress request");

        assert_eq!(decoded.event_range.from_seq, None);
        assert_eq!(decoded.event_range.to_seq, None);
        assert_eq!(decoded.event_range.event_types, None);
        assert_eq!(decoded.event_range.limit, Some(100));
        assert_eq!(decoded.normalized_event_range().limit, Some(100));
    }

    #[test]
    fn session_progress_request_normalizes_nested_empty_event_range() {
        // Pins: an explicit nested event_range object cannot bypass the bounded default.
        let decoded: SessionProgressRequest = serde_json::from_str(r#"{"event_range":{}}"#)
            .expect("deserialize empty nested event range");

        assert_eq!(decoded.event_range.limit, None);
        assert_eq!(decoded.normalized_event_range().limit, Some(100));
    }

    #[test]
    fn session_progress_request_clamps_oversized_event_limit() {
        // Pins: Session/progress remains a compact progress endpoint, not bulk event export.
        let decoded: SessionProgressRequest =
            serde_json::from_str(r#"{"event_range":{"limit":10000}}"#)
                .expect("deserialize oversized event range");

        assert_eq!(decoded.event_range.limit, Some(10_000));
        assert_eq!(decoded.normalized_event_range().limit, Some(500));
    }

    #[test]
    fn session_progress_response_omits_missing_active_turn_progress() {
        // Pins: idle sessions can use the same convenience endpoint without a synthetic turn object.
        let progress = SessionProgress {
            snapshot: SessionSnapshot {
                session_id: "session-123".to_string(),
                active_turn_id: None,
                pending_message_count: 0,
                last_outcome: None,
            },
            active_turn_progress: None,
            events: Vec::new(),
        };

        let json = serde_json::to_string(&progress).expect("serialize session progress");
        assert!(!json.contains("active_turn_progress"));

        let decoded: SessionProgress =
            serde_json::from_str(&json).expect("deserialize session progress");
        assert_eq!(decoded, progress);
    }
}
