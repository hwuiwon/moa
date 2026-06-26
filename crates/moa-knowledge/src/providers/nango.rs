//! Nango linked-account provider adapter.

use bytes::Bytes;
use reqwest::{Client, header::HeaderMap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    domain::{
        CreateLinkTokenRequest, ExchangePublicTokenRequest, LinkToken, LinkedAccount,
        ListChangedRecordsRequest, ProviderRecord, RecordPage, TriggerSyncRequest, TriggeredSync,
        WebhookEvent,
    },
    error::{Error, Result},
    normalize::redact_provider_metadata,
    providers::{LinkedIntegrationProvider, http},
};

/// HTTP client for Nango tenant knowledge connections.
#[derive(Debug, Clone)]
pub struct NangoProvider {
    client: Client,
    base_url: String,
    api_key: String,
}

impl NangoProvider {
    /// Creates a Nango provider with the default HTTP client.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: http::build_http_client()?,
            base_url: trim_base_url(base_url.into()),
            api_key: api_key.into(),
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
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }
}

#[async_trait::async_trait]
impl LinkedIntegrationProvider for NangoProvider {
    async fn create_link_token(&self, req: CreateLinkTokenRequest) -> Result<LinkToken> {
        #[derive(Deserialize)]
        struct Response {
            token: Option<String>,
            link_token: Option<String>,
            link_url: Option<String>,
            expires_at: Option<chrono::DateTime<chrono::Utc>>,
        }

        let body = json!({
            "provider_config_key": req.connector,
            "connection_id": req.external_account_id,
            "tenant_id": req.tenant_id.to_string(),
            "redirect_url": req.redirect_url,
        });
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
        let token = response.link_token.or(response.token).ok_or_else(|| {
            Error::provider("nango", "link token response did not include a token")
        })?;
        Ok(LinkToken {
            provider: "nango".to_string(),
            token,
            link_url: response.link_url,
            expires_at: response.expires_at,
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
            provider_account_id,
            metadata: redact_provider_metadata(response.metadata.unwrap_or(Value::Null)),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> Result<TriggeredSync> {
        let response = self
            .client
            .post(self.url("/sync/trigger"))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "connection_id": req.connection.provider_account_id,
                "provider_config_key": req.connection.connector,
                "sync_name": req.model,
            }))
            .send()
            .await
            .map_err(|error| Error::provider("nango", format!("sync trigger failed: {error}")))?;
        let value: Value = http::json_response(response).await?;
        Ok(TriggeredSync {
            provider: "nango".to_string(),
            provider_sync_id: string_field(&value, &["sync_id", "id"]),
            status: string_field(&value, &["status"]).unwrap_or_else(|| "triggered".to_string()),
            metadata: redact_provider_metadata(value),
        })
    }

    async fn list_changed_records(&self, req: ListChangedRecordsRequest) -> Result<RecordPage> {
        let mut url = parse_url(&self.url("/records"))?;
        url.query_pairs_mut()
            .append_pair("connection_id", &req.connection.provider_account_id)
            .append_pair("provider_config_key", &req.connection.connector);
        if let Some(cursor) = &req.cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        if let Some(limit) = req.limit {
            url.query_pairs_mut()
                .append_pair("limit", &limit.to_string());
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| Error::provider("nango", format!("record listing failed: {error}")))?;
        let page: NangoRecordPage = http::json_response(response).await?;
        Ok(page.into_record_page())
    }

    async fn verify_webhook(&self, _headers: HeaderMap, body: Bytes) -> Result<WebhookEvent> {
        let value: Value = serde_json::from_slice(&body).map_err(|error| {
            Error::provider("nango", format!("webhook JSON decode failed: {error}"))
        })?;
        Ok(WebhookEvent {
            provider: "nango".to_string(),
            event_id: string_field(&value, &["id", "event_id"])
                .unwrap_or_else(|| "unknown".to_string()),
            event_type: string_field(&value, &["type", "event_type"])
                .unwrap_or_else(|| "unknown".to_string()),
            metadata: redact_provider_metadata(value),
        })
    }
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

fn trim_base_url(value: String) -> String {
    value.trim_end_matches('/').to_string()
}

fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let mut current = value;
        for segment in key.split('.') {
            current = current.get(segment)?;
        }
        current.as_str().map(ToOwned::to_owned)
    })
}

fn stable_payload_id(value: &Value) -> String {
    blake3::hash(value.to_string().as_bytes())
        .to_hex()
        .to_string()
}

fn parse_url(value: &str) -> Result<reqwest::Url> {
    reqwest::Url::parse(value)
        .map_err(|error| Error::provider("nango", format!("invalid URL `{value}`: {error}")))
}
