//! Platform transport and message rendering types.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ApprovalRequest, SessionId, SessionStatus, UserId};

/// Platform a session or message originated from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    /// Telegram Bot API.
    Telegram,
    /// Slack.
    Slack,
    /// Discord.
    Discord,
    /// One-shot CLI.
    Cli,
}

impl Platform {
    /// Returns the canonical lowercase string label for this platform variant.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Telegram => "telegram",
            Self::Slack => "slack",
            Self::Discord => "discord",
            Self::Cli => "cli",
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Platform-specific user identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformUser {
    /// Platform-native identifier.
    pub platform_id: String,
    /// Display name.
    pub display_name: String,
    /// Linked MOA user identifier, when known.
    pub moa_user_id: Option<UserId>,
}

/// Normalized inbound channel reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelRef {
    /// Direct message.
    DirectMessage { user_id: String },
    /// Group channel.
    Group { channel_id: String },
    /// Thread within a channel.
    Thread {
        channel_id: String,
        thread_id: String,
    },
}

/// File or rich attachment metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Attachment display name.
    pub name: String,
    /// MIME type when known.
    pub mime_type: Option<String>,
    /// Remote URL when applicable.
    pub url: Option<String>,
    /// Local filesystem path when applicable.
    pub path: Option<PathBuf>,
    /// Attachment size in bytes when known.
    pub size_bytes: Option<u64>,
}

/// Normalized inbound platform message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    /// Source platform.
    pub platform: Platform,
    /// Platform-native message identifier.
    pub platform_msg_id: String,
    /// Message author.
    pub user: PlatformUser,
    /// Channel or thread reference.
    pub channel: ChannelRef,
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
    /// Identifier for a sent outbound platform message.
    pub struct MessageId
);

/// Button style for outbound platform actions.
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

/// Diff hunk for rendered platform output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    /// Starting line number in the old file.
    pub old_start: usize,
    /// Starting line number in the new file.
    pub new_start: usize,
    /// Unified diff lines.
    pub lines: Vec<String>,
}

/// Tool execution status for platform rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// Tool execution is pending approval or scheduling.
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
    /// Approval request card.
    ApprovalRequest { request: ApprovalRequest },
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
    /// Platform callback payload.
    pub callback_data: String,
}

/// Normalized outbound message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    /// Renderable message content.
    pub content: MessageContent,
    /// Attached buttons.
    pub buttons: Vec<ActionButton>,
    /// Optional parent message identifier.
    pub reply_to: Option<String>,
    /// Whether the message is ephemeral.
    pub ephemeral: bool,
}

/// Platform transport capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformCapabilities {
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
