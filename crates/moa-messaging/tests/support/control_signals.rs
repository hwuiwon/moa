//! Control-signal test fixtures.

use moa_core::{
    Channel, ChannelActor, ChannelRef, InboundMessage, MessageContent, OutboundMessage, SessionId,
    types::Attachment,
};
use uuid::Uuid;

/// Returns a deterministic session id for messaging control-flow tests.
pub fn fixed_session_id() -> SessionId {
    SessionId(
        Uuid::parse_str("018f6d7a-0b0c-7d00-8000-000000000002")
            .expect("fixed session UUID should parse"),
    )
}

/// Builds a canonical inbound message for control-signal tests.
pub fn inbound_message(channel: Channel, text: impl Into<String>) -> InboundMessage {
    InboundMessage {
        channel,
        channel_msg_id: "msg-001".to_string(),
        actor: ChannelActor {
            external_id: "user-001".to_string(),
            display_name: "Test User".to_string(),
            channel_account_id: None,
            moa_user_id: None,
        },
        channel_ref: ChannelRef::Slack {
            team_id: None,
            slack_channel_id: Some("channel-001".to_string()),
            thread_ts: None,
            user_id: None,
        },
        text: text.into(),
        attachments: Vec::<Attachment>::new(),
        reply_to: None,
        timestamp: chrono::DateTime::parse_from_rfc3339("2026-05-07T12:00:00Z")
            .expect("fixed timestamp should parse")
            .with_timezone(&chrono::Utc),
    }
}

/// Extracts text from an outbound acknowledgement.
pub fn outbound_text(message: &OutboundMessage) -> &str {
    match &message.content {
        MessageContent::Text(text) | MessageContent::Markdown(text) => text,
        other => panic!("expected outbound text, got {other:?}"),
    }
}
