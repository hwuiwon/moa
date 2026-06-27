//! Live provider smoke tests for external knowledge providers.

use std::{collections::HashMap, path::PathBuf};

use moa_core::TenantId;
use moa_knowledge::{
    domain::CreateLinkTokenRequest,
    providers::{LinkedIntegrationProvider, merge::MergeProvider},
};
use uuid::Uuid;

const LIVE_FLAG: &str = "MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS";

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS=1 and MERGE_API_KEY"]
async fn merge_live_creates_link_token() {
    // Pins: the Merge adapter can create a hosted link token against the live API.
    require_live_flag();
    let api_key = required_secret("MERGE_API_KEY");
    let provider = MergeProvider::new("https://api.merge.dev", api_key)
        .expect("merge provider should initialize");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let account_id = format!("moa-live-{}", Uuid::now_v7());

    let token = provider
        .create_link_token(CreateLinkTokenRequest {
            tenant_id,
            connector: "crm".to_string(),
            external_account_id: Some(account_id.clone()),
            end_user_email_address: Some(format!("{account_id}@example.com")),
            redirect_url: Some("https://example.com/merge/callback".to_string()),
        })
        .await
        .expect("merge live link token creation should succeed");

    assert_eq!(token.provider, "merge");
    assert!(!token.token.trim().is_empty());
}

fn require_live_flag() {
    assert_eq!(
        std::env::var(LIVE_FLAG).as_deref(),
        Ok("1"),
        "{LIVE_FLAG}=1 is required for live provider tests"
    );
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
