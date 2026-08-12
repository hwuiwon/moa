//! Dependency construction for orchestrator runtime startup.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::brain_bridge::{TurnPipelineStageFactory, TurnRequestPreparer};
use crate::connector_catalog::ScopedConnectorCatalogProvider;
use crate::credential_ingress::{
    ConnectorCredentialIngress, CredentialIngressCoordinator,
    ManagementCredentialIngressCoordinator,
};
use anyhow::{Context as AnyhowContext, Result, bail};
use async_trait::async_trait;
use moa_authz::{AwakeableResolver, FgaClient};
use moa_brain::pipeline::{memory::GraphMemoryRetriever, skills::SkillInjector};
use moa_config::MoaConfig;
use moa_connectors::catalog::{
    FgaConnectorUseAuthorizer, GovernedInstalledConnectorCatalog, InstalledConnectorCatalog,
    InstalledConnectorCatalogSource,
};
use moa_connectors::executor::{ConnectorActionRuntime, ConnectorInvocationCompletionService};
use moa_connectors::http::HttpConnectorRuntime;
use moa_connectors::repository::{
    ConnectionLifecycleRepository, ConnectionUseGrantRepository, ConnectorInvocationRepository,
    ManagedParentRepository, PostgresConnectionRepository,
};
use moa_connectors::service::{ConnectorService, CredentialSlotVerifier};
use moa_core::{
    traits::{ChannelAdapter, EmbeddingProvider, RuntimeCacheStore},
    types::channel::Channel,
    types::identifiers::TenantId,
};
use moa_hands::{
    PostgresTenantSandboxPolicyStore, ToolRouter, core::leases::PostgresHandLeaseStore,
};
use moa_memory_pii::{HeuristicPiiClassifier, OpenAiPrivacyFilterClassifier, PiiClassifier};
use moa_messaging::ProviderDeliverySink;
use moa_providers::{
    EmbedderConstructionRole, ProviderRegistry, build_embedder_from_config,
    build_embedding_provider_from_config,
};
use moa_retrieval::engine::MemoryRetrievalEngine;
#[cfg(feature = "integration")]
use moa_security::outbound_http::TokioOutboundHostResolver;
use moa_security::{McpEgressGuard, OutboundHttpPolicy};
use moa_session::PostgresSessionStore;
use sqlx::PgPool;

use crate::services::{
    authz_challenges_reaper::HttpAwakeableResolver,
    connectors::{
        authz::{
            ConnectorManagementAuthorizationError, ConnectorManagementAuthorizer,
            FgaConnectorManagementAuthorizer,
        },
        credentials::{
            ConnectionCredentialRevoker, VaultConnectionCredentialRevoker,
            VaultCredentialSlotVerifier,
        },
        definitions::{ArtifactConnectorDefinitionResolver, ConnectorDefinitionResolver},
        management::{
            ConnectorDestinationVerifier, ConnectorManagementService,
            PolicyConnectorDestinationVerifier,
        },
    },
    scim::ScimState,
};
use moa_artifacts::registry::ArtifactRegistry;

#[cfg(all(feature = "provider-overrides", feature = "integration"))]
use crate::services::tool_executor::{
    ExecutionExternalJobAdapter, FixtureExternalJobTool, FixtureHttpExecutionExternalJobAdapter,
};
use crate::{
    config::ProvidersOverride,
    lineage::{LineageSinkRuntime, build_lineage_sink},
    runtime::{jobs::restate_ingress_base_url, kms::KmsRuntime},
    services::tool_executor::ExecutionExternalJobAdapterRegistry,
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
    /// Shared asynchronous-provider adapter registry used by execution and raw callbacks.
    pub external_job_adapters: ExecutionExternalJobAdapterRegistry,
    /// Authenticated checkpoint-bucket versioning observer shared with the store gate.
    pub checkpoint_versioning_observer: Option<
        moa_hands::core::sandbox_workspace::checkpoint::versioning::CheckpointBucketVersioningObserver,
    >,
    /// Process-wide workspace maintenance and tenant-purge owner.
    pub workspace_maintenance: Option<
        Arc<moa_hands::core::sandbox_workspace::maintenance::WorkspaceMaintenanceCoordinator>,
    >,
    /// Tenants whose active destruction fence denied quota bootstrap.
    ///
    /// Maintenance remains available to finish their purge, while every new
    /// workspace admission path fails closed for the lifetime of this process.
    pub sandbox_workspace_fenced_tenants: Arc<HashSet<TenantId>>,
    /// Host-owned graph-memory ingestion runtime shared by both ingest adapters.
    pub(crate) ingest_runtime: Arc<moa_memory_ingest::IngestRuntime>,
    /// Process-wide graph-memory retrieval engine shared by every read adapter.
    pub(crate) retrieval_engine: Arc<MemoryRetrievalEngine>,
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
    /// Explicit root-turn request compiler injected into the turn workflow.
    pub(crate) turn_request_preparer: Arc<TurnRequestPreparer>,
    /// Process-owned operator-message delivery sink.
    pub(crate) delivery_sink: ProviderDeliverySink,
    /// Selected lineage sink and optional writer.
    pub lineage: LineageSinkRuntime,
    /// Awakeable resolver used by builtin async authorization.
    pub awakeable_resolver: Arc<dyn AwakeableResolver>,
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
        maintenance_pool: Option<PgPool>,
        restate_ingress_url: &str,
        providers_override: ProvidersOverride,
        skip_fga: bool,
    ) -> Result<Self> {
        let workspace_runtime_enabled = config.sandbox_workspaces.mode.maintenance_enabled();
        let sandbox_workspace_bootstrap = if workspace_runtime_enabled {
            let maintenance_pool = maintenance_pool.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "sandbox workspace maintenance requires its dedicated database pool"
                )
            })?;
            moa_hands::core::sandbox_workspace::maintenance::WorkspaceMaintenanceCoordinator::verify_maintenance_pool(
                maintenance_pool,
            )
            .await
            .context("verify dedicated sandbox workspace maintenance database role")?;
            crate::runtime::sandbox_workspace_rollout::bootstrap_accounts_and_quotas(
                config.as_ref(),
                maintenance_pool,
            )
            .await
            .context("bootstrap sandbox provider accounts and quota routes")?
        } else {
            crate::runtime::sandbox_workspace_rollout::SandboxWorkspaceBootstrapReport::default()
        };
        let sandbox_workspace_fenced_tenants =
            Arc::new(sandbox_workspace_bootstrap.fenced_tenants());
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
            let fga_client = build_fga_client(config.as_ref())?;
            if workspace_runtime_enabled {
                let expected_model: serde_json::Value =
                    serde_json::from_str(moa_authz_schema::SCHEMA_V1_JSON)
                        .context("decode compiled OpenFGA model v7")?;
                fga_client
                    .verify_authorization_model(&expected_model)
                    .await
                    .context("verify exact OpenFGA model v7 before workspace runtime startup")?;
            }
            Some(fga_client.with_security_audit(moa_authz::SecurityAudit {
                pool: pool.clone(),
                emitter: audit.emitter(),
                emit_allows: config.audit_security.emit_authz_allows,
            }))
        };
        let session_store = Arc::new(
            PostgresSessionStore::from_existing_pool_with_config(config.as_ref(), pool.clone())
                .await?,
        );
        let awakeable_resolver: Arc<dyn AwakeableResolver> = Arc::new(HttpAwakeableResolver::new(
            restate_ingress_base_url(restate_ingress_url),
        )?);
        let contact_token_issuer = moa_auth_providers::build_contact_token_issuer(config.as_ref())
            .context("build contact-token issuer")?;
        let credential_vault: Arc<dyn moa_core::traits::CredentialVault> =
            Arc::new(moa_auth_providers::PostgresCredentialVault::new(
                Arc::new(pool.clone()),
                kms.provider(),
            ));
        let delivery_sink = ProviderDeliverySink::from_env(&config.messaging)
            .context("build operator-message delivery sink")?;
        let egress_classifier = (!config.mcp_servers.is_empty() || config.llm_dlp.tokenize_enabled)
            .then(|| build_egress_pii_classifier(config.as_ref()));
        let provider_override_active = providers_override.is_active();
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
        let lineage = build_lineage_sink(config.as_ref(), background_pool.clone()).await?;
        let retrieval_embedder =
            build_retrieval_embedder(config.as_ref(), Arc::clone(&runtime_cache));
        let retrieval_engine = Arc::new(
            MemoryRetrievalEngine::new(
                config.as_ref().clone(),
                pool.clone(),
                kms.provider(),
                retrieval_embedder.clone(),
            )
            .with_assume_app_role(true),
        );
        let graph_memory_retriever = Arc::new(
            GraphMemoryRetriever::from_engine(pool.clone(), retrieval_engine.as_ref().clone())
                .with_lineage(lineage.handle.clone()),
        );
        let mut skill_injector = SkillInjector::new(pool.clone())
            .with_session_store(session_store.clone())
            .with_segment_store(session_store.clone())
            .with_budget_config(config.skill_budget.clone());
        if let Some(embedder) = retrieval_embedder {
            skill_injector = skill_injector.with_embedder(embedder);
        }
        let skill_injector = Arc::new(skill_injector);
        let ingest_runtime = Arc::new(
            moa_memory_ingest::IngestRuntime::from_config(
                background_pool.clone(),
                kms.provider(),
                config.as_ref(),
            )
            .context("build graph-memory ingestion runtime")?,
        );
        let mcp_egress_guard = build_mcp_egress_guard(config.as_ref(), egress_classifier.as_ref())?;
        let (checkpoint_store, checkpoint_versioning_observer) = if workspace_runtime_enabled {
            kms.require_durable("sandbox workspace checkpoints")?;
            let (store, observer) =
                moa_hands::core::sandbox_workspace::checkpoint::store::CheckpointObjectStore::from_config_with_versioning_observer(
                    config.as_ref(),
                    kms.provider(),
                )?;
            observer
                .observe_unversioned()
                .await
                .context("observe checkpoint bucket versioning before workspace startup")?;
            let store = Arc::new(store);
            store
                .preflight_create_only_namespace()
                .await
                .context("preflight checkpoint bucket create-only namespace")?;
            (Some(store), Some(observer))
        } else {
            (None, None)
        };
        let tool_router = ToolRouter::from_config_with_checkpoint_store(
            config.as_ref(),
            mcp_egress_guard,
            Some(session_store.clone()),
            checkpoint_store.clone(),
            Some(pool.clone()),
            workspace_runtime_enabled.then(|| kms.provider()),
            workspace_runtime_enabled,
        )
        .await?;
        let tool_router = if workspace_runtime_enabled {
            tool_router
                .with_hand_lease_store(Arc::new(PostgresHandLeaseStore::new(pool.clone())))
                // Runtime jobs starts the destruction owner before listeners.
                .with_hand_lease_reaper()
                .with_tenant_sandbox_policy_store(Arc::new(PostgresTenantSandboxPolicyStore::new(
                    pool.clone(),
                )))
        } else {
            tool_router
        }
        .with_session_store(session_store.clone())
        .with_memory_retrieval_executor(Arc::new(
            crate::services::memory::OrchestratorMemoryRetrievalExecutor::from_retrieval_engine(
                pool.clone(),
                kms.provider(),
                retrieval_engine.clone(),
            ),
        ))
        .with_memory_tool_executor(Arc::new(
            moa_memory_ingest::FastMemoryToolExecutor::new(ingest_runtime.clone()),
        ));
        let external_job_adapters = build_external_job_adapter_registry(provider_override_active)?;
        let tool_router = register_fixture_external_job_tool(tool_router, &external_job_adapters)?;
        // Both sandbox owners are attached by the builder chain above, so the
        // cloud requirement can only be checked once the router is complete.
        if workspace_runtime_enabled {
            tool_router.validate_cloud_startup(config.as_ref())?;
        }
        let tool_router = Arc::new(tool_router);
        let workspace_maintenance = if config.sandbox_workspaces.mode.maintenance_enabled() {
            let maintenance_pool = maintenance_pool.ok_or_else(|| {
                anyhow::anyhow!(
                    "sandbox workspace maintenance requires its dedicated database pool"
                )
            })?;
            let checkpoint_store = checkpoint_store.clone().ok_or_else(|| {
                anyhow::anyhow!(
                    "sandbox workspace maintenance requires the durable checkpoint store"
                )
            })?;
            Some(Arc::new(
                moa_hands::core::sandbox_workspace::maintenance::WorkspaceMaintenanceCoordinator::new(
                    maintenance_pool,
                    checkpoint_store,
                    tool_router.sandbox_storage_providers(),
                    tool_router.hand_providers(),
                    config.sandbox_checkpoints.retention.clone(),
                    std::time::Duration::from_secs(
                        config.sandbox_workspaces.reconciliation_claim_ttl_seconds,
                    ),
                )
                .context("build sandbox workspace maintenance coordinator")?,
            ))
        } else {
            None
        };
        let connector_runtime = build_connector_runtime_dependencies(
            pool.clone(),
            fga_client.clone(),
            credential_vault.clone(),
            tool_router.clone(),
        );
        let turn_request_preparer = Arc::new(TurnRequestPreparer::new(
            session_store.clone(),
            config.clone(),
            providers.clone(),
            connector_runtime.catalogs.clone(),
            TurnPipelineStageFactory::new(
                pool.clone(),
                graph_memory_retriever.clone(),
                skill_injector.clone(),
            ),
            lineage.handle.clone(),
        ));
        let channel_adapters = build_channel_adapters(config.as_ref(), runtime_cache.clone())?;
        Ok(Self {
            config,
            pool,
            background_pool,
            session_store,
            fga_client,
            contact_token_issuer,
            credential_vault,
            kms,
            runtime_cache,
            providers,
            embedding_provider,
            tool_router,
            external_job_adapters,
            checkpoint_versioning_observer,
            workspace_maintenance,
            sandbox_workspace_fenced_tenants,
            ingest_runtime,
            retrieval_engine,
            connector_catalogs: connector_runtime.catalogs,
            connector_completion: connector_runtime.completion,
            connector_connections: connector_runtime.connections,
            connector_management: connector_runtime.management,
            connector_credential_ingress: connector_runtime.credential_ingress,
            turn_request_preparer,
            delivery_sink,
            lineage,
            awakeable_resolver,
            channel_adapters,
            audit: Arc::new(audit),
        })
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
    let lifecycle: Arc<dyn ConnectionLifecycleRepository> = repository.clone();
    let managed_parents: Arc<dyn ManagedParentRepository> = repository.clone();
    let invocations: Arc<dyn ConnectorInvocationRepository> = repository.clone();
    let use_grants: Arc<dyn ConnectionUseGrantRepository> = repository;
    let destination_policy = connector_destination_policy();
    let runtime: Arc<dyn ConnectorActionRuntime> = Arc::new(HttpConnectorRuntime::new(
        lifecycle.clone(),
        invocations.clone(),
        credential_vault.clone(),
        destination_policy.clone(),
    ));
    let credential_verifier: Arc<dyn CredentialSlotVerifier> =
        Arc::new(VaultCredentialSlotVerifier::new(credential_vault.clone()));
    let connector_service = ConnectorService::new(lifecycle, managed_parents, credential_verifier);
    let management_authorizer: Arc<dyn ConnectorManagementAuthorizer> = match fga_client {
        Some(fga) => Arc::new(FgaConnectorManagementAuthorizer::new(fga)),
        None => Arc::new(UnavailableConnectorManagementAuthorizer),
    };
    let definitions: Arc<dyn ConnectorDefinitionResolver> = Arc::new(
        ArtifactConnectorDefinitionResolver::new(ArtifactRegistry::new(pool)),
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
        completion: ConnectorInvocationCompletionService::new(invocations),
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

#[cfg(all(feature = "provider-overrides", feature = "integration"))]
fn build_external_job_adapter_registry(
    provider_override_active: bool,
) -> Result<ExecutionExternalJobAdapterRegistry> {
    let Some(base_url) = std::env::var("MOA_FIXTURE_EXTERNAL_JOB_ADAPTER_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(ExecutionExternalJobAdapterRegistry::default());
    };
    if !provider_override_active {
        bail!(
            "MOA_FIXTURE_EXTERNAL_JOB_ADAPTER_URL requires the provider-override integration lane"
        );
    }
    let adapter = Arc::new(FixtureHttpExecutionExternalJobAdapter::new(&base_url)?)
        as Arc<dyn ExecutionExternalJobAdapter>;
    ExecutionExternalJobAdapterRegistry::new([adapter]).map_err(Into::into)
}

#[cfg(not(all(feature = "provider-overrides", feature = "integration")))]
fn build_external_job_adapter_registry(
    _provider_override_active: bool,
) -> Result<ExecutionExternalJobAdapterRegistry> {
    if std::env::var_os("MOA_FIXTURE_EXTERNAL_JOB_ADAPTER_URL").is_some() {
        bail!(
            "MOA_FIXTURE_EXTERNAL_JOB_ADAPTER_URL requires provider-overrides and integration features"
        );
    }
    Ok(ExecutionExternalJobAdapterRegistry::default())
}

#[cfg(all(feature = "provider-overrides", feature = "integration"))]
fn register_fixture_external_job_tool(
    tool_router: ToolRouter,
    adapters: &ExecutionExternalJobAdapterRegistry,
) -> Result<ToolRouter> {
    if !adapters.is_empty() {
        return tool_router
            .with_additional_builtin(Arc::new(FixtureExternalJobTool))
            .map_err(Into::into);
    }
    Ok(tool_router)
}

#[cfg(not(all(feature = "provider-overrides", feature = "integration")))]
fn register_fixture_external_job_tool(
    tool_router: ToolRouter,
    _adapters: &ExecutionExternalJobAdapterRegistry,
) -> Result<ToolRouter> {
    Ok(tool_router)
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
