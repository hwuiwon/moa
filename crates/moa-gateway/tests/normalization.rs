//! Out-of-line tests for gateway inbound normalization.

mod support;

use moa_core::{ChannelRef, MoaError, Platform};
use moa_gateway::{discord, slack, telegram};
use support::{assert_typed_gateway_error, fixture_json, fixture_text, inbound_top_level_fields};

#[test]
fn telegram_update_normalizes_to_canonical_inbound_with_user_workspace_text() {
    let inbound = telegram::normalize_update_json(&fixture_text("telegram_update_minimal.json"))
        .expect("telegram fixture should normalize");

    assert_eq!(inbound.platform, Platform::Telegram);
    assert_eq!(inbound.user.platform_id, "12345");
    assert_eq!(inbound.text, "hello");
    assert!(inbound.attachments.is_empty());
    assert_eq!(inbound.reply_to, None);
    assert_eq!(
        inbound.channel,
        ChannelRef::DirectMessage {
            user_id: "12345".to_string()
        }
    );
}

#[test]
fn slack_event_normalizes_to_canonical_inbound_with_thread_ts_preserved() {
    let inbound = slack::normalize_event_json(&fixture_text("slack_event_with_thread.json"))
        .expect("slack thread fixture should normalize");

    assert_eq!(inbound.platform, Platform::Slack);
    assert_eq!(inbound.user.platform_id, "U12345");
    assert_eq!(inbound.text, "hello");
    assert_eq!(
        inbound.channel,
        ChannelRef::Thread {
            channel_id: "C12345".to_string(),
            thread_id: "1700000000.000100".to_string(),
        }
    );
    assert_eq!(inbound.reply_to, Some("1700000000.000100".to_string()));
}

#[test]
fn discord_message_normalizes_to_canonical_inbound_with_guild_and_channel() {
    let inbound = discord::normalize_message_json(&fixture_text("discord_message_minimal.json"))
        .expect("discord fixture should normalize");

    assert_eq!(inbound.platform, Platform::Discord);
    assert_eq!(inbound.user.platform_id, "555555555555555555");
    assert_eq!(inbound.text, "hello");
    assert_eq!(
        inbound.channel,
        ChannelRef::Group {
            channel_id: "987654321".to_string()
        }
    );
}

#[test]
fn telegram_update_with_attachment_includes_file_id_and_mime_type() {
    let inbound =
        telegram::normalize_update_json(&fixture_text("telegram_update_with_attachment.json"))
            .expect("telegram attachment fixture should normalize");

    assert_eq!(inbound.text, "see attached");
    assert_eq!(inbound.attachments.len(), 1);
    let attachment = &inbound.attachments[0];
    assert_eq!(attachment.name, "notes.txt");
    assert_eq!(attachment.mime_type.as_deref(), Some("text/plain"));
    assert_eq!(
        attachment.url.as_deref(),
        Some("telegram://file/tg-file-001"),
        "telegram file_id should be preserved in the canonical attachment URL"
    );
    assert_eq!(attachment.size_bytes, Some(1234));
}

#[test]
fn all_three_platforms_produce_equivalent_inbound_for_simple_text_message() {
    let telegram = telegram::normalize_update_json(&fixture_text("telegram_update_minimal.json"))
        .expect("telegram fixture should normalize");
    let slack = slack::normalize_event_json(&fixture_text("slack_event_minimal.json"))
        .expect("slack fixture should normalize");
    let discord = discord::normalize_message_json(&fixture_text("discord_message_minimal.json"))
        .expect("discord fixture should normalize");

    assert_eq!(telegram.text, "hello");
    assert_eq!(slack.text, telegram.text);
    assert_eq!(discord.text, telegram.text);
    for inbound in [&telegram, &slack, &discord] {
        assert!(
            !inbound.user.platform_id.is_empty(),
            "{} normalized an empty platform user id",
            inbound.platform
        );
        assert!(
            matches!(
                inbound.channel,
                ChannelRef::DirectMessage { .. }
                    | ChannelRef::Group { .. }
                    | ChannelRef::Thread { .. }
            ),
            "{} normalized an invalid channel shape",
            inbound.platform
        );
    }
    assert_eq!(
        inbound_top_level_fields(&telegram),
        inbound_top_level_fields(&slack),
        "Slack leaked platform-specific top-level fields"
    );
    assert_eq!(
        inbound_top_level_fields(&telegram),
        inbound_top_level_fields(&discord),
        "Discord leaked platform-specific top-level fields"
    );

    assert_eq!(
        fixture_json("telegram_update_with_emoji_4byte.json")["message"]["text"],
        "hello 👋"
    );
    assert_eq!(
        fixture_json("slack_event_with_blocks.json")["event"]["text"],
        "hello"
    );
    assert_eq!(
        fixture_json("discord_message_with_mention.json")["content"],
        "hello <@666666666666666666>"
    );
    assert_eq!(
        fixture_json("discord_message_with_emoji_4byte.json")["content"],
        "hello 👋"
    );
}

#[test]
fn normalization_rejects_malformed_payloads_with_typed_errors() {
    assert_typed_gateway_error(telegram::normalize_update_json("{"));
    assert_typed_gateway_error(slack::normalize_event_json(
        r#"{"event":{"type":"message"}}"#,
    ));
    assert_typed_gateway_error(discord::normalize_message_json(
        r#"{"id":{},"channel_id":"987654321"}"#,
    ));

    let telegram_missing = telegram::normalize_update_json(r#"{"update_id":1}"#);
    assert!(
        matches!(telegram_missing, Err(MoaError::ValidationError(_))),
        "telegram missing-message payload should return a typed validation error"
    );
}
