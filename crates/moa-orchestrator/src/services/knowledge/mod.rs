//! Restate service for tenant knowledge-base link, sync, webhook, and inspection APIs.

mod ingest;
mod inspect;
mod link;
mod sync;
mod webhook;
mod webhook_verifier;

pub use ingest::{KnowledgeIngestionRunner, ProductionKnowledgeIngestionRunner};
pub use webhook_verifier::{KnowledgeWebhookVerifier, ParserWebhookVerifier};

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use moa_authz_schema::Relation;
use moa_config::MoaConfig;
use moa_core::types::credentials::{
    CredentialContext, CredentialError, CredentialIdentity, CredentialKind, CredentialOperation,
    CredentialPrincipal, CredentialRef, CredentialServiceActor, CredentialSource, RedactedSecret,
};
use moa_core::types::memory::RlsContext;
use moa_core::{
    error::MoaError,
    traits::{CredentialVault, Identity, RuntimeCacheStore},
    types::identifiers::TenantId,
};
use moa_knowledge::{
    domain::{KnowledgeConnection, LinkedAccount},
    providers::{LinkedIntegrationProvider, merge::MergeProvider, nango::NangoProvider},
    repository::{
        KnowledgeDiscoveryStore, KnowledgeRepository, PostgresKnowledgeDiscoveryStore,
        PostgresKnowledgeRepository,
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

use crate::ctx::RequestHeaders;
use crate::workflows::knowledge_sync_ingestion::{
    KnowledgeSyncIngestionClient, KnowledgeSyncIngestionRequest,
};

use self::webhook_verifier::LinkedProviderWebhookVerifier;

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
        authorize_tenant(&ctx, request.tenant_id).await?;
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
        let caller = authorize_knowledge_caller(&mut ctx, request.tenant_id).await?;
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
        let caller = authorize_knowledge_caller(&mut ctx, request.tenant_id).await?;
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
        authorize_tenant(&ctx, request.tenant_id).await?;
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
        authorize_tenant(&ctx, request.tenant_id).await?;
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
        let caller = authorize_knowledge_caller(&mut ctx, request.tenant_id).await?;
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
        authorize_tenant(&ctx, request.tenant_id).await?;
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
        let caller = authorize_knowledge_caller(&mut ctx, request.tenant_id).await?;
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
        let caller = authorize_knowledge_caller(&mut ctx, request.tenant_id).await?;
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
        authorize_tenant(&ctx, request.tenant_id).await?;
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
        authorize_tenant(&ctx, request.tenant_id).await?;
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
        authorize_tenant(&ctx, request.tenant_id).await?;
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
    pub fn new(service: KnowledgeService) -> Self {
        Self { service }
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
    ingestion_runner: Arc<dyn KnowledgeIngestionRunner>,
    max_preview_chars: usize,
    lineage_clickhouse: Option<Arc<moa_lineage_sink::ClickHouseStore>>,
}

impl KnowledgeService {
    /// Creates a knowledge service with explicit dependencies for tests or alternate runtimes.
    #[must_use]
    pub fn new(
        repository: Arc<dyn KnowledgeRepository>,
        discovery: Arc<dyn KnowledgeDiscoveryStore>,
        providers: Arc<dyn KnowledgeProviderResolver>,
        credentials: Arc<dyn KnowledgeCredentialStore>,
        ingestion_runner: Arc<dyn KnowledgeIngestionRunner>,
        max_preview_chars: usize,
    ) -> Self {
        Self {
            repository: KnowledgeRepositorySource::Fixed(repository),
            discovery,
            providers,
            credentials,
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
    ) -> Self {
        Self::from_postgres_pool(
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
        provider: &str,
    ) -> Result<Arc<dyn LinkedIntegrationProvider>, KnowledgeServiceError> {
        self.providers.provider(provider)
    }

    fn webhook_verifier(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn KnowledgeWebhookVerifier>, KnowledgeServiceError> {
        self.providers.webhook_verifier(provider)
    }

    fn repository(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeRepository> {
        self.repository.repository(tenant_id)
    }

    fn discovery(&self) -> &dyn KnowledgeDiscoveryStore {
        self.discovery.as_ref()
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
    ) -> Result<RedactedSecret, KnowledgeServiceError> {
        self.credentials
            .resolve_linked_account(connection.tenant_id, connection, caller)
            .await
    }

    fn postgres_pool(&self) -> Option<sqlx::PgPool> {
        match &self.repository {
            KnowledgeRepositorySource::Fixed(_) => None,
            KnowledgeRepositorySource::Postgres { pool } => Some(pool.clone()),
        }
    }
}

#[derive(Clone)]
enum KnowledgeRepositorySource {
    Fixed(Arc<dyn KnowledgeRepository>),
    Postgres { pool: sqlx::PgPool },
}

impl KnowledgeRepositorySource {
    fn repository(&self, tenant_id: TenantId) -> Arc<dyn KnowledgeRepository> {
        match self {
            Self::Fixed(repository) => repository.clone(),
            Self::Postgres { pool } => Arc::new(PostgresKnowledgeRepository::scoped(
                pool.clone(),
                RlsContext::tenant(tenant_id),
            )),
        }
    }
}

/// Resolves linked-integration providers by stable provider identifier.
pub trait KnowledgeProviderResolver: Send + Sync {
    /// Returns the provider implementation for a selected provider identifier.
    fn provider(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn LinkedIntegrationProvider>, KnowledgeServiceError>;

    /// Returns the resolvable provider identifiers in deterministic order.
    fn provider_ids(&self) -> Vec<String>;

    /// Returns the webhook verifier for a selected provider identifier.
    fn webhook_verifier(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn KnowledgeWebhookVerifier>, KnowledgeServiceError> {
        Ok(Arc::new(LinkedProviderWebhookVerifier::new(
            self.provider(provider)?,
        )))
    }
}

/// Static provider resolver used by offline service tests.
#[derive(Clone, Default)]
pub struct StaticKnowledgeProviders {
    providers: HashMap<String, Arc<dyn LinkedIntegrationProvider>>,
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
        provider: impl Into<String>,
        implementation: Arc<dyn LinkedIntegrationProvider>,
    ) -> Self {
        self.providers.insert(provider.into(), implementation);
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
        provider: &str,
    ) -> Result<Arc<dyn LinkedIntegrationProvider>, KnowledgeServiceError> {
        self.providers
            .get(provider)
            .cloned()
            .ok_or_else(|| KnowledgeServiceError::UnknownProvider(provider.to_string()))
    }

    fn provider_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.providers.keys().cloned().collect();
        ids.sort();
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
            self.provider(provider)?,
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

/// Stores linked-account credentials and returns the reference persisted on connections.
#[async_trait]
pub trait KnowledgeCredentialStore: Send + Sync {
    /// Stores the first credential version for one newly linked connection.
    ///
    /// `connection_uid` is generated before the credential is written so the
    /// stored version is bound to its owning connection from the start and can
    /// never be adopted by a different one.
    async fn store_linked_account(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        caller: &KnowledgeCaller,
        account: &LinkedAccount,
    ) -> Result<String, KnowledgeServiceError>;

    /// Resolves one connection's credential immediately before a provider request.
    ///
    /// The plaintext is returned as a non-serializable [`RedactedSecret`] so it
    /// cannot be cloned into a connection, an event, Restate state, or a model
    /// payload on the way to the provider.
    async fn resolve_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<RedactedSecret, KnowledgeServiceError>;

    /// Deletes MOA-managed credentials for a linked account.
    async fn delete_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<bool, KnowledgeServiceError>;

    /// Revokes exactly one credential version, leaving its audit history intact.
    ///
    /// Used by link compensation to undo a candidate it wrote, so a failed
    /// re-link cannot leave usable material behind.
    async fn revoke_credential(
        &self,
        tenant_id: TenantId,
        reference: &str,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError>;

    /// Reports whether one connection's MOA-managed credential is still usable.
    ///
    /// Returns `None` for a provider-native handle that MOA does not store. This
    /// deliberately takes one exact connection rather than listing a tenant's
    /// credentials: there is no enumeration surface to authorize.
    async fn credential_status(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<Option<String>, KnowledgeServiceError>;
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
    async fn store_linked_account(
        &self,
        tenant_id: TenantId,
        connection_uid: Uuid,
        caller: &KnowledgeCaller,
        account: &LinkedAccount,
    ) -> Result<String, KnowledgeServiceError> {
        let Some(material) = account.credential_material.as_deref() else {
            // A provider-native handle: the provider keeps the material, so MOA
            // stores nothing and the connection carries the provider's own
            // opaque reference unchanged.
            return Ok(account.credential_ref.clone());
        };
        let identity = CredentialIdentity {
            tenant_id,
            connection_uid,
            kind: CredentialKind::ProviderApiKey,
        };
        let version = self
            .vault
            .create(
                identity,
                SecretString::from(material.to_string()),
                &Self::context(
                    tenant_id,
                    caller.principal(),
                    CredentialOperation::Create,
                    &caller.step("credential-create"),
                    &[
                        &connection_uid.to_string(),
                        CredentialKind::ProviderApiKey.as_str(),
                        &account.provider,
                        &account.provider_account_id,
                    ],
                ),
            )
            .await
            .map_err(credential_error)?;
        Ok(version.reference.to_string())
    }

    async fn resolve_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<RedactedSecret, KnowledgeServiceError> {
        let Some(reference) = managed_credential_reference(&connection.credential_ref) else {
            // Provider-native handle: hand the provider back its own reference
            // through the same redacted carrier so no call site can tell the two
            // apart and accidentally log one of them.
            return Ok(RedactedSecret::new(connection.credential_ref.clone()));
        };
        if connection.tenant_id != tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }
        self.vault
            .resolve(
                &CredentialSource::TenantConnection { reference },
                &Self::context(
                    tenant_id,
                    caller.principal(),
                    CredentialOperation::Resolve,
                    &caller.step("credential-resolve"),
                    &[
                        &connection.connection_uid.to_string(),
                        &reference.to_string(),
                    ],
                ),
            )
            .await
            .map_err(credential_error)
    }

    async fn delete_linked_account(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<bool, KnowledgeServiceError> {
        if managed_credential_reference(&connection.credential_ref).is_none() {
            return Ok(false);
        }
        if connection.tenant_id != tenant_id {
            return Err(KnowledgeServiceError::NotFound("knowledge connection"));
        }
        let removed = self
            .vault
            .delete_connection(
                connection.connection_uid,
                &Self::context(
                    tenant_id,
                    caller.principal(),
                    CredentialOperation::Delete,
                    &caller.step("credential-delete"),
                    &[&connection.connection_uid.to_string()],
                ),
            )
            .await
            .map_err(credential_error)?;
        Ok(removed > 0)
    }

    async fn revoke_credential(
        &self,
        tenant_id: TenantId,
        reference: &str,
        caller: &KnowledgeCaller,
    ) -> Result<(), KnowledgeServiceError> {
        let Some(reference) = managed_credential_reference(reference) else {
            // A provider-native handle was never written by MOA, so there is
            // nothing for compensation to undo.
            return Ok(());
        };
        self.vault
            .revoke(
                reference,
                &Self::context(
                    tenant_id,
                    caller.principal(),
                    CredentialOperation::Revoke,
                    &caller.step("credential-revoke"),
                    &[&reference.to_string()],
                ),
            )
            .await
            .map_err(credential_error)
    }

    async fn credential_status(
        &self,
        tenant_id: TenantId,
        connection: &KnowledgeConnection,
        caller: &KnowledgeCaller,
    ) -> Result<Option<String>, KnowledgeServiceError> {
        let Some(reference) = managed_credential_reference(&connection.credential_ref) else {
            return Ok(None);
        };
        // Status is a metadata read for a reference this tenant already holds:
        // no material is opened and no listing is performed.
        let ctx = Self::context(
            tenant_id,
            caller.principal(),
            CredentialOperation::Resolve,
            &caller.step(&format!("credential-status-{}", connection.connection_uid)),
            &[&connection.connection_uid.to_string()],
        );
        match self.vault.describe(reference, &ctx).await {
            Ok(version) if version.revoked => Ok(Some("revoked".to_string())),
            Ok(version) if !version.active => Ok(Some("superseded".to_string())),
            Ok(_) => Ok(Some("present".to_string())),
            Err(CredentialError::NotFound) => Ok(Some("missing".to_string())),
            Err(error) => Err(credential_error(error)),
        }
    }
}

/// Parses a connection reference that MOA stores, ignoring provider-native handles.
///
/// MOA-managed references are exactly the opaque credential-version identifiers
/// the vault issues. Anything else is a provider's own handle, which MOA passes
/// through without ever treating it as addressable vault storage.
fn managed_credential_reference(value: &str) -> Option<CredentialRef> {
    Uuid::parse_str(value).ok().map(CredentialRef::from_uuid)
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
        provider: &str,
    ) -> Result<Arc<dyn LinkedIntegrationProvider>, KnowledgeServiceError> {
        match provider {
            "nango" => {
                let api_key = self.config.selected_provider_api_key(provider)?;
                let mut implementation =
                    NangoProvider::new(self.config.nango.api_base_url.clone(), api_key)?;
                if let Some(signing_key) =
                    moa_config::optional_config_secret(&self.config.nango.webhook_signing_key)
                {
                    implementation = implementation.with_webhook_signing_key(signing_key);
                }
                Ok(Arc::new(implementation))
            }
            "merge" => {
                let api_key = self.config.selected_provider_api_key(provider)?;
                let mut implementation =
                    MergeProvider::new(self.config.merge.api_base_url.clone(), api_key)?;
                if let Some(signature_key) =
                    moa_config::optional_config_secret(&self.config.merge.webhook_signature_key)
                {
                    implementation = implementation.with_webhook_signature_key(signature_key);
                }
                Ok(Arc::new(implementation))
            }
            other => Err(KnowledgeServiceError::UnknownProvider(other.to_string())),
        }
    }

    fn provider_ids(&self) -> Vec<String> {
        let mut ids = self.config.providers.enabled.clone();
        ids.sort();
        ids
    }

    fn webhook_verifier(
        &self,
        provider: &str,
    ) -> Result<Arc<dyn KnowledgeWebhookVerifier>, KnowledgeServiceError> {
        match provider {
            "nango" | "merge" => Ok(Arc::new(LinkedProviderWebhookVerifier::new(
                self.provider(provider)?,
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
    /// Knowledge crate operation failed.
    #[error(transparent)]
    Knowledge(#[from] moa_knowledge::Error),
    /// MOA runtime operation failed.
    #[error(transparent)]
    Moa(#[from] MoaError),
}

async fn authorize_tenant(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
) -> Result<Identity, HandlerError> {
    crate::handlers::authz_shim::authorize_tenant(ctx, tenant_id, Relation::Operator).await
}

/// Authorizes the caller for one tenant and binds a replay-stable operation id.
///
/// Authorization happens before any credential work, and the id comes from the
/// invocation's deterministic RNG rather than `Uuid::now_v7`, so a replayed
/// handler reuses the same credential audit rows instead of appending new ones.
async fn authorize_knowledge_caller(
    ctx: &mut Context<'_>,
    tenant_id: TenantId,
) -> Result<KnowledgeCaller, HandlerError> {
    let identity = authorize_tenant(&*ctx, tenant_id).await?;
    Ok(KnowledgeCaller::authorized(
        &identity,
        ctx.rand_uuid().to_string(),
    ))
}

fn knowledge_handler_error(error: KnowledgeServiceError) -> HandlerError {
    match terminal_knowledge_error_code(&error) {
        Some(code) => TerminalError::new_with_code(code, error.to_string()).into(),
        None => HandlerError::from(error),
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
        KnowledgeServiceError::Credential(_)
        | KnowledgeServiceError::Knowledge(_)
        | KnowledgeServiceError::Moa(_) => None,
    }
}
