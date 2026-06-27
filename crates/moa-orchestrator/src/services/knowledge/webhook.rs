//! Signed provider webhook handling for the Knowledge service.

use base64::{Engine as _, engine::general_purpose};
use chrono::Utc;
use moa_core::{
    TenantId,
    wire::knowledge::{KnowledgeProviderWebhookRequest, KnowledgeProviderWebhookResponse},
};
use moa_knowledge::domain::{
    IngestionStepStatus, KnowledgeConnection, KnowledgeIngestionStep, KnowledgeProviderEventRecord,
    KnowledgeSyncRun, SyncRunStatus,
};
use moa_knowledge::repository::ProviderAccountConnectionLookup;
use moa_observability::record_knowledge_sync_run;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};
use tracing::Instrument;
use uuid::Uuid;

use super::{KnowledgeService, KnowledgeServiceError};

impl KnowledgeService {
    /// Verifies, records, and idempotently handles one provider webhook delivery.
    pub async fn provider_webhook(
        &self,
        request: KnowledgeProviderWebhookRequest,
    ) -> Result<KnowledgeProviderWebhookResponse, KnowledgeServiceError> {
        let verifier = self.webhook_verifier(&request.provider)?;
        let headers = header_map(&request.headers)?;
        let body = webhook_body(&request)?;
        let verify_span = tracing::info_span!(
            "knowledge_provider_request",
            tenant_id = tracing::field::Empty,
            connection_id = tracing::field::Empty,
            sync_run_id = tracing::field::Empty,
            provider = %request.provider,
            parser = "webhook",
            operation = "verify_webhook",
            status = tracing::field::Empty,
            error_code = tracing::field::Empty
        );
        let verified = match verifier
            .verify_webhook(headers, body.into())
            .instrument(verify_span.clone())
            .await
        {
            Ok(verified) => {
                verify_span.record("status", "accepted");
                verify_span.record("error_code", "none");
                tracing::info!(
                    provider = %request.provider,
                    "knowledge provider webhook accepted"
                );
                verified
            }
            Err(error) => {
                verify_span.record("status", "failed");
                verify_span.record("error_code", "webhook_verification_failed");
                tracing::warn!(
                    provider = %request.provider,
                    error_code = "webhook_verification_failed",
                    "knowledge provider webhook rejected"
                );
                return Err(error.into());
            }
        };
        let binding = self
            .resolve_verified_webhook_binding(&verified.provider, &verified.metadata)
            .await?;
        verify_span.record("tenant_id", tracing::field::display(binding.tenant_id));
        verify_span.record(
            "connection_id",
            tracing::field::display(binding.connection_uid),
        );
        let repository = self.repository(binding.tenant_id);
        let recorded = repository
            .record_provider_event(KnowledgeProviderEventRecord {
                provider_event_uid: Uuid::now_v7(),
                tenant_id: binding.tenant_id,
                connection_uid: Some(binding.connection_uid),
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
        if !recorded.duplicate && should_enqueue_ingestion(&recorded.provider, &recorded.event_type)
        {
            let connection_uid = binding.connection_uid;
            if let Some(mut run) = repository
                .latest_sync_run_for_connection(connection_uid, &[SyncRunStatus::ProviderSyncing])
                .await?
            {
                run.status = SyncRunStatus::ProviderSynced;
                repository.update_sync_run(run.clone()).await?;
                verify_span.record("sync_run_id", tracing::field::display(run.sync_run_uid));
                record_knowledge_sync_run(&recorded.provider, run.status.as_str());
                record_ingestion_enqueue_step(&*repository, &run).await?;
                sync_run_uid = Some(run.sync_run_uid);
                ingestion_enqueued = true;
            } else if let Some(run) = repository
                .latest_sync_run_for_connection(connection_uid, non_terminal_sync_run_statuses())
                .await?
            {
                verify_span.record("sync_run_id", tracing::field::display(run.sync_run_uid));
                sync_run_uid = Some(run.sync_run_uid);
            } else {
                let run = KnowledgeSyncRun {
                    sync_run_uid: Uuid::now_v7(),
                    tenant_id: binding.tenant_id,
                    connection_uid,
                    parser: None,
                    max_records: None,
                    status: SyncRunStatus::ProviderSynced,
                    records_seen: 0,
                    records_changed: 0,
                    records_deleted: 0,
                    records_ingested: 0,
                    records_failed: 0,
                    objects_parsed: 0,
                    chunks_embedded: 0,
                    graph_nodes_upserted: 0,
                    graph_edges_upserted: 0,
                    error_code: None,
                    started_at: Utc::now(),
                    finished_at: None,
                };
                repository.create_sync_run(run.clone()).await?;
                verify_span.record("sync_run_id", tracing::field::display(run.sync_run_uid));
                record_knowledge_sync_run(&recorded.provider, run.status.as_str());
                record_ingestion_enqueue_step(&*repository, &run).await?;
                sync_run_uid = Some(run.sync_run_uid);
                ingestion_enqueued = true;
            }
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

    async fn resolve_verified_webhook_binding(
        &self,
        provider: &str,
        metadata: &Value,
    ) -> Result<VerifiedWebhookBinding, KnowledgeServiceError> {
        if let (Some(tenant_id), Some(connection_uid)) = (
            tenant_id_from_metadata(metadata),
            uuid_from_metadata(metadata, &["connection_uid"]),
        ) {
            let repository = self.repository(tenant_id);
            let connection = repository
                .get_connection(connection_uid)
                .await?
                .ok_or(KnowledgeServiceError::NotFound("knowledge connection"))?;
            if connection.tenant_id != tenant_id
                || !connection_provider_matches_webhook(provider, &connection.provider)
            {
                return Err(KnowledgeServiceError::NotFound("knowledge connection"));
            }
            return Ok(VerifiedWebhookBinding {
                tenant_id,
                connection_uid,
            });
        }

        let Some(candidate) = provider_account_binding_candidate(provider, metadata) else {
            return Err(KnowledgeServiceError::InvalidRequest(
                "verified webhook did not include tenant_id and connection_uid or a provider account binding".to_string(),
            ));
        };

        let lookup = self
            .webhook_lookup_repository()
            .lookup_connection_by_provider_account(
                provider,
                candidate.connector.as_deref(),
                &candidate.provider_account_id,
            )
            .await?;
        let connection = match lookup {
            ProviderAccountConnectionLookup::NotFound => {
                return Err(KnowledgeServiceError::NotFound("knowledge connection"));
            }
            ProviderAccountConnectionLookup::Unique(connection) => connection,
            ProviderAccountConnectionLookup::Ambiguous { .. } => {
                return Err(KnowledgeServiceError::InvalidRequest(
                    "verified webhook provider account binding matched multiple knowledge connections".to_string(),
                ));
            }
        };
        if let Some(signed_tenant_id) = tenant_id_from_metadata(metadata)
            && connection.tenant_id != signed_tenant_id
        {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }
        verified_binding_from_connection(provider, connection)
    }
}

pub(super) async fn record_ingestion_enqueue_step(
    repository: &dyn moa_knowledge::repository::KnowledgeRepository,
    run: &KnowledgeSyncRun,
) -> Result<(), KnowledgeServiceError> {
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
            counters: json!({}),
            summary: Some("Provider completed sync; ingestion accepted".to_string()),
            retry_count: 0,
            error_code: None,
        })
        .await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VerifiedWebhookBinding {
    tenant_id: TenantId,
    connection_uid: Uuid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderAccountBindingCandidate {
    connector: Option<String>,
    provider_account_id: String,
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

fn provider_account_binding_candidate(
    provider: &str,
    metadata: &Value,
) -> Option<ProviderAccountBindingCandidate> {
    match provider {
        "nango" => {
            let provider_account_id = string_from_metadata(metadata, &["connection_id"])?;
            let connector = string_from_metadata(metadata, &["provider_config_key"]);
            Some(ProviderAccountBindingCandidate {
                connector,
                provider_account_id,
            })
        }
        "merge" => {
            let provider_account_id = nested_string(metadata, &["linked_account", "id"])
                .or_else(|| string_from_metadata(metadata, &["linked_account_id"]))?;
            Some(ProviderAccountBindingCandidate {
                connector: None,
                provider_account_id,
            })
        }
        _ => None,
    }
}

fn verified_binding_from_connection(
    provider: &str,
    connection: KnowledgeConnection,
) -> Result<VerifiedWebhookBinding, KnowledgeServiceError> {
    if !connection_provider_matches_webhook(provider, &connection.provider) {
        return Err(KnowledgeServiceError::NotFound("knowledge connection"));
    }
    Ok(VerifiedWebhookBinding {
        tenant_id: connection.tenant_id,
        connection_uid: connection.connection_uid,
    })
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

fn should_enqueue_ingestion(provider: &str, event_type: &str) -> bool {
    // Keep this conservative: every new provider event name must be pinned in
    // provider adapter tests before enabling ingestion dispatch here.
    match provider {
        "nango" => matches!(event_type, "sync.completed" | "sync:completed"),
        "merge" => matches!(event_type, "linked_account.synced"),
        _ => false,
    }
}

fn is_parser_origin_provider(provider: &str) -> bool {
    matches!(provider, "llamaparse" | "reducto" | "unstructured")
}

fn connection_provider_matches_webhook(webhook_provider: &str, connection_provider: &str) -> bool {
    connection_provider == webhook_provider || is_parser_origin_provider(webhook_provider)
}

fn non_terminal_sync_run_statuses() -> &'static [SyncRunStatus] {
    &[
        SyncRunStatus::Queued,
        SyncRunStatus::ProviderSyncing,
        SyncRunStatus::ProviderSynced,
        SyncRunStatus::ParsePending,
        SyncRunStatus::Ingesting,
        SyncRunStatus::FailedRetryable,
    ]
}
