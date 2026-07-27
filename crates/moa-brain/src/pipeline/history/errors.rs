//! Error-preservation rules for compacted history replay.

use moa_core::{
    events::Event, types::context::ContextMessage, types::context::ContextSourceRef,
    types::events_stream::EventRecord,
};

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
            // A prior turn that died must survive compaction, or the model retries
            // into the same wall with no record that the attempt was already made.
            // The canonical fact's summary is bounded and secret-free by
            // construction, so it is safe to replay verbatim.
            Event::TurnFailed { actor, summary, .. } => messages.push(
                ContextMessage::system(format!(
                    "<previous_turn_failure actor=\"{}\">{summary}</previous_turn_failure>",
                    actor.actor_key()
                ))
                .with_source_ref(ContextSourceRef::session_event(record)),
            ),
            _ => {}
        }
    }
    messages
}
