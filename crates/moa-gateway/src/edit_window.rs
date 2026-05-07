//! Edit-window fallback handling for gateway adapters.

use std::future::Future;

use moa_core::{MessageId, MoaError, Platform, Result};

/// Normalized edit response returned by a platform adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayEditResponse {
    /// HTTP or platform status code.
    pub status: u16,
    /// Platform response body.
    pub body: String,
    /// Edited platform message id when the edit succeeded.
    pub message_id: Option<MessageId>,
}

impl GatewayEditResponse {
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
pub enum GatewayEditOutcome {
    /// The original platform message was edited.
    Edited {
        /// Edited platform message id.
        message_id: MessageId,
    },
    /// The edit failed with a stale-window error and a follow-up message was sent instead.
    FollowUp {
        /// New follow-up platform message id.
        message_id: MessageId,
        /// Original platform message id used as the reply reference.
        reply_to: MessageId,
        /// Byte-for-byte fallback message content.
        content: String,
    },
}

/// Runs an edit and falls back to a threaded or replied follow-up when the platform rejects it.
pub async fn edit_with_followup_fallback<EditFn, EditFut, FollowFn, FollowFut>(
    platform: Platform,
    original_message_id: MessageId,
    content: String,
    edit: EditFn,
    followup: FollowFn,
) -> Result<GatewayEditOutcome>
where
    EditFn: FnOnce(String) -> EditFut,
    EditFut: Future<Output = Result<GatewayEditResponse>>,
    FollowFn: FnOnce(MessageId, String) -> FollowFut,
    FollowFut: Future<Output = Result<MessageId>>,
{
    let edit_response = edit(content.clone()).await?;
    if is_fallback_edit_error(&platform, &edit_response) {
        let message_id = followup(original_message_id.clone(), content.clone()).await?;
        return Ok(GatewayEditOutcome::FollowUp {
            message_id,
            reply_to: original_message_id,
            content,
        });
    }

    if edit_response.status < 400 {
        let message_id = edit_response
            .message_id
            .unwrap_or_else(|| original_message_id.clone());
        return Ok(GatewayEditOutcome::Edited { message_id });
    }

    Err(MoaError::ProviderError(format!(
        "{} edit failed with status {}: {}",
        platform, edit_response.status, edit_response.body
    )))
}

/// Returns true when an edit error should be converted into a follow-up message.
pub fn is_fallback_edit_error(platform: &Platform, response: &GatewayEditResponse) -> bool {
    let body = response.body.to_ascii_lowercase();
    match platform {
        Platform::Telegram => {
            response.status == 400
                && (body.contains("message can't be edited")
                    || body.contains("message can not be edited"))
        }
        Platform::Slack => {
            body.contains("message_not_found") || body.contains("cant_update_message")
        }
        Platform::Discord => response.status == 404 || body.contains("unknown message"),
        Platform::Cli => false,
    }
}
