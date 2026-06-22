//! Session lifecycle, signals, and persisted session state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    Attachment, Channel, ContactId, ContactRef, ModelId, SequenceNum, SessionActorRef,
    SessionChannelBindingId, SessionId, UserId, WorkspaceId,
};

/// Session lifecycle status.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum SessionStatus {
    /// Session exists but has not started execution.
    Created,
    /// Session is currently executing.
    Running,
    /// Session execution is paused.
    Paused,
    /// Session finished successfully.
    Completed,
    /// Session was cancelled.
    Cancelled,
    /// Session failed.
    Failed,
}

impl SessionStatus {
    /// Returns the stable database representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// Canonical user-authored message payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    /// Message text.
    pub text: String,
    /// Attached files or images.
    pub attachments: Vec<Attachment>,
}

/// Cancellation mode requested for a session virtual object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelMode {
    /// Finish the current step, then stop at the next cooperative boundary.
    Soft,
    /// Abort as soon as the session reaches the next cancellation check.
    Hard,
}

/// Outcome returned by one brain-loop iteration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    /// Another turn should run immediately.
    Continue,
    /// No more work is pending right now.
    Idle,
    /// The session has been cancelled.
    Cancelled,
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
    /// Active delivery channel.
    pub channel: Channel,
    /// Active session channel binding, when persisted.
    #[serde(default)]
    pub active_channel_binding_id: Option<SessionChannelBindingId>,
    /// Model identifier.
    pub model: ModelId,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last update timestamp.
    pub updated_at: DateTime<Utc>,
    /// Completion timestamp.
    pub completed_at: Option<DateTime<Utc>>,
    /// Parent session identifier for child sessions.
    pub parent_session_id: Option<SessionId>,
    /// Agent-facing contact attached to this session.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// Boundary actor that created this session.
    #[serde(default)]
    pub created_by: Option<SessionActorRef>,
    /// Previous anonymous or unverified contact promoted into the current contact.
    #[serde(default)]
    pub contact_promoted_from_id: Option<ContactId>,
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
            channel: Channel::Chat,
            active_channel_binding_id: None,
            model: ModelId::new(""),
            created_at: now,
            updated_at: now,
            completed_at: None,
            parent_session_id: None,
            contact: None,
            created_by: None,
            contact_promoted_from_id: None,
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
    /// Active delivery channel.
    pub channel: Channel,
    /// Model identifier.
    pub model: ModelId,
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
    /// Restrict to a single communication channel.
    pub channel: Option<Channel>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}
