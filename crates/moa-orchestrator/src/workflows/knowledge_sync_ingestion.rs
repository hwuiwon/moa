//! Restate workflow that owns one tenant knowledge sync ingestion pass.

use std::{collections::HashSet, sync::Arc};

use async_trait::async_trait;
use moa_config::MoaConfig;
use moa_core::traits::CredentialVault;
use moa_core::types::credentials::{CredentialServiceActor, RedactedSecret};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::RlsContext;
use moa_crypto::KeyManagementProvider;
use moa_knowledge::{
    domain::{
        KnowledgeConnection, KnowledgeSyncRun, ListChangedRecordsRequest, RecordPage, SyncRunStatus,
    },
    ingestion::PageIngestionReport,
    observability::classify_failure,
    providers::{ConnectionCredentialResolver, LinkedProviderContentFetcher, RecordContentFetcher},
    repository::{
        KnowledgeDiscoveryStore as _, KnowledgeRepository as _, PostgresKnowledgeDiscoveryStore,
        PostgresKnowledgeRepository,
    },
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;
use sqlx::PgPool;
use uuid::Uuid;

use crate::services::knowledge::{
    ConfigKnowledgeProviders, KnowledgeCaller, KnowledgeCredentialStore,
    KnowledgeIngestionRunner as _, KnowledgeProviderResolver as _, KnowledgeServiceError,
    ProductionKnowledgeIngestionRunner, VaultKnowledgeCredentialStore,
};
use crate::workflows::errors::handler_error_message;

/// Workflow input for one knowledge sync ingestion run.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeSyncIngestionRequest {
    /// Sync run whose provider records should be applied.
    pub sync_run_uid: Uuid,
}

/// Serializable report for one knowledge sync ingestion workflow execution.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeSyncIngestionReport {
    /// Sync run processed by the workflow.
    pub sync_run_uid: Uuid,
    /// Tenant derived from the stored sync run row.
    pub tenant_id: TenantId,
    /// Knowledge connection derived from the stored sync run row.
    pub connection_uid: Uuid,
    /// Provider records listed and applied within the sync-run cap.
    pub records_listed: u64,
    /// Provider records applied by the page-application step.
    pub records_applied: u64,
    /// Previously active local objects pruned because they were absent from a full selected-source sync.
    pub records_pruned: u64,
    /// Human-readable workflow status.
    pub status: String,
}

/// Stored run, validated connection, and sync limits prepared for ingestion.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeSyncPreparedRun {
    /// Sync run being ingested.
    pub run: KnowledgeSyncRun,
    /// Stored linked connection for the run.
    pub connection: KnowledgeConnection,
    /// Low-cardinality provider label.
    pub provider: String,
    /// Parser label selected for this run.
    pub parser_label: String,
    /// Provider page size to request before applying the run cap.
    pub page_size: u32,
    /// Maximum provider records this run may process.
    pub max_records: u32,
}

/// Provider page and low-cardinality ingestion labels returned by the listing step.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeSyncProviderPage {
    /// Provider label used for ingestion observability.
    pub provider: String,
    /// Provider records represented by this page.
    pub page: RecordPage,
    /// Number of provider records represented by this page.
    pub records_listed: u64,
}

/// Page-application report returned after the ingestion pipeline processes one page.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnowledgeSyncPageApplication {
    /// Number of provider records processed by the ingestion pipeline.
    pub records_listed: u64,
    /// Number of changed records ingested into local knowledge state.
    pub records_ingested: u64,
    /// Number of unchanged records skipped by ingestion idempotency checks.
    pub records_skipped: u64,
    /// Number of local deletions applied from provider deletes or source-selection pruning.
    pub records_deleted: u64,
    /// Number of embeddings created by the pipeline.
    pub embeddings_created: u64,
    /// Number of records or prune deletions applied or intentionally skipped by local ingestion.
    pub records_applied: u64,
}

/// Restate workflow surface for one-shot knowledge sync ingestion runs.
#[restate_sdk::workflow]
pub trait KnowledgeSyncIngestion {
    /// Runs one durable tenant knowledge sync ingestion pass.
    async fn run(
        request: Json<KnowledgeSyncIngestionRequest>,
    ) -> Result<Json<KnowledgeSyncIngestionReport>, HandlerError>;
}

/// Concrete knowledge sync ingestion workflow implementation.
#[derive(Clone)]
pub struct KnowledgeSyncIngestionImpl {
    pool: PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    credentials: Arc<dyn KnowledgeCredentialStore>,
    config: Arc<MoaConfig>,
}

impl KnowledgeSyncIngestionImpl {
    /// Creates a knowledge-sync workflow with its storage and provider configuration.
    ///
    /// `credential_vault` is the process's single durable credential owner, so a
    /// workflow reconstructed on another replica resolves the same stored
    /// versions instead of building an empty process-local vault.
    #[must_use]
    pub fn new(
        pool: PgPool,
        kms: Arc<dyn KeyManagementProvider>,
        credential_vault: Arc<dyn CredentialVault>,
        config: Arc<MoaConfig>,
    ) -> Self {
        Self {
            pool,
            kms,
            credentials: Arc::new(VaultKnowledgeCredentialStore::new(credential_vault)),
            config,
        }
    }
}

impl KnowledgeSyncIngestion for KnowledgeSyncIngestionImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Internal-only workflow; tenant and connection are derived from the stored sync run row.
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<KnowledgeSyncIngestionRequest>,
    ) -> Result<Json<KnowledgeSyncIngestionReport>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("KnowledgeSyncIngestion", "run");
        let request = request.into_inner();
        let mut steps = RestateKnowledgeSyncIngestionSteps {
            ctx: &ctx,
            pool: self.pool.clone(),
            kms: self.kms.clone(),
            credentials: self.credentials.clone(),
            config: self.config.clone(),
        };
        let report = run_knowledge_sync_ingestion_workflow(&mut steps, request).await?;

        Ok(Json::from(report))
    }
}

/// Durable operations used by the knowledge sync ingestion workflow body.
#[async_trait]
pub trait KnowledgeSyncIngestionSteps {
    /// Loads and validates the sync run that owns this workflow instance.
    async fn prepare_ingestion_run(
        &mut self,
        request: &KnowledgeSyncIngestionRequest,
    ) -> Result<KnowledgeSyncPreparedRun, HandlerError>;

    /// Lists one provider page for this sync run.
    async fn list_changed_records_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        cursor: Option<String>,
        limit: u32,
        page_index: u32,
    ) -> Result<KnowledgeSyncProviderPage, HandlerError>;

    /// Applies one provider page to local knowledge state.
    async fn apply_record_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        page: KnowledgeSyncProviderPage,
        page_index: u32,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError>;

    /// Tombstones active local objects absent from an exhaustive selected-source sync.
    async fn prune_unseen_objects(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        seen_source_ids: HashSet<String>,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError>;

    /// Marks the run completed and advances the connection sync watermark.
    async fn complete_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
    ) -> Result<(), HandlerError>;

    /// Marks the run failed when a page-level workflow step cannot continue.
    async fn fail_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        stage: &'static str,
        error_message: String,
    ) -> Result<(), HandlerError>;
}

/// Runs the knowledge sync ingestion workflow body against durable steps.
pub async fn run_knowledge_sync_ingestion_workflow(
    steps: &mut impl KnowledgeSyncIngestionSteps,
    request: KnowledgeSyncIngestionRequest,
) -> Result<KnowledgeSyncIngestionReport, HandlerError> {
    let prepared = steps.prepare_ingestion_run(&request).await?;
    let mut cursor = None;
    let mut page_index = 0_u32;
    let mut records_processed = 0_u64;
    let mut records_applied = 0_u64;
    let mut records_pruned = 0_u64;
    let mut seen_source_ids = HashSet::new();
    let mut listing_exhaustive = false;

    while records_processed < u64::from(prepared.max_records) {
        let remaining = u64::from(prepared.max_records).saturating_sub(records_processed);
        let limit = u32::try_from(remaining.min(u64::from(prepared.page_size))).map_err(|_| {
            HandlerError::from(TerminalError::new("knowledge sync page limit overflow"))
        })?;
        let listed_page = match steps
            .list_changed_records_page(&prepared, cursor.clone(), limit, page_index)
            .await
        {
            Ok(page) => page,
            Err(error) => {
                fail_prepared_run(steps, &prepared, "provider_records_listed", &error).await;
                return Err(error);
            }
        };
        let provider_next_cursor = listed_page.page.next_cursor.clone();
        let provider_returned_over_limit = listed_page.records_listed > remaining;
        let page = cap_provider_page(listed_page, remaining);
        let next_cursor = page.page.next_cursor.clone();
        let records_in_page = page.records_listed;
        seen_source_ids.extend(
            page.page
                .records
                .iter()
                .map(|record| record.source_id.clone()),
        );
        let empty_page = records_in_page == 0;
        let reached_cap =
            records_processed.saturating_add(records_in_page) >= u64::from(prepared.max_records);
        let application = match steps.apply_record_page(&prepared, page, page_index).await {
            Ok(application) => application,
            Err(error) => {
                fail_prepared_run(steps, &prepared, "knowledge_sync_page_application", &error)
                    .await;
                return Err(error);
            }
        };
        records_processed = records_processed.saturating_add(records_in_page);
        records_applied = records_applied.saturating_add(application.records_applied);

        if next_cursor.is_none() || reached_cap || empty_page {
            listing_exhaustive = provider_next_cursor.is_none() && !provider_returned_over_limit;
            break;
        }
        cursor = next_cursor;
        page_index = page_index.saturating_add(1);
    }

    if prepared.connection.last_synced_at.is_none() && listing_exhaustive {
        let prune = match steps.prune_unseen_objects(&prepared, seen_source_ids).await {
            Ok(prune) => prune,
            Err(error) => {
                fail_prepared_run(steps, &prepared, "source_selection_pruned", &error).await;
                return Err(error);
            }
        };
        records_pruned = prune.records_deleted;
        records_applied = records_applied.saturating_add(prune.records_applied);
    }

    if let Err(error) = steps.complete_ingestion_run(&prepared).await {
        fail_prepared_run(steps, &prepared, "knowledge_sync_complete_run", &error).await;
        return Err(error);
    }

    Ok(KnowledgeSyncIngestionReport {
        sync_run_uid: request.sync_run_uid,
        tenant_id: prepared.run.tenant_id,
        connection_uid: prepared.run.connection_uid,
        records_listed: records_processed,
        records_applied,
        records_pruned,
        status: "completed".to_string(),
    })
}

struct RestateKnowledgeSyncIngestionSteps<'ctx, 'workflow> {
    ctx: &'ctx WorkflowContext<'workflow>,
    pool: PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    credentials: Arc<dyn KnowledgeCredentialStore>,
    config: Arc<MoaConfig>,
}

#[async_trait]
impl KnowledgeSyncIngestionSteps for RestateKnowledgeSyncIngestionSteps<'_, '_> {
    async fn prepare_ingestion_run(
        &mut self,
        request: &KnowledgeSyncIngestionRequest,
    ) -> Result<KnowledgeSyncPreparedRun, HandlerError> {
        let pool = self.pool.clone();
        let config = self.config.clone();
        let sync_run_uid = request.sync_run_uid;
        self.ctx
            .run(|| async move {
                let discovery = PostgresKnowledgeDiscoveryStore::new(pool.clone());
                let tenant_id = discovery
                    .resolve_sync_run_tenant(sync_run_uid)
                    .await
                    .map_err(knowledge_ingestion_error)?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(404, "knowledge sync run not found")
                    })?;
                let repository =
                    PostgresKnowledgeRepository::scoped(pool, RlsContext::tenant(tenant_id));
                let mut run = repository
                    .get_sync_run(sync_run_uid)
                    .await
                    .map_err(knowledge_ingestion_error)?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(404, "knowledge sync run not found")
                    })?;
                if !matches!(
                    run.status,
                    SyncRunStatus::ProviderSynced | SyncRunStatus::Ingesting
                ) {
                    return Err(TerminalError::new_with_code(
                        400,
                        "knowledge sync run is not ready for ingestion",
                    )
                    .into());
                }
                let connection = repository
                    .get_connection(run.connection_uid)
                    .await
                    .map_err(knowledge_ingestion_error)?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(404, "knowledge connection not found")
                    })?;
                if connection.tenant_id != run.tenant_id
                    || connection.connection_uid != run.connection_uid
                {
                    return Err(TerminalError::new_with_code(
                        404,
                        "knowledge connection tenant mismatch",
                    )
                    .into());
                }
                let provider = connection.provider.clone();
                let parser_label = run
                    .parser
                    .clone()
                    .unwrap_or(config.knowledge.parser.external_default.clone());
                let page_size = config.knowledge.sync.default_page_size.max(1);
                let max_records = run
                    .max_records
                    .unwrap_or(config.knowledge.sync.max_records_per_run);
                run.status = SyncRunStatus::Ingesting;
                run.parser = Some(parser_label.clone());
                repository
                    .update_sync_run(run.clone())
                    .await
                    .map_err(knowledge_ingestion_error)?;
                Ok(Json::from(KnowledgeSyncPreparedRun {
                    run,
                    connection,
                    provider,
                    parser_label,
                    page_size,
                    max_records,
                }))
            })
            .name("knowledge_sync_prepare_run")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn list_changed_records_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        cursor: Option<String>,
        limit: u32,
        page_index: u32,
    ) -> Result<KnowledgeSyncProviderPage, HandlerError> {
        let config = self.config.clone();
        let tenant_id = prepared.run.tenant_id;
        let provider_label = prepared.provider.clone();
        let connection = prepared.connection.clone();
        let credentials = self.credentials.clone();
        let caller = KnowledgeCaller::service(
            CredentialServiceActor::KnowledgeSyncListing,
            sync_listing_operation_id(prepared.run.sync_run_uid, page_index),
        );
        self.ctx
            .run(|| async move {
                let modified_after = connection.last_synced_at;
                // Resolved through the shared durable owner immediately before
                // the outbound call; the plaintext never enters the connection
                // row, the journal, or the returned page.
                let credential = credentials
                    .resolve_linked_account(tenant_id, &connection, &caller)
                    .await
                    .map_err(knowledge_service_handler_error)?;
                let provider = ConfigKnowledgeProviders::new(config.knowledge.clone())
                    .provider(&provider_label)
                    .map_err(knowledge_service_handler_error)?;
                let page = provider
                    .list_changed_records(ListChangedRecordsRequest {
                        connection,
                        credential,
                        cursor,
                        modified_after,
                        limit: Some(limit),
                        variant: None,
                    })
                    .await
                    .map_err(knowledge_ingestion_error)?;
                let records_listed = page.records.len() as u64;
                Ok(Json::from(KnowledgeSyncProviderPage {
                    provider: provider_label,
                    page,
                    records_listed,
                }))
            })
            .name(format!("knowledge_sync_provider_page_listing_{page_index}"))
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn apply_record_page(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        page: KnowledgeSyncProviderPage,
        page_index: u32,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let pool = self.pool.clone();
        let kms = self.kms.clone();
        let config = self.config.clone();
        let run = prepared.run.clone();
        let provider_label = prepared.provider.clone();
        let connection = prepared.connection.clone();
        let credentials = self.credentials.clone();
        let content_caller = KnowledgeCaller::service(
            CredentialServiceActor::KnowledgeContentFetch,
            content_fetch_operation_id(prepared.run.sync_run_uid, page_index),
        );
        self.ctx
            .run(|| async move {
                let content_fetcher = build_record_content_fetcher(
                    &config,
                    &provider_label,
                    connection,
                    Arc::new(StoreConnectionCredentialResolver::new(
                        credentials,
                        content_caller,
                    )),
                );
                let runner =
                    ProductionKnowledgeIngestionRunner::new(pool, kms, config.as_ref().clone())
                        .with_content_fetcher(content_fetcher);
                let report = runner
                    .ingest_record_page(&run, &page.provider, page.page)
                    .await
                    .map_err(knowledge_service_handler_error)?;
                Ok::<_, HandlerError>(Json::from(KnowledgeSyncPageApplication::from(report)))
            })
            .name(format!(
                "knowledge_sync_provider_page_application_{page_index}"
            ))
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn prune_unseen_objects(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        seen_source_ids: HashSet<String>,
    ) -> Result<KnowledgeSyncPageApplication, HandlerError> {
        let pool = self.pool.clone();
        let kms = self.kms.clone();
        let config = self.config.clone();
        let run = prepared.run.clone();
        let provider = prepared.provider.clone();
        self.ctx
            .run(|| async move {
                let runner =
                    ProductionKnowledgeIngestionRunner::new(pool, kms, config.as_ref().clone());
                let report = runner
                    .prune_unseen_objects(&run, &provider, &seen_source_ids)
                    .await
                    .map_err(knowledge_service_handler_error)?;
                Ok::<_, HandlerError>(Json::from(KnowledgeSyncPageApplication::from(report)))
            })
            .name("knowledge_sync_source_selection_prune")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn complete_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
    ) -> Result<(), HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = prepared.run.tenant_id;
        let sync_run_uid = prepared.run.sync_run_uid;
        let connection_uid = prepared.run.connection_uid;
        self.ctx
            .run(|| async move {
                let repository =
                    PostgresKnowledgeRepository::scoped(pool, RlsContext::tenant(tenant_id));
                let mut run = repository
                    .get_sync_run(sync_run_uid)
                    .await
                    .map_err(knowledge_ingestion_error)?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(404, "knowledge sync run not found")
                    })?;
                if run.tenant_id != tenant_id || run.connection_uid != connection_uid {
                    return Err(TerminalError::new_with_code(
                        404,
                        "knowledge sync run tenant mismatch",
                    )
                    .into());
                }
                let completed_at = chrono::Utc::now();
                run.status = SyncRunStatus::Completed;
                run.error_code = None;
                run.finished_at = Some(completed_at);
                repository
                    .update_sync_run(run)
                    .await
                    .map_err(knowledge_ingestion_error)?;

                let mut connection = repository
                    .get_connection(connection_uid)
                    .await
                    .map_err(knowledge_ingestion_error)?
                    .ok_or_else(|| {
                        TerminalError::new_with_code(404, "knowledge connection not found")
                    })?;
                if connection.tenant_id != tenant_id {
                    return Err(TerminalError::new_with_code(
                        404,
                        "knowledge connection tenant mismatch",
                    )
                    .into());
                }
                connection.last_synced_at = Some(completed_at);
                repository
                    .upsert_connection(connection)
                    .await
                    .map_err(knowledge_ingestion_error)?;
                Ok(Json::from(()))
            })
            .name("knowledge_sync_complete_run")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }

    async fn fail_ingestion_run(
        &mut self,
        prepared: &KnowledgeSyncPreparedRun,
        stage: &'static str,
        error_message: String,
    ) -> Result<(), HandlerError> {
        let pool = self.pool.clone();
        let tenant_id = prepared.run.tenant_id;
        let sync_run_uid = prepared.run.sync_run_uid;
        let provider = prepared.provider.clone();
        self.ctx
            .run(|| async move {
                let repository =
                    PostgresKnowledgeRepository::scoped(pool, RlsContext::tenant(tenant_id));
                let Some(mut run) = repository
                    .get_sync_run(sync_run_uid)
                    .await
                    .map_err(knowledge_ingestion_error)?
                else {
                    return Ok(Json::from(()));
                };
                if matches!(
                    run.status,
                    SyncRunStatus::FailedRetryable | SyncRunStatus::FailedTerminal
                ) {
                    return Ok(Json::from(()));
                }
                let classification = classify_workflow_failure(stage, &provider, error_message);
                run.status = if classification.retryable {
                    SyncRunStatus::FailedRetryable
                } else {
                    SyncRunStatus::FailedTerminal
                };
                run.records_failed = run.records_failed.saturating_add(1);
                run.error_code = Some(classification.error_code.to_string());
                run.finished_at = Some(chrono::Utc::now());
                repository
                    .update_sync_run(run)
                    .await
                    .map_err(knowledge_ingestion_error)?;
                Ok(Json::from(()))
            })
            .name("knowledge_sync_fail_run")
            .await
            .map(Json::into_inner)
            .map_err(HandlerError::from)
    }
}

impl From<PageIngestionReport> for KnowledgeSyncPageApplication {
    fn from(report: PageIngestionReport) -> Self {
        let records_applied = report
            .records_ingested
            .saturating_add(report.records_deleted)
            .saturating_add(report.records_skipped);
        Self {
            records_listed: report.records_listed,
            records_ingested: report.records_ingested,
            records_skipped: report.records_skipped,
            records_deleted: report.records_deleted,
            embeddings_created: report.embeddings_created,
            records_applied,
        }
    }
}

/// Builds a per-page record content fetcher from the configured provider and the
/// stored connection.
///
/// A build failure degrades gracefully to metadata-only ingestion (records that
/// need content fall back to their title) rather than failing the page: the
/// provider was already constructed successfully during the listing step, so a
/// failure here is not expected but must not abort applying the page.
fn build_record_content_fetcher(
    config: &std::sync::Arc<moa_config::MoaConfig>,
    provider_label: &str,
    connection: KnowledgeConnection,
    credentials: Arc<dyn ConnectionCredentialResolver>,
) -> Option<Arc<dyn RecordContentFetcher>> {
    match ConfigKnowledgeProviders::new(config.knowledge.clone()).provider(provider_label) {
        Ok(provider) => Some(Arc::new(LinkedProviderContentFetcher::new(
            provider,
            connection,
            credentials,
        ))),
        Err(error) => {
            tracing::warn!(
                provider = provider_label,
                error = %error,
                "could not build knowledge content fetcher; records needing content fall back to title-only"
            );
            None
        }
    }
}

/// Resolves connection credentials for content fetches through the shared owner.
///
/// Exists so `moa-knowledge` keeps no dependency on credential storage: the
/// orchestrator owns the vault and hands the ingestion pipeline this narrow,
/// already-authorized resolver bound to one service actor and one operation.
struct StoreConnectionCredentialResolver {
    credentials: Arc<dyn KnowledgeCredentialStore>,
    caller: KnowledgeCaller,
}

impl StoreConnectionCredentialResolver {
    /// Binds a credential store and caller context to one ingestion page.
    fn new(credentials: Arc<dyn KnowledgeCredentialStore>, caller: KnowledgeCaller) -> Self {
        Self {
            credentials,
            caller,
        }
    }
}

#[async_trait]
impl ConnectionCredentialResolver for StoreConnectionCredentialResolver {
    async fn resolve(
        &self,
        connection: &KnowledgeConnection,
    ) -> moa_knowledge::Result<RedactedSecret> {
        self.credentials
            .resolve_linked_account(connection.tenant_id, connection, &self.caller)
            .await
            .map_err(|error| {
                // Only the typed, secret-free service error text crosses this
                // boundary; provider bodies and material never do.
                moa_knowledge::Error::Repository(error.to_string())
            })
    }
}

/// Returns the replay-stable operation id for one provider listing page.
fn sync_listing_operation_id(sync_run_uid: Uuid, page_index: u32) -> String {
    format!("knowledge-sync:{sync_run_uid}:listing:{page_index}")
}

/// Returns the replay-stable operation id for one page's content fetches.
///
/// Content is fetched per record but authorized once per page: the selector and
/// service actor are identical for every record in the page, so one audited
/// resolve operation describes the page rather than one row per record.
fn content_fetch_operation_id(sync_run_uid: Uuid, page_index: u32) -> String {
    format!("knowledge-sync:{sync_run_uid}:content:{page_index}")
}

fn cap_provider_page(
    mut page: KnowledgeSyncProviderPage,
    remaining: u64,
) -> KnowledgeSyncProviderPage {
    let limit = usize::try_from(remaining).unwrap_or(usize::MAX);
    if page.page.records.len() > limit {
        page.page.records.truncate(limit);
        page.page.next_cursor = None;
    }
    page.records_listed = page.page.records.len() as u64;
    page
}

async fn fail_prepared_run(
    steps: &mut impl KnowledgeSyncIngestionSteps,
    prepared: &KnowledgeSyncPreparedRun,
    stage: &'static str,
    error: &HandlerError,
) {
    let message = handler_error_message(error);
    if let Err(fail_error) = steps.fail_ingestion_run(prepared, stage, message).await {
        tracing::warn!(
            sync_run_id = %prepared.run.sync_run_uid,
            stage,
            error = %handler_error_message(&fail_error),
            "failed to mark knowledge sync ingestion run failed"
        );
    }
}

fn classify_workflow_failure(
    stage: &'static str,
    provider: &str,
    error_message: String,
) -> moa_knowledge::observability::FailureClassification {
    let error = if stage == "provider_records_listed" {
        moa_knowledge::Error::provider(provider.to_string(), error_message)
    } else {
        moa_knowledge::Error::Repository(error_message)
    };
    classify_failure(stage, &error)
}

fn knowledge_ingestion_error(error: moa_knowledge::Error) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}

fn knowledge_service_handler_error(error: KnowledgeServiceError) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
