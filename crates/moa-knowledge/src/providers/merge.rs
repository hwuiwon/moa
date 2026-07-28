//! Merge linked-account provider adapter.

use base64::{Engine as _, engine::general_purpose};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use reqwest::{Client, header::HeaderMap};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

use crate::acl_key::SourceAclKey;
use crate::{
    domain::{
        CreateLinkTokenRequest, ExchangePublicTokenRequest, InitialSyncStarted, LinkToken,
        LinkedAccount, ListChangedRecordsRequest, ProviderAclCapability, ProviderIntegration,
        ProviderRecord, RecordPage, StartInitialSyncRequest, TriggerSyncRequest, TriggeredSync,
        WebhookEvent,
    },
    error::{Error, Result},
    normalize::redact_provider_metadata,
    providers::{
        LinkedIntegrationProvider,
        http::{self, string_field, trim_base_url},
    },
};

/// HTTP client for Merge tenant knowledge connections.
#[derive(Clone)]
pub struct MergeProvider {
    client: Client,
    base_url: String,
    api_key: String,
    webhook_signature_key: Option<String>,
}

impl MergeProvider {
    /// Creates a Merge provider with the default HTTP client.
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Result<Self> {
        Ok(Self {
            client: http::build_http_client()?,
            base_url: trim_base_url(base_url.into()),
            api_key: api_key.into(),
            webhook_signature_key: None,
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
            webhook_signature_key: None,
        }
    }

    /// Configures the webhook signature key used to verify Merge webhook payloads.
    #[must_use]
    pub fn with_webhook_signature_key(mut self, signature_key: impl Into<String>) -> Self {
        self.webhook_signature_key = Some(signature_key.into());
        self
    }

    fn url(&self, path: &str) -> String {
        http::join_url(&self.base_url, path)
    }

    fn verify_signature(&self, headers: &HeaderMap, body: &[u8]) -> Result<()> {
        let signature_key = self.webhook_signature_key.as_deref().ok_or_else(|| {
            Error::Config("Merge webhook signature key is not configured".to_string())
        })?;
        let signature = http::header_value("merge", headers, "x-merge-webhook-signature")?;
        let signature = decode_signature(signature)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(signature_key.as_bytes()).map_err(|error| {
                Error::provider("merge", format!("webhook signature key failed: {error}"))
            })?;
        mac.update(body);
        mac.verify_slice(&signature)
            .map_err(|_| Error::provider("merge", "webhook signature verification failed"))
    }
}

/// Merge unified-API product categories MOA can sync knowledge from, as
/// `(category id, display name)`. Merge models integrations as categories rather
/// than a per-vendor catalog, so the `connector` in the link flow is a category
/// id (e.g. `knowledgebase`) that fans out to every vendor Merge supports for it.
const MERGE_KNOWLEDGE_CATEGORIES: &[(&str, &str)] = &[
    ("knowledgebase", "Knowledge Base"),
    ("filestorage", "File Storage"),
];

#[async_trait::async_trait]
impl LinkedIntegrationProvider for MergeProvider {
    /// Every Merge category MOA syncs (knowledge bases, file storage, ticketing)
    /// is permission-bearing at the source, so Merge always returns native ACLs.
    /// A record whose category does not expose a permission listing normalizes to
    /// an INCOMPLETE ACL and stays hidden rather than becoming tenant-readable.
    fn acl_capability(&self) -> ProviderAclCapability {
        ProviderAclCapability::NativeSnapshots
    }

    async fn list_integrations(&self) -> Result<Vec<ProviderIntegration>> {
        // Static category list: Merge exposes integrations as unified-API product
        // categories, and the category id is what the link flow passes as
        // `connector`.
        Ok(MERGE_KNOWLEDGE_CATEGORIES
            .iter()
            .map(|(id, display_name)| ProviderIntegration {
                id: (*id).to_string(),
                display_name: (*display_name).to_string(),
                logo_url: None,
            })
            .collect())
    }

    async fn create_link_token(&self, req: CreateLinkTokenRequest) -> Result<LinkToken> {
        #[derive(Deserialize)]
        struct Response {
            link_token: Option<String>,
            magic_link_url: Option<String>,
            expires_at: Option<chrono::DateTime<chrono::Utc>>,
        }
        let end_user_origin_id = req
            .external_account_id
            .clone()
            .unwrap_or_else(|| req.tenant_id.to_string());
        let mut body = serde_json::Map::new();
        body.insert(
            "end_user_origin_id".to_string(),
            Value::String(end_user_origin_id),
        );
        if let Some(email) = req.end_user_email_address {
            body.insert("end_user_email_address".to_string(), Value::String(email));
        }
        body.insert(
            "end_user_organization_name".to_string(),
            Value::String(req.tenant_id.to_string()),
        );
        body.insert("categories".to_string(), json!([req.connector]));
        if let Some(redirect_url) = req.redirect_url {
            body.insert("redirect_uri".to_string(), Value::String(redirect_url));
        }
        let response = self
            .client
            .post(self.url("/api/integrations/create-link-token"))
            .bearer_auth(&self.api_key)
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|error| {
                Error::provider("merge", format!("link token request failed: {error}"))
            })?;
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
        let category = validated_category(&req.connector)?;
        let mut url = http::parse_url(&self.url("/api/integrations/account-token"), |m| {
            Error::provider("merge", m)
        })?;
        append_path_segment(&mut url, &req.public_token)?;
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| Error::provider("merge", format!("token exchange failed: {error}")))?;
        let response: Response = http::json_response(response).await?;
        let account_token = response
            .account_token
            .ok_or_else(|| Error::provider("merge", "exchange response missing account_token"))?;
        let integration = response.integration.unwrap_or(Value::Null);
        // The linked integration reports which categories it actually supports.
        // Rejecting a mismatch here keeps every later request for this
        // connection on the category the operator linked, instead of silently
        // querying a category the integration cannot serve.
        ensure_integration_supports_category(&integration, category)?;
        let provider_account_id = response.id.unwrap_or_else(|| stable_id(&account_token));
        Ok(LinkedAccount {
            provider: "merge".to_string(),
            connector: category.to_string(),
            provider_account_id,
            credential_ref: "merge-account-token".to_string(),
            credential_material: Some(account_token),
            metadata: redact_provider_metadata(integration),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> Result<TriggeredSync> {
        let category = validated_category(&req.connection.connector)?;
        let response = self
            .client
            .post(self.url(&format!("/api/{category}/v1/sync-status/resync")))
            .bearer_auth(&self.api_key)
            .header(
                "X-Account-Token",
                req.credential.expose_for_outbound_request(),
            )
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

    async fn start_initial_sync(&self, req: StartInitialSyncRequest) -> Result<InitialSyncStarted> {
        // Merge starts a linked account's initial sync automatically, so there
        // is nothing to start. Deliberately read-only: the force-resync endpoint
        // is plan-gated and consumes credits per call, which a replayed link
        // would charge repeatedly for no benefit.
        let category = validated_category(&req.connection.connector)?;
        let response = self
            .client
            .get(self.url(&format!("/api/{category}/v1/sync-status")))
            .bearer_auth(&self.api_key)
            .header(
                "X-Account-Token",
                req.credential.expose_for_outbound_request(),
            )
            .send()
            .await
            .map_err(|error| {
                Error::provider("merge", format!("sync status read failed: {error}"))
            })?;
        let value: Value = http::json_response(response).await?;
        let completed = initial_sync_completed(&value)?;
        Ok(InitialSyncStarted {
            provider: "merge".to_string(),
            provider_sync_id: None,
            completed,
            metadata: redact_provider_metadata(value),
        })
    }

    async fn list_changed_records(&self, req: ListChangedRecordsRequest) -> Result<RecordPage> {
        let category = validated_category(&req.connection.connector)?;
        let resource = category_record_resource(category);
        let mut url = http::parse_url(&self.url(&format!("/api/{category}/v1/{resource}")), |m| {
            Error::provider("merge", m)
        })?;
        if let Some(cursor) = &req.cursor {
            url.query_pairs_mut().append_pair("cursor", cursor);
        }
        if let Some(modified_after) = req.modified_after {
            url.query_pairs_mut()
                .append_pair("modified_after", &modified_after.to_rfc3339());
        }
        if let Some(limit) = req.limit {
            url.query_pairs_mut()
                .append_pair("page_size", &limit.to_string());
        }
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .header(
                "X-Account-Token",
                req.credential.expose_for_outbound_request(),
            )
            .send()
            .await
            .map_err(|error| Error::provider("merge", format!("record listing failed: {error}")))?;
        let value: Value = http::json_response(response).await?;
        // Principals are scoped to the linked category, so a person in a Merge
        // knowledge base and the same person in a ticketing account are distinct
        // principals unless a verified binding says otherwise.
        let namespace = format!("merge:{}", req.connection.connector);
        Ok(RecordPage {
            next_cursor: string_field(&value, &["next", "next_cursor", "cursor"]),
            records: value
                .get("results")
                .or_else(|| value.get("records"))
                .and_then(Value::as_array)
                .map(|records| {
                    records
                        .iter()
                        .map(|record| value_to_provider_record(&namespace, &req.acl_key, record))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    async fn verify_webhook(&self, headers: HeaderMap, body: Bytes) -> Result<WebhookEvent> {
        self.verify_signature(&headers, &body)?;
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

/// Returns the requested Merge category, rejecting anything outside the closed set.
///
/// The category is part of every category-scoped URL this adapter builds, so it
/// must never come from an unvalidated string.
fn validated_category(connector: &str) -> Result<&'static str> {
    MERGE_KNOWLEDGE_CATEGORIES
        .iter()
        .find(|(id, _)| *id == connector)
        .map(|(id, _)| *id)
        .ok_or_else(|| {
            Error::provider(
                "merge",
                format!("`{connector}` is not a supported Merge knowledge category"),
            )
        })
}

/// Returns the category's record collection under the versioned category API.
fn category_record_resource(category: &str) -> &'static str {
    match category {
        "filestorage" => "files",
        // `knowledgebase` is the only other supported category, and
        // `validated_category` already rejected everything else.
        _ => "articles",
    }
}

/// Fails unless the linked integration declares support for `category`.
///
/// An integration that omits `categories` entirely is treated as unverifiable
/// rather than compatible: accepting it would let a link proceed against an API
/// surface the integration may not serve.
fn ensure_integration_supports_category(integration: &Value, category: &str) -> Result<()> {
    let categories = integration
        .get("categories")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Error::provider(
                "merge",
                "exchange response integration did not declare categories",
            )
        })?;
    if categories
        .iter()
        .filter_map(Value::as_str)
        .any(|declared| declared == category)
    {
        return Ok(());
    }
    Err(Error::provider(
        "merge",
        format!("linked Merge integration does not support the `{category}` category"),
    ))
}

/// Decides whether a Merge sync-status payload proves the initial sync finished.
///
/// Merge's documented readiness rule is that a model is caught up when its
/// `status` is `DONE` or it is no longer performing its initial sync
/// (`is_initial_sync = false`). Disabled models are skipped because they will
/// never report progress, while failed and paused models — and any status
/// outside the documented set — fail closed rather than being read as progress.
fn initial_sync_completed(value: &Value) -> Result<bool> {
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::provider("merge", "sync status response had no results array"))?;
    let mut evaluated = 0_usize;
    let mut completed = true;
    for entry in results {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::provider("merge", "sync status entry had no status"))?
            .to_ascii_uppercase();
        match status.as_str() {
            "DISABLED" => continue,
            "FAILED" | "PAUSED" => {
                return Err(Error::provider(
                    "merge",
                    format!("linked account sync status is {status}"),
                ));
            }
            "DONE" | "SYNCING" | "PARTIALLY_SYNCED" => {}
            other => {
                return Err(Error::provider(
                    "merge",
                    format!("unrecognized sync status `{other}`"),
                ));
            }
        }
        evaluated += 1;
        let is_initial_sync = entry
            .get("is_initial_sync")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if status != "DONE" && is_initial_sync {
            completed = false;
        }
    }
    // Every model disabled means nothing will ever sync; treating that as
    // "completed" would let a link finalize against a connection that can never
    // produce records.
    if evaluated == 0 {
        return Err(Error::provider(
            "merge",
            "linked account has no enabled models to sync",
        ));
    }
    Ok(completed)
}

fn value_to_provider_record(
    namespace: &str,
    acl_key: &SourceAclKey,
    value: &Value,
) -> ProviderRecord {
    let payload = redact_provider_metadata(value.clone());
    let acl = crate::providers::acl_normalize::record_acl_from_payload(
        namespace,
        &payload,
        crate::domain::ProviderAclProvenance::ProviderListing,
        acl_key,
    );
    ProviderRecord {
        source_id: string_field(&payload, &["id", "remote_id"])
            .unwrap_or_else(|| stable_id(&payload.to_string())),
        object_type: string_field(&payload, &["model", "object_type", "type"])
            .unwrap_or_else(|| "record".to_string()),
        title: string_field(&payload, &["name", "title", "subject"]),
        source_uri: string_field(&payload, &["article_url", "url", "remote_url", "web_url"]),
        change_token: string_field(
            &payload,
            &["modified_at", "updated_at", "remote_updated_at"],
        ),
        deleted: payload
            .get("remote_was_deleted")
            .or_else(|| payload.get("is_deleted"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        source_updated_at: string_field(&payload, &["modified_at", "updated_at"])
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(&value).ok())
            .map(|value| value.with_timezone(&chrono::Utc)),
        metadata: Value::Null,
        payload,
        acl,
    }
}

fn decode_signature(value: &str) -> Result<Vec<u8>> {
    general_purpose::URL_SAFE
        .decode(value.trim())
        .or_else(|_| general_purpose::URL_SAFE_NO_PAD.decode(value.trim()))
        .or_else(|_| general_purpose::STANDARD.decode(value.trim()))
        .map_err(|error| {
            Error::provider(
                "merge",
                format!("webhook signature was not base64: {error}"),
            )
        })
}

fn stable_id(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn append_path_segment(url: &mut reqwest::Url, segment: &str) -> Result<()> {
    url.path_segments_mut()
        .map_err(|_| Error::provider("merge", "URL cannot accept path segments"))?
        .push(segment);
    Ok(())
}
