//! Shared rendering helpers for messaging platform adapters.

use moa_core::{
    ActionButton, ApprovalRequest, MessageContent, OutboundMessage, Platform, SessionStatus,
    ToolStatus, types::DiffHunk,
};

#[cfg(feature = "slack")]
use moa_core::ButtonStyle;
#[cfg(feature = "slack")]
use slack_morphism::prelude::{
    SlackActionBlockElement, SlackActionId, SlackActionsBlock, SlackBlock, SlackBlockButtonElement,
    SlackBlockMarkDownText, SlackBlockPlainText, SlackBlockPlainTextOnly, SlackSectionBlock,
};

/// Telegram's documented hard cap for message text.
pub const TELEGRAM_MAX_MESSAGE_LENGTH: usize = 4096;

/// Slack's documented hard cap for one message text payload.
pub const SLACK_MAX_MESSAGE_LENGTH: usize = 40_000;

/// Discord's documented hard cap for plain message text.
pub const DISCORD_MAX_MESSAGE_LENGTH: usize = 2_000;

#[cfg(feature = "slack")]
const SLACK_MAX_BLOCK_TEXT_LENGTH: usize = 3_000;
#[cfg(feature = "discord")]
const DISCORD_MAX_EMBED_DESCRIPTION_LENGTH: usize = 4_096;

/// One Telegram-ready outbound text chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelegramRenderChunk {
    /// Text sent in one Telegram message.
    pub text: String,
    /// Inline buttons attached to the chunk.
    pub buttons: Vec<ActionButton>,
}

/// Platform-adaptive renderer for Telegram output.
#[derive(Debug, Default, Clone, Copy)]
pub struct TelegramRenderer;

impl TelegramRenderer {
    /// Creates a new Telegram renderer.
    pub fn new() -> Self {
        Self
    }

    /// Renders one normalized outbound message into Telegram-sized chunks.
    pub fn render(&self, message: &OutboundMessage) -> Vec<TelegramRenderChunk> {
        let mut chunks = match &message.content {
            MessageContent::Text(text) | MessageContent::Markdown(text) => {
                split_plain_text(text, TELEGRAM_MAX_MESSAGE_LENGTH)
            }
            MessageContent::CodeBlock { language, code } => {
                split_fenced_block(language, code, TELEGRAM_MAX_MESSAGE_LENGTH)
            }
            MessageContent::Diff { filename, hunks } => {
                let diff = render_diff(filename, hunks);
                split_fenced_block("diff", &diff, TELEGRAM_MAX_MESSAGE_LENGTH)
            }
            MessageContent::ToolCard {
                tool,
                status,
                summary,
                detail,
            } => split_plain_text(
                &render_tool_card(tool, status, summary, detail.as_deref()),
                TELEGRAM_MAX_MESSAGE_LENGTH,
            ),
            MessageContent::ApprovalRequest { request } => split_plain_text(
                &render_approval_request(request),
                TELEGRAM_MAX_MESSAGE_LENGTH,
            ),
            MessageContent::StatusUpdate {
                session_id,
                status,
                summary,
            } => split_plain_text(
                &format!(
                    "{} Session {}: {}",
                    session_status_icon(status),
                    session_id,
                    summary
                ),
                TELEGRAM_MAX_MESSAGE_LENGTH,
            ),
        };

        if chunks.is_empty() {
            chunks.push(String::new());
        }

        let chunk_count = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(index, text)| TelegramRenderChunk {
                text,
                buttons: if index + 1 == chunk_count {
                    message.buttons.clone()
                } else {
                    Vec::new()
                },
            })
            .collect()
    }

    /// Returns Telegram's platform capabilities.
    pub fn capabilities(&self) -> moa_core::PlatformCapabilities {
        moa_core::PlatformCapabilities {
            max_message_length: TELEGRAM_MAX_MESSAGE_LENGTH,
            supports_inline_buttons: true,
            supports_modals: false,
            supports_ephemeral: false,
            supports_threads: true,
            supports_code_blocks: true,
            supports_edit: true,
            supports_reactions: false,
            min_edit_interval: std::time::Duration::from_secs(2),
        }
    }

    /// Returns the platform handled by this renderer.
    pub fn platform(&self) -> Platform {
        Platform::Telegram
    }
}

/// One Slack-ready outbound chunk.
#[cfg(feature = "slack")]
#[derive(Debug, Clone, PartialEq)]
pub struct SlackRenderChunk {
    /// Fallback text used for notifications and accessibility.
    pub text: String,
    /// Optional Block Kit payload.
    pub blocks: Option<Vec<SlackBlock>>,
}

/// Platform-adaptive renderer for Slack output.
#[cfg(feature = "slack")]
#[derive(Debug, Default, Clone, Copy)]
pub struct SlackRenderer;

#[cfg(feature = "slack")]
impl SlackRenderer {
    /// Creates a new Slack renderer.
    pub fn new() -> Self {
        Self
    }

    /// Renders one normalized outbound message into Slack-sized chunks.
    pub fn render(&self, message: &OutboundMessage) -> Vec<SlackRenderChunk> {
        let text = match &message.content {
            MessageContent::Text(text) | MessageContent::Markdown(text) => text.clone(),
            MessageContent::CodeBlock { language, code } => {
                if language.is_empty() {
                    format!("```\n{code}\n```")
                } else {
                    format!("```{language}\n{code}\n```")
                }
            }
            MessageContent::Diff { filename, hunks } => {
                let diff = render_diff(filename, hunks);
                format!("```diff\n{diff}\n```")
            }
            MessageContent::ToolCard {
                tool,
                status,
                summary,
                detail,
            } => render_tool_card(tool, status, summary, detail.as_deref()),
            MessageContent::ApprovalRequest { request } => render_approval_request(request),
            MessageContent::StatusUpdate {
                session_id,
                status,
                summary,
            } => format!(
                "{} Session {}: {}",
                session_status_icon(status),
                session_id,
                summary
            ),
        };

        let limit = if message.buttons.is_empty() {
            SLACK_MAX_MESSAGE_LENGTH
        } else {
            SLACK_MAX_BLOCK_TEXT_LENGTH
        };
        let chunks = split_plain_text(&text, limit);
        let chunk_count = chunks.len();

        chunks
            .into_iter()
            .enumerate()
            .map(|(index, text)| SlackRenderChunk {
                blocks: slack_blocks_for_chunk(
                    &text,
                    if index + 1 == chunk_count {
                        &message.buttons
                    } else {
                        &[]
                    },
                ),
                text,
            })
            .collect()
    }

    /// Returns Slack's platform capabilities.
    pub fn capabilities(&self) -> moa_core::PlatformCapabilities {
        moa_core::PlatformCapabilities {
            max_message_length: SLACK_MAX_MESSAGE_LENGTH,
            supports_inline_buttons: true,
            supports_modals: true,
            supports_ephemeral: true,
            supports_threads: true,
            supports_code_blocks: true,
            supports_edit: true,
            supports_reactions: true,
            min_edit_interval: std::time::Duration::from_secs(1),
        }
    }

    /// Returns the platform handled by this renderer.
    pub fn platform(&self) -> Platform {
        Platform::Slack
    }
}

/// One Discord-ready outbound chunk.
#[cfg(feature = "discord")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscordRenderChunk {
    /// Optional plain-text content sent alongside the embed.
    pub content: Option<String>,
    /// Optional embed title.
    pub embed_title: Option<String>,
    /// Optional embed description.
    pub embed_description: Option<String>,
    /// Optional embed accent color.
    pub embed_color: Option<u32>,
    /// Platform-neutral buttons attached to the last chunk only.
    pub buttons: Vec<ActionButton>,
}

/// Platform-adaptive renderer for Discord output.
#[cfg(feature = "discord")]
#[derive(Debug, Default, Clone, Copy)]
pub struct DiscordRenderer;

#[cfg(feature = "discord")]
impl DiscordRenderer {
    /// Creates a new Discord renderer.
    pub fn new() -> Self {
        Self
    }

    /// Renders one normalized outbound message into Discord-sized chunks.
    pub fn render(&self, message: &OutboundMessage) -> Vec<DiscordRenderChunk> {
        let (title, body, color) = match &message.content {
            MessageContent::Text(text) | MessageContent::Markdown(text) => {
                (None, text.clone(), None)
            }
            MessageContent::CodeBlock { language, code } => (
                Some("Code".to_string()),
                if language.is_empty() {
                    format!("```\n{code}\n```")
                } else {
                    format!("```{language}\n{code}\n```")
                },
                None,
            ),
            MessageContent::Diff { filename, hunks } => (
                Some(filename.clone()),
                format!("```diff\n{}\n```", render_diff(filename, hunks)),
                Some(0xF59E0B),
            ),
            MessageContent::ToolCard {
                tool,
                status,
                summary,
                detail,
            } => (
                Some(format!("{} {tool}", tool_status_icon(status))),
                render_tool_card(tool, status, summary, detail.as_deref()),
                Some(tool_status_color(status)),
            ),
            MessageContent::ApprovalRequest { request } => (
                Some(format!(
                    "{} Approval required: {}",
                    risk_icon(&request.risk_level),
                    request.tool_name
                )),
                render_approval_request(request),
                Some(risk_color(&request.risk_level)),
            ),
            MessageContent::StatusUpdate {
                session_id,
                status,
                summary,
            } => (
                Some(format!(
                    "{} Session {}",
                    session_status_icon(status),
                    session_id
                )),
                summary.clone(),
                Some(session_status_color(status)),
            ),
        };

        let mut chunks = split_plain_text(&body, DISCORD_MAX_EMBED_DESCRIPTION_LENGTH);
        if chunks.is_empty() {
            chunks.push(String::new());
        }

        let chunk_count = chunks.len();
        chunks
            .into_iter()
            .enumerate()
            .map(|(index, embed_description)| DiscordRenderChunk {
                content: None,
                embed_title: if index == 0 { title.clone() } else { None },
                embed_description: Some(embed_description),
                embed_color: color,
                buttons: if index + 1 == chunk_count {
                    message.buttons.clone()
                } else {
                    Vec::new()
                },
            })
            .collect()
    }

    /// Returns Discord's platform capabilities.
    pub fn capabilities(&self) -> moa_core::PlatformCapabilities {
        moa_core::PlatformCapabilities {
            max_message_length: DISCORD_MAX_MESSAGE_LENGTH,
            supports_inline_buttons: true,
            supports_modals: true,
            supports_ephemeral: true,
            supports_threads: true,
            supports_code_blocks: true,
            supports_edit: true,
            supports_reactions: true,
            min_edit_interval: std::time::Duration::from_secs(2),
        }
    }

    /// Returns the platform handled by this renderer.
    pub fn platform(&self) -> Platform {
        Platform::Discord
    }
}

fn render_diff(filename: &str, hunks: &[DiffHunk]) -> String {
    let mut rendered = format!("--- a/{filename}\n+++ b/{filename}\n");
    for hunk in hunks {
        rendered.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            hunk.old_start,
            hunk.lines.len(),
            hunk.new_start,
            hunk.lines.len()
        ));
        for line in &hunk.lines {
            rendered.push_str(line);
            if !line.ends_with('\n') {
                rendered.push('\n');
            }
        }
    }
    rendered
}

fn render_tool_card(
    tool: &str,
    status: &ToolStatus,
    summary: &str,
    detail: Option<&str>,
) -> String {
    let mut text = format!("{} {tool}\n{summary}", tool_status_icon(status));
    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        text.push_str("\n\n");
        text.push_str(detail);
    }
    text
}

fn render_approval_request(request: &ApprovalRequest) -> String {
    format!(
        "{} Approval required: {}\n{}\nRequest: {}",
        risk_icon(&request.risk_level),
        request.tool_name,
        request.input_summary,
        request.request_id
    )
}

fn tool_status_icon(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "🕒",
        ToolStatus::Running => "🔄",
        ToolStatus::Succeeded => "✅",
        ToolStatus::Failed => "❌",
    }
}

#[cfg(feature = "discord")]
fn tool_status_color(status: &ToolStatus) -> u32 {
    match status {
        ToolStatus::Pending => 0x94A3B8,
        ToolStatus::Running => 0x2563EB,
        ToolStatus::Succeeded => 0x16A34A,
        ToolStatus::Failed => 0xDC2626,
    }
}

fn session_status_icon(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Created => "🆕",
        SessionStatus::Running => "🔄",
        SessionStatus::Paused => "⏸",
        SessionStatus::WaitingApproval => "🟡",
        SessionStatus::Completed => "✅",
        SessionStatus::Cancelled => "⏹",
        SessionStatus::Failed => "❌",
    }
}

#[cfg(feature = "discord")]
fn session_status_color(status: &SessionStatus) -> u32 {
    match status {
        SessionStatus::Created => 0x6366F1,
        SessionStatus::Running => 0x2563EB,
        SessionStatus::Paused => 0x94A3B8,
        SessionStatus::WaitingApproval => 0xF59E0B,
        SessionStatus::Completed => 0x16A34A,
        SessionStatus::Cancelled => 0x6B7280,
        SessionStatus::Failed => 0xDC2626,
    }
}

fn risk_icon(risk_level: &moa_core::RiskLevel) -> &'static str {
    match risk_level {
        moa_core::RiskLevel::Low => "🟢",
        moa_core::RiskLevel::Medium => "🟡",
        moa_core::RiskLevel::High => "🔴",
    }
}

#[cfg(feature = "discord")]
fn risk_color(risk_level: &moa_core::RiskLevel) -> u32 {
    match risk_level {
        moa_core::RiskLevel::Low => 0x16A34A,
        moa_core::RiskLevel::Medium => 0xF59E0B,
        moa_core::RiskLevel::High => 0xDC2626,
    }
}

fn split_fenced_block(language: &str, body: &str, limit: usize) -> Vec<String> {
    let prefix = if language.is_empty() {
        "```\n".to_string()
    } else {
        format!("```{language}\n")
    };
    let suffix = "\n```";
    let overhead = prefix.chars().count() + suffix.chars().count();
    if overhead >= limit {
        return vec![prefix + suffix];
    }

    split_plain_text(body, limit - overhead)
        .into_iter()
        .map(|chunk| format!("{prefix}{chunk}{suffix}"))
        .collect()
}

fn split_plain_text(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if text.chars().count() <= limit {
        return vec![text.to_string()];
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    for piece in text.split_inclusive('\n') {
        append_piece(piece, limit, &mut current, &mut chunks);
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    if chunks.is_empty() {
        chunks.push(text.chars().take(limit).collect());
    }
    chunks
}

fn append_piece(piece: &str, limit: usize, current: &mut String, chunks: &mut Vec<String>) {
    let piece_len = piece.chars().count();
    if piece_len > limit {
        for fragment in split_hard(piece, limit) {
            append_piece(&fragment, limit, current, chunks);
        }
        return;
    }

    let current_len = current.chars().count();
    if current_len == 0 || current_len + piece_len <= limit {
        current.push_str(piece);
        return;
    }

    chunks.push(std::mem::take(current));
    current.push_str(piece);
}

fn split_hard(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if current.chars().count() == limit {
            parts.push(std::mem::take(&mut current));
        }
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(feature = "slack")]
fn slack_blocks_for_chunk(text: &str, buttons: &[ActionButton]) -> Option<Vec<SlackBlock>> {
    if buttons.is_empty() {
        return None;
    }

    let section = SlackSectionBlock {
        block_id: None,
        text: Some(
            SlackBlockMarkDownText {
                text: text.to_string(),
                verbatim: None,
            }
            .as_block_text(),
        ),
        fields: None,
        accessory: None,
    };
    let actions = SlackActionsBlock {
        block_id: None,
        elements: buttons
            .iter()
            .map(slack_button)
            .map(SlackActionBlockElement::from)
            .collect(),
    };
    Some(vec![section.into(), actions.into()])
}

#[cfg(feature = "slack")]
fn slack_button(button: &ActionButton) -> SlackBlockButtonElement {
    let style = match button.style {
        ButtonStyle::Primary => Some("primary".to_string()),
        ButtonStyle::Danger => Some("danger".to_string()),
        ButtonStyle::Secondary => None,
    };

    SlackBlockButtonElement {
        action_id: SlackActionId(button.id.clone()),
        text: SlackBlockPlainTextOnly::from(SlackBlockPlainText::from(button.label.as_str())),
        url: None,
        value: Some(button.callback_data.clone()),
        style,
        confirm: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::approval_buttons;
    use moa_core::RiskLevel;

    #[test]
    fn renderer_splits_long_text_at_telegram_limit() {
        let text = "a".repeat(TELEGRAM_MAX_MESSAGE_LENGTH + 50);
        let message = OutboundMessage {
            content: MessageContent::Text(text.clone()),
            buttons: Vec::new(),
            reply_to: Some("42".to_string()),
            ephemeral: false,
        };

        let chunks = TelegramRenderer::new().render(&message);
        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.text.chars().count() <= TELEGRAM_MAX_MESSAGE_LENGTH)
        );
        assert_eq!(
            chunks
                .into_iter()
                .map(|chunk| chunk.text)
                .collect::<String>(),
            text
        );
    }

    #[test]
    fn renderer_keeps_buttons_on_last_chunk_only() {
        let request_id = uuid::Uuid::new_v4();
        let message = OutboundMessage {
            content: MessageContent::ApprovalRequest {
                request: ApprovalRequest {
                    request_id,
                    tool_name: "bash".to_string(),
                    input_summary: "npm test".to_string(),
                    risk_level: RiskLevel::High,
                },
            },
            buttons: approval_buttons(Platform::Telegram, request_id),
            reply_to: Some("42".to_string()),
            ephemeral: false,
        };

        let chunks = TelegramRenderer::new().render(&message);
        assert!(!chunks.is_empty());
        for chunk in &chunks[..chunks.len().saturating_sub(1)] {
            assert!(chunk.buttons.is_empty());
        }
        assert_eq!(chunks.last().map(|chunk| chunk.buttons.len()), Some(3));
    }

    #[cfg(feature = "slack")]
    #[test]
    fn slack_renderer_splits_long_text_at_slack_limit() {
        let text = "a".repeat(SLACK_MAX_MESSAGE_LENGTH + 50);
        let message = OutboundMessage {
            content: MessageContent::Text(text.clone()),
            buttons: Vec::new(),
            reply_to: Some("123".to_string()),
            ephemeral: false,
        };

        let chunks = SlackRenderer::new().render(&message);
        assert!(chunks.len() >= 2);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.text.chars().count() <= SLACK_MAX_MESSAGE_LENGTH)
        );
        assert_eq!(
            chunks
                .into_iter()
                .map(|chunk| chunk.text)
                .collect::<String>(),
            text
        );
    }

    #[cfg(feature = "slack")]
    #[test]
    fn slack_renderer_attaches_buttons_to_last_chunk_only() {
        let request_id = uuid::Uuid::new_v4();
        let message = OutboundMessage {
            content: MessageContent::ApprovalRequest {
                request: ApprovalRequest {
                    request_id,
                    tool_name: "bash".to_string(),
                    input_summary: "npm test".to_string(),
                    risk_level: RiskLevel::High,
                },
            },
            buttons: approval_buttons(Platform::Slack, request_id),
            reply_to: Some("123".to_string()),
            ephemeral: false,
        };

        let chunks = SlackRenderer::new().render(&message);
        assert!(!chunks.is_empty());
        for chunk in &chunks[..chunks.len().saturating_sub(1)] {
            assert!(chunk.blocks.is_none());
        }
        assert!(
            chunks
                .last()
                .and_then(|chunk| chunk.blocks.as_ref())
                .is_some()
        );
    }

    #[cfg(feature = "discord")]
    #[test]
    fn discord_renderer_uses_embed_limit_for_long_text() {
        let text = "a".repeat(DISCORD_MAX_EMBED_DESCRIPTION_LENGTH + 50);
        let message = OutboundMessage {
            content: MessageContent::Text(text.clone()),
            buttons: Vec::new(),
            reply_to: Some("321".to_string()),
            ephemeral: false,
        };

        let chunks = DiscordRenderer::new().render(&message);
        assert!(chunks.len() >= 2);
        assert!(chunks.iter().all(|chunk| {
            chunk
                .embed_description
                .as_ref()
                .is_some_and(|text| text.chars().count() <= DISCORD_MAX_EMBED_DESCRIPTION_LENGTH)
        }));
        assert_eq!(
            chunks
                .into_iter()
                .filter_map(|chunk| chunk.embed_description)
                .collect::<String>(),
            text
        );
    }

    #[cfg(feature = "discord")]
    #[test]
    fn discord_renderer_attaches_buttons_to_last_chunk_only() {
        let request_id = uuid::Uuid::new_v4();
        let message = OutboundMessage {
            content: MessageContent::ApprovalRequest {
                request: ApprovalRequest {
                    request_id,
                    tool_name: "file_write".to_string(),
                    input_summary: "update src/lib.rs".to_string(),
                    risk_level: RiskLevel::Medium,
                },
            },
            buttons: approval_buttons(Platform::Discord, request_id),
            reply_to: Some("321".to_string()),
            ephemeral: false,
        };

        let chunks = DiscordRenderer::new().render(&message);
        assert!(!chunks.is_empty());
        for chunk in &chunks[..chunks.len().saturating_sub(1)] {
            assert!(chunk.buttons.is_empty());
        }
        assert_eq!(chunks.last().map(|chunk| chunk.buttons.len()), Some(3));
    }
}
