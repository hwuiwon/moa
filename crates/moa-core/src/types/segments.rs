//! Task-segment state shared across session storage and orchestration.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ResolutionScore, SegmentId, SessionId};

/// Derives a stable segment identifier from a session identifier and segment index.
#[must_use]
pub fn deterministic_segment_id(session_id: SessionId, segment_index: u32) -> SegmentId {
    let mut bytes = *session_id.0.as_bytes();
    for (offset, value) in segment_index.to_be_bytes().iter().enumerate() {
        bytes[12 + offset] ^= value;
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    SegmentId(uuid::Uuid::from_bytes(bytes))
}

/// A task segment represents one discrete unit of work within a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSegment {
    /// Stable segment identifier.
    pub id: SegmentId,
    /// Session that owns the segment.
    pub session_id: SessionId,
    /// Tenant scope used for aggregate segment analytics.
    pub tenant_id: String,
    /// Zero-based segment index within the session.
    pub segment_index: u32,
    /// Optional intent label. `None` means undefined.
    pub intent_label: Option<String>,
    /// Optional confidence for the intent label.
    pub intent_confidence: Option<f64>,
    /// Short best-effort task summary.
    pub task_summary: Option<String>,
    /// Segment start timestamp.
    pub started_at: DateTime<Utc>,
    /// Segment end timestamp, when closed.
    pub ended_at: Option<DateTime<Utc>>,
    /// Number of turns attributed to the segment.
    pub turn_count: u32,
    /// Tool names used during the segment.
    pub tools_used: Vec<String>,
    /// Skill names activated during the segment.
    pub skills_activated: Vec<String>,
    /// Token cost attributed to the segment.
    pub token_cost: u64,
    /// Previous segment in the same session, when present.
    pub previous_segment_id: Option<SegmentId>,
    /// Resolution outcome populated by later resolution tracking.
    pub resolution: Option<String>,
    /// Serialized signal breakdown that produced the latest resolution.
    pub resolution_signal: Option<ResolutionScore>,
    /// Confidence for the resolution outcome.
    pub resolution_confidence: Option<f64>,
}

impl TaskSegment {
    /// Returns the lightweight active-segment projection for VO state.
    #[must_use]
    pub fn active_view(&self) -> ActiveSegment {
        ActiveSegment {
            id: self.id,
            segment_index: self.segment_index,
            intent_label: self.intent_label.clone(),
            task_summary: self.task_summary.clone(),
            started_at: self.started_at,
            tools_used: self.tools_used.clone(),
            skills_activated: self.skills_activated.clone(),
            turn_count: self.turn_count,
            token_cost: self.token_cost,
        }
    }
}

/// Lightweight segment reference stored in session VO state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveSegment {
    /// Stable segment identifier.
    pub id: SegmentId,
    /// Zero-based segment index within the session.
    pub segment_index: u32,
    /// Optional intent label. `None` means undefined.
    pub intent_label: Option<String>,
    /// Short best-effort task summary.
    pub task_summary: Option<String>,
    /// Segment start timestamp.
    pub started_at: DateTime<Utc>,
    /// Tool names used during the segment.
    pub tools_used: Vec<String>,
    /// Skill names activated during the segment.
    pub skills_activated: Vec<String>,
    /// Number of turns attributed to the segment.
    pub turn_count: u32,
    /// Token cost attributed to the segment.
    pub token_cost: u64,
}

/// Mutable fields applied when a segment is completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentCompletion {
    /// Segment end timestamp.
    pub ended_at: DateTime<Utc>,
    /// Final segment turn count.
    pub turn_count: u32,
    /// Final segment tool list.
    pub tools_used: Vec<String>,
    /// Final segment skill list.
    pub skills_activated: Vec<String>,
    /// Final token cost attributed to the segment.
    pub token_cost: u64,
}
