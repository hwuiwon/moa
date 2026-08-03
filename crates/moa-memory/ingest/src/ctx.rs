//! Explicit runtime dependencies for hosts that execute graph-memory ingestion.

use std::sync::Arc;

use moa_config::MoaConfig;
use moa_core::{error::MoaError, traits::EmbeddingProvider};
use moa_crypto::KeyManagementProvider;
use moa_memory_graph::GraphStore;
use moa_memory_pii::{HeuristicPiiClassifier, OpenAiPrivacyFilterClassifier, PiiClassifier};
use moa_memory_vector::{VectorStore, VectorStoreFactory};
use moa_providers::{EmbedderConstructionRole, build_embedder_from_config};
use sqlx::PgPool;

use crate::model_client::resolved_extraction_config;
use crate::{
    ContradictionDetector, EntityMergeVerifier, EntityResolver, FactExtractor,
    HeuristicFactExtractor, ModelEntityMergeVerifier, ModelFactExtractor, RrfPlusJudgeDetector,
};

/// Error returned when constructing an ingestion runtime fails.
#[derive(Debug, thiserror::Error)]
pub enum IngestRuntimeError {
    /// Runtime configuration could not construct a required dependency.
    #[error("ingestion runtime configuration failed: {0}")]
    Configuration(#[from] MoaError),
}

/// Scope-specific dependencies used by ingestion helpers.
#[derive(Clone)]
pub struct IngestCtx {
    /// Shared KMS used by every scope-specific graph store created for this ingest.
    pub kms: Arc<dyn KeyManagementProvider>,
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
        kms: Arc<dyn KeyManagementProvider>,
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
        embedder: Arc<dyn EmbeddingProvider>,
        pii: Arc<dyn PiiClassifier>,
        contradict: Arc<dyn ContradictionDetector>,
    ) -> Self {
        Self::new_with_extractor(
            pool,
            kms,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_extractor(
        pool: PgPool,
        kms: Arc<dyn KeyManagementProvider>,
        graph: Arc<dyn GraphStore>,
        vector: Arc<dyn VectorStore>,
        embedder: Arc<dyn EmbeddingProvider>,
        pii: Arc<dyn PiiClassifier>,
        contradict: Arc<dyn ContradictionDetector>,
        extractor: Arc<dyn FactExtractor>,
    ) -> Self {
        Self {
            kms,
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

/// Host-owned runtime inputs shared by slow and fast ingestion adapters.
#[derive(Clone)]
pub struct IngestRuntime {
    pool: PgPool,
    kms: Arc<dyn KeyManagementProvider>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    pii_classifier: Arc<dyn PiiClassifier>,
    extractor: Arc<dyn FactExtractor>,
    extractor_name: &'static str,
    entity_resolver: Arc<EntityResolver>,
    entity_blocking_enabled: bool,
    contradiction_detector: Arc<dyn ContradictionDetector>,
    vector_store_factory: VectorStoreFactory,
    fact_extraction_enabled: bool,
}

impl IngestRuntime {
    /// Creates a runtime from a Postgres pool.
    ///
    /// # Errors
    ///
    /// Returns an error when the default embedder selector is invalid or its
    /// client cannot be constructed. Missing credentials keep ingestion in
    /// no-vector mode.
    pub fn new(
        pool: PgPool,
        kms: Arc<dyn KeyManagementProvider>,
    ) -> std::result::Result<Self, IngestRuntimeError> {
        let default_config = MoaConfig::default();
        Self::from_config(pool, kms, &default_config)
    }

    /// Creates a runtime from a Postgres pool and shared MOA config.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid embedder selectors, models, dimensions, or
    /// client construction. Disabled selection and missing selected-provider
    /// credentials construct a no-vector runtime instead.
    pub fn from_config(
        pool: PgPool,
        kms: Arc<dyn KeyManagementProvider>,
        config: &MoaConfig,
    ) -> std::result::Result<Self, IngestRuntimeError> {
        let (extractor, extractor_name) = extractor_from_config(config);
        let (entity_resolver, _) = entity_resolver_from_config(config);
        let embedder = build_configured_ingestion_embedder(config)?;
        let entity_blocking_enabled = embedder.is_some();
        if let Some(provider) = embedder.as_deref() {
            tracing::info!(
                model = provider.model_id(),
                model_version = provider.model_version(),
                dimensions = provider.dimensions(),
                "memory entity embedding block installed"
            );
        }
        let pii_classifier = build_shared_pii_classifier(config.memory.pii_service_url.as_deref());
        Ok(Self {
            pool,
            kms,
            embedder,
            pii_classifier,
            extractor,
            extractor_name,
            entity_resolver,
            entity_blocking_enabled,
            contradiction_detector: Arc::new(RrfPlusJudgeDetector::from_config_or_heuristic(
                config,
            )),
            vector_store_factory: VectorStoreFactory::from_config(config),
            fact_extraction_enabled: config.memory.extraction.enabled,
        })
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

    /// Returns a copy of this runtime with explicit fast-path classification and
    /// embedding dependencies.
    ///
    /// This dependency-injection seam keeps exported fast-memory integration
    /// tests hermetic while preserving the same runtime-owned entry point used
    /// by production hosts.
    #[must_use]
    pub fn with_fast_path_dependencies(
        mut self,
        embedder: Arc<dyn EmbeddingProvider>,
        pii_classifier: Arc<dyn PiiClassifier>,
    ) -> Self {
        self.embedder = Some(embedder);
        self.pii_classifier = pii_classifier;
        self
    }

    /// Returns a copy of this runtime that uses the provided entity merge verifier.
    #[must_use]
    pub fn with_entity_merge_verifier(self, verifier: Arc<dyn EntityMergeVerifier>) -> Self {
        self.with_entity_resolver(Arc::new(EntityResolver::for_app_role(verifier)))
    }

    /// Returns the Postgres pool used by ingestion handlers.
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Returns the process-shared KMS used by all ingestion graph stores.
    #[must_use]
    pub fn kms(&self) -> Arc<dyn KeyManagementProvider> {
        self.kms.clone()
    }

    /// Returns the process-shared fact embedder, when a credential is configured.
    ///
    /// The embedder owns a pooled HTTP client and is built once at runtime
    /// construction so ingestion steps reuse it instead of rebuilding a client
    /// per turn.
    #[must_use]
    pub fn embedder(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embedder.clone()
    }

    /// Returns the process-shared PII classifier.
    ///
    /// Resolves to the `openai/privacy-filter` sidecar client when a service URL
    /// is configured and otherwise to the deterministic heuristic classifier.
    /// Built once at runtime construction so its pooled HTTP client is reused.
    #[must_use]
    pub fn pii_classifier(&self) -> Arc<dyn PiiClassifier> {
        self.pii_classifier.clone()
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
    ///
    /// Reuses the process-shared embedder rather than constructing a new HTTP
    /// client per turn.
    #[must_use]
    pub fn entity_blocking_embedder(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        if !self.entity_blocking_enabled {
            return None;
        }
        self.embedder.clone()
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

    /// Returns whether config-level memory learning (fact extraction) is enabled.
    ///
    /// This is the single off-switch that also gates background incident capture:
    /// deployments with memory learning disabled record no negative-results nodes.
    #[must_use]
    pub fn fact_extraction_enabled(&self) -> bool {
        self.fact_extraction_enabled
    }
}

fn entity_resolver_from_config(config: &MoaConfig) -> (Arc<EntityResolver>, &'static str) {
    if resolved_extraction_config(config).is_some() {
        match ModelEntityMergeVerifier::from_config(config) {
            Ok(verifier) => {
                tracing::info!(
                    model = %config.memory.extraction.model,
                    "memory entity merge verifier installed: model"
                );
                return (
                    Arc::new(EntityResolver::for_app_role(Arc::new(verifier))),
                    "model",
                );
            }
            Err(error) => tracing::warn!(
                error = %error,
                "memory entity merge verifier could not initialize; installing deterministic verifier"
            ),
        }
    }
    tracing::info!("memory entity merge verifier installed: deterministic");
    (
        Arc::new(EntityResolver::deterministic_for_app_role()),
        "deterministic",
    )
}

/// Builds the configured session-ingestion embedder without crossing vector spaces.
///
/// Disabled selection and a missing selected-provider credential return
/// `Ok(None)` after one structured warning so slow ingestion can persist facts
/// without vectors. Every other factory error is propagated.
pub(crate) fn build_configured_ingestion_embedder(
    config: &MoaConfig,
) -> moa_core::error::Result<Option<Arc<dyn EmbeddingProvider>>> {
    let selector = config.memory.vector.embedder.name.trim();
    if selector.is_empty() || selector.eq_ignore_ascii_case("disabled") {
        tracing::warn!(
            selector,
            credential_field = "<disabled>",
            reason = "disabled",
            "configured ingestion embedder unavailable; slow ingestion will store facts without vectors"
        );
        return Ok(None);
    }

    match build_embedder_from_config(config, None, EmbedderConstructionRole::Ingestion) {
        Ok(embedder) => Ok(Some(embedder)),
        Err(MoaError::MissingEnvironmentVariable(credential_field)) => {
            tracing::warn!(
                selector,
                credential_field,
                reason = "missing_credential",
                "configured ingestion embedder unavailable; slow ingestion will store facts without vectors"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Builds the process-shared PII classifier from an optional sidecar URL.
///
/// Falls back to the deterministic heuristic classifier when no URL is
/// configured or the sidecar HTTP client cannot be constructed.
pub(crate) fn build_shared_pii_classifier(pii_service_url: Option<&str>) -> Arc<dyn PiiClassifier> {
    if let Some(url) = pii_service_url.filter(|url| !url.trim().is_empty()) {
        match OpenAiPrivacyFilterClassifier::new(url.to_string()) {
            Ok(classifier) => return Arc::new(classifier) as Arc<dyn PiiClassifier>,
            Err(error) => tracing::warn!(
                error = %error,
                "pii classifier client init failed; falling back to heuristic classifier"
            ),
        }
    }
    Arc::new(HeuristicPiiClassifier)
}

fn extractor_from_config(config: &MoaConfig) -> (Arc<dyn FactExtractor>, &'static str) {
    if !config.memory.extraction.enabled {
        tracing::info!("memory fact extractor installed: heuristic");
        return (Arc::new(HeuristicFactExtractor), "heuristic");
    }
    if resolved_extraction_config(config).is_none() {
        return (Arc::new(HeuristicFactExtractor), "heuristic");
    }
    match ModelFactExtractor::from_config(config) {
        Ok(extractor) => {
            tracing::info!(
                model = %config.memory.extraction.model,
                max_facts_per_chunk = config.memory.extraction.max_facts_per_chunk,
                "memory fact extractor installed: model"
            );
            (Arc::new(extractor), "model")
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "memory extraction enabled but model extractor could not initialize; installing heuristic extractor"
            );
            (Arc::new(HeuristicFactExtractor), "heuristic")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, OnceLock};

    use moa_config::MoaConfig;
    use moa_core::error::MoaError;
    use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
    use sqlx::postgres::PgPoolOptions;
    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    use super::{IngestRuntime, IngestRuntimeError};

    fn test_kms() -> Arc<dyn KeyManagementProvider> {
        static KMS: OnceLock<Arc<dyn KeyManagementProvider>> = OnceLock::new();
        KMS.get_or_init(|| Arc::new(LocalKmsProvider::new()))
            .clone()
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct CapturedWarning {
        fields: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct WarningSubscriber {
        warnings: Arc<Mutex<Vec<CapturedWarning>>>,
    }

    impl Subscriber for WarningSubscriber {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = WarningVisitor::default();
            event.record(&mut visitor);
            self.warnings
                .lock()
                .expect("warning capture lock")
                .push(CapturedWarning {
                    fields: visitor.fields,
                });
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    #[derive(Default)]
    struct WarningVisitor {
        fields: BTreeMap<String, String>,
    }

    impl Visit for WarningVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }

    fn lazy_pool() -> sqlx::PgPool {
        PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect")
    }

    #[tokio::test]
    async fn runtime_uses_configured_ingestion_embedder() {
        // Pins: production session ingestion follows memory.vector.embedder instead
        // of silently constructing Cohere, and entity blocking shares that client.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "gemini:gemini-embedding-2".to_string();
        config.memory.vector.embedder.output_dim = 1_024;
        config.providers.google.api_key = "test-google-key".to_string();

        let runtime = IngestRuntime::from_config(lazy_pool(), test_kms(), &config)
            .expect("configured Gemini ingestion runtime should build without a provider call");
        let fact_embedder = runtime
            .embedder()
            .expect("configured ingestion embedder should be available");
        let entity_embedder = runtime
            .entity_blocking_embedder()
            .expect("entity blocking should use the configured ingestion embedder");

        assert_eq!(fact_embedder.model_id(), "gemini-embedding-2");
        assert_eq!(fact_embedder.model_version(), 2);
        assert_eq!(fact_embedder.dimensions(), 1_024);
        assert!(Arc::ptr_eq(&fact_embedder, &entity_embedder));
    }

    #[tokio::test]
    async fn runtime_without_selected_provider_credentials_has_no_ingestion_embedder() {
        // Pins: a missing selected-provider credential emits exactly one structured
        // warning and leaves slow ingestion in its explicit no-vector mode.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "gemini:gemini-embedding-2".to_string();
        config.providers.google.api_key.clear();
        // Isolate the embedder diagnostic: the default reranker also warns at
        // construction when its provider key is absent in the test env.
        config.memory.retrieval.reranker_model = "noop".to_string();
        let subscriber = WarningSubscriber::default();
        let captured = subscriber.warnings.clone();

        let runtime = tracing::subscriber::with_default(subscriber, || {
            IngestRuntime::from_config(lazy_pool(), test_kms(), &config)
        })
        .expect("missing selected credential should preserve no-vector ingestion");

        assert!(runtime.embedder().is_none());
        assert!(runtime.entity_blocking_embedder().is_none());
        let warnings = captured.lock().expect("warning capture lock");
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].fields.get("selector").map(String::as_str),
            Some("gemini:gemini-embedding-2")
        );
        assert_eq!(
            warnings[0]
                .fields
                .get("credential_field")
                .map(String::as_str),
            Some("MOA_GOOGLE_API_KEY")
        );
        assert_eq!(
            warnings[0].fields.get("reason").map(String::as_str),
            Some("missing_credential")
        );
    }

    #[tokio::test]
    async fn disabled_ingestion_embedder_emits_one_warning_and_stays_unavailable() {
        // Pins: the explicit disabled selector has the same slow no-vector
        // boundary as missing credentials and is diagnosed once at construction.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "disabled".to_string();
        // Isolate the embedder diagnostic from the default reranker's own
        // missing-credential warning.
        config.memory.retrieval.reranker_model = "noop".to_string();
        let subscriber = WarningSubscriber::default();
        let captured = subscriber.warnings.clone();

        let runtime = tracing::subscriber::with_default(subscriber, || {
            IngestRuntime::from_config(lazy_pool(), test_kms(), &config)
        })
        .expect("disabled ingestion embedder should preserve no-vector ingestion");

        assert!(runtime.embedder().is_none());
        let warnings = captured.lock().expect("warning capture lock");
        assert_eq!(warnings.len(), 1);
        assert_eq!(
            warnings[0].fields.get("selector").map(String::as_str),
            Some("disabled")
        );
        assert_eq!(
            warnings[0]
                .fields
                .get("credential_field")
                .map(String::as_str),
            Some("<disabled>")
        );
        assert_eq!(
            warnings[0].fields.get("reason").map(String::as_str),
            Some("disabled")
        );
    }

    #[tokio::test]
    async fn invalid_ingestion_embedder_configuration_is_propagated() {
        // Pins: an invalid selected model is a startup configuration error, not a
        // silent no-vector downgrade or a cross-provider fallback.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "gemini:not-a-real-model".to_string();
        config.providers.google.api_key = "test-google-key".to_string();

        let Err(error) = IngestRuntime::from_config(lazy_pool(), test_kms(), &config) else {
            panic!("invalid selected ingestion model should fail runtime construction");
        };

        assert!(matches!(
            error,
            IngestRuntimeError::Configuration(MoaError::ConfigError(message))
                if message == "gemini embedder only supports gemini-embedding-2, got not-a-real-model"
        ));
    }

    #[tokio::test]
    async fn ingest_runtime_reuses_contradiction_detector() {
        // Pins: runtime-backed ingestion paths share one detector allocation and its judge cache.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let runtime = IngestRuntime::new(pool, test_kms()).expect("default runtime should build");

        let first = runtime.contradiction_detector();
        let second = runtime.contradiction_detector();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn runtime_installs_model_extractor_only_when_enabled_and_provider_configured() {
        // Pins: model-backed extraction is gated by both config and a direct configured credential.
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://localhost/moa_test")
            .expect("lazy test pool should not connect");
        let mut config = MoaConfig::default();
        config.memory.extraction.enabled = true;

        let missing = IngestRuntime::from_config(pool.clone(), test_kms(), &config)
            .expect("missing extraction credential should keep the heuristic extractor");
        assert_eq!(missing.extractor_name(), "heuristic");

        config.providers.openai.api_key = "test-openai-key".to_string();
        let credentialed = IngestRuntime::from_config(pool.clone(), test_kms(), &config)
            .expect("configured extraction credential should build the runtime");
        assert_eq!(credentialed.extractor_name(), "model");

        config.memory.extraction.enabled = false;
        let disabled = IngestRuntime::from_config(pool, test_kms(), &config)
            .expect("disabled extraction should build the runtime");
        assert_eq!(disabled.extractor_name(), "heuristic");
    }

    #[test]
    fn entity_resolver_uses_provider_backed_memory_extraction_model() {
        // Pins: merge verification follows the same provider-backed model path as fact extraction.
        let mut config = MoaConfig::default();
        config.memory.extraction.enabled = true;
        config.providers.openai.api_key = "test-openai-key".to_string();

        let (_resolver, resolver_name) = super::entity_resolver_from_config(&config);

        assert_eq!(resolver_name, "model");
    }
}
