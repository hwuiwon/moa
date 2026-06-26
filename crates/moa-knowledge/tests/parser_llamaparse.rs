//! Offline LlamaParse parser coverage.

use moa_core::TenantId;
use moa_knowledge::{
    Error,
    chunking::{ChunkingConfig, blocks_to_chunks, elements_to_blocks},
    domain::{KnowledgeObject, ObjectStatus, ParseInput},
    parser::{DocumentParser, llamaparse::LlamaParseParser},
};
use serde_json::json;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

fn input() -> ParseInput {
    ParseInput {
        object: KnowledgeObject {
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

#[tokio::test]
async fn parse_request_preserves_items_page_metadata_timing_and_identity() {
    // Pins: LlamaParse markdown plus structured items map to stable MOA blocks/chunks offline.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/parsing/job"))
        .and(body_string_contains("\"tier\":\"premium\""))
        .and(body_string_contains("\"version\":\"2026-06\""))
        .and(body_string_contains("page_metadata"))
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
        "premium",
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

#[tokio::test]
async fn missing_credentials_fail_with_typed_config_error() {
    // Pins: missing LlamaParse credentials fail before transport with a typed config error.
    let parser = LlamaParseParser::with_client(
        reqwest::Client::new(),
        "https://llamaparse.invalid",
        "",
        "premium",
        "2026-06",
        Vec::new(),
    );
    let error = parser
        .parse(input())
        .await
        .expect_err("missing key should fail");
    assert!(matches!(error, Error::Config(message) if message.contains("api_key")));
}
