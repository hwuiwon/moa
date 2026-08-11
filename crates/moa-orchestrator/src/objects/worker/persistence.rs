//! Parent session persistence helpers for worker events.

use super::*;

pub(super) fn render_user_message(message: &UserMessage) -> String {
    moa_core::types::channel::render_user_message_with_attachments(
        &message.text,
        &message.attachments,
    )
}
