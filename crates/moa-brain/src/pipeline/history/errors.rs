//! Error-preservation rules for compacted history replay.

use moa_core::{ContextMessage, ContextSourceRef, Event, EventRecord};

pub(crate) fn preserved_error_messages(events: &[&EventRecord]) -> Vec<ContextMessage> {
    let mut messages = Vec::new();
    for record in events {
        match &record.event {
            Event::Error { message, .. } => messages.push(
                ContextMessage::system(format!("<previous_error>{message}</previous_error>"))
                    .with_source_ref(ContextSourceRef::session_event(record)),
            ),
            Event::ToolError { error, tool_id, .. } => messages.push(
                ContextMessage::tool(format!("<tool_error id=\"{tool_id}\">{error}</tool_error>"))
                    .with_source_ref(ContextSourceRef::tool_error_event(record, *tool_id)),
            ),
            _ => {}
        }
    }
    messages
}
