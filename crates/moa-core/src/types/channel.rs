//! Channel transport and message rendering types.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    ActionEnvelope, ActionReviewPreview, ContactPointId, SessionAttachmentId, SessionId,
    SessionStatus, UserId,
};

uuid_id!(
    /// Identifier for a provider-native channel account linked to a contact.
    pub struct ChannelAccountId
);

uuid_id!(
    /// Identifier for a session's active or historical channel route binding.
    pub struct SessionChannelBindingId
);

/// Communication channel a session or message uses.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Default,
    Serialize,
    Deserialize,
    strum::IntoStaticStr,
    strum::EnumString,
    strum::Display,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum Channel {
    /// First-party or generic chat conversation.
    #[default]
    Chat,
    /// Slack conversation.
    Slack,
    /// Email delivery.
    Email,
    /// SMS delivery.
    Sms,
}

impl Channel {
    /// Returns the canonical lowercase string label for this channel variant.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// Provider-native actor observed on a channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelActor {
    /// Provider-native actor identifier.
    pub external_id: String,
    /// Display name.
    pub display_name: String,
    /// Linked channel account identifier, when known.
    #[serde(default)]
    pub channel_account_id: Option<ChannelAccountId>,
    /// Linked MOA user identifier, when known.
    #[serde(default)]
    pub moa_user_id: Option<UserId>,
}

/// Lightweight channel account projection suitable for API responses and events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelAccountRef {
    /// Channel account identifier.
    pub channel_account_id: ChannelAccountId,
    /// Contact point backing email or SMS accounts, when applicable.
    #[serde(default)]
    pub contact_point_id: Option<ContactPointId>,
    /// Communication channel for this account.
    pub channel: Channel,
    /// Display-safe account label.
    #[serde(default)]
    pub display_name: Option<String>,
}

/// Normalized route reference for a communication channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelRef {
    /// First-party or generic chat conversation.
    Chat {
        /// Stable conversation identifier supplied by the API client.
        conversation_id: String,
        /// Channel-native user identifier, when known.
        #[serde(default)]
        user_id: Option<String>,
        /// API client session identifier, when known.
        #[serde(default)]
        client_session_id: Option<String>,
    },
    /// Slack conversation or thread.
    Slack {
        /// Slack workspace or enterprise identifier, when known.
        #[serde(default)]
        team_id: Option<String>,
        /// Slack channel identifier.
        #[serde(default)]
        slack_channel_id: Option<String>,
        /// Slack thread timestamp, when the route is a thread.
        #[serde(default)]
        thread_ts: Option<String>,
        /// Slack user identifier for direct messages.
        #[serde(default)]
        user_id: Option<String>,
    },
    /// Email route backed by a channel account and contact point.
    Email {
        /// Channel account used for email delivery.
        channel_account_id: ChannelAccountId,
    },
    /// SMS route backed by a channel account and contact point.
    Sms {
        /// Channel account used for SMS delivery.
        channel_account_id: ChannelAccountId,
    },
}

impl ChannelRef {
    /// Returns the communication channel used by this route.
    #[must_use]
    pub fn channel(&self) -> Channel {
        match self {
            Self::Chat { .. } => Channel::Chat,
            Self::Slack { .. } => Channel::Slack,
            Self::Email { .. } => Channel::Email,
            Self::Sms { .. } => Channel::Sms,
        }
    }
}

/// Active channel binding route for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionChannelBinding {
    /// Binding identifier stored in `session_channel_bindings`.
    pub binding_id: SessionChannelBindingId,
    /// Concrete channel route for outbound replies.
    pub channel_ref: ChannelRef,
}

/// File or rich attachment metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Durable session attachment id when the attachment is stored by MOA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<SessionAttachmentId>,
    /// Attachment display name.
    pub name: String,
    /// MIME type when known.
    pub mime_type: Option<String>,
    /// Lowercase SHA-256 hex digest when the attachment is stored by MOA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Remote URL when applicable.
    pub url: Option<String>,
    /// Local filesystem path when applicable.
    pub path: Option<PathBuf>,
    /// Attachment size in bytes when known.
    pub size_bytes: Option<u64>,
}

/// Renders a user-authored text body with durable attachment references.
#[must_use]
pub fn render_user_message_with_attachments(text: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return text.to_string();
    }

    let mut rendered = String::new();
    if !text.trim().is_empty() {
        rendered.push_str(text);
        rendered.push_str("\n\n");
    }
    rendered.push_str("Attachments (stored references; contents are not embedded):");
    for attachment in attachments {
        rendered.push_str("\n- ");
        rendered.push_str(&attachment.name);
        if let Some(id) = attachment.id {
            rendered.push_str(" id=");
            rendered.push_str(&id.to_string());
        }
        if let Some(mime_type) = attachment.mime_type.as_deref() {
            rendered.push_str(" mime=");
            rendered.push_str(mime_type);
        }
        if let Some(size_bytes) = attachment.size_bytes {
            rendered.push_str(" bytes=");
            rendered.push_str(&size_bytes.to_string());
        }
        if let Some(url) = attachment.url.as_deref().filter(|url| !url.is_empty()) {
            rendered.push_str(" url=");
            rendered.push_str(url);
        }
    }
    rendered
}

/// Normalized inbound channel message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Source channel.
    pub channel: Channel,
    /// Provider-native message identifier.
    pub channel_msg_id: String,
    /// Message author.
    pub actor: ChannelActor,
    /// Conversation, thread, or destination reference.
    pub channel_ref: ChannelRef,
    /// Message text.
    pub text: String,
    /// Attached media or files.
    pub attachments: Vec<Attachment>,
    /// Optional message being replied to.
    pub reply_to: Option<String>,
    /// Event timestamp.
    pub timestamp: DateTime<Utc>,
}

string_id!(
    /// Identifier for a sent outbound channel message.
    pub struct MessageId
);

/// Button style for outbound channel actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ButtonStyle {
    /// Primary action.
    Primary,
    /// Destructive or dangerous action.
    Danger,
    /// Secondary action.
    Secondary,
}

/// Diff hunk for rendered channel output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// Starting line number in the old file.
    pub old_start: usize,
    /// Starting line number in the new file.
    pub new_start: usize,
    /// Unified diff lines.
    pub lines: Vec<String>,
}

/// Tool execution status for channel rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Tool execution is pending review or scheduling.
    Pending,
    /// Tool execution is in progress.
    Running,
    /// Tool execution succeeded.
    Succeeded,
    /// Tool execution failed.
    Failed,
}

/// Outbound message content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageContent {
    /// Plain text.
    Text(String),
    /// Markdown content.
    Markdown(String),
    /// Code block.
    CodeBlock { language: String, code: String },
    /// Diff content.
    Diff {
        filename: String,
        hunks: Vec<DiffHunk>,
    },
    /// Tool execution card.
    ToolCard {
        /// Tool name.
        tool: String,
        /// Tool status.
        status: ToolStatus,
        /// Concise summary.
        summary: String,
        /// Optional detailed output.
        detail: Option<String>,
    },
    /// Tenant-admin action-review request card.
    ActionReviewRequest {
        /// Durable policy-facing action envelope.
        envelope: Box<ActionEnvelope>,
        /// Human-readable review preview.
        preview: Box<ActionReviewPreview>,
    },
    /// Session status update.
    StatusUpdate {
        /// Session identifier.
        session_id: SessionId,
        /// Current status.
        status: SessionStatus,
        /// Human-readable summary.
        summary: String,
    },
}

/// Outbound button definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionButton {
    /// Stable button identifier.
    pub id: String,
    /// Button label.
    pub label: String,
    /// Button style.
    pub style: ButtonStyle,
    /// Channel callback payload.
    pub callback_data: String,
}

/// Normalized outbound message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// Renderable message content.
    pub content: MessageContent,
    /// Attached buttons.
    pub buttons: Vec<ActionButton>,
    /// Concrete channel route when the caller already resolved it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_ref: Option<ChannelRef>,
    /// Optional parent message identifier.
    pub reply_to: Option<String>,
    /// Whether the message is ephemeral.
    pub ephemeral: bool,
}

/// Channel transport capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCapabilities {
    /// Maximum message length.
    pub max_message_length: usize,
    /// Whether inline buttons are supported.
    pub supports_inline_buttons: bool,
    /// Whether modals are supported.
    pub supports_modals: bool,
    /// Whether ephemeral messages are supported.
    pub supports_ephemeral: bool,
    /// Whether threaded conversations are supported.
    pub supports_threads: bool,
    /// Whether code blocks are supported.
    pub supports_code_blocks: bool,
    /// Whether edit operations are supported.
    pub supports_edit: bool,
    /// Whether reactions are supported.
    pub supports_reactions: bool,
    /// Minimum edit interval.
    pub min_edit_interval: Duration,
}

#[cfg(test)]
mod tests {
    use super::{Channel, ChannelAccountId, ChannelRef};

    #[test]
    fn channel_serializes_as_snake_case() {
        // Pins: public API and database route labels use stable snake_case values.
        assert_eq!(Channel::Chat.as_str(), "chat");
        assert_eq!(
            serde_json::to_string(&Channel::Slack).expect("channel should serialize"),
            "\"slack\""
        );
        assert_eq!(Channel::Email.as_str(), "email");
    }

    #[test]
    fn channel_ref_reports_transport_without_losing_route_details() {
        // Pins: concrete destinations are separate from the transport enum.
        let account_id = ChannelAccountId::new();
        let sms = ChannelRef::Sms {
            channel_account_id: account_id,
        };
        let slack = ChannelRef::Slack {
            team_id: Some("T123".to_string()),
            slack_channel_id: Some("C123".to_string()),
            thread_ts: Some("1700000000.000100".to_string()),
            user_id: Some("U123".to_string()),
        };

        assert_eq!(sms.channel(), Channel::Sms);
        assert_eq!(slack.channel(), Channel::Slack);
    }
}
