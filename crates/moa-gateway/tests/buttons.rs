//! Out-of-line tests for gateway approval button rendering.

mod support;

use moa_core::{ApprovalDecision, MessageContent, OutboundMessage, Platform};
use moa_gateway::{
    ApprovalCallbackAction, DiscordRenderer, SlackRenderer, approval_buttons, discord,
    resolved_approval_buttons, telegram,
};
use support::{approval_request, fixed_request_id};

#[test]
fn telegram_approval_buttons_render_with_three_choices_and_correct_callback_data() {
    let request_id = fixed_request_id();
    let buttons = approval_buttons(Platform::Telegram, request_id);
    let markup = telegram::render_inline_keyboard(&buttons).expect("telegram buttons render");
    let value = serde_json::to_value(markup).expect("telegram markup should serialize");
    let row = value["inline_keyboard"][0]
        .as_array()
        .expect("telegram inline keyboard should contain one button row");

    assert_eq!(row.len(), 3);
    assert_eq!(row[0]["text"], "✅ Allow");
    assert_eq!(row[1]["text"], "🔁 Always");
    assert_eq!(row[2]["text"], "❌ Deny");
    assert_eq!(row[0]["callback_data"], format!("ap:o:{request_id}"));
    assert_eq!(row[1]["callback_data"], format!("ap:a:{request_id}"));
    assert_eq!(row[2]["callback_data"], format!("ap:d:{request_id}"));
}

#[test]
fn slack_approval_buttons_render_as_block_kit_actions_with_primary_and_danger_styles() {
    let message = approval_outbound(Platform::Slack);
    let chunks = SlackRenderer::new().render(&message);
    let blocks = chunks[0]
        .blocks
        .as_ref()
        .expect("slack approval render should include Block Kit");
    let value = serde_json::to_value(blocks).expect("slack blocks should serialize");
    let actions = value
        .as_array()
        .and_then(|blocks| blocks.iter().find(|block| block["type"] == "actions"))
        .expect("slack blocks should include an actions block");
    let buttons = actions["elements"]
        .as_array()
        .expect("slack actions block should contain buttons");

    assert_eq!(buttons.len(), 3);
    assert_eq!(buttons[0]["text"]["text"], "Allow");
    assert_eq!(buttons[0]["style"], "primary");
    assert_eq!(buttons[1]["text"]["text"], "Always");
    assert!(buttons[1].get("style").is_none());
    assert_eq!(buttons[2]["text"]["text"], "Deny");
    assert_eq!(buttons[2]["style"], "danger");
    insta::assert_json_snapshot!("slack_approval_button_blocks", value);
}

#[test]
fn discord_approval_buttons_render_as_action_row_with_5_button_styles_constraint() {
    let request_id = fixed_request_id();
    let buttons = approval_buttons(Platform::Discord, request_id);
    let rows = discord::render_action_rows(&buttons, false);
    let value = serde_json::to_value(&rows).expect("discord action rows should serialize");
    let row = &value[0];
    let components = row["components"]
        .as_array()
        .expect("discord action row should contain components");

    assert_eq!(components.len(), 3);
    assert_eq!(row["type"], 1);
    assert_eq!(components[0]["custom_id"], format!("ap:o:{request_id}"));
    assert_eq!(components[0]["style"], 1);
    assert_eq!(components[1]["custom_id"], format!("ap:a:{request_id}"));
    assert_eq!(components[1]["style"], 2);
    assert_eq!(components[2]["custom_id"], format!("ap:d:{request_id}"));
    assert_eq!(components[2]["style"], 4);
    insta::assert_json_snapshot!("discord_approval_action_row", value);
}

#[test]
fn approval_button_callback_data_round_trips_through_each_platform_parser() {
    let request_id = fixed_request_id();
    let telegram_buttons = approval_buttons(Platform::Telegram, request_id);
    let slack_buttons = approval_buttons(Platform::Slack, request_id);
    let discord_buttons = approval_buttons(Platform::Discord, request_id);

    for (platform, buttons) in [
        (Platform::Telegram, telegram_buttons),
        (Platform::Slack, slack_buttons),
        (Platform::Discord, discord_buttons),
    ] {
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
            ],
            "{platform} approval callback data did not round-trip"
        );
    }
}

#[test]
fn telegram_callback_data_under_64_byte_limit_for_long_request_ids() {
    let buttons = approval_buttons(Platform::Telegram, fixed_request_id());

    for button in buttons {
        assert!(
            button.callback_data.len() <= 64,
            "telegram callback data exceeds 64-byte limit: {}",
            button.callback_data
        );
    }
}

#[test]
fn approval_buttons_disabled_after_decision_per_platform() {
    let request_id = fixed_request_id();
    let decision = ApprovalDecision::AllowOnce;

    let telegram_buttons =
        resolved_approval_buttons(Platform::Telegram, request_id, &decision, "@test-user");
    let telegram_markup =
        telegram::render_inline_keyboard(&telegram_buttons).expect("telegram resolved marker");
    let telegram_value =
        serde_json::to_value(telegram_markup).expect("telegram resolved markup serializes");
    assert_eq!(
        telegram_value["inline_keyboard"][0][0]["text"],
        "✓ Allowed by @test-user"
    );

    let slack_buttons =
        resolved_approval_buttons(Platform::Slack, request_id, &decision, "@test-user");
    let mut slack_message = approval_outbound(Platform::Slack);
    slack_message.buttons = slack_buttons;
    let slack_rendered = SlackRenderer::new().render(&slack_message);
    assert!(
        slack_rendered[0].blocks.is_none(),
        "Slack should remove approval action buttons after a decision"
    );

    let discord_buttons =
        resolved_approval_buttons(Platform::Discord, request_id, &decision, "@test-user");
    let discord_rows = discord::render_action_rows(&discord_buttons, true);
    let discord_value =
        serde_json::to_value(discord_rows).expect("discord disabled rows should serialize");
    for component in discord_value[0]["components"]
        .as_array()
        .expect("discord disabled row should contain buttons")
    {
        assert_eq!(component["disabled"], true);
    }

    let discord_chunks = DiscordRenderer::new().render(&approval_outbound(Platform::Discord));
    assert_eq!(
        discord_chunks.last().map(|chunk| chunk.buttons.len()),
        Some(3),
        "Discord pending approval render should still attach three buttons"
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
