//! Out-of-line tests for gateway edit-window fallback control flow.

mod support;

use moa_core::{MessageId, Platform};
use moa_gateway::{GatewayEditOutcome, GatewayEditResponse, edit_with_followup_fallback};
use support::message_id;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn telegram_edit_within_48_hour_window_succeeds_and_returns_edited_message_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/edit"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"message_id":"tg-001"}"#))
        .mount(&server)
        .await;
    let original = message_id("tg-001");

    let outcome = edit_with_followup_fallback(
        Platform::Telegram,
        original.clone(),
        "updated text".to_string(),
        |content| {
            edit_request(
                &server,
                content,
                GatewayEditResponse::success(original.clone()),
            )
        },
        |_reply_to, _content| async {
            panic!("follow-up should not be sent for successful Telegram edit")
        },
    )
    .await
    .expect("successful Telegram edit should not error");

    assert_eq!(
        outcome,
        GatewayEditOutcome::Edited {
            message_id: message_id("tg-001")
        }
    );
}

#[tokio::test]
async fn telegram_edit_after_48_hour_window_falls_back_to_followup_message_with_reference() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/edit"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string("Bad Request: message can't be edited"),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"message_id":"tg-new"}"#))
        .mount(&server)
        .await;
    let original = message_id("tg-old");
    let content = "replacement text 👋".to_string();

    let outcome = edit_with_followup_fallback(
        Platform::Telegram,
        original.clone(),
        content.clone(),
        |content| {
            edit_request(
                &server,
                content,
                GatewayEditResponse::failure(400, "Bad Request: message can't be edited"),
            )
        },
        |reply_to, content| followup_request(&server, reply_to, content, message_id("tg-new")),
    )
    .await
    .expect("stale Telegram edit should fall back to follow-up");

    assert_eq!(
        outcome,
        GatewayEditOutcome::FollowUp {
            message_id: message_id("tg-new"),
            reply_to: original,
            content: content.clone(),
        }
    );
    let requests = server
        .received_requests()
        .await
        .expect("request recording should be enabled");
    assert!(
        requests.iter().any(|request| request.url.path() == "/send"
            && request
                .body_json::<serde_json::Value>()
                .expect("send body should be JSON")
                == serde_json::json!({
                    "reply_to_message_id": "tg-old",
                    "text": content
                })),
        "fallback send should reference the original Telegram message"
    );
}

#[tokio::test]
async fn slack_edit_failure_with_message_not_found_falls_back_to_thread_reply() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/edit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": false,
            "error": "message_not_found"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "1700000000.000300"
        })))
        .mount(&server)
        .await;

    let outcome = edit_with_followup_fallback(
        Platform::Slack,
        message_id("1700000000.000100"),
        "replacement slack text".to_string(),
        |content| {
            edit_request(
                &server,
                content,
                GatewayEditResponse::failure(200, "message_not_found"),
            )
        },
        |reply_to, content| {
            followup_request(&server, reply_to, content, message_id("1700000000.000300"))
        },
    )
    .await
    .expect("Slack message_not_found should fall back to thread reply");

    assert!(matches!(outcome, GatewayEditOutcome::FollowUp { .. }));
}

#[tokio::test]
async fn edit_fallback_preserves_message_content_byte_for_byte() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/edit"))
        .respond_with(
            ResponseTemplate::new(400).set_body_string("Bad Request: message can't be edited"),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"message_id":"tg-new"}"#))
        .mount(&server)
        .await;
    let content = "exact `markdown` bytes\nwith emoji 👨‍👩‍👧".to_string();

    let outcome = edit_with_followup_fallback(
        Platform::Telegram,
        message_id("tg-old"),
        content.clone(),
        |content| {
            edit_request(
                &server,
                content,
                GatewayEditResponse::failure(400, "Bad Request: message can't be edited"),
            )
        },
        |reply_to, content| followup_request(&server, reply_to, content, message_id("tg-new")),
    )
    .await
    .expect("fallback should preserve content");

    assert_eq!(
        outcome,
        GatewayEditOutcome::FollowUp {
            message_id: message_id("tg-new"),
            reply_to: message_id("tg-old"),
            content,
        }
    );
}

async fn edit_request(
    server: &MockServer,
    content: String,
    response: GatewayEditResponse,
) -> moa_core::Result<GatewayEditResponse> {
    post_json(server, "/edit", serde_json::json!({ "text": content })).await?;
    Ok(response)
}

async fn followup_request(
    server: &MockServer,
    reply_to: MessageId,
    content: String,
    message_id: MessageId,
) -> moa_core::Result<MessageId> {
    post_json(
        server,
        "/send",
        serde_json::json!({
            "reply_to_message_id": reply_to.as_str(),
            "text": content
        }),
    )
    .await?;
    Ok(message_id)
}

async fn post_json(
    server: &MockServer,
    path: &str,
    body: serde_json::Value,
) -> moa_core::Result<()> {
    reqwest::Client::new()
        .post(format!("{}{}", server.uri(), path))
        .json(&body)
        .send()
        .await
        .map_err(|error| moa_core::MoaError::ProviderError(error.to_string()))?;
    Ok(())
}
