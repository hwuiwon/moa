//! Offline LlamaParse parser coverage.

use hmac::{Hmac, Mac};
use moa_core::types::identifiers::TenantId;
use moa_knowledge::{
    Error,
    chunking::{ChunkingConfig, blocks_to_chunks, elements_to_blocks},
    domain::{KnowledgeObject, ObjectStatus, ParseInput},
    parser::{DocumentParser, llamaparse::LlamaParseParser, verify_parser_webhook},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

fn input() -> ParseInput {
    ParseInput {
        object: KnowledgeObject {
            acl: moa_knowledge::domain::ObjectAcl::incomplete(),
            object_uid: Uuid::from_u128(21),
            tenant_id: TenantId::from(Uuid::from_u128(22)),
            connection_uid: Uuid::from_u128(23),
            object_type: "document".to_string(),
            source_id: "llamaparse-source".to_string(),
            parent_source_id: None,
            source_uri: None,
            title: Some("LlamaParse Source".to_string()),
            change_token: None,
            metadata: json!({}),
            status: ObjectStatus::Active,
            source_updated_at: None,
            deleted_at: None,
        },
        file_name: Some("guide.pdf".to_string()),
        mime_type: Some("application/pdf".to_string()),
        source_url: Some("https://files.example/guide.pdf".to_string()),
        bytes: None,
        text: None,
        options: json!({}),
    }
}

fn webhook_signature(body: &[u8], signing_key: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes())
        .expect("test signing key should initialize HMAC");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

#[tokio::test]
async fn parse_request_preserves_items_page_metadata_timing_and_identity() {
    // Pins: LlamaParse markdown plus structured items map to stable MOA blocks/chunks offline.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/parse"))
        .and(body_string_contains("\"tier\":\"agentic\""))
        .and(body_string_contains("\"version\":\"2026-06\""))
        .and(body_string_contains(
            "\"source_url\":\"https://files.example/guide.pdf\"",
        ))
        .and(body_string_contains(
            "\"processing_options\":{\"cost_optimizer\":{\"enable\":true}}",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job_id": "job-123",
            "markdown": "# Alpha\n\nBody one\n\n| A | B |",
            "items": [
                {"id": "h1", "type": "heading", "level": 1, "text": "Alpha", "page_number": 1},
                {"id": "p1", "type": "paragraph", "text": "Body   one", "page_number": 1},
                {"id": "t1", "type": "table", "text": "| A | B |", "page_number": 2}
            ],
            "page_metadata": [{"page": 1, "width": 612, "height": 792}],
            "job_metadata": {"version": "2026-06"},
            "timing": {"parse_ms": 42},
            "version": "2026-06"
        })))
        .mount(&server)
        .await;

    let parser = LlamaParseParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "agentic",
        "2026-06",
        Vec::new(),
    );
    let parsed = parser
        .parse(input())
        .await
        .expect("parse llamaparse fixture");
    assert_eq!(parsed.parser_job_id.as_deref(), Some("job-123"));
    assert_eq!(parsed.elements[1].heading_path, vec!["Alpha"]);
    assert_eq!(parsed.elements[2].page_number, Some(2));
    assert_eq!(parsed.elements[1].metadata["parser_timing"]["parse_ms"], 42);

    let version_uid = Uuid::from_u128(24);
    let blocks = elements_to_blocks(version_uid, &parsed.elements);
    let chunks = blocks_to_chunks(
        version_uid,
        &blocks,
        ChunkingConfig {
            target_tokens: 4,
            max_tokens: 8,
            min_tokens: 1,
        },
    );
    let repeated_blocks = elements_to_blocks(version_uid, &parsed.elements);
    let repeated_chunks = blocks_to_chunks(
        version_uid,
        &repeated_blocks,
        ChunkingConfig {
            target_tokens: 4,
            max_tokens: 8,
            min_tokens: 1,
        },
    );
    assert_eq!(blocks, repeated_blocks);
    assert_eq!(chunks, repeated_chunks);
}

#[test]
fn webhook_payload_maps_job_object_and_rejects_bad_signature() {
    // Pins: LlamaParse webhook verification maps job/object metadata and rejects invalid HMAC.
    let signing_key = "llamaparse-webhook-secret";
    let object_uid = Uuid::from_u128(21).to_string();
    let body = serde_json::to_vec(&json!({
        "event_id": "evt-llama-1",
        "event_type": "parse.success",
        "job_id": "job-123",
        "status": "success",
        "metadata": {
            "object_uid": object_uid,
            "source_id": "llamaparse-source",
            "raw_text": "must not be retained"
        }
    }))
    .expect("serialize LlamaParse webhook fixture");
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-llamaparse-webhook-signature",
        HeaderValue::from_str(&webhook_signature(&body, signing_key))
            .expect("valid LlamaParse signature header"),
    );

    let event = verify_parser_webhook("llamaparse", &headers, &body, signing_key)
        .expect("valid LlamaParse webhook signature");
    assert_eq!(event.provider, "llamaparse");
    assert_eq!(event.event_id, "evt-llama-1");
    assert_eq!(event.event_type, "parse.success");
    assert_eq!(event.metadata["parser_job_id"], "job-123");
    assert_eq!(event.metadata["object_uid"], object_uid);
    assert_eq!(event.metadata["source_id"], "llamaparse-source");
    assert!(event.metadata["metadata"].get("raw_text").is_none());

    headers.insert(
        "x-llamaparse-webhook-signature",
        HeaderValue::from_static("00000000000000000000000000000000"),
    );
    let error = verify_parser_webhook("llamaparse", &headers, &body, signing_key)
        .expect_err("bad LlamaParse webhook signature should fail");
    assert!(matches!(
        error,
        Error::Parser { parser, message }
            if parser == "llamaparse" && message.contains("signature verification failed")
    ));
}

#[tokio::test]
async fn partial_success_preserves_parser_status_and_errors() {
    // Pins: LlamaParse partial-success payloads keep provider status and safe error metadata.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/parse"))
        .and(body_string_contains("\"tier\":\"agentic\""))
        .and(body_string_contains(
            "\"processing_options\":{\"cost_optimizer\":{\"enable\":true}}",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job_id": "job-partial",
            "status": "partial_success",
            "errors": [{"code": "page_timeout", "page": 2}],
            "markdown": "Recovered text",
            "items": [
                {"id": "p1", "type": "paragraph", "text": "Recovered text", "page_number": 1}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let parser = LlamaParseParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "agentic",
        "2026-06",
        Vec::new(),
    );
    let parsed = parser
        .parse(input())
        .await
        .expect("parse partial-success LlamaParse fixture");

    assert_eq!(parsed.parser_job_id.as_deref(), Some("job-partial"));
    assert_eq!(parsed.metadata["parser_status"], "partial_success");
    assert_eq!(parsed.metadata["parser_errors"][0]["code"], "page_timeout");
    assert_eq!(parsed.elements.len(), 1);
    assert_eq!(parsed.elements[0].text, "Recovered text");
}

#[tokio::test]
async fn parse_error_maps_to_typed_http_status() {
    // Pins: LlamaParse parser failures surface as typed HTTP status errors.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v2/parse"))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "detail": "parse failed"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let parser = LlamaParseParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "agentic",
        "2026-06",
        Vec::new(),
    );
    let error = parser
        .parse(input())
        .await
        .expect_err("LlamaParse HTTP error should fail");
    assert!(matches!(error, Error::HttpStatus { status: 422, .. }));
}

#[tokio::test]
async fn missing_credentials_fail_with_typed_config_error() {
    // Pins: missing LlamaParse credentials fail before transport with a typed config error.
    let parser = LlamaParseParser::with_client(
        reqwest::Client::new(),
        "https://llamaparse.invalid",
        "",
        "agentic",
        "2026-06",
        Vec::new(),
    );
    let error = parser
        .parse(input())
        .await
        .expect_err("missing key should fail");
    assert!(matches!(error, Error::Config(message) if message.contains("api_key")));
}
