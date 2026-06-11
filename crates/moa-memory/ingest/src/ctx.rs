//! Runtime context installed by hosts that execute graph-memory ingestion.

use std::sync::{Arc, OnceLock};

use moa_core::{MoaConfig, traits::EmbeddingProvider};
use moa_memory_graph::GraphStore;
use moa_memory_pii::PiiClassifier;
use moa_memory_vector::CohereV4Embedder;
use moa_memory_vector::VectorStore;
use secrecy::SecretString;
use sqlx::PgPool;

use crate::{
    ContradictionDetector, EntityMergeVerifier, EntityResolver, FactExtractor,
    HeuristicFactExtractor, IngestError, LlmEntityMergeVerifier, LlmFactExtractor, Result,
    RrfPlusJudgeDetector,
};

static INGEST_RUNTIME: OnceLock<IngestRuntime> = OnceLock::new();

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
    cohere_api_key_env: String,
    extractor: Arc<dyn FactExtractor>,
    extractor_name: &'static str,
    entity_resolver: Arc<EntityResolver>,
    entity_blocking_enabled: bool,
    contradiction_detector: Arc<dyn ContradictionDetector>,
}

impl IngestRuntime {
    /// Creates a runtime from a Postgres pool.
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        let default_config = MoaConfig::default();
        let contradiction_detector = Arc::new(RrfPlusJudgeDetector::from_config_or_heuristic(
            &default_config,
        ));
        Self {
            pool,
            pii_service_url: None,
            cohere_api_key_env: default_config.memory.vector.embedder.cohere.api_key_env,
            extractor: Arc::new(HeuristicFactExtractor),
            extractor_name: "heuristic",
            entity_resolver: Arc::new(EntityResolver::deterministic_for_app_role()),
            entity_blocking_enabled: false,
            contradiction_detector,
        }
    }

    /// Creates a runtime from a Postgres pool and shared MOA config.
    #[must_use]
    pub fn from_config(pool: PgPool, config: &MoaConfig) -> Self {
        let (extractor, extractor_name) = extractor_from_config(config);
        let entity_resolver = entity_resolver_from_config(config);
        let entity_blocking_enabled = entity_blocking_enabled_from_config(config);
        Self {
            pool,
            pii_service_url: config.memory.pii_service_url.clone(),
            cohere_api_key_env: config.memory.vector.embedder.cohere.api_key_env.clone(),
            extractor,
            extractor_name,
            entity_resolver,
            entity_blocking_enabled,
            contradiction_detector: Arc::new(RrfPlusJudgeDetector::from_config_or_heuristic(
                config,
            )),
        }
    }

    /// Returns a copy of this runtime that uses the provided fact extractor.
    #[must_use]
    pub fn with_extractor(mut self, extractor: Arc<dyn FactExtractor>) -> Self {
        self.extractor = extractor;
        self.extractor_name = "custom";
        self
    }

    /// Returns a copy of this runtime that uses the provided entity resolver.
    #[must_use]
    pub fn with_entity_resolver(mut self, entity_resolver: Arc<EntityResolver>) -> Self {
        self.entity_resolver = entity_resolver;
        self
    }

    /// Returns a copy of this runtime with embedding-blocked entity resolution enabled or disabled.
    #[must_use]
    pub fn with_entity_embedding_blocking(mut self, enabled: bool) -> Self {
        self.entity_blocking_enabled = enabled;
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

    /// Returns the configured Cohere API-key environment variable name.
    #[must_use]
    pub fn cohere_api_key_env(&self) -> &str {
        &self.cohere_api_key_env
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
        let api_key = std::env::var(&self.cohere_api_key_env).ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        Some(Arc::new(CohereV4Embedder::new(SecretString::from(api_key))))
    }

    /// Returns the configured contradiction detector.
    #[must_use]
    pub fn contradiction_detector(&self) -> Arc<dyn ContradictionDetector> {
        self.contradiction_detector.clone()
    }
}

fn entity_resolver_from_config(config: &MoaConfig) -> Arc<EntityResolver> {
    let extraction = &config.memory.extraction;
    if extraction.enabled
        && std::env::var(&extraction.api_key_env).is_ok()
        && let Ok(verifier) = LlmEntityMergeVerifier::from_env(
            &extraction.api_key_env,
            &extraction.model,
            extraction.timeout_ms,
        )
    {
        tracing::info!(
            model = %extraction.model,
            "memory entity merge verifier installed: llm"
        );
        return Arc::new(EntityResolver::for_app_role(Arc::new(verifier)));
    }
    tracing::info!("memory entity merge verifier installed: deterministic");
    Arc::new(EntityResolver::deterministic_for_app_role())
}

fn entity_blocking_enabled_from_config(config: &MoaConfig) -> bool {
    let api_key_env = &config.memory.vector.embedder.cohere.api_key_env;
    let enabled = std::env::var(api_key_env)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if enabled {
        tracing::info!("memory entity embedding block installed: cohere");
    } else {
        tracing::info!(
            api_key_env = %api_key_env,
            "memory entity embedding block disabled; credential env var is absent"
        );
    }
    enabled
}

fn extractor_from_config(config: &MoaConfig) -> (Arc<dyn FactExtractor>, &'static str) {
    let extraction = &config.memory.extraction;
    if !extraction.enabled {
        tracing::info!("memory fact extractor installed: heuristic");
        return (Arc::new(HeuristicFactExtractor), "heuristic");
    }
    if std::env::var(&extraction.api_key_env).is_err() {
        tracing::warn!(
            api_key_env = %extraction.api_key_env,
            "memory extraction enabled but credential env var is absent; installing heuristic extractor"
        );
        return (Arc::new(HeuristicFactExtractor), "heuristic");
    }
    match LlmFactExtractor::from_config(extraction) {
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
pub fn install_runtime(runtime: IngestRuntime) -> std::result::Result<(), IngestRuntime> {
    INGEST_RUNTIME.set(runtime)
}

/// Installs the process-local ingestion runtime from a Postgres pool.
pub fn install_runtime_with_pool(pool: PgPool) -> std::result::Result<(), IngestRuntime> {
    install_runtime(IngestRuntime::new(pool))
}

/// Installs the process-local ingestion runtime from a Postgres pool and shared config.
pub fn install_runtime_with_config(
    pool: PgPool,
    config: &MoaConfig,
) -> std::result::Result<(), IngestRuntime> {
    install_runtime(IngestRuntime::from_config(pool, config))
}

/// Returns the installed process-local ingestion runtime.
pub fn current_runtime() -> Result<IngestRuntime> {
    INGEST_RUNTIME
        .get()
        .cloned()
        .ok_or(IngestError::RuntimeNotInstalled)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;

    use moa_core::MoaConfig;
    use sqlx::postgres::PgPoolOptions;

    use super::IngestRuntime;

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
        // Pins: model-backed extraction is gated by both config and the configured credential env.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let mut config = MoaConfig::default();
        config.memory.extraction.enabled = true;
        config.memory.extraction.api_key_env = "MOA_TEST_EXTRACTION_KEY".to_string();
        unsafe {
            std::env::remove_var("MOA_TEST_EXTRACTION_KEY");
        }

        let missing = IngestRuntime::from_config(pool.clone(), &config);
        assert_eq!(missing.extractor_name(), "heuristic");

        unsafe {
            std::env::set_var("MOA_TEST_EXTRACTION_KEY", "test-key");
        }
        let credentialed = IngestRuntime::from_config(pool.clone(), &config);
        assert_eq!(credentialed.extractor_name(), "llm");

        config.memory.extraction.enabled = false;
        let disabled = IngestRuntime::from_config(pool, &config);
        assert_eq!(disabled.extractor_name(), "heuristic");

        unsafe {
            std::env::remove_var("MOA_TEST_EXTRACTION_KEY");
        }
    }
}
