//! Session lifecycle, signals, and persisted session state.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    action_policy::CallOrigin, agent::AgentContext, channel::Attachment, channel::Channel,
    channel::SessionChannelBindingId, contact::ContactId, contact::ContactRef,
    contact::SessionActorRef, events_stream::SequenceNum, identifiers::ModelId,
    identifiers::SessionId, identifiers::TenantId,
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
    /// Session is inactive between turns and can accept new work.
    Idle,
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

/// Scope of a cancellation requested for a session virtual object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelScope {
    /// Stop only the active coordinator turn; leave children running.
    CoordinatorOnly,
    /// Cancel the active coordinator turn and the whole child task tree (today's behavior).
    TaskTree,
}

impl Default for CancelScope {
    /// Defaults to [`CancelScope::TaskTree`] so a bare "stop" cancels the coordinator turn and
    /// the whole child task tree, preserving the historical cancel-everything behavior.
    fn default() -> Self {
        Self::TaskTree
    }
}

impl CancelScope {
    /// Returns whether this scope cancels the whole child task tree in addition to the active
    /// coordinator turn. [`CancelScope::CoordinatorOnly`] leaves registered children running.
    #[must_use]
    pub fn cancels_task_tree(self) -> bool {
        matches!(self, Self::TaskTree)
    }
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
pub struct Checkpoint {
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
    /// Tenant runtime boundary that owns this session.
    pub tenant_id: TenantId,
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
    /// Admin, operator, service, contact, or anonymous actor that created this session.
    #[serde(default)]
    pub created_by: Option<SessionActorRef>,
    /// Previous anonymous or unverified contact promoted into the current contact.
    #[serde(default)]
    pub contact_promoted_from_id: Option<ContactId>,
    /// Configured agent revision and policy snapshot pinned to this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_context: Option<AgentContext>,
    /// Provenance class of the runtime this session was created for.
    ///
    /// Stated once at creation and durable from then on, because every tool
    /// dispatch — the immediate path, the durable recovery path, a worker turn,
    /// an execution task, and a cleared action review — reloads this record
    /// rather than carrying the origin in its request. An eval-owned session
    /// therefore cannot lose its ceiling by taking a different path, and no
    /// request field can restore what the session gave up.
    #[serde(default)]
    pub call_origin: CallOrigin,
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
            tenant_id: TenantId::new(),
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
            agent_context: Some(AgentContext::system_default()),
            call_origin: CallOrigin::Production,
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
    /// Tenant runtime boundary that owns this session.
    pub tenant_id: TenantId,
    /// Agent-facing contact attached to this session.
    #[serde(default)]
    pub contact: Option<ContactRef>,
    /// Admin, operator, service, contact, or anonymous actor that created this session.
    #[serde(default)]
    pub created_by: Option<SessionActorRef>,
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
    /// Restrict to a single tenant runtime boundary.
    pub tenant_id: Option<TenantId>,
    /// Restrict to a single agent-facing contact.
    pub contact_id: Option<ContactId>,
    /// Restrict to a single creator actor.
    pub created_by: Option<SessionActorRef>,
    /// Restrict to a single status.
    pub status: Option<SessionStatus>,
    /// Restrict to a single communication channel.
    pub channel: Option<Channel>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::SessionStatus;

    #[test]
    fn session_status_idle_is_a_hard_serialization_cutover() {
        // Pins: new readers emit and accept only `idle`; retaining `paused` as a
        // serde alias would let the one-way persisted-state cutover silently drift.
        assert_eq!(
            serde_json::to_string(&SessionStatus::Idle)
                .expect("idle session status should serialize"),
            "\"idle\""
        );
        assert_eq!(
            serde_json::from_str::<SessionStatus>("\"idle\"")
                .expect("idle session status should deserialize"),
            SessionStatus::Idle
        );
        serde_json::from_str::<SessionStatus>("\"paused\"")
            .expect_err("the retired paused label must not remain readable");
    }
}
