//! Session lifecycle, signals, and persisted session state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::error::Result;

use super::{
    Attachment, EventRecord, PendingSignalId, Platform, SequenceNum, SessionId, UserId, WorkspaceId,
};

/// Session lifecycle status.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Session exists but has not started execution.
    Created,
    /// Session is currently executing.
    Running,
    /// Session execution is paused.
    Paused,
    /// Session is blocked on human approval.
    WaitingApproval,
    /// Session finished successfully.
    Completed,
    /// Session was cancelled.
    Cancelled,
    /// Session failed.
    Failed,
}

/// Observation verbosity for a live session stream.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserveLevel {
    /// Summary-only observation.
    Summary,
    /// Standard observation.
    Normal,
    /// Most detailed observation.
    Verbose,
}

/// Canonical user-authored message payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    /// Message text.
    pub text: String,
    /// Attached files or images.
    pub attachments: Vec<Attachment>,
}

/// Signals delivered to a running session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSignal {
    /// Queue a new user message for the session.
    QueueMessage(UserMessage),
    /// Request a graceful cancellation.
    SoftCancel,
    /// Request an immediate cancellation.
    HardCancel,
    /// Notify the session of an approval decision.
    ApprovalDecided {
        /// Approval request identifier.
        request_id: Uuid,
        /// User decision.
        decision: super::ApprovalDecision,
    },
}

/// In-memory buffered user message awaiting turn-boundary processing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BufferedUserMessage {
    /// The user-visible message payload.
    pub message: UserMessage,
    /// Optional durable pending-signal identifier associated with the message.
    pub pending_signal_id: Option<PendingSignalId>,
}

impl BufferedUserMessage {
    /// Creates a buffered message without a durable pending-signal identifier.
    pub fn direct(message: UserMessage) -> Self {
        Self {
            message,
            pending_signal_id: None,
        }
    }

    /// Creates a buffered message from a persisted pending signal.
    pub fn from_pending_signal(signal: PendingSignal) -> Result<Self> {
        Ok(Self {
            message: signal.user_message()?,
            pending_signal_id: Some(signal.id),
        })
    }
}

/// Supported pending signal kinds stored durably outside the append-only event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingSignalType {
    /// A user message queued for later turn-boundary flush.
    QueueMessage,
}

/// Durable but unresolved session signal awaiting turn-boundary processing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingSignal {
    /// Stable identifier for this stored signal.
    pub id: PendingSignalId,
    /// Session that owns the pending signal.
    pub session_id: SessionId,
    /// Kind of pending signal.
    pub signal_type: PendingSignalType,
    /// JSON-serialized payload for the signal.
    pub payload: Value,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
}

impl PendingSignal {
    /// Creates a pending queued-message signal from a user message.
    pub fn queue_message(session_id: SessionId, message: UserMessage) -> Result<Self> {
        Ok(Self {
            id: PendingSignalId::new(),
            session_id,
            signal_type: PendingSignalType::QueueMessage,
            payload: serde_json::to_value(message)?,
            created_at: Utc::now(),
        })
    }

    /// Decodes the queued user message payload.
    pub fn user_message(&self) -> Result<UserMessage> {
        match self.signal_type {
            PendingSignalType::QueueMessage => Ok(serde_json::from_value(self.payload.clone())?),
        }
    }
}

/// Handle returned for a database checkpoint branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointHandle {
    /// Neon branch identifier.
    pub id: String,
    /// Human-readable label for the checkpoint.
    pub label: String,
    /// Connection string for the checkpoint branch.
    pub connection_url: String,
    /// Creation timestamp of the checkpoint.
    pub created_at: DateTime<Utc>,
    /// Session associated with the checkpoint, if any.
    pub session_id: Option<SessionId>,
}

/// Metadata about an active checkpoint branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointInfo {
    /// Primary checkpoint handle.
    pub handle: CheckpointHandle,
    /// Approximate logical size of the branch in bytes, when available.
    pub size_bytes: Option<u64>,
    /// Parent branch identifier for this checkpoint.
    pub parent_branch: String,
}

/// Request for starting a new session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartSessionRequest {
    /// Workspace the session belongs to.
    pub workspace_id: WorkspaceId,
    /// User initiating the session.
    pub user_id: UserId,
    /// Source platform.
    pub platform: Platform,
    /// Model identifier to use.
    pub model: String,
    /// Optional first message.
    pub initial_message: Option<UserMessage>,
    /// Optional title override.
    pub title: Option<String>,
    /// Optional parent session for child-brain flows.
    pub parent_session_id: Option<SessionId>,
}

/// Handle returned when a session starts or resumes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    /// Running session identifier.
    pub session_id: SessionId,
}

/// Snapshot of the active workspace budget state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBudgetStatus {
    /// Configured daily workspace budget in cents. `0` means unlimited.
    pub daily_budget_cents: u32,
    /// Total spend for the active UTC day in cents.
    pub daily_spent_cents: u32,
}

/// Persistent session metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Session identifier.
    pub id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Optional title.
    pub title: Option<String>,
    /// Current session status.
    pub status: SessionStatus,
    /// Source platform.
    pub platform: Platform,
    /// Platform-specific channel identifier.
    pub platform_channel: Option<String>,
    /// Model identifier.
    pub model: String,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Completion timestamp.
    pub completed_at: Option<DateTime<Utc>>,
    /// Parent session identifier for child sessions.
    pub parent_session_id: Option<SessionId>,
    /// Aggregate input token usage across all cache states.
    pub total_input_tokens: usize,
    /// Aggregate uncached input token usage.
    #[serde(default)]
    pub total_input_tokens_uncached: usize,
    /// Aggregate cache-write input token usage.
    #[serde(default)]
    pub total_input_tokens_cache_write: usize,
    /// Aggregate cache-read input token usage.
    #[serde(default)]
    pub total_input_tokens_cache_read: usize,
    /// Aggregate output token usage.
    pub total_output_tokens: usize,
    /// Aggregate cost in cents.
    pub total_cost_cents: u32,
    /// Number of events in the session log.
    pub event_count: usize,
    /// Sequence number of the last checkpoint event.
    pub last_checkpoint_seq: Option<SequenceNum>,
}

impl SessionMeta {
    /// Returns the fraction of total input tokens that were served from cache for this session.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.total_input_tokens == 0 {
            return 0.0;
        }

        self.total_input_tokens_cache_read as f64 / self.total_input_tokens as f64
    }
}

impl Default for SessionMeta {
    fn default() -> Self {
        let now = Utc::now();
        Self {
            id: SessionId::new(),
            workspace_id: WorkspaceId::new(""),
            user_id: UserId::new(""),
            title: None,
            status: SessionStatus::Created,
            platform: Platform::Desktop,
            platform_channel: None,
            model: String::new(),
            created_at: now,
            updated_at: now,
            completed_at: None,
            parent_session_id: None,
            total_input_tokens: 0,
            total_input_tokens_uncached: 0,
            total_input_tokens_cache_write: 0,
            total_input_tokens_cache_read: 0,
            total_output_tokens: 0,
            total_cost_cents: 0,
            event_count: 0,
            last_checkpoint_seq: None,
        }
    }
}

/// A compact session listing record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// Session identifier.
    pub session_id: SessionId,
    /// Workspace identifier.
    pub workspace_id: WorkspaceId,
    /// User identifier.
    pub user_id: UserId,
    /// Optional title.
    pub title: Option<String>,
    /// Current status.
    pub status: SessionStatus,
    /// Source platform.
    pub platform: Platform,
    /// Model identifier.
    pub model: String,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
}

/// Session listing filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFilter {
    /// Restrict to a single workspace.
    pub workspace_id: Option<WorkspaceId>,
    /// Restrict to a single user.
    pub user_id: Option<UserId>,
    /// Restrict to a single status.
    pub status: Option<SessionStatus>,
    /// Restrict to a single platform.
    pub platform: Option<Platform>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

/// Recovered session state returned when a brain wakes from the event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WakeContext {
    /// Current persisted session metadata.
    pub session: SessionMeta,
    /// Most recent checkpoint summary, if one exists.
    pub checkpoint_summary: Option<String>,
    /// Events that occurred after the checkpoint, or all events when no checkpoint exists.
    pub recent_events: Vec<EventRecord>,
    /// Unresolved queued signals that must be re-buffered on resume.
    #[serde(default)]
    pub pending_signals: Vec<PendingSignal>,
}

#[cfg(test)]
mod tests {
    use super::{
        PendingSignal, PendingSignalType, SessionId, SessionMeta, SessionStatus, UserMessage,
    };

    #[test]
    fn session_status_serialization() {
        let status = SessionStatus::WaitingApproval;
        let json = serde_json::to_string(&status).expect("serialize session status");
        assert!(json.contains("WaitingApproval") || json.contains("waiting_approval"));
    }

    #[test]
    fn pending_signal_queue_message_round_trip() {
        let session_id = SessionId::new();
        let signal = PendingSignal::queue_message(
            session_id.clone(),
            UserMessage {
                text: "queued".to_string(),
                attachments: vec![],
            },
        )
        .expect("create pending signal");

        assert_eq!(signal.session_id, session_id);
        assert_eq!(signal.signal_type, PendingSignalType::QueueMessage);
        assert_eq!(
            signal
                .user_message()
                .expect("decode queued user message")
                .text,
            "queued"
        );
    }

    #[test]
    fn session_meta_default_builds_created_session() {
        let meta = SessionMeta::default();
        assert_eq!(meta.status, SessionStatus::Created);
    }
}
