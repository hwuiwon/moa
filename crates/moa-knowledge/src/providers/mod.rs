//! Linked-account provider traits and adapters.

use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::HeaderMap;

use crate::{
    domain::{
        ApplySourceSelectionRequest, CreateLinkTokenRequest, ExchangePublicTokenRequest, LinkToken,
        LinkedAccount, ListChangedRecordsRequest, RecordPage, TriggerSyncRequest, TriggeredSync,
        WebhookEvent,
    },
    error::Result,
};

pub mod merge;
pub mod nango;

/// Tenant knowledge linked-account provider seam.
#[async_trait]
pub trait LinkedIntegrationProvider: Send + Sync {
    /// Creates a short-lived link token or hosted link URL.
    async fn create_link_token(&self, req: CreateLinkTokenRequest) -> Result<LinkToken>;

    /// Exchanges a public token for a linked account reference.
    async fn exchange_public_token(&self, req: ExchangePublicTokenRequest)
    -> Result<LinkedAccount>;

    /// Triggers a provider-side sync for one connection.
    async fn trigger_sync(&self, req: TriggerSyncRequest) -> Result<TriggeredSync>;

    /// Applies provider-native selected source state for one connection.
    async fn apply_source_selection(&self, _req: ApplySourceSelectionRequest) -> Result<()> {
        Ok(())
    }

    /// Lists changed source records from the provider cache or API.
    async fn list_changed_records(&self, req: ListChangedRecordsRequest) -> Result<RecordPage>;

    /// Verifies and normalizes a provider webhook.
    async fn verify_webhook(&self, headers: HeaderMap, body: Bytes) -> Result<WebhookEvent>;
}

pub(crate) mod http {
    //! Shared HTTP helpers for knowledge provider and parser adapters.

    use std::time::Duration;

    use reqwest::header::HeaderMap;
    use reqwest::{Client, Response, StatusCode, header::RETRY_AFTER};
    use serde::de::DeserializeOwned;
    use serde_json::Value;

    use crate::error::{Error, Result};

    const MAX_RESPONSE_BYTES: usize = 10 * 1024 * 1024;

    /// Builds the shared HTTP client used by knowledge adapters.
    pub(crate) fn build_http_client() -> Result<Client> {
        Client::builder()
            .user_agent(concat!("moa-knowledge/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|error| Error::Transport(format!("failed to build HTTP client: {error}")))
    }

    /// Reads a successful response as JSON or maps an error status.
    pub(crate) async fn json_response<T>(response: Response) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let response = ensure_success(response).await?;
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
        {
            return Err(Error::Decode(
                "JSON response body was too large".to_string(),
            ));
        }
        let body = response
            .bytes()
            .await
            .map_err(|error| Error::Transport(format!("failed to read response body: {error}")))?;
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(Error::Decode(
                "JSON response body was too large".to_string(),
            ));
        }
        serde_json::from_slice(&body)
            .map_err(|error| Error::Decode(format!("failed to decode JSON response: {error}")))
    }

    /// Returns the response when it has a success status.
    pub(crate) async fn ensure_success(response: Response) -> Result<Response> {
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }

        let retry_after = retry_after_delay(status, response.headers().get(RETRY_AFTER));
        Err(Error::HttpStatus {
            status: status.as_u16(),
            retry_after,
            message: "upstream returned non-success status".to_string(),
        })
    }

    fn retry_after_delay(
        status: StatusCode,
        value: Option<&reqwest::header::HeaderValue>,
    ) -> Option<Duration> {
        if status != StatusCode::TOO_MANY_REQUESTS {
            return None;
        }
        let seconds = value?.to_str().ok()?.trim().parse::<u64>().ok()?;
        Some(Duration::from_secs(seconds))
    }

    /// Trims a single trailing slash from a base URL.
    pub(crate) fn trim_base_url(value: String) -> String {
        value.trim_end_matches('/').to_string()
    }

    /// Joins a trimmed base URL with a request path, normalizing the separator.
    pub(crate) fn join_url(base_url: &str, path: &str) -> String {
        format!("{}/{}", base_url, path.trim_start_matches('/'))
    }

    /// Resolves a dotted JSON path to a nested value when every segment exists.
    pub(crate) fn nested_value<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
        let mut current = value;
        for segment in path.split('.') {
            current = current.get(segment)?;
        }
        Some(current)
    }

    /// Returns the first dotted key that resolves to a JSON string.
    pub(crate) fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
        keys.iter()
            .find_map(|key| nested_value(value, key)?.as_str().map(ToOwned::to_owned))
    }

    /// Returns the first dotted key that resolves to a non-null JSON value.
    pub(crate) fn value_field(value: &Value, keys: &[&str]) -> Option<Value> {
        keys.iter().find_map(|key| {
            let current = nested_value(value, key)?;
            (!current.is_null()).then(|| current.clone())
        })
    }

    /// Parses a URL, mapping failures through the provided error constructor.
    pub(crate) fn parse_url(
        value: &str,
        make_error: impl FnOnce(String) -> Error,
    ) -> Result<reqwest::Url> {
        reqwest::Url::parse(value)
            .map_err(|error| make_error(format!("invalid URL `{value}`: {error}")))
    }

    /// Reads a required webhook header as a string for a labeled provider.
    pub(crate) fn header_value<'a>(
        provider: &str,
        headers: &'a HeaderMap,
        name: &str,
    ) -> Result<&'a str> {
        headers
            .get(name)
            .ok_or_else(|| Error::provider(provider, format!("webhook missing `{name}` header")))?
            .to_str()
            .map_err(|error| {
                Error::provider(provider, format!("webhook header `{name}` failed: {error}"))
            })
    }

    /// Returns the lowercased status string from the first matching JSON pointer.
    pub(crate) fn parse_status(value: &Value, pointers: &[&str]) -> String {
        pointers
            .iter()
            .find_map(|pointer| value.pointer(pointer))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    /// Returns whether a parse/partition status denotes a terminal failure.
    pub(crate) fn status_failed(status: &str) -> bool {
        matches!(status, "error" | "failed" | "failure")
    }

    /// Returns whether a parse/partition status denotes an in-progress job.
    pub(crate) fn status_pending(status: &str) -> bool {
        matches!(status, "pending" | "queued" | "running" | "processing")
    }
}
