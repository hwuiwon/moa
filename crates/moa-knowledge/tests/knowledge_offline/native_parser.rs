//! Native parser coverage for deterministic local structures and liteparse layout mapping.

use moa_core::types::identifiers::TenantId;
use moa_knowledge::{
    chunking::elements_to_blocks,
    domain::{ConnectionStatus, KnowledgeConnection, KnowledgeObject, ObjectStatus, ParseInput},
    parser::{
        DocumentParser,
        native::{
            NativeDocumentParser, NativeLiteparseDocument, NativeLiteparsePage,
            NativeLiteparseTextItem, map_liteparse_document,
        },
    },
};
use serde_json::json;
use uuid::Uuid;

fn object() -> KnowledgeObject {
    let tenant_id = TenantId::from(Uuid::from_u128(1));
    KnowledgeObject {
        acl: moa_knowledge::domain::ObjectAcl::incomplete(),
        object_uid: Uuid::from_u128(2),
        tenant_id,
        connection_uid: Uuid::from_u128(3),
        object_type: "document".to_string(),
        source_id: "native-source".to_string(),
        parent_source_id: None,
        source_uri: None,
        title: Some("Native Source".to_string()),
        change_token: None,
        metadata: json!({}),
        status: ObjectStatus::Active,
        source_updated_at: None,
        deleted_at: None,
    }
}

fn parse_input(file_name: &str, mime_type: &str, text: &str) -> ParseInput {
    let _connection = KnowledgeConnection {
        connection_uid: Uuid::from_u128(3),
        tenant_id: TenantId::from(Uuid::from_u128(1)),
        provider: "nango".to_string(),
        connector: "fixture".to_string(),
        provider_account_id: "fixture".to_string(),
        credential_ref: "vault://fixture".to_string(),
        status: ConnectionStatus::Active,
        metadata: json!({}),
        source_selection: json!({}),
        information_barrier: None,
        created_at: moa_test_support::fixtures::pg_now(),
        updated_at: moa_test_support::fixtures::pg_now(),
        last_synced_at: None,
    };
    ParseInput {
        object: object(),
        file_name: Some(file_name.to_string()),
        mime_type: Some(mime_type.to_string()),
        source_url: None,
        bytes: None,
        text: Some(text.to_string()),
        options: json!({}),
    }
}

#[tokio::test]
async fn markdown_html_json_and_csv_map_structure_deterministically() {
    // Pins: native text-like formats emit stable element kinds, heading paths, and block hashes.
    let parser = NativeDocumentParser::new();
    let markdown = parser
        .parse(parse_input(
            "guide.md",
            "text/markdown",
            "# Alpha\r\nIntro   text\r\n\r\n## Beta\r\n- First item\r\n| A | B |\r\n| 1 | 2 |",
        ))
        .await
        .expect("parse markdown fixture");
    assert_eq!(markdown.elements[0].heading_path, vec!["Alpha"]);
    assert_eq!(markdown.elements[2].heading_path, vec!["Alpha", "Beta"]);
    assert_eq!(markdown.elements[3].text, "First item");
    assert_eq!(markdown.elements[4].text, "| A | B |");

    let html = parser
        .parse(parse_input(
            "guide.html",
            "text/html",
            "<h1>Alpha</h1><p>One&nbsp;&amp;&nbsp;two</p><h2>Beta</h2><li>Item</li>",
        ))
        .await
        .expect("parse html fixture");
    assert_eq!(html.elements[1].heading_path, vec!["Alpha"]);
    assert_eq!(html.elements[3].heading_path, vec!["Alpha", "Beta"]);

    let json_doc = parser
        .parse(parse_input(
            "record.json",
            "application/json",
            r#"{"title":"Alpha","items":[{"name":"Beta","count":2}]}"#,
        ))
        .await
        .expect("parse json fixture");
    assert_eq!(
        json_doc
            .elements
            .iter()
            .map(|element| element.text.as_str())
            .collect::<Vec<_>>(),
        vec![
            "$.title: Alpha",
            "$.items[0].name: Beta",
            "$.items[0].count: 2"
        ]
    );

    let csv = parser
        .parse(parse_input(
            "records.csv",
            "text/csv",
            "name,notes\nAlpha,\"one, two\"\nBeta,three",
        ))
        .await
        .expect("parse csv fixture");
    assert_eq!(csv.elements[0].text, "name: Alpha | notes: one, two");
    assert_eq!(csv.elements[1].text, "name: Beta | notes: three");

    let version_uid = Uuid::from_u128(4);
    assert_eq!(
        elements_to_blocks(version_uid, &markdown.elements),
        elements_to_blocks(version_uid, &markdown.elements)
    );
}

#[test]
fn liteparse_layout_output_maps_page_dimensions_and_item_boxes() {
    // Pins: fake liteparse page/text-item layout maps into DocumentElement layout deterministically.
    let parsed = map_liteparse_document(NativeLiteparseDocument {
        text: "Page one text".to_string(),
        pages: vec![NativeLiteparsePage {
            page_number: 1,
            page_width: 612.0,
            page_height: 792.0,
            text: "Page one text".to_string(),
            text_items: vec![NativeLiteparseTextItem {
                text: "Page one text".to_string(),
                x: 72.0,
                y: 96.0,
                width: 120.0,
                height: 14.0,
                confidence: Some(0.91),
            }],
        }],
    });

    assert_eq!(parsed.elements.len(), 1);
    let element = &parsed.elements[0];
    assert_eq!(element.element_id, "liteparse:p1:0");
    assert_eq!(element.page_number, Some(1));
    let layout = element.layout.expect("liteparse layout should be present");
    assert_eq!(layout.page_width, Some(612.0));
    assert_eq!(layout.page_height, Some(792.0));
    assert_eq!(layout.confidence, Some(0.91));
    assert_eq!(element.metadata["source"], "liteparse");
}
