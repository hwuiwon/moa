//! Dependency construction for orchestrator runtime startup.

use std::collections::HashMap;
use std::sync::Arc;

use crate::connector_catalog::ScopedConnectorCatalogProvider;
use crate::credential_ingress::{
    ConnectorCredentialIngress, CredentialIngressCoordinator,
    ManagementCredentialIngressCoordinator,
};
use crate::ctx::{
    AuthDeps, LineageDeps, MemoryDeps, OrchestratorCtx, OrchestratorDeps, PersistenceDeps,
    ProviderDeps, ToolDeps,
};
use anyhow::{Context as AnyhowContext, Result, bail};
use async_trait::async_trait;
use moa_authz::{AwakeableResolver, FgaClient};
use moa_brain::{
    build_graph_memory_retriever,
    pipeline::{memory::GraphMemoryRetriever, skills::SkillInjector},
};
use moa_config::MoaConfig;
use moa_connectors::catalog::{
    FgaConnectorUseAuthorizer, GovernedInstalledConnectorCatalog, InstalledConnectorCatalog,
    InstalledConnectorCatalogSource,
};
use moa_connectors::executor::{
    ConnectorActionRuntime, ConnectorInvocationCompletionService, ConnectorInvocationCoordinator,
};
use moa_connectors::http::HttpConnectorRuntime;
use moa_connectors::repository::ConnectionUseGrantRepository;
use moa_connectors::repository::{ConnectionRepository, PostgresConnectionRepository};
use moa_connectors::service::{ConnectorService, CredentialSlotVerifier};
use moa_core::{
    traits::{ChannelAdapter, EmbeddingProvider, RuntimeCacheStore},
    types::channel::Channel,
};
use moa_hands::{
    PostgresTenantSandboxPolicyStore, ToolRouter, core::leases::PostgresHandLeaseStore,
};
use moa_memory_pii::{HeuristicPiiClassifier, OpenAiPrivacyFilterClassifier, PiiClassifier};
use moa_providers::{
    EmbedderConstructionRole, ProviderRegistry, build_embedder_from_config,
    build_embedding_provider_from_config,
};
#[cfg(feature = "integration")]
use moa_security::outbound_http::TokioOutboundHostResolver;
use moa_security::{McpEgressGuard, OutboundHttpPolicy};
use moa_session::PostgresSessionStore;
use sqlx::PgPool;

use crate::services::{
    authz_challenges_reaper::HttpAwakeableResolver,
    connectors::{
        ArtifactConnectorDefinitionResolver, ConnectionCredentialRevoker,
        ConnectorDefinitionResolver, ConnectorDestinationVerifier,
        ConnectorManagementAuthorizationError, ConnectorManagementAuthorizer,
        ConnectorManagementService, FgaConnectorManagementAuthorizer,
        ManagedKnowledgeConnectorDefinitionResolver, PolicyConnectorDestinationVerifier,
        VaultConnectionCredentialRevoker, VaultCredentialSlotVerifier,
    },
    scim::ScimState,
};
use moa_artifacts::registry::ArtifactRegistry;

use crate::{
    config::ProvidersOverride,
    lineage::{LineageSinkRuntime, build_lineage_sink},
    runtime::{
        jobs::{restate_ingress_base_url, start_authz_outbox_poller},
        kms::KmsRuntime,
    },
};

/// Constructed dependencies shared by Restate handlers and process services.
pub struct RuntimeDeps {
    /// Shared orchestrator configuration.
    pub config: Arc<MoaConfig>,
    /// Runtime Postgres pool.
    pub pool: PgPool,
    /// Postgres pool reserved for process-owned background workers.
    pub background_pool: PgPool,
    /// Session and analytics store backed by Postgres.
    pub session_store: Arc<PostgresSessionStore>,
    /// Optional OpenFGA authorization client.
    pub fga_client: Option<FgaClient>,
    /// Optional issuer used by contact-facing authentication flows.
    pub contact_token_issuer: Option<Arc<moa_auth_providers::ContactTokenIssuer>>,
    /// Configured third-party token vault backed by the shared runtime KMS.
    pub token_vault_provider: Arc<dyn moa_core::traits::TokenVaultProvider>,
    /// The process's single durable tenant credential owner.
    ///
    /// Constructed once and shared by every service and workflow that resolves
    /// connector material, so a credential written on one replica resolves on
    /// every other and a reconstructed workflow reads the same durable state.
    pub credential_vault: Arc<dyn moa_core::traits::CredentialVault>,
    /// Explicit key-management handle shared by runtime owners and readiness.
    pub kms: KmsRuntime,
    /// Runtime cache used for ephemeral coordination state.
    pub runtime_cache: Arc<dyn RuntimeCacheStore>,
    /// Configured LLM provider registry.
    pub providers: Arc<ProviderRegistry>,
    /// Optional embedding provider.
    pub embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Tool router used by ToolExecutor and runtime services.
    pub tool_router: Arc<ToolRouter>,
    /// Authenticated ephemeral catalog provider for tenant-installed actions.
    pub(crate) connector_catalogs: ScopedConnectorCatalogProvider,
    /// Post-journal connector invocation completion boundary.
    pub(crate) connector_completion: ConnectorInvocationCompletionService,
    /// Shared connector lifecycle service used by management and knowledge capabilities.
    pub(crate) connector_connections: ConnectorService,
    /// Secret-free connector management application service shared by Restate and ingress.
    pub(crate) connector_management: ConnectorManagementService,
    /// Plaintext-owning private connector credential ingress controller.
    pub(crate) connector_credential_ingress: ConnectorCredentialIngress,
    /// Graph-memory retriever used by the context pipeline.
    pub graph_memory_retriever: Arc<GraphMemoryRetriever>,
    /// Skill injector used by the context pipeline.
    pub skill_injector: Arc<SkillInjector>,
    /// Selected lineage sink and optional writer.
    pub lineage: LineageSinkRuntime,
    /// Awakeable resolver used by builtin async authorization.
    pub awakeable_resolver: Arc<dyn AwakeableResolver>,
    /// Optional OpenFGA outbox poller handle.
    pub authz_outbox_poller: Option<moa_authz::PollerHandle>,
    /// Owned security-audit writer.
    ///
    /// Held so shutdown can drain it. Dropping these deps instead aborts it,
    /// which is the correct outcome for a process that is going away without a
    /// graceful path — and is visible, unlike a detached global task.
    pub audit: Arc<moa_ocsf::AuditRuntime>,
    /// Configured live outbound channel adapters.
    pub channel_adapters: HashMap<Channel, Arc<dyn ChannelAdapter>>,
}

impl RuntimeDeps {
    /// Builds all runtime dependencies from configuration and the runtime pool.
    pub async fn build(
        config: Arc<MoaConfig>,
        pool: PgPool,
        background_pool: PgPool,
        restate_ingress_url: &str,
        providers_override: ProvidersOverride,
        skip_fga: bool,
    ) -> Result<Self> {
        // The audit writer is instance-owned and started before anything that can
        // produce an event. Startup fails outright if it cannot start: the
        // previous global initializer logged a warning and left every audit event
        // for the process lifetime as a silent counted drop.
        let audit = moa_ocsf::AuditRuntime::start(pool.clone())
            .context("start orchestrator security audit writer")?;
        let runtime_cache = build_runtime_cache_store(config.as_ref()).await?;
        let kms = KmsRuntime::build_serving(config.as_ref(), pool.clone()).await?;
        let fga_client = if skip_fga {
            tracing::warn!("MOA_SKIP_FGA set; authz outbox poller disabled");
            None
        } else {
            // Every `require_authz*` call already takes this client, so hanging
            // the audit here makes the audit writer an explicit dependency of
            // every authorization check without touching a call site.
            Some(
                build_fga_client(config.as_ref())?.with_security_audit(moa_authz::SecurityAudit {
                    pool: pool.clone(),
                    emitter: audit.emitter(),
                    emit_allows: config.audit_security.emit_authz_allows,
                }),
            )
        };
        let authz_outbox_poller = fga_client
            .clone()
            .map(|fga_client| start_authz_outbox_poller(&background_pool, fga_client));
        let session_store = Arc::new(
            PostgresSessionStore::from_existing_pool_with_config(config.as_ref(), pool.clone())
                .await?,
        );
        let awakeable_resolver: Arc<dyn AwakeableResolver> = Arc::new(HttpAwakeableResolver::new(
            restate_ingress_base_url(restate_ingress_url),
        )?);
        let contact_token_issuer = moa_auth_providers::build_contact_token_issuer(config.as_ref())
            .context("build contact-token issuer")?;
        let token_vault_provider = moa_auth_providers::build_token_vault_provider(
            config.as_ref(),
            Arc::new(pool.clone()),
            kms.provider(),
        )
        .context("build token-vault provider")?;
        // One owner for the whole process. Deployment-owned transport secrets
        // are attached here rather than read again downstream, so a tenant
        // connection can never fall back to an operator credential.
        let credential_vault: Arc<dyn moa_core::traits::CredentialVault> = Arc::new(
            moa_auth_providers::PostgresCredentialVault::new(
                Arc::new(pool.clone()),
                kms.provider(),
            )
            .with_deployment_secrets(moa_messaging::delivery_deployment_secrets_from_env()),
        );
        let egress_classifier = (!config.mcp_servers.is_empty() || config.llm_dlp.tokenize_enabled)
            .then(|| build_egress_pii_classifier(config.as_ref()));
        let providers = Arc::new(build_provider_registry(
            config.as_ref(),
            Arc::clone(&runtime_cache),
            providers_override,
            egress_classifier.as_ref(),
        )?);
        let embedding_provider = build_embedding_provider_from_config(
            config.as_ref(),
            Some(Arc::clone(&runtime_cache)),
        )?;
        let mcp_egress_guard = build_mcp_egress_guard(config.as_ref(), egress_classifier.as_ref())?;
        let tool_router = ToolRouter::from_config(
            config.as_ref(),
            mcp_egress_guard,
            Some(session_store.clone()),
        )
        .await?
        .with_hand_lease_store(Arc::new(PostgresHandLeaseStore::new(pool.clone())))
        // The reaper is started unconditionally by `runtime::jobs` for this
        // process, so the router may admit deadlines whose destruction owner is
        // that reaper. Declaring it here rather than inferring it keeps the
        // admission check honest: a deployment that stops starting the reaper
        // has to change this line and will fail admission until it does.
        .with_hand_lease_reaper()
        .with_tenant_sandbox_policy_store(Arc::new(PostgresTenantSandboxPolicyStore::new(
            pool.clone(),
        )))
        .with_session_store(session_store.clone())
        .with_memory_retrieval_executor(Arc::new(
            crate::services::memory::OrchestratorMemoryRetrievalExecutor::new(
                pool.clone(),
                kms.provider(),
                config.clone(),
                Arc::clone(&runtime_cache),
            ),
        ))
        .with_memory_tool_executor(Arc::new(moa_memory_ingest::FastMemoryToolExecutor));
        // Both sandbox owners are attached by the builder chain above, so the
        // cloud requirement can only be checked once the router is complete.
        tool_router.validate_cloud_startup(config.as_ref())?;
        let tool_router = Arc::new(tool_router);
        let connector_runtime = build_connector_runtime_dependencies(
            pool.clone(),
            fga_client.clone(),
            credential_vault.clone(),
            tool_router.clone(),
        );
        let lineage = build_lineage_sink(config.as_ref(), background_pool.clone()).await?;
        let retrieval_embedder =
            build_retrieval_embedder(config.as_ref(), Arc::clone(&runtime_cache));
        // Reused for skill-manifest ranking; the retriever moves the original.
        let skill_embedder = retrieval_embedder.clone();
        let graph_memory_retriever = build_graph_memory_retriever(
            config.as_ref(),
            pool.clone(),
            kms.provider(),
            retrieval_embedder,
            lineage.handle.clone(),
        );
        let mut skill_injector = SkillInjector::new(pool.clone())
            .with_session_store(session_store.clone())
            .with_segment_store(session_store.clone())
            .with_budget_config(config.skill_budget.clone());
        if let Some(embedder) = skill_embedder {
            skill_injector = skill_injector.with_embedder(embedder);
        }
        let skill_injector = Arc::new(skill_injector);
        let channel_adapters = build_channel_adapters(config.as_ref(), runtime_cache.clone())?;
        moa_memory_ingest::install_runtime_with_config(
            background_pool.clone(),
            kms.provider(),
            config.as_ref(),
        )
        .context("install graph-memory ingestion runtime")?;

        Ok(Self {
            config,
            pool,
            background_pool,
            session_store,
            fga_client,
            contact_token_issuer,
            token_vault_provider,
            credential_vault,
            kms,
            runtime_cache,
            providers,
            embedding_provider,
            tool_router,
            connector_catalogs: connector_runtime.catalogs,
            connector_completion: connector_runtime.completion,
            connector_connections: connector_runtime.connections,
            connector_management: connector_runtime.management,
            connector_credential_ingress: connector_runtime.credential_ingress,
            graph_memory_retriever,
            skill_injector,
            lineage,
            awakeable_resolver,
            authz_outbox_poller,
            channel_adapters,
            audit: Arc::new(audit),
        })
    }

    /// Installs these dependencies into the process-wide handler context.
    pub fn install_orchestrator_ctx(&self) -> Result<(), &'static str> {
        OrchestratorCtx::install(Arc::new(self.orchestrator_ctx()))
    }

    /// Builds SCIM HTTP server state from the runtime dependencies.
    #[must_use]
    pub fn scim_state(&self, scim_base_url: String) -> ScimState {
        ScimState::new(
            self.pool.clone(),
            Arc::new(moa_auth_providers::LocalAuthProvider::new(Arc::new(
                self.pool.clone(),
            ))),
            self.fga_client.clone(),
            scim_base_url,
        )
    }

    /// Returns the process-owned private connector credential ingress controller.
    #[must_use]
    pub fn connector_credential_ingress(&self) -> ConnectorCredentialIngress {
        self.connector_credential_ingress.clone()
    }

    fn orchestrator_ctx(&self) -> OrchestratorCtx {
        OrchestratorCtx::new(
            self.config.clone(),
            OrchestratorDeps {
                persistence: PersistenceDeps::new(self.session_store.clone(), self.pool.clone()),
                auth: AuthDeps::new(self.fga_client.clone()),
                runtime_cache: self.runtime_cache.clone(),
                providers: ProviderDeps::new(
                    self.providers.clone(),
                    self.embedding_provider.clone(),
                ),
                tools: ToolDeps::new(self.tool_router.clone(), self.connector_catalogs.clone()),
                memory: MemoryDeps::new(
                    self.kms.provider(),
                    self.graph_memory_retriever.clone(),
                    self.skill_injector.clone(),
                ),
                lineage: LineageDeps::new(self.lineage.handle.clone()),
            },
        )
    }
}

/// Shared production connector-action composition used by context and Restate services.
#[derive(Clone)]
pub(crate) struct ConnectorRuntimeDeps {
    /// Scoped prompt and dispatch catalog provider.
    pub(crate) catalogs: ScopedConnectorCatalogProvider,
    /// Post-journal durable completion boundary.
    pub(crate) completion: ConnectorInvocationCompletionService,
    /// Single connector lifecycle service over the shared repository instance.
    pub(crate) connections: ConnectorService,
    /// Shared authorization-first connection-management service.
    pub(crate) management: ConnectorManagementService,
    /// Private, non-Restate credential-ingress controller.
    pub(crate) credential_ingress: ConnectorCredentialIngress,
}

#[derive(Clone, Copy)]
struct UnavailableConnectorManagementAuthorizer;

#[async_trait]
impl ConnectorManagementAuthorizer for UnavailableConnectorManagementAuthorizer {
    async fn require_tenant_admin(
        &self,
        _identity: &moa_core::traits::Identity,
    ) -> Result<(), ConnectorManagementAuthorizationError> {
        Err(ConnectorManagementAuthorizationError::Unavailable)
    }

    async fn require_connection_manage(
        &self,
        _identity: &moa_core::traits::Identity,
        _connection_id: moa_core::types::identifiers::ConnectorConnectionId,
    ) -> Result<(), ConnectorManagementAuthorizationError> {
        Err(ConnectorManagementAuthorizationError::Unavailable)
    }
}

/// Builds the governed catalog, constrained HTTP runtime, and completion service.
pub(crate) fn build_connector_runtime_dependencies(
    pool: PgPool,
    fga_client: Option<FgaClient>,
    credential_vault: Arc<dyn moa_core::traits::CredentialVault>,
    tool_router: Arc<ToolRouter>,
) -> ConnectorRuntimeDeps {
    let repository = Arc::new(PostgresConnectionRepository::new(pool.clone()));
    let catalog_source: Arc<dyn InstalledConnectorCatalogSource> = repository.clone();
    let authorizer = Arc::new(FgaConnectorUseAuthorizer::new(fga_client.clone()));
    let installed_catalog: Arc<dyn InstalledConnectorCatalog> = Arc::new(
        GovernedInstalledConnectorCatalog::new(catalog_source, authorizer),
    );
    let repository_port: Arc<dyn ConnectionRepository> = repository.clone();
    let destination_policy = connector_destination_policy();
    let coordinator =
        ConnectorInvocationCoordinator::new(installed_catalog.clone(), repository_port.clone());
    let runtime: Arc<dyn ConnectorActionRuntime> =
        Arc::new(HttpConnectorRuntime::with_coordinator(
            coordinator,
            credential_vault.clone(),
            destination_policy.clone(),
        ));
    let credential_verifier: Arc<dyn CredentialSlotVerifier> =
        Arc::new(VaultCredentialSlotVerifier::new(credential_vault.clone()));
    let connector_service = ConnectorService::new(repository_port.clone(), credential_verifier);
    let use_grants: Arc<dyn ConnectionUseGrantRepository> = repository;
    let management_authorizer: Arc<dyn ConnectorManagementAuthorizer> = match fga_client {
        Some(fga) => Arc::new(FgaConnectorManagementAuthorizer::new(fga)),
        None => Arc::new(UnavailableConnectorManagementAuthorizer),
    };
    let artifact_definitions: Arc<dyn ConnectorDefinitionResolver> = Arc::new(
        ArtifactConnectorDefinitionResolver::new(ArtifactRegistry::new(pool)),
    );
    let definitions: Arc<dyn ConnectorDefinitionResolver> = Arc::new(
        ManagedKnowledgeConnectorDefinitionResolver::new(artifact_definitions),
    );
    let destinations: Arc<dyn ConnectorDestinationVerifier> = Arc::new(
        PolicyConnectorDestinationVerifier::new(destination_policy.clone()),
    );
    let credential_revoker: Arc<dyn ConnectionCredentialRevoker> = Arc::new(
        VaultConnectionCredentialRevoker::new(credential_vault.clone()),
    );
    let management = ConnectorManagementService::new(
        management_authorizer,
        definitions,
        connector_service.clone(),
        use_grants,
        destinations,
        credential_revoker,
    );
    let coordinator: Arc<dyn CredentialIngressCoordinator> = Arc::new(
        ManagementCredentialIngressCoordinator::new(management.clone()),
    );
    let credential_ingress = ConnectorCredentialIngress::new(coordinator, credential_vault);
    ConnectorRuntimeDeps {
        catalogs: ScopedConnectorCatalogProvider::new(tool_router, installed_catalog, runtime),
        completion: ConnectorInvocationCompletionService::new(repository_port),
        connections: connector_service,
        management,
        credential_ingress,
    }
}

fn connector_destination_policy() -> OutboundHttpPolicy {
    #[cfg(feature = "integration")]
    if std::env::var("MOA_INTEGRATION_CONNECTOR_LOOPBACK_ENABLED").as_deref() == Ok("1") {
        // The constructor and opt-in are both integration-only. Ordinary binaries
        // keep the production HTTPS/public-address policy even if this test env
        // name is accidentally present in their deployment.
        return OutboundHttpPolicy::loopback_http_for_tests(Arc::new(TokioOutboundHostResolver));
    }

    OutboundHttpPolicy::production_system()
}

async fn build_runtime_cache_store(config: &MoaConfig) -> Result<Arc<dyn RuntimeCacheStore>> {
    // Backend selection fails closed at the source: `auto` without a Redis URL
    // (and without an explicit memory opt-in) returns a `ConfigError` rather than
    // silently selecting the process-local cache. Propagate that error; an
    // explicit `Memory` backend is rejected below with the orchestrator's own
    // message.
    let selected_backend = moa_runtime_store::select_runtime_cache_backend(&config.runtime_cache)?;
    match selected_backend {
        moa_runtime_store::ResolvedRuntimeCacheBackend::Memory => {
            bail!(
                "moa-orchestrator requires runtime_cache.backend = redis with runtime_cache.redis_url; memory runtime cache is process-local"
            );
        }
        moa_runtime_store::ResolvedRuntimeCacheBackend::Redis => {
            let Some(redis_url) = config
                .runtime_cache
                .redis_url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
            else {
                bail!("runtime_cache.redis_url must be set when runtime_cache.backend is redis");
            };
            let store = build_redis_runtime_cache_store(redis_url).await?;
            tracing::info!(
                backend = "redis",
                "runtime cache backend selected for distributed runtime coordination"
            );
            Ok(store)
        }
    }
}

async fn build_redis_runtime_cache_store(redis_url: &str) -> Result<Arc<dyn RuntimeCacheStore>> {
    Ok(Arc::new(
        moa_runtime_store::RedisRuntimeCacheStore::new(redis_url)
            .await
            .context("build Redis runtime cache store")?,
    ))
}

fn build_channel_adapters(
    config: &MoaConfig,
    runtime_cache: Arc<dyn RuntimeCacheStore>,
) -> Result<HashMap<Channel, Arc<dyn ChannelAdapter>>> {
    #[cfg(feature = "slack")]
    let mut adapters: HashMap<Channel, Arc<dyn ChannelAdapter>> = HashMap::new();
    #[cfg(not(feature = "slack"))]
    let adapters: HashMap<Channel, Arc<dyn ChannelAdapter>> = HashMap::new();
    #[cfg(feature = "slack")]
    match moa_messaging::SlackAdapter::from_config_with_runtime_cache(config, runtime_cache) {
        Ok(adapter) => {
            adapters.insert(Channel::Slack, Arc::new(adapter));
        }
        Err(moa_core::error::MoaError::MissingEnvironmentVariable(name)) => {
            tracing::warn!(
                env = %name,
                "Slack live progress delivery disabled because credentials are not configured"
            );
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(not(feature = "slack"))]
    {
        let _ = config;
        let _ = runtime_cache;
    }
    Ok(adapters)
}

fn build_retrieval_embedder(
    config: &MoaConfig,
    runtime_cache: Arc<dyn RuntimeCacheStore>,
) -> Option<Arc<dyn EmbeddingProvider>> {
    match build_embedder_from_config(
        config,
        Some(runtime_cache),
        EmbedderConstructionRole::Retrieval,
    ) {
        Ok(embedder) => Some(embedder),
        Err(error) => {
            tracing::warn!(
                %error,
                "graph memory vector retrieval disabled because the retrieval embedder could not be constructed"
            );
            None
        }
    }
}

fn build_provider_registry(
    config: &MoaConfig,
    runtime_cache: Arc<dyn RuntimeCacheStore>,
    providers_override: ProvidersOverride,
    egress_classifier: Option<&EgressPiiClassifier>,
) -> Result<ProviderRegistry> {
    match providers_override {
        ProvidersOverride::None => apply_llm_dlp(
            config,
            ProviderRegistry::from_config(config, Some(runtime_cache))?,
            egress_classifier,
        ),
        ProvidersOverride::Scripted { path } => {
            tracing::warn!(
                path = %path.display(),
                "loading scripted provider override (test mode)"
            );
            Ok(ProviderRegistry::scripted(path)?)
        }
        ProvidersOverride::Mock { seed } => {
            tracing::warn!(seed, "using mock provider override (test mode)");
            Ok(ProviderRegistry::mock(seed)?)
        }
    }
}

/// Attaches LLM DLP to `registry` when `[llm_dlp].tokenize_enabled`
/// is set, otherwise returns it unchanged (providers used directly, zero
/// overhead).
///
/// This is the composition point where `GovernedLLMProvider` is wired in:
/// with governance attached the registry wraps every resolved provider so
/// restricted spans are tokenized before egress and detokenized on the response.
fn apply_llm_dlp(
    config: &MoaConfig,
    registry: ProviderRegistry,
    egress_classifier: Option<&EgressPiiClassifier>,
) -> Result<ProviderRegistry> {
    if !config.llm_dlp.tokenize_enabled {
        return Ok(registry);
    }
    tracing::info!("egress DLP tokenization enabled; governing all LLM providers");
    let classifier = egress_classifier.context("LLM DLP classifier missing")?;
    Ok(registry.with_llm_dlp(
        Arc::clone(&classifier.classifier),
        classifier.namespace.clone(),
        classifier.model,
    ))
}

/// Builds the required data-class guard for configured outbound MCP servers.
///
/// MCP egress enforcement is independent of optional LLM tokenization. When
/// both paths are active they share one classifier instance.
fn build_mcp_egress_guard(
    config: &MoaConfig,
    egress_classifier: Option<&EgressPiiClassifier>,
) -> Result<Option<Arc<McpEgressGuard>>> {
    if config.mcp_servers.is_empty() {
        return Ok(None);
    }
    let classifier = egress_classifier.context("MCP egress classifier missing")?;
    Ok(Some(Arc::new(McpEgressGuard::new(Arc::clone(
        &classifier.classifier,
    )))))
}

/// Constructed egress classifier plus the identity used for performance-cache keys.
struct EgressPiiClassifier {
    classifier: Arc<dyn PiiClassifier>,
    namespace: String,
    model: &'static str,
}

/// Builds the PII classifier used for egress tokenization.
///
/// Mirrors the ingestion selection: the configured PII sidecar when a URL is set,
/// otherwise the deterministic heuristic classifier.
fn build_egress_pii_classifier(config: &MoaConfig) -> EgressPiiClassifier {
    if let Some(url) = config
        .memory
        .pii_service_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        match OpenAiPrivacyFilterClassifier::new(url.to_string()) {
            Ok(classifier) => {
                return EgressPiiClassifier {
                    classifier: Arc::new(classifier),
                    namespace: format!("moa.pii.sidecar:{url}"),
                    model: "openai/privacy-filter",
                };
            }
            Err(error) => tracing::warn!(
                %error,
                "egress DLP falling back to the heuristic classifier; PII sidecar client unavailable"
            ),
        }
    }
    EgressPiiClassifier {
        classifier: Arc::new(HeuristicPiiClassifier),
        namespace: "moa.memory.pii".to_string(),
        model: "heuristic-v1",
    }
}

fn build_fga_client(config: &MoaConfig) -> Result<FgaClient> {
    let openfga = config
        .authz
        .openfga
        .as_ref()
        .context("authz.openfga config missing")?;
    FgaClient::new(moa_authz::FgaConfig {
        url: openfga.url.clone(),
        preshared_key: openfga.preshared_key.clone(),
        store_id: openfga.store_id.clone(),
        model_id: openfga.model_id.clone(),
        timeout_ms: openfga.timeout_ms,
    })
    .context("build OpenFGA client")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use moa_config::MoaConfig;
    use moa_config::{McpServerConfig, RuntimeCacheBackend, RuntimeCacheConfig};
    use moa_core::traits::RuntimeCacheStore;

    use super::{build_egress_pii_classifier, build_mcp_egress_guard, build_runtime_cache_store};

    #[test]
    fn mcp_guard_is_built_when_llm_dlp_is_disabled_offline() {
        // Pins: configured MCP destinations always receive an egress guard;
        // optional LLM tokenization does not control this security boundary.
        let mut config = MoaConfig::default();
        assert!(!config.llm_dlp.tokenize_enabled);
        config.mcp_servers.push(McpServerConfig {
            required: false,
            discovery: moa_config::McpDiscoveryMode::Eager,
            name: "external".to_string(),
            url: "http://127.0.0.1:1".to_string(),
            credentials: None,
            trust_tool_annotations: false,
            allowed_data_classes: Vec::new(),
        });
        let classifier = build_egress_pii_classifier(&config);

        assert!(
            build_mcp_egress_guard(&config, Some(&classifier))
                .expect("configured MCP must have a classifier")
                .is_some()
        );
    }

    #[tokio::test]
    async fn runtime_cache_auto_without_redis_url_rejects_memory_fallback() {
        // Pins: orchestrator startup cannot use process-local runtime coordination.
        // Backend selection now fails closed at the source, so `auto` without a
        // Redis URL surfaces the selector's ConfigError rather than the
        // orchestrator's own bail.
        let config = MoaConfig::default();
        let error = match build_runtime_cache_store(&config).await {
            Ok(_) => panic!("auto memory runtime cache should fail startup"),
            Err(error) => error,
        };

        assert!(
            error.to_string().contains("no Redis URL is configured"),
            "expected fail-closed backend-selection error, got: {error}"
        );
    }

    #[tokio::test]
    async fn runtime_cache_memory_backend_rejects_process_local_memory() {
        // Pins: explicit memory backend is not valid for the distributed orchestrator.
        let config = MoaConfig {
            runtime_cache: RuntimeCacheConfig {
                backend: RuntimeCacheBackend::Memory,
                redis_url: Some("redis://unused.example:6379/0".to_string()),
            },
            ..MoaConfig::default()
        };

        let error = match build_runtime_cache_store(&config).await {
            Ok(_) => panic!("memory runtime cache should fail startup"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "moa-orchestrator requires runtime_cache.backend = redis with runtime_cache.redis_url; memory runtime cache is process-local"
        );
    }

    #[tokio::test]
    async fn runtime_cache_redis_backend_without_url_fails_clearly() {
        // Pins: selecting Redis without its URL fails startup before handlers are installed.
        let config = MoaConfig {
            runtime_cache: RuntimeCacheConfig {
                backend: RuntimeCacheBackend::Redis,
                redis_url: None,
            },
            ..MoaConfig::default()
        };

        let error = match build_runtime_cache_store(&config).await {
            Ok(_) => panic!("redis backend without URL should fail startup"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "runtime_cache.redis_url must be set when runtime_cache.backend is redis"
        );
    }

    #[tokio::test]
    async fn runtime_cache_memory_store_remains_available_for_non_orchestrator_tests() {
        // Pins: the process-local implementation still exists for isolated unit tests.
        let store = moa_runtime_store::MemoryRuntimeCacheStore::new();
        store
            .set(
                "runtime-cache:unit",
                b"memory".to_vec(),
                Duration::from_secs(60),
            )
            .await
            .expect("memory runtime cache should accept writes");

        assert_eq!(
            store
                .get("runtime-cache:unit")
                .await
                .expect("memory runtime cache should accept reads"),
            Some(b"memory".to_vec())
        );
    }
}
