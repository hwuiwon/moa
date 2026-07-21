//! Wiremock offline counterpart for the privacy-filter sidecar live coverage.

use moa_core::types::security::SensitivityClass;
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
    assert_eq!(private.class, SensitivityClass::Restricted, "{private:?}");
    assert_eq!(private.spans.len(), 2);

    let clean = classifier
        .classify("the auth service uses JWT")
        .await
        .expect("classify clean text with wiremock sidecar");
    assert_eq!(clean.class, SensitivityClass::None, "{clean:?}");
}

#[tokio::test]
async fn openai_filter_offline_drops_spans_below_category_threshold() {
    // Pins the false-positive suppression in resolve_class: a span whose
    // confidence is below its category threshold must NOT escalate the class.
    // Default SSN threshold is 0.85; an SSN span at 0.80 is sub-threshold, so the
    // aggregate class stays None even though a span was returned. A regression
    // that drops the `>= threshold` gate would wrongly classify this as Phi.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/classify"))
        .and(body_string_contains("maybe an ssn"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model_version": "openai/privacy-filter:test",
            "abstained": false,
            "spans": [
                {
                    "start": 0,
                    "end": 11,
                    "category": "SSN",
                    "confidence": 0.80
                }
            ]
        })))
        .mount(&server)
        .await;

    let classifier = OpenAiPrivacyFilterClassifier::new(server.uri())
        .expect("classifier should build")
        .with_fail_closed_on_error(false);

    let result = classifier
        .classify("123-45-6789 maybe an ssn")
        .await
        .expect("classify sub-threshold span with wiremock sidecar");

    assert_eq!(
        result.class,
        SensitivityClass::None,
        "sub-threshold SSN span must not escalate the class: {result:?}"
    );
    assert_eq!(result.spans.len(), 1, "the span is still surfaced");
    assert!(!result.abstained, "{result:?}");
}

#[tokio::test]
async fn openai_filter_offline_abstain_forces_pii_class() {
    // Pins classify_inner's abstain branch: when the model reports
    // `"abstained": true`, MOA fails closed to SensitivityClass::Pii regardless of the
    // (here empty) span set, and surfaces the abstained flag to callers.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/classify"))
        .and(body_string_contains("ambiguous record"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "model_version": "openai/privacy-filter:test",
            "abstained": true,
            "spans": []
        })))
        .mount(&server)
        .await;

    let classifier = OpenAiPrivacyFilterClassifier::new(server.uri())
        .expect("classifier should build")
        .with_fail_closed_on_error(false);

    let result = classifier
        .classify("ambiguous record")
        .await
        .expect("classify abstained response with wiremock sidecar");

    assert_eq!(
        result.class,
        SensitivityClass::Pii,
        "an abstained model response must fail closed to Pii: {result:?}"
    );
    assert!(result.abstained, "{result:?}");
}
