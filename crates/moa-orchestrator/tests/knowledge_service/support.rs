//! Shared fixtures for the tenant Knowledge service integration-test modules.

mod connections;
mod ingestion;
mod inspection;
mod trace;
mod webhook;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use moa_core::types::memory::{InformationBarrierId, RlsContext};
use moa_core::types::security::SensitivityClass;
use moa_core::{
    traits::EmbeddingProvider, types::contact::ContactId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::identifiers::UserId,
};
use moa_db::ScopedConn;
use moa_knowledge::{
    Error as KnowledgeError,
    chunking::ChunkingConfig,
    contact_groups::derive_contact_groups_from_object_with_resolved_members,
    domain::{
        ApplySourceSelectionRequest, ConnectionStatus, ContactGroup, ContactGroupMembership,
        ContactGroupTarget, CreateLinkTokenRequest, DocumentElement, DocumentElementKind,
        DocumentVersion, ElementLayout, ExchangePublicTokenRequest, KnowledgeBlock, KnowledgeChunk,
        KnowledgeConnection, KnowledgeConnectionProjection, KnowledgeIngestionStep,
        KnowledgeObject, KnowledgeObjectInspection, KnowledgeObjectProjection,
        KnowledgeProviderEventRecord, KnowledgeSyncCounters, KnowledgeSyncRun, LinkToken,
        LinkedAccount, ListChangedRecordsRequest, ObjectStatus, ParseInput, ParsedDocument,
        ProviderIntegration, ProviderRecord, RecordPage, SyncRunStatus, TriggerSyncRequest,
        TriggeredSync, WebhookEvent,
    },
    ingestion::{
        KnowledgeIngestionPipeline, KnowledgeIngestionPipelineConfig, MemoryKnowledgeGraphWriter,
        PageIngestionReport,
    },
    parser::DocumentParser,
    providers::LinkedIntegrationProvider,
    repository::{
        DocumentVersionIngestionClaim, KnowledgeDiscoveryStore, KnowledgeRepository,
        PostgresKnowledgeRepository, ProviderAccountConnectionLookup, SyncRunClaim,
    },
};
use moa_lineage_core::{
    BackendIntrospection, FusedHit, GraphPath, LineageEvent, RecordKind, RerankHit,
    RetrievalLineage, RetrievalSelectedHit, RetrievalStage, StageTimings, TurnId, VecHit,
};
use moa_memory_graph::{GraphStore, NodeLabel, NodeWriteIntent, PostgresGraphStore};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION};
use moa_orchestrator::services::knowledge::{
    KnowledgeCredentialStore, KnowledgeIngestionRunner, KnowledgeService, KnowledgeServiceError,
    KnowledgeWebhookVerifier, ParserWebhookVerifier, StaticKnowledgeProviders,
};
use moa_orchestrator::workflows::knowledge_sync_ingestion::{
    KnowledgeSyncIngestionRequest, KnowledgeSyncIngestionSteps, KnowledgeSyncPageApplication,
    KnowledgeSyncPreparedRun, KnowledgeSyncProviderPage, run_knowledge_sync_ingestion_workflow,
};
use moa_wire::knowledge::{
    KnowledgeConnectionListRequest, KnowledgeDisconnectConnectionRequest,
    KnowledgeExchangeTokenRequest, KnowledgeIntegrationListRequest, KnowledgeObjectInspectRequest,
    KnowledgeObjectListRequest, KnowledgeProviderWebhookRequest, KnowledgeQueryTraceRequest,
    KnowledgeSyncEventsRequest, KnowledgeSyncRequest, KnowledgeSyncStatusRequest,
    KnowledgeUpdateConnectionSourceSelectionRequest,
};
use reqwest::header::HeaderMap;
use restate_sdk::prelude::{HandlerError, TerminalError};
use serde_json::{Value, json};
use sha2::Sha256;
use tokio_util::bytes::Bytes;
use uuid::Uuid;

const PROVIDER: &str = "fake";
const CONNECTOR: &str = "drive";
const SECRET_TOKEN: &str = "provider-secret-token-123";
const SECRET_BEARER: &str = "Bearer provider-secret-token-456";
const RAW_DOCUMENT_TAIL: &str = "RAW_FULL_DOCUMENT_TAIL_SHOULD_NOT_APPEAR";

fn fixture_service(
    repository: Arc<InMemoryKnowledgeRepository>,
    provider: Arc<dyn LinkedIntegrationProvider>,
    max_preview_chars: usize,
) -> KnowledgeService {
    KnowledgeService::new(
        repository.clone(),
        repository,
        Arc::new(StaticKnowledgeProviders::new().with_provider(PROVIDER, provider)),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        max_preview_chars,
    )
}

fn fixture_webhook_service(
    repository: Arc<InMemoryKnowledgeRepository>,
    provider: &'static str,
    max_preview_chars: usize,
) -> KnowledgeService {
    KnowledgeService::new(
        repository.clone(),
        repository,
        Arc::new(
            StaticKnowledgeProviders::new()
                .with_webhook_verifier(provider, Arc::new(PayloadWebhookVerifier::new(provider))),
        ),
        Arc::new(FakeKnowledgeCredentialStore::default()),
        fake_ingestion_runner(),
        max_preview_chars,
    )
}

fn fake_ingestion_runner() -> Arc<dyn KnowledgeIngestionRunner> {
    Arc::new(FakeKnowledgeIngestionRunner::default())
}

#[derive(Debug)]
struct FakeKnowledgeSyncIngestionSteps {
    prepared: KnowledgeSyncPreparedRun,
    pages: Vec<RecordPage>,
    list_calls: Vec<FakeListPageCall>,
    apply_calls: Vec<FakeApplyPageCall>,
    prune_calls: Vec<FakePruneCall>,
    fail_calls: Vec<FakeFailCall>,
    status_transitions: Vec<SyncRunStatus>,
}

impl FakeKnowledgeSyncIngestionSteps {
    fn new(prepared: KnowledgeSyncPreparedRun) -> Self {
        Self {
            prepared,
            pages: Vec::new(),
            list_calls: Vec::new(),
            apply_calls: Vec::new(),
            prune_calls: Vec::new(),
            fail_calls: Vec::new(),
            status_transitions: Vec::new(),
        }
    }

    fn with_pages(mut self, pages: Vec<RecordPage>) -> Self {
        self.pages = pages;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeListPageCall {
    cursor: Option<String>,
    limit: u32,
    page_index: u32,
    credential_ref: String,
    modified_after: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeListChangedRecordsRequest {
    connection_uid: Uuid,
    cursor: Option<String>,
    limit: Option<u32>,
    modified_after: Option<DateTime<Utc>>,
    variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeApplyPageCall {
    page_index: u32,
    source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakePruneCall {
    source_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeFailCall {
    stage: &'static str,
    error_message: String,
}

#[async_trait]
impl KnowledgeSyncIngestionSteps for FakeKnowledgeSyncIngestionSteps {
    async fn prepare_ingestion_run(
        &mut self,
        request: &KnowledgeSyncIngestionRequest,
    ) -> Result<KnowledgeSyncPreparedRun, HandlerError> {
        if request.sync_run_uid != self.prepared.run.sync_run_uid {
            return Err(TerminalError::new_with_code(404, "sync run mismatch").into());
        }
        self.status_transitions.push(self.prepared.run.status);
        self.prepared.run.status = SyncRunStatus::Ingesting;
        self.status_transitions.push(self.prepared.run.status);
        Ok(self.prepared.clone())
    }

    async fn list_changed_records_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        cursor: Option<String>,
        limit: u32,
        page_index: u32,
    ) -> Result<KnowledgeSyncProviderPage, HandlerError> {
        self.list_calls.push(FakeListPageCall {
            cursor,
            limit,
            page_index,
            credential_ref: prepared.connection.credential_ref.clone(),
            modified_after: prepared.connection.last_synced_at,
        });
        let page = if self.pages.is_empty() {
            RecordPage {
                records: Vec::new(),
                next_cursor: None,
            }
        } else {
            self.pages.remove(0)
        };
        let records_listed = page.records.len() as u64;
        Ok(KnowledgeSyncProviderPage {
            provider: prepared.provider.clone(),
            page,
            records_listed,
        })
    }

    async fn apply_record_page(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        page: KnowledgeSyncProviderPage,
        page_index: u32,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let source_ids = page
            .page
            .records
            .iter()
            .map(|record| record.source_id.clone())
            .collect::<Vec<_>>();
        let records_applied = source_ids.len() as u64;
        self.apply_calls.push(FakeApplyPageCall {
            page_index,
            source_ids,
        });
        Ok(KnowledgeSyncPageApplication {
            records_listed: page.records_listed,
            records_ingested: records_applied,
            records_skipped: 0,
            records_deleted: 0,
            embeddings_created: 0,
            records_applied,
        })
    }

    async fn prune_unseen_objects(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        seen_source_ids: HashSet<String>,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let mut source_ids = seen_source_ids.into_iter().collect::<Vec<_>>();
        source_ids.sort();
        self.prune_calls.push(FakePruneCall { source_ids });
        Ok(KnowledgeSyncPageApplication {
            records_listed: 0,
            records_ingested: 0,
            records_skipped: 0,
            records_deleted: 0,
            embeddings_created: 0,
            records_applied: 0,
        })
    }

    async fn complete_ingestion_run(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
    ) -> Result<(), HandlerError> {
        self.status_transitions.push(SyncRunStatus::Completed);
        Ok(())
    }

    async fn fail_ingestion_run(
        &mut self,
        _prepared: &KnowledgeSyncPreparedRun,
        stage: &'static str,
        error_message: String,
    ) -> Result<(), HandlerError> {
        self.fail_calls.push(FakeFailCall {
            stage,
            error_message,
        });
        Ok(())
    }
}

fn fake_prepared_sync_run(
    tenant_id: TenantId,
    connection_uid: Uuid,
    sync_run_uid: Uuid,
    max_records: u32,
) -> KnowledgeSyncPreparedRun {
    KnowledgeSyncPreparedRun {
        run: KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("native".to_string()),
            max_records: Some(max_records),
            information_barrier: None,
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
        },
        connection: KnowledgeConnection {
            connection_uid,
            tenant_id,
            provider: PROVIDER.to_string(),
            connector: CONNECTOR.to_string(),
            provider_account_id: "provider-account-1".to_string(),
            credential_ref: "resolved-provider-token".to_string(),
            status: ConnectionStatus::Active,
            metadata: json!({}),
            source_selection: json!({}),
            information_barrier: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            last_synced_at: None,
        },
        provider: PROVIDER.to_string(),
        parser_label: "native".to_string(),
        page_size: 100,
        max_records,
    }
}

fn fake_record_page(source_ids: &[&str], next_cursor: Option<&str>) -> RecordPage {
    RecordPage {
        records: source_ids
            .iter()
            .map(|source_id| ProviderRecord {
                source_id: (*source_id).to_string(),
                object_type: "document".to_string(),
                title: Some((*source_id).to_string()),
                source_uri: None,
                change_token: Some(format!("{source_id}-etag")),
                deleted: false,
                source_updated_at: None,
                metadata: json!({}),
                payload: json!({ "text": source_id }),
            })
            .collect(),
        next_cursor: next_cursor.map(ToOwned::to_owned),
    }
}

type Task14KnowledgeIngestionPipeline = KnowledgeIngestionPipeline<
    PostgresKnowledgeRepository,
    Task14Parser,
    Task14Embedder,
    MemoryKnowledgeGraphWriter<PostgresGraphStore>,
>;

struct DbKnowledgeAutoSyncSteps {
    repository: Arc<PostgresKnowledgeRepository>,
    provider: Arc<Task14LinkedIntegrationProvider>,
    pipeline: Arc<Task14KnowledgeIngestionPipeline>,
    page_size: u32,
    parser_label: String,
}

impl DbKnowledgeAutoSyncSteps {
    fn new(
        repository: Arc<PostgresKnowledgeRepository>,
        provider: Arc<Task14LinkedIntegrationProvider>,
        pipeline: Arc<Task14KnowledgeIngestionPipeline>,
        page_size: u32,
        parser_label: impl Into<String>,
    ) -> Self {
        Self {
            repository,
            provider,
            pipeline,
            page_size,
            parser_label: parser_label.into(),
        }
    }
}

#[async_trait]
impl KnowledgeSyncIngestionSteps for DbKnowledgeAutoSyncSteps {
    async fn prepare_ingestion_run(
        &mut self,
        request: &KnowledgeSyncIngestionRequest,
    ) -> Result<KnowledgeSyncPreparedRun, HandlerError> {
        let mut run = self
            .repository
            .get_sync_run(request.sync_run_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge sync run not found"))?;
        let connection = self
            .repository
            .get_connection(run.connection_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge connection not found"))?;
        if connection.tenant_id != run.tenant_id || connection.connection_uid != run.connection_uid
        {
            return Err(
                TerminalError::new_with_code(404, "knowledge connection tenant mismatch").into(),
            );
        }
        let max_records = run.max_records.unwrap_or(100);
        run.status = SyncRunStatus::Ingesting;
        run.parser = Some(self.parser_label.clone());
        self.repository
            .update_sync_run(run.clone())
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPreparedRun {
            provider: connection.provider.clone(),
            run,
            connection,
            parser_label: self.parser_label.clone(),
            page_size: self.page_size,
            max_records,
        })
    }

    async fn list_changed_records_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        cursor: Option<String>,
        limit: u32,
        _page_index: u32,
    ) -> Result<KnowledgeSyncProviderPage, HandlerError> {
        let page = self
            .provider
            .list_changed_records(ListChangedRecordsRequest {
                connection: prepared.connection.clone(),
                cursor,
                modified_after: prepared.connection.last_synced_at,
                limit: Some(limit),
                variant: None,
            })
            .await
            .map_err(test_handler_error)?;
        let records_listed = page.records.len() as u64;
        Ok(KnowledgeSyncProviderPage {
            provider: prepared.provider.clone(),
            page,
            records_listed,
        })
    }

    async fn apply_record_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        page: KnowledgeSyncProviderPage,
        _page_index: u32,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let report = self
            .pipeline
            .ingest_record_page(
                prepared.run.sync_run_uid,
                prepared.run.connection_uid,
                prepared.run.tenant_id,
                page.page,
            )
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPageApplication::from(report))
    }

    async fn prune_unseen_objects(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        seen_source_ids: HashSet<String>,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let report = self
            .pipeline
            .prune_unseen_objects(
                prepared.run.sync_run_uid,
                prepared.run.connection_uid,
                prepared.run.tenant_id,
                &seen_source_ids,
            )
            .await
            .map_err(test_handler_error)?;
        Ok(KnowledgeSyncPageApplication::from(report))
    }

    async fn complete_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
    ) -> Result<(), HandlerError> {
        let mut run = self
            .repository
            .get_sync_run(prepared.run.sync_run_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge sync run not found"))?;
        run.status = SyncRunStatus::Completed;
        run.error_code = None;
        run.finished_at = Some(Utc::now());
        self.repository
            .update_sync_run(run)
            .await
            .map_err(test_handler_error)?;

        let mut connection = self
            .repository
            .get_connection(prepared.run.connection_uid)
            .await
            .map_err(test_handler_error)?
            .ok_or_else(|| TerminalError::new_with_code(404, "knowledge connection not found"))?;
        connection.last_synced_at = Some(Utc::now());
        self.repository
            .upsert_connection(connection)
            .await
            .map_err(test_handler_error)?;
        Ok(())
    }

    async fn fail_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        stage: &'static str,
        error_message: String,
    ) -> Result<(), HandlerError> {
        let Some(mut run) = self
            .repository
            .get_sync_run(prepared.run.sync_run_uid)
            .await
            .map_err(test_handler_error)?
        else {
            return Ok(());
        };
        let classification = moa_knowledge::observability::classify_failure(
            stage,
            &KnowledgeError::provider(prepared.provider.clone(), error_message),
        );
        run.status = if classification.retryable {
            SyncRunStatus::FailedRetryable
        } else {
            SyncRunStatus::FailedTerminal
        };
        run.records_failed = run.records_failed.saturating_add(1);
        run.error_code = Some(classification.error_code.to_string());
        run.finished_at = Some(Utc::now());
        self.repository
            .update_sync_run(run)
            .await
            .map_err(test_handler_error)?;
        Ok(())
    }
}

async fn create_provider_synced_run(
    repository: &PostgresKnowledgeRepository,
    tenant_id: TenantId,
    connection_uid: Uuid,
    max_records: Option<u32>,
) -> Uuid {
    let sync_run_uid = Uuid::now_v7();
    repository
        .create_sync_run(KnowledgeSyncRun {
            sync_run_uid,
            tenant_id,
            connection_uid,
            parser: Some("task14".to_string()),
            max_records,
            information_barrier: None,
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
        })
        .await
        .expect("create provider-synced sync run");
    sync_run_uid
}

fn task14_ingestion_pipeline(
    pool: sqlx::PgPool,
    repository: Arc<PostgresKnowledgeRepository>,
    tenant_id: TenantId,
    provider: &str,
) -> Arc<Task14KnowledgeIngestionPipeline> {
    let scope = RlsContext::tenant(tenant_id);
    let vector = Arc::new(PgvectorStore::new_for_app_role(pool.clone(), scope.clone()));
    let graph_store = Arc::new(
        PostgresGraphStore::scoped_for_app_role(
            pool,
            scope.clone(),
            Arc::new(moa_crypto::LocalKmsProvider::new()),
        )
        .with_vector_store(vector),
    );
    let graph_writer = Arc::new(MemoryKnowledgeGraphWriter::new(
        graph_store,
        MemoryScope::Tenant { tenant_id },
        "knowledge-auto-sync-test",
        None,
    ));
    Arc::new(KnowledgeIngestionPipeline::new(
        repository,
        Arc::new(Task14Parser),
        Arc::new(Task14Embedder),
        graph_writer,
        KnowledgeIngestionPipelineConfig {
            chunking: ChunkingConfig {
                target_tokens: 128,
                max_tokens: 256,
                min_tokens: 1,
            },
            provider: provider.to_string(),
            parser_label: "task14".to_string(),
        },
    ))
}

fn test_handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

fn handler_error_text(error: &HandlerError) -> String {
    let error_ref = <HandlerError as AsRef<dyn std::error::Error + Send + Sync>>::as_ref(error);
    error_ref.to_string()
}

#[derive(Debug, Clone, Default)]
struct FakeKnowledgeIngestionRunner {
    calls: Arc<Mutex<Vec<FakeKnowledgeIngestionCall>>>,
}

impl FakeKnowledgeIngestionRunner {
    fn calls(&self) -> Vec<FakeKnowledgeIngestionCall> {
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FakeKnowledgeIngestionCall {
    sync_run_uid: Uuid,
    connection_uid: Uuid,
    tenant_id: TenantId,
    provider: String,
    records_listed: u64,
}

#[async_trait]
impl KnowledgeIngestionRunner for FakeKnowledgeIngestionRunner {
    async fn ingest_record_page(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        page: RecordPage,
    ) -> Result<PageIngestionReport, KnowledgeServiceError> {
        let records_listed = page.records.len() as u64;
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .push(FakeKnowledgeIngestionCall {
                sync_run_uid: run.sync_run_uid,
                connection_uid: run.connection_uid,
                tenant_id: run.tenant_id,
                provider: provider.to_string(),
                records_listed,
            });
        Ok(PageIngestionReport {
            records_listed,
            records_ingested: records_listed,
            ..PageIngestionReport::default()
        })
    }

    async fn prune_unseen_objects(
        &self,
        run: &KnowledgeSyncRun,
        provider: &str,
        seen_source_ids: &HashSet<String>,
    ) -> Result<PageIngestionReport, KnowledgeServiceError> {
        self.calls
            .lock()
            .expect("fake ingestion runner calls should not be poisoned")
            .push(FakeKnowledgeIngestionCall {
                sync_run_uid: run.sync_run_uid,
                connection_uid: run.connection_uid,
                tenant_id: run.tenant_id,
                provider: provider.to_string(),
                records_listed: seen_source_ids.len() as u64,
            });
        Ok(PageIngestionReport::default())
    }
}

fn fixture_connection(tenant_id: TenantId) -> KnowledgeConnection {
    KnowledgeConnection {
        connection_uid: Uuid::now_v7(),
        tenant_id,
        provider: PROVIDER.to_string(),
        connector: CONNECTOR.to_string(),
        provider_account_id: "provider-account-1".to_string(),
        credential_ref: "vault://existing".to_string(),
        status: ConnectionStatus::Active,
        metadata: json!({ "safe": "connection" }),
        source_selection: json!({}),
        information_barrier: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        last_synced_at: None,
    }
}

fn fixture_connection_for_provider(
    tenant_id: TenantId,
    provider: &str,
    connector: &str,
    provider_account_id: &str,
) -> KnowledgeConnection {
    let mut connection = fixture_connection(tenant_id);
    connection.provider = provider.to_string();
    connection.connector = connector.to_string();
    connection.provider_account_id = provider_account_id.to_string();
    connection
}

fn fixture_object(tenant_id: TenantId, connection_uid: Uuid) -> KnowledgeObject {
    KnowledgeObject {
        object_uid: Uuid::now_v7(),
        tenant_id,
        connection_uid,
        object_type: "document".to_string(),
        source_id: "doc-1".to_string(),
        parent_source_id: None,
        source_uri: Some("https://example.test/doc-1".to_string()),
        title: Some("Rotation Runbook".to_string()),
        change_token: Some("etag-1".to_string()),
        metadata: json!({
            "safe": "object",
            "access_token": SECRET_TOKEN,
            "nested": { "authorization": SECRET_BEARER }
        }),
        status: ObjectStatus::Active,
        source_updated_at: Some(Utc::now()),
        deleted_at: None,
    }
}

fn fixture_version(object_uid: Uuid) -> DocumentVersion {
    DocumentVersion {
        version_uid: Uuid::now_v7(),
        object_uid,
        parser: "native".to_string(),
        parser_job_id: Some("job-1".to_string()),
        content_hash: "content-hash".to_string(),
        metadata: json!({
            "safe": "version",
            "refresh_token": SECRET_TOKEN
        }),
        created_at: Utc::now(),
    }
}

fn webhook_request(
    tenant_id: TenantId,
    connection_uid: Uuid,
    event_id: &str,
) -> KnowledgeProviderWebhookRequest {
    let payload = json!({
        "tenant_id": tenant_id.to_string(),
        "connection_uid": connection_uid.to_string(),
        "event_id": event_id,
        "event_type": "sync.completed"
    });
    KnowledgeProviderWebhookRequest {
        provider: PROVIDER.to_string(),
        event_id: event_id.to_string(),
        event_type: "sync.completed".to_string(),
        payload,
        headers: vec![("x-test-signature".to_string(), "valid".to_string())],
        body_base64: None,
    }
}

fn signed_connection_webhook_request(
    provider: &str,
    tenant_id: TenantId,
    connection_uid: Uuid,
    event_id: &str,
    event_type: &str,
) -> KnowledgeProviderWebhookRequest {
    signed_provider_webhook_request(
        provider,
        json!({
            "tenant_id": tenant_id.to_string(),
            "connection_uid": connection_uid.to_string(),
            "event_id": event_id,
            "event_type": event_type
        }),
    )
}

fn signed_provider_webhook_request(
    provider: &str,
    payload: Value,
) -> KnowledgeProviderWebhookRequest {
    let event_id = payload
        .get("event_id")
        .and_then(Value::as_str)
        .expect("provider webhook fixture should include event_id")
        .to_string();
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .expect("provider webhook fixture should include event_type")
        .to_string();
    KnowledgeProviderWebhookRequest {
        provider: provider.to_string(),
        event_id,
        event_type,
        payload,
        headers: vec![("x-test-signature".to_string(), "valid".to_string())],
        body_base64: None,
    }
}

fn parser_webhook_payload(
    tenant_id: TenantId,
    connection_uid: Uuid,
    object_uid: Option<Uuid>,
    source_id: Option<&str>,
    event_id: &str,
) -> Value {
    let mut payload = json!({
        "tenant_id": tenant_id.to_string(),
        "connection_uid": connection_uid.to_string(),
        "event_id": event_id,
        "event_type": "parse.completed",
        "status": "completed",
        "metadata": {
            "safe": "parser",
            "access_token": SECRET_TOKEN,
            "raw_document_text": format!("parser document body {RAW_DOCUMENT_TAIL}")
        }
    });
    if let Some(object_uid) = object_uid {
        payload["object_uid"] = json!(object_uid.to_string());
    }
    if let Some(source_id) = source_id {
        payload["source_id"] = json!(source_id);
    }
    payload
}

fn parser_webhook_request(
    provider: &str,
    payload: Value,
    headers: Vec<(String, String)>,
) -> KnowledgeProviderWebhookRequest {
    let event_id = payload
        .get("event_id")
        .and_then(Value::as_str)
        .expect("parser webhook fixture should include event_id")
        .to_string();
    let event_type = payload
        .get("event_type")
        .and_then(Value::as_str)
        .expect("parser webhook fixture should include event_type")
        .to_string();
    KnowledgeProviderWebhookRequest {
        provider: provider.to_string(),
        event_id,
        event_type,
        payload,
        headers,
        body_base64: None,
    }
}

fn webhook_signature_hex(signing_key: &str, payload: &Value) -> String {
    let body = serde_json::to_vec(payload).expect("parser webhook fixture should serialize");
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_key.as_bytes())
        .expect("parser webhook signing key should be valid");
    mac.update(&body);
    hex::encode(mac.finalize().into_bytes())
}

async fn complete_sync_run(
    repository: &PostgresKnowledgeRepository,
    sync_run_uid: Uuid,
) -> moa_knowledge::Result<()> {
    let Some(mut run) = repository.get_sync_run(sync_run_uid).await? else {
        return Err(KnowledgeError::Repository(format!(
            "missing sync run {sync_run_uid}"
        )));
    };
    run.status = SyncRunStatus::Completed;
    run.finished_at = Some(Utc::now());
    repository.update_sync_run(run).await
}

async fn seed_task14_embedder_state(pool: &sqlx::PgPool, tenant_id: TenantId) {
    let mut conn = ScopedConn::begin_tenant(pool, tenant_id)
        .await
        .expect("begin Task14 embedder state seed transaction");
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await
        .expect("set app role for Task14 embedder state seed");
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady'
        "#,
    )
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(TASK14_EMBEDDING_MODEL)
    .bind(TASK14_EMBEDDING_MODEL_VERSION)
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await
    .expect("seed Task14 storage partition embedder state");
    conn.commit()
        .await
        .expect("commit Task14 embedder state seed");
}

async fn insert_retrieval_lineage_row(
    pool: &sqlx::PgPool,
    event: LineageEvent,
    trace_uid: Uuid,
    tenant_id: TenantId,
) -> sqlx::Result<()> {
    sqlx::query(
        r#"
        INSERT INTO analytics.turn_lineage (
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier,
            record_kind,
            payload,
            integrity_hash,
            prev_hash
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, NULL)
        "#,
    )
    .bind(trace_uid)
    .bind(SessionId::new().0)
    .bind("task14-contact")
    .bind(StoragePartitionId::for_tenant(tenant_id).to_string())
    .bind(Utc::now())
    .bind(1_i16)
    .bind(RecordKind::Retrieval.as_i16())
    .bind(serde_json::to_value(event).expect("retrieval lineage should serialize"))
    .bind(vec![0_u8; 32])
    .execute(pool)
    .await
    .map(|_| ())
}

fn assert_sync_status_counters(
    status: &moa_wire::knowledge::KnowledgeSyncStatusResponse,
    expected_records: u64,
    expected_graph_nodes: u64,
    expected_graph_edges: u64,
) {
    assert_eq!(status.status, "completed");
    assert_eq!(status.records_seen, expected_records);
    assert_eq!(status.records_changed, expected_records);
    assert_eq!(status.records_deleted, 0);
    assert_eq!(status.records_ingested, expected_records);
    assert_eq!(status.records_failed, 0);
    assert_eq!(status.objects_parsed, expected_records);
    assert_eq!(status.chunks_embedded, expected_records);
    assert_eq!(status.graph_nodes_upserted, expected_graph_nodes);
    assert_eq!(status.graph_edges_upserted, expected_graph_edges);
}

fn object_ingestion_steps() -> Vec<&'static str> {
    vec![
        "object_change_checked",
        "content_fetched",
        "parse_submitted",
        "parse_completed",
        "normalized",
        "blocks_diffed",
        "semantic_graph_extracted",
        "chunks_diffed",
        "embedded",
        "graph_upserted",
        "vector_indexed",
        "contact_groups_derived",
    ]
}

async fn create_contact_group_graph_node(
    graph: &PostgresGraphStore,
    tenant_id: TenantId,
    group: &ContactGroup,
) -> moa_memory_graph::Result<Uuid> {
    graph
        .create_node(NodeWriteIntent {
            barrier: None,
            uid: group.group_uid,
            data_subject_id: tenant_id.0,
            label: NodeLabel::ContactGroup,
            storage_partition_id: Some(tenant_id.to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: group.display_name.clone(),
            properties: json!({
                "group_key": group.group_key,
                "display_name": group.display_name,
            }),
            pii_class: SensitivityClass::None,
            confidence: Some(0.95),
            valid_from: Utc::now(),
            embedding: None,
            embedding_model: None,
            embedding_model_version: None,
            embedding_text: None,
            actor_id: Uuid::now_v7().to_string(),
            actor_kind: "system".to_string(),
        })
        .await
}

async fn graph_label_counts(pool: &sqlx::PgPool, tenant_id: TenantId) -> HashMap<String, i64> {
    sqlx::query_as::<_, (String, i64)>(
        r#"
        SELECT label::TEXT, count(*)
        FROM moa.node_index
        WHERE storage_partition_id = $1
          AND valid_to IS NULL
        GROUP BY label
        "#,
    )
    .bind(tenant_id.to_string())
    .fetch_all(pool)
    .await
    .expect("read graph label counts")
    .into_iter()
    .collect()
}

async fn chunk_vector_row_count(pool: &sqlx::PgPool, tenant_id: TenantId) -> i64 {
    sqlx::query_scalar::<_, i64>(
        r#"
        SELECT count(*)
        FROM moa.embeddings
        WHERE storage_partition_id = $1
          AND label = 'Chunk'
        "#,
    )
    .bind(tenant_id.to_string())
    .fetch_one(pool)
    .await
    .expect("read chunk vector row count")
}

#[derive(Debug, Clone)]
struct Task14LinkedIntegrationProvider {
    provider: &'static str,
    connector: &'static str,
    records: Arc<Vec<ProviderRecord>>,
    calls: Arc<Mutex<FakeProviderCalls>>,
    list_error: Option<&'static str>,
}

impl Task14LinkedIntegrationProvider {
    fn new(provider: &'static str, connector: &'static str, records: Vec<ProviderRecord>) -> Self {
        Self {
            provider,
            connector,
            records: Arc::new(records),
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            list_error: None,
        }
    }

    fn failing_list(
        provider: &'static str,
        connector: &'static str,
        message: &'static str,
    ) -> Self {
        Self {
            provider,
            connector,
            records: Arc::new(Vec::new()),
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            list_error: Some(message),
        }
    }

    fn trigger_sync_count(&self) -> usize {
        self.calls().trigger_sync
    }

    fn list_changed_records_count(&self) -> usize {
        self.calls().list_changed_records
    }

    fn list_changed_record_requests(&self) -> Vec<FakeListChangedRecordsRequest> {
        self.calls().list_changed_record_requests
    }

    fn calls(&self) -> FakeProviderCalls {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl LinkedIntegrationProvider for Task14LinkedIntegrationProvider {
    async fn create_link_token(
        &self,
        _req: CreateLinkTokenRequest,
    ) -> moa_knowledge::Result<LinkToken> {
        Ok(LinkToken {
            provider: self.provider.to_string(),
            token: format!("{}-task14-link-token", self.provider),
            link_url: Some(format!("https://{}.example.test/link", self.provider)),
            expires_at: None,
        })
    }

    async fn exchange_public_token(
        &self,
        _req: ExchangePublicTokenRequest,
    ) -> moa_knowledge::Result<LinkedAccount> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .exchange_public_token += 1;
        Ok(LinkedAccount {
            provider: self.provider.to_string(),
            connector: self.connector.to_string(),
            provider_account_id: format!("{}-task14-account", self.provider),
            credential_ref: format!("{}-account-token", self.provider),
            credential_material: Some(format!("{}-raw-token-should-enter-vault", self.provider)),
            metadata: json!({
                "provider": self.provider,
                "access_token": format!("{}-secret", self.provider),
            }),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> moa_knowledge::Result<TriggeredSync> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .trigger_sync += 1;
        Ok(TriggeredSync {
            provider: self.provider.to_string(),
            provider_sync_id: Some(format!(
                "{}-sync-{}",
                self.provider, req.connection.connection_uid
            )),
            status: "accepted".to_string(),
            metadata: json!({ "provider_trigger": "accepted" }),
        })
    }

    async fn list_changed_records(
        &self,
        req: ListChangedRecordsRequest,
    ) -> moa_knowledge::Result<RecordPage> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .record_list_changed_records_request(&req);
        if let Some(message) = self.list_error {
            return Err(KnowledgeError::provider(self.provider, message));
        }
        let limit = req.limit.unwrap_or(u32::MAX) as usize;
        Ok(RecordPage {
            records: self.records.iter().take(limit).cloned().collect(),
            next_cursor: None,
        })
    }

    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        self.calls
            .lock()
            .expect("task14 fake provider call log should not be poisoned")
            .verify_webhook += 1;
        Ok(WebhookEvent {
            provider: self.provider.to_string(),
            event_id: format!("{}-task14-webhook", self.provider),
            event_type: "sync.completed".to_string(),
            metadata: json!({ "provider": self.provider }),
        })
    }
}

#[derive(Debug, Default)]
struct Task14Parser;

#[async_trait]
impl DocumentParser for Task14Parser {
    async fn parse(&self, input: ParseInput) -> moa_knowledge::Result<ParsedDocument> {
        match input.object.source_id.as_str() {
            "merge-md-handbook" => Ok(parsed_doc(
                "native",
                None,
                "Benefits Handbook",
                json!({ "job_status": "completed", "format": "markdown" }),
                vec![
                    element(
                        "md-heading-1",
                        DocumentElementKind::Heading,
                        "PTO Policy",
                        vec!["Benefits Handbook", "PTO Policy"],
                        0,
                        None,
                        json!({ "markdown_heading_level": 1 }),
                    ),
                    element(
                        "md-paragraph-1",
                        DocumentElementKind::Paragraph,
                        "PTO policy is standardized for all employees.",
                        vec!["Benefits Handbook", "PTO Policy"],
                        1,
                        None,
                        json!({ "markdown": true }),
                    ),
                    element(
                        "md-list-1",
                        DocumentElementKind::ListItem,
                        "Carryover is capped at five days.",
                        vec!["Benefits Handbook", "PTO Policy"],
                        2,
                        None,
                        json!({ "list_marker": "-" }),
                    ),
                ],
            )),
            "nango-llamaparse-policy" => Ok(parsed_doc(
                "llamaparse",
                Some("lp-task14-job"),
                "Finance Controls",
                json!({
                    "job_status": "completed",
                    "markdown": true,
                    "items": 2,
                    "job_metadata": { "pages": 1 }
                }),
                vec![
                    element(
                        "lp-heading-1",
                        DocumentElementKind::Heading,
                        "Finance Controls",
                        vec!["Finance Controls"],
                        0,
                        None,
                        json!({ "llamaparse_item_type": "heading" }),
                    ),
                    element(
                        "lp-item-1",
                        DocumentElementKind::ListItem,
                        "Finance control is dual approval before payroll export.",
                        vec!["Finance Controls"],
                        1,
                        None,
                        json!({ "llamaparse_item_id": "item-1" }),
                    ),
                ],
            )),
            "nango-unstructured-guide" => Ok(parsed_doc(
                "unstructured",
                Some("unstructured-task14-job"),
                "Support Guide",
                json!({ "job_status": "completed", "element_count": 2 }),
                vec![
                    element(
                        "un-title-1",
                        DocumentElementKind::Heading,
                        "Support Guide",
                        vec!["Support Guide"],
                        0,
                        None,
                        json!({ "unstructured_type": "Title" }),
                    ),
                    element(
                        "un-narrative-1",
                        DocumentElementKind::Paragraph,
                        "Support guide is escalated when billing evidence is missing.",
                        vec!["Support Guide"],
                        1,
                        Some(ElementLayout {
                            x: 12.0,
                            y: 24.0,
                            width: 300.0,
                            height: 90.0,
                            page_width: Some(612.0),
                            page_height: Some(792.0),
                            confidence: Some(0.99),
                        }),
                        json!({ "filename": "support-guide.pdf" }),
                    ),
                ],
            )),
            "nango-reducto-layout" => Ok(parsed_doc(
                "reducto",
                Some("reducto-task14-job"),
                "Warehouse Layout",
                json!({
                    "job_status": "completed",
                    "usage": { "pages": 1 },
                    "studio_link": "https://reducto.example.test/studio/task14",
                    "blocks": [
                        {
                            "type": "paragraph",
                            "bbox": [0.1, 0.2, 0.7, 0.4]
                        }
                    ]
                }),
                vec![element(
                    "reducto-chunk-1",
                    DocumentElementKind::ParserChunk,
                    "Warehouse layout is receiving on the east dock.",
                    vec!["Warehouse Layout"],
                    0,
                    Some(ElementLayout {
                        x: 0.1,
                        y: 0.2,
                        width: 0.6,
                        height: 0.2,
                        page_width: Some(1.0),
                        page_height: Some(1.0),
                        confidence: Some(0.98),
                    }),
                    json!({
                        "blocks": [
                            {
                                "type": "paragraph",
                                "bbox": [0.1, 0.2, 0.7, 0.4]
                            }
                        ]
                    }),
                )],
            )),
            "merge-crm-contact" => Ok(parsed_doc(
                "native",
                None,
                "CRM Contact",
                json!({ "job_status": "completed", "format": "crm_contact" }),
                vec![element(
                    "crm-contact-field-1",
                    DocumentElementKind::Field,
                    "CRM contact is linked to the existing MOA contact.",
                    vec!["CRM Contact"],
                    0,
                    None,
                    json!({ "crm_model": "contact", "moa_contact_linked": true }),
                )],
            )),
            "merge-crm-account" => Ok(parsed_doc(
                "native",
                None,
                "Acme Account",
                json!({ "job_status": "completed", "format": "crm_account" }),
                vec![element(
                    "crm-account-field-1",
                    DocumentElementKind::Field,
                    "Acme account is the enterprise renewal group.",
                    vec!["Acme Account"],
                    0,
                    None,
                    json!({ "crm_model": "account" }),
                )],
            )),
            source_id => Err(KnowledgeError::parser(
                "task14",
                format!("unexpected task14 source id {source_id}"),
            )),
        }
    }
}

fn parsed_doc(
    parser: &str,
    parser_job_id: Option<&str>,
    fallback_title: &str,
    metadata: Value,
    elements: Vec<DocumentElement>,
) -> ParsedDocument {
    let text = elements
        .iter()
        .map(|element| element.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    ParsedDocument {
        parser: parser.to_string(),
        parser_job_id: parser_job_id.map(ToOwned::to_owned),
        text: if text.is_empty() {
            fallback_title.to_string()
        } else {
            text
        },
        elements,
        metadata,
    }
}

fn element(
    element_id: &str,
    kind: DocumentElementKind,
    text: &str,
    heading_path: Vec<&str>,
    ordinal: u32,
    layout: Option<ElementLayout>,
    metadata: Value,
) -> DocumentElement {
    DocumentElement {
        element_id: element_id.to_string(),
        kind,
        text: text.to_string(),
        heading_path: heading_path.into_iter().map(ToOwned::to_owned).collect(),
        ordinal,
        page_number: Some(1),
        layout,
        metadata,
    }
}

#[derive(Debug, Default)]
struct Task14Embedder;

const TASK14_EMBEDDING_MODEL: &str = "embed-v4.0";
const TASK14_EMBEDDING_MODEL_VERSION: i32 = 1;

#[async_trait]
impl EmbeddingProvider for Task14Embedder {
    fn model_id(&self) -> &str {
        TASK14_EMBEDDING_MODEL
    }

    fn dimensions(&self) -> usize {
        VECTOR_DIMENSION
    }

    fn model_version(&self) -> i32 {
        TASK14_EMBEDDING_MODEL_VERSION
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        Ok(inputs.iter().map(|input| task14_vector(input)).collect())
    }
}

fn task14_vector(input: &str) -> Vec<f32> {
    let mut vector = vec![0.0; VECTOR_DIMENSION];
    for (index, byte) in input.bytes().enumerate() {
        vector[index % VECTOR_DIMENSION] += f32::from(byte) / 255.0;
    }
    vector[0] += 1.0;
    vector
}

fn task14_merge_records() -> Vec<ProviderRecord> {
    vec![
        provider_record(
            "merge-md-handbook",
            "article",
            "Benefits Handbook",
            "https://merge.example.test/kb/benefits",
            "# PTO Policy\n\nPTO policy is standardized for all employees.\n\n- Carryover is capped at five days.",
            json!({ "mime_type": "text/markdown", "merge": { "category": "knowledge" } }),
        ),
        provider_record(
            "merge-crm-contact",
            "crm_contact",
            "CRM Contact",
            "https://merge.example.test/crm/contact/member-a",
            "CRM contact is linked to the existing MOA contact.",
            json!({
                "mime_type": "application/json",
                "merge": {
                    "contact": { "id": "contact-task14", "name": "Member A" },
                    "account": { "id": "acct-task14", "name": "Acme" }
                }
            }),
        ),
        provider_record(
            "merge-crm-account",
            "crm_account",
            "Acme Account",
            "https://merge.example.test/crm/account/acct-task14",
            "Acme account is the enterprise renewal group.",
            json!({
                "mime_type": "application/json",
                "merge": {
                    "account": { "id": "acct-task14", "name": "Acme" },
                    "members": [
                        { "email": "member-a@example.invalid" }
                    ]
                }
            }),
        ),
    ]
}

fn task14_nango_records() -> Vec<ProviderRecord> {
    vec![
        provider_record(
            "nango-llamaparse-policy",
            "document",
            "Finance Controls",
            "https://nango.example.test/docs/finance-controls",
            "Finance control is dual approval before payroll export.",
            json!({ "mime_type": "application/pdf", "parser": "llamaparse" }),
        ),
        provider_record(
            "nango-unstructured-guide",
            "document",
            "Support Guide",
            "https://nango.example.test/docs/support-guide",
            "Support guide is escalated when billing evidence is missing.",
            json!({ "mime_type": "application/pdf", "parser": "unstructured" }),
        ),
        provider_record(
            "nango-reducto-layout",
            "document",
            "Warehouse Layout",
            "https://nango.example.test/docs/warehouse-layout",
            "Warehouse layout is receiving on the east dock.",
            json!({ "mime_type": "application/pdf", "parser": "reducto" }),
        ),
    ]
}

fn provider_record(
    source_id: &str,
    object_type: &str,
    title: &str,
    source_uri: &str,
    text: &str,
    metadata: Value,
) -> ProviderRecord {
    ProviderRecord {
        source_id: source_id.to_string(),
        object_type: object_type.to_string(),
        title: Some(title.to_string()),
        source_uri: Some(source_uri.to_string()),
        change_token: Some(format!("{source_id}-v1")),
        deleted: false,
        source_updated_at: Some(Utc::now()),
        metadata,
        payload: json!({ "text": text }),
    }
}

#[derive(Debug, Clone)]
struct FakeLinkedIntegrationProvider {
    calls: Arc<Mutex<FakeProviderCalls>>,
    trigger_status: String,
    integrations: Vec<ProviderIntegration>,
    integrations_error: Option<String>,
}

impl Default for FakeLinkedIntegrationProvider {
    fn default() -> Self {
        Self {
            calls: Arc::new(Mutex::new(FakeProviderCalls::default())),
            trigger_status: "accepted".to_string(),
            integrations: Vec::new(),
            integrations_error: None,
        }
    }
}

impl FakeLinkedIntegrationProvider {
    fn with_trigger_status(status: impl Into<String>) -> Self {
        Self {
            trigger_status: status.into(),
            ..Self::default()
        }
    }

    fn with_integrations(integrations: Vec<ProviderIntegration>) -> Self {
        Self {
            integrations,
            ..Self::default()
        }
    }

    fn with_integrations_error(message: impl Into<String>) -> Self {
        Self {
            integrations_error: Some(message.into()),
            ..Self::default()
        }
    }

    fn trigger_sync_count(&self) -> usize {
        self.calls().trigger_sync
    }

    fn list_changed_records_count(&self) -> usize {
        self.calls().list_changed_records
    }

    fn exchange_count(&self) -> usize {
        self.calls().exchange_public_token
    }

    fn apply_source_selection_count(&self) -> usize {
        self.calls().apply_source_selection
    }

    fn applied_source_selections(&self) -> Vec<Value> {
        self.calls().source_selection_requests
    }

    fn calls(&self) -> FakeProviderCalls {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .clone()
    }
}

#[derive(Debug, Clone, Default)]
struct FakeProviderCalls {
    exchange_public_token: usize,
    apply_source_selection: usize,
    trigger_sync: usize,
    list_changed_records: usize,
    verify_webhook: usize,
    list_changed_record_requests: Vec<FakeListChangedRecordsRequest>,
    source_selection_requests: Vec<Value>,
}

impl FakeProviderCalls {
    fn record_list_changed_records_request(&mut self, req: &ListChangedRecordsRequest) {
        self.list_changed_records += 1;
        self.list_changed_record_requests
            .push(FakeListChangedRecordsRequest {
                connection_uid: req.connection.connection_uid,
                cursor: req.cursor.clone(),
                limit: req.limit,
                modified_after: req.modified_after,
                variant: req.variant.clone(),
            });
    }
}

#[derive(Debug, Clone)]
struct PayloadWebhookVerifier {
    provider: &'static str,
}

impl PayloadWebhookVerifier {
    fn new(provider: &'static str) -> Self {
        Self { provider }
    }
}

#[async_trait]
impl KnowledgeWebhookVerifier for PayloadWebhookVerifier {
    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| KnowledgeError::provider(self.provider, error.to_string()))?;
        let event_id = value
            .get("event_id")
            .and_then(Value::as_str)
            .ok_or_else(|| KnowledgeError::provider(self.provider, "missing `event_id`"))?;
        let event_type = value
            .get("event_type")
            .and_then(Value::as_str)
            .ok_or_else(|| KnowledgeError::provider(self.provider, "missing `event_type`"))?;
        Ok(WebhookEvent {
            provider: self.provider.to_string(),
            event_id: event_id.to_string(),
            event_type: event_type.to_string(),
            metadata: value,
        })
    }
}

#[derive(Debug, Clone)]
struct FixedWebhookVerifier {
    event: WebhookEvent,
}

impl FixedWebhookVerifier {
    fn new(event: WebhookEvent) -> Self {
        Self { event }
    }
}

#[async_trait]
impl KnowledgeWebhookVerifier for FixedWebhookVerifier {
    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        _body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        Ok(self.event.clone())
    }
}

#[async_trait]
impl LinkedIntegrationProvider for FakeLinkedIntegrationProvider {
    async fn list_integrations(&self) -> moa_knowledge::Result<Vec<ProviderIntegration>> {
        if let Some(message) = &self.integrations_error {
            return Err(KnowledgeError::Provider {
                provider: PROVIDER.to_string(),
                message: message.clone(),
            });
        }
        Ok(self.integrations.clone())
    }

    async fn create_link_token(
        &self,
        _req: CreateLinkTokenRequest,
    ) -> moa_knowledge::Result<LinkToken> {
        Ok(LinkToken {
            provider: PROVIDER.to_string(),
            token: "link-token".to_string(),
            link_url: Some("https://provider.example/link".to_string()),
            expires_at: None,
        })
    }

    async fn exchange_public_token(
        &self,
        _req: ExchangePublicTokenRequest,
    ) -> moa_knowledge::Result<LinkedAccount> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .exchange_public_token += 1;
        Ok(LinkedAccount {
            provider: PROVIDER.to_string(),
            connector: CONNECTOR.to_string(),
            provider_account_id: "provider-account-1".to_string(),
            credential_ref: "provider-account-token".to_string(),
            credential_material: Some(SECRET_TOKEN.to_string()),
            metadata: json!({
                "safe": "account",
                "access_token": SECRET_TOKEN
            }),
        })
    }

    async fn trigger_sync(&self, req: TriggerSyncRequest) -> moa_knowledge::Result<TriggeredSync> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .trigger_sync += 1;
        Ok(TriggeredSync {
            provider: PROVIDER.to_string(),
            provider_sync_id: Some(format!("sync-{}", req.connection.connection_uid)),
            status: self.trigger_status.clone(),
            metadata: json!({ "status": self.trigger_status.clone() }),
        })
    }

    async fn apply_source_selection(
        &self,
        req: ApplySourceSelectionRequest,
    ) -> moa_knowledge::Result<()> {
        let mut calls = self
            .calls
            .lock()
            .expect("fake provider call log should not be poisoned");
        calls.apply_source_selection += 1;
        calls
            .source_selection_requests
            .push(req.connection.source_selection);
        Ok(())
    }

    async fn list_changed_records(
        &self,
        req: ListChangedRecordsRequest,
    ) -> moa_knowledge::Result<RecordPage> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .record_list_changed_records_request(&req);
        Ok(RecordPage {
            records: Vec::new(),
            next_cursor: None,
        })
    }

    async fn verify_webhook(
        &self,
        _headers: HeaderMap,
        body: Bytes,
    ) -> moa_knowledge::Result<WebhookEvent> {
        self.calls
            .lock()
            .expect("fake provider call log should not be poisoned")
            .verify_webhook += 1;
        let value: Value = serde_json::from_slice(&body)
            .map_err(|error| KnowledgeError::provider(PROVIDER, error.to_string()))?;
        Ok(WebhookEvent {
            provider: PROVIDER.to_string(),
            event_id: required_string(&value, "event_id")?,
            event_type: required_string(&value, "event_type")?,
            metadata: value,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct FakeKnowledgeCredentialStore {
    accounts: Arc<Mutex<Vec<(TenantId, LinkedAccount)>>>,
}

impl FakeKnowledgeCredentialStore {
    fn stored_account_count(&self) -> usize {
        self.accounts
            .lock()
            .expect("fake credential store should not be poisoned")
            .len()
    }

    fn vault_ref_for(&self, tenant_id: TenantId) -> String {
        self.vault_ref_for_account(tenant_id, "provider-account-1")
    }

    fn vault_ref_for_account(&self, tenant_id: TenantId, provider_account_id: &str) -> String {
        format!("vault://tenant/{tenant_id}/knowledge/{PROVIDER}/{provider_account_id}")
    }
}

#[async_trait]
impl KnowledgeCredentialStore for FakeKnowledgeCredentialStore {
    async fn store_linked_account(
        &self,
        tenant_id: TenantId,
        account: &LinkedAccount,
    ) -> Result<String, moa_orchestrator::services::knowledge::KnowledgeServiceError> {
        self.accounts
            .lock()
            .expect("fake credential store should not be poisoned")
            .push((tenant_id, account.clone()));
        Ok(self.vault_ref_for_account(tenant_id, &account.provider_account_id))
    }

    async fn resolve_linked_account(
        &self,
        _tenant_id: TenantId,
        connection: &KnowledgeConnection,
    ) -> Result<String, moa_orchestrator::services::knowledge::KnowledgeServiceError> {
        let accounts = self
            .accounts
            .lock()
            .expect("fake credential store should not be poisoned");
        accounts
            .iter()
            .find(|(tenant_id, account)| {
                *tenant_id == connection.tenant_id
                    && account.provider_account_id == connection.provider_account_id
            })
            .and_then(|(_, account)| account.credential_material.clone())
            .or_else(|| Some(connection.credential_ref.clone()))
            .ok_or_else(|| {
                moa_orchestrator::services::knowledge::KnowledgeServiceError::Credential(
                    "fake credential not found".to_string(),
                )
            })
    }

    async fn delete_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
    ) -> Result<bool, moa_orchestrator::services::knowledge::KnowledgeServiceError> {
        let mut accounts = self
            .accounts
            .lock()
            .expect("fake credential store should not be poisoned");
        let before = accounts.len();
        accounts.retain(|(account_tenant_id, account)| {
            !(*account_tenant_id == tenant_id
                && account.provider_account_id == connection.provider_account_id)
        });
        Ok(accounts.len() != before)
    }

    async fn list_linked_account_refs(
        &self,
        tenant_id: TenantId,
    ) -> Result<
        std::collections::BTreeSet<String>,
        moa_orchestrator::services::knowledge::KnowledgeServiceError,
    > {
        Ok(self
            .accounts
            .lock()
            .expect("fake credential store should not be poisoned")
            .iter()
            .filter(|(account_tenant_id, _)| *account_tenant_id == tenant_id)
            .map(|(_, account)| self.vault_ref_for_account(tenant_id, &account.provider_account_id))
            .collect())
    }
}

#[derive(Debug, Clone, Default)]
struct InMemoryKnowledgeRepository {
    state: Arc<Mutex<RepositoryState>>,
}

impl InMemoryKnowledgeRepository {
    fn insert_connection(&self, connection: KnowledgeConnection) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            state
                .connections
                .insert(connection.connection_uid, connection);
        })
    }

    fn insert_object_inspection(
        &self,
        object: KnowledgeObject,
        version: DocumentVersion,
        chunks: Vec<KnowledgeChunk>,
    ) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            state.versions.insert(version.object_uid, version.clone());
            state.chunks.insert(version.version_uid, chunks);
            state.objects.insert(object.object_uid, object);
        })
    }

    fn connection(&self, connection_uid: Uuid) -> Option<KnowledgeConnection> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .connections
            .get(&connection_uid)
            .cloned()
    }

    fn op_count(&self, op: &'static str) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .op_counts
            .get(op)
            .copied()
            .unwrap_or(0)
    }

    fn sync_run_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .sync_runs
            .len()
    }

    fn sync_run(&self, sync_run_uid: Uuid) -> Option<KnowledgeSyncRun> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .sync_runs
            .get(&sync_run_uid)
            .cloned()
    }

    fn step_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .steps
            .len()
    }

    fn provider_event_count(&self) -> usize {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .provider_events
            .len()
    }

    fn provider_event(
        &self,
        tenant_id: TenantId,
        provider: &str,
        provider_event_id: &str,
    ) -> Option<KnowledgeProviderEventRecord> {
        self.state
            .lock()
            .expect("repository state should not be poisoned")
            .provider_events
            .get(&(
                tenant_id,
                provider.to_string(),
                provider_event_id.to_string(),
            ))
            .cloned()
    }

    fn record_op(&self, op: &'static str) -> moa_knowledge::Result<()> {
        self.with_state(|state| {
            *state.op_counts.entry(op).or_insert(0) += 1;
        })
    }

    fn with_state<T>(
        &self,
        apply: impl FnOnce(&mut RepositoryState) -> T,
    ) -> moa_knowledge::Result<T> {
        self.state
            .lock()
            .map_err(|error| {
                KnowledgeError::Repository(format!("repository mutex poisoned: {error}"))
            })
            .map(|mut state| apply(&mut state))
    }
}

#[derive(Debug, Default)]
struct RepositoryState {
    connections: HashMap<Uuid, KnowledgeConnection>,
    sync_runs: HashMap<Uuid, KnowledgeSyncRun>,
    steps: Vec<KnowledgeIngestionStep>,
    objects: HashMap<Uuid, KnowledgeObject>,
    versions: HashMap<Uuid, DocumentVersion>,
    ingestion_claims: HashMap<(Uuid, String), InMemoryDocumentIngestionClaim>,
    chunks: HashMap<Uuid, Vec<KnowledgeChunk>>,
    provider_events: HashMap<(TenantId, String, String), KnowledgeProviderEventRecord>,
    op_counts: HashMap<&'static str, usize>,
}

#[derive(Debug, Clone)]
struct InMemoryDocumentIngestionClaim {
    version: DocumentVersion,
    sync_run_uid: Uuid,
    claim_token: Uuid,
    status: InMemoryDocumentIngestionClaimStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InMemoryDocumentIngestionClaimStatus {
    Started,
    Completed,
    Failed,
}

fn sync_run_is_active(status: SyncRunStatus) -> bool {
    matches!(
        status,
        SyncRunStatus::Queued
            | SyncRunStatus::ProviderSyncing
            | SyncRunStatus::ProviderSynced
            | SyncRunStatus::ParsePending
            | SyncRunStatus::Ingesting
    )
}

#[async_trait]
impl KnowledgeDiscoveryStore for InMemoryKnowledgeRepository {
    async fn lookup_connection_by_provider_account(
        &self,
        provider: &str,
        connector: Option<&str>,
        provider_account_id: &str,
    ) -> moa_knowledge::Result<ProviderAccountConnectionLookup> {
        self.record_op("lookup_connection_by_provider_account")?;
        self.with_state(|state| {
            let matches = state
                .connections
                .values()
                .filter(|connection| connection.provider == provider)
                .filter(|connection| {
                    connector.is_none_or(|connector| connector == connection.connector)
                })
                .filter(|connection| connection.provider_account_id == provider_account_id)
                .take(2)
                .cloned()
                .collect::<Vec<_>>();
            match matches.as_slice() {
                [] => ProviderAccountConnectionLookup::NotFound,
                [connection] => ProviderAccountConnectionLookup::Unique(connection.clone()),
                matches => ProviderAccountConnectionLookup::Ambiguous {
                    matches: matches.len(),
                },
            }
        })
    }

    async fn resolve_sync_run_tenant(
        &self,
        sync_run_uid: Uuid,
    ) -> moa_knowledge::Result<Option<TenantId>> {
        self.record_op("resolve_sync_run_tenant")?;
        self.with_state(|state| state.sync_runs.get(&sync_run_uid).map(|run| run.tenant_id))
    }
}

#[async_trait]
impl KnowledgeRepository for InMemoryKnowledgeRepository {
    async fn upsert_connection(
        &self,
        connection: KnowledgeConnection,
    ) -> moa_knowledge::Result<KnowledgeConnection> {
        self.record_op("upsert_connection")?;
        self.with_state(|state| {
            state
                .connections
                .insert(connection.connection_uid, connection.clone());
            connection
        })
    }

    async fn get_connection(
        &self,
        connection_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeConnection>> {
        self.record_op("get_connection")?;
        self.with_state(|state| state.connections.get(&connection_uid).cloned())
    }

    async fn update_connection_source_selection(
        &self,
        connection_uid: Uuid,
        source_selection: Value,
    ) -> moa_knowledge::Result<KnowledgeConnection> {
        self.record_op("update_connection_source_selection")?;
        self.with_state(|state| {
            let connection = state.connections.get_mut(&connection_uid).ok_or_else(|| {
                KnowledgeError::Repository("connection should exist for fixture update".to_string())
            })?;
            connection.source_selection = source_selection;
            connection.last_synced_at = None;
            connection.updated_at = Utc::now();
            Ok(connection.clone())
        })?
    }

    async fn disable_connection(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
    ) -> moa_knowledge::Result<KnowledgeConnection> {
        self.record_op("disable_connection")?;
        self.with_state(|state| {
            let connection = state.connections.get_mut(&connection_uid).ok_or_else(|| {
                KnowledgeError::Repository(
                    "connection should exist for fixture disable".to_string(),
                )
            })?;
            if connection.tenant_id != tenant_id {
                return Err(KnowledgeError::Repository(
                    "connection should be tenant-visible for fixture disable".to_string(),
                ));
            }
            connection.status = ConnectionStatus::Disabled;
            connection.updated_at = Utc::now();
            Ok(connection.clone())
        })?
    }

    async fn list_connections(
        &self,
        tenant_id: TenantId,
        provider: Option<&str>,
    ) -> moa_knowledge::Result<Vec<KnowledgeConnectionProjection>> {
        self.record_op("list_connections")?;
        self.with_state(|state| {
            state
                .connections
                .values()
                .filter(|connection| connection.tenant_id == tenant_id)
                .filter(|connection| {
                    provider.is_none_or(|provider| provider == connection.provider)
                })
                .cloned()
                .map(|connection| {
                    let last_sync_status = state
                        .sync_runs
                        .values()
                        .filter(|run| run.connection_uid == connection.connection_uid)
                        .max_by_key(|run| run.started_at)
                        .map(|run| run.status);
                    KnowledgeConnectionProjection {
                        connection,
                        last_sync_status,
                    }
                })
                .collect()
        })
    }

    async fn create_sync_run(&self, run: KnowledgeSyncRun) -> moa_knowledge::Result<()> {
        self.record_op("create_sync_run")?;
        self.with_state(|state| {
            state.sync_runs.insert(run.sync_run_uid, run);
        })
    }

    async fn claim_sync_run(&self, run: KnowledgeSyncRun) -> moa_knowledge::Result<SyncRunClaim> {
        self.record_op("claim_sync_run")?;
        self.with_state(|state| {
            if let Some(active) = state
                .sync_runs
                .values()
                .filter(|existing| existing.connection_uid == run.connection_uid)
                .filter(|existing| sync_run_is_active(existing.status))
                .max_by_key(|existing| (existing.started_at, existing.sync_run_uid))
                .cloned()
            {
                return SyncRunClaim::AlreadyRunning(active);
            }
            state.sync_runs.insert(run.sync_run_uid, run.clone());
            SyncRunClaim::Claimed(run)
        })
    }

    async fn get_sync_run(
        &self,
        sync_run_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeSyncRun>> {
        self.record_op("get_sync_run")?;
        self.with_state(|state| state.sync_runs.get(&sync_run_uid).cloned())
    }

    async fn latest_sync_run_for_connection(
        &self,
        connection_uid: Uuid,
        statuses: &[SyncRunStatus],
    ) -> moa_knowledge::Result<Option<KnowledgeSyncRun>> {
        self.record_op("latest_sync_run_for_connection")?;
        self.with_state(|state| {
            state
                .sync_runs
                .values()
                .filter(|run| run.connection_uid == connection_uid)
                .filter(|run| statuses.is_empty() || statuses.contains(&run.status))
                .max_by_key(|run| (run.started_at, run.sync_run_uid))
                .cloned()
        })
    }

    async fn update_sync_run(&self, run: KnowledgeSyncRun) -> moa_knowledge::Result<()> {
        self.record_op("update_sync_run")?;
        self.with_state(|state| {
            state.sync_runs.insert(run.sync_run_uid, run);
        })
    }

    async fn add_sync_counters(
        &self,
        sync_run_uid: Uuid,
        counters: KnowledgeSyncCounters,
    ) -> moa_knowledge::Result<()> {
        self.record_op("add_sync_counters")?;
        self.with_state(|state| {
            if let Some(run) = state.sync_runs.get_mut(&sync_run_uid) {
                run.records_seen += counters.records_seen;
                run.records_changed += counters.records_changed;
                run.records_deleted += counters.records_deleted;
                run.records_ingested += counters.records_ingested;
                run.records_failed += counters.records_failed;
                run.objects_parsed += counters.objects_parsed;
                run.chunks_embedded += counters.chunks_embedded;
                run.graph_nodes_upserted += counters.graph_nodes_upserted;
                run.graph_edges_upserted += counters.graph_edges_upserted;
            }
        })
    }

    async fn record_ingestion_step(
        &self,
        step: KnowledgeIngestionStep,
    ) -> moa_knowledge::Result<()> {
        self.record_op("record_ingestion_step")?;
        self.with_state(|state| {
            state.steps.push(step);
        })
    }

    async fn record_ingestion_step_once(
        &self,
        step: KnowledgeIngestionStep,
        counter_delta: KnowledgeSyncCounters,
    ) -> moa_knowledge::Result<bool> {
        self.record_op("record_ingestion_step_once")?;
        self.with_state(|state| {
            let step_object = step.object_uid.unwrap_or(Uuid::nil());
            let exists = state.steps.iter().any(|existing| {
                existing.sync_run_uid == step.sync_run_uid
                    && existing.object_uid.unwrap_or(Uuid::nil()) == step_object
                    && existing.step == step.step
                    && existing.retry_count == step.retry_count
            });
            if exists {
                return false;
            }
            if let Some(run) = state.sync_runs.get_mut(&step.sync_run_uid) {
                run.records_seen += counter_delta.records_seen;
                run.records_changed += counter_delta.records_changed;
                run.records_deleted += counter_delta.records_deleted;
                run.records_ingested += counter_delta.records_ingested;
                run.records_failed += counter_delta.records_failed;
                run.objects_parsed += counter_delta.objects_parsed;
                run.chunks_embedded += counter_delta.chunks_embedded;
                run.graph_nodes_upserted += counter_delta.graph_nodes_upserted;
                run.graph_edges_upserted += counter_delta.graph_edges_upserted;
            }
            state.steps.push(step);
            true
        })
    }

    async fn sync_run_steps(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
    ) -> moa_knowledge::Result<Vec<KnowledgeIngestionStep>> {
        self.record_op("sync_run_steps")?;
        self.with_state(|state| {
            let mut steps = state
                .steps
                .iter()
                .filter(|step| step.sync_run_uid == sync_run_uid)
                .filter(|step| {
                    object_uid.is_none_or(|object_uid| step.object_uid == Some(object_uid))
                })
                .cloned()
                .collect::<Vec<_>>();
            steps.sort_by_key(|step| (step.started_at, step.step.clone(), step.retry_count));
            steps
        })
    }

    async fn upsert_object(&self, object: KnowledgeObject) -> moa_knowledge::Result<()> {
        self.record_op("upsert_object")?;
        self.with_state(|state| {
            state.objects.insert(object.object_uid, object);
        })
    }

    async fn get_object(&self, object_uid: Uuid) -> moa_knowledge::Result<Option<KnowledgeObject>> {
        self.record_op("get_object")?;
        self.with_state(|state| state.objects.get(&object_uid).cloned())
    }

    async fn list_objects(
        &self,
        tenant_id: TenantId,
        connection_uid: Option<Uuid>,
        object_type: Option<&str>,
        limit: u32,
    ) -> moa_knowledge::Result<Vec<KnowledgeObjectProjection>> {
        self.record_op("list_objects")?;
        self.with_state(|state| {
            state
                .objects
                .values()
                .filter(|object| object.tenant_id == tenant_id)
                .filter(|object| {
                    connection_uid
                        .is_none_or(|connection_uid| object.connection_uid == connection_uid)
                })
                .filter(|object| {
                    object_type.is_none_or(|object_type| object.object_type == object_type)
                })
                .take(limit as usize)
                .cloned()
                .map(|object| {
                    let version = state.versions.get(&object.object_uid);
                    let chunks = version
                        .and_then(|version| state.chunks.get(&version.version_uid))
                        .cloned()
                        .unwrap_or_default();
                    KnowledgeObjectProjection {
                        parser: version.map(|version| version.parser.clone()),
                        parser_status: if version.is_some() {
                            "parsed".to_string()
                        } else {
                            "pending".to_string()
                        },
                        chunk_count: chunks.len() as u64,
                        graph_node_count: chunks
                            .iter()
                            .filter(|chunk| chunk.graph_node_uid.is_some())
                            .count() as u64,
                        object,
                    }
                })
                .collect()
        })
    }

    async fn get_object_by_source(
        &self,
        connection_uid: Uuid,
        source_id: &str,
    ) -> moa_knowledge::Result<Option<KnowledgeObject>> {
        self.record_op("get_object_by_source")?;
        self.with_state(|state| {
            state
                .objects
                .values()
                .find(|object| {
                    object.connection_uid == connection_uid && object.source_id == source_id
                })
                .cloned()
        })
    }

    async fn unseen_active_objects_for_connection(
        &self,
        connection_uid: Uuid,
        tenant_id: TenantId,
        seen_source_ids: &[String],
        after: Option<(String, Uuid)>,
        limit: i64,
    ) -> moa_knowledge::Result<Vec<KnowledgeObject>> {
        self.record_op("unseen_active_objects_for_connection")?;
        self.with_state(|state| {
            let seen = seen_source_ids
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>();
            let mut objects = state
                .objects
                .values()
                .filter(|object| object.connection_uid == connection_uid)
                .filter(|object| object.tenant_id == tenant_id)
                .filter(|object| object.status != ObjectStatus::Deleted)
                .filter(|object| !seen.contains(object.source_id.as_str()))
                .filter(|object| match &after {
                    Some((source_id, object_uid)) => {
                        (object.source_id.as_str(), object.object_uid)
                            > (source_id.as_str(), *object_uid)
                    }
                    None => true,
                })
                .cloned()
                .collect::<Vec<_>>();
            objects.sort_by(|left, right| {
                left.source_id
                    .cmp(&right.source_id)
                    .then_with(|| left.object_uid.cmp(&right.object_uid))
            });
            objects.truncate(usize::try_from(limit).unwrap_or(0));
            objects
        })
    }

    async fn latest_document_version(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Option<DocumentVersion>> {
        self.record_op("latest_document_version")?;
        self.with_state(|state| state.versions.get(&object_uid).cloned())
    }

    async fn chunks_for_version(
        &self,
        version_uid: Uuid,
    ) -> moa_knowledge::Result<Vec<KnowledgeChunk>> {
        self.record_op("chunks_for_version")?;
        self.with_state(|state| state.chunks.get(&version_uid).cloned().unwrap_or_default())
    }

    async fn active_chunks_for_object(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Vec<KnowledgeChunk>> {
        self.record_op("active_chunks_for_object")?;
        self.with_state(|state| {
            let Some(version) = state.versions.get(&object_uid) else {
                return Vec::new();
            };
            state
                .chunks
                .get(&version.version_uid)
                .map(|chunks| {
                    chunks
                        .iter()
                        .filter(|chunk| {
                            chunk.metadata.get("active").and_then(Value::as_bool) != Some(false)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        })
    }

    async fn object_ingestion_completed_since(
        &self,
        object_uid: Uuid,
        since: DateTime<Utc>,
    ) -> moa_knowledge::Result<bool> {
        self.record_op("object_ingestion_completed_since")?;
        self.with_state(|state| {
            state.steps.iter().any(|step| {
                step.object_uid == Some(object_uid)
                    && step.step == "contact_groups_derived"
                    && step.status == moa_knowledge::domain::IngestionStepStatus::Completed
                    && step
                        .counters
                        .get("records_ingested")
                        .and_then(Value::as_u64)
                        == Some(1)
                    && step.ended_at.unwrap_or(step.started_at) >= since
            })
        })
    }

    async fn inspect_object(
        &self,
        object_uid: Uuid,
    ) -> moa_knowledge::Result<Option<KnowledgeObjectInspection>> {
        self.record_op("inspect_object")?;
        self.with_state(|state| {
            let object = state.objects.get(&object_uid)?.clone();
            let version = state.versions.get(&object_uid).cloned();
            let chunks = version
                .as_ref()
                .and_then(|version| state.chunks.get(&version.version_uid))
                .cloned()
                .unwrap_or_default();
            let steps = state
                .steps
                .iter()
                .filter(|step| step.object_uid == Some(object_uid))
                .cloned()
                .collect();
            Some(KnowledgeObjectInspection {
                object,
                version,
                chunks,
                steps,
            })
        })
    }

    async fn insert_document_version(&self, version: DocumentVersion) -> moa_knowledge::Result<()> {
        self.record_op("insert_document_version")?;
        self.with_state(|state| {
            state.versions.insert(version.object_uid, version);
        })
    }

    async fn claim_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version: DocumentVersion,
    ) -> moa_knowledge::Result<DocumentVersionIngestionClaim> {
        self.record_op("claim_document_version_ingestion")?;
        self.with_state(|state| {
            let key = (version.object_uid, version.content_hash.clone());
            if let Some(existing) = state.ingestion_claims.get(&key) {
                match existing.status {
                    InMemoryDocumentIngestionClaimStatus::Started => {
                        return DocumentVersionIngestionClaim::AlreadyInProgress(
                            existing.version.clone(),
                        );
                    }
                    InMemoryDocumentIngestionClaimStatus::Completed => {
                        return DocumentVersionIngestionClaim::AlreadyCompleted(
                            existing.version.clone(),
                        );
                    }
                    InMemoryDocumentIngestionClaimStatus::Failed => {}
                }
            }

            let claim_token = Uuid::now_v7();
            state.versions.insert(version.object_uid, version.clone());
            state.ingestion_claims.insert(
                key,
                InMemoryDocumentIngestionClaim {
                    version: version.clone(),
                    sync_run_uid,
                    claim_token,
                    status: InMemoryDocumentIngestionClaimStatus::Started,
                },
            );
            DocumentVersionIngestionClaim::Claimed {
                version,
                claim_token,
            }
        })
    }

    async fn complete_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
        claim_token: Uuid,
    ) -> moa_knowledge::Result<()> {
        self.record_op("complete_document_version_ingestion")?;
        self.with_state(|state| {
            let Some(claim) = state
                .ingestion_claims
                .values_mut()
                .find(|claim| claim.version.version_uid == version_uid)
            else {
                return Err(KnowledgeError::Repository(
                    "document version ingestion claim not found".to_string(),
                ));
            };
            if claim.sync_run_uid != sync_run_uid
                || claim.claim_token != claim_token
                || claim.status != InMemoryDocumentIngestionClaimStatus::Started
            {
                return Err(KnowledgeError::Repository(
                    "document version ingestion claim token mismatch".to_string(),
                ));
            }
            claim.status = InMemoryDocumentIngestionClaimStatus::Completed;
            Ok(())
        })?
    }

    async fn fail_document_version_ingestion(
        &self,
        sync_run_uid: Uuid,
        version_uid: Uuid,
        claim_token: Uuid,
    ) -> moa_knowledge::Result<()> {
        self.record_op("fail_document_version_ingestion")?;
        self.with_state(|state| {
            let Some(claim) = state
                .ingestion_claims
                .values_mut()
                .find(|claim| claim.version.version_uid == version_uid)
            else {
                return Err(KnowledgeError::Repository(
                    "document version ingestion claim not found".to_string(),
                ));
            };
            if claim.sync_run_uid != sync_run_uid
                || claim.claim_token != claim_token
                || claim.status != InMemoryDocumentIngestionClaimStatus::Started
            {
                return Err(KnowledgeError::Repository(
                    "document version ingestion claim token mismatch".to_string(),
                ));
            }
            claim.status = InMemoryDocumentIngestionClaimStatus::Failed;
            Ok(())
        })?
    }

    async fn replace_blocks(
        &self,
        _version_uid: Uuid,
        _blocks: Vec<KnowledgeBlock>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_blocks")
    }

    async fn replace_chunks(
        &self,
        version_uid: Uuid,
        chunks: Vec<KnowledgeChunk>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_chunks")?;
        self.with_state(|state| {
            state.chunks.insert(version_uid, chunks);
        })
    }

    async fn tombstone_chunks(&self, _chunk_uids: &[Uuid]) -> moa_knowledge::Result<()> {
        self.record_op("tombstone_chunks")
    }

    async fn mark_object_deleted(
        &self,
        object_uid: Uuid,
        deleted_at: chrono::DateTime<chrono::Utc>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("mark_object_deleted")?;
        self.with_state(|state| {
            if let Some(object) = state.objects.get_mut(&object_uid) {
                object.status = ObjectStatus::Deleted;
                object.deleted_at = Some(deleted_at);
            }
        })
    }

    async fn upsert_contact_group(&self, _group: ContactGroup) -> moa_knowledge::Result<()> {
        self.record_op("upsert_contact_group")
    }

    async fn replace_contact_group_memberships(
        &self,
        _group_uid: Uuid,
        _memberships: Vec<ContactGroupMembership>,
    ) -> moa_knowledge::Result<()> {
        self.record_op("replace_contact_group_memberships")
    }

    async fn contact_group_targets(
        &self,
        _tenant_id: TenantId,
        _group_key: &str,
    ) -> moa_knowledge::Result<Option<ContactGroupTarget>> {
        self.record_op("contact_group_targets")?;
        Ok(None)
    }

    async fn record_provider_event(
        &self,
        event: KnowledgeProviderEventRecord,
    ) -> moa_knowledge::Result<KnowledgeProviderEventRecord> {
        self.record_op("record_provider_event")?;
        self.with_state(|state| {
            let key = (
                event.tenant_id,
                event.provider.clone(),
                event.provider_event_id.clone(),
            );
            if let Some(existing) = state.provider_events.get(&key) {
                let mut duplicate = existing.clone();
                duplicate.duplicate = true;
                return duplicate;
            }
            state.provider_events.insert(key, event.clone());
            event
        })
    }
}

fn required_string(value: &Value, field: &str) -> moa_knowledge::Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| KnowledgeError::provider(PROVIDER, format!("missing `{field}`")))
}
