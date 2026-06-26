//! Sync-run command and status logic for the Knowledge service.

use chrono::Utc;
use moa_core::wire::knowledge::{
    KnowledgeConnectionListRequest, KnowledgeConnectionListResponse, KnowledgeConnectionSummary,
    KnowledgeSyncEventsRequest, KnowledgeSyncEventsResponse, KnowledgeSyncRequest,
    KnowledgeSyncResponse, KnowledgeSyncStatusRequest, KnowledgeSyncStatusResponse,
    KnowledgeSyncStepView,
};
use moa_knowledge::domain::{
    IngestionStepStatus, KnowledgeIngestionStep, KnowledgeSyncRun, SyncRunStatus,
    TriggerSyncRequest,
};
use serde_json::json;
use uuid::Uuid;

use super::{KnowledgeService, KnowledgeServiceError};

impl KnowledgeService {
    /// Starts a provider sync and returns after enqueueing provider-side work.
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
            status: SyncRunStatus::Pending,
            records_seen: 0,
            records_ingested: 0,
            records_failed: 0,
            started_at: now,
            finished_at: None,
        };
        repository.create_sync_run(run.clone()).await?;

        let triggered = self
            .provider(&connection.provider)?
            .trigger_sync(TriggerSyncRequest {
                connection,
                model: None,
            })
            .await?;
        run.status = SyncRunStatus::Running;
        repository.update_sync_run(run.clone()).await?;
        repository
            .record_ingestion_step(KnowledgeIngestionStep {
                step_uid: Uuid::now_v7(),
                sync_run_uid: run.sync_run_uid,
                object_uid: None,
                step: "provider_sync_triggered".to_string(),
                status: IngestionStepStatus::Completed,
                started_at: now,
                ended_at: Some(Utc::now()),
                duration_ms: None,
                counters: json!({
                    "provider_status": triggered.status,
                    "provider_sync_id": triggered.provider_sync_id,
                }),
                summary: Some("Provider sync accepted".to_string()),
                retry_count: 0,
                error_code: None,
            })
            .await?;

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
            records_ingested: run.records_ingested,
            records_failed: run.records_failed,
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
        let connections = self
            .repository(request.tenant_id)
            .list_connections(request.tenant_id, request.provider.as_deref())
            .await?
            .into_iter()
            .map(|projection| KnowledgeConnectionSummary {
                connection_uid: projection.connection.connection_uid,
                provider: projection.connection.provider,
                connector: projection.connection.connector,
                provider_account_id: projection.connection.provider_account_id,
                status: projection.connection.status.as_str().to_string(),
                last_sync_status: projection
                    .last_sync_status
                    .map(|status| status.as_str().to_string()),
                last_synced_at: projection.connection.last_synced_at,
            })
            .collect();

        Ok(KnowledgeConnectionListResponse { connections })
    }
}

pub(crate) fn step_view(step: KnowledgeIngestionStep) -> KnowledgeSyncStepView {
    KnowledgeSyncStepView {
        step_uid: step.step_uid,
        step: step.step,
        status: step.status.as_str().to_string(),
        object_uid: step.object_uid,
        preview: step.summary,
        metadata: step.counters,
        created_at: step.started_at,
    }
}
