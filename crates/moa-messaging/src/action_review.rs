//! Unified action-review rendering across channel adapters.

use moa_core::{Channel, ChannelCapabilities, MessageContent, OutboundMessage};

use crate::renderer::render_action_review_request;

/// Adds channel-native action-review affordances to an outbound message when possible.
pub fn prepare_outbound_message(
    _channel: Channel,
    capabilities: &ChannelCapabilities,
    mut message: OutboundMessage,
) -> OutboundMessage {
    let MessageContent::ActionReviewRequest { envelope, preview } = &message.content else {
        return message;
    };

    if !capabilities.supports_inline_buttons {
        message.content = MessageContent::Markdown(render_action_review_request(envelope, preview));
    }

    message
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        ActionClass, ActionEnvelope, ActionReviewField, ActionReviewPreview, Channel,
        ChannelCapabilities, MessageContent, OutboundMessage, RiskLevel, SessionActorRef, TenantId,
        ToolCallId,
    };
    use uuid::Uuid;

    use super::prepare_outbound_message;

    fn review_message() -> OutboundMessage {
        OutboundMessage {
            content: MessageContent::ActionReviewRequest {
                envelope: Box::new(ActionEnvelope {
                    review_id: Uuid::now_v7(),
                    tenant_id: TenantId::from(
                        Uuid::parse_str("11111111-1111-1111-1111-111111111111")
                            .expect("fixture tenant id parses"),
                    ),
                    requested_by: SessionActorRef::Identity {
                        id: Uuid::parse_str("22222222-2222-2222-2222-222222222222")
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
                    created_at: Utc::now(),
                }),
                preview: Box::new(ActionReviewPreview {
                    fields: vec![ActionReviewField {
                        label: "Command".to_string(),
                        value: "npm test".to_string(),
                    }],
                    file_diffs: Vec::new(),
                }),
            },
            buttons: Vec::new(),
            channel_ref: None,
            reply_to: Some("42".to_string()),
            ephemeral: false,
        }
    }

    fn capabilities(supports_inline_buttons: bool) -> ChannelCapabilities {
        ChannelCapabilities {
            max_message_length: 2_000,
            supports_inline_buttons,
            supports_modals: supports_inline_buttons,
            supports_ephemeral: supports_inline_buttons,
            supports_threads: true,
            supports_code_blocks: true,
            supports_edit: supports_inline_buttons,
            supports_reactions: false,
            min_edit_interval: std::time::Duration::from_secs(0),
        }
    }

    #[test]
    fn prepare_outbound_message_keeps_review_card_when_buttons_are_available() {
        let prepared =
            prepare_outbound_message(Channel::Slack, &capabilities(true), review_message());

        assert!(matches!(
            prepared.content,
            MessageContent::ActionReviewRequest { .. }
        ));
        assert!(prepared.buttons.is_empty());
    }

    #[test]
    fn prepare_outbound_message_degrades_review_card_to_text() {
        let prepared =
            prepare_outbound_message(Channel::Chat, &capabilities(false), review_message());

        match prepared.content {
            MessageContent::Markdown(text) => {
                assert!(text.contains("Action review requested"));
                assert!(text.contains("npm test"));
            }
            other => panic!("expected markdown fallback, got {other:?}"),
        }
    }
}
