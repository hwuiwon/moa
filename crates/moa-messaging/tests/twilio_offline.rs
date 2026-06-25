//! Wiremock offline coverage for the Twilio SMS connector.

#![cfg(feature = "twilio")]

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose};
use moa_core::{Credential, CredentialVault, MessagingConfig, MoaError};
use moa_messaging::{
    TWILIO_ACCOUNT_SID_SERVICE, TWILIO_API_KEY_SECRET_SERVICE, TWILIO_API_KEY_SID_SERVICE,
    TWILIO_MESSAGING_SERVICE_SID_SERVICE, TwilioSmsClient, TwilioSmsFailureClass, TwilioSmsMessage,
    TwilioSmsSendResult,
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
async fn twilio_offline_from_vault_uses_api_key_and_messaging_service() {
    // Pins: production wiring can resolve Twilio API-key credentials and sender defaults from CredentialVault.
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
    let vault = Arc::new(
        MockVault::default()
            .with(TWILIO_ACCOUNT_SID_SERVICE, ACCOUNT_SID)
            .with(
                TWILIO_API_KEY_SID_SERVICE,
                "SK11111111111111111111111111111111",
            )
            .with(TWILIO_API_KEY_SECRET_SERVICE, "api-secret")
            .with(TWILIO_MESSAGING_SERVICE_SID_SERVICE, MESSAGING_SERVICE_SID),
    );
    let config = MessagingConfig {
        twilio_base_url: server.uri(),
        ..MessagingConfig::default()
    };
    let client = TwilioSmsClient::from_vault(vault, "tenant-1", &config)
        .await
        .expect("Twilio client should build from vault credentials");
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

#[derive(Debug, Default)]
struct MockVault {
    credentials: HashMap<(String, String), Credential>,
}

impl MockVault {
    fn with(mut self, service: &str, value: &str) -> Self {
        self.credentials.insert(
            (service.to_string(), "tenant-1".to_string()),
            Credential::Bearer(value.to_string()),
        );
        self
    }
}

#[async_trait]
impl CredentialVault for MockVault {
    async fn get(&self, service: &str, scope: &str) -> moa_core::Result<Credential> {
        self.credentials
            .get(&(service.to_string(), scope.to_string()))
            .cloned()
            .ok_or_else(|| MoaError::MissingEnvironmentVariable(service.to_string()))
    }

    async fn set(&self, _service: &str, _scope: &str, _cred: Credential) -> moa_core::Result<()> {
        Err(MoaError::StorageError(
            "mock vault is read-only".to_string(),
        ))
    }

    async fn delete(&self, _service: &str, _scope: &str) -> moa_core::Result<()> {
        Err(MoaError::StorageError(
            "mock vault is read-only".to_string(),
        ))
    }

    async fn list(&self, scope: &str) -> moa_core::Result<Vec<String>> {
        Ok(self
            .credentials
            .keys()
            .filter(|(_, candidate_scope)| candidate_scope == scope)
            .map(|(service, _)| service.clone())
            .collect())
    }
}
