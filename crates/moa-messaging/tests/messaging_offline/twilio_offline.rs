//! Wiremock offline coverage for the Twilio SMS connector.

use std::collections::BTreeSet;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose};
use moa_config::MessagingConfig;
use moa_core::error::MoaError;
use moa_core::types::credentials::{DeploymentSecret, DeploymentSecrets};
use moa_messaging::{
    TwilioSmsClient, TwilioSmsFailureClass, TwilioSmsMessage, TwilioSmsSendResult,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ACCOUNT_SID: &str = "AC11111111111111111111111111111111";
const MESSAGE_SID: &str = "SM11111111111111111111111111111111";
const MESSAGING_SERVICE_SID: &str = "MG11111111111111111111111111111111";
const TEST_TO_NUMBER: &str = "+15005550006";
const TEST_TO_FORM_PAIR: &str = "To=%2B15005550006";

#[tokio::test]
async fn twilio_offline_sends_sms_with_form_body_and_basic_auth() {
    // Pins: Twilio sends use the documented Messages endpoint, form body, and Basic auth.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/2010-04-01/Accounts/{ACCOUNT_SID}/Messages.json"
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(success_body(
            TEST_TO_NUMBER,
            Some("+15551234567"),
            None,
        )))
        .mount(&server)
        .await;
    let client = TwilioSmsClient::from_account_auth_token(ACCOUNT_SID, "auth-token")
        .with_base_url(server.uri())
        .with_default_from("+15551234567");
    let message = TwilioSmsMessage::new(TEST_TO_NUMBER, "moa-alert");

    let result = client
        .send_sms(&message)
        .await
        .expect("wiremock Twilio send should succeed");

    assert_eq!(
        result,
        TwilioSmsSendResult {
            sid: MESSAGE_SID.to_string(),
            status: "queued".to_string(),
            to: TEST_TO_NUMBER.to_string(),
            from: Some("+15551234567".to_string()),
            messaging_service_sid: None,
            error_code: None,
            error_message: None,
            uri: format!("/2010-04-01/Accounts/{ACCOUNT_SID}/Messages/{MESSAGE_SID}.json"),
        }
    );
    let request = only_request(&server).await;
    let expected_auth = format!(
        "Basic {}",
        general_purpose::STANDARD.encode(format!("{ACCOUNT_SID}:auth-token"))
    );
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some(expected_auth.as_str())
    );
    let pairs = form_pairs(&request.body);
    assert_eq!(
        pairs,
        BTreeSet::from([
            "Body=moa-alert".to_string(),
            "From=%2B15551234567".to_string(),
            TEST_TO_FORM_PAIR.to_string(),
        ])
    );
}

#[tokio::test]
async fn twilio_offline_deployment_secrets_use_api_key_and_messaging_service() {
    // Pins: production wiring resolves Twilio API-key credentials and sender
    // defaults from the typed deployment-secret source, and prefers the API key
    // pair over the broader account auth token when both are configured.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/2010-04-01/Accounts/{ACCOUNT_SID}/Messages.json"
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(success_body(
            TEST_TO_NUMBER,
            None,
            Some(MESSAGING_SERVICE_SID),
        )))
        .mount(&server)
        .await;
    let secrets = DeploymentSecrets::new()
        .with(
            DeploymentSecret::TwilioAccountSid,
            Some(ACCOUNT_SID.to_string()),
        )
        .with(
            DeploymentSecret::TwilioApiKeySid,
            Some("SK11111111111111111111111111111111".to_string()),
        )
        .with(
            DeploymentSecret::TwilioApiKeySecret,
            Some("api-secret".to_string()),
        )
        .with(
            DeploymentSecret::TwilioAuthToken,
            Some("account-auth-token".to_string()),
        )
        .with(
            DeploymentSecret::TwilioMessagingServiceSid,
            Some(MESSAGING_SERVICE_SID.to_string()),
        );
    let config = MessagingConfig {
        twilio_base_url: server.uri(),
        ..MessagingConfig::default()
    };
    let client = TwilioSmsClient::from_deployment_secrets(&secrets, &config)
        .expect("Twilio client should build from deployment secrets")
        .expect("an account sid is configured, so a client is built");
    let message = TwilioSmsMessage::new(TEST_TO_NUMBER, "moa-alert");

    let result = client
        .send_sms(&message)
        .await
        .expect("wiremock Twilio send should succeed");

    assert_eq!(result.sid, MESSAGE_SID);
    assert_eq!(
        result.messaging_service_sid.as_deref(),
        Some(MESSAGING_SERVICE_SID)
    );
    let request = only_request(&server).await;
    let expected_auth = format!(
        "Basic {}",
        general_purpose::STANDARD.encode("SK11111111111111111111111111111111:api-secret")
    );
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some(expected_auth.as_str())
    );
    let pairs = form_pairs(&request.body);
    assert_eq!(
        pairs,
        BTreeSet::from([
            "Body=moa-alert".to_string(),
            format!("MessagingServiceSid={MESSAGING_SERVICE_SID}"),
            TEST_TO_FORM_PAIR.to_string(),
        ])
    );
}

#[tokio::test]
async fn twilio_offline_absent_account_sid_leaves_sms_unconfigured() {
    // Pins: a deployment without Twilio configured yields no client rather than
    // one that would authenticate with empty credentials.
    let client = TwilioSmsClient::from_deployment_secrets(
        &DeploymentSecrets::new(),
        &MessagingConfig::default(),
    )
    .expect("an unconfigured deployment is not an error");

    assert!(client.is_none());
}

#[tokio::test]
async fn twilio_offline_half_configured_api_key_fails_instead_of_downgrading() {
    // Pins: an API key SID without its secret is a typed configuration error. It
    // must not silently fall back to the account auth token, which is a broader
    // credential than the operator asked to use.
    let secrets = DeploymentSecrets::new()
        .with(
            DeploymentSecret::TwilioAccountSid,
            Some(ACCOUNT_SID.to_string()),
        )
        .with(
            DeploymentSecret::TwilioApiKeySid,
            Some("SK11111111111111111111111111111111".to_string()),
        )
        .with(
            DeploymentSecret::TwilioAuthToken,
            Some("account-auth-token".to_string()),
        );

    // `TwilioSmsClient` deliberately has no `Debug`, so the success arm is
    // matched rather than unwrapped through `expect_err`.
    let error =
        match TwilioSmsClient::from_deployment_secrets(&secrets, &MessagingConfig::default()) {
            Ok(_) => panic!("a half-configured api key pair must fail closed"),
            Err(error) => error,
        };

    assert!(matches!(error, MoaError::ConfigError(_)), "{error:?}");
}

#[tokio::test]
async fn twilio_offline_surfaces_provider_status_errors() {
    // Pins: Twilio non-2xx responses preserve status and response body.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/2010-04-01/Accounts/{ACCOUNT_SID}/Messages.json"
        )))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"code": 20003, "message": "Authenticate"})),
        )
        .mount(&server)
        .await;
    let client = TwilioSmsClient::from_account_auth_token(ACCOUNT_SID, "bad-token")
        .with_base_url(server.uri());
    let message = TwilioSmsMessage::new(TEST_TO_NUMBER, "moa-alert").with_from("+15551234567");

    let error = client
        .send_sms(&message)
        .await
        .expect_err("Twilio 401 should surface as an HTTP status error");

    assert!(matches!(
        error,
        MoaError::HttpStatus {
            status: 401,
            retry_after: None,
            message
        } if message == r#"{"code":20003,"message":"Authenticate"}"#
    ));
}

#[tokio::test]
async fn twilio_offline_retries_safe_rate_limit_responses() {
    // Pins: Twilio HTTP 429 responses are safe to retry after backoff and do not surface as send failures.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/2010-04-01/Accounts/{ACCOUNT_SID}/Messages.json"
        )))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_json(json!({"code": 20429, "message": "Too many requests"})),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/2010-04-01/Accounts/{ACCOUNT_SID}/Messages.json"
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(success_body(
            TEST_TO_NUMBER,
            Some("+15551234567"),
            None,
        )))
        .mount(&server)
        .await;
    let client = TwilioSmsClient::from_account_auth_token(ACCOUNT_SID, "auth-token")
        .with_base_url(server.uri())
        .with_default_from("+15551234567")
        .with_max_rate_limit_retries(1)
        .with_rate_limit_backoff(Duration::ZERO);
    let message = TwilioSmsMessage::new(TEST_TO_NUMBER, "moa-alert");

    let result = client
        .send_sms(&message)
        .await
        .expect("429 then success should retry and accept the SMS");

    assert_eq!(result.sid, MESSAGE_SID);
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn twilio_offline_exhausted_rate_limit_surfaces_retryable_error() {
    // Pins: exhausted Twilio 429 retries remain typed as rate-limit failures for durable callers.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/2010-04-01/Accounts/{ACCOUNT_SID}/Messages.json"
        )))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "0")
                .set_body_json(json!({"code": 20429, "message": "Too many requests"})),
        )
        .mount(&server)
        .await;
    let client = TwilioSmsClient::from_account_auth_token(ACCOUNT_SID, "auth-token")
        .with_base_url(server.uri())
        .with_default_from("+15551234567")
        .with_max_rate_limit_retries(1)
        .with_rate_limit_backoff(Duration::ZERO);
    let message = TwilioSmsMessage::new(TEST_TO_NUMBER, "moa-alert");

    let error = client
        .send_sms(&message)
        .await
        .expect_err("repeated 429 responses should exhaust the Twilio retry budget");

    assert!(matches!(
        error,
        MoaError::RateLimited {
            retries: 1,
            message
        } if message == r#"{"code":20429,"message":"Too many requests"}"#
    ));
    assert_eq!(request_count(&server).await, 2);
}

#[tokio::test]
async fn twilio_offline_fetches_sms_status_with_error_details() {
    // Pins: live diagnostics can fetch Twilio's final message status after API acceptance.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/2010-04-01/Accounts/{ACCOUNT_SID}/Messages/{MESSAGE_SID}.json"
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sid": MESSAGE_SID,
            "status": "undelivered",
            "to": TEST_TO_NUMBER,
            "from": "+13239876987",
            "messaging_service_sid": null,
            "error_code": 30034,
            "error_message": null,
            "uri": format!("/2010-04-01/Accounts/{ACCOUNT_SID}/Messages/{MESSAGE_SID}.json")
        })))
        .mount(&server)
        .await;
    let client = TwilioSmsClient::from_account_auth_token(ACCOUNT_SID, "auth-token")
        .with_base_url(server.uri());

    let result = client
        .fetch_sms(MESSAGE_SID)
        .await
        .expect("wiremock Twilio status fetch should succeed");

    assert_eq!(result.sid, MESSAGE_SID);
    assert_eq!(result.status, "undelivered");
    assert_eq!(result.error_code, Some(30034));
    let failure = result
        .delivery_failure()
        .expect("A2P 30034 should classify as a terminal delivery failure");
    assert_eq!(failure.class, TwilioSmsFailureClass::Permanent);
    assert!(!failure.is_retryable());
    assert_eq!(failure.backoff_hint, None);
}

#[test]
fn twilio_offline_classifies_retryable_delivery_failures() {
    // Pins: downstream transient delivery errors are surfaced for durable callers to retry explicitly.
    let result = TwilioSmsSendResult {
        sid: MESSAGE_SID.to_string(),
        status: "failed".to_string(),
        to: TEST_TO_NUMBER.to_string(),
        from: Some("+15551234567".to_string()),
        messaging_service_sid: None,
        error_code: Some(30001),
        error_message: Some("Queue overflow".to_string()),
        uri: format!("/2010-04-01/Accounts/{ACCOUNT_SID}/Messages/{MESSAGE_SID}.json"),
    };

    let failure = result
        .delivery_failure()
        .expect("queue overflow should classify as a terminal delivery failure");

    assert_eq!(failure.class, TwilioSmsFailureClass::Retryable);
    assert!(failure.is_retryable());
    assert_eq!(failure.backoff_hint, Some(Duration::from_secs(60)));
}

fn success_body(
    to: &str,
    from: Option<&str>,
    messaging_service_sid: Option<&str>,
) -> serde_json::Value {
    json!({
        "account_sid": ACCOUNT_SID,
        "api_version": "2010-04-01",
        "body": "moa-alert",
        "date_created": "Thu, 18 Jun 2026 12:00:00 +0000",
        "date_sent": null,
        "date_updated": "Thu, 18 Jun 2026 12:00:00 +0000",
        "direction": "outbound-api",
        "error_code": null,
        "error_message": null,
        "from": from,
        "messaging_service_sid": messaging_service_sid,
        "num_media": "0",
        "num_segments": "1",
        "price": null,
        "price_unit": null,
        "sid": MESSAGE_SID,
        "status": "queued",
        "to": to,
        "uri": format!("/2010-04-01/Accounts/{ACCOUNT_SID}/Messages/{MESSAGE_SID}.json")
    })
}

async fn only_request(server: &MockServer) -> wiremock::Request {
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 1, "expected exactly one Twilio request");
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

fn form_pairs(body: &[u8]) -> BTreeSet<String> {
    String::from_utf8(body.to_vec())
        .expect("captured Twilio form should be UTF-8")
        .split('&')
        .map(ToOwned::to_owned)
        .collect()
}
