//! User-message event fixture.

use moa_core::Event;

/// Returns a user-message event suitable for append-event tests.
pub fn user_message_event(text: impl Into<String>) -> Event {
    Event::UserMessage {
        text: text.into(),
        attachments: vec![],
    }
}
