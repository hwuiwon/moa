//! Smoke coverage for the HTTP-backed PII classifier.

use moa_memory_pii::{
    MockClassifier, OpenAiPrivacyFilterClassifier, PiiCategory, PiiClass, PiiClassifier, PiiResult,
    PiiSpan, PrivacyFilterThresholds, openai_filter::resolve_class,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn spawn_test_service() -> String {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local test classifier");
    let addr = listener.local_addr().expect("read local classifier addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _peer)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 8192];
                let read = stream
                    .read(&mut buffer)
                    .await
                    .expect("read classifier request");
                let request = String::from_utf8_lossy(&buffer[..read]);
                let body = if request.contains("123-45-6789") {
                    r#"{"spans":[{"start":10,"end":21,"category":"SSN","confidence":0.97}],"abstained":false,"model_version":"test/privacy-filter"}"#
                } else {
                    r#"{"spans":[],"abstained":false,"model_version":"test/privacy-filter"}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write classifier response");
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn classify_smoke_maps_ssn_to_phi_and_clean_text_to_none() {
    let base_url = spawn_test_service().await;
    let classifier =
        OpenAiPrivacyFilterClassifier::new(base_url).expect("create HTTP PII classifier");

    let ssn = classifier
        .classify("My SSN is 123-45-6789")
        .await
        .expect("classify SSN text");
    assert_eq!(ssn.class, PiiClass::Phi);
    assert_eq!(ssn.spans.len(), 1);
    assert_eq!(ssn.spans[0].category, PiiCategory::Ssn);

    let clean = classifier
        .classify("the auth service uses JWT")
        .await
        .expect("classify clean text");
    assert_eq!(clean.class, PiiClass::None);
    assert!(clean.spans.is_empty());
}

#[tokio::test]
async fn mock_classifier_round_trips_fixed_result() {
    let fixed = PiiResult {
        class: PiiClass::Pii,
        spans: vec![PiiSpan {
            start: 0,
            end: 5,
            category: PiiCategory::Email,
            confidence: 0.88,
        }],
        model_version: "mock".to_string(),
        abstained: false,
    };
    let classifier = MockClassifier {
        fixed: fixed.clone(),
    };

    let result = classifier
        .classify("anything")
        .await
        .expect("classify with mock");
    assert_eq!(result, fixed);
}

#[tokio::test]
async fn classifier_fails_closed_on_network_error() {
    let classifier = OpenAiPrivacyFilterClassifier::new("http://127.0.0.1:9")
        .expect("create fail-closed classifier");
    let result = classifier
        .classify("network should fail")
        .await
        .expect("fail-closed classifier should return a result");
    assert_eq!(result.class, PiiClass::Pii);
    assert!(result.abstained);
}

#[test]
fn real_model_aliases_map_to_moa_categories() {
    assert_eq!(
        PiiCategory::parse_label("private_person"),
        Some(PiiCategory::Person)
    );
    assert_eq!(
        PiiCategory::parse_label("account_number"),
        Some(PiiCategory::FinancialAccount)
    );
    assert_eq!(
        PiiCategory::parse_label("secret"),
        Some(PiiCategory::Secret)
    );

    let class = resolve_class(
        &[PiiSpan {
            start: 0,
            end: 12,
            category: PiiCategory::Secret,
            confidence: 0.99,
        }],
        PrivacyFilterThresholds::default(),
    );
    assert_eq!(class, PiiClass::Restricted);
}
