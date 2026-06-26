//! Offline Unstructured parser coverage.

use moa_core::TenantId;
use moa_knowledge::{
    Error,
    chunking::{ChunkingConfig, blocks_to_chunks, elements_to_blocks},
    domain::{KnowledgeObject, ObjectStatus, ParseInput},
    parser::{DocumentParser, unstructured::UnstructuredParser},
};
use serde_json::{Value, json};
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

fn input() -> ParseInput {
    ParseInput {
        object: KnowledgeObject {
            object_uid: Uuid::from_u128(31),
            tenant_id: TenantId::from(Uuid::from_u128(32)),
            connection_uid: Uuid::from_u128(33),
            object_type: "document".to_string(),
            source_id: "unstructured-source".to_string(),
            parent_source_id: None,
            source_uri: None,
            title: Some("Unstructured Source".to_string()),
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
        options: json!({"chunking_options": {"max_characters": 4000}}),
    }
}

#[tokio::test]
async fn partition_elements_preserve_parent_filetype_source_coordinates_and_identity() {
    // Pins: Unstructured elements are parser structure, not final MOA chunks, and map deterministically.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/general/v0/general"))
        .and(body_string_contains("\"strategy\":\"hi_res\""))
        .and(body_string_contains("\"chunking_strategy\":\"by_title\""))
        .and(|request: &wiremock::Request| {
            request
                .body_json::<Value>()
                .ok()
                .and_then(|body| {
                    body.pointer("/chunking_options/max_characters")
                        .and_then(Value::as_u64)
                })
                == Some(4000)
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "element_id": "title-1",
                "type": "Title",
                "text": "Alpha",
                "metadata": {
                    "page_number": 1,
                    "filetype": "application/pdf",
                    "source": "drive",
                    "coordinates": {"points": [[10, 20], [110, 20], [110, 40], [10, 40]], "layout_width": 612, "layout_height": 792}
                }
            },
            {
                "element_id": "body-1",
                "type": "NarrativeText",
                "text": "Body   one",
                "metadata": {"page_number": 1, "parent_id": "title-1", "filetype": "application/pdf", "source": "drive"}
            },
            {
                "element_id": "chunk-1",
                "type": "CompositeElement",
                "text": "Provider chunk text",
                "metadata": {"page_number": 2, "parent_id": "title-1"}
            }
        ])))
        .mount(&server)
        .await;

    let parser = UnstructuredParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "hi_res",
        "by_title",
    );
    let parsed = parser
        .parse(input())
        .await
        .expect("parse unstructured fixture");
    assert_eq!(parsed.elements[1].heading_path, vec!["Alpha"]);
    assert_eq!(parsed.elements[1].metadata["parent_id"], "title-1");
    assert_eq!(parsed.elements[1].metadata["filetype"], "application/pdf");
    assert_eq!(parsed.elements[1].metadata["source"], "drive");
    assert_eq!(parsed.elements[2].metadata["parser_chunk"], true);
    let layout = parsed.elements[0].layout.expect("coordinates should map");
    assert_eq!(layout.page_width, Some(612.0));
    assert_eq!(layout.page_height, Some(792.0));

    let version_uid = Uuid::from_u128(34);
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
async fn object_response_preserves_parser_status_and_warnings() {
    // Pins: Unstructured object responses keep parser status/warnings while mapping elements.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/general/v0/general"))
        .and(body_string_contains("\"strategy\":\"hi_res\""))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "completed_with_warnings",
            "warnings": [{"code": "low_confidence"}],
            "elements": [
                {
                    "element_id": "body-1",
                    "type": "NarrativeText",
                    "text": "Recovered body",
                    "metadata": {"page_number": 1}
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let parser = UnstructuredParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "hi_res",
        "by_title",
    );
    let parsed = parser
        .parse(input())
        .await
        .expect("parse Unstructured status fixture");

    assert_eq!(parsed.metadata["parser_status"], "completed_with_warnings");
    assert_eq!(
        parsed.metadata["parser_warnings"][0]["code"],
        "low_confidence"
    );
    assert_eq!(parsed.elements.len(), 1);
    assert_eq!(parsed.elements[0].text, "Recovered body");
}

#[tokio::test]
async fn parser_error_maps_to_typed_http_status() {
    // Pins: Unstructured parser failures surface as typed HTTP status errors.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/general/v0/general"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error": "partition failed"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let parser = UnstructuredParser::with_client(
        reqwest::Client::new(),
        server.uri(),
        "test-key",
        "hi_res",
        "by_title",
    );
    let error = parser
        .parse(input())
        .await
        .expect_err("Unstructured HTTP error should fail");
    assert!(matches!(error, Error::HttpStatus { status: 500, .. }));
}

#[tokio::test]
async fn missing_credentials_fail_with_typed_config_error() {
    // Pins: missing Unstructured credentials fail before transport with a typed config error.
    let parser = UnstructuredParser::with_client(
        reqwest::Client::new(),
        "https://unstructured.invalid",
        "",
        "hi_res",
        "by_title",
    );
    let error = parser
        .parse(input())
        .await
        .expect_err("missing key should fail");
    assert!(matches!(error, Error::Config(message) if message.contains("api_key")));
}
