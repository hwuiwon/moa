//! Live Postmark provider E2E coverage using local `.env` credentials.

#![cfg(feature = "postmark")]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use moa_core::{
    config::MessagingConfig, error::MoaError, traits::CredentialVault, types::model::Credential,
};
use moa_messaging::{
    POSTMARK_SERVER_API_TOKEN_ENV, POSTMARK_SERVER_TOKEN_SERVICE, POSTMARK_TEST_TOKEN,
    PostmarkEmailClient, PostmarkEmailMessage,
};

const LIVE_FLAG_ENV: &str = "MOA_RUN_LIVE_POSTMARK_TESTS";
const POSTMARK_TEST_FROM_ENV: &str = "POSTMARK_TEST_FROM";
const POSTMARK_TEST_TO_ENV: &str = "POSTMARK_TEST_TO";
const POSTMARK_TEST_MESSAGE_STREAM_ENV: &str = "POSTMARK_TEST_MESSAGE_STREAM";
const DEFAULT_TEST_FROM: &str = "sender@example.com";
const DEFAULT_TEST_TO: &str = "receiver@example.com";
const TEST_SCOPE: &str = "postmark-provider-e2e";

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_POSTMARK_TESTS=1 and POSTMARK_SERVER_API_TOKEN"]
async fn postmark_provider_e2e_sends_email_using_local_env_token() {
    let Some(env) = LocalPostmarkEnv::load() else {
        return;
    };
    let vault = Arc::new(SingleCredentialVault::new(Credential::Bearer(env.token)));
    let config = MessagingConfig {
        postmark_base_url: env.base_url,
        postmark_message_stream: env.message_stream,
        ..MessagingConfig::default()
    };
    let client = PostmarkEmailClient::from_vault(vault, TEST_SCOPE, &config)
        .await
        .expect("Postmark e2e client should load the local token through CredentialVault");
    let subject = format!("MOA Postmark e2e {}", Utc::now().to_rfc3339());
    let message = PostmarkEmailMessage::new(env.from, env.to, subject)
        .with_text_body("MOA live Postmark e2e validation.")
        .with_html_body("<p>MOA live Postmark e2e validation.</p>")
        .with_tag("moa-postmark-e2e")
        .with_metadata("source", "moa-messaging");

    let result = client
        .send_email(&message)
        .await
        .expect("Postmark e2e send should be accepted");

    assert_eq!(result.error_code, 0);
    assert!(
        !result.message_id.trim().is_empty(),
        "Postmark should return a message id"
    );
    assert!(
        !result.to.trim().is_empty(),
        "Postmark should echo recipient information"
    );
}

#[derive(Debug)]
struct LocalPostmarkEnv {
    token: String,
    from: String,
    to: String,
    base_url: String,
    message_stream: String,
}

impl LocalPostmarkEnv {
    fn load() -> Option<Self> {
        if !local_env_bool(LIVE_FLAG_ENV) {
            return None;
        }

        let token = required_local_env(POSTMARK_SERVER_API_TOKEN_ENV);
        let using_test_token = token == POSTMARK_TEST_TOKEN;
        let from = optional_local_env(POSTMARK_TEST_FROM_ENV).unwrap_or_else(|| {
            assert!(
                using_test_token,
                "{LIVE_FLAG_ENV}=1 with a real Postmark token requires {POSTMARK_TEST_FROM_ENV}"
            );
            DEFAULT_TEST_FROM.to_string()
        });
        let to = optional_local_env(POSTMARK_TEST_TO_ENV).unwrap_or_else(|| {
            assert!(
                using_test_token,
                "{LIVE_FLAG_ENV}=1 with a real Postmark token requires {POSTMARK_TEST_TO_ENV}"
            );
            DEFAULT_TEST_TO.to_string()
        });
        let defaults = MessagingConfig::default();
        let base_url = optional_local_env("MOA_MESSAGING_POSTMARK_BASE_URL")
            .unwrap_or(defaults.postmark_base_url);
        let message_stream = optional_local_env(POSTMARK_TEST_MESSAGE_STREAM_ENV)
            .or_else(|| optional_local_env("MOA_MESSAGING_POSTMARK_MESSAGE_STREAM"))
            .unwrap_or(defaults.postmark_message_stream);

        Some(Self {
            token,
            from,
            to,
            base_url,
            message_stream,
        })
    }
}

#[derive(Debug)]
struct SingleCredentialVault {
    credential: Credential,
}

impl SingleCredentialVault {
    fn new(credential: Credential) -> Self {
        Self { credential }
    }
}

#[async_trait]
impl CredentialVault for SingleCredentialVault {
    async fn get(&self, service: &str, scope: &str) -> moa_core::error::Result<Credential> {
        if service == POSTMARK_SERVER_TOKEN_SERVICE && scope == TEST_SCOPE {
            return Ok(self.credential.clone());
        }
        Err(MoaError::StorageError(format!(
            "missing credential {service} for {scope}"
        )))
    }

    async fn set(
        &self,
        _service: &str,
        _scope: &str,
        _cred: Credential,
    ) -> moa_core::error::Result<()> {
        Err(MoaError::StorageError(
            "Postmark e2e vault is read-only".to_string(),
        ))
    }

    async fn delete(&self, _service: &str, _scope: &str) -> moa_core::error::Result<bool> {
        Err(MoaError::StorageError(
            "Postmark e2e vault is read-only".to_string(),
        ))
    }

    async fn list(
        &self,
        _service_prefix: &str,
    ) -> moa_core::error::Result<Vec<moa_core::traits::StoredCredentialMetadata>> {
        Err(MoaError::StorageError(
            "Postmark e2e vault does not support listing".to_string(),
        ))
    }
}

fn local_env_bool(name: &str) -> bool {
    // Accept the common truthy spellings (`1`, `true`, `yes`, `on`) so a
    // developer's `.env` enables the live lane regardless of casing/spacing.
    optional_local_env(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn required_local_env(name: &str) -> String {
    optional_local_env(name).unwrap_or_else(|| panic!("{LIVE_FLAG_ENV}=1 requires {name}"))
}

fn optional_local_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .and_then(non_empty)
        .or_else(|| dotenv_values().remove(name).and_then(non_empty))
}

fn dotenv_values() -> HashMap<String, String> {
    let path = repo_root().join(".env");
    let Ok(contents) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    contents.lines().filter_map(parse_dotenv_line).collect()
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crate manifest should live two levels below repo root")
        .to_path_buf()
}

fn parse_dotenv_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let assignment = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (key, value) = assignment.split_once('=')?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        return None;
    }
    Some((key.to_string(), unquote(value.trim())))
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
