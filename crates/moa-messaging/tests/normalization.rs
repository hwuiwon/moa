//! Out-of-line tests for Slack messaging inbound normalization.

mod support;

use moa_core::{ChannelRef, Platform};
use moa_messaging::slack;
use support::{assert_typed_messaging_error, fixture_text};

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
fn slack_normalization_rejects_malformed_payloads_with_typed_errors() {
    assert_typed_messaging_error(slack::normalize_event_json(
        r#"{"event":{"type":"message"}}"#,
    ));
}
