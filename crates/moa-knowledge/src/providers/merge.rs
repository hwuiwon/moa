//! Merge linked-account provider adapter.

use bytes::Bytes;
use reqwest::{Client, header::HeaderMap};
use serde::Deserialize;
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

/// HTTP client for Merge tenant knowledge connections.
#[derive(Debug, Clone)]
pub struct MergeProvider {
    client: Client,
    base_url: String,
    api_key: String,
}

impl MergeProvider {
    /// Creates a Merge provider with the default HTTP client.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: http::build_http_client()?,
            base_url: trim_base_url(base_url.into()),
            api_key: api_key.into(),
        })
    }

    /// Creates a Merge provider with an injected HTTP client.
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
impl LinkedIntegrationProvider for MergeProvider {
    async fn create_link_token(&self, req: CreateLinkTokenRequest) -> Result<LinkToken> {
        #[derive(Deserialize)]
        struct Response {
            link_token: Option<String>,
            magic_link_url: Option<String>,
            expires_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let response = self
            .client
            .post(self.url("/api/integrations/create-link-token"))
            .bearer_auth(&self.api_key)
            .json(&json!({
                "end_user_origin_id": req.external_account_id.unwrap_or_else(|| req.tenant_id.to_string()),
                "end_user_organization_name": req.tenant_id.to_string(),
                "categories": [req.connector],
                "redirect_uri": req.redirect_url,
            }))
            .send()
            .await
            .map_err(|error| Error::provider("merge", format!("link token request failed: {error}")))?;
        let response: Response = http::json_response(response).await?;
        Ok(LinkToken {
            provider: "merge".to_string(),
            token: response.link_token.ok_or_else(|| {
                Error::provider("merge", "link token response missing link_token")
            })?,
            link_url: response.magic_link_url,
            expires_at: response.expires_at,
        })
    }

    async fn exchange_public_token(
        &self,
        req: ExchangePublicTokenRequest,
    ) -> Result<LinkedAccount> {
        #[derive(Deserialize)]
        struct Response {
            account_token: Option<String>,
            id: Option<String>,
            integration: Option<Value>,
        }
        let response = self
            .client
            .post(self.url("/api/integrations/account-token"))
            .bearer_auth(&self.api_key)
            .json(&json!({ "public_token": req.public_token }))
            .send()
            .await
            .map_err(|error| Error::provider("merge", format!("token exchange failed: {error}")))?;
        let response: Response = http::json_response(response).await?;
        let account_token = response
            .account_token
            .ok_or_else(|| Error::provider("merge", "exchange response missing account_token"))?;
        let provider_account_id = response.id.unwrap_or_else(|| stable_id(&account_token));
        Ok(LinkedAccount {
            provider: "merge".to_string(),
            connector: "merge".to_string(),
            provider_account_id,
            credential_ref: format!("merge:{account_token}"),
            metadata: redact_provider_metadata(response.integration.unwrap_or(Value::Null)),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> Result<TriggeredSync> {
        let response = self
            .client
            .post(self.url("/api/sync-status/resync"))
            .bearer_auth(&self.api_key)
            .header("X-Account-Token", req.connection.credential_ref)
            .json(&json!({ "model_name": req.model }))
            .send()
            .await
            .map_err(|error| Error::provider("merge", format!("sync trigger failed: {error}")))?;
        let value: Value = http::json_response(response).await?;
        Ok(TriggeredSync {
            provider: "merge".to_string(),
            provider_sync_id: string_field(&value, &["id", "sync_id"]),
            status: string_field(&value, &["status"]).unwrap_or_else(|| "triggered".to_string()),
            metadata: redact_provider_metadata(value),
        })
    }

    async fn list_changed_records(&self, req: ListChangedRecordsRequest) -> Result<RecordPage> {
        let mut url = parse_url(&self.url("/api/knowledge/records"))?;
        if let Some(cursor) = &req.cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        if let Some(limit) = req.limit {
            url.query_pairs_mut()
                .append_pair("page_size", &limit.to_string());
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .header("X-Account-Token", req.connection.credential_ref)
            .send()
            .await
            .map_err(|error| Error::provider("merge", format!("record listing failed: {error}")))?;
        let value: Value = http::json_response(response).await?;
        Ok(RecordPage {
            next_cursor: string_field(&value, &["next", "next_cursor", "cursor"]),
            records: value
                .get("results")
                .or_else(|| value.get("records"))
                .and_then(Value::as_array)
                .map(|records| records.iter().map(value_to_provider_record).collect())
                .unwrap_or_default(),
        })
    }

    async fn verify_webhook(&self, _headers: HeaderMap, body: Bytes) -> Result<WebhookEvent> {
        let value: Value = serde_json::from_slice(&body).map_err(|error| {
            Error::provider("merge", format!("webhook JSON decode failed: {error}"))
        })?;
        Ok(WebhookEvent {
            provider: "merge".to_string(),
            event_id: string_field(&value, &["hook.id", "id", "event_id"])
                .unwrap_or_else(|| "unknown".to_string()),
            event_type: string_field(&value, &["event", "event_type", "type"])
                .unwrap_or_else(|| "unknown".to_string()),
            metadata: redact_provider_metadata(value),
        })
    }
}

fn value_to_provider_record(value: &Value) -> ProviderRecord {
    let payload = redact_provider_metadata(value.clone());
    ProviderRecord {
        source_id: string_field(&payload, &["id", "remote_id"])
            .unwrap_or_else(|| stable_id(&payload.to_string())),
        object_type: string_field(&payload, &["model", "object_type", "type"])
            .unwrap_or_else(|| "record".to_string()),
        title: string_field(&payload, &["name", "title", "subject"]),
        source_uri: string_field(&payload, &["url", "remote_url", "web_url"]),
        change_token: string_field(
            &payload,
            &["modified_at", "updated_at", "remote_updated_at"],
        ),
        deleted: payload
            .get("is_deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        source_updated_at: string_field(&payload, &["modified_at", "updated_at"])
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&chrono::Utc)),
        metadata: Value::Null,
        payload,
    }
}

fn trim_base_url(value: String) -> String {
    value.trim_end_matches('/').to_string()
}

fn stable_id(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
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

fn parse_url(value: &str) -> Result<reqwest::Url> {
    reqwest::Url::parse(value)
        .map_err(|error| Error::provider("merge", format!("invalid URL `{value}`: {error}")))
}
