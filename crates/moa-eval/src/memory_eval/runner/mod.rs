//! Hermetic memory-retrieval evaluation runner.

use std::collections::{BTreeMap, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_brain::planning::{
    PlannedQuery, PlanningCtx, QueryPlanner, parse_temporal,
    should_skip_graph_expansion_for_direct_lookup,
};
use moa_brain::retrieval::{
    GraphPathTrace, GraphRetrievalDiagnostics, GraphRetrievalPolicy, HybridRetriever,
    RankingConfig, RetrievalHit, RetrievalOutput, RetrievalRequest,
};
use moa_core::RlsContext;
use moa_core::{ContactId, MemoryDigestConfig, MoaConfig, UserId, traits::EmbeddingProvider};
use moa_db::ScopedConn;
use moa_memory_graph::{GraphStore, NodeIndexRow, PiiClass, PostgresGraphStore};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, DeterministicEntityMergeVerifier,
    EXTRACTION_PROMPT_VERSION, EmbeddedFact, EntityMergeFixtureRecord, EntityMergeVerifier,
    EntityResolver, ExtractedFact, ExtractionFixtureRecord, FactExtractor, HeuristicFactExtractor,
    IngestCtx, IngestError, MERGE_PROMPT_VERSION, ModelEntityMergeVerifier, ModelFactExtractor,
    RecordedEntityMergeVerifier, RecordedFactExtractor, SessionTurn, TurnChunk, chunk_turn,
    normalize_entity_name,
};
use moa_memory_lifecycle::{ConsolidationOptions, ConsolidationOutcome, beta_smoothed_quality};
use moa_memory_pii::{PiiCategory, PiiClassifier, PiiError, PiiResult, PiiSpan, redact_text};
use moa_memory_types::{MemoryScope, ScopeTier};
use moa_memory_vector::{PgvectorStore, VECTOR_DIMENSION, VectorStore};
use moa_providers::{
    COHERE_DEFAULT_RERANK_MODEL, ConfiguredReranker, EmbedderConstructionRole, NoopReranker,
    RerankHit, Reranker, build_embedder_from_config, build_reranker_from_config,
};
use moa_session::PostgresSessionStore;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use tokio::time::{Instant as TokioInstant, sleep_until};
use uuid::Uuid;

use super::scope::{
    stable_uuid_from_label, tenant_id_from_label, tenant_id_from_storage_partition,
    tenant_id_from_storage_partition_id,
};
use super::{
    BootstrapConfig, CachedEmbeddingFixture, CachedEmbeddingProvider, CorpusManifest,
    DEFAULT_BOOTSTRAP_RESAMPLES, DeterministicJudge, EmbeddingInput, ExtractionPrecisionCounts,
    GoldPiiStatus, GoldResolutionReport, GraphImpact, JudgeInput, JudgeOutcome, LedgerFact, Probe,
    ProbeGraphComparison, ProbeGraphPathDiagnostic, ProbeResult, ProbeType, RetrievedCandidate,
    SyntheticSession, candidates_from_retrieval_hits, embedding_text_hash,
    read_embedding_inputs_jsonl, read_embeddings_jsonl, read_ledger_jsonl, read_manifest_json,
    read_probes_jsonl, read_sessions_jsonl, resolve_gold_nodes, validate_corpus,
};
use crate::kernel::{
    CostLedger, CountingEmbedder, CountingExtractor, CountingMergeVerifier, CountingReranker,
    FixtureStore, ProviderProvenance, SharedCostLedger,
};
use moa_eval_core::{EvalError, Result};

use super::io::io_error;

mod report;
mod rewrite;

pub use report::{MemoryGraphDiagnostics, MemoryRetrievalEvalReport, QueryRewriteClassMetrics};

use report::{ReportBuildInput, build_eval_report};
use rewrite::{QueryRewriteAccounting, QueryRewriteSummary, probe_for_rewrite_policy};

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
    rewrite_policy: QueryRewritePolicy,
    extractor_mode: MemoryEvalExtractorMode,
    extractions_path: Option<PathBuf>,
    merges_path: Option<PathBuf>,
    lane: EvalLane,
    budget_usd: Option<f64>,
    consolidate: bool,
    digests: bool,
    invert_quality_priors: bool,
    graph_expansion_policy: GraphExpansionEvalPolicy,
}

/// Eval-only graph expansion policy used for memory-retrieval A/B runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GraphExpansionEvalPolicy {
    /// Use the production graph retrieval policy.
    #[default]
    Current,
    /// Disable graph expansion only for direct exact-anchor non-temporal probes.
    SkipExactDirect,
    /// Preserve the pre-guardrail broad graph expansion behavior for A/B reports.
    LegacyBroadExpansion,
}

impl GraphExpansionEvalPolicy {
    /// Returns the stable CLI label for this eval policy.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::SkipExactDirect => "skip-exact-direct",
            Self::LegacyBroadExpansion => "legacy-broad-expansion",
        }
    }

    /// Returns the production graph retrieval policy used by this eval lane.
    #[must_use]
    pub const fn graph_retrieval_policy(self) -> GraphRetrievalPolicy {
        match self {
            Self::Current | Self::SkipExactDirect => GraphRetrievalPolicy::AnchoredRescue,
            Self::LegacyBroadExpansion => GraphRetrievalPolicy::LegacyBroadExpansion,
        }
    }
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
            rewrite_policy: QueryRewritePolicy::Gated,
            extractor_mode: MemoryEvalExtractorMode::Heuristic,
            extractions_path: None,
            merges_path: None,
            lane: EvalLane::Pr,
            budget_usd: None,
            consolidate: false,
            digests: false,
            invert_quality_priors: false,
            graph_expansion_policy: GraphExpansionEvalPolicy::Current,
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

    /// Overrides the full deterministic ranking configuration used by the eval run.
    #[must_use]
    pub fn with_ranking_config(mut self, ranking_config: RankingConfig) -> Self {
        self.ranking_config = ranking_config;
        self
    }

    /// Overrides the query rewrite policy used by retrieval probes.
    #[must_use]
    pub fn with_rewrite_policy(mut self, rewrite_policy: QueryRewritePolicy) -> Self {
        self.rewrite_policy = rewrite_policy;
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

    /// Inverts synthetic quality priors for the negative-control eval lane.
    #[must_use]
    pub fn with_inverted_quality_priors(mut self, invert_quality_priors: bool) -> Self {
        self.invert_quality_priors = invert_quality_priors;
        self
    }

    /// Overrides the eval-only graph expansion policy.
    #[must_use]
    pub fn with_graph_expansion_policy(mut self, policy: GraphExpansionEvalPolicy) -> Self {
        self.graph_expansion_policy = policy;
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

    /// Returns the configured deterministic ranking config.
    #[must_use]
    pub fn ranking_config(&self) -> &RankingConfig {
        &self.ranking_config
    }

    /// Returns the configured query rewrite policy.
    #[must_use]
    pub fn rewrite_policy(&self) -> QueryRewritePolicy {
        self.rewrite_policy
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

    /// Returns whether seeded quality priors should be inverted.
    #[must_use]
    pub fn invert_quality_priors(&self) -> bool {
        self.invert_quality_priors
    }

    fn validate(&self) -> Result<()> {
        if self
            .budget_usd
            .is_some_and(|budget_usd| !budget_usd.is_finite() || budget_usd < 0.0)
        {
            return Err(EvalError::InvalidConfig(
                "--budget-usd must be a finite non-negative number".to_string(),
            ));
        }
        if !self.ranking_config.weights.quality.is_finite() {
            return Err(EvalError::InvalidConfig(
                "--quality-weight must be finite".to_string(),
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
    /// Live provider lane using provider-backed extraction/merge plus Cohere embedding/reranking.
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

/// Query rewrite policy used by the memory retrieval eval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryRewritePolicy {
    /// Use the original probe query for every retrieval.
    Off,
    /// Treat every probe as rewritten by a deterministic fixture.
    Always,
    /// Use deterministic query-class gating to decide whether a rewrite fixture applies.
    #[default]
    Gated,
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

/// Runs the hermetic memory-retrieval eval and writes `report.json`.
pub async fn run_memory_retrieval_eval(
    options: MemoryRetrievalEvalOptions,
) -> Result<MemoryRetrievalEvalReport> {
    options.validate()?;
    let corpus = LoadedMemoryEvalCorpus::load_for_lane(options.corpus_dir(), options.lane).await?;
    let store = IsolatedEvalStore::create().await?;
    let result = run_memory_retrieval_eval_in_store(&options, corpus, &store).await;
    if env::var("MOA_EVAL_KEEP_STORE").is_ok() {
        eprintln!("[keep-store] schema kept: {}", store.schema_name);
        return result;
    }
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
    seed_eval_storage_partition_embedder_state(
        store.pool(),
        &corpus.ledger,
        providers.embedder.as_ref(),
    )
    .await?;
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
    seed_eval_quality_scores(
        store.pool(),
        &corpus.ledger,
        &gold_resolution,
        options.invert_quality_priors(),
    )
    .await?;
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
            rewrite_summary: QueryRewriteSummary::empty(options.rewrite_policy()),
            graph_expansion_policy: options.graph_expansion_policy,
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
    let mut rewrite_accounting = QueryRewriteAccounting::new(options.rewrite_policy());

    for (probe_index, probe) in corpus.probes.iter().enumerate() {
        let rewrite_decision = rewrite_accounting.record(probe);
        let retrieval_probe = probe_for_rewrite_policy(probe, rewrite_decision);
        let retrieval = retrieve_probe(
            store.pool(),
            &planner,
            providers.embedder.as_ref(),
            providers.reranker.clone(),
            &retrieval_probe,
            ProbeRetrieveOptions {
                use_reranker: options.reranker_enabled(),
                ranking_config: options.ranking_config().clone(),
                ranking_reference_time: Some(ranking_reference_time),
                deterministic_replay: providers.deterministic_replay,
                graph_expansion_policy: options.graph_expansion_policy,
            },
        )
        .await?;
        let candidates = candidates_from_retrieval_hits(
            &retrieval.pre_rerank_hits,
            &fact_ids_by_uid,
            &equivalent_fact_ids_by_uid,
        );
        let graph_comparison =
            retrieval
                .graph_off_retrieval_latency_ms
                .map(|graph_off_retrieval_latency_ms| {
                    let graph_off_candidates = candidates_from_retrieval_hits(
                        &retrieval.graph_off_hits,
                        &fact_ids_by_uid,
                        &equivalent_fact_ids_by_uid,
                    );
                    probe_graph_comparison(
                        &probe.expected_fact_ids,
                        candidates.as_slice(),
                        graph_off_candidates,
                        &retrieval.graph_diagnostics,
                        graph_off_retrieval_latency_ms,
                    )
                });
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
        probe_results.push(probe_result_for(ProbeResultInput {
            probe,
            candidates,
            post_rerank_candidates: Some(post_rerank_candidates),
            retrieval_latency_ms: retrieval.retrieval_latency_ms,
            gold_records_by_fact_id: &gold_records_by_fact_id,
            preference_context_hit,
            graph_diagnostics: Some(retrieval.graph_diagnostics),
            graph_comparison,
        })?);
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
                rewrite_summary: rewrite_accounting.summary(),
                graph_expansion_policy: options.graph_expansion_policy,
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
            rewrite_summary: rewrite_accounting.summary(),
            graph_expansion_policy: options.graph_expansion_policy,
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
        rewrite_summary: rewrite_accounting.summary(),
        graph_expansion_policy: options.graph_expansion_policy,
        aborted_over_budget: false,
        cost: Some(cost_snapshot(&providers.ledger).await),
        providers: Some(providers.provenance),
        consolidation,
    });
    write_report(options.output_path(), &report).await?;
    if env::var("MOA_EVAL_KEEP_STORE").is_err() {
        cleanup_eval_graph_rows(store.pool(), &corpus.ledger).await?;
    }
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
        let raw_embedder = build_embedder_from_config(&config, EmbedderConstructionRole::Retrieval)
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
            let configured = build_reranker_from_config(&config).map_err(|error| {
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

#[derive(Clone)]
struct SharedEmbeddingProvider(Arc<dyn EmbeddingProvider>);

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

    async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        self.0.embed(inputs).await
    }
}

#[derive(Clone)]
struct SharedReranker(Arc<dyn Reranker>);

#[async_trait]
impl Reranker for SharedReranker {
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> moa_core::Result<Vec<RerankHit>> {
        self.0.rerank(model, query, documents, top_n).await
    }
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
    ) -> moa_core::Result<Vec<RerankHit>> {
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

async fn check_budget(
    ledger: &SharedCostLedger,
) -> std::result::Result<(), crate::kernel::CostError> {
    ledger.lock().await.check_budget()
}

async fn cost_snapshot(ledger: &SharedCostLedger) -> CostLedger {
    ledger.lock().await.clone()
}

pub(crate) async fn cleanup_eval_graph_rows(pool: &PgPool, ledger: &[LedgerFact]) -> Result<()> {
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    if storage_partition_ids.is_empty() {
        return Ok(());
    }
    sqlx::query("DELETE FROM moa.edge_index WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.embeddings WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.ingest_dlq WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.ingest_dedup WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.memory_digests WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.retrieval_lineage WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.graph_changelog WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.node_index WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM moa.storage_partition_state WHERE storage_partition_id = ANY($1)")
        .bind(&storage_partition_ids)
        .execute(pool)
        .await?;
    Ok(())
}

async fn seed_eval_storage_partition_embedder_state(
    pool: &PgPool,
    ledger: &[LedgerFact],
    embedder: &dyn EmbeddingProvider,
) -> Result<()> {
    for storage_partition_id in eval_storage_partition_ids(ledger) {
        seed_eval_storage_partition_embedder_state_row(pool, &storage_partition_id, embedder)
            .await?;
    }
    Ok(())
}

async fn seed_eval_storage_partition_embedder_state_row(
    pool: &PgPool,
    storage_partition_id: &str,
    embedder: &dyn EmbeddingProvider,
) -> Result<()> {
    let scope = RlsContext::tenant(tenant_id_from_storage_partition(storage_partition_id));
    let mut conn = ScopedConn::begin(pool, &scope).await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(conn.as_mut())
        .await?;
    sqlx::query(
        r#"
        INSERT INTO moa.storage_partition_state
            (storage_partition_id, embedding_model, embedding_model_version, embedding_dimension)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (storage_partition_id) DO UPDATE
            SET embedding_model = EXCLUDED.embedding_model,
                embedding_model_version = EXCLUDED.embedding_model_version,
                embedding_dimension = EXCLUDED.embedding_dimension,
                reembed_state = 'steady',
                updated_at = now()
        "#,
    )
    .bind(storage_partition_id)
    .bind(embedder.model_id())
    .bind(embedder.model_version())
    .bind(VECTOR_DIMENSION as i32)
    .execute(conn.as_mut())
    .await?;
    conn.commit().await?;
    Ok(())
}

fn eval_storage_partition_ids(ledger: &[LedgerFact]) -> Vec<String> {
    ledger
        .iter()
        .map(|fact| tenant_id_from_storage_partition_id(&fact.storage_partition_id).to_string())
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
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    if storage_partition_ids.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE moa.node_index SET last_accessed_at = valid_from WHERE storage_partition_id = ANY($1)",
    )
    .bind(&storage_partition_ids)
    .execute(pool)
    .await?;
    Ok(())
}

async fn seed_eval_quality_scores(
    pool: &PgPool,
    ledger: &[LedgerFact],
    gold_resolution: &GoldResolutionReport,
    invert_priors: bool,
) -> Result<()> {
    let facts = ledger_by_fact_id(ledger);
    for record in &gold_resolution.records {
        let Some(fact) = facts.get(record.fact_id.as_str()) else {
            continue;
        };
        let (Some(uses), Some(successes)) = (fact.prior_uses, fact.prior_successes) else {
            continue;
        };
        if record.node_uids.is_empty() {
            continue;
        }
        let successes = if invert_priors {
            uses.saturating_sub(successes)
        } else {
            successes
        };
        let quality_score = beta_smoothed_quality(u64::from(uses), u64::from(successes));
        sqlx::query("UPDATE moa.node_index SET quality_score = $1 WHERE uid = ANY($2)")
            .bind(quality_score)
            .bind(&record.node_uids)
            .execute(pool)
            .await?;
    }
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
        graph_expansion_policy,
    } = options;
    let started = Instant::now();
    let scope = MemoryScope::Contact {
        tenant_id: tenant_id_from_storage_partition_id(&probe.storage_partition_id),
        contact_id: contact_id_from_user_id(&probe.user_id),
    };
    let scope_context = scope.to_rls_context();
    let mut vector_store = PgvectorStore::new_for_app_role(pool.clone(), scope_context.clone());
    if deterministic_replay {
        vector_store = vector_store.with_exact_search(true);
    }
    let vector = Arc::new(vector_store);
    let graph_vector: Arc<dyn VectorStore> = vector.clone();
    let graph_store = PostgresGraphStore::scoped_for_app_role(pool.clone(), scope_context)
        .with_vector_store(graph_vector);
    let graph: Arc<dyn GraphStore> = Arc::new(graph_store);
    let hybrid = HybridRetriever::new(pool.clone(), graph.clone(), vector)
        .with_ranking_config(ranking_config)
        .with_reranker(reranker)
        .with_assume_app_role(true);
    let planning = PlanningCtx::new(scope, graph);
    let planned = planner
        .plan(&probe.query, &planning)
        .await
        .map_err(|error| memory_retrieval_error(probe, error))?;
    let query_embedding = embed_probe_query(embedder, probe).await?;

    let pre_rerank_output = retrieve_probe_output(
        &hybrid,
        &planned,
        probe,
        query_embedding.clone(),
        ProbeHitOptions {
            k_final: RETRIEVAL_EVAL_CANDIDATE_K,
            use_reranker: false,
            ranking_reference_time,
            graph_expansion_policy,
            force_graph_off: false,
        },
    )
    .await?;
    let post_rerank_hits = if use_reranker {
        retrieve_probe_output(
            &hybrid,
            &planned,
            probe,
            query_embedding.clone(),
            ProbeHitOptions {
                k_final: RETRIEVAL_EVAL_FINAL_K,
                use_reranker: true,
                ranking_reference_time,
                graph_expansion_policy,
                force_graph_off: false,
            },
        )
        .await?
        .hits
    } else {
        pre_rerank_output
            .hits
            .iter()
            .take(RETRIEVAL_EVAL_FINAL_K)
            .cloned()
            .collect()
    };
    let primary_retrieval_latency_ms = if deterministic_replay {
        0
    } else {
        duration_ms_u64(started.elapsed())
    };
    let (graph_off_hits, graph_off_retrieval_latency_ms) =
        if should_compare_graph(pre_rerank_output.diagnostics.policy) {
            let graph_off_started = Instant::now();
            let graph_off_output = retrieve_probe_output(
                &hybrid,
                &planned,
                probe,
                query_embedding,
                ProbeHitOptions {
                    k_final: RETRIEVAL_EVAL_CANDIDATE_K,
                    use_reranker: false,
                    ranking_reference_time,
                    graph_expansion_policy,
                    force_graph_off: true,
                },
            )
            .await?;
            (
                graph_off_output.hits,
                Some(if deterministic_replay {
                    0
                } else {
                    duration_ms_u64(graph_off_started.elapsed())
                }),
            )
        } else {
            (Vec::new(), None)
        };

    Ok(ProbeRetrieval {
        pre_rerank_hits: pre_rerank_output.hits,
        post_rerank_hits,
        graph_diagnostics: pre_rerank_output.diagnostics,
        graph_off_hits,
        graph_off_retrieval_latency_ms,
        retrieval_latency_ms: primary_retrieval_latency_ms,
    })
}

struct ProbeRetrieveOptions {
    use_reranker: bool,
    ranking_config: RankingConfig,
    ranking_reference_time: Option<DateTime<Utc>>,
    deterministic_replay: bool,
    graph_expansion_policy: GraphExpansionEvalPolicy,
}

fn should_skip_graph_expansion_for_exact_direct_probe(
    planned: &PlannedQuery,
    req: &RetrievalRequest,
) -> bool {
    req.as_of.is_none() && should_skip_graph_expansion_for_direct_lookup(planned, &req.query_text)
}

async fn embed_probe_query(embedder: &dyn EmbeddingProvider, probe: &Probe) -> Result<Vec<f32>> {
    let query_input = vec![probe.query.clone()];
    let mut embeddings = embedder.embed(&query_input).await.map_err(|error| {
        EvalError::InvalidConfig(format!(
            "memory query embedding failed for probe {}: {error}",
            probe.probe_id
        ))
    })?;
    embeddings.pop().ok_or_else(|| {
        EvalError::InvalidConfig(format!(
            "memory query embedding returned no vector for probe {}",
            probe.probe_id
        ))
    })
}

async fn retrieve_probe_output(
    hybrid: &HybridRetriever,
    planned: &PlannedQuery,
    probe: &Probe,
    query_embedding: Vec<f32>,
    options: ProbeHitOptions,
) -> Result<RetrievalOutput> {
    let request = probe_retrieval_request(planned, probe, query_embedding, options);
    hybrid
        .retrieve_with_diagnostics(request)
        .await
        .map_err(|error| memory_retrieval_error(probe, error))
}

fn probe_retrieval_request(
    planned: &PlannedQuery,
    probe: &Probe,
    query_embedding: Vec<f32>,
    options: ProbeHitOptions,
) -> RetrievalRequest {
    let mut request = planned.clone().into_retrieval_request(
        &probe.query,
        query_embedding,
        PiiClass::Restricted,
        options.k_final,
        options.use_reranker,
    );
    request.ranking_reference_time = options.ranking_reference_time;
    request.disable_leg_timeouts = true;
    request.disable_graph_expansion = options.force_graph_off
        || should_skip_graph_expansion_for_direct_lookup(planned, &request.query_text)
        || (options.graph_expansion_policy == GraphExpansionEvalPolicy::SkipExactDirect
            && should_skip_graph_expansion_for_exact_direct_probe(planned, &request));
    request
}

fn memory_retrieval_error(
    probe: &Probe,
    error: impl std::fmt::Display,
) -> moa_eval_core::EvalError {
    EvalError::InvalidConfig(format!(
        "memory retrieval failed for probe {}: {error}",
        probe.probe_id
    ))
}

fn should_compare_graph(policy: GraphRetrievalPolicy) -> bool {
    !matches!(
        policy,
        GraphRetrievalPolicy::Off | GraphRetrievalPolicy::ContextOnly
    )
}

fn probe_graph_comparison(
    expected_fact_ids: &[String],
    graph_candidates: &[RetrievedCandidate],
    graph_off_candidates: Vec<RetrievedCandidate>,
    graph_diagnostics: &GraphRetrievalDiagnostics,
    graph_off_retrieval_latency_ms: u64,
) -> ProbeGraphComparison {
    let relevant_rank_with_graph = first_relevant_rank(graph_candidates, expected_fact_ids);
    let relevant_rank_without_graph = first_relevant_rank(&graph_off_candidates, expected_fact_ids);
    let impact = classify_graph_impact(relevant_rank_with_graph, relevant_rank_without_graph);
    let top_harmful_graph_paths = if impact == GraphImpact::Hurt {
        top_harmful_graph_paths(graph_diagnostics, graph_candidates, expected_fact_ids)
    } else {
        Vec::new()
    };
    ProbeGraphComparison {
        impact,
        relevant_rank_with_graph,
        relevant_rank_without_graph,
        rank_delta_with_minus_without: rank_delta(
            relevant_rank_with_graph,
            relevant_rank_without_graph,
        ),
        graph_off_candidates,
        top_harmful_graph_paths,
        graph_off_retrieval_latency_ms,
    }
}

fn classify_graph_impact(
    rank_with_graph: Option<usize>,
    rank_without_graph: Option<usize>,
) -> GraphImpact {
    match rank_order_value(rank_with_graph).cmp(&rank_order_value(rank_without_graph)) {
        std::cmp::Ordering::Less => GraphImpact::Rescue,
        std::cmp::Ordering::Equal => GraphImpact::Neutral,
        std::cmp::Ordering::Greater => GraphImpact::Hurt,
    }
}

fn rank_order_value(rank: Option<usize>) -> usize {
    rank.unwrap_or(usize::MAX)
}

fn rank_delta(rank_with_graph: Option<usize>, rank_without_graph: Option<usize>) -> Option<i64> {
    match (rank_with_graph, rank_without_graph) {
        (Some(with_graph), Some(without_graph)) => Some(with_graph as i64 - without_graph as i64),
        _ => None,
    }
}

fn first_relevant_rank(
    candidates: &[RetrievedCandidate],
    expected_fact_ids: &[String],
) -> Option<usize> {
    let expected = expected_fact_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    if expected.is_empty() {
        return None;
    }
    candidates
        .iter()
        .filter(|candidate| candidate_matches_expected(candidate, &expected))
        .map(|candidate| candidate.rank)
        .min()
}

fn candidate_matches_expected(
    candidate: &RetrievedCandidate,
    expected: &std::collections::BTreeSet<&str>,
) -> bool {
    candidate
        .fact_id
        .as_deref()
        .is_some_and(|fact_id| expected.contains(fact_id))
        || candidate
            .equivalent_fact_ids
            .iter()
            .any(|fact_id| expected.contains(fact_id.as_str()))
}

fn top_harmful_graph_paths(
    diagnostics: &GraphRetrievalDiagnostics,
    graph_candidates: &[RetrievedCandidate],
    expected_fact_ids: &[String],
) -> Vec<ProbeGraphPathDiagnostic> {
    let expected = expected_fact_ids
        .iter()
        .map(String::as_str)
        .collect::<std::collections::BTreeSet<_>>();
    let contexts = graph_candidate_contexts(graph_candidates);
    let harmful_uids = graph_candidates
        .iter()
        .filter(|candidate| candidate.legs.graph)
        .filter(|candidate| !candidate_matches_expected(candidate, &expected))
        .map(|candidate| candidate.uid)
        .collect::<std::collections::HashSet<_>>();
    let graph_uids = graph_candidates
        .iter()
        .filter(|candidate| candidate.legs.graph)
        .map(|candidate| candidate.uid)
        .collect::<std::collections::HashSet<_>>();
    let mut paths =
        graph_path_diagnostics_for_candidates(&diagnostics.path_traces, &contexts, &harmful_uids);
    if paths.is_empty() {
        paths =
            graph_path_diagnostics_for_candidates(&diagnostics.path_traces, &contexts, &graph_uids);
    }
    if paths.is_empty() {
        paths = diagnostics
            .path_traces
            .iter()
            .map(ProbeGraphPathDiagnostic::from)
            .collect();
    }
    paths.sort_by(|left, right| {
        left.candidate_rank_with_graph
            .unwrap_or(usize::MAX)
            .cmp(&right.candidate_rank_with_graph.unwrap_or(usize::MAX))
            .then_with(|| left.hop.cmp(&right.hop))
            .then_with(|| left.seed_uid.cmp(&right.seed_uid))
            .then_with(|| left.candidate_uid.cmp(&right.candidate_uid))
            .then_with(|| left.edge_labels.cmp(&right.edge_labels))
    });
    paths.truncate(5);
    paths
}

fn graph_candidate_contexts(
    candidates: &[RetrievedCandidate],
) -> HashMap<Uuid, ProbeGraphCandidateContext> {
    let mut contexts = HashMap::new();
    for candidate in candidates.iter().filter(|candidate| candidate.legs.graph) {
        contexts
            .entry(candidate.uid)
            .or_insert(ProbeGraphCandidateContext {
                rank: candidate.rank,
                fact_id: candidate.fact_id.clone(),
            });
    }
    contexts
}

fn graph_path_diagnostics_for_candidates(
    traces: &[GraphPathTrace],
    contexts: &HashMap<Uuid, ProbeGraphCandidateContext>,
    candidate_uids: &std::collections::HashSet<Uuid>,
) -> Vec<ProbeGraphPathDiagnostic> {
    traces
        .iter()
        .filter(|trace| candidate_uids.contains(&trace.candidate_uid))
        .map(|trace| {
            let context = contexts.get(&trace.candidate_uid);
            ProbeGraphPathDiagnostic {
                seed_uid: trace.seed_uid,
                seed_source: trace.seed_source,
                candidate_uid: trace.candidate_uid,
                candidate_rank_with_graph: context.map(|context| context.rank),
                candidate_fact_id: context.and_then(|context| context.fact_id.clone()),
                hop: trace.hop,
                edge_labels: trace.edge_labels.clone(),
            }
        })
        .collect()
}

struct ProbeGraphCandidateContext {
    rank: usize,
    fact_id: Option<String>,
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
    graph_diagnostics: GraphRetrievalDiagnostics,
    graph_off_hits: Vec<RetrievalHit>,
    graph_off_retrieval_latency_ms: Option<u64>,
    retrieval_latency_ms: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProbeHitOptions {
    k_final: usize,
    use_reranker: bool,
    ranking_reference_time: Option<DateTime<Utc>>,
    graph_expansion_policy: GraphExpansionEvalPolicy,
    force_graph_off: bool,
}

fn duration_ms_u64(elapsed: std::time::Duration) -> u64 {
    elapsed.as_millis().min(u128::from(u64::MAX)) as u64
}

struct ProbeResultInput<'a> {
    probe: &'a Probe,
    candidates: Vec<RetrievedCandidate>,
    post_rerank_candidates: Option<Vec<RetrievedCandidate>>,
    retrieval_latency_ms: u64,
    gold_records_by_fact_id: &'a HashMap<String, super::GoldNodeRecord>,
    preference_context_hit: Option<bool>,
    graph_diagnostics: Option<GraphRetrievalDiagnostics>,
    graph_comparison: Option<ProbeGraphComparison>,
}

fn probe_result_for(input: ProbeResultInput<'_>) -> Result<ProbeResult> {
    let ProbeResultInput {
        probe,
        candidates,
        post_rerank_candidates,
        retrieval_latency_ms,
        gold_records_by_fact_id,
        preference_context_hit,
        graph_diagnostics,
        graph_comparison,
    } = input;
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
    let judge_outcome = deterministic_judge_outcome_for_probe(
        probe,
        expected_found_at_4,
        blocked_leaked,
        pii_redacted,
    )?;
    let answer_faithful = judge_outcome
        .as_ref()
        .and_then(|outcome| outcome.answer_faithful)
        .or_else(|| retrieval_answer_faithful_for_probe(probe, expected_found_at_4));
    let abstention_correct = judge_outcome
        .as_ref()
        .and_then(|outcome| outcome.abstention_correct);
    let pii_redacted = judge_outcome
        .as_ref()
        .and_then(|outcome| outcome.pii_redacted)
        .or(pii_redacted);
    let temporal_as_of_correct = judge_outcome
        .as_ref()
        .and_then(|outcome| outcome.temporal_as_of_correct);
    let (temporal_filter_parsed, temporal_filter_matches_as_of) = temporal_parse_diagnostics(probe);

    Ok(ProbeResult {
        probe_id: probe.probe_id.clone(),
        user_id: probe.user_id.as_str().to_string(),
        probe_type: probe.probe_type,
        expected_fact_ids: probe.expected_fact_ids.clone(),
        blocked_fact_ids: probe.blocked_fact_ids.clone(),
        candidates,
        post_rerank_candidates,
        retrieval_latency_ms,
        answer_faithful,
        abstention_correct,
        pii_redacted,
        temporal_as_of_correct,
        temporal_filter_parsed,
        temporal_filter_matches_as_of,
        preference_context_hit,
        graph_diagnostics,
        graph_comparison,
    })
}

fn retrieval_answer_faithful_for_probe(probe: &Probe, expected_found_at_4: bool) -> Option<bool> {
    match probe.probe_type {
        _ if probe.expected_fact_ids.is_empty() => None,
        _ => Some(expected_found_at_4),
    }
}

fn deterministic_judge_outcome_for_probe(
    probe: &Probe,
    expected_found_at_4: bool,
    blocked_leaked: bool,
    pii_redacted: Option<bool>,
) -> Result<Option<JudgeOutcome>> {
    if !deterministic_judge_supports(probe.probe_type) {
        return Ok(None);
    }

    let mut input = JudgeInput::new(
        probe.probe_type,
        probe.answer.clone(),
        candidate_answer_for_deterministic_judge(probe, expected_found_at_4, blocked_leaked),
    )
    .with_query(probe.query.clone());

    if matches!(
        probe.probe_type,
        ProbeType::Abstention | ProbeType::CrossUserIsolation
    ) {
        input = input.with_abstained(!blocked_leaked);
    }
    if probe.probe_type == ProbeType::PiiRedaction {
        input = input.with_expected_redacted(probe.expected_redacted);
        if let Some(redacted) = pii_redacted {
            input = input.with_pii_redacted(redacted);
        }
    }

    DeterministicJudge::new().judge_sync(&input).map(Some)
}

fn deterministic_judge_supports(probe_type: ProbeType) -> bool {
    !matches!(
        probe_type,
        ProbeType::MultiHop | ProbeType::PreferenceApplication
    )
}

fn candidate_answer_for_deterministic_judge(
    probe: &Probe,
    expected_found_at_4: bool,
    blocked_leaked: bool,
) -> String {
    match probe.probe_type {
        ProbeType::Abstention | ProbeType::CrossUserIsolation if !blocked_leaked => {
            "I do not have enough information to answer.".to_string()
        }
        ProbeType::Abstention | ProbeType::CrossUserIsolation => {
            "blocked memory leaked".to_string()
        }
        _ if expected_found_at_4 => probe.answer.clone(),
        _ => String::new(),
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
                && candidate_fact_ids(candidate).any(|fact_id| fact_id == expected_fact_id)
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
            && candidate_fact_ids(candidate)
                .any(|fact_id| blocked.iter().any(|blocked| blocked == fact_id))
    })
}

fn candidate_fact_ids(candidate: &RetrievedCandidate) -> impl Iterator<Item = &str> {
    candidate
        .fact_id
        .as_deref()
        .into_iter()
        .chain(candidate.equivalent_fact_ids.iter().map(String::as_str))
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
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    if storage_partition_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT target_uid, (payload->>'replacement_uid')::uuid AS replacement_uid
        FROM moa.graph_changelog
        WHERE storage_partition_id = ANY($1)
          AND op = 'supersede'
          AND target_kind = 'node'
          AND target_label = 'Fact'
          AND payload ? 'replacement_uid'
        ORDER BY change_id ASC
        "#,
    )
    .bind(&storage_partition_ids)
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
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    let mut outcome = ConsolidationOutcome::default();
    for storage_partition_id in &storage_partition_ids {
        let tenant_id = tenant_id_from_storage_partition(storage_partition_id);
        let workspace_outcome = moa_memory_lifecycle::consolidate_tenant(
            pool,
            tenant_id,
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
                "memory consolidation failed for storage partition {storage_partition_id}: {error}"
            ))
        })?;
        add_consolidation_outcome(&mut outcome, workspace_outcome);
    }

    verify_restatement_pairs_collapsed(pool, ledger, gold_resolution, fact_ids_by_uid).await?;

    let mut second = ConsolidationOutcome::default();
    for storage_partition_id in &storage_partition_ids {
        let tenant_id = tenant_id_from_storage_partition(storage_partition_id);
        let second_outcome = moa_memory_lifecycle::consolidate_tenant(
            pool,
            tenant_id,
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
                "second memory consolidation failed for storage partition {storage_partition_id}: {error}"
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
    for storage_partition_id in eval_storage_partition_ids(ledger) {
        let tenant_id = tenant_id_from_storage_partition(&storage_partition_id);
        let stats = moa_memory_lifecycle::rebuild_digests(
            pool,
            &tenant_id,
            reference_time,
            &config,
        )
        .await
        .map_err(|error| {
            EvalError::InvalidConfig(format!(
                "memory digest rebuild failed for storage partition {storage_partition_id}: {error}"
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
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    let total_fact_nodes = if storage_partition_ids.is_empty() {
        0_i64
    } else {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM moa.node_index WHERE label = 'Fact' AND storage_partition_id = ANY($1)",
        )
        .bind(&storage_partition_ids)
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
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    let active_entity_nodes = if storage_partition_ids.is_empty() {
        0_i64
    } else {
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COUNT(*)
            FROM moa.node_index
            WHERE label = 'Entity'
              AND valid_to IS NULL
              AND storage_partition_id = ANY($1)
            "#,
        )
        .bind(&storage_partition_ids)
        .fetch_one(pool)
        .await?
    };
    let distinct_ledger_mentions = ledger
        .iter()
        .flat_map(|fact| {
            [&fact.subject, &fact.object].into_iter().map(|mention| {
                let user_id = match fact.scope {
                    ScopeTier::Contact => fact.user_id.to_string(),
                    ScopeTier::Tenant => String::new(),
                };
                (
                    scope_tier_name(fact.scope).to_string(),
                    fact.storage_partition_id.to_string(),
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
        ScopeTier::Tenant => "tenant",
        ScopeTier::Contact => "contact",
    }
}

fn contact_id_from_user_id(user_id: &UserId) -> ContactId {
    uuid::Uuid::parse_str(user_id.as_str())
        .map(ContactId)
        .unwrap_or_else(|_| ContactId(stable_uuid_from_label(user_id.as_str())))
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
    let storage_partition_ids = eval_storage_partition_ids(ledger);
    if storage_partition_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query(
        r#"
        SELECT storage_partition_id, user_id, scope, content
        FROM moa.memory_digests
        WHERE storage_partition_id = ANY($1)
        ORDER BY storage_partition_id ASC, CASE scope WHEN 'user' THEN 0 ELSE 1 END, user_id ASC NULLS FIRST
        "#,
    )
    .bind(&storage_partition_ids)
    .fetch_all(pool)
    .await?;
    let mut tenant_content = HashMap::<String, String>::new();
    let mut user_content = HashMap::<(String, String), String>::new();
    for row in rows {
        let storage_partition_id: String = row.try_get("storage_partition_id")?;
        let user_id: Option<String> = row.try_get("user_id")?;
        let scope: String = row.try_get("scope")?;
        let content: String = row.try_get("content")?;
        if scope == "tenant" {
            tenant_content.insert(storage_partition_id, content);
        } else if scope == "contact"
            && let Some(user_id) = user_id
        {
            user_content.insert((storage_partition_id, user_id), content);
        }
    }

    let users = ledger
        .iter()
        .map(|fact| {
            (
                fact.storage_partition_id.to_string(),
                fact.user_id.to_string(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut contexts = HashMap::new();
    for (storage_partition_id, user_id) in users {
        let mut content = String::new();
        if let Some(user_digest) =
            user_content.get(&(storage_partition_id.clone(), user_id.clone()))
        {
            content.push_str(user_digest);
            content.push('\n');
        }
        if let Some(tenant_digest) = tenant_content.get(&storage_partition_id) {
            content.push_str(tenant_digest);
        }
        contexts.insert((storage_partition_id, user_id), content);
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
        .get(&(
            probe.storage_partition_id.to_string(),
            probe.user_id.to_string(),
        ))
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
                tenant_id: tenant_id_from_storage_partition_id(&session.storage_partition_id),
                contact_id: Some(contact_id_from_user_id(&session.user_id)),
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
        let tenant_id = tenant_id_from_label(&format!("memory-eval-runner-{}", self.schema_name));
        let scope = RlsContext::tenant(tenant_id);
        let vector = Arc::new(PgvectorStore::new_for_app_role(
            self.pool().clone(),
            scope.clone(),
        ));
        let graph = Arc::new(
            PostgresGraphStore::scoped_for_app_role(self.pool().clone(), scope)
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
    env::var("MOA_DATABASE_URL").map_err(|_| {
        EvalError::InvalidConfig(
            "MOA_DATABASE_URL must be set for memory retrieval eval".to_string(),
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
            spans.push(PiiSpan::new(start, end, PiiCategory::Email, 0.95));
        } else if token.contains("sk-") || token.to_ascii_lowercase().contains("secret") {
            spans.push(PiiSpan::new(start, end, PiiCategory::Secret, 0.90));
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
        _pii_class: PiiClass,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_eval::{
        CorpusProfile, generate_memory_eval_corpus, write_memory_eval_corpus,
    };
    use moa_brain::planning::Strategy;
    use moa_core::{SessionId, StoragePartitionId, TenantId};

    #[derive(Debug)]
    struct PrDeterministicEmbedder;

    #[async_trait]
    impl EmbeddingProvider for PrDeterministicEmbedder {
        fn model_id(&self) -> &str {
            "memory-eval-deterministic-sha256-v1"
        }

        fn dimensions(&self) -> usize {
            VECTOR_DIMENSION
        }

        fn model_version(&self) -> i32 {
            7
        }

        async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
            Ok(vec![vec![0.0; VECTOR_DIMENSION]; inputs.len()])
        }
    }

    #[test]
    fn graph_expansion_current_uses_guarded_default_and_legacy_is_explicit() {
        // Pins: memory-eval reports can still request legacy broad expansion,
        // but the ambient current lane follows the guarded production default.
        assert_eq!(
            GraphExpansionEvalPolicy::Current.graph_retrieval_policy(),
            GraphRetrievalPolicy::AnchoredRescue
        );
        assert_eq!(
            GraphExpansionEvalPolicy::SkipExactDirect.graph_retrieval_policy(),
            GraphRetrievalPolicy::AnchoredRescue
        );
        assert_eq!(
            GraphExpansionEvalPolicy::LegacyBroadExpansion.graph_retrieval_policy(),
            GraphRetrievalPolicy::LegacyBroadExpansion
        );
    }

    #[test]
    fn graph_expansion_policy_skips_only_exact_direct_non_temporal_probes() {
        // Pins: the A/B policy only disables graph expansion for direct exact-anchor lookups.
        let planned = planned_for_policy(Strategy::Both, None);
        let req = request_for_policy("Who owns incident INC-123?");

        assert!(should_skip_graph_expansion_for_exact_direct_probe(
            &planned, &req
        ));
    }

    #[test]
    fn graph_expansion_policy_keeps_graph_first_and_temporal_probes() {
        // Pins: multi-hop and historical probes still run graph expansion in the A/B lane.
        let graph_first = planned_for_policy(Strategy::GraphFirst, None);
        let temporal = planned_for_policy(
            Strategy::Both,
            Some(
                DateTime::parse_from_rfc3339("2026-03-01T00:00:00Z")
                    .expect("test timestamp should parse")
                    .with_timezone(&Utc),
            ),
        );
        let req = request_for_policy("What depends on incident INC-123?");

        assert!(!should_skip_graph_expansion_for_exact_direct_probe(
            &graph_first,
            &req
        ));
        assert!(!should_skip_graph_expansion_for_exact_direct_probe(
            &temporal, &req
        ));
    }

    #[test]
    fn graph_expansion_policy_does_not_treat_contractions_as_exact_anchors() {
        // Pins: natural prose with apostrophes is not an exact-anchor lookup.
        let planned = planned_for_policy(Strategy::Both, None);
        let req = request_for_policy("What's failing in the deploy flow?");

        assert!(!should_skip_graph_expansion_for_exact_direct_probe(
            &planned, &req
        ));
    }

    #[test]
    fn probe_graph_comparison_classifies_hurt_and_keeps_path_identity() {
        // Pins: memory eval graph A/B diagnostics keep the seed and path behind hurt probes.
        use crate::memory_eval::CandidateLegs;
        use moa_brain::retrieval::{
            GraphCandidateCounts, GraphPathTrace, GraphSeedDiagnostics, GraphSeedSource,
        };

        let seed_uid = Uuid::from_u128(0x4_0000);
        let harmful_uid = Uuid::from_u128(0x5_0001);
        let relevant_uid = Uuid::from_u128(0x5_0002);
        let graph_candidates = vec![
            RetrievedCandidate {
                uid: harmful_uid,
                rank: 1,
                score: 0.9,
                fact_id: Some("fact-wrong".to_string()),
                equivalent_fact_ids: Vec::new(),
                legs: CandidateLegs {
                    graph: true,
                    vector: false,
                    lexical: false,
                    lexical_backend: None,
                },
            },
            RetrievedCandidate {
                uid: relevant_uid,
                rank: 2,
                score: 0.8,
                fact_id: Some("fact-right".to_string()),
                equivalent_fact_ids: Vec::new(),
                legs: CandidateLegs {
                    graph: false,
                    vector: true,
                    lexical: false,
                    lexical_backend: None,
                },
            },
        ];
        let graph_off_candidates = vec![RetrievedCandidate {
            uid: relevant_uid,
            rank: 1,
            score: 1.0,
            fact_id: Some("fact-right".to_string()),
            equivalent_fact_ids: Vec::new(),
            legs: CandidateLegs {
                graph: false,
                vector: true,
                lexical: false,
                lexical_backend: None,
            },
        }];
        let diagnostics = GraphRetrievalDiagnostics {
            policy: GraphRetrievalPolicy::LegacyBroadExpansion,
            seed_counts: GraphSeedDiagnostics {
                broad_fallback: 1,
                ..GraphSeedDiagnostics::default()
            },
            path_label_histogram: BTreeMap::from([("RELATED_TO".to_string(), 1)]),
            hop_histogram: BTreeMap::from([(1, 1)]),
            path_traces: vec![GraphPathTrace {
                seed_uid,
                seed_source: Some(GraphSeedSource::BroadFallback),
                candidate_uid: harmful_uid,
                hop: 1,
                edge_labels: vec!["RELATED_TO".to_string()],
                edge_directions: vec!["outgoing".to_string()],
            }],
            candidate_counts: GraphCandidateCounts {
                graph_only: 1,
                ..GraphCandidateCounts::default()
            },
            article_ranking: moa_brain::retrieval::ArticleRankingDiagnostics::default(),
            graph_latency_ms: 9,
            raw_path_count: 1,
        };

        let comparison = probe_graph_comparison(
            &["fact-right".to_string()],
            &graph_candidates,
            graph_off_candidates,
            &diagnostics,
            4,
        );

        assert_eq!(comparison.impact, GraphImpact::Hurt);
        assert_eq!(comparison.relevant_rank_with_graph, Some(2));
        assert_eq!(comparison.relevant_rank_without_graph, Some(1));
        assert_eq!(comparison.rank_delta_with_minus_without, Some(1));
        assert_eq!(comparison.top_harmful_graph_paths.len(), 1);
        let path = &comparison.top_harmful_graph_paths[0];
        assert_eq!(path.seed_uid, seed_uid);
        assert_eq!(path.seed_source, Some(GraphSeedSource::BroadFallback));
        assert_eq!(path.candidate_uid, harmful_uid);
        assert_eq!(path.candidate_rank_with_graph, Some(1));
        assert_eq!(path.candidate_fact_id.as_deref(), Some("fact-wrong"));
        assert_eq!(path.hop, 1);
        assert_eq!(path.edge_labels, vec!["RELATED_TO".to_string()]);
    }

    fn planned_for_policy(
        strategy: Strategy,
        temporal_filter: Option<DateTime<Utc>>,
    ) -> PlannedQuery {
        let scope = MemoryScope::Tenant {
            tenant_id: TenantId::new(),
        };
        PlannedQuery {
            strategy,
            seeds: Vec::new(),
            label_hint: None,
            scope: scope.clone(),
            temporal_filter,
        }
    }

    fn request_for_policy(query_text: &str) -> RetrievalRequest {
        RetrievalRequest {
            seeds: Vec::new(),
            query_text: query_text.to_string(),
            query_embedding: vec![0.0; VECTOR_DIMENSION],
            scope: MemoryScope::Tenant {
                tenant_id: TenantId::new(),
            },
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: RETRIEVAL_EVAL_FINAL_K,
            use_reranker: false,
            strategy: Some(Strategy::Both),
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: true,
            disable_graph_expansion: false,
        }
    }

    fn ledger_fact(storage_partition_id: StoragePartitionId, fact_id: &str) -> LedgerFact {
        LedgerFact {
            storage_partition_id,
            user_id: UserId::new("user"),
            scope: ScopeTier::Tenant,
            fact_id: fact_id.to_string(),
            valid_from: Utc::now(),
            valid_to: None,
            subject: "eval".to_string(),
            predicate: "uses_embedder".to_string(),
            object: "deterministic".to_string(),
            answer: "Eval uses the deterministic embedder.".to_string(),
            supersedes: Vec::new(),
            restates: None,
            prior_uses: None,
            prior_successes: None,
            source_session_id: SessionId::new(),
            source_turn_seq: 1,
            pii_class: PiiClass::None,
            expected_redacted: false,
        }
    }

    #[test]
    fn live_lane_validation_defers_credentials_to_configured_provider_builders() {
        // Pins: live eval option validation checks lane shape only; provider
        // credentials are loaded through MoaConfig and validated by provider builders.
        let options =
            MemoryRetrievalEvalOptions::new("target/missing-corpus", "target/report.json")
                .with_lane(EvalLane::Live);

        options.validate().expect("lane shape should be valid");
    }

    #[test]
    fn pr_lane_refuses_live_only_flags() {
        // Pins: PR eval cannot accept budget-only live-lane flags and pretend it ran hermetically.
        let options =
            MemoryRetrievalEvalOptions::new("target/missing-corpus", "target/report.json")
                .with_budget_usd(1.0);

        let error = options
            .validate()
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

    #[test]
    fn rewrite_accounting_gates_by_query_class() {
        // Pins: gated PR rewrite policy records fewer calls than always and preserves exact controls.
        let mut always = QueryRewriteAccounting::new(QueryRewritePolicy::Always);
        let mut gated = QueryRewriteAccounting::new(QueryRewritePolicy::Gated);
        let explicit = Probe {
            probe_id: "explicit".to_string(),
            probe_type: ProbeType::PointRecall,
            storage_partition_id: StoragePartitionId::new("workspace"),
            user_id: moa_core::UserId::new("user"),
            query: "Which runbook is required for deploy?".to_string(),
            rewrite_query: None,
            expected_rewrite: None,
            query_class: None,
            answer: "Use the tenant deploy runbook.".to_string(),
            expected_fact_ids: Vec::new(),
            blocked_fact_ids: Vec::new(),
            as_of: None,
            expected_redacted: false,
        };
        let exact = Probe {
            probe_id: "exact".to_string(),
            query: "Find docs/runbook.md".to_string(),
            ..explicit.clone()
        };
        let multi_hop = Probe {
            probe_id: "multi-hop".to_string(),
            probe_type: ProbeType::MultiHop,
            query: "Which team owns the library that api depends on?".to_string(),
            ..explicit.clone()
        };

        for probe in [&explicit, &exact, &multi_hop] {
            always.record(probe);
            gated.record(probe);
        }
        let always = always.summary();
        let gated = gated.summary();

        assert_eq!(always.call_count, 3);
        assert_eq!(gated.call_count, 1);
        assert_eq!(gated.skip_count, 2);
        assert_eq!(
            gated
                .by_class
                .get("exact_identifier")
                .expect("exact class should be recorded")
                .call_count,
            0
        );
    }

    #[test]
    fn retrieval_runner_scores_policy_probes_through_deterministic_judge() {
        // Pins: hermetic retrieval eval reuses the memory-eval judge policy for answer outcomes.
        let probe = Probe {
            probe_id: "probe-pii".to_string(),
            probe_type: ProbeType::PiiRedaction,
            storage_partition_id: StoragePartitionId::new("workspace"),
            user_id: moa_core::UserId::new("user"),
            query: "What is Alice's phone?".to_string(),
            rewrite_query: None,
            expected_rewrite: None,
            query_class: None,
            answer: "Alice's phone is [PHONE].".to_string(),
            expected_fact_ids: vec!["fact-phone".to_string()],
            blocked_fact_ids: Vec::new(),
            as_of: None,
            expected_redacted: true,
        };

        let unredacted = deterministic_judge_outcome_for_probe(&probe, true, false, Some(false))
            .expect("judge should score deterministic PII probe")
            .expect("PII probe should be deterministic");
        let redacted = deterministic_judge_outcome_for_probe(&probe, true, false, Some(true))
            .expect("judge should score deterministic PII probe")
            .expect("PII probe should be deterministic");

        assert_eq!(unredacted.answer_faithful, Some(false));
        assert_eq!(unredacted.pii_redacted, Some(false));
        assert_eq!(redacted.answer_faithful, Some(true));
        assert_eq!(redacted.pii_redacted, Some(true));
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

    #[tokio::test]
    #[ignore = "requires MOA_DATABASE_URL and local Postgres"]
    async fn eval_seed_sets_pr_embedder_state_before_ingestion_db_memory() {
        // Pins: eval ingestion partitions are configured to the active PR embedder before vector writes.
        let store = IsolatedEvalStore::create()
            .await
            .expect("create isolated eval store");
        let storage_partition_a =
            StoragePartitionId::new(format!("memory-eval-pr-seed-a-{}", Uuid::now_v7()));
        let storage_partition_b =
            StoragePartitionId::new(format!("memory-eval-pr-seed-b-{}", Uuid::now_v7()));
        let ledger = vec![
            ledger_fact(storage_partition_a.clone(), "fact-a"),
            ledger_fact(storage_partition_b.clone(), "fact-b"),
            ledger_fact(storage_partition_b.clone(), "fact-b-duplicate-partition"),
        ];

        seed_eval_storage_partition_embedder_state(store.pool(), &ledger, &PrDeterministicEmbedder)
            .await
            .expect("seed eval storage partition state");

        let storage_partition_ids = vec![
            tenant_id_from_storage_partition_id(&storage_partition_a).to_string(),
            tenant_id_from_storage_partition_id(&storage_partition_b).to_string(),
        ];
        let rows = sqlx::query(
            r#"
            SELECT storage_partition_id, embedding_model, embedding_model_version, embedding_dimension
            FROM moa.storage_partition_state
            WHERE storage_partition_id = ANY($1)
            ORDER BY storage_partition_id ASC
            "#,
        )
        .bind(&storage_partition_ids)
        .fetch_all(store.pool())
        .await
        .expect("read seeded storage partition state");

        assert_eq!(rows.len(), 2);
        for row in rows {
            let model: String = row.try_get("embedding_model").expect("model column");
            let version: i32 = row
                .try_get("embedding_model_version")
                .expect("model version column");
            let dimension: i32 = row
                .try_get("embedding_dimension")
                .expect("embedding dimension column");
            assert_eq!(model, "memory-eval-deterministic-sha256-v1");
            assert_ne!(model, "cohere-embed-v4");
            assert_ne!(model, "embed-v4.0");
            assert_eq!(version, 7);
            assert_eq!(dimension, VECTOR_DIMENSION as i32);
        }

        sqlx::query("DELETE FROM moa.storage_partition_state WHERE storage_partition_id = ANY($1)")
            .bind(&storage_partition_ids)
            .execute(store.pool())
            .await
            .expect("delete seeded storage partition state rows");
        store.cleanup().await.expect("cleanup isolated eval store");
    }
}
