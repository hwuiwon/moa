//! Nango linked-account provider adapter.

use bytes::Bytes;
use hmac::{Hmac, Mac};
use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::{
    domain::{
        ApplySourceSelectionRequest, CreateLinkTokenRequest, ExchangePublicTokenRequest,
        FetchRecordContentRequest, FetchedRecordContent, KnowledgeConnection, LinkToken,
        LinkedAccount, ListChangedRecordsRequest, ProviderIntegration, ProviderRecord, RecordPage,
        TriggerSyncRequest, TriggeredSync, WebhookEvent,
    },
    error::{Error, Result},
    normalize::redact_provider_metadata,
    providers::{
        LinkedIntegrationProvider,
        http::{self, string_field, trim_base_url},
    },
};

mod google_drive;

/// Maximum record content size fetched through the proxy, mirroring the crate's
/// 10 MiB HTTP response cap.
const MAX_RECORD_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// A resolved Nango proxy content-fetch request for one record: path segments
/// appended after `/proxy/`, query parameters, and how the result MIME is
/// labeled. Integration strategy modules build this; [`NangoProvider::proxy_fetch`]
/// executes it.
struct ProxyFetchPlan {
    /// Path segments appended after `/proxy/` (e.g. `["drive","v3","files",id]`).
    path_segments: Vec<String>,
    /// Query parameters appended to the proxied request.
    query: Vec<(String, String)>,
    /// Authoritative result MIME (e.g. a chosen export format); wins over the
    /// response `Content-Type`.
    result_mime: Option<String>,
    /// MIME used only when the response carries no `Content-Type` (verbatim
    /// downloads).
    fallback_mime: Option<String>,
}

/// Resolves the content-fetch strategy for a Nango integration.
///
/// This is the integration registry. **Adding an integration = a new sibling
/// module (like [`google_drive`]) exposing `content_fetch_plan`, plus one match
/// arm below** — nothing else keys on the integration id. Unregistered
/// integrations return `None`, so the pipeline keeps its title-only fallback
/// rather than erroring. Records that already carry inline text never reach here
/// because the pipeline materializes them without a fetch.
fn integration_content_fetch_plan(
    connector: &str,
    record: &ProviderRecord,
) -> Option<ProxyFetchPlan> {
    match connector.trim().to_ascii_lowercase().as_str() {
        "google-drive" | "google_drive" | "googledrive" => google_drive::content_fetch_plan(record),
        _ => None,
    }
}

/// HTTP client for Nango tenant knowledge connections.
#[derive(Clone)]
pub struct NangoProvider {
    client: Client,
    base_url: String,
    api_key: String,
    webhook_signing_key: Option<String>,
}

impl NangoProvider {
    /// Creates a Nango provider with the default HTTP client.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: http::build_http_client()?,
            base_url: trim_base_url(base_url.into()),
            api_key: api_key.into(),
            webhook_signing_key: None,
        })
    }

    /// Creates a Nango provider with an injected HTTP client.
    #[must_use]
    pub fn with_client(
        client: Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client,
            base_url: trim_base_url(base_url.into()),
            api_key: api_key.into(),
            webhook_signing_key: None,
        }
    }

    /// Configures the webhook signing key used to verify Nango webhook payloads.
    #[must_use]
    pub fn with_webhook_signing_key(mut self, signing_key: impl Into<String>) -> Self {
        self.webhook_signing_key = Some(signing_key.into());
        self
    }

    fn url(&self, path: &str) -> String {
        http::join_url(&self.base_url, path)
    }

    /// Executes a resolved [`ProxyFetchPlan`] against the Nango proxy.
    ///
    /// This is integration-agnostic Nango infrastructure: it builds the
    /// `/proxy/...` URL, forwards with the same auth as `/records` (the Nango
    /// secret key plus the connection and provider-config identifiers), enforces
    /// the content size cap, and labels the result MIME. Path/query/MIME choices
    /// belong to the per-integration strategy module.
    async fn proxy_fetch(
        &self,
        connection: &KnowledgeConnection,
        plan: ProxyFetchPlan,
    ) -> Result<Option<FetchedRecordContent>> {
        let mut url = http::parse_url(&self.url("/proxy"), |message| {
            Error::provider("nango", message)
        })?;
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|()| Error::provider("nango", "Nango base URL cannot be a base"))?;
            for segment in &plan.path_segments {
                segments.push(segment);
            }
        }
        for (key, value) in &plan.query {
            url.query_pairs_mut().append_pair(key, value);
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .header("Connection-Id", &connection.provider_account_id)
            .header("Provider-Config-Key", &connection.connector)
            .send()
            .await
            .map_err(|error| Error::provider("nango", format!("content fetch failed: {error}")))?;
        let response_mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let bytes = http::bytes_response_capped(response, MAX_RECORD_CONTENT_BYTES).await?;
        if bytes.is_empty() {
            return Ok(None);
        }
        let mime_type = plan.result_mime.or(response_mime).or(plan.fallback_mime);
        Ok(Some(FetchedRecordContent { bytes, mime_type }))
    }

    fn verify_signature(&self, headers: &HeaderMap, body: &[u8]) -> Result<()> {
        let signing_key = self.webhook_signing_key.as_deref().ok_or_else(|| {
            Error::Config("Nango webhook signing key is not configured".to_string())
        })?;
        let signature = http::header_value("nango", headers, "x-nango-hmac-sha256")?;
        let signature = hex::decode(signature.trim()).map_err(|error| {
            Error::provider("nango", format!("webhook signature was not hex: {error}"))
        })?;
        let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes()).map_err(|error| {
            Error::provider("nango", format!("webhook signing key failed: {error}"))
        })?;
        mac.update(body);
        mac.verify_slice(&signature)
            .map_err(|_| Error::provider("nango", "webhook signature verification failed"))
    }
}

#[async_trait::async_trait]
impl LinkedIntegrationProvider for NangoProvider {
    async fn create_link_token(&self, req: CreateLinkTokenRequest) -> Result<LinkToken> {
        #[derive(Deserialize)]
        struct ResponseData {
            token: Option<String>,
            connect_link: Option<String>,
            expires_at: Option<chrono::DateTime<chrono::Utc>>,
        }

        #[derive(Deserialize)]
        struct Response {
            data: Option<ResponseData>,
            token: Option<String>,
            link_token: Option<String>,
            link_url: Option<String>,
            expires_at: Option<chrono::DateTime<chrono::Utc>>,
        }

        let mut body = json!({
            "tags": {
                "tenant_id": req.tenant_id.to_string()
            },
            "allowed_integrations": [req.connector],
        });
        if let Some(account_id) = req.external_account_id
            && let Some(object) = body.as_object_mut()
            && let Some(tags) = object.get_mut("tags").and_then(Value::as_object_mut)
        {
            tags.insert("external_account_id".to_string(), Value::String(account_id));
        }
        if let Some(email) = req.end_user_email_address
            && let Some(object) = body.as_object_mut()
            && let Some(tags) = object.get_mut("tags").and_then(Value::as_object_mut)
        {
            tags.insert("end_user_email".to_string(), Value::String(email));
        }

        let response = self
            .client
            .post(self.url("/connect/sessions"))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                Error::provider("nango", format!("link token request failed: {error}"))
            })?;
        let response: Response = http::json_response(response).await?;
        let token = response
            .data
            .as_ref()
            .and_then(|data| data.token.clone())
            .or(response.link_token)
            .or(response.token)
            .ok_or_else(|| {
                Error::provider("nango", "link token response did not include a token")
            })?;
        let link_url = response
            .data
            .as_ref()
            .and_then(|data| data.connect_link.clone())
            .or(response.link_url);
        let expires_at = response
            .data
            .and_then(|data| data.expires_at)
            .or(response.expires_at);
        Ok(LinkToken {
            provider: "nango".to_string(),
            token,
            link_url,
            expires_at,
        })
    }

    async fn exchange_public_token(
        &self,
        req: ExchangePublicTokenRequest,
    ) -> Result<LinkedAccount> {
        #[derive(Deserialize)]
        struct Response {
            connection_id: Option<String>,
            provider_config_key: Option<String>,
            provider: Option<String>,
            credentials_reference: Option<String>,
            metadata: Option<Value>,
        }

        let response = self
            .client
            .post(self.url("/connect/sessions/exchange"))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "public_token": req.public_token,
                "tenant_id": req.tenant_id.to_string(),
            }))
            .send()
            .await
            .map_err(|error| Error::provider("nango", format!("token exchange failed: {error}")))?;
        let response: Response = http::json_response(response).await?;
        let provider_account_id = response
            .connection_id
            .ok_or_else(|| Error::provider("nango", "exchange response missing connection_id"))?;
        Ok(LinkedAccount {
            provider: "nango".to_string(),
            connector: response
                .provider_config_key
                .or(response.provider)
                .unwrap_or_default(),
            credential_ref: response
                .credentials_reference
                .unwrap_or_else(|| format!("nango:{provider_account_id}")),
            credential_material: None,
            provider_account_id,
            metadata: redact_provider_metadata(response.metadata.unwrap_or(Value::Null)),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> Result<TriggeredSync> {
        let selected_variant = req
            .variant
            .or_else(|| nango_selected_variant(&req.connection.source_selection));
        let selected_model = req
            .model
            .or_else(|| nango_selected_model(&req.connection.source_selection));
        // Per Nango's POST /sync/trigger contract, an empty `syncs` array triggers
        // all syncs configured for the connection, which is the intended behavior
        // when no specific sync model/variant is selected. The `/records` model
        // requirement is enforced separately in `list_changed_records`.
        let syncs = match (selected_model, selected_variant) {
            (Some(model), Some(variant)) => vec![json!({ "name": model, "variant": variant })],
            (Some(model), None) => vec![Value::String(model)],
            (None, _) => Vec::new(),
        };
        let response = self
            .client
            .post(self.url("/sync/trigger"))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "connection_id": req.connection.provider_account_id,
                "provider_config_key": req.connection.connector,
                "syncs": syncs,
            }))
            .send()
            .await
            .map_err(|error| Error::provider("nango", format!("sync trigger failed: {error}")))?;
        let value: Value = http::json_response(response).await?;
        Ok(TriggeredSync {
            provider: "nango".to_string(),
            provider_sync_id: string_field(&value, &["sync_id", "id"]),
            status: string_field(&value, &["status"])
                .or_else(|| match value.get("success").and_then(Value::as_bool) {
                    Some(true) => Some("accepted".to_string()),
                    Some(false) => Some("failed".to_string()),
                    None => None,
                })
                .unwrap_or_else(|| "triggered".to_string()),
            metadata: redact_provider_metadata(value),
        })
    }

    async fn apply_source_selection(&self, req: ApplySourceSelectionRequest) -> Result<()> {
        let selection = nango_source_selection(&req.connection.source_selection);
        if let Some(metadata) = selection.metadata {
            let response = self
                .client
                .post(self.url("/connections/metadata"))
                .bearer_auth(&self.api_key)
                .json(&json!({
                    "connection_id": req.connection.provider_account_id,
                    "provider_config_key": req.connection.connector,
                    "metadata": metadata,
                }))
                .send()
                .await
                .map_err(|error| {
                    Error::provider("nango", format!("metadata update failed: {error}"))
                })?;
            http::ensure_success(response).await?;
        }

        for variant in selection.variants {
            let mut url = http::parse_url(&self.url("/"), |m| Error::provider("nango", m))?;
            url.path_segments_mut()
                .map_err(|_| Error::provider("nango", "Nango base URL cannot be a base"))?
                .push("sync")
                .push(&variant.sync_name)
                .push("variant")
                .push(&variant.variant);
            let response = self
                .client
                .post(url)
                .bearer_auth(&self.api_key)
                .json(&json!({
                    "connection_id": req.connection.provider_account_id,
                    "provider_config_key": req.connection.connector,
                }))
                .send()
                .await
                .map_err(|error| {
                    Error::provider("nango", format!("sync variant creation failed: {error}"))
                })?;
            if response.status() == reqwest::StatusCode::CONFLICT {
                continue;
            }
            http::ensure_success(response).await?;
        }

        Ok(())
    }

    async fn list_changed_records(&self, req: ListChangedRecordsRequest) -> Result<RecordPage> {
        let mut url = http::parse_url(&self.url("/records"), |m| Error::provider("nango", m))?;
        // Nango's GET /records requires a `model` query parameter; without one it
        // rejects the request and never yields records. Fail fast with an
        // actionable message instead of silently listing nothing.
        let model = nango_selected_model(&req.connection.source_selection).ok_or_else(|| {
            Error::provider(
                "nango",
                "Nango /records requires a sync model; set source_selection.model \
                 (or source_selection.nango.model) to the connection's sync model name",
            )
        })?;
        url.query_pairs_mut().append_pair("model", &model);
        if let Some(cursor) = &req.cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        if let Some(modified_after) = req.modified_after {
            url.query_pairs_mut()
                .append_pair("modified_after", &modified_after.to_rfc3339());
        }
        if let Some(limit) = req.limit {
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
        }
        if let Some(variant) = req
            .variant
            .or_else(|| nango_selected_variant(&req.connection.source_selection))
        {
            url.query_pairs_mut().append_pair("variant", &variant);
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .header("Connection-Id", &req.connection.provider_account_id)
            .header("Provider-Config-Key", &req.connection.connector)
            .send()
            .await
            .map_err(|error| Error::provider("nango", format!("record listing failed: {error}")))?;
        let page: NangoRecordPage = http::json_response(response).await?;
        Ok(page.into_record_page())
    }

    async fn fetch_record_content(
        &self,
        req: FetchRecordContentRequest,
    ) -> Result<Option<FetchedRecordContent>> {
        // Resolve the integration-specific fetch strategy. Unregistered
        // integrations, and records with no fetchable content, yield None so the
        // pipeline keeps its title-only fallback instead of erroring.
        let Some(plan) = integration_content_fetch_plan(&req.connection.connector, &req.record)
        else {
            return Ok(None);
        };
        self.proxy_fetch(&req.connection, plan).await
    }

    async fn list_integrations(&self) -> Result<Vec<ProviderIntegration>> {
        #[derive(Deserialize)]
        struct Integration {
            unique_key: Option<String>,
            provider: Option<String>,
            display_name: Option<String>,
            logo: Option<String>,
        }
        #[derive(Deserialize)]
        struct Response {
            #[serde(default, alias = "configs", alias = "integrations")]
            data: Vec<Integration>,
        }

        let response = self
            .client
            .get(self.url("/integrations"))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| {
                Error::provider("nango", format!("integration listing failed: {error}"))
            })?;
        let response: Response = http::json_response(response).await?;
        Ok(response
            .data
            .into_iter()
            .filter_map(|integration| {
                // `unique_key` is the provider_config_key used as `connector`.
                let id = integration
                    .unique_key
                    .clone()
                    .or_else(|| integration.provider.clone())?;
                let display_name = integration
                    .display_name
                    .filter(|name| !name.trim().is_empty())
                    .or(integration.provider)
                    .unwrap_or_else(|| id.clone());
                Some(ProviderIntegration {
                    id,
                    display_name,
                    logo_url: integration.logo,
                })
            })
            .collect())
    }

    async fn verify_webhook(&self, headers: HeaderMap, body: Bytes) -> Result<WebhookEvent> {
        self.verify_signature(&headers, &body)?;
        let value: Value = serde_json::from_slice(&body).map_err(|error| {
            Error::provider("nango", format!("webhook JSON decode failed: {error}"))
        })?;
        Ok(WebhookEvent {
            provider: "nango".to_string(),
            event_id: string_field(&value, &["id", "event_id"])
                .unwrap_or_else(|| "unknown".to_string()),
            event_type: string_field(&value, &["type", "event_type"])
                .unwrap_or_else(|| "unknown".to_string()),
            metadata: value,
        })
    }
}

#[derive(Debug, Default)]
struct NangoSourceSelection {
    metadata: Option<Value>,
    variants: Vec<NangoSyncVariant>,
}

#[derive(Debug)]
struct NangoSyncVariant {
    sync_name: String,
    variant: String,
}

fn nango_source_selection(value: &Value) -> NangoSourceSelection {
    let Some(selection) = provider_source_selection(value, "nango") else {
        return NangoSourceSelection::default();
    };
    NangoSourceSelection {
        metadata: nango_metadata(selection),
        variants: nango_sync_variants(selection),
    }
}

fn provider_source_selection<'a>(value: &'a Value, provider: &str) -> Option<&'a Value> {
    if value.is_null() {
        return None;
    }
    value.get(provider).or(Some(value)).filter(|selection| {
        !selection.is_null() && !selection.as_object().is_some_and(serde_json::Map::is_empty)
    })
}

/// Selection keys consumed by sync-model/variant resolution. These drive Nango
/// sync selection and must never be forwarded as provider connection metadata.
const NANGO_CONTROL_KEYS: &[&str] = &["model", "sync_name", "name", "variant", "variants"];

fn nango_metadata(value: &Value) -> Option<Value> {
    let selection = provider_source_selection(value, "nango")?;
    if let Some(metadata) = selection.get("metadata") {
        return Some(redact_provider_metadata(metadata.clone()));
    }
    // Fallback: treat the selection itself as provider metadata, but strip every
    // control key that drives sync-model/variant selection so none leak to
    // /connections/metadata. If nothing remains, there is no metadata to apply.
    let remaining: serde_json::Map<String, Value> = selection
        .as_object()?
        .iter()
        .filter(|(key, _)| !NANGO_CONTROL_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    if remaining.is_empty() {
        return None;
    }
    Some(redact_provider_metadata(Value::Object(remaining)))
}

fn nango_selected_variant(value: &Value) -> Option<String> {
    let selection = provider_source_selection(value, "nango")?;
    string_field(selection, &["variant"]).or_else(|| {
        selection
            .get("variants")
            .and_then(Value::as_array)
            .and_then(|variants| variants.first())
            .and_then(|variant| string_field(variant, &["variant"]))
    })
}

fn nango_selected_model(value: &Value) -> Option<String> {
    let selection = provider_source_selection(value, "nango")?;
    string_field(selection, &["model", "sync_name", "name"]).or_else(|| {
        selection
            .get("variants")
            .and_then(Value::as_array)
            .and_then(|variants| variants.first())
            .and_then(|variant| string_field(variant, &["model", "sync_name", "name"]))
    })
}

fn nango_sync_variants(value: &Value) -> Vec<NangoSyncVariant> {
    let Some(selection) = provider_source_selection(value, "nango") else {
        return Vec::new();
    };
    let mut variants = Vec::new();
    if let (Some(sync_name), Some(variant)) = (
        string_field(selection, &["sync_name", "name"]),
        string_field(selection, &["variant"]),
    ) {
        variants.push(NangoSyncVariant { sync_name, variant });
    }
    if let Some(values) = selection.get("variants").and_then(Value::as_array) {
        variants.extend(values.iter().filter_map(|value| {
            Some(NangoSyncVariant {
                sync_name: string_field(value, &["sync_name", "name"])?,
                variant: string_field(value, &["variant"])?,
            })
        }));
    }
    variants
}

#[derive(Debug, Deserialize)]
struct NangoRecordPage {
    #[serde(default, alias = "records")]
    data: Vec<NangoRecord>,
    #[serde(default, alias = "next_cursor")]
    next_cursor: Option<String>,
}

impl NangoRecordPage {
    fn into_record_page(self) -> RecordPage {
        RecordPage {
            records: self
                .data
                .into_iter()
                .map(NangoRecord::into_provider_record)
                .collect(),
            next_cursor: self.next_cursor,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct NangoRecord {
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    modified_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    metadata: Value,
    #[serde(flatten)]
    payload: Value,
}

impl NangoRecord {
    fn into_provider_record(self) -> ProviderRecord {
        let payload = redact_provider_metadata(self.payload);
        ProviderRecord {
            source_id: self.id.unwrap_or_else(|| stable_payload_id(&payload)),
            object_type: self.model.unwrap_or_else(|| "record".to_string()),
            title: string_field(&payload, &["title", "name", "subject"]),
            source_uri: string_field(&payload, &["url", "web_url", "html_url"]),
            change_token: string_field(
                &payload,
                &["_nango_metadata.last_action", "updated_at", "modified_at"],
            ),
            deleted: self.deleted,
            source_updated_at: self.modified_at,
            metadata: redact_provider_metadata(self.metadata),
            payload,
        }
    }
}

fn stable_payload_id(value: &Value) -> String {
    blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::nango_metadata;
    use serde_json::json;

    #[test]
    fn control_only_selection_yields_no_connection_metadata() {
        // Pins: a selection carrying only sync-control keys produces no provider
        // metadata, so control fields never reach POST /connections/metadata.
        assert_eq!(nango_metadata(&json!({ "model": "documents" })), None);
        assert_eq!(
            nango_metadata(&json!({
                "model": "documents",
                "sync_name": "documents",
                "name": "documents",
                "variant": "selected-sources",
                "variants": [{ "sync_name": "documents", "variant": "v" }]
            })),
            None
        );
    }

    #[test]
    fn fallback_metadata_strips_control_keys_and_keeps_real_metadata() {
        // Pins: real metadata survives the fallback while control keys (here
        // `model`) are stripped out.
        assert_eq!(
            nango_metadata(&json!({ "model": "documents", "folders": ["f1"] })),
            Some(json!({ "folders": ["f1"] }))
        );
    }

    #[test]
    fn explicit_metadata_object_passes_through() {
        // Pins: an explicit `metadata` object is used as-is (still redacted).
        assert_eq!(
            nango_metadata(&json!({ "metadata": { "folders": ["f1"] } })),
            Some(json!({ "folders": ["f1"] }))
        );
    }
}
