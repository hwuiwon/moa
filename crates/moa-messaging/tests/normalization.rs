//! Out-of-line tests for Slack messaging inbound normalization.

#[path = "support/normalization.rs"]
mod support;

use moa_core::{Channel, ChannelRef};
use moa_messaging::slack;
use support::{assert_typed_messaging_error, fixture_text};

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
fn slack_normalization_rejects_malformed_payloads_with_typed_errors() {
    assert_typed_messaging_error(slack::normalize_event_json(
        r#"{"event":{"type":"message"}}"#,
    ));
}
