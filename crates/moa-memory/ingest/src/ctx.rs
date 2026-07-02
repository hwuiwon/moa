//! Runtime context installed by hosts that execute graph-memory ingestion.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::{Arc, OnceLock},
};

use moa_core::{MoaConfig, traits::EmbeddingProvider};
use moa_memory_graph::GraphStore;
use moa_memory_pii::PiiClassifier;
use moa_memory_vector::{VectorStore, VectorStoreFactory};
use moa_providers::CohereV4Embedder;
use sqlx::PgPool;

use crate::{
    ContradictionDetector, EntityMergeVerifier, EntityResolver, FactExtractor,
    HeuristicFactExtractor, IngestError, LlmEntityMergeVerifier, LlmFactExtractor, Result,
    RrfPlusJudgeDetector,
};

static INGEST_RUNTIME: OnceLock<IngestRuntime> = OnceLock::new();

/// Error returned when installing the process-local ingestion runtime fails.
#[derive(Debug, thiserror::Error)]
pub enum IngestRuntimeInstallError {
    /// A different runtime has already been installed in this process.
    #[error(
        "ingestion runtime already installed with incompatible dependencies: installed={installed}, requested={requested}"
    )]
    IncompatibleRuntime {
        /// Summary of the installed runtime configuration.
        installed: String,
        /// Summary of the requested runtime configuration.
        requested: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct IngestRuntimeFingerprint {
    pool_options_hash: u64,
    pii_service_url: Option<String>,
    cohere_api_key_configured: bool,
    cohere_api_key_hash: u64,
    extractor_name: &'static str,
    entity_resolver_name: &'static str,
    entity_blocking_enabled: bool,
    contradiction_detector_name: &'static str,
    memory_config_hash: u64,
    observability_environment: Option<String>,
}

impl IngestRuntimeFingerprint {
    fn new(
        pool: &PgPool,
        config: &MoaConfig,
        extractor_name: &'static str,
        entity_resolver_name: &'static str,
        entity_blocking_enabled: bool,
        contradiction_detector_name: &'static str,
    ) -> Self {
        Self {
            pool_options_hash: hash_debug(pool.connect_options().as_ref()),
            pii_service_url: config.memory.pii_service_url.clone(),
            cohere_api_key_configured: !config.providers.cohere.api_key.trim().is_empty(),
            cohere_api_key_hash: hash_debug(&config.providers.cohere.api_key),
            extractor_name,
            entity_resolver_name,
            entity_blocking_enabled,
            contradiction_detector_name,
            memory_config_hash: hash_debug(&config.memory),
            observability_environment: config.observability.environment.clone(),
        }
    }

    fn summary(&self) -> String {
        format!(
            "pool={}, pii={}, cohere_api_key_configured={}, cohere_api_key_hash={}, extractor={}, entity_resolver={}, entity_blocking={}, contradiction={}, memory_config={}, observability_environment={}",
            self.pool_options_hash,
            self.pii_service_url.as_deref().unwrap_or("<none>"),
            self.cohere_api_key_configured,
            self.cohere_api_key_hash,
            self.extractor_name,
            self.entity_resolver_name,
            self.entity_blocking_enabled,
            self.contradiction_detector_name,
            self.memory_config_hash,
            self.observability_environment
                .as_deref()
                .unwrap_or("<none>")
        )
    }
}

/// Scope-specific dependencies used by ingestion helpers.
#[derive(Clone)]
pub struct IngestCtx {
    /// Graph store used for atomic graph writes.
    pub graph: Arc<dyn GraphStore>,
    /// Vector store used for candidate retrieval and vector writes.
    pub vector: Arc<dyn VectorStore>,
    /// Embedder used by ingestion paths that produce vectors.
    pub embedder: Arc<dyn EmbeddingProvider>,
    /// PII classifier used before graph writes.
    pub pii: Arc<dyn PiiClassifier>,
    /// Contradiction detector shared by slow and fast ingestion.
    pub contradict: Arc<dyn ContradictionDetector>,
    /// Fact extractor used before privacy classification and graph writes.
    pub extractor: Arc<dyn FactExtractor>,
    /// Entity resolver used to connect extracted facts to shared entity nodes.
    pub entity_resolver: Arc<EntityResolver>,
    /// Whether slow-path ingestion should enable embedding-blocked entity resolution.
    pub entity_blocking_enabled: bool,
    /// Postgres pool used for sidecar and dedup queries.
    pub pool: PgPool,
}

impl IngestCtx {
    /// Creates an ingestion context from explicit dependencies.
    #[must_use]
    pub fn new(
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
        embedder: Arc<dyn EmbeddingProvider>,
        pii: Arc<dyn PiiClassifier>,
        contradict: Arc<dyn ContradictionDetector>,
    ) -> Self {
        Self::new_with_extractor(
            pool,
            graph,
            vector,
            embedder,
            pii,
            contradict,
            Arc::new(HeuristicFactExtractor),
        )
    }

    /// Creates an ingestion context with an explicit fact extractor.
    #[must_use]
    pub fn new_with_extractor(
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
        embedder: Arc<dyn EmbeddingProvider>,
        pii: Arc<dyn PiiClassifier>,
        contradict: Arc<dyn ContradictionDetector>,
        extractor: Arc<dyn FactExtractor>,
    ) -> Self {
        Self {
            graph,
            vector,
            embedder,
            pii,
            contradict,
            extractor,
            entity_resolver: Arc::new(EntityResolver::deterministic_for_app_role()),
            entity_blocking_enabled: false,
            pool,
        }
    }

    /// Returns a copy of this context that uses the provided fact extractor.
    #[must_use]
    pub fn with_extractor(mut self, extractor: Arc<dyn FactExtractor>) -> Self {
        self.extractor = extractor;
        self
    }

    /// Returns a copy of this context that uses the provided entity resolver.
    #[must_use]
    pub fn with_entity_resolver(mut self, entity_resolver: Arc<EntityResolver>) -> Self {
        self.entity_resolver = entity_resolver;
        self
    }

    /// Returns a copy of this context that uses the provided entity merge verifier.
    #[must_use]
    pub fn with_entity_merge_verifier(self, verifier: Arc<dyn EntityMergeVerifier>) -> Self {
        self.with_entity_resolver(Arc::new(EntityResolver::for_app_role(verifier)))
    }

    /// Returns a copy of this context with embedding-blocked entity resolution enabled or disabled.
    #[must_use]
    pub fn with_entity_embedding_blocking(mut self, enabled: bool) -> Self {
        self.entity_blocking_enabled = enabled;
        self
    }
}

/// Process-local runtime inputs needed by Restate ingestion handlers.
#[derive(Clone)]
pub struct IngestRuntime {
    pool: PgPool,
    pii_service_url: Option<String>,
    cohere_api_key: String,
    extractor: Arc<dyn FactExtractor>,
    extractor_name: &'static str,
    entity_resolver: Arc<EntityResolver>,
    entity_resolver_name: &'static str,
    entity_blocking_enabled: bool,
    contradiction_detector: Arc<dyn ContradictionDetector>,
    contradiction_detector_name: &'static str,
    vector_store_factory: VectorStoreFactory,
    fingerprint: IngestRuntimeFingerprint,
}

impl IngestRuntime {
    /// Creates a runtime from a Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        let default_config = MoaConfig::default();
        let contradiction_detector = Arc::new(RrfPlusJudgeDetector::from_config_or_heuristic(
            &default_config,
        ));
        let extractor_name = "heuristic";
        let entity_resolver_name = "deterministic";
        let entity_blocking_enabled = false;
        let contradiction_detector_name = "rrf_plus_judge";
        let vector_store_factory = VectorStoreFactory::from_config(&default_config);
        let fingerprint = IngestRuntimeFingerprint::new(
            &pool,
            &default_config,
            extractor_name,
            entity_resolver_name,
            entity_blocking_enabled,
            contradiction_detector_name,
        );
        Self {
            pool,
            pii_service_url: None,
            cohere_api_key: default_config.providers.cohere.api_key,
            extractor: Arc::new(HeuristicFactExtractor),
            extractor_name,
            entity_resolver: Arc::new(EntityResolver::deterministic_for_app_role()),
            entity_resolver_name,
            entity_blocking_enabled,
            contradiction_detector,
            contradiction_detector_name,
            vector_store_factory,
            fingerprint,
        }
    }

    /// Creates a runtime from a Postgres pool and shared MOA config.
    #[must_use]
    pub fn from_config(pool: PgPool, config: &MoaConfig) -> Self {
        let (extractor, extractor_name) = extractor_from_config(config);
        let (entity_resolver, entity_resolver_name) = entity_resolver_from_config(config);
        let entity_blocking_enabled = entity_blocking_enabled_from_config(config);
        let contradiction_detector_name = "rrf_plus_judge";
        let fingerprint = IngestRuntimeFingerprint::new(
            &pool,
            config,
            extractor_name,
            entity_resolver_name,
            entity_blocking_enabled,
            contradiction_detector_name,
        );
        Self {
            pool,
            pii_service_url: config.memory.pii_service_url.clone(),
            cohere_api_key: config.providers.cohere.api_key.clone(),
            extractor,
            extractor_name,
            entity_resolver,
            entity_resolver_name,
            entity_blocking_enabled,
            contradiction_detector: Arc::new(RrfPlusJudgeDetector::from_config_or_heuristic(
                config,
            )),
            contradiction_detector_name,
            vector_store_factory: VectorStoreFactory::from_config(config),
            fingerprint,
        }
    }

    /// Returns a copy of this runtime that uses the provided fact extractor.
    #[must_use]
    pub fn with_extractor(mut self, extractor: Arc<dyn FactExtractor>) -> Self {
        self.extractor = extractor;
        self.extractor_name = "custom";
        self.fingerprint.extractor_name = "custom";
        self
    }

    /// Returns a copy of this runtime that uses the provided entity resolver.
    #[must_use]
    pub fn with_entity_resolver(mut self, entity_resolver: Arc<EntityResolver>) -> Self {
        self.entity_resolver = entity_resolver;
        self.entity_resolver_name = "custom";
        self.fingerprint.entity_resolver_name = "custom";
        self
    }

    /// Returns a copy of this runtime with embedding-blocked entity resolution enabled or disabled.
    #[must_use]
    pub fn with_entity_embedding_blocking(mut self, enabled: bool) -> Self {
        self.entity_blocking_enabled = enabled;
        self.fingerprint.entity_blocking_enabled = enabled;
        self
    }

    /// Returns a copy of this runtime that uses the provided entity merge verifier.
    #[must_use]
    pub fn with_entity_merge_verifier(self, verifier: Arc<dyn EntityMergeVerifier>) -> Self {
        self.with_entity_resolver(Arc::new(EntityResolver::for_app_role(verifier)))
    }

    /// Returns a copy of this runtime that uses the provided contradiction detector.
    #[must_use]
    pub fn with_contradiction_detector(mut self, detector: Arc<dyn ContradictionDetector>) -> Self {
        self.contradiction_detector = detector;
        self.contradiction_detector_name = "custom";
        self.fingerprint.contradiction_detector_name = "custom";
        self
    }

    /// Returns the Postgres pool used by ingestion handlers.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the configured PII classifier sidecar URL.
    #[must_use]
    pub fn pii_service_url(&self) -> Option<&str> {
        self.pii_service_url.as_deref()
    }

    /// Returns the configured Cohere API key.
    #[must_use]
    pub fn cohere_api_key(&self) -> &str {
        &self.cohere_api_key
    }

    /// Returns the configured fact extractor.
    #[must_use]
    pub fn extractor(&self) -> Arc<dyn FactExtractor> {
        self.extractor.clone()
    }

    /// Returns the configured extractor kind for startup logging and tests.
    #[must_use]
    pub fn extractor_name(&self) -> &'static str {
        self.extractor_name
    }

    /// Returns the configured entity resolver.
    #[must_use]
    pub fn entity_resolver(&self) -> Arc<EntityResolver> {
        self.entity_resolver.clone()
    }

    /// Returns an embedder for embedding-blocked entity resolution when configured and credentialed.
    #[must_use]
    pub fn entity_blocking_embedder(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        if !self.entity_blocking_enabled {
            return None;
        }
        let api_key = self.cohere_api_key.trim();
        if api_key.is_empty() {
            return None;
        }
        CohereV4Embedder::new(api_key.to_string())
            .ok()
            .map(|embedder| Arc::new(embedder) as Arc<dyn EmbeddingProvider>)
    }

    /// Returns the configured contradiction detector.
    #[must_use]
    pub fn contradiction_detector(&self) -> Arc<dyn ContradictionDetector> {
        self.contradiction_detector.clone()
    }

    /// Returns the configured vector-store factory.
    #[must_use]
    pub fn vector_store_factory(&self) -> VectorStoreFactory {
        self.vector_store_factory.clone()
    }

    fn is_compatible_with(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint
    }

    fn summary(&self) -> String {
        self.fingerprint.summary()
    }
}

fn entity_resolver_from_config(config: &MoaConfig) -> (Arc<EntityResolver>, &'static str) {
    let extraction = &config.memory.extraction;
    if extraction.enabled && !config.providers.cohere.api_key.trim().is_empty() {
        let verifier = LlmEntityMergeVerifier::from_api_key(
            config.providers.cohere.api_key.clone(),
            &extraction.model,
            extraction.timeout_ms,
        );
        tracing::info!(
            model = %extraction.model,
            "memory entity merge verifier installed: llm"
        );
        return (
            Arc::new(EntityResolver::for_app_role(Arc::new(verifier))),
            "llm",
        );
    }
    tracing::info!("memory entity merge verifier installed: deterministic");
    (
        Arc::new(EntityResolver::deterministic_for_app_role()),
        "deterministic",
    )
}

fn entity_blocking_enabled_from_config(config: &MoaConfig) -> bool {
    let enabled = !config.providers.cohere.api_key.trim().is_empty();
    if enabled {
        tracing::info!("memory entity embedding block installed: cohere");
    } else {
        tracing::info!("memory entity embedding block disabled; credential is absent");
    }
    enabled
}

fn extractor_from_config(config: &MoaConfig) -> (Arc<dyn FactExtractor>, &'static str) {
    let extraction = &config.memory.extraction;
    if !extraction.enabled {
        tracing::info!("memory fact extractor installed: heuristic");
        return (Arc::new(HeuristicFactExtractor), "heuristic");
    }
    let mut extraction = extraction.clone();
    if extraction.api_key.trim().is_empty() {
        extraction.api_key = config.providers.cohere.api_key.clone();
    }
    if extraction.api_key.trim().is_empty() {
        tracing::warn!(
            "memory extraction enabled but credential is absent; installing heuristic extractor"
        );
        return (Arc::new(HeuristicFactExtractor), "heuristic");
    }
    match LlmFactExtractor::from_config(&extraction) {
        Ok(extractor) => {
            tracing::info!(
                model = %extraction.model,
                max_facts_per_chunk = extraction.max_facts_per_chunk,
                "memory fact extractor installed: llm"
            );
            (Arc::new(extractor), "llm")
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "memory extraction enabled but LLM extractor could not initialize; installing heuristic extractor"
            );
            (Arc::new(HeuristicFactExtractor), "heuristic")
        }
    }
}

/// Installs the process-local ingestion runtime.
pub fn install_runtime(
    runtime: IngestRuntime,
) -> std::result::Result<(), IngestRuntimeInstallError> {
    match INGEST_RUNTIME.get() {
        Some(installed) if installed.is_compatible_with(&runtime) => Ok(()),
        Some(installed) => Err(IngestRuntimeInstallError::IncompatibleRuntime {
            installed: installed.summary(),
            requested: runtime.summary(),
        }),
        None => match INGEST_RUNTIME.set(runtime) {
            Ok(()) => Ok(()),
            Err(runtime) => {
                let requested = runtime.summary();
                let Some(installed) = INGEST_RUNTIME.get() else {
                    return Err(IngestRuntimeInstallError::IncompatibleRuntime {
                        installed: "<missing after OnceLock set race>".to_string(),
                        requested,
                    });
                };
                if installed.is_compatible_with(&runtime) {
                    Ok(())
                } else {
                    Err(IngestRuntimeInstallError::IncompatibleRuntime {
                        installed: installed.summary(),
                        requested,
                    })
                }
            }
        },
    }
}

/// Installs the process-local ingestion runtime from a Postgres pool.
pub fn install_runtime_with_pool(
    pool: PgPool,
) -> std::result::Result<(), IngestRuntimeInstallError> {
    install_runtime(IngestRuntime::new(pool))
}

/// Installs the process-local ingestion runtime from a Postgres pool and shared config.
pub fn install_runtime_with_config(
    pool: PgPool,
    config: &MoaConfig,
) -> std::result::Result<(), IngestRuntimeInstallError> {
    install_runtime(IngestRuntime::from_config(pool, config))
}

/// Returns the installed process-local ingestion runtime.
pub fn current_runtime() -> Result<IngestRuntime> {
    INGEST_RUNTIME
        .get()
        .cloned()
        .ok_or(IngestError::RuntimeNotInstalled)
}

fn hash_debug(value: &impl std::fmt::Debug) -> u64 {
    let mut hasher = DefaultHasher::new();
    format!("{value:?}").hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use moa_core::MoaConfig;
    use sqlx::postgres::PgPoolOptions;

    use super::{IngestRuntime, install_runtime};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[tokio::test]
    async fn ingest_runtime_reuses_contradiction_detector() {
        // Pins: runtime-backed ingestion paths share one detector allocation and its judge cache.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let runtime = IngestRuntime::new(pool);

        let first = runtime.contradiction_detector();
        let second = runtime.contradiction_detector();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn runtime_installs_llm_extractor_only_when_enabled_and_credentialed() {
        // Pins: model-backed extraction is gated by both config and a direct configured credential.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let mut config = MoaConfig::default();
        config.memory.extraction.enabled = true;

        let missing = IngestRuntime::from_config(pool.clone(), &config);
        assert_eq!(missing.extractor_name(), "heuristic");

        config.providers.cohere.api_key = "test-key".to_string();
        let credentialed = IngestRuntime::from_config(pool.clone(), &config);
        assert_eq!(credentialed.extractor_name(), "llm");

        config.memory.extraction.enabled = false;
        let disabled = IngestRuntime::from_config(pool, &config);
        assert_eq!(disabled.extractor_name(), "heuristic");
    }

    #[tokio::test]
    async fn incompatible_runtime_install_fails_clearly() {
        // Pins: orchestrator startup fails instead of silently keeping a stale ingest runtime.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let mut config = MoaConfig::default();
        config.memory.pii_service_url = Some("http://pii-a.test".to_string());
        let installed = IngestRuntime::from_config(pool.clone(), &config);

        install_runtime(installed.clone()).expect("first runtime install should succeed");
        install_runtime(installed).expect("compatible runtime reinstall should be idempotent");

        config.memory.pii_service_url = Some("http://pii-b.test".to_string());
        let requested = IngestRuntime::from_config(pool, &config);
        let error = install_runtime(requested)
            .expect_err("incompatible runtime install should fail clearly");

        assert!(
            error
                .to_string()
                .contains("ingestion runtime already installed with incompatible dependencies")
        );
        assert!(error.to_string().contains("pii=http://pii-a.test"));
        assert!(error.to_string().contains("pii=http://pii-b.test"));
    }
}
