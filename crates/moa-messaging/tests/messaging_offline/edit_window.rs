//! Out-of-line tests for Slack messaging edit-window fallback control flow.

#[path = "../support/edit_window.rs"]
mod support;

use moa_core::{Channel, MessageId};
use moa_messaging::{MessagingEditOutcome, MessagingEditResponse, edit_with_followup_fallback};
use support::message_id;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
        Channel::Slack,
        message_id("1700000000.000100"),
        "replacement slack text".to_string(),
        |content| {
            edit_request(
                &server,
                content,
                MessagingEditResponse::failure(200, "message_not_found"),
            )
        },
        |reply_to, content| {
            followup_request(&server, reply_to, content, message_id("1700000000.000300"))
        },
    )
    .await
    .expect("Slack message_not_found should fall back to thread reply");

    assert!(matches!(outcome, MessagingEditOutcome::FollowUp { .. }));
}

#[tokio::test]
async fn slack_edit_fallback_preserves_message_content_byte_for_byte() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/edit"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": false,
            "error": "cant_update_message"
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/send"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "ts": "1700000000.000400"
        })))
        .mount(&server)
        .await;
    let content = "exact `markdown` bytes\nwith emoji 👨‍👩‍👧".to_string();

    let outcome = edit_with_followup_fallback(
        Channel::Slack,
        message_id("1700000000.000100"),
        content.clone(),
        |content| {
            edit_request(
                &server,
                content,
                MessagingEditResponse::failure(200, "cant_update_message"),
            )
        },
        |reply_to, content| {
            followup_request(&server, reply_to, content, message_id("1700000000.000400"))
        },
    )
    .await
    .expect("fallback should preserve content");

    assert_eq!(
        outcome,
        MessagingEditOutcome::FollowUp {
            message_id: message_id("1700000000.000400"),
            reply_to: message_id("1700000000.000100"),
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
                    "reply_to_message_id": "1700000000.000100",
                    "text": content
                })),
        "fallback send should reference the original Slack message"
    );
}

async fn edit_request(
    server: &MockServer,
    content: String,
    response: MessagingEditResponse,
) -> moa_core::Result<MessagingEditResponse> {
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
