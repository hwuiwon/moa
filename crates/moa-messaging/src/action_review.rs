//! Unified action-review rendering across channel adapters.

use moa_core::{types::channel::MessageContent, types::channel::OutboundMessage};

use crate::renderer::render_action_review_request;

/// Converts structured action-review requests into text-only outbound content.
pub fn prepare_outbound_message(mut message: OutboundMessage) -> OutboundMessage {
    let MessageContent::ActionReviewRequest { envelope, preview } = &message.content else {
        return message;
    };

    message.content = MessageContent::Markdown(render_action_review_request(envelope, preview));

    message
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{
        types::action_policy::ActionClass, types::action_policy::ActionEnvelope,
        types::action_policy::ActionReviewField, types::action_policy::ActionReviewPreview,
        types::action_policy::RiskLevel, types::channel::MessageContent,
        types::channel::OutboundMessage, types::contact::SessionActorRef,
        types::identifiers::TenantId, types::identifiers::ToolCallId,
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
            channel_ref: None,
            reply_to: Some("42".to_string()),
            ephemeral: false,
        }
    }

    #[test]
    fn prepare_outbound_message_degrades_review_card_to_text() {
        let prepared = prepare_outbound_message(review_message());

        match prepared.content {
            MessageContent::Markdown(text) => {
                assert!(text.contains("Action review requested"));
                assert!(text.contains("npm test"));
            }
            other => panic!("expected markdown fallback, got {other:?}"),
        }
    }

    #[test]
    fn prepare_outbound_message_preserves_non_action_content() {
        let message = OutboundMessage {
            content: MessageContent::Text("hello".to_string()),
            channel_ref: None,
            reply_to: None,
            ephemeral: false,
        };

        assert_eq!(prepare_outbound_message(message.clone()), message);
    }
}
