//! Shared rendering helpers for messaging channel adapters.

#[cfg(feature = "slack")]
use moa_core::{
    types::channel::Channel, types::channel::DiffHunk, types::channel::MessageContent,
    types::channel::OutboundMessage, types::channel::ToolStatus, types::session::SessionStatus,
};
#[cfg(feature = "slack")]
use unicode_segmentation::UnicodeSegmentation;

/// Slack's documented hard cap for one message text payload.
pub const SLACK_MAX_MESSAGE_LENGTH: usize = 40_000;

/// One Slack-ready outbound chunk.
#[cfg(feature = "slack")]
#[derive(Debug, Clone, PartialEq)]
pub struct SlackRenderChunk {
    /// Text sent to Slack.
    pub text: String,
}

/// Channel-adaptive renderer for Slack output.
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
            MessageContent::ActionReviewRequest { envelope, preview } => {
                render_action_review_request(envelope, preview)
            }
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

        split_plain_text(&text, SLACK_MAX_MESSAGE_LENGTH)
            .into_iter()
            .map(|text| SlackRenderChunk { text })
            .collect()
    }

    /// Returns Slack's channel capabilities.
    pub fn capabilities(&self) -> moa_core::types::channel::ChannelCapabilities {
        moa_core::types::channel::ChannelCapabilities {
            max_message_length: SLACK_MAX_MESSAGE_LENGTH,
            supports_ephemeral: true,
            supports_threads: true,
            supports_code_blocks: true,
            supports_edit: true,
            supports_reactions: true,
            min_edit_interval: std::time::Duration::from_secs(1),
        }
    }

    /// Returns the channel handled by this renderer.
    pub fn channel(&self) -> Channel {
        Channel::Slack
    }
}

#[cfg(feature = "slack")]
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

#[cfg(feature = "slack")]
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

pub(crate) fn render_action_review_request(
    envelope: &moa_core::types::action_policy::ActionEnvelope,
    preview: &moa_core::types::action_policy::ActionReviewPreview,
) -> String {
    let mut rendered = format!(
        "{} Action review requested: {}\n{}\nReview: {}",
        risk_icon(&envelope.risk_level),
        envelope.tool_name,
        envelope.input_summary,
        envelope.review_id
    );
    for field in &preview.fields {
        rendered.push('\n');
        rendered.push_str(&field.label);
        rendered.push_str(": ");
        rendered.push_str(&field.value);
    }
    rendered
}

fn risk_icon(risk_level: &moa_core::types::action_policy::RiskLevel) -> &'static str {
    match risk_level {
        moa_core::types::action_policy::RiskLevel::Low => "🟢",
        moa_core::types::action_policy::RiskLevel::Medium => "🟡",
        moa_core::types::action_policy::RiskLevel::High => "🔴",
    }
}

#[cfg(feature = "slack")]
fn tool_status_icon(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "🕒",
        ToolStatus::Running => "🔄",
        ToolStatus::Succeeded => "✅",
        ToolStatus::Failed => "❌",
    }
}

#[cfg(feature = "slack")]
fn session_status_icon(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Created => "🆕",
        SessionStatus::Running => "🔄",
        SessionStatus::Paused => "⏸",
        SessionStatus::Completed => "✅",
        SessionStatus::Cancelled => "⏹",
        SessionStatus::Failed => "❌",
    }
}

#[cfg(feature = "slack")]
fn split_plain_text(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    if text.graphemes(true).count() <= limit {
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

#[cfg(feature = "slack")]
fn append_piece(piece: &str, limit: usize, current: &mut String, chunks: &mut Vec<String>) {
    let piece_len = piece.graphemes(true).count();
    if piece_len > limit {
        for fragment in split_hard(piece, limit) {
            append_piece(&fragment, limit, current, chunks);
        }
        return;
    }

    let current_len = current.graphemes(true).count();
    if current_len == 0 || current_len + piece_len <= limit {
        current.push_str(piece);
        return;
    }

    chunks.push(std::mem::take(current));
    current.push_str(piece);
}

#[cfg(feature = "slack")]
fn split_hard(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut current_len = 0;
    for grapheme in text.graphemes(true) {
        if current_len == limit {
            parts.push(std::mem::take(&mut current));
            current_len = 0;
        }
        current.push_str(grapheme);
        current_len += 1;
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "slack")]
    use super::*;
    #[cfg(feature = "slack")]
    use moa_core::{
        types::action_policy::ActionClass, types::action_policy::ActionEnvelope,
        types::action_policy::ActionReviewField, types::action_policy::ActionReviewPreview,
        types::action_policy::RiskLevel, types::contact::SessionActorRef,
        types::identifiers::SessionId, types::identifiers::TenantId,
        types::identifiers::ToolCallId, types::session::SessionStatus,
    };

    #[cfg(feature = "slack")]
    #[test]
    fn slack_renderer_splits_long_text_at_slack_limit() {
        let text = "a".repeat(SLACK_MAX_MESSAGE_LENGTH + 50);
        let message = OutboundMessage {
            content: MessageContent::Text(text.clone()),
            channel_ref: None,
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
    fn slack_renderer_renders_status_update_as_single_line() {
        // Pins: durable progress bridges render through MessageContent::StatusUpdate.
        let session_id = SessionId::new();
        let message = OutboundMessage {
            content: MessageContent::StatusUpdate {
                session_id,
                status: SessionStatus::Running,
                summary: "Calling the model".to_string(),
            },
            channel_ref: None,
            reply_to: None,
            ephemeral: false,
        };

        let chunks = SlackRenderer::new().render(&message);

        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].text,
            format!("🔄 Session {session_id}: Calling the model")
        );
    }

    #[cfg(feature = "slack")]
    #[test]
    fn slack_renderer_renders_action_review_as_text() {
        let message = OutboundMessage {
            content: MessageContent::ActionReviewRequest {
                envelope: Box::new(ActionEnvelope {
                    review_id: uuid::Uuid::now_v7(),
                    tenant_id: TenantId::from(
                        uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111")
                            .expect("fixture tenant id parses"),
                    ),
                    requested_by: SessionActorRef::Identity {
                        id: uuid::Uuid::parse_str("22222222-2222-2222-2222-222222222222")
                            .expect("fixture identity id parses"),
                    },
                    session_id: None,
                    worker_id: None,
                    tool_call_id: ToolCallId::new(),
                    tool_name: "bash".to_string(),
                    normalized_input: "npm test".to_string(),
                    input_summary: "npm test".to_string(),
                    risk_level: RiskLevel::High,
                    action_class: ActionClass::CommandExecution,
                    origin_kind: None,
                    origin_id: None,
                    origin_step_id: None,
                    idempotency_key: None,
                    created_at: chrono::Utc::now(),
                }),
                preview: Box::new(ActionReviewPreview {
                    fields: vec![ActionReviewField {
                        label: "Command".to_string(),
                        value: "npm test".to_string(),
                    }],
                    file_diffs: Vec::new(),
                }),
            },
            channel_ref: None,
            reply_to: Some("123".to_string()),
            ephemeral: false,
        };

        let chunks = SlackRenderer::new().render(&message);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("Action review requested"));
        assert!(chunks[0].text.contains("npm test"));
    }
}
