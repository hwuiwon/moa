//! Out-of-line tests for Slack messaging approval button rendering.

mod support;

use moa_core::{ApprovalDecision, MessageContent, OutboundMessage, Platform};
use moa_messaging::{
    ApprovalCallbackAction, SlackRenderer, approval_buttons, resolved_approval_buttons,
};
use support::{approval_request, fixed_request_id};

#[test]
fn slack_approval_buttons_render_as_block_kit_actions_with_primary_and_danger_styles() {
    let request_id = fixed_request_id();
    let message = approval_outbound(Platform::Slack);
    let chunks = SlackRenderer::new().render(&message);
    let blocks = chunks[0]
        .blocks
        .as_ref()
        .expect("slack approval render should include Block Kit");
    let value = serde_json::to_value(blocks).expect("slack blocks should serialize");
    let block_values = value
        .as_array()
        .expect("slack Block Kit payload should serialize as an array");
    assert_eq!(block_values.len(), 2);
    assert_eq!(block_values[0]["type"], "section");
    assert_eq!(block_values[0]["text"]["type"], "mrkdwn");
    assert_eq!(
        block_values[0]["text"]["text"],
        format!("🔴 Approval required: bash\nnpm test\nRequest: {request_id}")
    );

    let actions = block_values
        .iter()
        .find(|block| block["type"] == "actions")
        .expect("slack blocks should include an actions block");
    let buttons = actions["elements"]
        .as_array()
        .expect("slack actions block should contain buttons");

    assert_eq!(buttons.len(), 3);
    assert_eq!(buttons[0]["type"], "button");
    assert_eq!(buttons[0]["action_id"], "allow");
    assert_eq!(buttons[0]["text"]["text"], "Allow");
    assert_eq!(buttons[0]["text"]["type"], "plain_text");
    assert_eq!(buttons[0]["value"], format!("ap:o:{request_id}"));
    assert_eq!(buttons[0]["style"], "primary");
    assert_eq!(buttons[1]["type"], "button");
    assert_eq!(buttons[1]["action_id"], "always");
    assert_eq!(buttons[1]["text"]["text"], "Always");
    assert_eq!(buttons[1]["text"]["type"], "plain_text");
    assert_eq!(buttons[1]["value"], format!("ap:a:{request_id}"));
    assert!(buttons[1].get("style").is_none());
    assert_eq!(buttons[2]["type"], "button");
    assert_eq!(buttons[2]["action_id"], "deny");
    assert_eq!(buttons[2]["text"]["text"], "Deny");
    assert_eq!(buttons[2]["text"]["type"], "plain_text");
    assert_eq!(buttons[2]["value"], format!("ap:d:{request_id}"));
    assert_eq!(buttons[2]["style"], "danger");
}

#[test]
fn approval_button_callback_data_round_trips_through_slack_button_payloads() {
    let request_id = fixed_request_id();
    let buttons = approval_buttons(Platform::Slack, request_id);

    let decoded = buttons
        .iter()
        .map(|button| ApprovalCallbackAction::decode(&button.callback_data))
        .collect::<Vec<_>>();
    assert_eq!(
        decoded,
        vec![
            Some(ApprovalCallbackAction::AllowOnce { request_id }),
            Some(ApprovalCallbackAction::AlwaysAllow { request_id }),
            Some(ApprovalCallbackAction::Deny { request_id }),
        ]
    );
}

#[test]
fn approval_buttons_are_removed_after_decision_for_slack() {
    let request_id = fixed_request_id();
    let decision = ApprovalDecision::AllowOnce;

    let slack_buttons =
        resolved_approval_buttons(Platform::Slack, request_id, &decision, "@test-user");
    let mut slack_message = approval_outbound(Platform::Slack);
    slack_message.buttons = slack_buttons;
    let slack_rendered = SlackRenderer::new().render(&slack_message);
    assert!(
        slack_rendered[0].blocks.is_none(),
        "Slack should remove approval action buttons after a decision"
    );
}

fn approval_outbound(platform: Platform) -> OutboundMessage {
    let request = approval_request();
    OutboundMessage {
        content: MessageContent::ApprovalRequest {
            request: request.clone(),
        },
        buttons: approval_buttons(platform, request.request_id),
        reply_to: Some("reply-001".to_string()),
        ephemeral: false,
    }
}
