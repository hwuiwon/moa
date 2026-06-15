//! Wiremock offline counterpart for the privacy-filter sidecar live coverage.

use moa_memory_graph::PiiClass;
use moa_memory_pii::{OpenAiPrivacyFilterClassifier, PiiClassifier};
use serde_json::json;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn openai_filter_offline_classifies_private_and_clean_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/classify"))
        .and(body_string_contains("jane.doe@example.com"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model_version": "openai/privacy-filter:test",
            "abstained": false,
            "spans": [
                {
                    "start": 12,
                    "end": 32,
                    "category": "EMAIL",
                    "confidence": 0.99
                },
                {
                    "start": 54,
                    "end": 72,
                    "category": "SECRET",
                    "confidence": 0.95
                }
            ]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/classify"))
        .and(body_string_contains("the auth service uses JWT"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model_version": "openai/privacy-filter:test",
            "abstained": false,
            "spans": []
        })))
        .mount(&server)
        .await;

    let classifier = OpenAiPrivacyFilterClassifier::new(server.uri())
        .expect("classifier should build")
        .with_fail_closed_on_error(false);

    let private = classifier
        .classify("My email is jane.doe@example.com and my API secret is sk-test-1234567890.")
        .await
        .expect("classify private text with wiremock sidecar");
    assert_eq!(private.class, PiiClass::Restricted, "{private:?}");
    assert_eq!(private.spans.len(), 2);

    let clean = classifier
        .classify("the auth service uses JWT")
        .await
        .expect("classify clean text with wiremock sidecar");
    assert_eq!(clean.class, PiiClass::None, "{clean:?}");
}
