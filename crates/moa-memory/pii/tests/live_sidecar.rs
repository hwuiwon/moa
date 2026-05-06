//! Live end-to-end tests against a running `moa-pii-service` container.

use std::time::Duration;

use moa_memory_pii::{OpenAiPrivacyFilterClassifier, PiiClass, PiiClassifier};

fn live_service_url() -> String {
    std::env::var("MOA_PII_SERVICE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

#[tokio::test]
#[ignore = "requires docker compose up -d moa-pii-service and model weights"]
async fn live_sidecar_classifies_private_and_clean_text() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .expect("create live sidecar HTTP client");
    let classifier = OpenAiPrivacyFilterClassifier::with_client(live_service_url(), client)
        .with_fail_closed_on_error(false);

    let private = classifier
        .classify("My email is jane.doe@example.com and my API secret is sk-test-1234567890.")
        .await
        .expect("classify private text with live sidecar");
    assert!(
        matches!(
            private.class,
            PiiClass::Pii | PiiClass::Phi | PiiClass::Restricted
        ),
        "{private:?}"
    );
    assert!(!private.spans.is_empty(), "{private:?}");

    let clean = classifier
        .classify("the auth service uses JWT")
        .await
        .expect("classify clean text with live sidecar");
    assert_eq!(clean.class, PiiClass::None, "{clean:?}");
}
