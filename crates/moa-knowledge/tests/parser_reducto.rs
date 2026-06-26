//! Offline Reducto parser coverage.

use moa_core::TenantId;
use moa_knowledge::{
    Error,
    chunking::{ChunkingConfig, blocks_to_chunks, elements_to_blocks},
    domain::{KnowledgeObject, ObjectStatus, ParseInput},
    parser::{DocumentParser, reducto::ReductoParser},
};
use serde_json::json;
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

fn input(result_url: Option<String>) -> ParseInput {
    ParseInput {
        object: KnowledgeObject {
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

#[tokio::test]
async fn url_result_chunks_and_blocks_preserve_metadata_and_identity() {
    // Pins: Reducto URL-result chunks/blocks map deterministically and keep parser metadata as metadata.
    let server = MockServer::start().await;
    let result_url = format!("{}/large-result.json", server.uri());
    Mock::given(method("POST"))
        .and(path("/parse"))
        .and(body_string_contains(
            "\"document_url\":\"https://files.example/guide.pdf\"",
        ))
        .and(body_string_contains("\"file_id\":\"file-123\""))
        .and(body_string_contains("\"mode\":\"standard\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "job_id": "job-red",
            "result_url": result_url
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
