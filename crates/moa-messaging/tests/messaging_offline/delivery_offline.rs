//! Offline coverage for the channel-neutral delivery sink.

use std::collections::{BTreeMap, BTreeSet};

use moa_core::{Channel, ContactId};
use moa_messaging::{DeliveryMessage, DeliveryPurpose, DeliverySink, ProviderDeliverySink};
use serde_json::json;
use uuid::Uuid;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[cfg(feature = "postmark")]
use moa_messaging::PostmarkEmailClient;
#[cfg(feature = "twilio")]
use moa_messaging::TwilioSmsClient;

#[cfg(feature = "twilio")]
const ACCOUNT_SID: &str = "AC11111111111111111111111111111111";
#[cfg(feature = "twilio")]
const MESSAGE_SID: &str = "SM11111111111111111111111111111111";

#[cfg(feature = "postmark")]
#[tokio::test]
async fn delivery_offline_dispatches_email_through_postmark() {
    // Pins: generic delivery uses the existing Postmark connector and preserves provider metadata.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/email"))
        .and(header("x-postmark-server-token", "test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "To": "user@example.com",
            "SubmittedAt": null,
            "MessageID": "postmark-message-1",
            "ErrorCode": 0,
            "Message": "OK"
        })))
        .mount(&server)
        .await;
    let sink = ProviderDeliverySink::empty("MOA <moa@example.com>")
        .with_email_client(PostmarkEmailClient::new("test-token").with_base_url(server.uri()));
    let message = delivery_message(Channel::Email, "user@example.com")
        .with_subject("Verify")
        .with_metadata("session_id", "session-123");

    let receipt = sink
        .deliver(message)
        .await
        .expect("delivery sink should send email through Postmark");

    assert_eq!(receipt.channel, Channel::Email);
    assert_eq!(receipt.provider, "postmark");
    assert_eq!(
        receipt.provider_message_id.as_deref(),
        Some("postmark-message-1")
    );
    let request = only_request(&server).await;
    let body: serde_json::Value =
        serde_json::from_slice(&request.body).expect("captured Postmark body should be JSON");
    assert_eq!(body["From"], "MOA <moa@example.com>");
    assert_eq!(body["To"], "user@example.com");
    assert_eq!(body["Subject"], "Verify");
    assert_eq!(body["TextBody"], "delivery body");
    assert_eq!(body["Tag"], "contact_verification");
    assert_eq!(body["Metadata"]["session_id"], "session-123");
}

#[cfg(feature = "twilio")]
#[tokio::test]
async fn delivery_offline_dispatches_sms_through_twilio() {
    // Pins: generic delivery uses the existing Twilio connector and its sender validation.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/2010-04-01/Accounts/{ACCOUNT_SID}/Messages.json"
        )))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "sid": MESSAGE_SID,
            "status": "queued",
            "to": "+15005550006",
            "from": "+15551234567",
            "messaging_service_sid": null,
            "error_code": null,
            "error_message": null,
            "uri": format!("/2010-04-01/Accounts/{ACCOUNT_SID}/Messages/{MESSAGE_SID}.json")
        })))
        .mount(&server)
        .await;
    let sink = ProviderDeliverySink::empty("MOA <moa@example.com>").with_sms_client(
        TwilioSmsClient::from_account_auth_token(ACCOUNT_SID, "auth-token")
            .with_base_url(server.uri())
            .with_default_from("+15551234567"),
    );

    let receipt = sink
        .deliver(delivery_message(Channel::Sms, "+15005550006"))
        .await
        .expect("delivery sink should send SMS through Twilio");

    assert_eq!(receipt.channel, Channel::Sms);
    assert_eq!(receipt.provider, "twilio");
    assert_eq!(receipt.provider_message_id.as_deref(), Some(MESSAGE_SID));
    let request = only_request(&server).await;
    let pairs = form_pairs(&request.body);
    assert_eq!(
        pairs,
        BTreeSet::from([
            "Body=delivery+body".to_string(),
            "From=%2B15551234567".to_string(),
            "To=%2B15005550006".to_string(),
        ])
    );
}

fn delivery_message(channel: Channel, to: &str) -> DeliveryMessage {
    DeliveryMessage {
        tenant_id: Uuid::now_v7(),
        contact_id: ContactId::new(),
        purpose: DeliveryPurpose::ContactVerification,
        channel,
        to: to.to_string(),
        subject: Some("Delivery".to_string()),
        text_body: "delivery body".to_string(),
        html_body: None,
        metadata: BTreeMap::new(),
    }
}

trait DeliveryMessageExt {
    fn with_subject(self, subject: &str) -> Self;
    fn with_metadata(self, key: &str, value: &str) -> Self;
}

impl DeliveryMessageExt for DeliveryMessage {
    fn with_subject(mut self, subject: &str) -> Self {
        self.subject = Some(subject.to_string());
        self
    }

    fn with_metadata(mut self, key: &str, value: &str) -> Self {
        self.metadata.insert(key.to_string(), value.to_string());
        self
    }
}

async fn only_request(server: &MockServer) -> wiremock::Request {
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose captured requests");
    assert_eq!(requests.len(), 1, "expected exactly one provider request");
    requests
        .into_iter()
        .next()
        .expect("captured request should exist")
}

#[cfg(feature = "twilio")]
fn form_pairs(body: &[u8]) -> BTreeSet<String> {
    std::str::from_utf8(body)
        .expect("captured Twilio form should be UTF-8")
        .split('&')
        .map(ToOwned::to_owned)
        .collect()
}
