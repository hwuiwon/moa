//! Signed provider webhook handling for the Knowledge service.

use base64::{Engine as _, engine::general_purpose};
use chrono::Utc;
use moa_core::{
    TenantId,
    wire::knowledge::{KnowledgeProviderWebhookRequest, KnowledgeProviderWebhookResponse},
};
use moa_knowledge::domain::{
    IngestionStepStatus, KnowledgeIngestionStep, KnowledgeProviderEventRecord, KnowledgeSyncRun,
    SyncRunStatus,
};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{KnowledgeService, KnowledgeServiceError};

impl KnowledgeService {
    /// Verifies, records, and idempotently handles one provider webhook delivery.
    pub async fn provider_webhook(
        &self,
        request: KnowledgeProviderWebhookRequest,
    ) -> Result<KnowledgeProviderWebhookResponse, KnowledgeServiceError> {
        let provider = self.provider(&request.provider)?;
        let headers = header_map(&request.headers)?;
        let body = webhook_body(&request)?;
        let verified = provider.verify_webhook(headers, body.into()).await?;
        let tenant_id = tenant_id_from_metadata(&verified.metadata)
            .or_else(|| tenant_id_from_metadata(&request.payload))
            .ok_or_else(|| {
                KnowledgeServiceError::InvalidRequest(
                    "verified webhook did not include tenant_id".to_string(),
                )
            })?;
        let connection_uid =
            uuid_from_metadata(&verified.metadata, &["connection_uid", "connection_id"]).or_else(
                || uuid_from_metadata(&request.payload, &["connection_uid", "connection_id"]),
            );
        let repository = self.repository(tenant_id);
        let recorded = repository
            .record_provider_event(KnowledgeProviderEventRecord {
                provider_event_uid: Uuid::now_v7(),
                tenant_id,
                connection_uid,
                provider: verified.provider,
                provider_event_id: verified.event_id,
                event_type: verified.event_type,
                status: "received".to_string(),
                payload: verified.metadata,
                duplicate: false,
            })
            .await?;

        let mut sync_run_uid = None;
        let mut ingestion_enqueued = false;
        if !recorded.duplicate
            && is_sync_completed_event(&recorded.event_type)
            && let Some(connection_uid) = recorded.connection_uid
        {
            let run = KnowledgeSyncRun {
                sync_run_uid: Uuid::now_v7(),
                tenant_id,
                connection_uid,
                parser: None,
                status: SyncRunStatus::Pending,
                records_seen: 0,
                records_ingested: 0,
                records_failed: 0,
                started_at: Utc::now(),
                finished_at: None,
            };
            repository.create_sync_run(run.clone()).await?;
            repository
                .record_ingestion_step(KnowledgeIngestionStep {
                    step_uid: Uuid::now_v7(),
                    sync_run_uid: run.sync_run_uid,
                    object_uid: None,
                    step: "ingestion_enqueued".to_string(),
                    status: IngestionStepStatus::Started,
                    started_at: run.started_at,
                    ended_at: None,
                    duration_ms: None,
                    counters: json!({
                        "provider_event_id": recorded.provider_event_id,
                        "provider": recorded.provider,
                    }),
                    summary: Some("Provider completed sync; ingestion accepted".to_string()),
                    retry_count: 0,
                    error_code: None,
                })
                .await?;
            sync_run_uid = Some(run.sync_run_uid);
            ingestion_enqueued = true;
        }

        Ok(KnowledgeProviderWebhookResponse {
            provider: recorded.provider,
            event_id: recorded.provider_event_id,
            status: recorded.status,
            duplicate: recorded.duplicate,
            sync_run_uid,
            ingestion_enqueued,
        })
    }
}

fn webhook_body(
    request: &KnowledgeProviderWebhookRequest,
) -> Result<Vec<u8>, KnowledgeServiceError> {
    match &request.body_base64 {
        Some(value) => general_purpose::STANDARD
            .decode(value)
            .or_else(|_| general_purpose::URL_SAFE_NO_PAD.decode(value))
            .map_err(|error| {
                KnowledgeServiceError::InvalidRequest(format!(
                    "webhook body_base64 was invalid: {error}"
                ))
            }),
        None => serde_json::to_vec(&request.payload).map_err(|error| {
            KnowledgeServiceError::InvalidRequest(format!("webhook payload encode failed: {error}"))
        }),
    }
}

fn header_map(headers: &[(String, String)]) -> Result<HeaderMap, KnowledgeServiceError> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            KnowledgeServiceError::InvalidRequest(format!("invalid webhook header name: {error}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            KnowledgeServiceError::InvalidRequest(format!("invalid webhook header value: {error}"))
        })?;
        map.insert(name, value);
    }
    Ok(map)
}

fn tenant_id_from_metadata(value: &Value) -> Option<TenantId> {
    string_from_metadata(value, &["tenant_id", "tenantId"])
        .or_else(|| nested_string(value, &["connection", "tenant_id"]))
        .and_then(|value| Uuid::parse_str(&value).ok())
        .map(TenantId::from)
}

fn uuid_from_metadata(value: &Value, keys: &[&str]) -> Option<Uuid> {
    string_from_metadata(value, keys).and_then(|value| Uuid::parse_str(&value).ok())
}

fn string_from_metadata(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    })
}

fn nested_string(value: &Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    current.as_str().map(ToOwned::to_owned)
}

fn is_sync_completed_event(event_type: &str) -> bool {
    let event_type = event_type.to_ascii_lowercase();
    event_type.contains("sync")
        && (event_type.contains("complete")
            || event_type.contains("completed")
            || event_type.contains("success"))
}
