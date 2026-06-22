//! Edit-window fallback handling for messaging adapters.

use std::future::Future;

use moa_core::{Channel, MessageId, MoaError, Result};

/// Normalized edit response returned by a channel adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagingEditResponse {
    /// HTTP or channel-provider status code.
    pub status: u16,
    /// Channel response body.
    pub body: String,
    /// Edited channel message id when the edit succeeded.
    pub message_id: Option<MessageId>,
}

impl MessagingEditResponse {
    /// Creates a successful edit response.
    pub fn success(message_id: MessageId) -> Self {
        Self {
            status: 200,
            body: String::new(),
            message_id: Some(message_id),
        }
    }

    /// Creates a failed edit response.
    pub fn failure(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            message_id: None,
        }
    }
}

/// Result of one edit operation after fallback handling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MessagingEditOutcome {
    /// The original channel message was edited.
    Edited {
        /// Edited channel message id.
        message_id: MessageId,
    },
    /// The edit failed with a stale-window error and a follow-up message was sent instead.
    FollowUp {
        /// New follow-up channel message id.
        message_id: MessageId,
        /// Original channel message id used as the reply reference.
        reply_to: MessageId,
        /// Byte-for-byte fallback message content.
        content: String,
    },
}

/// Runs an edit and falls back to a threaded or replied follow-up when the channel rejects it.
pub async fn edit_with_followup_fallback<EditFn, EditFut, FollowFn, FollowFut>(
    channel: Channel,
    original_message_id: MessageId,
    content: String,
    edit: EditFn,
    followup: FollowFn,
) -> Result<MessagingEditOutcome>
where
    EditFn: FnOnce(String) -> EditFut,
    EditFut: Future<Output = Result<MessagingEditResponse>>,
    FollowFn: FnOnce(MessageId, String) -> FollowFut,
    FollowFut: Future<Output = Result<MessageId>>,
{
    let edit_response = edit(content.clone()).await?;
    if is_fallback_edit_error(&channel, &edit_response) {
        let message_id = followup(original_message_id.clone(), content.clone()).await?;
        return Ok(MessagingEditOutcome::FollowUp {
            message_id,
            reply_to: original_message_id,
            content,
        });
    }

    if edit_response.status < 400 {
        let message_id = edit_response
            .message_id
            .unwrap_or_else(|| original_message_id.clone());
        return Ok(MessagingEditOutcome::Edited { message_id });
    }

    Err(MoaError::ProviderError(format!(
        "{} edit failed with status {}: {}",
        channel, edit_response.status, edit_response.body
    )))
}

/// Returns true when an edit error should be converted into a follow-up message.
pub fn is_fallback_edit_error(channel: &Channel, response: &MessagingEditResponse) -> bool {
    let body = response.body.to_ascii_lowercase();
    match channel {
        Channel::Slack => {
            body.contains("message_not_found") || body.contains("cant_update_message")
        }
        _ => false,
    }
}
