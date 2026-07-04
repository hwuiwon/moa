//! Sync-run command and status logic for the Knowledge service.

use chrono::Utc;
use moa_core::wire::knowledge::{
    KnowledgeConnectionListRequest, KnowledgeConnectionListResponse, KnowledgeConnectionSummary,
    KnowledgeIntegrationListRequest, KnowledgeIntegrationListResponse, KnowledgeIntegrationSummary,
    KnowledgeSyncEventsRequest, KnowledgeSyncEventsResponse, KnowledgeSyncRequest,
    KnowledgeSyncResponse, KnowledgeSyncStatusRequest, KnowledgeSyncStatusResponse,
    KnowledgeSyncStepView, KnowledgeUnavailableProvider,
};
use moa_knowledge::domain::{
    IngestionStepStatus, KnowledgeIngestionStep, KnowledgeSyncRun, SyncRunStatus,
    TriggerSyncRequest,
};
use moa_knowledge::observability::{build_step_row, classify_failure, failed_outcome};
use moa_knowledge::providers::LinkedIntegrationProvider;
use moa_knowledge::repository::SyncRunClaim;
use moa_observability::record_knowledge_sync_run;
use serde_json::json;
use tracing::Instrument;
use uuid::Uuid;

use super::{KnowledgeService, KnowledgeServiceError, webhook::record_ingestion_enqueue_step};

impl KnowledgeService {
    /// Starts a provider sync and returns after enqueueing provider-side work.
    #[tracing::instrument(
        name = "knowledge_sync_run",
        skip(self, request),
        fields(
            tenant_id = %request.tenant_id,
            connection_id = %request.connection_uid,
            sync_run_id = tracing::field::Empty,
            provider = tracing::field::Empty,
            parser = %request.parser.as_deref().unwrap_or("default"),
            status = tracing::field::Empty,
            error_code = tracing::field::Empty
        )
    )]
    pub async fn sync_connection(
        &self,
        request: KnowledgeSyncRequest,
    ) -> Result<KnowledgeSyncResponse, KnowledgeServiceError> {
        let repository = self.repository(request.tenant_id);
        let connection = repository
            .get_connection(request.connection_uid)
            .await?
            .ok_or(KnowledgeServiceError::NotFound("knowledge connection"))?;
        if connection.tenant_id != request.tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }
        let now = Utc::now();
        let mut run = KnowledgeSyncRun {
            sync_run_uid: Uuid::now_v7(),
            tenant_id: request.tenant_id,
            connection_uid: request.connection_uid,
            parser: request.parser,
            max_records: request.max_records,
            status: SyncRunStatus::Queued,
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
            started_at: now,
            finished_at: None,
        };
        match repository.claim_sync_run(run.clone()).await? {
            SyncRunClaim::Claimed(claimed) => {
                run = claimed;
            }
            SyncRunClaim::AlreadyRunning(existing) => {
                return Ok(KnowledgeSyncResponse {
                    sync_run_uid: existing.sync_run_uid,
                    status: existing.status.as_str().to_string(),
                    started_at: existing.started_at,
                });
            }
        }
        let provider_label = connection.provider.clone();
        let current_span = tracing::Span::current();
        current_span.record("sync_run_id", tracing::field::display(run.sync_run_uid));
        current_span.record("provider", provider_label.as_str());
        current_span.record("status", run.status.as_str());
        current_span.record("error_code", "none");
        record_knowledge_sync_run(&provider_label, run.status.as_str());

        let provider = self.provider(&connection.provider)?;
        let provider_connection = self.connection_with_credential(&connection).await?;
        let provider_span = tracing::info_span!(
            "knowledge_provider_request",
            tenant_id = %request.tenant_id,
            connection_id = %request.connection_uid,
            sync_run_id = %run.sync_run_uid,
            provider = %provider_label,
            parser = %run.parser.as_deref().unwrap_or("default"),
            operation = "trigger_sync",
            status = tracing::field::Empty,
            error_code = tracing::field::Empty
        );
        let triggered = match provider
            .trigger_sync(TriggerSyncRequest {
                connection: provider_connection,
                model: None,
                variant: None,
            })
            .instrument(provider_span.clone())
            .await
        {
            Ok(triggered) => {
                provider_span.record("status", "accepted");
                provider_span.record("error_code", "none");
                triggered
            }
            Err(error) => {
                let classification = classify_failure("provider_triggered", &error);
                provider_span.record("status", "failed");
                provider_span.record("error_code", classification.error_code);
                run.status = if classification.retryable {
                    SyncRunStatus::FailedRetryable
                } else {
                    SyncRunStatus::FailedTerminal
                };
                run.records_failed = run.records_failed.saturating_add(1);
                run.error_code = Some(classification.error_code.to_string());
                run.finished_at = Some(Utc::now());
                repository.update_sync_run(run.clone()).await?;
                current_span.record("status", run.status.as_str());
                current_span.record("error_code", classification.error_code);
                repository
                    .record_ingestion_step(build_step_row(
                        run.sync_run_uid,
                        None,
                        "provider_triggered",
                        failed_outcome(classification),
                    ))
                    .await?;
                record_knowledge_sync_run(&provider_label, run.status.as_str());
                return Err(error.into());
            }
        };
        let provider_completed = provider_trigger_completed(&triggered.status);
        run.status = if provider_completed {
            SyncRunStatus::ProviderSynced
        } else {
            SyncRunStatus::ProviderSyncing
        };
        repository.update_sync_run(run.clone()).await?;
        current_span.record("status", run.status.as_str());
        current_span.record("error_code", "none");
        record_knowledge_sync_run(&provider_label, run.status.as_str());
        repository
            .record_ingestion_step(KnowledgeIngestionStep {
                step_uid: Uuid::now_v7(),
                sync_run_uid: run.sync_run_uid,
                object_uid: None,
                step: "provider_triggered".to_string(),
                status: IngestionStepStatus::Completed,
                started_at: now,
                ended_at: Some(Utc::now()),
                duration_ms: None,
                counters: json!({}),
                summary: Some("Provider sync accepted".to_string()),
                retry_count: 0,
                error_code: None,
            })
            .await?;
        if provider_completed {
            record_ingestion_enqueue_step(&*repository, &run).await?;
        }
        tracing::info!(
            sync_run_id = %run.sync_run_uid,
            provider = %provider_label,
            provider_status = %triggered.status,
            "knowledge provider sync accepted"
        );

        Ok(KnowledgeSyncResponse {
            sync_run_uid: run.sync_run_uid,
            status: run.status.as_str().to_string(),
            started_at: run.started_at,
        })
    }

    /// Reads local sync-run status and redacted step summaries.
    pub async fn sync_status(
        &self,
        request: KnowledgeSyncStatusRequest,
    ) -> Result<KnowledgeSyncStatusResponse, KnowledgeServiceError> {
        let repository = self.repository(request.tenant_id);
        let run = repository
            .get_sync_run(request.sync_run_uid)
            .await?
            .ok_or(KnowledgeServiceError::NotFound("knowledge sync run"))?;
        if run.tenant_id != request.tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge sync run"));
        }
        let steps = repository
            .sync_run_steps(request.sync_run_uid, None)
            .await?
            .into_iter()
            .map(step_view)
            .collect();

        Ok(KnowledgeSyncStatusResponse {
            sync_run_uid: run.sync_run_uid,
            status: run.status.as_str().to_string(),
            records_seen: run.records_seen,
            records_changed: run.records_changed,
            records_deleted: run.records_deleted,
            records_ingested: run.records_ingested,
            records_failed: run.records_failed,
            objects_parsed: run.objects_parsed,
            chunks_embedded: run.chunks_embedded,
            graph_nodes_upserted: run.graph_nodes_upserted,
            graph_edges_upserted: run.graph_edges_upserted,
            error_code: run.error_code.clone(),
            retry_classification: retry_classification(run.status).map(ToString::to_string),
            steps,
            started_at: run.started_at,
            finished_at: run.finished_at,
        })
    }

    /// Reads ordered ingestion events for one local sync run.
    pub async fn sync_events(
        &self,
        request: KnowledgeSyncEventsRequest,
    ) -> Result<KnowledgeSyncEventsResponse, KnowledgeServiceError> {
        let repository = self.repository(request.tenant_id);
        let run = repository
            .get_sync_run(request.sync_run_uid)
            .await?
            .ok_or(KnowledgeServiceError::NotFound("knowledge sync run"))?;
        if run.tenant_id != request.tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge sync run"));
        }
        let limit = request.limit.unwrap_or(100).min(500) as usize;
        let events = repository
            .sync_run_steps(request.sync_run_uid, request.object_uid)
            .await?
            .into_iter()
            .take(limit)
            .map(step_view)
            .collect();

        Ok(KnowledgeSyncEventsResponse {
            events,
            next_cursor: None,
        })
    }

    /// Lists tenant knowledge linked connections with last-sync status.
    pub async fn list_connections(
        &self,
        request: KnowledgeConnectionListRequest,
    ) -> Result<KnowledgeConnectionListResponse, KnowledgeServiceError> {
        let credential_refs = self
            .credentials
            .list_linked_account_refs(request.tenant_id)
            .await?;
        let connections = self
            .repository(request.tenant_id)
            .list_connections(request.tenant_id, request.provider.as_deref())
            .await?
            .into_iter()
            .map(|projection| KnowledgeConnectionSummary {
                credential_status: credential_status(
                    &projection.connection.credential_ref,
                    &credential_refs,
                ),
                connection_uid: projection.connection.connection_uid,
                provider: projection.connection.provider,
                connector: projection.connection.connector,
                provider_account_id: projection.connection.provider_account_id,
                status: projection.connection.status.as_str().to_string(),
                last_sync_status: projection
                    .last_sync_status
                    .map(|status| status.as_str().to_string()),
                last_synced_at: projection.connection.last_synced_at,
                source_selection: projection.connection.source_selection,
            })
            .collect();

        Ok(KnowledgeConnectionListResponse { connections })
    }

    /// Lists the integrations tenants can connect, across linked-account providers.
    ///
    /// With an explicit `provider` filter, resolver and provider errors propagate
    /// so misconfiguration is visible. Without a filter, providers that fail to
    /// resolve or list keep the response partial but are reported in
    /// `unavailable_providers`, so connect UIs can distinguish "no integrations"
    /// from provider misconfiguration. Results are sorted by provider, then
    /// integration id.
    pub async fn list_integrations(
        &self,
        request: KnowledgeIntegrationListRequest,
    ) -> Result<KnowledgeIntegrationListResponse, KnowledgeServiceError> {
        let mut integrations = Vec::new();
        let mut unavailable_providers = Vec::new();
        match request.provider.as_deref() {
            Some(provider_id) => {
                let provider = self.provider(provider_id)?;
                append_provider_integrations(&mut integrations, provider_id, provider.as_ref())
                    .await?;
            }
            None => {
                for provider_id in self.providers.provider_ids() {
                    let listing = match self.provider(&provider_id) {
                        Ok(provider) => {
                            append_provider_integrations(
                                &mut integrations,
                                &provider_id,
                                provider.as_ref(),
                            )
                            .await
                        }
                        Err(error) => Err(error),
                    };
                    if let Err(error) = listing {
                        tracing::warn!(
                            provider = %provider_id,
                            ?error,
                            "knowledge provider unavailable for integration listing"
                        );
                        unavailable_providers.push(KnowledgeUnavailableProvider {
                            provider: provider_id,
                            reason: error.to_string(),
                        });
                    }
                }
            }
        }
        integrations.sort_by(|a, b| a.provider.cmp(&b.provider).then_with(|| a.id.cmp(&b.id)));
        Ok(KnowledgeIntegrationListResponse {
            integrations,
            unavailable_providers,
        })
    }
}

fn credential_status(
    credential_ref: &str,
    credential_refs: &std::collections::BTreeSet<String>,
) -> Option<String> {
    if !credential_ref.starts_with("vault://") {
        return None;
    }
    if credential_refs.contains(credential_ref) {
        Some("present".to_string())
    } else {
        Some("missing".to_string())
    }
}

/// Appends one provider's connectable integrations as wire summaries.
async fn append_provider_integrations(
    integrations: &mut Vec<KnowledgeIntegrationSummary>,
    provider_id: &str,
    provider: &dyn LinkedIntegrationProvider,
) -> Result<(), KnowledgeServiceError> {
    for integration in provider.list_integrations().await? {
        integrations.push(KnowledgeIntegrationSummary {
            provider: provider_id.to_string(),
            id: integration.id,
            display_name: integration.display_name,
            logo_url: integration.logo_url,
        });
    }
    Ok(())
}

fn provider_trigger_completed(status: &str) -> bool {
    let normalized = status.trim().to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "completed" | "complete" | "success" | "succeeded"
    )
}

pub(crate) fn step_view(step: KnowledgeIngestionStep) -> KnowledgeSyncStepView {
    let metadata = step_metadata(&step);
    KnowledgeSyncStepView {
        step_uid: step.step_uid,
        step: step.step,
        status: step.status.as_str().to_string(),
        object_uid: step.object_uid,
        preview: step.summary,
        metadata,
        created_at: step.started_at,
    }
}

fn retry_classification(status: SyncRunStatus) -> Option<&'static str> {
    match status {
        SyncRunStatus::FailedRetryable => Some("retryable"),
        SyncRunStatus::FailedTerminal => Some("terminal"),
        _ => None,
    }
}

fn step_metadata(step: &KnowledgeIngestionStep) -> serde_json::Value {
    let mut metadata = match &step.counters {
        serde_json::Value::Object(map) => serde_json::Value::Object(map.clone()),
        _ => json!({}),
    };
    if let serde_json::Value::Object(map) = &mut metadata {
        map.insert("retry_count".to_string(), json!(step.retry_count));
        if let Some(error_code) = &step.error_code {
            map.insert("error_code".to_string(), json!(error_code));
        }
    }
    metadata
}
