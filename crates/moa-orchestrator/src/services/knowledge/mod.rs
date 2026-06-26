//! Restate service for tenant knowledge-base link, sync, webhook, and inspection APIs.

mod inspect;
mod link;
mod sync;
mod webhook;
mod webhook_verifier;

pub use webhook_verifier::{KnowledgeWebhookVerifier, ParserWebhookVerifier};

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use moa_authz::require_authz_with_delegation;
use moa_authz_schema::{ObjectType, Relation};
use moa_core::{
    MoaConfig, MoaError, TenantId,
    wire::knowledge::{
        KnowledgeConnectionListRequest, KnowledgeConnectionListResponse,
        KnowledgeCreateLinkTokenRequest, KnowledgeCreateLinkTokenResponse,
        KnowledgeExchangeTokenRequest, KnowledgeExchangeTokenResponse,
        KnowledgeObjectInspectRequest, KnowledgeObjectInspectResponse, KnowledgeObjectListRequest,
        KnowledgeObjectListResponse, KnowledgeProviderWebhookRequest,
        KnowledgeProviderWebhookResponse, KnowledgeQueryTraceRequest, KnowledgeQueryTraceResponse,
        KnowledgeSyncEventsRequest, KnowledgeSyncEventsResponse, KnowledgeSyncRequest,
        KnowledgeSyncResponse, KnowledgeSyncStatusRequest, KnowledgeSyncStatusResponse,
    },
};
use moa_knowledge::{
    domain::LinkedAccount,
    providers::{LinkedIntegrationProvider, merge::MergeProvider, nango::NangoProvider},
    repository::{KnowledgeRepository, PostgresKnowledgeRepository},
};
use moa_memory_types::ScopeContext;
use moa_observability::restate_observability::annotate_restate_handler_span;
use restate_sdk::prelude::*;

use crate::{
    OrchestratorCtx,
    ctx::RequestHeaders,
    handlers::authz_shim::{require_fga_client, require_identity, translate_authz_error},
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
#[derive(Clone, Default)]
pub struct KnowledgeImpl;

impl Knowledge for KnowledgeImpl {
    #[tracing::instrument(skip(self, ctx, request))]
    async fn create_link_token(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeCreateLinkTokenRequest>,
    ) -> Result<Json<KnowledgeCreateLinkTokenResponse>, HandlerError> {
        annotate_restate_handler_span("Knowledge", "create_link_token");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
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
        ctx: Context<'_>,
        request: Json<KnowledgeExchangeTokenRequest>,
    ) -> Result<Json<KnowledgeExchangeTokenResponse>, HandlerError> {
        annotate_restate_handler_span("Knowledge", "exchange_public_token");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
        Ok(ctx
            .run(|| async move {
                service
                    .exchange_public_token(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_exchange_public_token")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn sync_connection(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeSyncRequest>,
    ) -> Result<Json<KnowledgeSyncResponse>, HandlerError> {
        annotate_restate_handler_span("Knowledge", "sync_connection");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
        Ok(ctx
            .run(|| async move {
                service
                    .sync_connection(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_sync_connection")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn sync_status(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeSyncStatusRequest>,
    ) -> Result<Json<KnowledgeSyncStatusResponse>, HandlerError> {
        annotate_restate_handler_span("Knowledge", "sync_status");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
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
        annotate_restate_handler_span("Knowledge", "sync_events");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
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
        ctx: Context<'_>,
        request: Json<KnowledgeConnectionListRequest>,
    ) -> Result<Json<KnowledgeConnectionListResponse>, HandlerError> {
        annotate_restate_handler_span("Knowledge", "list_connections");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
        Ok(ctx
            .run(|| async move {
                service
                    .list_connections(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_list_connections")
            .await?)
    }

    #[tracing::instrument(skip(self, ctx, request))]
    async fn list_objects(
        &self,
        ctx: Context<'_>,
        request: Json<KnowledgeObjectListRequest>,
    ) -> Result<Json<KnowledgeObjectListResponse>, HandlerError> {
        annotate_restate_handler_span("Knowledge", "list_objects");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
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
        annotate_restate_handler_span("Knowledge", "inspect_object");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
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
        annotate_restate_handler_span("Knowledge", "query_trace");
        let request = request.into_inner();
        authorize_tenant(&ctx, request.tenant_id).await?;
        let service = production_service(request.tenant_id);
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
        annotate_restate_handler_span("Knowledge", "provider_webhook");
        let request = request.into_inner();
        let service = production_service_for_webhook();
        Ok(ctx
            .run(|| async move {
                service
                    .provider_webhook(request)
                    .await
                    .map(Json::from)
                    .map_err(knowledge_handler_error)
            })
            .name("knowledge_provider_webhook")
            .await?)
    }
}

/// Application logic for the Knowledge Restate service.
#[derive(Clone)]
pub struct KnowledgeService {
    repository: KnowledgeRepositorySource,
    providers: Arc<dyn KnowledgeProviderResolver>,
    credentials: Arc<dyn KnowledgeCredentialStore>,
    max_preview_chars: usize,
}

impl KnowledgeService {
    /// Creates a knowledge service with explicit dependencies for tests or alternate runtimes.
    #[must_use]
    pub fn new(
        repository: Arc<dyn KnowledgeRepository>,
        providers: Arc<dyn KnowledgeProviderResolver>,
        credentials: Arc<dyn KnowledgeCredentialStore>,
        max_preview_chars: usize,
    ) -> Self {
        Self {
            repository: KnowledgeRepositorySource::Fixed(repository),
            providers,
            credentials,
            max_preview_chars,
        }
    }

    /// Creates a knowledge service backed by tenant-scoped Postgres repositories.
    #[must_use]
    pub fn from_postgres_pool(
        pool: sqlx::PgPool,
        providers: Arc<dyn KnowledgeProviderResolver>,
        credentials: Arc<dyn KnowledgeCredentialStore>,
        max_preview_chars: usize,
    ) -> Self {
        Self {
            repository: KnowledgeRepositorySource::Postgres { pool },
            providers,
            credentials,
            max_preview_chars,
        }
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
                ScopeContext::tenant(tenant_id),
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

/// Stores linked-account credentials and returns the reference persisted on connections.
#[async_trait]
pub trait KnowledgeCredentialStore: Send + Sync {
    /// Stores credential material for a linked account and returns a persisted reference.
    async fn store_linked_account(
        &self,
        tenant_id: TenantId,
        account: &LinkedAccount,
    ) -> Result<String, KnowledgeServiceError>;
}

/// Credential store that emits deterministic service-owned references without persisting secrets.
#[derive(Debug, Clone, Default)]
pub struct DeterministicKnowledgeCredentialStore;

#[async_trait]
impl KnowledgeCredentialStore for DeterministicKnowledgeCredentialStore {
    async fn store_linked_account(
        &self,
        tenant_id: TenantId,
        account: &LinkedAccount,
    ) -> Result<String, KnowledgeServiceError> {
        Ok(format!(
            "vault://tenant/{tenant_id}/knowledge/{}/{}",
            account.provider, account.provider_account_id
        ))
    }
}

#[derive(Clone)]
struct ConfigKnowledgeProviders {
    config: moa_core::config::KnowledgeConfig,
}

impl ConfigKnowledgeProviders {
    fn new(config: moa_core::config::KnowledgeConfig) -> Self {
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
                if let Some(signing_key) = self.config.nango.webhook_signing_key.clone() {
                    implementation = implementation.with_webhook_signing_key(signing_key);
                }
                Ok(Arc::new(implementation))
            }
            "merge" => {
                let api_key = self.config.selected_provider_api_key(provider)?;
                let mut implementation =
                    MergeProvider::new(self.config.merge.api_base_url.clone(), api_key)?;
                if let Some(signature_key) = self.config.merge.webhook_signature_key.clone() {
                    implementation = implementation.with_webhook_signature_key(signature_key);
                }
                Ok(Arc::new(implementation))
            }
            other => Err(KnowledgeServiceError::UnknownProvider(other.to_string())),
        }
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
                self.config.llamaparse.webhook_signing_key.clone(),
                self.config.llamaparse.webhook_header_name.clone(),
                self.config.llamaparse.webhook_header_value.clone(),
            ),
            "reducto" => (
                self.config.reducto.webhook_signing_key.clone(),
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

fn production_service(_tenant_id: TenantId) -> KnowledgeService {
    let config = OrchestratorCtx::current_config();
    service_from_config(OrchestratorCtx::current_graph_pool(), &config)
}

fn production_service_for_webhook() -> KnowledgeService {
    let config = OrchestratorCtx::current_config();
    service_from_config(OrchestratorCtx::current_graph_pool(), &config)
}

fn service_from_config(pool: sqlx::PgPool, config: &MoaConfig) -> KnowledgeService {
    KnowledgeService::from_postgres_pool(
        pool,
        Arc::new(ConfigKnowledgeProviders::new(config.knowledge.clone())),
        Arc::new(DeterministicKnowledgeCredentialStore),
        config.knowledge.observability.max_object_preview_chars,
    )
}

async fn authorize_tenant(
    ctx: &impl RequestHeaders,
    tenant_id: TenantId,
) -> Result<(), HandlerError> {
    let identity = require_identity(ctx)?;
    let fga = require_fga_client()?;
    require_authz_with_delegation(
        &fga,
        &identity,
        ObjectType::Tenant,
        tenant_id,
        Relation::Operator,
    )
    .await
    .map_err(translate_authz_error)
}

fn knowledge_handler_error(error: impl std::fmt::Display) -> HandlerError {
    TerminalError::new(error.to_string()).into()
}
