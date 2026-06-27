//! Session user-message request fixture.

use moa_core::UserMessage;

/// Returns a user message payload suitable for `Session/post_message`.
pub fn user_message(text: impl Into<String>) -> UserMessage {
    UserMessage {
        text: text.into(),
        attachments: vec![],
    }
}
