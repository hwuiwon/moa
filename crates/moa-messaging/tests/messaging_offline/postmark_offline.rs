//! Wiremock offline coverage for the Postmark email connector.

use std::time::Duration;

use moa_config::MessagingConfig;
use moa_core::error::MoaError;
use moa_core::types::credentials::{DeploymentSecret, DeploymentSecrets};
use moa_messaging::{
    PostmarkEmailClient, PostmarkEmailFailureClass, PostmarkEmailMessage, PostmarkEmailSendResult,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn postmark_offline_sends_single_email_with_expected_request_shape() {
    // Pins: Postmark single-email sends use the documented /email endpoint and server-token header.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/email"))
        .and(header("accept", "application/json"))
        .and(header("content-type", "application/json"))
        .and(header("x-postmark-server-token", "test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "To": "ops@example.com",
            "SubmittedAt": "2026-06-18T12:00:00Z",
            "MessageID": "postmark-message-1",
            "ErrorCode": 0,
            "Message": "OK"
        })))
        .mount(&server)
        .await;

    let client = PostmarkEmailClient::new("test-token").with_base_url(server.uri());
    let message = PostmarkEmailMessage::new("MOA <moa@example.com>", "ops@example.com", "Alert")
        .with_text_body("Session failed")
        .with_html_body("<strong>Session failed</strong>")
        .with_tag("moa-alert")
        .with_metadata("session_id", "session-123");

    let result = client
        .send_email(&message)
        .await
        .expect("wiremock Postmark send should succeed");

    assert_eq!(
        result,
        PostmarkEmailSendResult {
            to: "ops@example.com".to_string(),
            submitted_at: Some(
                "2026-06-18T12:00:00Z"
                    .parse()
                    .expect("fixed timestamp should parse")
            ),
            message_id: "postmark-message-1".to_string(),
            error_code: 0,
            message: "OK".to_string(),
        }
    );
    let request = only_request(&server).await;
    let body: Value =
        serde_json::from_slice(&request.body).expect("captured Postmark body should be JSON");
    assert_eq!(body["From"], "MOA <moa@example.com>");
    assert_eq!(body["To"], "ops@example.com");
    assert_eq!(body["Subject"], "Alert");
    assert_eq!(body["TextBody"], "Session failed");
    assert_eq!(body["HtmlBody"], "<strong>Session failed</strong>");
    assert_eq!(body["Tag"], "moa-alert");
    assert_eq!(body["MessageStream"], "outbound");
    assert_eq!(body["Metadata"]["session_id"], "session-123");
}

#[tokio::test]
async fn postmark_offline_deployment_secret_supplies_token_and_message_stream() {
    // Pins: production wiring resolves the Postmark server token from the typed
    // deployment-secret source and sends it as the server-token header.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/email"))
        .and(header("x-postmark-server-token", "deployment-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "To": "ops@example.com",
            "SubmittedAt": null,
            "MessageID": "postmark-message-2",
            "ErrorCode": 0,
            "Message": "OK"
        })))
        .mount(&server)
        .await;
    let secrets = DeploymentSecrets::new().with(
        DeploymentSecret::PostmarkServerToken,
        Some("deployment-token".to_string()),
    );
    let config = MessagingConfig {
        postmark_base_url: server.uri(),
        postmark_message_stream: "alerts".to_string(),
        ..MessagingConfig::default()
    };
    let client = PostmarkEmailClient::from_deployment_secrets(&secrets, &config)
        .expect("Postmark client should build from the deployment server token");
    let message = PostmarkEmailMessage::new("moa@example.com", "ops@example.com", "Alert")
        .with_text_body("body");

    let result = client
        .send_email(&message)
        .await
        .expect("wiremock Postmark send should succeed");

    assert_eq!(result.message_id, "postmark-message-2");
    let request = only_request(&server).await;
    let body: Value =
        serde_json::from_slice(&request.body).expect("captured Postmark body should be JSON");
    assert_eq!(body["MessageStream"], "alerts");
}

#[tokio::test]
async fn postmark_offline_absent_deployment_token_leaves_email_unconfigured() {
    // Pins: a deployment without a Postmark token yields no client at all rather
    // than a client that would send an empty server-token header.
    let secrets = DeploymentSecrets::new();

    assert!(
        PostmarkEmailClient::from_deployment_secrets(&secrets, &MessagingConfig::default())
            .is_none()
    );
}

#[tokio::test]
async fn postmark_offline_blank_deployment_token_is_treated_as_unset() {
    // Pins: a present-but-blank deployment variable cannot make email delivery
    // look configured and then fail at the provider with an empty credential.
    let secrets = DeploymentSecrets::new().with(
        DeploymentSecret::PostmarkServerToken,
        Some("   ".to_string()),
    );

    assert!(!secrets.contains(DeploymentSecret::PostmarkServerToken));
    assert!(
        PostmarkEmailClient::from_deployment_secrets(&secrets, &MessagingConfig::default())
            .is_none()
    );
}

#[tokio::test]
async fn postmark_offline_surfaces_provider_status_errors() {
    // Pins: Postmark non-2xx responses preserve status and response body.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/email"))
        .respond_with(ResponseTemplate::new(422).set_body_string("inactive sender signature"))
        .mount(&server)
        .await;
    let client = PostmarkEmailClient::new("test-token").with_base_url(server.uri());
    let message = PostmarkEmailMessage::new("moa@example.com", "ops@example.com", "Alert")
        .with_text_body("body");

    let error = client
        .send_email(&message)
        .await
        .expect_err("Postmark 422 should surface as an HTTP status error");

    assert!(matches!(
        error,
        MoaError::HttpStatus {
            status: 422,
            retry_after: None,
            message
        } if message == "inactive sender signature"
    ));
}

#[tokio::test]
async fn postmark_offline_retries_safe_rate_limit_responses() {
    // Pins: Postmark 429 responses are retried locally using Retry-After before surfacing failure.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/email"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_string(r#"{"ErrorCode":429,"Message":"Rate limit exceeded"}"#),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/email"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "To": "ops@example.com",
            "SubmittedAt": null,
            "MessageID": "postmark-message-3",
            "ErrorCode": 0,
            "Message": "OK"
        })))
        .mount(&server)
        .await;
    let client = PostmarkEmailClient::new("test-token")
        .with_base_url(server.uri())
        .with_max_rate_limit_retries(1)
        .with_rate_limit_backoff(Duration::ZERO);
    let message = PostmarkEmailMessage::new("moa@example.com", "ops@example.com", "Alert")
        .with_text_body("body");

    let result = client
        .send_email(&message)
        .await
        .expect("429 then success should retry and accept the email");

    assert_eq!(result.message_id, "postmark-message-3");
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn postmark_offline_exhausted_rate_limit_surfaces_retryable_error() {
    // Pins: exhausted Postmark 429 retries remain typed as rate-limit failures for durable callers.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/email"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_string(r#"{"ErrorCode":429,"Message":"Rate limit exceeded"}"#),
        )
        .mount(&server)
        .await;
    let client = PostmarkEmailClient::new("test-token")
        .with_base_url(server.uri())
        .with_max_rate_limit_retries(1)
        .with_rate_limit_backoff(Duration::ZERO);
    let message = PostmarkEmailMessage::new("moa@example.com", "ops@example.com", "Alert")
        .with_text_body("body");

    let error = client
        .send_email(&message)
        .await
        .expect_err("repeated 429 responses should exhaust the Postmark retry budget");

    assert!(matches!(
        error,
        MoaError::RateLimited {
            retries: 1,
            message
        } if message == r#"{"ErrorCode":429,"Message":"Rate limit exceeded"}"#
    ));
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn postmark_offline_rejects_nonzero_api_error_codes() {
    // Pins: Postmark ErrorCode values are not treated as successful sends.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/email"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "To": "ops@example.com",
            "SubmittedAt": null,
            "MessageID": "",
            "ErrorCode": 406,
            "Message": "Inactive recipient"
        })))
        .mount(&server)
        .await;
    let client = PostmarkEmailClient::new("test-token").with_base_url(server.uri());
    let message = PostmarkEmailMessage::new("moa@example.com", "ops@example.com", "Alert")
        .with_text_body("body");

    let error = client
        .send_email(&message)
        .await
        .expect_err("Postmark ErrorCode 406 should reject the send");

    assert!(matches!(
        error,
        MoaError::ProviderError(message)
            if message == "postmark email permanent failure ErrorCode 406: Inactive recipient"
    ));
}

#[test]
fn postmark_offline_classifies_retryable_api_error_codes() {
    // Pins: transient Postmark API response failures are exposed for durable callers.
    let result = PostmarkEmailSendResult {
        to: "ops@example.com".to_string(),
        submitted_at: None,
        message_id: String::new(),
        error_code: 100,
        message: "Maintenance".to_string(),
    };

    let failure = result
        .send_failure()
        .expect("Postmark maintenance should classify as a send failure");

    assert_eq!(failure.class, PostmarkEmailFailureClass::Retryable);
    assert!(failure.is_retryable());
    assert_eq!(failure.backoff_hint, Some(Duration::from_secs(300)));
}

async fn only_request(server: &MockServer) -> wiremock::Request {
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 1, "expected exactly one Postmark request");
    requests
        .into_iter()
        .next()
        .expect("captured request should exist")
}

async fn request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests")
        .len()
}
