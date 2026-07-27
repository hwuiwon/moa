//! Live Twilio SMS provider E2E coverage using local `.env` credentials.

#![cfg(feature = "twilio")]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use moa_config::MessagingConfig;
use moa_core::types::credentials::{DeploymentSecret, DeploymentSecrets};
use moa_messaging::{
    TWILIO_ACCOUNT_SID_ENV, TWILIO_API_KEY_SECRET_ENV, TWILIO_API_KEY_SID_ENV,
    TWILIO_AUTH_TOKEN_ENV, TWILIO_FROM_NUMBER_ENV, TWILIO_MESSAGING_SERVICE_SID_ENV,
    TwilioSmsClient, TwilioSmsMessage, TwilioSmsSendResult,
};
use tokio::time::sleep;

const LIVE_FLAG_ENV: &str = "MOA_RUN_LIVE_TWILIO_TESTS";
const TWILIO_API_KEY_ENV: &str = "TWILIO_API_KEY";
const TWILIO_API_SECRET_ENV: &str = "TWILIO_API_SECRET";
const TWILIO_TEST_TO_ENV: &str = "TWILIO_TEST_TO";
const STATUS_POLL_ATTEMPTS: usize = 15;
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(2);

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_TWILIO_TESTS=1 and Twilio credentials"]
async fn twilio_provider_e2e_sends_sms_to_configured_test_number() {
    let Some(env) = LocalTwilioEnv::load() else {
        return;
    };
    let test_to = env.test_to.clone();
    let secrets = env.into_deployment_secrets();
    let config = MessagingConfig {
        twilio_base_url: optional_local_env("MOA_MESSAGING_TWILIO_BASE_URL")
            .unwrap_or_else(|| MessagingConfig::default().twilio_base_url),
        ..MessagingConfig::default()
    };
    let client = TwilioSmsClient::from_deployment_secrets(&secrets, &config)
        .expect("Twilio e2e client should load local credentials as deployment secrets")
        .expect("the live lane configures an account sid");
    let message = TwilioSmsMessage::new(
        test_to.clone(),
        format!(
            "MOA Twilio SMS e2e {}",
            chrono::DateTime::<chrono::Utc>::from_timestamp_micros(
                chrono::Utc::now().timestamp_micros()
            )
            .expect("microsecond timestamp")
            .to_rfc3339()
        ),
    );

    let initial = client
        .send_sms(&message)
        .await
        .expect("Twilio e2e SMS send should be accepted");
    let result = wait_for_sms_handoff_or_delivery(&client, initial).await;

    assert!(
        result.sid.starts_with("SM") || result.sid.starts_with("MM"),
        "Twilio should return a message SID, got {}",
        result.sid
    );
    assert!(
        matches!(result.status.as_str(), "sent" | "delivered"),
        "Twilio message was not handed to the carrier or delivered: sid={}, status={}, error_code={:?}, error_message={:?}",
        result.sid,
        result.status,
        result.error_code,
        result.error_message
    );
    assert_eq!(result.to, test_to);
}

async fn wait_for_sms_handoff_or_delivery(
    client: &TwilioSmsClient,
    initial: TwilioSmsSendResult,
) -> TwilioSmsSendResult {
    let mut latest = initial;
    for _ in 0..STATUS_POLL_ATTEMPTS {
        if matches!(
            latest.status.as_str(),
            "sent" | "delivered" | "failed" | "undelivered" | "canceled"
        ) {
            return latest;
        }
        sleep(STATUS_POLL_INTERVAL).await;
        latest = client
            .fetch_sms(&latest.sid)
            .await
            .expect("Twilio e2e status fetch should succeed");
    }
    latest
}

#[derive(Debug)]
struct LocalTwilioEnv {
    account_sid: String,
    auth_token: Option<String>,
    api_key_sid: Option<String>,
    api_key_secret: Option<String>,
    from_number: Option<String>,
    messaging_service_sid: Option<String>,
    test_to: String,
}

impl LocalTwilioEnv {
    fn load() -> Option<Self> {
        if !local_env_bool(LIVE_FLAG_ENV) {
            return None;
        }

        let test_to = optional_local_env(TWILIO_TEST_TO_ENV)
            .map(|value| normalize_us_test_number(&value))
            .filter(|value| !value.trim().is_empty())?;
        let account_sid = required_local_env(TWILIO_ACCOUNT_SID_ENV);
        let auth_token = optional_local_env(TWILIO_AUTH_TOKEN_ENV);
        let api_key_sid = optional_local_env(TWILIO_API_KEY_SID_ENV)
            .or_else(|| optional_local_env(TWILIO_API_KEY_ENV));
        let api_key_secret = optional_local_env(TWILIO_API_KEY_SECRET_ENV)
            .or_else(|| optional_local_env(TWILIO_API_SECRET_ENV));
        assert!(
            auth_token.is_some() || (api_key_sid.is_some() && api_key_secret.is_some()),
            "{LIVE_FLAG_ENV}=1 requires {TWILIO_AUTH_TOKEN_ENV} or {TWILIO_API_KEY_SID_ENV}/{TWILIO_API_KEY_SECRET_ENV}"
        );

        let from_number = optional_local_env(TWILIO_FROM_NUMBER_ENV);
        let messaging_service_sid = optional_local_env(TWILIO_MESSAGING_SERVICE_SID_ENV);
        assert!(
            from_number.is_some() || messaging_service_sid.is_some(),
            "{LIVE_FLAG_ENV}=1 requires {TWILIO_FROM_NUMBER_ENV} or {TWILIO_MESSAGING_SERVICE_SID_ENV}"
        );

        Some(Self {
            account_sid,
            auth_token,
            api_key_sid,
            api_key_secret,
            from_number,
            messaging_service_sid,
            test_to,
        })
    }

    fn into_deployment_secrets(self) -> DeploymentSecrets {
        DeploymentSecrets::new()
            .with(DeploymentSecret::TwilioAccountSid, Some(self.account_sid))
            .with(DeploymentSecret::TwilioAuthToken, self.auth_token)
            .with(DeploymentSecret::TwilioApiKeySid, self.api_key_sid)
            .with(DeploymentSecret::TwilioApiKeySecret, self.api_key_secret)
            .with(DeploymentSecret::TwilioFromNumber, self.from_number)
            .with(
                DeploymentSecret::TwilioMessagingServiceSid,
                self.messaging_service_sid,
            )
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

fn normalize_us_test_number(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with('+') {
        return trimmed.to_string();
    }
    let digits = trimmed
        .chars()
        .filter(|character| character.is_ascii_digit())
        .collect::<String>();
    if digits.len() == 10 {
        format!("+1{digits}")
    } else {
        digits
    }
}
