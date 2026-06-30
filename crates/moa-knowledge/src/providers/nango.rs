//! Nango linked-account provider adapter.

use bytes::Bytes;
use hmac::{Hmac, Mac};
use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::{
    domain::{
        ApplySourceSelectionRequest, CreateLinkTokenRequest, ExchangePublicTokenRequest, LinkToken,
        LinkedAccount, ListChangedRecordsRequest, ProviderRecord, RecordPage, TriggerSyncRequest,
        TriggeredSync, WebhookEvent,
    },
    error::{Error, Result},
    normalize::redact_provider_metadata,
    providers::{
        LinkedIntegrationProvider,
        http::{self, string_field, trim_base_url},
    },
};

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
        if let Some(model) = nango_selected_model(&req.connection.source_selection) {
            url.query_pairs_mut().append_pair("model", &model);
        }
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

fn nango_metadata(value: &Value) -> Option<Value> {
    let selection = provider_source_selection(value, "nango")?;
    if let Some(metadata) = selection.get("metadata") {
        return Some(redact_provider_metadata(metadata.clone()));
    }
    if selection.get("variant").is_some()
        || selection.get("variants").is_some()
        || selection.get("sync_name").is_some()
    {
        return None;
    }
    Some(redact_provider_metadata(selection.clone()))
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
