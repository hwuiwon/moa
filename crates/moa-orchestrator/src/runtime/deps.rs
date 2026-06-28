//! Dependency construction for orchestrator runtime startup.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as AnyhowContext, Result, bail};
use moa_authz::{AwakeableResolver, FgaClient};
use moa_brain::{
    build_graph_memory_retriever,
    pipeline::{memory::GraphMemoryRetriever, skills::SkillInjector},
};
use moa_core::{
    Channel, MoaConfig,
    traits::{ChannelAdapter, EmbeddingProvider, RuntimeCacheStore},
};
use moa_hands::{ToolRouter, core::leases::PostgresHandLeaseStore};
use moa_memory_vector::{EmbedderConstructionRole, build_embedder_from_config};
use moa_observability::record_retrieval_embedder_construction;
use moa_providers::{ProviderRegistry, build_embedding_provider_from_config};
use moa_session::PostgresSessionStore;
use serde_json::Value;
use sqlx::PgPool;

use crate::{
    config::ProvidersOverride,
    ctx::{
        AuthDeps, LineageDeps, MemoryDeps, MessagingDeps, OrchestratorCtx, OrchestratorDeps,
        PersistenceDeps, ProviderDeps, ToolDeps,
    },
    lineage::{LineageSinkRuntime, build_lineage_sink},
    runtime::jobs::{restate_ingress_base_url, start_authz_outbox_poller},
    services::{authz_challenges_reaper::HttpAwakeableResolver, scim::ScimState},
};

/// Constructed dependencies shared by Restate handlers and process services.
pub struct RuntimeDeps {
    /// Shared orchestrator configuration.
    pub config: Arc<MoaConfig>,
    /// Runtime Postgres pool.
    pub pool: PgPool,
    /// Session and analytics store backed by Postgres.
    pub session_store: Arc<PostgresSessionStore>,
    /// Optional OpenFGA authorization client.
    pub fga_client: Option<FgaClient>,
    /// Authentication, token-vault, and async-approval providers.
    pub auth_providers: moa_auth_providers::Providers,
    /// Runtime cache used for ephemeral coordination state.
    pub runtime_cache: Arc<dyn RuntimeCacheStore>,
    /// Configured LLM provider registry.
    pub providers: Arc<ProviderRegistry>,
    /// Optional embedding provider.
    pub embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Tool router used by ToolExecutor and runtime services.
    pub tool_router: Arc<ToolRouter>,
    /// Precompiled tool schemas.
    pub tool_schemas: Arc<Vec<Value>>,
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
    /// Configured live outbound channel adapters.
    pub channel_adapters: HashMap<Channel, Arc<dyn ChannelAdapter>>,
}

impl RuntimeDeps {
    /// Builds all runtime dependencies from configuration and the runtime pool.
    pub async fn build(
        config: Arc<MoaConfig>,
        pool: PgPool,
        restate_ingress_url: &str,
        providers_override: ProvidersOverride,
        skip_fga: bool,
    ) -> Result<Self> {
        moa_authz::configure_security_audit(pool.clone(), config.audit_security.emit_authz_allows);
        let fga_client = if skip_fga {
            tracing::warn!("MOA_SKIP_FGA set; authz outbox poller disabled");
            None
        } else {
            Some(build_fga_client(config.as_ref())?)
        };
        let authz_outbox_poller = fga_client
            .clone()
            .map(|fga_client| start_authz_outbox_poller(&pool, fga_client));
        let session_store = Arc::new(
            PostgresSessionStore::from_existing_pool_with_config(config.as_ref(), pool.clone())
                .await?,
        );
        let awakeable_resolver: Arc<dyn AwakeableResolver> = Arc::new(HttpAwakeableResolver::new(
            restate_ingress_base_url(restate_ingress_url),
        )?);
        let auth_providers = moa_auth_providers::build_providers_with_resolver(
            config.as_ref(),
            Arc::new(pool.clone()),
            Some(awakeable_resolver.clone()),
        )
        .context("build providers bundle")?;
        let runtime_cache = build_runtime_cache_store(config.as_ref()).await?;

        let providers = Arc::new(build_provider_registry(
            config.as_ref(),
            providers_override,
        )?);
        let embedding_provider = build_embedding_provider_from_config(config.as_ref())?;
        let tool_router = Arc::new(
            ToolRouter::from_config(config.as_ref())
                .await?
                .with_hand_lease_store(Arc::new(PostgresHandLeaseStore::new(pool.clone())))
                .with_rule_store(session_store.clone())
                .with_session_store(session_store.clone()),
        );
        validate_lineage_journal_startup(config.as_ref())?;
        let lineage = build_lineage_sink(config.as_ref(), pool.clone()).await?;
        let retrieval_embedder = build_retrieval_embedder(config.as_ref());
        let graph_memory_retriever = build_graph_memory_retriever(
            config.as_ref(),
            pool.clone(),
            retrieval_embedder,
            lineage.handle.clone(),
        );
        let skill_injector = Arc::new(
            SkillInjector::new(pool.clone())
                .with_session_store(session_store.clone())
                .with_segment_store(session_store.clone())
                .with_budget_config(config.skill_budget.clone()),
        );
        let tool_schemas = Arc::new(tool_router.tool_schemas());
        let channel_adapters = build_channel_adapters(config.as_ref(), runtime_cache.clone())?;
        moa_memory_ingest::install_runtime_with_config(pool.clone(), config.as_ref())
            .context("install graph-memory ingestion runtime")?;

        Ok(Self {
            config,
            pool,
            session_store,
            fga_client,
            auth_providers,
            runtime_cache,
            providers,
            embedding_provider,
            tool_router,
            tool_schemas,
            graph_memory_retriever,
            skill_injector,
            lineage,
            awakeable_resolver,
            authz_outbox_poller,
            channel_adapters,
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

    fn orchestrator_ctx(&self) -> OrchestratorCtx {
        OrchestratorCtx::new(
            self.config.clone(),
            OrchestratorDeps {
                persistence: PersistenceDeps::new(self.session_store.clone(), self.pool.clone()),
                auth: AuthDeps::new(self.fga_client.clone(), self.auth_providers.clone()),
                runtime_cache: self.runtime_cache.clone(),
                providers: ProviderDeps::new(
                    self.providers.clone(),
                    self.embedding_provider.clone(),
                ),
                tools: ToolDeps::new(self.tool_router.clone(), self.tool_schemas.clone()),
                memory: MemoryDeps::new(
                    self.graph_memory_retriever.clone(),
                    self.skill_injector.clone(),
                ),
                lineage: LineageDeps::new(self.lineage.handle.clone(), self.lineage.writer.clone()),
                messaging: MessagingDeps::new(self.channel_adapters.clone()),
            },
        )
    }
}

fn validate_lineage_journal_startup(config: &MoaConfig) -> Result<()> {
    if config.observability.lineage.enabled || lineage_sink_env_uses_journal() {
        config
            .observability
            .lineage
            .validate_journal_path(config.cloud.enabled)?;
    }
    Ok(())
}

fn lineage_sink_env_uses_journal() -> bool {
    std::env::var("MOA_LINEAGE_SINK")
        .ok()
        .as_deref()
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("postgres"))
}

async fn build_runtime_cache_store(config: &MoaConfig) -> Result<Arc<dyn RuntimeCacheStore>> {
    let selected_backend = moa_runtime_store::select_runtime_cache_backend(&config.runtime_cache);
    match selected_backend {
        moa_runtime_store::ResolvedRuntimeCacheBackend::Memory => {
            if config.cloud.enabled {
                bail!(
                    "cloud.enabled requires runtime_cache.backend = redis with runtime_cache.redis_url; memory runtime cache is process-local"
                );
            }
            tracing::info!(
                backend = "memory",
                "runtime cache backend selected; memory backend is process-local and must not be used for correctness-sensitive distributed coordination"
            );
            Ok(Arc::new(moa_runtime_store::MemoryRuntimeCacheStore::new()))
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

#[cfg(feature = "redis")]
async fn build_redis_runtime_cache_store(redis_url: &str) -> Result<Arc<dyn RuntimeCacheStore>> {
    Ok(Arc::new(
        moa_runtime_store::RedisRuntimeCacheStore::new(redis_url)
            .await
            .context("build Redis runtime cache store")?,
    ))
}

#[cfg(not(feature = "redis"))]
async fn build_redis_runtime_cache_store(redis_url: &str) -> Result<Arc<dyn RuntimeCacheStore>> {
    let _ = redis_url;
    bail!("runtime_cache.backend = redis requires the moa-orchestrator redis feature");
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
        Err(moa_core::MoaError::MissingEnvironmentVariable(name)) => {
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

fn build_retrieval_embedder(config: &MoaConfig) -> Option<Arc<dyn EmbeddingProvider>> {
    let embedder_started = Instant::now();
    match build_embedder_from_config(config, EmbedderConstructionRole::Retrieval) {
        Ok(embedder) => {
            record_retrieval_embedder_construction("success", embedder_started.elapsed());
            Some(embedder)
        }
        Err(error) => {
            record_retrieval_embedder_construction("failure", embedder_started.elapsed());
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
    providers_override: ProvidersOverride,
) -> Result<ProviderRegistry> {
    match providers_override {
        ProvidersOverride::None => Ok(ProviderRegistry::from_config(config)),
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

    use moa_core::{
        MoaConfig,
        config::{CloudConfig, RuntimeCacheBackend, RuntimeCacheConfig},
    };

    use super::build_runtime_cache_store;

    #[tokio::test]
    async fn runtime_cache_auto_without_redis_url_builds_memory_store() {
        // Pins: backend auto falls back to process-local memory when no Redis URL is configured.
        let config = MoaConfig::default();
        let store = build_runtime_cache_store(&config)
            .await
            .expect("auto cache should build");

        store
            .set(
                "runtime-cache:auto",
                b"memory".to_vec(),
                Duration::from_secs(60),
            )
            .await
            .expect("memory runtime cache should accept writes");

        assert_eq!(
            store
                .get("runtime-cache:auto")
                .await
                .expect("memory runtime cache should accept reads"),
            Some(b"memory".to_vec())
        );
    }

    #[tokio::test]
    async fn runtime_cache_memory_backend_builds_memory_store() {
        // Pins: explicit memory backend constructs the runtime cache used by local/test wiring.
        let config = MoaConfig {
            runtime_cache: RuntimeCacheConfig {
                backend: RuntimeCacheBackend::Memory,
                redis_url: Some("redis://unused.example:6379/0".to_string()),
                ..RuntimeCacheConfig::default()
            },
            ..MoaConfig::default()
        };

        let store = build_runtime_cache_store(&config)
            .await
            .expect("memory cache should build");

        store
            .set(
                "runtime-cache:memory",
                b"configured".to_vec(),
                Duration::from_secs(60),
            )
            .await
            .expect("memory runtime cache should accept writes");

        assert_eq!(
            store
                .get("runtime-cache:memory")
                .await
                .expect("memory runtime cache should accept reads"),
            Some(b"configured".to_vec())
        );
    }

    #[tokio::test]
    async fn runtime_cache_redis_backend_without_url_fails_clearly() {
        // Pins: selecting Redis without its URL fails startup before handlers are installed.
        let config = MoaConfig {
            runtime_cache: RuntimeCacheConfig {
                backend: RuntimeCacheBackend::Redis,
                redis_url: None,
                ..RuntimeCacheConfig::default()
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
    async fn runtime_cache_cloud_mode_rejects_process_local_memory() {
        // Pins: cloud startup cannot silently select process-local runtime coordination.
        let config = MoaConfig {
            cloud: CloudConfig {
                enabled: true,
                ..CloudConfig::default()
            },
            runtime_cache: RuntimeCacheConfig {
                backend: RuntimeCacheBackend::Memory,
                redis_url: Some("redis://cache.example:6379/0".to_string()),
            },
            ..MoaConfig::default()
        };

        let error = match build_runtime_cache_store(&config).await {
            Ok(_) => panic!("cloud memory runtime cache should fail startup"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "cloud.enabled requires runtime_cache.backend = redis with runtime_cache.redis_url; memory runtime cache is process-local"
        );
    }

    #[tokio::test]
    async fn runtime_cache_cloud_mode_rejects_auto_memory_fallback() {
        // Pins: cloud startup cannot use auto when auto resolves to memory.
        let config = MoaConfig {
            cloud: CloudConfig {
                enabled: true,
                ..CloudConfig::default()
            },
            runtime_cache: RuntimeCacheConfig {
                backend: RuntimeCacheBackend::Auto,
                redis_url: None,
            },
            ..MoaConfig::default()
        };

        let error = match build_runtime_cache_store(&config).await {
            Ok(_) => panic!("cloud auto memory runtime cache should fail startup"),
            Err(error) => error,
        };

        assert_eq!(
            error.to_string(),
            "cloud.enabled requires runtime_cache.backend = redis with runtime_cache.redis_url; memory runtime cache is process-local"
        );
    }
}
