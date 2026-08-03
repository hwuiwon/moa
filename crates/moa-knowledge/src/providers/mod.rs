//! Linked-account provider traits and adapters.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use moa_core::types::credentials::RedactedSecret;
use reqwest::header::HeaderMap;

use crate::{
    domain::{
        ApplySourceSelectionRequest, CreateLinkTokenRequest, ExchangePublicTokenRequest,
        FetchRecordContentRequest, FetchedRecordContent, InitialSyncStarted, KnowledgeConnection,
        LinkToken, LinkedAccount, ListChangedRecordsRequest, ProviderIntegration, ProviderRecord,
        ProviderRecordMaterialization, RecordPage, RemoteRevokeRequest, StartInitialSyncRequest,
        TriggerSyncRequest, TriggeredSync, WebhookEvent,
    },
    error::Result,
};

pub(crate) mod acl_normalize;
pub mod merge;
pub mod nango;

const PROVIDER_MIME_TYPE_FIELDS: &[&str] =
    &["mime_type", "mimeType", "content_type", "contentType"];

/// Reads a provider-reported MIME type from the supported payload aliases.
pub(crate) fn provider_mime_type(payload: &serde_json::Value) -> Option<String> {
    http::string_field(payload, PROVIDER_MIME_TYPE_FIELDS)
}

/// Derives a provider-record fallback ID from canonical JSON bytes.
///
/// Provider-native IDs remain authoritative. This fallback is used only when a provider omits
/// one, and therefore must not change when the same object arrives with a different key order.
pub(crate) fn stable_provider_record_id(value: &serde_json::Value) -> serde_json::Result<String> {
    let canonical = moa_core::canonical_json::canonical_json_bytes(value)?;
    Ok(blake3::hash(&canonical).to_hex().to_string())
}

/// Normalizes provider payload fields into one explicit ingestion intent.
///
/// Field-name knowledge belongs at the adapter boundary. Integrations that can
/// fetch otherwise metadata-only records may promote the returned
/// [`ProviderRecordMaterialization::MetadataOnly`] intent to `ProviderFetch`.
pub(crate) fn materialization_from_payload(
    payload: &serde_json::Value,
) -> ProviderRecordMaterialization {
    const INLINE_TEXT_FIELDS: &[&str] = &[
        "text",
        "content",
        "body",
        "plain_text",
        "plaintext",
        "markdown",
        "html",
    ];
    const FETCHABLE_URL_FIELDS: &[&str] = &[
        "download_url",
        "file_url",
        "content_url",
        "web_content_link",
        "webContentLink",
    ];
    let mime_type = provider_mime_type(payload);
    if let Some(text) = http::string_field(payload, INLINE_TEXT_FIELDS) {
        return ProviderRecordMaterialization::InlineText { text, mime_type };
    }
    if let Some(url) = http::string_field(payload, FETCHABLE_URL_FIELDS) {
        return ProviderRecordMaterialization::FetchableUrl { url, mime_type };
    }
    ProviderRecordMaterialization::MetadataOnly
}

/// Tenant knowledge linked-account provider seam.
#[async_trait]
pub trait LinkedIntegrationProvider: Send + Sync {
    /// Lists the integrations this provider can connect for a tenant.
    ///
    /// The returned `id`s are the values passed as `connector` in the link flow.
    /// The default returns an empty list for providers without an integration
    /// catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider's integration catalog cannot be read.
    async fn list_integrations(&self) -> Result<Vec<ProviderIntegration>> {
        Ok(Vec::new())
    }

    /// Creates a short-lived link token or hosted link URL.
    async fn create_link_token(&self, req: CreateLinkTokenRequest) -> Result<LinkToken>;

    /// Exchanges a public token for a linked account reference.
    async fn exchange_public_token(&self, req: ExchangePublicTokenRequest)
    -> Result<LinkedAccount>;

    /// Triggers an operator-requested provider-side sync for one connection.
    ///
    /// This is the re-sync path and may use one-off or plan-gated provider
    /// endpoints. It is not safe to replay, so the initial link uses
    /// [`LinkedIntegrationProvider::start_initial_sync`] instead.
    async fn trigger_sync(&self, req: TriggerSyncRequest) -> Result<TriggeredSync>;

    /// Starts, or re-confirms, the initial sync for a newly linked connection.
    ///
    /// The link claim persists its trigger boundary only after this returns, so
    /// a crash between claiming the sync run and dispatching replays this exact
    /// call. Implementations must therefore be naturally idempotent or purely
    /// read-only, and must never consume a provider quota per attempt.
    async fn start_initial_sync(&self, req: StartInitialSyncRequest) -> Result<InitialSyncStarted>;

    /// Revokes the provider-native linked account selected by the connection.
    ///
    /// This method is required with no default because silently skipping remote
    /// revocation would leave provider access live after MOA reports a
    /// disconnect. Implementations must not classify an already-absent response
    /// as success unless the provider's published contract guarantees that
    /// replay behavior.
    async fn revoke_remote_connection(&self, req: RemoteRevokeRequest) -> Result<()>;

    /// Applies provider-native selected source state for one connection.
    async fn apply_source_selection(&self, _req: ApplySourceSelectionRequest) -> Result<()> {
        Ok(())
    }

    /// Lists changed source records from the provider cache or API.
    async fn list_changed_records(&self, req: ListChangedRecordsRequest) -> Result<RecordPage>;

    /// Fetches the byte content of one provider record when the provider can
    /// download it directly.
    ///
    /// Providers that emit
    /// [`ProviderRecordMaterialization::ProviderFetch`] implement this so
    /// authenticated document bytes reach the ingestion pipeline. The default
    /// returns `Ok(None)`, which is an error for a record that explicitly
    /// requires provider fetch. Metadata-only records never call this hook.
    ///
    /// # Errors
    ///
    /// Returns an error when a supported fetch is attempted but the download
    /// fails (transport or non-success status). The ingestion pipeline records
    /// that failure and never substitutes record metadata for content.
    async fn fetch_record_content(
        &self,
        _req: FetchRecordContentRequest<'_>,
    ) -> Result<Option<FetchedRecordContent>> {
        Ok(None)
    }

    /// Verifies and normalizes a provider webhook.
    async fn verify_webhook(&self, headers: HeaderMap, body: Bytes) -> Result<WebhookEvent>;
}

/// Per-run seam the ingestion pipeline uses to download record byte content.
///
/// A concrete fetcher binds one [`LinkedIntegrationProvider`] to one
/// [`KnowledgeConnection`] so the pipeline can request content for a record
/// without threading connection identity through every call. See
/// [`LinkedProviderContentFetcher`].
#[async_trait]
pub trait RecordContentFetcher: Send + Sync {
    /// Fetches byte content for one record, or `None` when unsupported.
    ///
    /// # Errors
    ///
    /// Returns an error when a supported fetch is attempted but fails.
    async fn fetch_record_content(
        &self,
        record: &ProviderRecord,
    ) -> Result<Option<FetchedRecordContent>>;
}

/// Adapts a [`LinkedIntegrationProvider`] and one [`KnowledgeConnection`] into a
/// page-scoped [`RecordContentFetcher`] for the ingestion pipeline.
pub struct LinkedProviderContentFetcher {
    provider: Arc<dyn LinkedIntegrationProvider>,
    connection: KnowledgeConnection,
    credential: Option<RedactedSecret>,
}

impl LinkedProviderContentFetcher {
    /// Binds a provider, connection, and resolved page credential for content fetches.
    #[must_use]
    pub fn new(
        provider: Arc<dyn LinkedIntegrationProvider>,
        connection: KnowledgeConnection,
        credential: Option<RedactedSecret>,
    ) -> Self {
        Self {
            provider,
            connection,
            credential,
        }
    }
}

#[async_trait]
impl RecordContentFetcher for LinkedProviderContentFetcher {
    async fn fetch_record_content(
        &self,
        record: &ProviderRecord,
    ) -> Result<Option<FetchedRecordContent>> {
        self.provider
            .fetch_record_content(FetchRecordContentRequest {
                connection: self.connection.clone(),
                credential: self.credential.as_ref(),
                record: record.clone(),
            })
            .await
    }
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

    /// Reads a successful response body as raw bytes, aborting when it exceeds
    /// `max_bytes`.
    ///
    /// The `Content-Length` header, when present, short-circuits oversized
    /// downloads before any body is read; the streaming loop then enforces the
    /// same cap for chunked responses that omit a length.
    pub(crate) async fn bytes_response_capped(
        response: Response,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let mut response = ensure_success(response).await?;
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(Error::Decode(
                "response body exceeded the content size cap".to_string(),
            ));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| Error::Transport(format!("failed to read response body: {error}")))?
        {
            if body.len().saturating_add(chunk.len()) > max_bytes {
                return Err(Error::Decode(
                    "response body exceeded the content size cap".to_string(),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
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

#[cfg(test)]
mod tests {
    //! Unit coverage for provider-neutral identity helpers.

    use super::stable_provider_record_id;

    #[test]
    fn provider_record_fallback_id_ignores_object_key_order() {
        // Pins: Nango and Merge cannot reintroduce one provider object as a new record solely
        // because the provider changed JSON object insertion order.
        let forward = serde_json::json!({
            "alpha": 1,
            "nested": {"first": true, "second": "value"}
        });
        let reverse: serde_json::Value =
            serde_json::from_str(r#"{"nested":{"second":"value","first":true},"alpha":1}"#)
                .expect("reordered provider fixture decodes");

        assert_eq!(
            stable_provider_record_id(&forward).expect("hash forward record"),
            stable_provider_record_id(&reverse).expect("hash reordered record")
        );
    }
}
