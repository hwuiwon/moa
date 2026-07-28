//! Offline Reducto parser coverage.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use base64::{Engine as _, engine::general_purpose};
use hmac::{Hmac, Mac};
use moa_core::types::identifiers::TenantId;
use moa_knowledge::{
    Error,
    chunking::{ChunkingConfig, blocks_to_chunks, elements_to_blocks},
    domain::{KnowledgeObject, ObjectStatus, ParseInput},
    parser::{DocumentParser, reducto::ReductoParser, verify_parser_webhook},
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::json;
use sha2::Sha256;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

struct QueuedThenCompletedJobResponder {
    lookups: Arc<AtomicUsize>,
    result_url: String,
}

impl Respond for QueuedThenCompletedJobResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.lookups.fetch_add(1, Ordering::SeqCst) == 0 {
            return ResponseTemplate::new(200).set_body_json(json!({
                "job_id": "job-async",
                "status": "queued"
            }));
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "status": "completed",
            "result": {
                "response_type": "parse",
                "job_id": "job-async",
                "duration": 321,
                "studio_link": "https://studio.reducto.test/job-async",
                "usage": {"num_pages": 1, "credits": 1},
                "result": {
                    "type": "url",
                    "url": self.result_url
                },
                "parse_mode": "standard"
            }
        }))
    }
}

fn input(result_url: Option<String>) -> ParseInput {
    ParseInput {
        object: KnowledgeObject {
            acl: moa_knowledge::domain::ObjectAcl::incomplete(),
            object_uid: Uuid::from_u128(41),
            tenant_id: TenantId::from(Uuid::from_u128(42)),
            connection_uid: Uuid::from_u128(43),
            object_type: "document".to_string(),
            source_id: "reducto-source".to_string(),
            parent_source_id: None,
            source_uri: None,
            title: Some("Reducto Source".to_string()),
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
        options: json!({
            "file_id": "file-123",
            "presigned_url": result_url
        }),
    }
}

fn svix_signature(body: &[u8], signing_key: &str, message_id: &str, timestamp: &str) -> String {
    let payload = format!(
        "{message_id}.{timestamp}.{}",
        std::str::from_utf8(body).expect("webhook fixture body should be UTF-8")
    );
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes())
        .expect("test signing key should initialize HMAC");
    mac.update(payload.as_bytes());
    format!(
        "v1,{}",
        general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    )
}

#[tokio::test]
async fn url_result_chunks_and_blocks_preserve_metadata_and_identity() {
    // Pins: Reducto URL-result chunks/blocks map deterministically and keep parser metadata as metadata.
    let server = MockServer::start().await;
    let result_url = format!("{}/large-result.json", server.uri());
    Mock::given(method("POST"))
        .and(path("/parse"))
        .and(body_string_contains("\"input\":\"file-123\""))
        .and(body_string_contains("\"chunk_mode\":\"variable\""))
        .and(body_string_contains("\"force_url_result\":true"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job_id": "job-red",
            "result": {
                "type": "url",
                "url": result_url
            }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/large-result.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job_id": "job-red",
            "parse_mode": "standard",
            "processing_duration": 1234,
            "usage": {"pages": 2, "credits": 3},
            "studio_link": "https://studio.reducto.test/job-red",
            "chunks": [{
                "id": "chunk-1",
                "content": "Chunk content",
                "embedding_optimized_content": "Embedding chunk content",
                "page_number": 1,
                "metadata": {"section": "alpha"},
                "blocks": [
                    {
                        "id": "heading-1",
                        "type": "heading",
                        "content": "Alpha",
                        "bounding_box": {"page": 1, "x": 10, "y": 20, "width": 100, "height": 20, "page_width": 612, "page_height": 792},
                        "confidence": 0.98
                    },
                    {
                        "id": "body-1",
                        "block_type": "paragraph",
                        "text": "Body   one",
                        "bbox": {"page": 1, "x": 12, "y": 50, "width": 300, "height": 60, "page_width": 612, "page_height": 792},
                        "confidence": 0.87
                    }
                ]
            }]
        })))
        .mount(&server)
        .await;

    let parser = ReductoParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "standard",
        false,
        "variable",
        true,
    );
    let parsed = parser
        .parse(input(Some(
            "https://presigned.example/guide.pdf".to_string(),
        )))
        .await
        .expect("parse reducto fixture");
    assert_eq!(parsed.parser_job_id.as_deref(), Some("job-red"));
    assert_eq!(parsed.metadata["usage_pages"], 2);
    assert_eq!(parsed.metadata["usage_credits"], 3);
    assert_eq!(
        parsed.elements[0].metadata["embedding_content"],
        "Embedding chunk content"
    );
    assert_eq!(parsed.elements[2].heading_path, vec!["Alpha"]);
    assert_eq!(parsed.elements[2].metadata["block_content"], "Body   one");
    let layout = parsed.elements[2].layout.expect("block bbox should map");
    assert_eq!(layout.page_width, Some(612.0));
    assert_eq!(layout.page_height, Some(792.0));
    assert_eq!(layout.confidence, Some(0.87));

    let version_uid = Uuid::from_u128(44);
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
    assert_eq!(blocks, elements_to_blocks(version_uid, &parsed.elements));
    assert_eq!(
        chunks,
        blocks_to_chunks(
            version_uid,
            &blocks,
            ChunkingConfig {
                target_tokens: 4,
                max_tokens: 8,
                min_tokens: 1,
            },
        )
    );
}

#[test]
fn svix_webhook_payload_maps_job_object_and_rejects_bad_signature() {
    // Pins: Reducto Svix webhook verification maps job/object metadata and rejects invalid signatures.
    let signing_key = "reducto-svix-secret";
    let message_id = "msg_reducto_1";
    let timestamp = moa_test_support::fixtures::pg_now().timestamp().to_string();
    let object_uid = Uuid::from_u128(41).to_string();
    let body = serde_json::to_vec(&json!({
        "event": "parse.completed",
        "data": {
            "id": "job-red",
            "status": "completed",
            "metadata": {
                "object_uid": object_uid,
                "source_id": "reducto-source",
                "document_text": "must not be retained"
            }
        }
    }))
    .expect("serialize Reducto webhook fixture");
    let mut headers = HeaderMap::new();
    headers.insert(
        "svix-id",
        HeaderValue::from_str(message_id).expect("valid Svix id header"),
    );
    headers.insert(
        "svix-timestamp",
        HeaderValue::from_str(&timestamp).expect("valid Svix timestamp header"),
    );
    headers.insert(
        "svix-signature",
        HeaderValue::from_str(&svix_signature(&body, signing_key, message_id, &timestamp))
            .expect("valid Svix signature header"),
    );

    let event = verify_parser_webhook("reducto", &headers, &body, signing_key)
        .expect("valid Reducto Svix webhook signature");
    assert_eq!(event.provider, "reducto");
    assert_eq!(event.event_id, "job-red");
    assert_eq!(event.event_type, "parse.completed");
    assert_eq!(event.metadata["parser_job_id"], "job-red");
    assert_eq!(event.metadata["object_uid"], object_uid);
    assert_eq!(event.metadata["source_id"], "reducto-source");
    assert!(
        event.metadata["data_metadata"]
            .get("document_text")
            .is_none()
    );

    headers.insert("svix-signature", HeaderValue::from_static("v1,AAAA"));
    let error = verify_parser_webhook("reducto", &headers, &body, signing_key)
        .expect_err("bad Reducto Svix signature should fail");
    assert!(matches!(
        error,
        Error::Parser { parser, message }
            if parser == "reducto" && message.contains("signature verification failed")
    ));
}

#[tokio::test]
async fn async_job_retrieval_preserves_parser_status_and_metadata() {
    // Pins: Reducto async parse jobs retrieve job results and keep parser status metadata.
    let server = MockServer::start().await;
    let job_lookups = Arc::new(AtomicUsize::new(0));
    let result_url = format!("{}/async-result.json", server.uri());
    Mock::given(method("POST"))
        .and(path("/parse_async"))
        .and(body_string_contains("\"input\":\"file-123\""))
        .and(body_string_contains("\"chunk_mode\":\"variable\""))
        .and(body_string_contains("\"force_url_result\":false"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job_id": "job-async",
            "status": "queued"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/job/job-async"))
        .respond_with(QueuedThenCompletedJobResponder {
            lookups: Arc::clone(&job_lookups),
            result_url,
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/async-result.json"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunks": [{
                "id": "chunk-async",
                "content": "Async chunk",
                "blocks": [{
                    "id": "body-async",
                    "block_type": "paragraph",
                    "text": "Async body"
                }]
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let parser = ReductoParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "standard",
        true,
        "variable",
        false,
    );
    let parsed = parser
        .parse(input(None))
        .await
        .expect("parse Reducto async fixture");

    assert_eq!(parsed.parser_job_id.as_deref(), Some("job-async"));
    assert_eq!(parsed.metadata["parser_status"], "completed");
    assert_eq!(parsed.metadata["processing_duration"], 321);
    assert_eq!(
        parsed.metadata["studio_link"],
        "https://studio.reducto.test/job-async"
    );
    assert_eq!(parsed.metadata["usage_pages"], 1);
    assert_eq!(parsed.elements.len(), 2);
    assert_eq!(parsed.elements[1].text, "Async body");
    assert_eq!(
        job_lookups.load(Ordering::SeqCst),
        2,
        "parser should poll until Reducto reports a terminal async status"
    );
}

#[tokio::test]
async fn parser_error_maps_to_typed_http_status() {
    // Pins: Reducto parser failures surface as typed HTTP status errors.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/parse_async"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job_id": "job-error"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/job/job-error"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "job failed"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let parser = ReductoParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "standard",
        true,
        "variable",
        false,
    );
    let error = parser
        .parse(input(None))
        .await
        .expect_err("Reducto HTTP error should fail");
    assert!(matches!(error, Error::HttpStatus { status: 500, .. }));
}

#[tokio::test]
async fn missing_credentials_fail_with_typed_config_error() {
    // Pins: missing Reducto credentials fail before transport with a typed config error.
    let parser = ReductoParser::with_client(
        reqwest::Client::new(),
        "https://reducto.invalid",
        "",
        "standard",
        false,
        "variable",
        true,
    );
    let error = parser
        .parse(input(None))
        .await
        .expect_err("missing key should fail");
    assert!(matches!(error, Error::Config(message) if message.contains("api_key")));
}
