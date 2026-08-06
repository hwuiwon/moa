//! Restate service for tenant knowledge-base link, sync, webhook, and inspection APIs.

pub mod ingest;
mod inspect;
mod link;
mod sync;
mod webhook;
pub mod webhook_verifier;

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use moa_authz_schema::Relation;
use moa_config::MoaConfig;
use moa_connectors::{
    domain::{
        ConnectionGeneration, ConnectionHealth, ConnectorConnection, ManagedParentClaim,
        ManagedParentDefinition, ManagedParentDeleteOutcome,
    },
    service::{
        ConnectorService, CredentialGenerationFenceRequest, ManagedParentActivationRequest,
        ManagedParentClaimRequest, ManagedParentDeleteRequest,
    },
};
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialKind, CredentialOperation,
    CredentialPrincipal, CredentialRef, CredentialServiceActor, CredentialSlotName,
    CredentialStagingToken, RedactedSecret,
};
use moa_core::types::memory::RlsContext;
use moa_core::{
    error::MoaError,
    traits::{CredentialVault, Identity, RuntimeCacheStore},
    types::identifiers::TenantId,
};
use moa_knowledge::{
    domain::{
        KnowledgeConnection, KnowledgeCredentialOwnership, LinkedAccount, LinkedProviderKind,
    },
    providers::{LinkedIntegrationProvider, merge::MergeProvider, nango::NangoProvider},
    repository::{
        KnowledgeDiscoveryStore, PostgresKnowledgeDiscoveryStore, PostgresKnowledgeRepository,
        connection::KnowledgeConnectionRepository, document::KnowledgeIngestionRepository,
        event::KnowledgeEventRepository, sync::KnowledgeSyncRepository,
    },
};
use moa_observability::restate_observability::annotate_restate_handler_span;
use moa_wire::knowledge::{
    KnowledgeConnectionListRequest, KnowledgeConnectionListResponse,
    KnowledgeCreateLinkTokenRequest, KnowledgeCreateLinkTokenResponse,
    KnowledgeDisconnectConnectionRequest, KnowledgeDisconnectConnectionResponse,
    KnowledgeExchangeTokenRequest, KnowledgeExchangeTokenResponse, KnowledgeIntegrationListRequest,
    KnowledgeIntegrationListResponse, KnowledgeObjectInspectRequest,
    KnowledgeObjectInspectResponse, KnowledgeObjectListRequest, KnowledgeObjectListResponse,
    KnowledgeProviderWebhookRequest, KnowledgeProviderWebhookResponse, KnowledgeQueryTraceRequest,
    KnowledgeQueryTraceResponse, KnowledgeSyncEventsRequest, KnowledgeSyncEventsResponse,
    KnowledgeSyncRequest, KnowledgeSyncResponse, KnowledgeSyncStatusRequest,
    KnowledgeSyncStatusResponse, KnowledgeUpdateConnectionSourceSelectionRequest,
    KnowledgeUpdateConnectionSourceSelectionResponse,
};
use restate_sdk::prelude::*;
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::workflows::errors::moa_error_to_status_handler_error;
use crate::workflows::knowledge_sync_ingestion::{
    KnowledgeSyncIngestionClient, KnowledgeSyncIngestionRequest,
};

use self::webhook_verifier::LinkedProviderWebhookVerifier;
use self::{
    ingest::{KnowledgeIngestionRunner, ProductionKnowledgeIngestionRunner},
    webhook_verifier::{KnowledgeWebhookVerifier, ParserWebhookVerifier},
};

/// Restate service surface for tenant knowledge-base operations.
#[restate_sdk::service]
#[name = "Knowledge"]
pub trait Knowledge {
    /// Creates a provider link token for one tenant.
    async fn create_link_token(
        request: Json<KnowledgeCreateLinkTokenRequest>,
    ) -> Result<Json<KnowledgeCreateLinkTokenResponse>, HandlerError>;

    /// Exchanges a provider public token and stores only a credential reference.
    async fn exchange_public_token(
        request: Json<KnowledgeExchangeTokenRequest>,
    ) -> Result<Json<KnowledgeExchangeTokenResponse>, HandlerError>;

    /// Starts a manual sync without running parsing or embedding inline.
    async fn sync_connection(
        request: Json<KnowledgeSyncRequest>,
    ) -> Result<Json<KnowledgeSyncResponse>, HandlerError>;

    /// Reads local sync-run status and counters.
    async fn sync_status(
        request: Json<KnowledgeSyncStatusRequest>,
    ) -> Result<Json<KnowledgeSyncStatusResponse>, HandlerError>;

    /// Reads ordered sync-run ingestion events.
    async fn sync_events(
        request: Json<KnowledgeSyncEventsRequest>,
    ) -> Result<Json<KnowledgeSyncEventsResponse>, HandlerError>;

    /// Lists tenant knowledge linked connections.
    async fn list_connections(
        request: Json<KnowledgeConnectionListRequest>,
    ) -> Result<Json<KnowledgeConnectionListResponse>, HandlerError>;

    /// Lists the integrations tenants can connect through enabled providers.
    async fn list_integrations(
        request: Json<KnowledgeIntegrationListRequest>,
    ) -> Result<Json<KnowledgeIntegrationListResponse>, HandlerError>;

    /// Updates provider-native selected sources for one linked connection.
    async fn update_connection_source_selection(
        request: Json<KnowledgeUpdateConnectionSourceSelectionRequest>,
    ) -> Result<Json<KnowledgeUpdateConnectionSourceSelectionResponse>, HandlerError>;

    /// Disconnects one linked tenant knowledge connection and revokes managed credentials.
    async fn disconnect_connection(
        request: Json<KnowledgeDisconnectConnectionRequest>,
    ) -> Result<Json<KnowledgeDisconnectConnectionResponse>, HandlerError>;

    /// Lists tenant knowledge source objects.
    async fn list_objects(
        request: Json<KnowledgeObjectListRequest>,
    ) -> Result<Json<KnowledgeObjectListResponse>, HandlerError>;

    /// Inspects one tenant knowledge source object safely.
    async fn inspect_object(
        request: Json<KnowledgeObjectInspectRequest>,
    ) -> Result<Json<KnowledgeObjectInspectResponse>, HandlerError>;

    /// Reads a tenant knowledge query trace.
    async fn query_trace(
        request: Json<KnowledgeQueryTraceRequest>,
    ) -> Result<Json<KnowledgeQueryTraceResponse>, HandlerError>;

    /// Processes a signed provider webhook.
    async fn provider_webhook(
        request: Json<KnowledgeProviderWebhookRequest>,
    ) -> Result<Json<KnowledgeProviderWebhookResponse>, HandlerError>;
}

/// Concrete knowledge service implementation.
#[derive(Clone)]
pub struct KnowledgeImpl {
    service: KnowledgeService,
    authz: crate::handlers::authz_shim::AuthzEnforcer,
}

impl Knowledge for KnowledgeImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn create_link_token(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeCreateLinkTokenRequest>,
    ) -> Result<Json<KnowledgeCreateLinkTokenResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "create_link_token");
        let request = request.into_inner();
        authorize_tenant(&self.authz, &ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .create_link_token(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_create_link_token")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn exchange_public_token(
        &self,
        mut ctx: Context<'_>,
        request: Json<KnowledgeExchangeTokenRequest>,
    ) -> Result<Json<KnowledgeExchangeTokenResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "exchange_public_token");
        let request = request.into_inner();
        let caller = authorize_knowledge_caller(&self.authz, &mut ctx, request.tenant_id).await?;
        let service = self.service.clone();
        let response = ctx
            .run(|| async move {
                service
                    .exchange_public_token(request, &caller)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_exchange_public_token")
            .await?
            .into_inner();
        if response
            .sync_status
            .as_deref()
            .is_some_and(should_dispatch_knowledge_sync_ingestion)
            && let Some(sync_run_uid) = response.sync_run_uid
        {
            Self::dispatch_knowledge_sync_ingestion(&ctx, sync_run_uid);
        }
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn sync_connection(
        &self,
        mut ctx: Context<'_>,
        request: Json<KnowledgeSyncRequest>,
    ) -> Result<Json<KnowledgeSyncResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "sync_connection");
        let request = request.into_inner();
        let caller = authorize_knowledge_caller(&self.authz, &mut ctx, request.tenant_id).await?;
        let service = self.service.clone();
        let response = ctx
            .run(|| async move {
                service
                    .sync_connection(request, &caller)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_sync_connection")
            .await?
            .into_inner();
        if should_dispatch_knowledge_sync_ingestion(&response.status) {
            Self::dispatch_knowledge_sync_ingestion(&ctx, response.sync_run_uid);
        }
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn sync_status(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeSyncStatusRequest>,
    ) -> Result<Json<KnowledgeSyncStatusResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "sync_status");
        let request = request.into_inner();
        authorize_tenant(&self.authz, &ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .sync_status(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_sync_status")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn sync_events(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeSyncEventsRequest>,
    ) -> Result<Json<KnowledgeSyncEventsResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "sync_events");
        let request = request.into_inner();
        authorize_tenant(&self.authz, &ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .sync_events(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_sync_events")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_connections(
        &self,
        mut ctx: Context<'_>,
        request: Json<KnowledgeConnectionListRequest>,
    ) -> Result<Json<KnowledgeConnectionListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "list_connections");
        let request = request.into_inner();
        let caller = authorize_knowledge_caller(&self.authz, &mut ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .list_connections(request, &caller)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_list_connections")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_integrations(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeIntegrationListRequest>,
    ) -> Result<Json<KnowledgeIntegrationListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "list_integrations");
        let request = request.into_inner();
        authorize_tenant(&self.authz, &ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .list_integrations(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_list_integrations")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn update_connection_source_selection(
        &self,
        mut ctx: Context<'_>,
        request: Json<KnowledgeUpdateConnectionSourceSelectionRequest>,
    ) -> Result<Json<KnowledgeUpdateConnectionSourceSelectionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "update_connection_source_selection");
        let request = request.into_inner();
        let caller = authorize_knowledge_caller(&self.authz, &mut ctx, request.tenant_id).await?;
        let service = self.service.clone();
        let response = ctx
            .run(|| async move {
                service
                    .update_connection_source_selection(request, &caller)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_update_connection_source_selection")
            .await?
            .into_inner();
        if response
            .sync_status
            .as_deref()
            .is_some_and(should_dispatch_knowledge_sync_ingestion)
            && let Some(sync_run_uid) = response.sync_run_uid
        {
            Self::dispatch_knowledge_sync_ingestion(&ctx, sync_run_uid);
        }
        Ok(Json::from(response))
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn disconnect_connection(
        &self,
        mut ctx: Context<'_>,
        request: Json<KnowledgeDisconnectConnectionRequest>,
    ) -> Result<Json<KnowledgeDisconnectConnectionResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "disconnect_connection");
        let request = request.into_inner();
        let caller = authorize_knowledge_caller(&self.authz, &mut ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .disconnect_connection(request, &caller)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_disconnect_connection")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_objects(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeObjectListRequest>,
    ) -> Result<Json<KnowledgeObjectListResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "list_objects");
        let request = request.into_inner();
        authorize_tenant(&self.authz, &ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .list_objects(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_list_objects")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn inspect_object(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeObjectInspectRequest>,
    ) -> Result<Json<KnowledgeObjectInspectResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "inspect_object");
        let request = request.into_inner();
        authorize_tenant(&self.authz, &ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .inspect_object(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_inspect_object")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn query_trace(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeQueryTraceRequest>,
    ) -> Result<Json<KnowledgeQueryTraceResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "query_trace");
        let request = request.into_inner();
        authorize_tenant(&self.authz, &ctx, request.tenant_id).await?;
        let service = self.service.clone();
        Ok(ctx
            .run(|| async move {
                service
                    .query_trace(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_query_trace")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    // SAFETY: Provider webhooks do not carry caller auth; provider adapters verify the raw signature before tenant-scoped idempotency writes.
    async fn provider_webhook(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeProviderWebhookRequest>,
    ) -> Result<Json<KnowledgeProviderWebhookResponse>, HandlerError> {
        crate::ctx::adopt_incoming_trace_parent(&ctx);
        annotate_restate_handler_span("Knowledge", "provider_webhook");
        let request = request.into_inner();
        let service = self.service.clone();
        let response = ctx
            .run(|| async move {
                service
                    .provider_webhook(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_provider_webhook")
            .await?
            .into_inner();
        if response.ingestion_enqueued
            && let Some(sync_run_uid) = response.sync_run_uid
        {
            Self::dispatch_knowledge_sync_ingestion(&ctx, sync_run_uid);
        }
        Ok(Json::from(response))
    }
}

impl KnowledgeImpl {
    /// Creates the Restate handler from the existing knowledge application service.
    #[must_use]
    pub fn new(
        service: KnowledgeService,
        authz: crate::handlers::authz_shim::AuthzEnforcer,
    ) -> Self {
        Self { service, authz }
    }

    fn dispatch_knowledge_sync_ingestion(ctx: &Context<'_>, sync_run_uid: Uuid) {
        crate::restate_identity::replay_safe_request(
            ctx.workflow_client::<KnowledgeSyncIngestionClient>(sync_run_uid.to_string())
                .run(Json::from(KnowledgeSyncIngestionRequest { sync_run_uid })),
        )
        .send();
    }
}

/// Application logic for the Knowledge Restate service.
#[derive(Clone)]
pub struct KnowledgeService {
    repository: KnowledgeRepositorySource,
    discovery: Arc<dyn KnowledgeDiscoveryStore>,
    providers: Arc<dyn KnowledgeProviderResolver>,
    credentials: Arc<dyn KnowledgeCredentialStore>,
    connector_connections: Option<Arc<dyn KnowledgeConnectorConnections>>,
    ingestion_runner: Arc<dyn KnowledgeIngestionRunner>,
    max_preview_chars: usize,
    lineage_clickhouse: Option<Arc<moa_lineage_sink::ClickHouseStore>>,
}

impl KnowledgeService {
    /// Creates a knowledge service with explicit dependencies for tests or alternate runtimes.
    #[must_use]
    pub fn new(
        repository: KnowledgeRepositoryCapabilities,
        discovery: Arc<dyn KnowledgeDiscoveryStore>,
        providers: Arc<dyn KnowledgeProviderResolver>,
        credentials: Arc<dyn KnowledgeCredentialStore>,
        ingestion_runner: Arc<dyn KnowledgeIngestionRunner>,
        max_preview_chars: usize,
    ) -> Self {
        Self {
            repository: KnowledgeRepositorySource::Fixed {
                connection: repository.connection,
                sync: repository.sync,
                ingestion: repository.ingestion,
                event: repository.event,
            },
            discovery,
            providers,
            credentials,
            connector_connections: None,
            ingestion_runner,
            max_preview_chars,
            lineage_clickhouse: None,
        }
    }

    /// Creates a knowledge service with explicit repository and connector ports.
    #[must_use]
    pub fn new_with_connector_connections(
        repository: KnowledgeRepositoryCapabilities,
        discovery: Arc<dyn KnowledgeDiscoveryStore>,
        providers: Arc<dyn KnowledgeProviderResolver>,
        credentials: Arc<dyn KnowledgeCredentialStore>,
        ingestion_runner: Arc<dyn KnowledgeIngestionRunner>,
        max_preview_chars: usize,
        connector_connections: Arc<dyn KnowledgeConnectorConnections>,
    ) -> Self {
        Self {
            repository: KnowledgeRepositorySource::Fixed {
                connection: repository.connection,
                sync: repository.sync,
                ingestion: repository.ingestion,
                event: repository.event,
            },
            discovery,
            providers,
            credentials,
            connector_connections: Some(connector_connections),
            ingestion_runner,
            max_preview_chars,
            lineage_clickhouse: None,
        }
    }

    /// Creates a knowledge service backed by tenant-scoped Postgres repositories.
    #[must_use]
    pub fn from_postgres_pool(
        pool: sqlx::PgPool,
        providers: Arc<dyn KnowledgeProviderResolver>,
        credentials: Arc<dyn KnowledgeCredentialStore>,
        ingestion_runner: Arc<dyn KnowledgeIngestionRunner>,
        max_preview_chars: usize,
    ) -> Self {
        let discovery = Arc::new(PostgresKnowledgeDiscoveryStore::new(pool.clone()));
        Self {
            repository: KnowledgeRepositorySource::Postgres { pool },
            discovery,
            providers,
            credentials,
            connector_connections: None,
            ingestion_runner,
            max_preview_chars,
            lineage_clickhouse: None,
        }
    }

    /// Creates a tenant-scoped Postgres knowledge service with connector support.
    #[must_use]
    pub fn from_postgres_pool_with_connector_connections(
        pool: sqlx::PgPool,
        providers: Arc<dyn KnowledgeProviderResolver>,
        credentials: Arc<dyn KnowledgeCredentialStore>,
        ingestion_runner: Arc<dyn KnowledgeIngestionRunner>,
        max_preview_chars: usize,
        connector_connections: Arc<dyn KnowledgeConnectorConnections>,
    ) -> Self {
        let discovery = Arc::new(PostgresKnowledgeDiscoveryStore::new(pool.clone()));
        Self {
            repository: KnowledgeRepositorySource::Postgres { pool },
            discovery,
            providers,
            credentials,
            connector_connections: Some(connector_connections),
            ingestion_runner,
            max_preview_chars,
            lineage_clickhouse: None,
        }
    }

    /// Creates the config-backed production knowledge application service.
    ///
    /// `credential_vault` is the process's single durable credential owner: it
    /// is constructed once during runtime composition and shared, so a
    /// credential written on one replica resolves on every other.
    #[must_use]
    pub(crate) fn from_config(
        pool: sqlx::PgPool,
        kms: Arc<dyn moa_crypto::KeyManagementProvider>,
        credential_vault: Arc<dyn CredentialVault>,
        config: &MoaConfig,
        runtime_cache: Arc<dyn RuntimeCacheStore>,
        connector_connections: Arc<dyn KnowledgeConnectorConnections>,
    ) -> Self {
        Self::from_postgres_pool_with_connector_connections(
            pool.clone(),
            Arc::new(ConfigKnowledgeProviders::new(config.knowledge.clone())),
            Arc::new(VaultKnowledgeCredentialStore::new(credential_vault)),
            Arc::new(ProductionKnowledgeIngestionRunner::new(
                pool,
                kms,
                config.clone(),
                runtime_cache,
            )),
            config.knowledge.observability.max_object_preview_chars,
            connector_connections,
        )
        .with_clickhouse_lineage(
            config
                .clickhouse
                .as_ref()
                .map(|clickhouse| Arc::new(moa_lineage_sink::ClickHouseStore::connect(clickhouse))),
        )
    }

    /// Points retrieval-trace reads at ClickHouse when that lineage backend
    /// is configured; `None` keeps the Postgres reads.
    #[must_use]
    pub fn with_clickhouse_lineage(
        mut self,
        lineage_clickhouse: Option<Arc<moa_lineage_sink::ClickHouseStore>>,
    ) -> Self {
        self.lineage_clickhouse = lineage_clickhouse;
        self
    }

    /// Returns the injected page-ingestion runner.
    #[must_use]
    pub fn ingestion_runner(&self) -> Arc<dyn KnowledgeIngestionRunner> {
        self.ingestion_runner.clone()
    }

    fn provider(
        &self,
        provider: LinkedProviderKind,
    ) -> Result<Arc<dyn LinkedIntegrationProvider>, KnowledgeServiceError> {
        self.providers.provider(provider)
    }

    fn webhook_verifier(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn KnowledgeWebhookVerifier>, KnowledgeServiceError> {
        self.providers.webhook_verifier(provider)
    }

    fn connection_repository(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeConnectionRepository> {
        self.repository.connection(tenant_id)
    }

    fn sync_repository(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeSyncRepository> {
        self.repository.sync(tenant_id)
    }

    fn ingestion_repository(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeIngestionRepository> {
        self.repository.ingestion(tenant_id)
    }

    fn event_repository(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeEventRepository> {
        self.repository.event(tenant_id)
    }

    fn discovery(&self) -> &dyn KnowledgeDiscoveryStore {
        self.discovery.as_ref()
    }

    fn connector_connections(
        &self,
    ) -> Result<&dyn KnowledgeConnectorConnections, KnowledgeServiceError> {
        self.connector_connections.as_deref().ok_or_else(|| {
            KnowledgeServiceError::InvalidRequest(
                "generic connector lifecycle service is not configured".to_string(),
            )
        })
    }

    /// Resolves one connection's provider credential for an outbound request.
    ///
    /// The plaintext never touches the connection row: it is returned as a
    /// non-serializable carrier that the caller hands straight to the provider
    /// request, so it cannot reach Restate state, an event, or a knowledge row.
    async fn resolve_connection_credential(
        &self,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<Option<RedactedSecret>, KnowledgeServiceError> {
        self.credentials
            .resolve_linked_account(connection.tenant_id, connection, caller)
            .await
    }

    fn postgres_pool(&self) -> Option<sqlx::PgPool> {
        match &self.repository {
            KnowledgeRepositorySource::Fixed { .. } => None,
            KnowledgeRepositorySource::Postgres { pool } => Some(pool.clone()),
        }
    }
}

/// Narrow knowledge adapter over the shared generic connector lifecycle service.
///
/// The production implementation delegates every method to one
/// [`ConnectorService`]. The trait exists only so service tests can observe
/// ordering and generation fences without implementing the connector
/// repository itself.
#[async_trait]
pub trait KnowledgeConnectorConnections: Send + Sync {
    /// Claims or exactly resumes the managed parent for one link operation.
    async fn claim_managed_parent(
        &self,
        request: ManagedParentClaimRequest,
    ) -> moa_connectors::Result<ManagedParentClaim>;

    /// Activates a knowledge-only managed parent without inventing actions.
    async fn activate_managed_knowledge_parent(
        &self,
        request: ManagedParentActivationRequest,
    ) -> moa_connectors::Result<ConnectorConnection>;

    /// Advances the generation fence after a staged credential write.
    async fn advance_credential_generation(
        &self,
        request: CredentialGenerationFenceRequest,
    ) -> moa_connectors::Result<ConnectorConnection>;

    /// Loads one same-tenant generic connector parent.
    async fn get(
        &self,
        tenant_id: TenantId,
        connection_id: moa_core::types::identifiers::ConnectorConnectionId,
    ) -> moa_connectors::Result<Option<ConnectorConnection>>;

    /// Fences an active or suspended parent into disconnecting.
    async fn disconnect(
        &self,
        tenant_id: TenantId,
        connection_id: moa_core::types::identifiers::ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> moa_connectors::Result<ConnectorConnection>;

    /// Marks a disconnecting parent deleted after all child teardown completes.
    async fn delete(
        &self,
        tenant_id: TenantId,
        connection_id: moa_core::types::identifiers::ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> moa_connectors::Result<ConnectorConnection>;

    /// Deletes a claim-created managed parent only when no capability depends on it.
    async fn delete_managed_parent_if_unused(
        &self,
        request: ManagedParentDeleteRequest,
    ) -> moa_connectors::Result<ManagedParentDeleteOutcome>;

    /// Records health independently of lifecycle under the observed generation.
    async fn update_health(
        &self,
        tenant_id: TenantId,
        connection_id: moa_core::types::identifiers::ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        health: ConnectionHealth,
        reason: Option<String>,
    ) -> moa_connectors::Result<ConnectorConnection>;
}

#[async_trait]
impl KnowledgeConnectorConnections for ConnectorService {
    async fn claim_managed_parent(
        &self,
        request: ManagedParentClaimRequest,
    ) -> moa_connectors::Result<ManagedParentClaim> {
        self.claim_managed_parent(request).await
    }

    async fn activate_managed_knowledge_parent(
        &self,
        request: ManagedParentActivationRequest,
    ) -> moa_connectors::Result<ConnectorConnection> {
        self.activate_managed_knowledge_parent(request).await
    }

    async fn advance_credential_generation(
        &self,
        request: CredentialGenerationFenceRequest,
    ) -> moa_connectors::Result<ConnectorConnection> {
        self.advance_credential_generation(request).await
    }

    async fn get(
        &self,
        tenant_id: TenantId,
        connection_id: moa_core::types::identifiers::ConnectorConnectionId,
    ) -> moa_connectors::Result<Option<ConnectorConnection>> {
        self.get(tenant_id, connection_id).await
    }

    async fn disconnect(
        &self,
        tenant_id: TenantId,
        connection_id: moa_core::types::identifiers::ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> moa_connectors::Result<ConnectorConnection> {
        self.disconnect(tenant_id, connection_id, expected_generation)
            .await
    }

    async fn delete(
        &self,
        tenant_id: TenantId,
        connection_id: moa_core::types::identifiers::ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
    ) -> moa_connectors::Result<ConnectorConnection> {
        self.delete(tenant_id, connection_id, expected_generation)
            .await
    }

    async fn delete_managed_parent_if_unused(
        &self,
        request: ManagedParentDeleteRequest,
    ) -> moa_connectors::Result<ManagedParentDeleteOutcome> {
        self.delete_managed_parent_if_unused(request).await
    }

    async fn update_health(
        &self,
        tenant_id: TenantId,
        connection_id: moa_core::types::identifiers::ConnectorConnectionId,
        expected_generation: ConnectionGeneration,
        health: ConnectionHealth,
        reason: Option<String>,
    ) -> moa_connectors::Result<ConnectorConnection> {
        self.update_health(
            tenant_id,
            connection_id,
            expected_generation,
            health,
            reason,
        )
        .await
    }
}

/// Independent repository capabilities used by the knowledge application service.
///
/// Each port can be backed by a different implementation, which keeps service
/// construction aligned with the narrow domain capabilities it actually uses.
#[derive(Clone)]
pub struct KnowledgeRepositoryCapabilities {
    connection: Arc<dyn KnowledgeConnectionRepository>,
    sync: Arc<dyn KnowledgeSyncRepository>,
    ingestion: Arc<dyn KnowledgeIngestionRepository>,
    event: Arc<dyn KnowledgeEventRepository>,
}

impl KnowledgeRepositoryCapabilities {
    /// Builds a repository capability bundle from four independently owned ports.
    #[must_use]
    pub fn new(
        connection: Arc<dyn KnowledgeConnectionRepository>,
        sync: Arc<dyn KnowledgeSyncRepository>,
        ingestion: Arc<dyn KnowledgeIngestionRepository>,
        event: Arc<dyn KnowledgeEventRepository>,
    ) -> Self {
        Self {
            connection,
            sync,
            ingestion,
            event,
        }
    }
}

#[derive(Clone)]
enum KnowledgeRepositorySource {
    Fixed {
        connection: Arc<dyn KnowledgeConnectionRepository>,
        sync: Arc<dyn KnowledgeSyncRepository>,
        ingestion: Arc<dyn KnowledgeIngestionRepository>,
        event: Arc<dyn KnowledgeEventRepository>,
    },
    Postgres {
        pool: sqlx::PgPool,
    },
}

impl KnowledgeRepositorySource {
    fn connection(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeConnectionRepository> {
        match self {
            Self::Fixed { connection, .. } => connection.clone(),
            Self::Postgres { pool } => Arc::new(PostgresKnowledgeRepository::scoped(
                pool.clone(),
                RlsContext::tenant(tenant_id),
            )),
        }
    }

    fn sync(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeSyncRepository> {
        match self {
            Self::Fixed { sync, .. } => sync.clone(),
            Self::Postgres { pool } => Arc::new(PostgresKnowledgeRepository::scoped(
                pool.clone(),
                RlsContext::tenant(tenant_id),
            )),
        }
    }

    fn ingestion(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeIngestionRepository> {
        match self {
            Self::Fixed { ingestion, .. } => ingestion.clone(),
            Self::Postgres { pool } => Arc::new(PostgresKnowledgeRepository::scoped(
                pool.clone(),
                RlsContext::tenant(tenant_id),
            )),
        }
    }

    fn event(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeEventRepository> {
        match self {
            Self::Fixed { event, .. } => event.clone(),
            Self::Postgres { pool } => Arc::new(PostgresKnowledgeRepository::scoped(
                pool.clone(),
                RlsContext::tenant(tenant_id),
            )),
        }
    }
}

fn parse_linked_provider(provider: &str) -> Result<LinkedProviderKind, KnowledgeServiceError> {
    LinkedProviderKind::from_str_exact(provider)
        .ok_or_else(|| KnowledgeServiceError::UnknownProvider(provider.to_string()))
}

/// Resolves linked-integration providers by stable provider identifier.
pub trait KnowledgeProviderResolver: Send + Sync {
    /// Returns the provider implementation for a selected provider identifier.
    fn provider(
        &self,
        provider: LinkedProviderKind,
    ) -> Result<Arc<dyn LinkedIntegrationProvider>, KnowledgeServiceError>;

    /// Returns the resolvable provider identifiers in deterministic order.
    fn provider_ids(&self) -> Vec<LinkedProviderKind>;

    /// Returns the webhook verifier for a selected provider identifier.
    fn webhook_verifier(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn KnowledgeWebhookVerifier>, KnowledgeServiceError> {
        Ok(Arc::new(LinkedProviderWebhookVerifier::new(
            self.provider(parse_linked_provider(provider)?)?,
        )))
    }
}

/// Static provider resolver used by offline service tests.
#[derive(Clone, Default)]
pub struct StaticKnowledgeProviders {
    providers: HashMap<LinkedProviderKind, Arc<dyn LinkedIntegrationProvider>>,
    webhook_verifiers: HashMap<String, Arc<dyn KnowledgeWebhookVerifier>>,
}

impl StaticKnowledgeProviders {
    /// Builds an empty static provider resolver.
    #[must_use]
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
            webhook_verifiers: HashMap::new(),
        }
    }

    /// Adds a provider implementation under a stable provider identifier.
    #[must_use]
    pub fn with_provider(
        mut self,
        provider: LinkedProviderKind,
        implementation: Arc<dyn LinkedIntegrationProvider>,
    ) -> Self {
        self.providers.insert(provider, implementation);
        self
    }

    /// Adds a webhook verifier under a stable provider identifier.
    #[must_use]
    pub fn with_webhook_verifier(
        mut self,
        provider: impl Into<String>,
        verifier: Arc<dyn KnowledgeWebhookVerifier>,
    ) -> Self {
        self.webhook_verifiers.insert(provider.into(), verifier);
        self
    }
}

impl KnowledgeProviderResolver for StaticKnowledgeProviders {
    fn provider(
        &self,
        provider: LinkedProviderKind,
    ) -> Result<Arc<dyn LinkedIntegrationProvider>, KnowledgeServiceError> {
        self.providers
            .get(&provider)
            .cloned()
            .ok_or_else(|| KnowledgeServiceError::UnknownProvider(provider.to_string()))
    }

    fn provider_ids(&self) -> Vec<LinkedProviderKind> {
        let mut ids: Vec<LinkedProviderKind> = self.providers.keys().copied().collect();
        ids.sort_by_key(|provider| provider.as_str());
        ids
    }

    fn webhook_verifier(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn KnowledgeWebhookVerifier>, KnowledgeServiceError> {
        if let Some(verifier) = self.webhook_verifiers.get(provider) {
            return Ok(verifier.clone());
        }
        Ok(Arc::new(LinkedProviderWebhookVerifier::new(
            self.provider(parse_linked_provider(provider)?)?,
        )))
    }
}

/// Authorized principal and replay-stable operation identity for one knowledge call.
///
/// Handlers build this from the identity returned by the tenant authorization
/// check plus a Restate-deterministic operation id, so every credential
/// operation performed while serving one request is attributable to a principal
/// that already passed `(Tenant, tenant_id, Operator)` and replays onto the same
/// audit rows instead of appending new ones.
#[derive(Debug, Clone)]
pub struct KnowledgeCaller {
    principal: CredentialPrincipal,
    operation_id: String,
}

impl KnowledgeCaller {
    /// Builds the caller context from an authorized identity.
    ///
    /// `acting_on_behalf_of` becomes the delegating owner, so an agent acting
    /// for a user is recorded as delegated rather than as the owner itself.
    #[must_use]
    pub fn authorized(identity: &Identity, operation_id: impl Into<String>) -> Self {
        Self {
            principal: CredentialPrincipal::Caller {
                identity_id: identity.id,
                delegated_by: identity.acting_on_behalf_of,
            },
            operation_id: operation_id.into(),
        }
    }

    /// Builds the caller context for a durable service actor.
    ///
    /// Reserved for reconstructed workflows, which have no caller to attribute;
    /// the actor is a closed allowlist entry that can only resolve.
    #[must_use]
    pub fn service(actor: CredentialServiceActor, operation_id: impl Into<String>) -> Self {
        Self {
            principal: CredentialPrincipal::Service { actor },
            operation_id: operation_id.into(),
        }
    }

    /// Returns the acting principal.
    #[must_use]
    pub fn principal(&self) -> CredentialPrincipal {
        self.principal
    }

    /// Returns the replay-stable operation id root.
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// Derives the replay-stable operation id for one credential step.
    ///
    /// One request can perform several distinct credential operations, and the
    /// audit's replay key is unique per `(tenant, operation_id)`. Suffixing keeps
    /// each step separately replayable instead of colliding as an idempotency
    /// conflict.
    #[must_use]
    pub fn step(&self, step: &str) -> String {
        format!("{}:{step}", self.operation_id)
    }
}

/// Stores linked-account credentials owned by the generic connector connection.
#[async_trait]
pub trait KnowledgeCredentialStore: Send + Sync {
    /// Stages one linked-account credential without changing the active version.
    ///
    /// `connection_uid` is generated before the credential is written so the
    /// stored version is bound to its owning connection from the start and can
    /// never be adopted by a different one.
    async fn stage_linked_account(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        caller: &KnowledgeCaller,
        account: &LinkedAccount,
    ) -> Result<StagedKnowledgeCredential, KnowledgeServiceError>;

    /// Activates one exact staged linked-account credential under vault CAS.
    async fn activate_staged_linked_account(
        &self,
        staged: &StagedKnowledgeCredential,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError>;

    /// Rolls back an activated candidate to its exact staged predecessor.
    async fn rollback_linked_account_activation(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        candidate_credential_ref: &str,
        previous_credential_ref: Option<&str>,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError>;

    /// Resolves one connection's credential immediately before a provider request.
    ///
    /// Merge returns a non-serializable [`RedactedSecret`] from the shared vault;
    /// Nango returns `None` because its deployment credential belongs to the
    /// provider adapter rather than to a tenant connection.
    async fn resolve_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<Option<RedactedSecret>, KnowledgeServiceError>;

    /// Revokes every MOA-managed credential version for a linked account.
    ///
    /// Disconnect retains version and audit history; destructive deletion is
    /// reserved for tenant purge.
    async fn revoke_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<bool, KnowledgeServiceError>;

    /// Reports credential status for exact, already-authorized connections.
    ///
    /// The returned vector matches input order. Provider-native handles return
    /// `None`; managed references report present, revoked, superseded, or
    /// missing. Implementations must use one batch metadata read rather than a
    /// tenant credential enumeration or one read per connection.
    async fn credential_statuses(
        &self,
        tenant_id: TenantId,
        connections: &[&KnowledgeConnection],
        caller: &KnowledgeCaller,
    ) -> Result<Vec<Option<String>>, KnowledgeServiceError>;
}

/// Host-local result of staging a knowledge provider credential.
///
/// The managed variant contains the vault's non-serializable staging receipt;
/// neither variant carries plaintext after the stage call returns.
pub enum StagedKnowledgeCredential {
    /// The provider owns credential material outside the tenant connection.
    ProviderNative,
    /// MOA owns an inactive credential version awaiting generation fencing.
    Managed {
        /// Exact inactive version and predecessor receipt retained host-locally.
        staging: CredentialStagingToken,
    },
}

impl StagedKnowledgeCredential {
    /// Returns the closed credential owner selected by the managed definition.
    #[must_use]
    pub const fn credential_ownership(&self) -> KnowledgeCredentialOwnership {
        match self {
            Self::ProviderNative => KnowledgeCredentialOwnership::ProviderNative,
            Self::Managed { .. } => KnowledgeCredentialOwnership::MoaManaged,
        }
    }

    /// Returns the exact candidate vault receipt, only for MOA-managed material.
    #[must_use]
    pub fn vault_candidate_reference(&self) -> Option<String> {
        match self {
            Self::ProviderNative => None,
            Self::Managed { staging, .. } => Some(staging.staged_reference().to_string()),
        }
    }

    /// Returns the exact active vault predecessor observed while staging.
    #[must_use]
    pub fn previous_vault_reference(&self) -> Option<String> {
        match self {
            Self::ProviderNative => None,
            Self::Managed { staging, .. } => staging
                .expected_prior_active()
                .map(|reference| reference.to_string()),
        }
    }

    /// Returns whether this stage requires a connector generation advance.
    #[must_use]
    pub const fn is_managed(&self) -> bool {
        matches!(self, Self::Managed { .. })
    }
}

/// Credential store backed by MOA's durable tenant credential owner.
#[derive(Clone)]
pub struct VaultKnowledgeCredentialStore {
    vault: Arc<dyn CredentialVault>,
}

impl VaultKnowledgeCredentialStore {
    /// Creates a knowledge credential store from the shared credential vault.
    #[must_use]
    pub fn new(vault: Arc<dyn CredentialVault>) -> Self {
        Self { vault }
    }

    /// Builds the replay-stable context for one credential operation.
    fn context(
        tenant_id: TenantId,
        principal: CredentialPrincipal,
        operation: CredentialOperation,
        operation_id: &str,
        selector: &[&str],
    ) -> CredentialContext {
        CredentialContext {
            tenant_id,
            principal,
            operation,
            operation_id: operation_id.to_string(),
            request_hash: canonical_request_hash(tenant_id, operation, selector),
        }
    }
}

#[async_trait]
impl KnowledgeCredentialStore for VaultKnowledgeCredentialStore {
    async fn stage_linked_account(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        caller: &KnowledgeCaller,
        account: &LinkedAccount,
    ) -> Result<StagedKnowledgeCredential, KnowledgeServiceError> {
        match ManagedParentDefinition::for_knowledge_provider(account.provider.as_str())? {
            ManagedParentDefinition::KnowledgeNango => {
                if account.credential_material.is_some() {
                    return Err(KnowledgeServiceError::InvalidRequest(
                        "Nango linked accounts must not return tenant credential material"
                            .to_string(),
                    ));
                }
                return Ok(StagedKnowledgeCredential::ProviderNative);
            }
            ManagedParentDefinition::KnowledgeMerge => {}
        }
        let material = account.credential_material.as_deref().ok_or_else(|| {
            KnowledgeServiceError::InvalidRequest(
                "Merge linked accounts must return tenant credential material".to_string(),
            )
        })?;
        let identity = CredentialIdentity {
            tenant_id,
            connection_uid,
            kind: CredentialKind::ProviderApiKey,
            slot_name: CredentialSlotName::PRIMARY,
        };
        let staging = self
            .vault
            .stage(
                identity,
                SecretString::from(material.to_string()),
                &Self::context(
                    tenant_id,
                    caller.principal(),
                    CredentialOperation::Stage,
                    &caller.step("credential-stage"),
                    &[
                        &connection_uid.to_string(),
                        CredentialKind::ProviderApiKey.as_str(),
                        CredentialSlotName::PRIMARY.as_str(),
                        account.provider.as_str(),
                        &account.provider_account_id,
                    ],
                ),
            )
            .await
            .map_err(credential_error)?;
        Ok(StagedKnowledgeCredential::Managed { staging })
    }

    async fn activate_staged_linked_account(
        &self,
        staged: &StagedKnowledgeCredential,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError> {
        let StagedKnowledgeCredential::Managed { staging } = staged else {
            return Ok(());
        };
        let identity = staging.identity();
        self.vault
            .activate_staged(
                staging,
                &Self::context(
                    identity.tenant_id,
                    caller.principal(),
                    CredentialOperation::Activate,
                    &caller.step("credential-activate"),
                    &[
                        &identity.connection_uid.to_string(),
                        CredentialKind::ProviderApiKey.as_str(),
                        CredentialSlotName::PRIMARY.as_str(),
                        &staging.staged_reference().to_string(),
                    ],
                ),
            )
            .await
            .map(|_| ())
            .map_err(credential_error)
    }

    async fn rollback_linked_account_activation(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        candidate_credential_ref: &str,
        previous_credential_ref: Option<&str>,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError> {
        let candidate = parse_vault_receipt(candidate_credential_ref)?;
        let previous = previous_credential_ref
            .map(parse_vault_receipt)
            .transpose()?;
        match self
            .vault
            .rollback_activation(
                candidate,
                previous,
                &Self::context(
                    tenant_id,
                    caller.principal(),
                    CredentialOperation::RollbackActivation,
                    &caller.step("credential-rollback-activation"),
                    &[
                        &connection_uid.to_string(),
                        &candidate.to_string(),
                        previous_credential_ref.unwrap_or("none"),
                    ],
                ),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(CredentialError::StaleVersion) => self
                .vault
                .revoke(
                    candidate,
                    &Self::context(
                        tenant_id,
                        caller.principal(),
                        CredentialOperation::Revoke,
                        &caller.step("credential-revoke-staged"),
                        &[&connection_uid.to_string(), &candidate.to_string()],
                    ),
                )
                .await
                .map_err(credential_error),
            Err(CredentialError::Revoked) => {
                let described = self
                    .vault
                    .describe_batch(
                        &[(connection_uid, candidate)],
                        &Self::context(
                            tenant_id,
                            caller.principal(),
                            CredentialOperation::Resolve,
                            &caller.step("credential-describe-rollback-candidate"),
                            &[&connection_uid.to_string(), &candidate.to_string()],
                        ),
                    )
                    .await
                    .map_err(credential_error)?;
                if matches!(
                    described.as_slice(),
                    [(described_connection, version)]
                        if *described_connection == connection_uid
                            && version.reference == candidate
                            && version.revoked
                            && !version.active
                ) {
                    Ok(())
                } else {
                    Err(credential_error(CredentialError::Revoked))
                }
            }
            Err(error) => Err(credential_error(error)),
        }
    }

    async fn resolve_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<Option<RedactedSecret>, KnowledgeServiceError> {
        if connection.tenant_id != tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }
        match ManagedParentDefinition::for_knowledge_provider(connection.provider.as_str())? {
            ManagedParentDefinition::KnowledgeNango => {
                return Ok(None);
            }
            ManagedParentDefinition::KnowledgeMerge => {}
        }
        let identity = CredentialIdentity {
            tenant_id,
            connection_uid: connection.connection_uid,
            kind: CredentialKind::ProviderApiKey,
            slot_name: CredentialSlotName::PRIMARY,
        };
        self.vault
            .resolve_active(
                &identity,
                &Self::context(
                    tenant_id,
                    caller.principal(),
                    CredentialOperation::Resolve,
                    &caller.step("credential-resolve"),
                    &[
                        &connection.connection_uid.to_string(),
                        CredentialKind::ProviderApiKey.as_str(),
                        CredentialSlotName::PRIMARY.as_str(),
                    ],
                ),
            )
            .await
            .map(Some)
            .map_err(credential_error)
    }

    async fn revoke_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<bool, KnowledgeServiceError> {
        if connection.tenant_id != tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }
        if ManagedParentDefinition::for_knowledge_provider(connection.provider.as_str())?
            == ManagedParentDefinition::KnowledgeNango
        {
            return Ok(false);
        }
        let revoked = self
            .vault
            .revoke_connection(
                connection.connection_uid,
                &Self::context(
                    tenant_id,
                    caller.principal(),
                    CredentialOperation::Revoke,
                    &caller.step("credential-revoke-connection"),
                    &[&connection.connection_uid.to_string()],
                ),
            )
            .await
            .map_err(credential_error)?;
        Ok(revoked > 0)
    }

    async fn credential_statuses(
        &self,
        tenant_id: TenantId,
        connections: &[&KnowledgeConnection],
        caller: &KnowledgeCaller,
    ) -> Result<Vec<Option<String>>, KnowledgeServiceError> {
        let mut identities = Vec::new();
        let mut merge_positions = Vec::new();
        let mut selectors = Vec::new();
        for (position, connection) in connections.iter().enumerate() {
            if connection.tenant_id != tenant_id {
                return Err(KnowledgeServiceError::NotFound("knowledge connection"));
            }
            match ManagedParentDefinition::for_knowledge_provider(connection.provider.as_str())? {
                ManagedParentDefinition::KnowledgeNango => {}
                ManagedParentDefinition::KnowledgeMerge => {
                    identities.push(CredentialIdentity {
                        tenant_id,
                        connection_uid: connection.connection_uid,
                        kind: CredentialKind::ProviderApiKey,
                        slot_name: CredentialSlotName::PRIMARY,
                    });
                    merge_positions.push(position);
                    selectors.push(connection.connection_uid.to_string());
                }
            }
        }
        let selector_refs = selectors.iter().map(String::as_str).collect::<Vec<_>>();
        let ctx = Self::context(
            tenant_id,
            caller.principal(),
            CredentialOperation::Resolve,
            &caller.step("credential-status-batch"),
            &selector_refs,
        );
        let active = self
            .vault
            .has_active_batch(&identities, &ctx)
            .await
            .map_err(credential_error)?;
        if active.len() != merge_positions.len() {
            return Err(KnowledgeServiceError::Credential(
                "credential readiness batch returned the wrong result count".to_string(),
            ));
        }
        let mut statuses = vec![None; connections.len()];
        for (position, active) in merge_positions.into_iter().zip(active) {
            statuses[position] = Some(if active { "present" } else { "missing" }.to_string());
        }
        Ok(statuses)
    }
}

/// Decodes one explicitly managed vault receipt after ownership was established.
fn parse_vault_receipt(value: &str) -> Result<CredentialRef, KnowledgeServiceError> {
    Uuid::parse_str(value)
        .map(CredentialRef::from_uuid)
        .map_err(|_| {
            KnowledgeServiceError::Credential(
                "managed credential receipt is not a valid vault reference".to_string(),
            )
        })
}

/// Builds the canonical, secret-free request hash for one credential operation.
///
/// The hash covers the tenant, the operation, and the operation's selector, and
/// never the material. Replaying the same logical operation reproduces the same
/// hash; changing any selector field with the same operation id is a typed
/// idempotency conflict instead of a silent overwrite.
fn canonical_request_hash(
    tenant_id: TenantId,
    operation: CredentialOperation,
    selector: &[&str],
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tenant_id.to_string().as_bytes());
    hasher.update([0]);
    hasher.update(operation.as_str().as_bytes());
    for part in selector {
        hasher.update([0]);
        hasher.update(part.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// Maps a typed vault failure onto the knowledge service error surface.
fn credential_error(error: CredentialError) -> KnowledgeServiceError {
    KnowledgeServiceError::Credential(error.to_string())
}

/// Config-backed production provider resolver used by services and internal workflows.
#[derive(Clone)]
pub(crate) struct ConfigKnowledgeProviders {
    config: moa_config::KnowledgeConfig,
}

impl ConfigKnowledgeProviders {
    /// Builds a provider resolver from tenant knowledge configuration.
    pub(crate) fn new(config: moa_config::KnowledgeConfig) -> Self {
        Self { config }
    }
}

impl KnowledgeProviderResolver for ConfigKnowledgeProviders {
    fn provider(
        &self,
        provider: LinkedProviderKind,
    ) -> Result<Arc<dyn LinkedIntegrationProvider>, KnowledgeServiceError> {
        match provider {
            LinkedProviderKind::Nango => {
                let api_key = self.config.selected_provider_api_key(provider.as_str())?;
                let mut implementation =
                    NangoProvider::new(self.config.nango.api_base_url.clone(), api_key)?;
                if let Some(signing_key) =
                    moa_config::optional_config_secret(&self.config.nango.webhook_signing_key)
                {
                    implementation = implementation.with_webhook_signing_key(signing_key);
                }
                Ok(Arc::new(implementation))
            }
            LinkedProviderKind::Merge => {
                let api_key = self.config.selected_provider_api_key(provider.as_str())?;
                let mut implementation =
                    MergeProvider::new(self.config.merge.api_base_url.clone(), api_key)?;
                if let Some(signature_key) =
                    moa_config::optional_config_secret(&self.config.merge.webhook_signature_key)
                {
                    implementation = implementation.with_webhook_signature_key(signature_key);
                }
                Ok(Arc::new(implementation))
            }
        }
    }

    fn provider_ids(&self) -> Vec<LinkedProviderKind> {
        [LinkedProviderKind::Merge, LinkedProviderKind::Nango]
            .into_iter()
            .filter(|provider| {
                self.config
                    .providers
                    .enabled
                    .iter()
                    .any(|candidate| candidate == provider.as_str())
            })
            .collect()
    }

    fn webhook_verifier(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn KnowledgeWebhookVerifier>, KnowledgeServiceError> {
        match provider {
            "nango" | "merge" => Ok(Arc::new(LinkedProviderWebhookVerifier::new(
                self.provider(parse_linked_provider(provider)?)?,
            ))),
            "llamaparse" => self.parser_webhook_verifier("llamaparse"),
            "reducto" => self.parser_webhook_verifier("reducto"),
            other => Err(KnowledgeServiceError::UnknownProvider(other.to_string())),
        }
    }
}

impl ConfigKnowledgeProviders {
    fn parser_webhook_verifier(
        &self,
        provider: &'static str,
    ) -> Result<Arc<dyn KnowledgeWebhookVerifier>, KnowledgeServiceError> {
        if !self
            .config
            .parsers
            .enabled
            .iter()
            .any(|candidate| candidate == provider)
        {
            return Err(KnowledgeServiceError::UnknownProvider(provider.to_string()));
        }
        let (signing_key, header_name, header_value) = match provider {
            "llamaparse" => (
                moa_config::optional_config_secret(&self.config.llamaparse.webhook_signing_key),
                self.config.llamaparse.webhook_header_name.clone(),
                self.config.llamaparse.webhook_header_value.clone(),
            ),
            "reducto" => (
                moa_config::optional_config_secret(&self.config.reducto.webhook_signing_key),
                self.config.reducto.webhook_header_name.clone(),
                self.config.reducto.webhook_header_value.clone(),
            ),
            other => return Err(KnowledgeServiceError::UnknownProvider(other.to_string())),
        };
        let mut verifier = ParserWebhookVerifier::new(provider);
        if let Some(signing_key) = signing_key {
            verifier = verifier.with_signing_key(signing_key);
        }
        match (header_name, header_value) {
            (Some(name), Some(value)) => {
                verifier = verifier.with_custom_header(name, value);
            }
            (None, None) => {}
            _ => {
                return Err(MoaError::ConfigError(format!(
                    "knowledge parser `{provider}` webhook custom header requires both name and value"
                ))
                .into());
            }
        }
        Ok(Arc::new(verifier))
    }
}

/// Errors emitted by knowledge service application logic.
#[derive(Debug, thiserror::Error)]
pub enum KnowledgeServiceError {
    /// Requested provider is not configured.
    #[error("knowledge provider `{0}` is not configured")]
    UnknownProvider(String),
    /// Requested row was not found.
    #[error("{0} not found")]
    NotFound(&'static str),
    /// Request or provider payload was invalid.
    #[error("invalid knowledge request: {0}")]
    InvalidRequest(String),
    /// Credential storage failed.
    #[error("knowledge credential store failed: {0}")]
    Credential(String),
    /// Generic connector parent lifecycle failed.
    #[error(transparent)]
    Connector(#[from] moa_connectors::Error),
    /// Knowledge crate operation failed.
    #[error(transparent)]
    Knowledge(#[from] moa_knowledge::Error),
    /// MOA runtime operation failed.
    #[error(transparent)]
    Moa(#[from] MoaError),
}

async fn authorize_tenant(
    authz: &crate::handlers::authz_shim::AuthzEnforcer,
    ctx: &Context<'_>,
    tenant_id: TenantId,
) -> Result<Identity, HandlerError> {
    authz
        .authorize_tenant(ctx, tenant_id, Relation::Operator)
        .await
}

/// Authorizes the caller for one tenant and binds a replay-stable operation id.
///
/// Authorization happens before any credential work, and the id comes from the
/// invocation's deterministic RNG rather than `Uuid::now_v7`, so a replayed
/// handler reuses the same credential audit rows instead of appending new ones.
async fn authorize_knowledge_caller(
    authz: &crate::handlers::authz_shim::AuthzEnforcer,
    ctx: &mut Context<'_>,
    tenant_id: TenantId,
) -> Result<KnowledgeCaller, HandlerError> {
    let identity = authorize_tenant(authz, &*ctx, tenant_id).await?;
    Ok(KnowledgeCaller::authorized(
        &identity,
        ctx.rand_uuid().to_string(),
    ))
}

fn knowledge_handler_error(error: KnowledgeServiceError) -> HandlerError {
    match error {
        KnowledgeServiceError::Moa(error) => moa_error_to_status_handler_error(error),
        KnowledgeServiceError::Connector(moa_connectors::Error::DatabaseScope(error)) => {
            moa_error_to_status_handler_error(error)
        }
        KnowledgeServiceError::Connector(moa_connectors::Error::Authorization(error)) => {
            crate::workflows::errors::authz_error_to_handler_error(error)
        }
        KnowledgeServiceError::Connector(moa_connectors::Error::Storage(error)) => {
            crate::workflows::errors::sqlx_error_to_handler_error(error)
        }
        other => match terminal_knowledge_error_code(&other) {
            Some(code) => TerminalError::new_with_code(code, other.to_string()).into(),
            None => HandlerError::from(other),
        },
    }
}

fn should_dispatch_knowledge_sync_ingestion(status: &str) -> bool {
    status == "provider_synced"
}

fn terminal_knowledge_error_code(error: &KnowledgeServiceError) -> Option<u16> {
    match error {
        KnowledgeServiceError::UnknownProvider(_) | KnowledgeServiceError::InvalidRequest(_) => {
            Some(400)
        }
        KnowledgeServiceError::NotFound(_) => Some(404),
        KnowledgeServiceError::Knowledge(moa_knowledge::Error::Config(_))
        | KnowledgeServiceError::Knowledge(moa_knowledge::Error::UnsupportedFormat(_))
        | KnowledgeServiceError::Moa(MoaError::MissingEnvironmentVariable(_))
        | KnowledgeServiceError::Moa(MoaError::ConfigError(_))
        | KnowledgeServiceError::Moa(MoaError::ValidationError(_))
        | KnowledgeServiceError::Moa(MoaError::PermissionDenied(_))
        | KnowledgeServiceError::Moa(MoaError::BudgetExhausted(_))
        | KnowledgeServiceError::Moa(MoaError::Cancelled) => Some(400),
        KnowledgeServiceError::Moa(MoaError::SessionNotFound(_))
        | KnowledgeServiceError::Moa(MoaError::BlobNotFound(_)) => Some(404),
        KnowledgeServiceError::Connector(error) => connector_terminal_error_code(error),
        KnowledgeServiceError::Credential(_)
        | KnowledgeServiceError::Knowledge(_)
        | KnowledgeServiceError::Moa(_) => None,
    }
}

fn connector_terminal_error_code(error: &moa_connectors::Error) -> Option<u16> {
    use moa_connectors::Error;

    match error {
        Error::ConnectionNotFound { .. } => Some(404),
        Error::AuthorizationDenied => Some(403),
        Error::GenerationConflict { .. }
        | Error::ManagedParentClaimConflict { .. }
        | Error::InvocationConflict { .. }
        | Error::InvocationStateConflict { .. }
        | Error::ManualReconciliationRequired { .. }
        | Error::InvocationUnavailable { .. } => Some(409),
        Error::InvalidConnectionOrigin { .. }
        | Error::InvalidGeneration { .. }
        | Error::GenerationExhausted
        | Error::InvalidTransition { .. }
        | Error::ManagedParentOwnerRequired { .. }
        | Error::ManagedParentMismatch { .. }
        | Error::UnsupportedManagedKnowledgeProvider
        | Error::ManagedParentActionDependents { .. }
        | Error::UseGrantConnectionUnavailable { .. }
        | Error::UseGrantSubjectNotFound { .. }
        | Error::UseGrantSubjectInactive { .. }
        | Error::InvalidContract { .. }
        | Error::CatalogInvariant { .. }
        | Error::ContractHashMismatch { .. }
        | Error::CredentialSlotMissing { .. }
        | Error::ActionPinMismatch { .. }
        | Error::SchemaValidation { .. } => Some(400),
        Error::Cancelled { .. } | Error::Credential(_) | Error::Serialization(_) => Some(400),
        Error::Http { .. }
        | Error::DatabaseScope(_)
        | Error::Authorization(_)
        | Error::AuthorizationUnavailable
        | Error::Storage(_) => None,
    }
}
