//! Shared fixtures for messaging integration tests.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use moa_core::{
    Channel, ChannelActor, ChannelRef, InboundMessage, MessageContent, MessageId, MoaError,
    OutboundMessage, SessionId, types::Attachment,
};
use moa_messaging::MessagingSendResponse;
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Loads a JSON fixture, stripping leading provenance comments before parsing.
pub fn fixture_text(name: &str) -> String {
    let path = fixture_path(name);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read fixture {}: {error}", path.display()));
    raw.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Loads a fixture as arbitrary JSON.
pub fn fixture_json(name: &str) -> serde_json::Value {
    serde_json::from_str(&fixture_text(name))
        .unwrap_or_else(|error| panic!("failed to parse fixture {name}: {error}"))
}

/// Builds a simple outbound text message with no buttons or reply target.
pub fn text_message(text: impl Into<String>) -> OutboundMessage {
    OutboundMessage {
        content: MessageContent::Text(text.into()),
        buttons: Vec::new(),
        channel_ref: None,
        reply_to: None,
        ephemeral: false,
    }
}

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

/// Returns a deterministic message id.
pub fn message_id(value: &str) -> MessageId {
    MessageId::new(value)
}

/// Asserts rendered text chunks preserve input and honor a grapheme-count limit.
pub fn assert_grapheme_chunks(parts: &[String], original: &str, limit: usize) {
    assert!(
        parts.len() >= 2,
        "expected text to split into at least two chunks"
    );
    assert_eq!(
        parts.concat(),
        original,
        "rendered chunks should reconstruct the original text exactly"
    );
    for part in parts {
        assert!(
            part.graphemes(true).count() <= limit,
            "chunk exceeds grapheme limit {limit}: {}",
            part.graphemes(true).count()
        );
        assert!(
            original.contains(part),
            "chunk should be a complete substring of the original text"
        );
    }
}

/// Returns the sorted top-level field names in a normalized inbound message.
pub fn inbound_top_level_fields(message: &InboundMessage) -> BTreeSet<String> {
    match serde_json::to_value(message).expect("inbound message should serialize") {
        serde_json::Value::Object(map) => map.keys().cloned().collect(),
        other => panic!("inbound message serialized to non-object: {other:?}"),
    }
}

/// Asserts a normalizer returned a typed messaging/core error rather than panicking.
pub fn assert_typed_messaging_error(result: moa_core::Result<InboundMessage>) {
    assert!(
        matches!(
            result,
            Err(MoaError::SerdeJson(_)) | Err(MoaError::ValidationError(_))
        ),
        "expected typed messaging error, got {result:?}"
    );
}

/// Starts a local mock endpoint suitable for channel HTTP tests.
#[allow(dead_code)]
pub async fn mock_channel_api() -> wiremock::MockServer {
    wiremock::MockServer::start().await
}

/// Starts a mock endpoint that returns one 429 response followed by 200 responses.
pub async fn mock_429_then_200(header_name: &str, retry_after: &str) -> Arc<MockServer> {
    let server = Arc::new(MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header(header_name, retry_after)
                .set_body_string("rate limited"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    server
}

/// Starts a mock endpoint that always returns 429.
pub async fn mock_always_429(header_name: &str, retry_after: &str) -> Arc<MockServer> {
    let server = Arc::new(MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header(header_name, retry_after)
                .set_body_string("rate limited"),
        )
        .mount(&server)
        .await;
    server
}

/// Starts a mock endpoint that always returns 200.
pub async fn mock_always_200() -> Arc<MockServer> {
    let server = Arc::new(MockServer::start().await);
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;
    server
}

/// Posts one synthetic channel send request to a mock server.
pub async fn post_send(server: Arc<MockServer>) -> moa_core::Result<MessagingSendResponse> {
    let response = reqwest::Client::new()
        .post(format!("{}/send", server.uri()))
        .body("{}")
        .send()
        .await
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("Retry-After")
        .or_else(|| response.headers().get("X-RateLimit-Reset-After"))
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string);
    let body = response
        .text()
        .await
        .map_err(|error| MoaError::ProviderError(error.to_string()))?;
    let mut normalized = MessagingSendResponse::new(status, body);
    if let Some(retry_after) = retry_after {
        normalized = normalized
            .with_header("Retry-After", retry_after.clone())
            .with_header("X-RateLimit-Reset-After", retry_after);
    }
    Ok(normalized)
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("support")
        .join("fixtures")
        .join(name)
}
