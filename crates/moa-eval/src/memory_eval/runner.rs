//! Hermetic memory-retrieval evaluation runner.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_brain::planning::{
    PlanningCtx, QueryPlanner, QueryRetrievalCtx, parse_temporal, retrieve_for_query,
};
use moa_brain::retrieval::{
    CachedHybridRetriever, CohereReranker, HybridRetriever, NoopReranker, RankingConfig,
    RankingMode, Reranker, RetrievalHit,
};
use moa_core::{
    MemoryDigestConfig, MemoryScope, ScopeContext, ScopeTier, WorkspaceId,
    config::MemoryExtractionConfig, traits::EmbeddingProvider,
};
use moa_memory_graph::{AgeGraphStore, GraphStore, NodeIndexRow, PiiClass};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, DeterministicEntityMergeVerifier,
    EXTRACTION_PROMPT_VERSION, EmbeddedFact, EntityMergeFixtureRecord, EntityMergeVerifier,
    EntityResolver, ExtractedFact, ExtractionFixtureRecord, FactExtractor, HeuristicFactExtractor,
    IngestCtx, IngestError, LlmEntityMergeVerifier, LlmFactExtractor, MERGE_PROMPT_VERSION,
    RecordedEntityMergeVerifier, RecordedFactExtractor, SessionTurn, TurnChunk, chunk_turn,
    normalize_entity_name,
};
use moa_memory_lifecycle::{ConsolidationOptions, ConsolidationOutcome};
use moa_memory_pii::{PiiCategory, PiiClassifier, PiiError, PiiResult, PiiSpan, redact_text};
use moa_memory_vector::{CohereV4Embedder, PgvectorStore, VectorStore};
use moa_session::PostgresSessionStore;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tokio::time::{Instant as TokioInstant, sleep_until};
use uuid::Uuid;

use super::{
    BootstrapConfig, CachedEmbeddingFixture, CachedEmbeddingProvider, ClusterBootstrapReport,
    CorpusManifest, DEFAULT_BOOTSTRAP_RESAMPLES, EmbeddingInput, ExtractionPrecisionCounts,
    GoldPiiStatus, GoldResolutionReport, LedgerFact, Probe, ProbeResult, ProbeType,
    RetrievalMetrics, RetrievedCandidate, SyntheticSession, candidates_from_retrieval_hits,
    embedding_text_hash, read_embedding_inputs_jsonl, read_embeddings_jsonl, read_ledger_jsonl,
    read_manifest_json, read_probes_jsonl, read_sessions_jsonl, resolve_gold_nodes,
    validate_corpus,
};
use crate::kernel::{
    CostLedger, CountingEmbedder, CountingExtractor, CountingMergeVerifier, CountingReranker,
    FixtureStore, ProviderProvenance, SharedCostLedger,
};
use crate::{EvalError, Result};

/// Number of fused candidates collected for each probe before metric truncation.
pub const RETRIEVAL_EVAL_CANDIDATE_K: usize = 25;

/// Final recall cutoff used by the retrieval metrics.
pub const RETRIEVAL_EVAL_FINAL_K: usize = 4;

const CHUNK_TARGET_TOKENS: usize = 700;
const CHUNK_OVERLAP_TOKENS: usize = 100;

/// Options for running a hermetic memory-retrieval eval.
#[derive(Debug, Clone)]
pub struct MemoryRetrievalEvalOptions {
    corpus_dir: PathBuf,
    output_path: PathBuf,
    bootstrap_config: BootstrapConfig,
    reranker_enabled: bool,
    ranking_config: RankingConfig,
    extractor_mode: MemoryEvalExtractorMode,
    extractions_path: Option<PathBuf>,
    merges_path: Option<PathBuf>,
    lane: EvalLane,
    budget_usd: Option<f64>,
    consolidate: bool,
    digests: bool,
}

impl MemoryRetrievalEvalOptions {
    /// Creates options for a corpus directory and JSON report output path.
    pub fn new(corpus_dir: impl Into<PathBuf>, output_path: impl Into<PathBuf>) -> Self {
        let mut ranking_config = RankingConfig::default();
        ranking_config.weights.recency = 0.0;
        ranking_config.weights.access = 0.0;
        Self {
            corpus_dir: corpus_dir.into(),
            output_path: output_path.into(),
            bootstrap_config: BootstrapConfig {
                resamples: DEFAULT_BOOTSTRAP_RESAMPLES,
                seed: 13_579,
            },
            reranker_enabled: false,
            ranking_config,
            extractor_mode: MemoryEvalExtractorMode::Heuristic,
            extractions_path: None,
            merges_path: None,
            lane: EvalLane::Pr,
            budget_usd: None,
            consolidate: false,
            digests: false,
        }
    }

    /// Overrides the bootstrap settings used for confidence intervals.
    #[must_use]
    pub fn with_bootstrap_config(mut self, bootstrap_config: BootstrapConfig) -> Self {
        self.bootstrap_config = bootstrap_config;
        self
    }

    /// Overrides whether the eval should collect a post-rerank top-4 window.
    #[must_use]
    pub fn with_reranker(mut self, enabled: bool) -> Self {
        self.reranker_enabled = enabled;
        self
    }

    /// Overrides the deterministic ranking mode used by the eval run.
    #[must_use]
    pub fn with_ranking_mode(mut self, ranking_mode: RankingMode) -> Self {
        self.ranking_config.mode = ranking_mode;
        self
    }

    /// Overrides the full deterministic ranking configuration used by the eval run.
    #[must_use]
    pub fn with_ranking_config(mut self, ranking_config: RankingConfig) -> Self {
        self.ranking_config = ranking_config;
        self
    }

    /// Overrides the fact extractor used by ingestion and gold matching.
    #[must_use]
    pub fn with_extractor_mode(mut self, extractor_mode: MemoryEvalExtractorMode) -> Self {
        self.extractor_mode = extractor_mode;
        self
    }

    /// Overrides the extraction fixture path for recorded extraction mode.
    #[must_use]
    pub fn with_extractions_path(mut self, extractions_path: impl Into<PathBuf>) -> Self {
        self.extractions_path = Some(extractions_path.into());
        self
    }

    /// Overrides the merge-verifier fixture path for recorded extraction mode.
    #[must_use]
    pub fn with_merges_path(mut self, merges_path: impl Into<PathBuf>) -> Self {
        self.merges_path = Some(merges_path.into());
        self
    }

    /// Overrides the eval lane provider preset.
    #[must_use]
    pub fn with_lane(mut self, lane: EvalLane) -> Self {
        self.lane = lane;
        self
    }

    /// Overrides the live-lane cost budget in USD.
    #[must_use]
    pub fn with_budget_usd(mut self, budget_usd: f64) -> Self {
        self.budget_usd = Some(budget_usd);
        self
    }

    /// Enables the post-gold memory consolidation pass.
    #[must_use]
    pub fn with_consolidation(mut self, consolidate: bool) -> Self {
        self.consolidate = consolidate;
        self
    }

    /// Enables standing digest rebuilds and preference-context scoring.
    #[must_use]
    pub fn with_digests(mut self, digests: bool) -> Self {
        self.digests = digests;
        self
    }

    /// Returns the corpus directory.
    #[must_use]
    pub fn corpus_dir(&self) -> &Path {
        &self.corpus_dir
    }

    /// Returns the report output path.
    #[must_use]
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    /// Returns whether this eval run is collecting post-rerank retrieval.
    #[must_use]
    pub fn reranker_enabled(&self) -> bool {
        self.reranker_enabled
    }

    /// Returns the configured deterministic ranking mode.
    #[must_use]
    pub fn ranking_mode(&self) -> RankingMode {
        self.ranking_config.mode
    }

    /// Returns the configured deterministic ranking config.
    #[must_use]
    pub fn ranking_config(&self) -> &RankingConfig {
        &self.ranking_config
    }

    /// Returns the configured eval extractor mode.
    #[must_use]
    pub fn extractor_mode(&self) -> MemoryEvalExtractorMode {
        self.extractor_mode
    }

    /// Returns the selected eval lane.
    #[must_use]
    pub fn lane(&self) -> EvalLane {
        self.lane
    }

    /// Returns the explicitly configured cost budget, when present.
    #[must_use]
    pub fn budget_usd(&self) -> Option<f64> {
        self.budget_usd
    }

    /// Returns whether the eval should run graph-memory consolidation before probes.
    #[must_use]
    pub fn consolidate(&self) -> bool {
        self.consolidate
    }

    /// Returns whether the eval should build digests and score preference context.
    #[must_use]
    pub fn digests(&self) -> bool {
        self.digests
    }

    fn validate(&self) -> Result<()> {
        self.validate_with_env(|name| env::var(name).ok())
    }

    fn validate_with_env(&self, env_lookup: impl Fn(&str) -> Option<String>) -> Result<()> {
        if self
            .budget_usd
            .is_some_and(|budget_usd| !budget_usd.is_finite() || budget_usd < 0.0)
        {
            return Err(EvalError::InvalidConfig(
                "--budget-usd must be a finite non-negative number".to_string(),
            ));
        }
        if self.lane == EvalLane::Pr {
            if self.budget_usd.is_some() {
                return Err(EvalError::InvalidConfig(
                    "--budget-usd is only valid with --lane live".to_string(),
                ));
            }
            return Ok(());
        }

        if self.extractions_path.is_some() || self.merges_path.is_some() {
            return Err(EvalError::InvalidConfig(
                "--extractions and --merges are PR-lane fixture flags; live lane uses live providers"
                    .to_string(),
            ));
        }
        let extraction = MemoryExtractionConfig::default();
        let api_key = env_lookup(&extraction.api_key_env).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "live lane requires {} to be set",
                extraction.api_key_env
            ))
        })?;
        if api_key.trim().is_empty() {
            return Err(EvalError::InvalidConfig(format!(
                "live lane requires {} to be non-empty",
                extraction.api_key_env
            )));
        }
        Ok(())
    }

    fn extractor_for_corpus(
        &self,
        corpus: &LoadedMemoryEvalCorpus,
    ) -> Result<Arc<dyn FactExtractor>> {
        match self.extractor_mode {
            MemoryEvalExtractorMode::Heuristic => Ok(Arc::new(HeuristicFactExtractor)),
            MemoryEvalExtractorMode::Recorded => {
                let path = self
                    .extractions_path
                    .clone()
                    .unwrap_or_else(|| default_extractions_path(&corpus.manifest));
                let remediation = format!(
                    "cargo run -p xtask -- record-memory-extractions --corpus {} --output {}",
                    self.corpus_dir.display(),
                    path.display()
                );
                let store = FixtureStore::<ExtractionFixtureRecord>::read_jsonl(
                    &path,
                    EXTRACTION_PROMPT_VERSION,
                )?
                .with_remediation_command(remediation.clone());
                Ok(Arc::new(RecordedFactExtractor::new(store, remediation)))
            }
        }
    }

    fn entity_merge_verifier_for_corpus(
        &self,
        corpus: &LoadedMemoryEvalCorpus,
    ) -> Result<Arc<dyn EntityMergeVerifier>> {
        match self.extractor_mode {
            MemoryEvalExtractorMode::Heuristic => Ok(Arc::new(DeterministicEntityMergeVerifier)),
            MemoryEvalExtractorMode::Recorded => {
                let path = self
                    .merges_path
                    .clone()
                    .unwrap_or_else(|| default_merges_path(&corpus.manifest));
                let remediation = format!(
                    "cargo run -p xtask -- record-memory-merges --corpus {} --output {}",
                    self.corpus_dir.display(),
                    path.display()
                );
                let store = FixtureStore::<EntityMergeFixtureRecord>::read_jsonl(
                    &path,
                    MERGE_PROMPT_VERSION,
                )?
                .with_remediation_command(remediation.clone());
                Ok(Arc::new(RecordedEntityMergeVerifier::new(
                    store,
                    remediation,
                )))
            }
        }
    }
}

/// Provider preset used by the memory retrieval eval runner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EvalLane {
    /// Hermetic PR lane using cached embeddings and configured extractor fixtures.
    #[default]
    Pr,
    /// Live provider lane using Cohere embedding, extraction, merge verification, and reranking.
    Live,
}

/// Fact extractor mode used by the memory retrieval eval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MemoryEvalExtractorMode {
    /// Use the deterministic heuristic extractor.
    #[default]
    Heuristic,
    /// Replay committed extraction fixtures with no network access.
    Recorded,
}

fn default_extractions_path(manifest: &CorpusManifest) -> PathBuf {
    PathBuf::from("crates/moa-eval/fixtures/memory").join(format!(
        "extractions-{}-{}.jsonl",
        manifest.corpus_id, EXTRACTION_PROMPT_VERSION
    ))
}

fn default_merges_path(manifest: &CorpusManifest) -> PathBuf {
    PathBuf::from("crates/moa-eval/fixtures/memory").join(format!(
        "merges-{}-{}.jsonl",
        manifest.corpus_id, MERGE_PROMPT_VERSION
    ))
}

/// JSON report written by `run-memory-retrieval-eval`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRetrievalEvalReport {
    /// Corpus manifest loaded for this run.
    pub manifest: CorpusManifest,
    /// Number of candidates requested from production retrieval for metric scoring.
    pub candidate_k: usize,
    /// Final answer-context cutoff used by recall@4 and nDCG@4 metrics.
    pub final_k: usize,
    /// Whether the eval collected a post-rerank top-4 retrieval pass.
    #[serde(default)]
    pub reranker_enabled: bool,
    /// Whether the runner stopped after crossing the configured live-lane budget.
    #[serde(default, skip_serializing_if = "is_false")]
    pub aborted_over_budget: bool,
    /// Optional provider cost ledger for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostLedger>,
    /// Optional provider provenance for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<ProviderProvenance>,
    /// Aggregated retrieval metrics.
    pub metrics: RetrievalMetrics,
    /// Per-probe retrieval results with candidate attribution.
    pub probe_results: Vec<ProbeResult>,
    /// Cluster-bootstrap confidence intervals by user.
    pub bootstrap: Vec<ClusterBootstrapReport>,
    /// Probe ids that retrieved blocked facts.
    pub cross_user_leak_probe_ids: Vec<String>,
    /// Gold-resolution ingestion and fact-to-node mapping details.
    pub gold_resolution: GoldResolutionReport,
    /// Optional consolidation outcome collected after gold resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidation: Option<ConsolidationOutcome>,
}

/// Runs the hermetic memory-retrieval eval and writes `report.json`.
pub async fn run_memory_retrieval_eval(
    options: MemoryRetrievalEvalOptions,
) -> Result<MemoryRetrievalEvalReport> {
    options.validate()?;
    let corpus = LoadedMemoryEvalCorpus::load_for_lane(options.corpus_dir(), options.lane).await?;
    let store = IsolatedEvalStore::create().await?;
    let result = run_memory_retrieval_eval_in_store(&options, corpus, &store).await;
    let cleanup = store.cleanup().await;

    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            tracing::warn!(error = %cleanup_error, "failed to clean up memory retrieval eval store");
            Err(error)
        }
    }
}

async fn run_memory_retrieval_eval_in_store(
    options: &MemoryRetrievalEvalOptions,
    corpus: LoadedMemoryEvalCorpus,
    store: &IsolatedEvalStore,
) -> Result<MemoryRetrievalEvalReport> {
    cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
    let providers = options.providers_for_corpus(&corpus).await?;
    let ingest_ctx = store.ingest_ctx(
        providers.embedder.clone(),
        providers.extractor.clone(),
        providers.entity_merge_verifier.clone(),
        providers.entity_blocking_enabled,
    );
    let mut gold_resolution =
        resolve_gold_nodes(ingest_ctx, &corpus.ledger, &corpus.sessions).await?;
    apply_eval_validity_windows(store.pool(), &mut gold_resolution).await?;
    stabilize_eval_access_times(store.pool(), &corpus.ledger).await?;
    let ranking_reference_time = deterministic_ranking_reference_time(&corpus.ledger);
    let consolidation_reference_time = deterministic_consolidation_reference_time(&corpus.ledger);
    let fact_ids_by_uid = fact_ids_by_uid(&gold_resolution);
    let consolidation = if options.consolidate() {
        let outcome = run_eval_consolidation(
            store.pool(),
            &corpus.ledger,
            &gold_resolution,
            &fact_ids_by_uid,
            providers.embedder.clone(),
            consolidation_reference_time,
            digest_config_for_eval(options.digests()),
        )
        .await?;
        Some(outcome)
    } else if options.digests() {
        Some(
            run_eval_digest_rebuild(store.pool(), &corpus.ledger, consolidation_reference_time)
                .await?,
        )
    } else {
        None
    };
    let equivalent_fact_ids_by_uid = if options.consolidate() {
        equivalent_fact_ids_by_uid(store.pool(), &corpus.ledger, &fact_ids_by_uid).await?
    } else {
        HashMap::new()
    };
    let extraction_precision =
        extraction_precision_counts(store.pool(), &corpus.ledger, &fact_ids_by_uid).await?;
    let entity_fragmentation = entity_fragmentation_counts(store.pool(), &corpus.ledger).await?;
    if let Err(error) = check_budget(&providers.ledger).await {
        let report = build_eval_report(ReportBuildInput {
            manifest: corpus.manifest,
            gold_resolution,
            probe_results: Vec::new(),
            bootstrap_config: options.bootstrap_config,
            extraction_precision,
            entity_fragmentation,
            reranker_enabled: options.reranker_enabled(),
            aborted_over_budget: true,
            cost: Some(cost_snapshot(&providers.ledger).await),
            providers: Some(providers.provenance),
            consolidation: consolidation.clone(),
        });
        write_report(options.output_path(), &report).await?;
        cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
        return Err(error.into());
    }
    let gold_records_by_fact_id = gold_records_by_fact_id(&gold_resolution);
    let ledger_by_fact_id = ledger_by_fact_id(&corpus.ledger);
    let digest_context = if options.digests() {
        digest_context_by_user(store.pool(), &corpus.ledger).await?
    } else {
        HashMap::new()
    };
    let planner = QueryPlanner::new();
    let mut probe_results = Vec::with_capacity(corpus.probes.len());

    for (probe_index, probe) in corpus.probes.iter().enumerate() {
        let retrieval = retrieve_probe(
            store.pool(),
            &planner,
            providers.embedder.as_ref(),
            providers.reranker.clone(),
            probe,
            ProbeRetrieveOptions {
                use_reranker: options.reranker_enabled(),
                ranking_config: options.ranking_config().clone(),
                ranking_reference_time: Some(ranking_reference_time),
                deterministic_replay: providers.deterministic_replay,
            },
        )
        .await?;
        let candidates = candidates_from_retrieval_hits(
            &retrieval.pre_rerank_hits,
            &fact_ids_by_uid,
            &equivalent_fact_ids_by_uid,
        );
        let post_rerank_candidates = candidates_from_retrieval_hits(
            &retrieval.post_rerank_hits,
            &fact_ids_by_uid,
            &equivalent_fact_ids_by_uid,
        );
        let preference_context_hit = preference_context_hit(
            probe,
            post_rerank_candidates.as_slice(),
            &digest_context,
            &ledger_by_fact_id,
        );
        probe_results.push(probe_result_for(
            probe,
            candidates,
            Some(post_rerank_candidates),
            retrieval.retrieval_latency_ms,
            &gold_records_by_fact_id,
            preference_context_hit,
        ));
        if options.lane == EvalLane::Live
            && (probe_index + 1) % 10 == 0
            && let Err(error) = check_budget(&providers.ledger).await
        {
            let report = build_eval_report(ReportBuildInput {
                manifest: corpus.manifest,
                gold_resolution,
                probe_results,
                bootstrap_config: options.bootstrap_config,
                extraction_precision,
                entity_fragmentation,
                reranker_enabled: options.reranker_enabled(),
                aborted_over_budget: true,
                cost: Some(cost_snapshot(&providers.ledger).await),
                providers: Some(providers.provenance),
                consolidation: consolidation.clone(),
            });
            write_report(options.output_path(), &report).await?;
            cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
            return Err(error.into());
        }
    }

    if let Err(error) = check_budget(&providers.ledger).await {
        let report = build_eval_report(ReportBuildInput {
            manifest: corpus.manifest,
            gold_resolution,
            probe_results,
            bootstrap_config: options.bootstrap_config,
            extraction_precision,
            entity_fragmentation,
            reranker_enabled: options.reranker_enabled(),
            aborted_over_budget: true,
            cost: Some(cost_snapshot(&providers.ledger).await),
            providers: Some(providers.provenance),
            consolidation: consolidation.clone(),
        });
        write_report(options.output_path(), &report).await?;
        cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
        return Err(error.into());
    }

    let report = build_eval_report(ReportBuildInput {
        manifest: corpus.manifest,
        gold_resolution,
        probe_results,
        bootstrap_config: options.bootstrap_config,
        extraction_precision,
        entity_fragmentation,
        reranker_enabled: options.reranker_enabled(),
        aborted_over_budget: false,
        cost: Some(cost_snapshot(&providers.ledger).await),
        providers: Some(providers.provenance),
        consolidation,
    });
    write_report(options.output_path(), &report).await?;
    cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
    Ok(report)
}

struct RunProviders {
    embedder: Arc<dyn EmbeddingProvider>,
    extractor: Arc<dyn FactExtractor>,
    entity_merge_verifier: Arc<dyn EntityMergeVerifier>,
    reranker: Arc<dyn Reranker>,
    entity_blocking_enabled: bool,
    deterministic_replay: bool,
    ledger: SharedCostLedger,
    provenance: ProviderProvenance,
}

impl MemoryRetrievalEvalOptions {
    async fn providers_for_corpus(&self, corpus: &LoadedMemoryEvalCorpus) -> Result<RunProviders> {
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
        let extraction = MemoryExtractionConfig {
            enabled: true,
            ..MemoryExtractionConfig::default()
        };
        let api_key = env::var(&extraction.api_key_env).map_err(|_| {
            EvalError::InvalidConfig(format!(
                "live lane requires {} to be set",
                extraction.api_key_env
            ))
        })?;
        if api_key.trim().is_empty() {
            return Err(EvalError::InvalidConfig(format!(
                "live lane requires {} to be non-empty",
                extraction.api_key_env
            )));
        }
        let budget = self
            .budget_usd
            .unwrap_or_else(|| default_live_budget_usd(corpus.manifest.profile));
        let ledger = Arc::new(tokio::sync::Mutex::new(CostLedger::new(budget)));
        let chat_throttle = Arc::new(LiveChatThrottle::new(Duration::from_millis(3_200)));
        let embed_throttle = Arc::new(LiveChatThrottle::new(Duration::from_millis(700)));
        let rerank_throttle = Arc::new(LiveChatThrottle::new(Duration::from_millis(6_500)));
        let secret = SecretString::from(api_key);
        let raw_embedder = CohereV4Embedder::new(secret.clone());
        let embedding_model = raw_embedder.model_id().to_string();
        let embedding_model_version = raw_embedder.model_version();
        let embedder = Arc::new(ThrottledEmbedder::new(
            CountingEmbedder::new(raw_embedder, ledger.clone()),
            embed_throttle,
        )) as Arc<dyn EmbeddingProvider>;
        let live_extractor = LlmFactExtractor::from_config(&extraction).map_err(|error| {
            EvalError::InvalidConfig(format!("failed to initialize live fact extractor: {error}"))
        })?;
        let extractor = Arc::new(MemoizedThrottledFactExtractor::new(
            CountingExtractor::new(live_extractor, ledger.clone()),
            chat_throttle.clone(),
        )) as Arc<dyn FactExtractor>;
        let live_merge_verifier = LlmEntityMergeVerifier::from_env(
            &extraction.api_key_env,
            &extraction.model,
            extraction.timeout_ms,
        )
        .map_err(|error| {
            EvalError::InvalidConfig(format!("failed to initialize live merge verifier: {error}"))
        })?;
        let entity_merge_verifier = Arc::new(ThrottledMergeVerifier::new(
            CountingMergeVerifier::new(live_merge_verifier, ledger.clone()),
            chat_throttle,
        )) as Arc<dyn EntityMergeVerifier>;
        let reranker_model = if self.reranker_enabled {
            "rerank-v4.0-fast"
        } else {
            "noop"
        };
        let reranker: Arc<dyn Reranker> = if self.reranker_enabled {
            Arc::new(ThrottledReranker::new(
                CountingReranker::new(CohereReranker::new(secret), ledger.clone()),
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
            reranker_model: reranker_model.to_string(),
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

struct LiveChatThrottle {
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

struct MemoizedThrottledFactExtractor<T> {
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

fn extractor_cache_key(chunks: &[TurnChunk]) -> String {
    let mut key = String::new();
    for chunk in chunks {
        key.push_str(&chunk.index.to_string());
        key.push('\0');
        key.push_str(&chunk.text);
        key.push('\0');
    }
    key
}

struct ThrottledEmbedder<T> {
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

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        self.throttle.wait().await;
        self.inner.embed(inputs).await
    }
}

struct ThrottledMergeVerifier<T> {
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

struct ThrottledReranker<T> {
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
    ) -> moa_brain::retrieval::hybrid::Result<Vec<moa_brain::retrieval::RerankHit>> {
        self.throttle.wait().await;
        self.inner.rerank(model, query, documents, top_n).await
    }
}

fn default_live_budget_usd(profile: super::CorpusProfile) -> f64 {
    match profile {
        super::CorpusProfile::Pr => 5.0,
        super::CorpusProfile::Full => 15.0,
    }
}

struct ReportBuildInput {
    manifest: CorpusManifest,
    gold_resolution: GoldResolutionReport,
    probe_results: Vec<ProbeResult>,
    bootstrap_config: BootstrapConfig,
    extraction_precision: ExtractionPrecisionCounts,
    entity_fragmentation: super::EntityFragmentationCounts,
    reranker_enabled: bool,
    aborted_over_budget: bool,
    cost: Option<CostLedger>,
    providers: Option<ProviderProvenance>,
    consolidation: Option<ConsolidationOutcome>,
}

fn build_eval_report(input: ReportBuildInput) -> MemoryRetrievalEvalReport {
    let retrieval = super::aggregate_retrieval_eval_with_diagnostics(
        &input.gold_resolution,
        input.probe_results,
        input.bootstrap_config,
        input.extraction_precision,
        input.entity_fragmentation,
    );
    MemoryRetrievalEvalReport {
        manifest: input.manifest,
        candidate_k: RETRIEVAL_EVAL_CANDIDATE_K,
        final_k: RETRIEVAL_EVAL_FINAL_K,
        reranker_enabled: input.reranker_enabled,
        aborted_over_budget: input.aborted_over_budget,
        cost: input.cost,
        providers: input.providers,
        metrics: retrieval.metrics,
        probe_results: retrieval.probe_results,
        bootstrap: retrieval.bootstrap,
        cross_user_leak_probe_ids: retrieval.cross_user_leak_probe_ids,
        gold_resolution: input.gold_resolution,
        consolidation: input.consolidation,
    }
}

async fn check_budget(
    ledger: &SharedCostLedger,
) -> std::result::Result<(), crate::kernel::CostError> {
    ledger.lock().await.check_budget()
}

async fn cost_snapshot(ledger: &SharedCostLedger) -> CostLedger {
    ledger.lock().await.clone()
}

fn is_false(value: &bool) -> bool {
    !*value
}

pub(crate) async fn cleanup_eval_graph_rows(pool: &PgPool, ledger: &[LedgerFact]) -> Result<()> {
    let workspace_ids = eval_workspace_ids(ledger);
    if workspace_ids.is_empty() {
        return Ok(());
    }
    cleanup_eval_age_rows(pool, &workspace_ids).await?;
    sqlx::query("DELETE FROM moa.embeddings WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.ingest_dlq WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.ingest_dedup WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.memory_digests WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.graph_changelog WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.node_index WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.workspace_state WHERE workspace_id = ANY($1)")
        .bind(&workspace_ids)
        .execute(pool)
        .await?;
    Ok(())
}

async fn cleanup_eval_age_rows(pool: &PgPool, workspace_ids: &[String]) -> Result<()> {
    for label in AGE_EDGE_LABELS {
        cleanup_eval_age_table(pool, label, workspace_ids).await?;
    }
    for label in AGE_NODE_LABELS {
        cleanup_eval_age_table(pool, label, workspace_ids).await?;
    }
    Ok(())
}

async fn cleanup_eval_age_table(
    pool: &PgPool,
    label: &str,
    workspace_ids: &[String],
) -> Result<()> {
    let sql = format!(
        r#"
        DELETE FROM moa_graph."{label}"
        WHERE trim(both '"' from moa.age_property(properties, 'workspace_id')::text) = $1
        "#
    );
    for workspace_id in workspace_ids {
        sqlx::query(&sql).bind(workspace_id).execute(pool).await?;
    }
    Ok(())
}

const AGE_NODE_LABELS: &[&str] = &[
    "Entity", "Concept", "Decision", "Incident", "Lesson", "Fact", "Source",
];

const AGE_EDGE_LABELS: &[&str] = &[
    "RELATES_TO",
    "DEPENDS_ON",
    "OWNED_BY",
    "SUPERSEDES",
    "CONTRADICTS",
    "DERIVED_FROM",
    "MENTIONED_IN",
    "CAUSED",
    "LEARNED_FROM",
    "APPLIES_TO",
];

fn eval_workspace_ids(ledger: &[LedgerFact]) -> Vec<String> {
    ledger
        .iter()
        .map(|fact| fact.workspace_id.to_string())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

async fn apply_eval_validity_windows(
    pool: &PgPool,
    gold_resolution: &mut GoldResolutionReport,
) -> Result<()> {
    for record in &mut gold_resolution.records {
        let Some(valid_to) = record.expected_valid_to else {
            continue;
        };
        if record.node_uids.is_empty() {
            continue;
        }

        sqlx::query("UPDATE moa.node_index SET valid_to = $1 WHERE uid = ANY($2)")
            .bind(valid_to)
            .bind(&record.node_uids)
            .execute(pool)
            .await?;
        sqlx::query("UPDATE moa.embeddings SET valid_to = $1 WHERE uid = ANY($2)")
            .bind(valid_to)
            .bind(&record.node_uids)
            .execute(pool)
            .await?;

        record.valid_to = Some(valid_to);
        record.active = false;
        for node in &mut record.nodes {
            node.valid_to = Some(valid_to);
            node.active = false;
        }
    }
    Ok(())
}

async fn stabilize_eval_access_times(pool: &PgPool, ledger: &[LedgerFact]) -> Result<()> {
    let workspace_ids = eval_workspace_ids(ledger);
    if workspace_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE moa.node_index SET last_accessed_at = valid_from WHERE workspace_id = ANY($1)",
    )
    .bind(&workspace_ids)
    .execute(pool)
    .await?;
    Ok(())
}

async fn retrieve_probe(
    pool: &PgPool,
    planner: &QueryPlanner,
    embedder: &dyn EmbeddingProvider,
    reranker: Arc<dyn Reranker>,
    probe: &Probe,
    options: ProbeRetrieveOptions,
) -> Result<ProbeRetrieval> {
    let ProbeRetrieveOptions {
        use_reranker,
        ranking_config,
        ranking_reference_time,
        deterministic_replay,
    } = options;
    let started = Instant::now();
    let scope = MemoryScope::User {
        workspace_id: probe.workspace_id.clone(),
        user_id: probe.user_id.clone(),
    };
    let scope_context = ScopeContext::new(scope.clone());
    let mut vector_store = PgvectorStore::new_for_app_role(pool.clone(), scope_context.clone());
    if deterministic_replay {
        vector_store = vector_store.with_exact_search(true);
    }
    let vector: Arc<dyn VectorStore> = Arc::new(vector_store);
    let graph_store = AgeGraphStore::scoped_for_app_role(pool.clone(), scope_context)
        .with_vector_store(vector.clone());
    let graph: Arc<dyn GraphStore> = Arc::new(graph_store);
    let hybrid = Arc::new(
        HybridRetriever::new(pool.clone(), graph.clone(), vector)
            .with_ranking_config(ranking_config)
            .with_reranker(reranker)
            .with_assume_app_role(true),
    );
    let cached = CachedHybridRetriever::new_for_app_role(hybrid, pool.clone());
    let planning = PlanningCtx::new(scope, graph);

    let pre_rerank_hits = retrieve_probe_hits(
        planner,
        &planning,
        embedder,
        &cached,
        probe,
        ProbeHitOptions {
            k_final: RETRIEVAL_EVAL_CANDIDATE_K,
            use_reranker: false,
            ranking_reference_time,
        },
    )
    .await?;
    let post_rerank_hits = if use_reranker {
        retrieve_probe_hits(
            planner,
            &planning,
            embedder,
            &cached,
            probe,
            ProbeHitOptions {
                k_final: RETRIEVAL_EVAL_FINAL_K,
                use_reranker: true,
                ranking_reference_time,
            },
        )
        .await?
    } else {
        pre_rerank_hits
            .iter()
            .take(RETRIEVAL_EVAL_FINAL_K)
            .cloned()
            .collect()
    };

    Ok(ProbeRetrieval {
        pre_rerank_hits,
        post_rerank_hits,
        retrieval_latency_ms: if deterministic_replay {
            0
        } else {
            duration_ms_u64(started.elapsed())
        },
    })
}

struct ProbeRetrieveOptions {
    use_reranker: bool,
    ranking_config: RankingConfig,
    ranking_reference_time: Option<DateTime<Utc>>,
    deterministic_replay: bool,
}

async fn retrieve_probe_hits(
    planner: &QueryPlanner,
    planning: &PlanningCtx,
    embedder: &dyn EmbeddingProvider,
    cached: &CachedHybridRetriever,
    probe: &Probe,
    options: ProbeHitOptions,
) -> Result<Vec<RetrievalHit>> {
    let mut retrieval_ctx =
        QueryRetrievalCtx::new(planner, planning, embedder, cached, PiiClass::Restricted)
            .with_k_final(options.k_final)
            .with_reranker(options.use_reranker);
    if let Some(ranking_reference_time) = options.ranking_reference_time {
        retrieval_ctx = retrieval_ctx.with_ranking_reference_time(ranking_reference_time);
    }
    retrieval_ctx = retrieval_ctx.with_leg_timeouts_disabled();

    retrieve_for_query(&probe.query, &retrieval_ctx)
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "memory retrieval failed for probe {}: {error}",
                probe.probe_id
            ))
        })
}

fn deterministic_ranking_reference_time(ledger: &[LedgerFact]) -> DateTime<Utc> {
    ledger
        .iter()
        .map(|fact| fact.valid_from)
        .max()
        .unwrap_or_else(Utc::now)
        + chrono::Duration::days(7)
}

fn deterministic_consolidation_reference_time(ledger: &[LedgerFact]) -> DateTime<Utc> {
    ledger
        .iter()
        .map(|fact| fact.valid_from)
        .min()
        .unwrap_or_else(Utc::now)
        + chrono::Duration::days(7)
}

struct ProbeRetrieval {
    pre_rerank_hits: Vec<RetrievalHit>,
    post_rerank_hits: Vec<RetrievalHit>,
    retrieval_latency_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProbeHitOptions {
    k_final: usize,
    use_reranker: bool,
    ranking_reference_time: Option<DateTime<Utc>>,
}

fn duration_ms_u64(elapsed: std::time::Duration) -> u64 {
    elapsed.as_millis().min(u128::from(u64::MAX)) as u64
}

fn probe_result_for(
    probe: &Probe,
    candidates: Vec<RetrievedCandidate>,
    post_rerank_candidates: Option<Vec<RetrievedCandidate>>,
    retrieval_latency_ms: u64,
    gold_records_by_fact_id: &HashMap<String, super::GoldNodeRecord>,
    preference_context_hit: Option<bool>,
) -> ProbeResult {
    let final_candidates = post_rerank_candidates.as_deref().unwrap_or(&candidates);
    let expected_found_at_4 = all_expected_found_at_k(
        final_candidates,
        &probe.expected_fact_ids,
        RETRIEVAL_EVAL_FINAL_K,
    );
    let blocked_leaked = any_blocked_found_at_k(
        &candidates,
        &probe.blocked_fact_ids,
        RETRIEVAL_EVAL_CANDIDATE_K,
    );
    let pii_redacted = pii_redacted_for_probe(probe, gold_records_by_fact_id);
    let answer_faithful =
        answer_faithful_for_probe(probe, expected_found_at_4, blocked_leaked, pii_redacted);
    let (temporal_filter_parsed, temporal_filter_matches_as_of) = temporal_parse_diagnostics(probe);

    ProbeResult {
        probe_id: probe.probe_id.clone(),
        user_id: probe.user_id.as_str().to_string(),
        probe_type: probe.probe_type,
        expected_fact_ids: probe.expected_fact_ids.clone(),
        blocked_fact_ids: probe.blocked_fact_ids.clone(),
        candidates,
        post_rerank_candidates,
        retrieval_latency_ms,
        answer_faithful,
        abstention_correct: abstention_correct_for_probe(probe, blocked_leaked),
        pii_redacted,
        temporal_as_of_correct: temporal_as_of_correct_for_probe(probe, expected_found_at_4),
        temporal_filter_parsed,
        temporal_filter_matches_as_of,
        preference_context_hit,
    }
}

fn answer_faithful_for_probe(
    probe: &Probe,
    expected_found_at_4: bool,
    blocked_leaked: bool,
    pii_redacted: Option<bool>,
) -> Option<bool> {
    match probe.probe_type {
        ProbeType::Abstention | ProbeType::CrossUserIsolation => Some(!blocked_leaked),
        ProbeType::PiiRedaction => pii_redacted.map(|redacted| redacted && expected_found_at_4),
        _ if probe.expected_fact_ids.is_empty() => None,
        _ => Some(expected_found_at_4),
    }
}

fn abstention_correct_for_probe(probe: &Probe, blocked_leaked: bool) -> Option<bool> {
    match probe.probe_type {
        ProbeType::Abstention | ProbeType::CrossUserIsolation => Some(!blocked_leaked),
        _ => None,
    }
}

fn temporal_as_of_correct_for_probe(probe: &Probe, expected_found_at_4: bool) -> Option<bool> {
    if probe.probe_type == ProbeType::TemporalAsOf {
        Some(expected_found_at_4)
    } else {
        None
    }
}

fn temporal_parse_diagnostics(probe: &Probe) -> (Option<bool>, Option<bool>) {
    if probe.probe_type != ProbeType::TemporalAsOf {
        return (None, None);
    }

    let parsed = parse_temporal(&probe.query);
    (
        Some(parsed.is_some()),
        parsed.map(|instant| Some(instant) == probe.as_of),
    )
}

fn pii_redacted_for_probe(
    probe: &Probe,
    gold_records_by_fact_id: &HashMap<String, super::GoldNodeRecord>,
) -> Option<bool> {
    if probe.probe_type != ProbeType::PiiRedaction {
        return None;
    }

    let mut resolved_pii = false;
    for fact_id in &probe.expected_fact_ids {
        let Some(record) = gold_records_by_fact_id.get(fact_id) else {
            continue;
        };
        match record.pii_status {
            GoldPiiStatus::Unredacted | GoldPiiStatus::Mixed => return Some(false),
            GoldPiiStatus::Redacted => resolved_pii = true,
            GoldPiiStatus::NotExpected | GoldPiiStatus::NotResolved => {}
        }
    }
    resolved_pii.then_some(true)
}

fn all_expected_found_at_k(
    candidates: &[RetrievedCandidate],
    expected: &[String],
    k: usize,
) -> bool {
    if expected.is_empty() {
        return false;
    }
    expected.iter().all(|expected_fact_id| {
        candidates.iter().any(|candidate| {
            candidate.rank > 0
                && candidate.rank <= k
                && candidate.fact_id.as_deref() == Some(expected_fact_id.as_str())
        })
    })
}

fn any_blocked_found_at_k(candidates: &[RetrievedCandidate], blocked: &[String], k: usize) -> bool {
    if blocked.is_empty() {
        return false;
    }
    candidates.iter().any(|candidate| {
        candidate.rank > 0
            && candidate.rank <= k
            && candidate
                .fact_id
                .as_ref()
                .is_some_and(|fact_id| blocked.contains(fact_id))
    })
}

fn fact_ids_by_uid(gold_resolution: &GoldResolutionReport) -> HashMap<Uuid, String> {
    let mut fact_ids = HashMap::new();
    for record in &gold_resolution.records {
        for uid in &record.node_uids {
            fact_ids
                .entry(*uid)
                .or_insert_with(|| record.fact_id.clone());
        }
    }
    fact_ids
}

async fn equivalent_fact_ids_by_uid(
    pool: &PgPool,
    ledger: &[LedgerFact],
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> Result<HashMap<Uuid, Vec<String>>> {
    let workspace_ids = eval_workspace_ids(ledger);
    if workspace_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT target_uid, (payload->>'replacement_uid')::uuid AS replacement_uid
        FROM moa.graph_changelog
        WHERE workspace_id = ANY($1)
          AND op = 'supersede'
          AND target_kind = 'node'
          AND target_label = 'Fact'
          AND payload ? 'replacement_uid'
        ORDER BY change_id ASC
        "#,
    )
    .bind(&workspace_ids)
    .fetch_all(pool)
    .await?;
    let mut replacement_by_old = HashMap::<Uuid, Uuid>::new();
    for row in rows {
        replacement_by_old.insert(row.try_get("target_uid")?, row.try_get("replacement_uid")?);
    }

    let mut aliases = HashMap::<Uuid, Vec<String>>::new();
    for (uid, fact_id) in fact_ids_by_uid {
        let representative = supersession_representative(*uid, &replacement_by_old);
        if representative != *uid {
            aliases
                .entry(representative)
                .or_default()
                .push(fact_id.clone());
        }
    }
    for fact_ids in aliases.values_mut() {
        fact_ids.sort();
        fact_ids.dedup();
    }
    Ok(aliases)
}

fn supersession_representative(uid: Uuid, replacement_by_old: &HashMap<Uuid, Uuid>) -> Uuid {
    let mut current = uid;
    let mut seen = std::collections::BTreeSet::new();
    while seen.insert(current) {
        let Some(next) = replacement_by_old.get(&current).copied() else {
            return current;
        };
        current = next;
    }
    uid
}

async fn run_eval_consolidation(
    pool: &PgPool,
    ledger: &[LedgerFact],
    gold_resolution: &GoldResolutionReport,
    fact_ids_by_uid: &HashMap<Uuid, String>,
    embedder: Arc<dyn EmbeddingProvider>,
    reference_time: DateTime<Utc>,
    digest_config: MemoryDigestConfig,
) -> Result<ConsolidationOutcome> {
    let workspace_ids = eval_workspace_ids(ledger);
    let mut outcome = ConsolidationOutcome::default();
    for workspace_id in &workspace_ids {
        let workspace_outcome = moa_memory_lifecycle::consolidate_workspace(
            pool,
            WorkspaceId::new(workspace_id.clone()),
            ConsolidationOptions {
                digest: digest_config.clone(),
                ..ConsolidationOptions::default()
            },
            reference_time,
            Some(embedder.clone()),
        )
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "memory consolidation failed for workspace {workspace_id}: {error}"
            ))
        })?;
        add_consolidation_outcome(&mut outcome, workspace_outcome);
    }

    verify_restatement_pairs_collapsed(pool, ledger, gold_resolution, fact_ids_by_uid).await?;

    let mut second = ConsolidationOutcome::default();
    for workspace_id in &workspace_ids {
        let second_outcome = moa_memory_lifecycle::consolidate_workspace(
            pool,
            WorkspaceId::new(workspace_id.clone()),
            ConsolidationOptions {
                digest: digest_config.clone(),
                ..ConsolidationOptions::default()
            },
            reference_time,
            Some(embedder.clone()),
        )
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "second memory consolidation failed for workspace {workspace_id}: {error}"
            ))
        })?;
        add_consolidation_outcome(&mut second, second_outcome);
    }
    if !second.has_no_work() {
        return Err(EvalError::InvalidConfig(format!(
            "second consolidation pass was not idempotent: {second:?}"
        )));
    }

    Ok(outcome)
}

fn add_consolidation_outcome(total: &mut ConsolidationOutcome, next: ConsolidationOutcome) {
    total.merged += next.merged;
    total.decayed += next.decayed;
    total.at_floor += next.at_floor;
    total.contradiction_supersessions += next.contradiction_supersessions;
    total.entity_embeddings_backfilled += next.entity_embeddings_backfilled;
    total.aliases_promoted += next.aliases_promoted;
    total.duplicates_remaining += next.duplicates_remaining;
    total.digests_rebuilt += next.digests_rebuilt;
    total.digests_skipped_fresh += next.digests_skipped_fresh;
}

async fn run_eval_digest_rebuild(
    pool: &PgPool,
    ledger: &[LedgerFact],
    reference_time: DateTime<Utc>,
) -> Result<ConsolidationOutcome> {
    let mut outcome = ConsolidationOutcome::default();
    let config = digest_config_for_eval(true);
    for workspace_id in eval_workspace_ids(ledger) {
        let stats = moa_memory_lifecycle::rebuild_digests(
            pool,
            &WorkspaceId::new(workspace_id.clone()),
            reference_time,
            &config,
        )
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "memory digest rebuild failed for workspace {workspace_id}: {error}"
            ))
        })?;
        outcome.digests_rebuilt += stats.digests_rebuilt;
        outcome.digests_skipped_fresh += stats.digests_skipped_fresh;
    }
    Ok(outcome)
}

fn digest_config_for_eval(enabled: bool) -> MemoryDigestConfig {
    MemoryDigestConfig {
        enabled,
        ..MemoryDigestConfig::default()
    }
}

async fn verify_restatement_pairs_collapsed(
    pool: &PgPool,
    ledger: &[LedgerFact],
    gold_resolution: &GoldResolutionReport,
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> Result<()> {
    let records = gold_records_by_fact_id(gold_resolution);
    for fact in ledger.iter().filter(|fact| fact.restates.is_some()) {
        let canonical_id = fact
            .restates
            .as_deref()
            .expect("filtered restating facts should have canonical ids");
        let canonical = records.get(canonical_id).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "restating fact {} references missing gold record {}",
                fact.fact_id, canonical_id
            ))
        })?;
        let restating = records.get(&fact.fact_id).ok_or_else(|| {
            EvalError::InvalidConfig(format!(
                "restating fact {} has no gold record",
                fact.fact_id
            ))
        })?;
        let mut uids = canonical
            .node_uids
            .iter()
            .chain(restating.node_uids.iter())
            .copied()
            .collect::<Vec<_>>();
        uids.sort_unstable();
        uids.dedup();
        for uid in &uids {
            if !fact_ids_by_uid.contains_key(uid) {
                return Err(EvalError::InvalidConfig(format!(
                    "restatement pair {} -> {} resolved uid {} missing from fact_ids_by_uid",
                    fact.fact_id, canonical_id, uid
                )));
            }
        }
        let active = sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM moa.node_index WHERE uid = ANY($1) AND valid_to IS NULL",
        )
        .bind(&uids)
        .fetch_one(pool)
        .await?;
        if active != 1 {
            return Err(EvalError::InvalidConfig(format!(
                "restatement pair {} -> {} has {active} active nodes after consolidation",
                fact.fact_id, canonical_id
            )));
        }
    }
    Ok(())
}

async fn extraction_precision_counts(
    pool: &PgPool,
    ledger: &[LedgerFact],
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> Result<ExtractionPrecisionCounts> {
    let workspace_ids = eval_workspace_ids(ledger);
    let total_fact_nodes = if workspace_ids.is_empty() {
        0_i64
    } else {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM moa.node_index WHERE label = 'Fact' AND workspace_id = ANY($1)",
        )
        .bind(&workspace_ids)
        .fetch_one(pool)
        .await?
    };
    Ok(ExtractionPrecisionCounts {
        mapped_fact_nodes: fact_ids_by_uid.len(),
        total_fact_nodes: usize::try_from(total_fact_nodes).map_err(|_| {
            EvalError::InvalidConfig(format!(
                "stored Fact node count {total_fact_nodes} cannot fit usize"
            ))
        })?,
    })
}

async fn entity_fragmentation_counts(
    pool: &PgPool,
    ledger: &[LedgerFact],
) -> Result<super::EntityFragmentationCounts> {
    let workspace_ids = eval_workspace_ids(ledger);
    let active_entity_nodes = if workspace_ids.is_empty() {
        0_i64
    } else {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM moa.node_index
            WHERE label = 'Entity'
              AND valid_to IS NULL
              AND workspace_id = ANY($1)
            "#,
        )
        .bind(&workspace_ids)
        .fetch_one(pool)
        .await?
    };
    let distinct_ledger_mentions = ledger
        .iter()
        .flat_map(|fact| {
            [&fact.subject, &fact.object].into_iter().map(|mention| {
                let user_id = match fact.scope {
                    ScopeTier::User => fact.user_id.to_string(),
                    ScopeTier::Workspace | ScopeTier::Global => String::new(),
                };
                (
                    scope_tier_name(fact.scope).to_string(),
                    fact.workspace_id.to_string(),
                    user_id,
                    normalize_entity_name(mention),
                )
            })
        })
        .filter(|(_, _, _, mention)| !mention.trim().is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    Ok(super::EntityFragmentationCounts {
        active_entity_nodes: usize::try_from(active_entity_nodes).map_err(|_| {
            EvalError::InvalidConfig(format!(
                "stored Entity node count {active_entity_nodes} cannot fit usize"
            ))
        })?,
        distinct_ledger_mentions,
    })
}

fn scope_tier_name(scope: ScopeTier) -> &'static str {
    match scope {
        ScopeTier::Global => "global",
        ScopeTier::Workspace => "workspace",
        ScopeTier::User => "user",
    }
}

fn gold_records_by_fact_id(
    gold_resolution: &GoldResolutionReport,
) -> HashMap<String, super::GoldNodeRecord> {
    gold_resolution
        .records
        .iter()
        .map(|record| (record.fact_id.clone(), record.clone()))
        .collect()
}

fn ledger_by_fact_id(ledger: &[LedgerFact]) -> HashMap<String, LedgerFact> {
    ledger
        .iter()
        .map(|fact| (fact.fact_id.clone(), fact.clone()))
        .collect()
}

async fn digest_context_by_user(
    pool: &PgPool,
    ledger: &[LedgerFact],
) -> Result<HashMap<(String, String), String>> {
    let workspace_ids = eval_workspace_ids(ledger);
    if workspace_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT workspace_id, user_id, scope, content
        FROM moa.memory_digests
        WHERE workspace_id = ANY($1)
        ORDER BY workspace_id ASC, CASE scope WHEN 'user' THEN 0 ELSE 1 END, user_id ASC NULLS FIRST
        "#,
    )
    .bind(&workspace_ids)
    .fetch_all(pool)
    .await?;
    let mut workspace_content = HashMap::<String, String>::new();
    let mut user_content = HashMap::<(String, String), String>::new();
    for row in rows {
        let workspace_id: String = row.try_get("workspace_id")?;
        let user_id: Option<String> = row.try_get("user_id")?;
        let scope: String = row.try_get("scope")?;
        let content: String = row.try_get("content")?;
        if scope == "workspace" {
            workspace_content.insert(workspace_id, content);
        } else if scope == "user"
            && let Some(user_id) = user_id
        {
            user_content.insert((workspace_id, user_id), content);
        }
    }

    let users = ledger
        .iter()
        .map(|fact| (fact.workspace_id.to_string(), fact.user_id.to_string()))
        .collect::<std::collections::BTreeSet<_>>();
    let mut contexts = HashMap::new();
    for (workspace_id, user_id) in users {
        let mut content = String::new();
        if let Some(user_digest) = user_content.get(&(workspace_id.clone(), user_id.clone())) {
            content.push_str(user_digest);
            content.push('\n');
        }
        if let Some(workspace_digest) = workspace_content.get(&workspace_id) {
            content.push_str(workspace_digest);
        }
        contexts.insert((workspace_id, user_id), content);
    }
    Ok(contexts)
}

fn preference_context_hit(
    probe: &Probe,
    final_candidates: &[RetrievedCandidate],
    digest_context: &HashMap<(String, String), String>,
    ledger_by_fact_id: &HashMap<String, LedgerFact>,
) -> Option<bool> {
    if probe.probe_type != ProbeType::PreferenceApplication {
        return None;
    }

    let mut context = digest_context
        .get(&(probe.workspace_id.to_string(), probe.user_id.to_string()))
        .cloned()
        .unwrap_or_default();
    for candidate in final_candidates
        .iter()
        .filter(|candidate| candidate.rank > 0 && candidate.rank <= RETRIEVAL_EVAL_FINAL_K)
    {
        for fact_id in candidate
            .fact_id
            .as_deref()
            .into_iter()
            .chain(candidate.equivalent_fact_ids.iter().map(String::as_str))
        {
            if let Some(fact) = ledger_by_fact_id.get(fact_id) {
                context.push('\n');
                context.push_str(&fact.subject);
                context.push(' ');
                context.push_str(&fact.predicate);
                context.push(' ');
                context.push_str(&fact.object);
                context.push('\n');
                context.push_str(&fact.answer);
            }
        }
    }

    Some(probe.expected_fact_ids.iter().all(|fact_id| {
        ledger_by_fact_id.get(fact_id).is_some_and(|fact| {
            tokens_contained(&fact.object, &context) || tokens_contained(&fact.answer, &context)
        })
    }))
}

fn tokens_contained(expected: &str, haystack: &str) -> bool {
    let haystack_tokens = token_set(haystack);
    let expected_tokens = token_set(expected);
    !expected_tokens.is_empty()
        && expected_tokens
            .iter()
            .all(|token| haystack_tokens.contains(token))
}

fn token_set(text: &str) -> std::collections::BTreeSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() >= 2)
        .map(str::to_ascii_lowercase)
        .collect()
}

async fn write_report(path: &Path, report: &MemoryRetrievalEvalReport) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error(parent, source))?;
    }
    let json = serde_json::to_vec_pretty(report)?;
    tokio::fs::write(path, json)
        .await
        .map_err(|source| io_error(path, source))
}

pub(crate) async fn cached_embedding_provider_for_corpus(
    corpus: &LoadedMemoryEvalCorpus,
    extractor: &dyn FactExtractor,
) -> Result<CachedEmbeddingProvider> {
    let mut fixtures_by_hash = BTreeMap::<String, CachedEmbeddingFixture>::new();
    for fixture in corpus.embeddings.clone() {
        insert_fixture(&mut fixtures_by_hash, fixture)?;
    }
    ensure_embedding_input_coverage(&corpus.embedding_inputs, &fixtures_by_hash)?;

    for text in extracted_embedding_texts(&corpus.sessions, extractor).await? {
        insert_fixture(
            &mut fixtures_by_hash,
            CachedEmbeddingFixture::for_text(&text),
        )?;
    }

    CachedEmbeddingProvider::from_fixtures(fixtures_by_hash.into_values().collect())
}

fn insert_fixture(
    fixtures_by_hash: &mut BTreeMap<String, CachedEmbeddingFixture>,
    fixture: CachedEmbeddingFixture,
) -> Result<()> {
    match fixtures_by_hash.get(&fixture.text_hash) {
        Some(existing) if existing == &fixture => Ok(()),
        Some(_) => Err(EvalError::InvalidConfig(format!(
            "cached embedding text_hash {} has conflicting fixture values",
            fixture.text_hash
        ))),
        None => {
            fixtures_by_hash.insert(fixture.text_hash.clone(), fixture);
            Ok(())
        }
    }
}

fn ensure_embedding_input_coverage(
    inputs: &[EmbeddingInput],
    fixtures_by_hash: &BTreeMap<String, CachedEmbeddingFixture>,
) -> Result<()> {
    for input in inputs {
        let text_hash = embedding_text_hash(&input.text);
        if !fixtures_by_hash.contains_key(&text_hash) {
            return Err(EvalError::InvalidConfig(format!(
                "embeddings.jsonl is missing text_hash {text_hash} for embedding input {}",
                input.input_id
            )));
        }
    }
    Ok(())
}

async fn extracted_embedding_texts(
    sessions: &[SyntheticSession],
    extractor: &dyn FactExtractor,
) -> Result<Vec<String>> {
    let finalized_at = DateTime::<Utc>::from_timestamp(0, 0).ok_or_else(|| {
        EvalError::InvalidConfig("failed to construct deterministic eval timestamp".to_string())
    })?;
    let mut texts = BTreeMap::<String, ()>::new();
    for session in sessions {
        for turn in &session.turns {
            let session_turn = SessionTurn {
                workspace_id: session.workspace_id.clone(),
                user_id: session.user_id.clone(),
                session_id: session.session_id,
                turn_seq: turn.turn_seq,
                transcript: turn.transcript.clone(),
                dominant_pii_class: "none".to_string(),
                finalized_at,
            };
            let chunks = chunk_turn(&session_turn, CHUNK_TARGET_TOKENS, CHUNK_OVERLAP_TOKENS)
                .map_err(|error| {
                    EvalError::InvalidConfig(format!(
                        "failed to chunk synthetic session {} turn {}: {error}",
                        session.session_id, turn.turn_seq
                    ))
                })?;
            for fact in extractor.extract(&chunks).await.map_err(|error| {
                EvalError::InvalidConfig(format!(
                    "failed to extract embedding texts for synthetic session {} turn {}: {error}",
                    session.session_id, turn.turn_seq
                ))
            })? {
                texts.insert(fact.summary.clone(), ());
                insert_entity_embedding_texts(&mut texts, &fact.subject);
                insert_entity_embedding_texts(&mut texts, &fact.object);
                let pii = deterministic_pii_result(&fact.summary);
                let redacted = redact_text(&fact.summary, &pii.spans);
                if redacted != fact.summary {
                    texts.insert(redacted, ());
                }
            }
        }
    }
    Ok(texts.into_keys().collect())
}

fn insert_entity_embedding_texts(texts: &mut BTreeMap<String, ()>, mention: &str) {
    let normalized = normalize_entity_name(mention);
    if !normalized.trim().is_empty() {
        texts.insert(normalized, ());
    }

    let pii = deterministic_pii_result(mention);
    let redacted = redact_text(mention, &pii.spans);
    if redacted != mention {
        let normalized_redacted = normalize_entity_name(&redacted);
        if !normalized_redacted.trim().is_empty() {
            texts.insert(normalized_redacted, ());
        }
    }
}

pub(crate) struct LoadedMemoryEvalCorpus {
    pub(crate) manifest: CorpusManifest,
    pub(crate) ledger: Vec<LedgerFact>,
    pub(crate) sessions: Vec<SyntheticSession>,
    pub(crate) probes: Vec<Probe>,
    pub(crate) embedding_inputs: Vec<EmbeddingInput>,
    pub(crate) embeddings: Vec<CachedEmbeddingFixture>,
}

impl LoadedMemoryEvalCorpus {
    pub(crate) async fn load(corpus_dir: &Path) -> Result<Self> {
        Self::load_for_lane(corpus_dir, EvalLane::Pr).await
    }

    pub(crate) async fn load_for_lane(corpus_dir: &Path, lane: EvalLane) -> Result<Self> {
        let manifest = read_manifest_json(&corpus_dir.join("manifest.json")).await?;
        let ledger = read_ledger_jsonl(&corpus_dir.join("ledger.jsonl")).await?;
        let sessions = read_sessions_jsonl(&corpus_dir.join("sessions.jsonl")).await?;
        let probes = read_probes_jsonl(&corpus_dir.join("probes.jsonl"), &ledger).await?;
        validate_corpus(&manifest, &ledger, &sessions, &probes)?;
        let (embedding_inputs, embeddings) = match lane {
            EvalLane::Pr => {
                let embedding_inputs = read_embedding_inputs_jsonl(
                    &corpus_dir.join("embedding_inputs.jsonl"),
                    &ledger,
                    &probes,
                )
                .await?;
                let embeddings =
                    read_embeddings_jsonl(&corpus_dir.join("embeddings.jsonl")).await?;
                (embedding_inputs, embeddings)
            }
            EvalLane::Live => (Vec::new(), Vec::new()),
        };
        Ok(Self {
            manifest,
            ledger,
            sessions,
            probes,
            embedding_inputs,
            embeddings,
        })
    }
}

pub(crate) struct IsolatedEvalStore {
    store: PostgresSessionStore,
    database_url: String,
    schema_name: String,
}

impl IsolatedEvalStore {
    pub(crate) async fn create() -> Result<Self> {
        let database_url = test_database_url()?;
        let schema_name = format!("moa_memory_eval_{}", Uuid::now_v7().simple());
        let store = PostgresSessionStore::new_in_schema(&database_url, &schema_name).await?;
        Ok(Self {
            store,
            database_url,
            schema_name,
        })
    }

    pub(crate) fn pool(&self) -> &PgPool {
        self.store.pool()
    }

    pub(crate) fn ingest_ctx(
        &self,
        embedder: Arc<dyn EmbeddingProvider>,
        extractor: Arc<dyn FactExtractor>,
        entity_merge_verifier: Arc<dyn EntityMergeVerifier>,
        entity_blocking_enabled: bool,
    ) -> IngestCtx {
        let workspace_id = WorkspaceId::new(format!("memory-eval-runner-{}", self.schema_name));
        let scope = ScopeContext::workspace(workspace_id);
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            self.pool().clone(),
            scope.clone(),
        ));
        let graph = Arc::new(
            AgeGraphStore::scoped_for_app_role(self.pool().clone(), scope)
                .with_vector_store(vector.clone()),
        );
        let entity_resolver = EntityResolver::for_app_role(entity_merge_verifier);
        IngestCtx::new(
            self.pool().clone(),
            graph,
            vector,
            embedder,
            Arc::new(MemoryEvalPiiClassifier),
            Arc::new(InsertOnlyContradictionDetector),
        )
        .with_extractor(extractor)
        .with_entity_resolver(Arc::new(entity_resolver))
        .with_entity_embedding_blocking(entity_blocking_enabled)
    }

    pub(crate) async fn cleanup(self) -> Result<()> {
        let pool = self.store.pool().clone();
        drop(self.store);
        pool.close().await;
        moa_session::testing::cleanup_test_schema(&self.database_url, &self.schema_name)
            .await
            .map_err(EvalError::from)
    }
}

fn test_database_url() -> Result<String> {
    env::var("MOA_TEST_POSTGRES_URL")
        .or_else(|_| env::var("TEST_DATABASE_URL"))
        .or_else(|_| env::var("DATABASE_URL"))
        .map_err(|_| {
            EvalError::InvalidConfig(
                "MOA_TEST_POSTGRES_URL, TEST_DATABASE_URL, or DATABASE_URL must be set for memory retrieval eval"
                    .to_string(),
            )
        })
}

#[derive(Debug, Clone)]
struct MemoryEvalPiiClassifier;

#[async_trait]
impl PiiClassifier for MemoryEvalPiiClassifier {
    async fn classify(&self, text: &str) -> std::result::Result<PiiResult, PiiError> {
        Ok(deterministic_pii_result(text))
    }
}

fn deterministic_pii_result(text: &str) -> PiiResult {
    let mut spans = Vec::new();
    let mut cursor = 0_usize;
    for token in text.split_whitespace() {
        let Some(offset) = text[cursor..].find(token) else {
            continue;
        };
        let start = cursor + offset;
        let end = start + token.len();
        cursor = end;
        if token.contains('@') {
            spans.push(PiiSpan {
                start,
                end,
                category: PiiCategory::Email,
                confidence: 0.95,
            });
        } else if token.contains("sk-") || token.to_ascii_lowercase().contains("secret") {
            spans.push(PiiSpan {
                start,
                end,
                category: PiiCategory::Secret,
                confidence: 0.90,
            });
        }
    }

    PiiResult {
        class: if spans.is_empty() {
            PiiClass::None
        } else {
            PiiClass::Pii
        },
        spans,
        model_version: "memory-eval-deterministic-pii-v1".to_string(),
        abstained: false,
    }
}

#[derive(Debug, Clone)]
struct InsertOnlyContradictionDetector;

#[async_trait]
impl ContradictionDetector for InsertOnlyContradictionDetector {
    async fn check_one_fast(
        &self,
        _fact_text: &str,
        _embedding: &[f32],
        _label: moa_memory_graph::NodeLabel,
        _ctx: &ContradictionContext,
    ) -> std::result::Result<Conflict, IngestError> {
        Ok(Conflict::Insert)
    }

    async fn check_one_slow(
        &self,
        _fact: &EmbeddedFact,
        _ctx: &ContradictionContext,
    ) -> std::result::Result<Conflict, IngestError> {
        Ok(Conflict::Insert)
    }
}

fn io_error(path: &Path, source: std::io::Error) -> EvalError {
    EvalError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_eval::{
        CorpusProfile, generate_memory_eval_corpus, write_memory_eval_corpus,
    };

    #[test]
    fn live_lane_refuses_to_start_without_credentials() {
        // Pins: live eval cannot silently fall back to recorded or deterministic providers.
        let options =
            MemoryRetrievalEvalOptions::new("target/missing-corpus", "target/report.json")
                .with_lane(EvalLane::Live);

        let error = options
            .validate_with_env(|_| None)
            .expect_err("live lane without credentials should fail");

        assert!(error.to_string().contains("COHERE_API_KEY"));
    }

    #[test]
    fn pr_lane_refuses_live_only_flags() {
        // Pins: PR eval cannot accept budget-only live-lane flags and pretend it ran hermetically.
        let options =
            MemoryRetrievalEvalOptions::new("target/missing-corpus", "target/report.json")
                .with_budget_usd(1.0);

        let error = options
            .validate_with_env(|_| Some("unused".to_string()))
            .expect_err("PR lane with a live budget should fail");

        assert!(error.to_string().contains("--budget-usd"));
    }

    #[test]
    fn entity_embedding_texts_include_redacted_mentions() {
        // Pins: hermetic eval fixture preload covers entity names after deterministic PII redaction.
        let mut texts = BTreeMap::new();

        insert_entity_embedding_texts(&mut texts, "ops@example.com");

        let keys = texts.into_keys().collect::<Vec<_>>();
        assert_eq!(keys, vec!["email redacted", "ops example com"]);
    }

    #[tokio::test]
    async fn live_lane_skips_fixture_coverage_check() {
        // Pins: live provider runs do not load or require hermetic embedding fixtures.
        let corpus = generate_memory_eval_corpus(CorpusProfile::Pr, vec![1, 2, 3])
            .expect("generate a small deterministic corpus");
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let corpus_dir = temp.path().join("corpus");
        write_memory_eval_corpus(&corpus_dir, &corpus)
            .await
            .expect("corpus files should be written without embeddings.jsonl");

        let live = LoadedMemoryEvalCorpus::load_for_lane(&corpus_dir, EvalLane::Live)
            .await
            .expect("live lane should not load embeddings.jsonl");
        let pr = LoadedMemoryEvalCorpus::load_for_lane(&corpus_dir, EvalLane::Pr).await;
        let error = match pr {
            Ok(_) => panic!("PR lane should require embeddings.jsonl"),
            Err(error) => error,
        };

        assert_eq!(live.embeddings.len(), 0);
        assert!(error.to_string().contains("embeddings.jsonl"));
    }
}
