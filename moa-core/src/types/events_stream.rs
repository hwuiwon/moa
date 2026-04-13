//! Event log metadata, filters, and live stream utilities.

use std::fmt::{self, Formatter};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::error::{MoaError, Result};
use crate::events::Event;

use super::{BrainId, SessionId, UserId, WorkspaceId};

/// Monotonic event sequence number within a session.
pub type SequenceNum = u64;

/// Event type discriminator used for filtering and indexing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    /// `SessionCreated`.
    SessionCreated,
    /// `SessionStatusChanged`.
    SessionStatusChanged,
    /// `SessionCompleted`.
    SessionCompleted,
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
    /// `ApprovalRequested`.
    ApprovalRequested,
    /// `ApprovalDecided`.
    ApprovalDecided,
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
    /// `Error`.
    Error,
    /// `Warning`.
    Warning,
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

    /// Returns a range constrained only by a result limit.
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

/// String payload that may be stored inline or behind a claim check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaybeBlob {
    /// Payload stored directly in the event row.
    Inline(String),
    /// Payload stored in the blob store.
    BlobRef(ClaimCheck),
}

impl MaybeBlob {
    /// Returns the inline text when available, otherwise the stored preview.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Inline(value) => value,
            Self::BlobRef(claim_check) => &claim_check.preview,
        }
    }

    /// Returns whether the payload has been offloaded to blob storage.
    pub fn is_blob_ref(&self) -> bool {
        matches!(self, Self::BlobRef(_))
    }

    /// Consumes the wrapper and returns the inline text or preview.
    pub fn into_string(self) -> String {
        match self {
            Self::Inline(value) => value,
            Self::BlobRef(claim_check) => claim_check.preview,
        }
    }
}

/// Event search filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventFilter {
    /// Restrict to a single session.
    pub session_id: Option<SessionId>,
    /// Restrict to a workspace.
    pub workspace_id: Option<WorkspaceId>,
    /// Restrict to a user.
    pub user_id: Option<UserId>,
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

/// Lightweight event stream with optional live broadcast updates.
///
/// NOTE: This type wraps `tokio::sync::broadcast::Receiver` and would ideally
/// sit closer to the orchestrator/session-store implementations that construct
/// it. It remains in `moa-core` because `BrainOrchestrator::observe` lives in
/// `moa-core` and returns `EventStream`, so moving it out would force a wider
/// trait and crate-boundary redesign without removing the existing unconditional
/// Tokio dependency from core.
#[derive(Serialize, Deserialize)]
pub struct EventStream {
    /// Buffered events currently available in the stream.
    pub events: Vec<EventRecord>,
    #[serde(skip)]
    receiver: Option<broadcast::Receiver<EventRecord>>,
    #[serde(skip)]
    cursor: usize,
}

impl EventStream {
    /// Creates an event stream from buffered historical events.
    pub fn from_events(events: Vec<EventRecord>) -> Self {
        Self {
            events,
            receiver: None,
            cursor: 0,
        }
    }

    /// Creates an event stream backed by a live broadcast receiver.
    pub fn from_broadcast(receiver: broadcast::Receiver<EventRecord>) -> Self {
        Self {
            events: Vec::new(),
            receiver: Some(receiver),
            cursor: 0,
        }
    }

    /// Creates an event stream from buffered history plus live broadcast updates.
    pub fn from_history_and_broadcast(
        events: Vec<EventRecord>,
        receiver: broadcast::Receiver<EventRecord>,
    ) -> Self {
        Self {
            events,
            receiver: Some(receiver),
            cursor: 0,
        }
    }

    /// Receives the next buffered or live event from the stream.
    pub async fn next(&mut self) -> Option<Result<EventRecord>> {
        if self.cursor < self.events.len() {
            let event = self.events[self.cursor].clone();
            self.cursor += 1;
            if self.cursor == self.events.len() {
                self.events.clear();
                self.cursor = 0;
            }
            return Some(Ok(event));
        }

        match &mut self.receiver {
            Some(receiver) => match receiver.recv().await {
                Ok(event) => Some(Ok(event)),
                Err(broadcast::error::RecvError::Closed) => None,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "event stream lagged behind live broadcast");
                    Some(Err(MoaError::StreamError(format!(
                        "event stream lagged by {skipped} messages"
                    ))))
                }
            },
            None => None,
        }
    }
}

impl Clone for EventStream {
    fn clone(&self) -> Self {
        Self {
            events: self.events.clone(),
            receiver: self.receiver.as_ref().map(broadcast::Receiver::resubscribe),
            cursor: self.cursor,
        }
    }
}

impl Default for EventStream {
    fn default() -> Self {
        Self::from_events(Vec::new())
    }
}

impl fmt::Debug for EventStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventStream")
            .field("events", &self.events)
            .field("live", &self.receiver.is_some())
            .finish()
    }
}

impl PartialEq for EventStream {
    fn eq(&self, other: &Self) -> bool {
        self.events == other.events && self.cursor == other.cursor
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use tokio::sync::broadcast;
    use uuid::Uuid;

    use super::{EventRecord, EventStream, EventType, SessionId};
    use crate::error::MoaError;
    use crate::events::Event;

    #[tokio::test]
    async fn event_stream_reports_lagged_broadcasts() {
        let (tx, rx) = broadcast::channel(1);
        let mut stream = EventStream::from_broadcast(rx);
        let session_id = SessionId::new();

        let first = EventRecord {
            id: Uuid::new_v4(),
            session_id: session_id.clone(),
            sequence_num: 0,
            event_type: EventType::Warning,
            event: Event::Warning {
                message: "first".to_string(),
            },
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        };
        let second = EventRecord {
            id: Uuid::new_v4(),
            session_id,
            sequence_num: 1,
            event_type: EventType::Warning,
            event: Event::Warning {
                message: "second".to_string(),
            },
            timestamp: Utc::now(),
            brain_id: None,
            hand_id: None,
            token_count: None,
        };

        let _ = tx.send(first);
        let _ = tx.send(second);

        let error = stream
            .next()
            .await
            .transpose()
            .expect_err("lagged broadcast should surface an error");
        assert!(matches!(error, MoaError::StreamError(message) if message.contains("lagged")));
    }
}
