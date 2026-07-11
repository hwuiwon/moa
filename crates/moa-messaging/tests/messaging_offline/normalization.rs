//! Out-of-line tests for Slack messaging inbound normalization.

#[path = "../support/normalization.rs"]
mod support;

use moa_core::{
    types::channel::Channel, types::channel::ChannelEvent, types::channel::ChannelRef,
    types::channel::ChannelSessionCommand,
};
use moa_messaging::slack;
use support::{assert_serde_json_error, assert_validation_error, fixture_text};

#[test]
fn slack_event_normalizes_to_canonical_inbound_with_thread_ts_preserved() {
    let event = slack::normalize_event_json(&fixture_text("slack_event_with_thread.json"))
        .expect("slack thread fixture should normalize");
    let ChannelEvent::Message(inbound) = event else {
        panic!("non-command Slack event should stay a message");
    };

    assert_eq!(inbound.channel, Channel::Slack);
    assert_eq!(inbound.actor.external_id, "U12345");
    assert_eq!(inbound.text, "hello");
    assert_eq!(
        inbound.channel_ref,
        ChannelRef::Slack {
            team_id: Some("T12345".to_string()),
            slack_channel_id: Some("C12345".to_string()),
            thread_ts: Some("1700000000.000100".to_string()),
            user_id: Some("U12345".to_string()),
        }
    );
    assert_eq!(inbound.reply_to, Some("1700000000.000100".to_string()));
}

#[test]
fn slack_status_command_normalizes_to_session_command() {
    // Pins: Slack users can request the active session status without a generic control bus.
    let event = slack::normalize_event_json(&slack_message_payload("/moa status"))
        .expect("status command should normalize");

    let ChannelEvent::SessionCommand(ChannelSessionCommand::Status(inbound)) = event else {
        panic!("expected status session command");
    };
    assert_eq!(inbound.text, "/moa status");
    assert_eq!(inbound.channel_msg_id, "1700000000.000200");
    assert_eq!(
        inbound.channel_ref,
        ChannelRef::Slack {
            team_id: Some("T12345".to_string()),
            slack_channel_id: Some("C12345".to_string()),
            thread_ts: Some("1700000000.000100".to_string()),
            user_id: Some("U12345".to_string()),
        }
    );
}

#[test]
fn slack_stop_command_normalizes_to_session_command() {
    // Pins: Slack users can request cancellation of the active session turn.
    let event = slack::normalize_event_json(&slack_message_payload("  /moa stop  "))
        .expect("stop command should normalize");

    let ChannelEvent::SessionCommand(ChannelSessionCommand::Stop(inbound)) = event else {
        panic!("expected stop session command");
    };
    assert_eq!(inbound.text, "  /moa stop  ");
}

#[test]
fn slack_unknown_moa_command_remains_plain_message() {
    // Pins: unsupported `/moa` commands do not resurrect the old generic control surface.
    let event = slack::normalize_event_json(&slack_message_payload("/moa queue"))
        .expect("unknown moa command should normalize as a message");

    let ChannelEvent::Message(inbound) = event else {
        panic!("unknown command should remain a message");
    };
    assert_eq!(inbound.text, "/moa queue");
}

#[test]
fn slack_normalization_rejects_unparseable_payload_with_serde_error() {
    // Pins: a payload missing the required Slack push-event envelope fields fails to deserialize,
    // surfacing as the exact `SerdeJson` variant (not collapsed with the validation path).
    assert_serde_json_error(slack::normalize_event_json(
        r#"{"event":{"type":"message"}}"#,
    ));
}

#[test]
fn slack_normalization_rejects_message_without_channel_with_validation_error() {
    // Pins: a well-formed `message` event that lacks a channel deserializes cleanly but is not a
    // supported user message, so the normalizer returns the exact `ValidationError` variant rather
    // than a deserialization error.
    let missing_channel = r#"{
        "token": "verification-token",
        "team_id": "T12345",
        "api_app_id": "A12345",
        "event": {
            "type": "message",
            "user": "U12345",
            "text": "hello",
            "ts": "1700000000.000200",
            "event_ts": "1700000000.000200"
        },
        "type": "event_callback",
        "event_id": "Ev12346",
        "event_time": 1700000000
    }"#;
    assert_validation_error(slack::normalize_event_json(missing_channel));
}

fn slack_message_payload(text: &str) -> String {
    format!(
        r#"{{
        "token": "verification-token",
        "team_id": "T12345",
        "api_app_id": "A12345",
        "event": {{
            "type": "message",
            "channel": "C12345",
            "user": "U12345",
            "text": "{text}",
            "ts": "1700000000.000200",
            "thread_ts": "1700000000.000100",
            "event_ts": "1700000000.000200",
            "channel_type": "channel"
        }},
        "type": "event_callback",
        "event_id": "Ev12346",
        "event_time": 1700000000
    }}"#
    )
}
