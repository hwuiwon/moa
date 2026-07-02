//! Out-of-line tests for Slack messaging inbound normalization.

#[path = "../support/normalization.rs"]
mod support;

use moa_core::{Channel, ChannelRef};
use moa_messaging::slack;
use support::{assert_serde_json_error, assert_validation_error, fixture_text};

#[test]
fn slack_event_normalizes_to_canonical_inbound_with_thread_ts_preserved() {
    let inbound = slack::normalize_event_json(&fixture_text("slack_event_with_thread.json"))
        .expect("slack thread fixture should normalize");

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
