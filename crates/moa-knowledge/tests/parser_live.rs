//! Live parser smoke tests for external knowledge parsers.

use std::{collections::HashMap, path::PathBuf};

use moa_core::types::identifiers::TenantId;
use moa_knowledge::{
    domain::{KnowledgeObject, ObjectStatus, ParseInput},
    parser::{
        DocumentParser, llamaparse::LlamaParseParser, reducto::ReductoParser,
        unstructured::UnstructuredParser,
    },
};
use serde_json::json;
use uuid::Uuid;

const LIVE_FLAG: &str = "MOA_RUN_LIVE_KNOWLEDGE_PARSER_TESTS";
const SMALL_PDF_URL: &str =
    "https://www.w3.org/WAI/ER/tests/xhtml/testfiles/resources/pdf/dummy.pdf";

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PARSER_TESTS=1 and LLAMAPARSE_API_KEY"]
async fn llamaparse_live_parses_public_pdf() {
    // Pins: the LlamaParse adapter works against the live provider API.
    require_live_flag();
    let api_key = required_secret("LLAMAPARSE_API_KEY");
    let parser = LlamaParseParser::new(
        "https://api.cloud.llamaindex.ai",
        api_key,
        "agentic",
        vec![
            "markdown".to_string(),
            "items".to_string(),
            "metadata".to_string(),
            "job_metadata".to_string(),
        ],
    )
    .expect("llamaparse parser should initialize");

    let parsed = parser
        .parse(input("llamaparse-live", "llamaparse-live.pdf"))
        .await
        .expect("llamaparse live parse should succeed");

    assert_eq!(parsed.parser, "llamaparse");
    assert!(parsed.parser_job_id.is_some());
    assert!(
        !parsed.text.trim().is_empty() || !parsed.elements.is_empty(),
        "llamaparse live parse should return text or elements"
    );
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PARSER_TESTS=1 and REDUCTO_API_KEY"]
async fn reducto_live_parses_public_pdf() {
    // Pins: the Reducto adapter works against the live provider API.
    require_live_flag();
    let api_key = required_secret("REDUCTO_API_KEY");
    let parser = ReductoParser::new(
        "https://platform.reducto.ai",
        api_key,
        "standard",
        true,
        "variable",
        true,
    )
    .expect("reducto parser should initialize");

    let parsed = parser
        .parse(input("reducto-live", "reducto-live.pdf"))
        .await
        .expect("reducto live parse should succeed");

    assert_eq!(parsed.parser, "reducto");
    assert!(
        !parsed.text.trim().is_empty() || !parsed.elements.is_empty(),
        "reducto live parse should return text or elements"
    );
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PARSER_TESTS=1 and UNSTRUCTURED_API_KEY"]
async fn unstructured_live_partitions_public_pdf() {
    // Pins: the Unstructured adapter works against the live provider API.
    require_live_flag();
    let api_key = required_secret("UNSTRUCTURED_API_KEY");
    let parser = UnstructuredParser::new(
        "https://api.unstructuredapp.io",
        api_key,
        "auto",
        "by_title",
    )
    .expect("unstructured parser should initialize");

    let parsed = parser
        .parse(input("unstructured-live", "unstructured-live.pdf"))
        .await
        .expect("unstructured live parse should succeed");

    assert_eq!(parsed.parser, "unstructured");
    assert!(
        !parsed.text.trim().is_empty() || !parsed.elements.is_empty(),
        "unstructured live parse should return text or elements"
    );
}

/// Returns `true` when `name` is set to a common truthy value (`1`, `true`,
/// `yes`, or `on`, case-insensitively after trimming), matching how live-test
/// flags are written in a developer's `.env`.
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn require_live_flag() {
    assert!(
        env_flag_enabled(LIVE_FLAG),
        "{LIVE_FLAG}=1 is required for live parser tests"
    );
}

fn input(source_id: &str, file_name: &str) -> ParseInput {
    ParseInput {
        object: KnowledgeObject {
            acl: moa_knowledge::domain::ObjectAcl::incomplete(),
            object_uid: Uuid::now_v7(),
            tenant_id: TenantId::from(Uuid::now_v7()),
            connection_uid: Uuid::now_v7(),
            object_type: "document".to_string(),
            source_id: source_id.to_string(),
            parent_source_id: None,
            source_uri: Some(SMALL_PDF_URL.to_string()),
            title: Some(file_name.to_string()),
            change_token: None,
            metadata: json!({ "mime_type": "application/pdf" }),
            status: ObjectStatus::Active,
            source_updated_at: None,
            deleted_at: None,
        },
        file_name: Some(file_name.to_string()),
        mime_type: Some("application/pdf".to_string()),
        source_url: Some(SMALL_PDF_URL.to_string()),
        bytes: None,
        text: None,
        options: json!({}),
    }
}

fn required_secret(name: &str) -> String {
    std::env::var(name)
        .ok()
        .and_then(non_empty)
        .or_else(|| dotenv_values().remove(name).and_then(non_empty))
        .unwrap_or_else(|| panic!("{name} must be set when {LIVE_FLAG}=1"))
}

fn dotenv_values() -> HashMap<String, String> {
    let path = repo_root().join(".env");
    let Ok(contents) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    contents.lines().filter_map(parse_dotenv_line).collect()
}

fn parse_dotenv_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, value) = trimmed.split_once('=')?;
    Some((key.trim().to_string(), unquote(value.trim()).to_string()))
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .expect("crate should live under crates/moa-knowledge")
        .to_path_buf()
}
