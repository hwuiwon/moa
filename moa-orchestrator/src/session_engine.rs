//! Shared session-lifecycle rules used by multiple orchestrator adapters.

use moa_brain::{find_pending_tool_approval, find_resolved_pending_tool_approval};
use moa_core::{Event, EventRecord, SessionMeta, SessionStatus};

/// Returns whether the persisted session log indicates more work is required.
pub(crate) fn session_requires_processing(session: &SessionMeta, events: &[EventRecord]) -> bool {
    if matches!(session.status, SessionStatus::Cancelled) {
        return false;
    }

    if find_pending_tool_approval(events).is_some()
        || find_resolved_pending_tool_approval(events).is_some()
    {
        return true;
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
            | Event::ApprovalDecided { .. }
            | Event::ToolCall { .. } => Some(true),
            _ => Some(false),
        })
        .unwrap_or(false)
}
