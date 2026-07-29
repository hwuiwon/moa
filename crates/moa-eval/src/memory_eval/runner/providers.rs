//! Provider construction, throttling, and cost-budget accounting.

use super::*;

pub(super) struct RunProviders {
    pub(super) embedder: Arc<dyn EmbeddingProvider>,
    pub(super) extractor: Arc<dyn FactExtractor>,
    pub(super) entity_merge_verifier: Arc<dyn EntityMergeVerifier>,
    pub(super) reranker: Arc<dyn Reranker>,
    pub(super) entity_blocking_enabled: bool,
    pub(super) deterministic_replay: bool,
    pub(super) ledger: SharedCostLedger,
    pub(super) provenance: ProviderProvenance,
}

impl MemoryRetrievalEvalOptions {
    pub(super) async fn providers_for_corpus(
        &self,
        corpus: &LoadedMemoryEvalCorpus,
    ) -> Result<RunProviders> {
        match self.lane {
            EvalLane::Pr => self.pr_providers_for_corpus(corpus).await,
            EvalLane::Live => self.live_providers_for_corpus(corpus).await,
        }
    }

    async fn pr_providers_for_corpus(
        &self,
        corpus: &LoadedMemoryEvalCorpus,
    ) -> Result<RunProviders> {
        let extractor = self.extractor_for_corpus(corpus)?;
        let embedder =
            Arc::new(cached_embedding_provider_for_corpus(corpus, extractor.as_ref()).await?);
        let entity_merge_verifier = self.entity_merge_verifier_for_corpus(corpus)?;
        let entity_blocking_enabled = self.extractor_mode == MemoryEvalExtractorMode::Recorded;
        let ledger = Arc::new(tokio::sync::Mutex::new(CostLedger::new(0.0)));
        let provenance = ProviderProvenance {
            lane: "pr".to_string(),
            embedding_model: embedder.model_id().to_string(),
            embedding_model_version: embedder.model_version(),
            extractor_model: match self.extractor_mode {
                MemoryEvalExtractorMode::Heuristic => "heuristic".to_string(),
                MemoryEvalExtractorMode::Recorded => "recorded-extraction-fixtures".to_string(),
            },
            extraction_prompt_version: (self.extractor_mode == MemoryEvalExtractorMode::Recorded)
                .then(|| EXTRACTION_PROMPT_VERSION.to_string()),
            merge_verifier_model: match self.extractor_mode {
                MemoryEvalExtractorMode::Heuristic => "deterministic".to_string(),
                MemoryEvalExtractorMode::Recorded => "recorded-merge-fixtures".to_string(),
            },
            merge_prompt_version: (self.extractor_mode == MemoryEvalExtractorMode::Recorded)
                .then(|| MERGE_PROMPT_VERSION.to_string()),
            reranker_model: "noop".to_string(),
        };
        Ok(RunProviders {
            embedder,
            extractor,
            entity_merge_verifier,
            reranker: Arc::new(NoopReranker),
            entity_blocking_enabled,
            deterministic_replay: self.extractor_mode == MemoryEvalExtractorMode::Recorded,
            ledger,
            provenance,
        })
    }

    async fn live_providers_for_corpus(
        &self,
        corpus: &LoadedMemoryEvalCorpus,
    ) -> Result<RunProviders> {
        let mut config = MoaConfig::load_from_env().map_err(|error| {
            EvalError::InvalidConfig(format!("failed to load MOA config for live eval: {error}"))
        })?;
        config.memory.extraction.enabled = true;
        if self.reranker_enabled
            && config.memory.retrieval.reranker_model.trim() == moa_providers::NOOP_RERANK_MODEL
        {
            config.memory.retrieval.reranker_model =
                format!("cohere:{COHERE_DEFAULT_RERANK_MODEL}");
        }
        let extraction = config.memory.extraction.clone();
        let budget = self
            .budget_usd
            .unwrap_or_else(|| default_live_budget_usd(corpus.manifest.profile));
        let ledger = Arc::new(tokio::sync::Mutex::new(CostLedger::new(budget)));
        let chat_throttle = Arc::new(LiveChatThrottle::new(Duration::from_millis(3_200)));
        let embed_throttle = Arc::new(LiveChatThrottle::new(Duration::from_millis(700)));
        let rerank_throttle = Arc::new(LiveChatThrottle::new(Duration::from_millis(6_500)));
        let raw_embedder =
            build_embedder_from_config(&config, None, EmbedderConstructionRole::Retrieval)
                .map_err(|error| {
                    EvalError::InvalidConfig(format!("failed to initialize live embedder: {error}"))
                })?;
        let embedding_model = raw_embedder.model_id().to_string();
        let embedding_model_version = raw_embedder.model_version();
        let embedder = Arc::new(ThrottledEmbedder::new(
            CountingEmbedder::new(SharedEmbeddingProvider(raw_embedder), ledger.clone()),
            embed_throttle,
        )) as Arc<dyn EmbeddingProvider>;
        let live_extractor = ModelFactExtractor::from_config(&config).map_err(|error| {
            EvalError::InvalidConfig(format!("failed to initialize live fact extractor: {error}"))
        })?;
        let extractor = Arc::new(MemoizedThrottledFactExtractor::new(
            CountingExtractor::new(live_extractor, ledger.clone()),
            chat_throttle.clone(),
        )) as Arc<dyn FactExtractor>;
        let live_merge_verifier =
            ModelEntityMergeVerifier::from_config(&config).map_err(|error| {
                EvalError::InvalidConfig(format!(
                    "failed to initialize live merge verifier: {error}"
                ))
            })?;
        let entity_merge_verifier = Arc::new(ThrottledMergeVerifier::new(
            CountingMergeVerifier::new(live_merge_verifier, ledger.clone()),
            chat_throttle,
        )) as Arc<dyn EntityMergeVerifier>;
        let configured_reranker = if self.reranker_enabled {
            let configured = build_reranker_from_config(&config, None).map_err(|error| {
                EvalError::InvalidConfig(format!("failed to initialize live reranker: {error}"))
            })?;
            if configured.provider == "noop" {
                return Err(EvalError::InvalidConfig(
                    "live eval reranker is enabled but no configured reranker provider is available"
                        .to_string(),
                ));
            }
            configured
        } else {
            ConfiguredReranker::noop()
        };
        let reranker_model = configured_reranker.model.clone();
        let reranker: Arc<dyn Reranker> = if self.reranker_enabled {
            Arc::new(ThrottledReranker::new(
                CountingReranker::new(SharedReranker(configured_reranker.reranker), ledger.clone()),
                rerank_throttle,
            ))
        } else {
            Arc::new(NoopReranker)
        };
        let provenance = ProviderProvenance {
            lane: "live".to_string(),
            embedding_model,
            embedding_model_version,
            extractor_model: extraction.model.clone(),
            extraction_prompt_version: Some(EXTRACTION_PROMPT_VERSION.to_string()),
            merge_verifier_model: extraction.model.clone(),
            merge_prompt_version: Some(MERGE_PROMPT_VERSION.to_string()),
            reranker_model,
        };
        Ok(RunProviders {
            embedder,
            extractor,
            entity_merge_verifier,
            reranker,
            entity_blocking_enabled: true,
            deterministic_replay: false,
            ledger,
            provenance,
        })
    }
}

pub(super) struct LiveChatThrottle {
    interval: Duration,
    next_allowed_at: tokio::sync::Mutex<TokioInstant>,
}

impl LiveChatThrottle {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_allowed_at: tokio::sync::Mutex::new(TokioInstant::now()),
        }
    }

    async fn wait(&self) {
        let sleep_until_instant = {
            let mut next_allowed_at = self.next_allowed_at.lock().await;
            let now = TokioInstant::now();
            let scheduled = (*next_allowed_at).max(now);
            *next_allowed_at = scheduled + self.interval;
            (scheduled > now).then_some(scheduled)
        };
        if let Some(instant) = sleep_until_instant {
            sleep_until(instant).await;
        }
    }
}

pub(super) struct MemoizedThrottledFactExtractor<T> {
    inner: T,
    throttle: Arc<LiveChatThrottle>,
    cache: tokio::sync::Mutex<BTreeMap<String, Vec<ExtractedFact>>>,
}

impl<T> MemoizedThrottledFactExtractor<T> {
    fn new(inner: T, throttle: Arc<LiveChatThrottle>) -> Self {
        Self {
            inner,
            throttle,
            cache: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }
}

#[async_trait]
impl<T> FactExtractor for MemoizedThrottledFactExtractor<T>
where
    T: FactExtractor,
{
    async fn extract(&self, chunks: &[TurnChunk]) -> moa_memory_ingest::Result<Vec<ExtractedFact>> {
        let key = extractor_cache_key(chunks);
        if let Some(facts) = self.cache.lock().await.get(&key).cloned() {
            return Ok(facts);
        }

        self.throttle.wait().await;
        let facts = self.inner.extract(chunks).await?;
        self.cache.lock().await.insert(key, facts.clone());
        Ok(facts)
    }
}

pub(super) fn extractor_cache_key(chunks: &[TurnChunk]) -> String {
    let mut key = String::new();
    for chunk in chunks {
        key.push_str(&chunk.index.to_string());
        key.push('\0');
        key.push_str(&chunk.text);
        key.push('\0');
    }
    key
}

#[derive(Clone)]
pub(super) struct SharedEmbeddingProvider(Arc<dyn EmbeddingProvider>);

#[async_trait]
impl EmbeddingProvider for SharedEmbeddingProvider {
    fn model_id(&self) -> &str {
        self.0.model_id()
    }

    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }

    fn model_version(&self) -> i32 {
        self.0.model_version()
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        self.0.embed(inputs).await
    }
}

#[derive(Clone)]
pub(super) struct SharedReranker(Arc<dyn Reranker>);

#[async_trait]
impl Reranker for SharedReranker {
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> moa_core::error::Result<Vec<RerankHit>> {
        self.0.rerank(model, query, documents, top_n).await
    }
}

pub(super) struct ThrottledEmbedder<T> {
    inner: T,
    throttle: Arc<LiveChatThrottle>,
}

impl<T> ThrottledEmbedder<T> {
    fn new(inner: T, throttle: Arc<LiveChatThrottle>) -> Self {
        Self { inner, throttle }
    }
}

#[async_trait]
impl<T> EmbeddingProvider for ThrottledEmbedder<T>
where
    T: EmbeddingProvider,
{
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_version(&self) -> i32 {
        self.inner.model_version()
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::error::Result<Vec<Vec<f32>>> {
        self.throttle.wait().await;
        self.inner.embed(inputs).await
    }
}

pub(super) struct ThrottledMergeVerifier<T> {
    inner: T,
    throttle: Arc<LiveChatThrottle>,
}

impl<T> ThrottledMergeVerifier<T> {
    fn new(inner: T, throttle: Arc<LiveChatThrottle>) -> Self {
        Self { inner, throttle }
    }
}

#[async_trait]
impl<T> EntityMergeVerifier for ThrottledMergeVerifier<T>
where
    T: EntityMergeVerifier,
{
    async fn should_merge(
        &self,
        mention: &str,
        candidate: &NodeIndexRow,
    ) -> moa_memory_ingest::Result<bool> {
        self.throttle.wait().await;
        self.inner.should_merge(mention, candidate).await
    }
}

pub(super) struct ThrottledReranker<T> {
    inner: T,
    throttle: Arc<LiveChatThrottle>,
}

impl<T> ThrottledReranker<T> {
    fn new(inner: T, throttle: Arc<LiveChatThrottle>) -> Self {
        Self { inner, throttle }
    }
}

#[async_trait]
impl<T> Reranker for ThrottledReranker<T>
where
    T: Reranker,
{
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> moa_core::error::Result<Vec<RerankHit>> {
        self.throttle.wait().await;
        self.inner.rerank(model, query, documents, top_n).await
    }
}

pub(super) fn default_live_budget_usd(profile: crate::memory_eval::CorpusProfile) -> f64 {
    match profile {
        crate::memory_eval::CorpusProfile::Pr => 5.0,
        crate::memory_eval::CorpusProfile::Full => 15.0,
    }
}

pub(super) async fn check_budget(
    ledger: &SharedCostLedger,
) -> std::result::Result<(), crate::kernel::CostError> {
    ledger.lock().await.check_budget()
}

pub(super) async fn cost_snapshot(ledger: &SharedCostLedger) -> CostLedger {
    ledger.lock().await.clone()
}
