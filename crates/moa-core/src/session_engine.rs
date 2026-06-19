//! Shared session-lifecycle rules used by multiple orchestrator adapters.

use crate::{Event, EventRecord, SessionMeta, SessionStatus};

/// Returns whether the persisted session log indicates more work is required.
pub fn session_requires_processing(session: &SessionMeta, events: &[EventRecord]) -> bool {
    if matches!(session.status, SessionStatus::Cancelled) {
        return false;
    }

    events
        .iter()
        .rev()
        .find_map(|record| match record.event {
            Event::SessionStatusChanged { .. }
            | Event::Warning { .. }
            | Event::MemoryWrite { .. }
            | Event::HandDestroyed { .. }
            | Event::HandError { .. }
            | Event::Checkpoint { .. } => None,
            Event::UserMessage { .. }
            | Event::QueuedMessage { .. }
            | Event::ToolResult { .. }
            | Event::ToolError { .. }
            | Event::ToolCall { .. } => Some(true),
            // Action reviews are workspace-admin state and do not resume the turn loop by themselves.
            Event::ActionReviewRequested { .. } | Event::ActionReviewDecided { .. } => Some(false),
            _ => Some(false),
        })
        .unwrap_or(false)
}
