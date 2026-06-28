//! Live provider smoke tests for external knowledge providers.

use std::{collections::HashMap, path::PathBuf};

use moa_core::TenantId;
use moa_knowledge::{
    domain::CreateLinkTokenRequest,
    providers::{LinkedIntegrationProvider, merge::MergeProvider, nango::NangoProvider},
};
use serde_json::json;
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
            source_selection: json!({}),
        })
        .await
        .expect("merge live link token creation should succeed");

    assert_eq!(token.provider, "merge");
    assert!(!token.token.trim().is_empty());
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_KNOWLEDGE_PROVIDER_TESTS=1, NANGO_API_KEY, and a configured Nango provider config key"]
async fn nango_live_creates_link_token() {
    // Pins: the Nango adapter can create a connect-session link token against the live API.
    require_live_flag();
    let api_key = required_secret("NANGO_API_KEY");
    let connector =
        optional_secret("NANGO_PROVIDER_CONFIG_KEY").unwrap_or_else(|| "google-drive".to_string());
    let provider = NangoProvider::new("https://api.nango.dev", api_key)
        .expect("nango provider should initialize");
    let tenant_id = TenantId::from(Uuid::now_v7());
    let account_id = format!("moa-live-{}", Uuid::now_v7());

    let token = provider
        .create_link_token(CreateLinkTokenRequest {
            tenant_id,
            connector,
            external_account_id: Some(account_id),
            end_user_email_address: None,
            redirect_url: Some("https://example.com/nango/callback".to_string()),
            source_selection: json!({
                "metadata": {
                    "selected_folder_ids": ["moa-live-smoke"]
                }
            }),
        })
        .await
        .expect("nango live link token creation should succeed");

    assert_eq!(token.provider, "nango");
    assert!(!token.token.trim().is_empty());
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
        "{LIVE_FLAG}=1 is required for live provider tests"
    );
}

fn required_secret(name: &str) -> String {
    optional_secret(name).unwrap_or_else(|| panic!("{name} must be set when {LIVE_FLAG}=1"))
}

fn optional_secret(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .and_then(non_empty)
        .or_else(|| dotenv_values().remove(name).and_then(non_empty))
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
