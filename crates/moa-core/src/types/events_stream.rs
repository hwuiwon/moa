//! Event log metadata, filters, and record types.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::events::Event;

use super::{BrainId, ContactId, SessionId, TenantId};

/// Monotonic event sequence number within a session.
pub type SequenceNum = u64;

/// Event type discriminator used for filtering and indexing.
///
/// The strum `IntoStaticStr`/`EnumString` derives intentionally use the verbatim
/// PascalCase variant names — that is the persisted database representation
/// (see `event_type_to_db`), which differs from the snake_case serde/JSON form.
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
pub enum EventType {
    /// `SessionCreated`.
    SessionCreated,
    /// `SessionStatusChanged`.
    SessionStatusChanged,
    /// `SessionChannelChanged`.
    SessionChannelChanged,
    /// `SessionCompleted`.
    SessionCompleted,
    /// `SegmentStarted`.
    SegmentStarted,
    /// `SegmentCompleted`.
    SegmentCompleted,
    /// `UserMessage`.
    UserMessage,
    /// `QueuedMessage`.
    QueuedMessage,
    /// `BrainThinking`.
    BrainThinking,
    /// `BrainResponse`.
    BrainResponse,
    /// `ToolCall`.
    ToolCall,
    /// `ToolResult`.
    ToolResult,
    /// `ToolError`.
    ToolError,
    /// `ActionReviewRequested`.
    ActionReviewRequested,
    /// `ActionReviewDecided`.
    ActionReviewDecided,
    /// `SubAgentSpawned`.
    SubAgentSpawned,
    /// `SubAgentMessageSent`.
    SubAgentMessageSent,
    /// `SubAgentStatusChanged`.
    SubAgentStatusChanged,
    /// `SubAgentNotificationDelivered`.
    SubAgentNotificationDelivered,
    /// `MemoryRead`.
    MemoryRead,
    /// `MemoryWrite`.
    MemoryWrite,
    /// `MemoryIngest`.
    MemoryIngest,
    /// `HandProvisioned`.
    HandProvisioned,
    /// `HandDestroyed`.
    HandDestroyed,
    /// `HandError`.
    HandError,
    /// `Checkpoint`.
    Checkpoint,
    /// `CacheReport`.
    CacheReport,
    /// `Error`.
    Error,
    /// `Warning`.
    Warning,
}

impl EventType {
    /// Returns the stable database representation.
    ///
    /// This is the verbatim PascalCase variant name (the persisted form), which
    /// is intentionally distinct from the snake_case serde/JSON representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// Event listing range.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventRange {
    /// First sequence number to include.
    pub from_seq: Option<SequenceNum>,
    /// Last sequence number to include.
    pub to_seq: Option<SequenceNum>,
    /// Event type filter.
    pub event_types: Option<Vec<EventType>>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

impl EventRange {
    /// Returns a range that includes every event.
    pub fn all() -> Self {
        Self::default()
    }

    /// Returns the latest `limit` events in chronological order.
    pub fn recent(limit: usize) -> Self {
        Self {
            limit: Some(limit),
            ..Self::default()
        }
    }
}

/// Reference to a payload stored outside the session event row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimCheck {
    /// Content-addressed blob identifier.
    pub blob_id: String,
    /// Original payload size in bytes.
    pub size: usize,
    /// Searchable inline preview of the payload.
    pub preview: String,
}

/// Event search filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFilter {
    /// Restrict to a single session.
    pub session_id: Option<SessionId>,
    /// Restrict to a single tenant runtime boundary.
    pub tenant_id: Option<TenantId>,
    /// Restrict to a single agent-facing contact.
    pub contact_id: Option<ContactId>,
    /// Restrict to event types.
    pub event_types: Option<Vec<EventType>>,
    /// Lower timestamp bound.
    pub from_time: Option<DateTime<Utc>>,
    /// Upper timestamp bound.
    pub to_time: Option<DateTime<Utc>>,
    /// Maximum number of results.
    pub limit: Option<usize>,
}

/// A stored event record with metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    /// Event identifier.
    pub id: Uuid,
    /// Session identifier.
    pub session_id: SessionId,
    /// Sequence number.
    pub sequence_num: SequenceNum,
    /// Event type discriminator.
    pub event_type: EventType,
    /// Event payload.
    pub event: Event,
    /// Emission timestamp.
    pub timestamp: DateTime<Utc>,
    /// Brain that emitted the event.
    pub brain_id: Option<BrainId>,
    /// Hand involved in the event.
    pub hand_id: Option<String>,
    /// Optional token count attributed to the event.
    pub token_count: Option<usize>,
}
