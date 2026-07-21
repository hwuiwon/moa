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
    EvidenceWindowPolicy, GraphPathTrace, GraphRetrievalDiagnostics, GraphRetrievalPolicy,
    HybridRetriever, RankingConfig, RetrievalHit, RetrievalOutput, RetrievalRequest,
};
use moa_core::types::memory::RlsContext;
use moa_core::types::security::SensitivityClass;
use moa_core::{
    config::MemoryDigestConfig, config::MemoryRankingConfig, config::MoaConfig,
    traits::EmbeddingProvider, types::contact::ContactId, types::identifiers::UserId,
};
use moa_crypto::{KeyManagementProvider, LocalKmsProvider};
use moa_db::ScopedConn;
use moa_memory_graph::{GraphStore, NodeIndexRow, PostgresGraphStore};
use moa_memory_ingest::{
    Conflict, ContradictionContext, ContradictionDetector, DeterministicEntityMergeVerifier,
    EXTRACTION_PROMPT_VERSION, EmbeddedFact, EntityMergeFixtureRecord, EntityMergeVerifier,
    EntityResolver, Error, ExtractedFact, ExtractionFixtureRecord, FactExtractor,
    HeuristicFactExtractor, IngestCtx, MERGE_PROMPT_VERSION, ModelEntityMergeVerifier,
    ModelFactExtractor, RecordedEntityMergeVerifier, RecordedFactExtractor, SessionTurn, TurnChunk,
    chunk_turn,
};
use moa_memory_lifecycle::{ConsolidationOptions, ConsolidationOutcome, beta_smoothed_quality};
use moa_memory_pii::{
    Error as PiiError, PiiCategory, PiiClassifier, PiiResult, PiiSpan, redact_text,
};
use moa_memory_types::{MemoryScope, ScopeTier, normalize_entity_name};
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

use super::{
    BootstrapConfig, CachedEmbeddingFixture, CachedEmbeddingProvider, CorpusManifest,
    DEFAULT_BOOTSTRAP_RESAMPLES, EmbeddingInput, ExtractionPrecisionCounts, GoldPiiStatus,
    GoldResolutionReport, GraphImpact, LedgerFact, Probe, ProbeGraphComparison,
    ProbeGraphPathDiagnostic, ProbeResult, ProbeType, RetrievedCandidate, SyntheticSession,
    candidates_from_retrieval_hits, embedding_text_hash, read_embedding_inputs_jsonl,
    read_embeddings_jsonl, read_ledger_jsonl, read_manifest_json, read_probes_jsonl,
    read_sessions_jsonl, resolve_gold_nodes, validate_corpus,
};
use super::{
    stable_uuid_from_label, tenant_id_from_label, tenant_id_from_storage_partition,
    tenant_id_from_storage_partition_id,
};
use crate::kernel::{
    CostLedger, CountingEmbedder, CountingExtractor, CountingMergeVerifier, CountingReranker,
    FixtureStore, ProviderProvenance, SharedCostLedger,
};
use moa_eval_core::{Error as EvalError, Result};

use super::io::io_error;

mod parity;
mod providers;
mod quality;
mod report;
mod retrieval;
mod rewrite;
mod scheduling;
pub(crate) mod store;
pub(crate) mod validation;

pub use report::{MemoryGraphDiagnostics, MemoryRetrievalEvalReport, QueryRewriteClassMetrics};

use parity::*;
use providers::*;
use quality::*;
use report::{ReportBuildInput, build_eval_report};
use retrieval::*;
use rewrite::{QueryRewriteAccounting, QueryRewriteSummary, probe_for_rewrite_policy};
use scheduling::run_memory_retrieval_eval_in_store;
use store::*;
use validation::*;

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
    parity: bool,
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
}

impl GraphExpansionEvalPolicy {
    /// Returns the stable CLI label for this eval policy.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::SkipExactDirect => "skip-exact-direct",
        }
    }

    /// Returns the production graph retrieval policy used by this eval lane.
    #[must_use]
    pub const fn graph_retrieval_policy(self) -> GraphRetrievalPolicy {
        match self {
            Self::Current | Self::SkipExactDirect => GraphRetrievalPolicy::AnchoredRescue,
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
            parity: false,
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

    /// Enables production-parity retrieval through the stage-7 evidence seam.
    ///
    /// Parity probes retrieve via `GraphMemoryRetriever::retrieve_evidence`
    /// (deterministic lexical router, per-scope admission, cross-scope merge,
    /// and evidence token-budget packing) instead of calling `HybridRetriever`
    /// directly, and record the rendered-context window that survived packing.
    #[must_use]
    pub fn with_parity(mut self, parity: bool) -> Self {
        self.parity = parity;
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

    /// Returns the request-scoped evidence-window policy for this eval lane.
    ///
    /// Hermetic lanes use pseudo-embeddings whose cosine values are
    /// uninformative (measured 2026-07-11: p50 0.000, max 0.26 across all
    /// window hits), so both window knobs stay off outside the live lane: a
    /// deterministic lane cannot exercise absolute-evidence abstention or a
    /// reranked-window trim, it can only distort them. The live lane rides the
    /// production `MemoryRankingConfig` defaults and is the lane that gates
    /// this behavior.
    #[must_use]
    pub fn lane_window_policy(&self) -> EvidenceWindowPolicy {
        if self.lane != EvalLane::Live {
            return EvidenceWindowPolicy::default();
        }
        let ranking = MemoryRankingConfig::default();
        EvidenceWindowPolicy {
            rerank_window: ranking.rerank_window,
            abstain_below_window_evidence: ranking.abstain_below_window_evidence,
        }
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

    /// Returns whether probes retrieve through the production stage-7 seam.
    #[must_use]
    pub fn parity(&self) -> bool {
        self.parity
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
        if self.parity && self.graph_expansion_policy != GraphExpansionEvalPolicy::Current {
            return Err(EvalError::InvalidConfig(
                "--parity drives the production graph-expansion policy; \
                 --graph-expansion-policy must be current"
                    .to_string(),
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
                    "cargo run -p xtask --features eval-tools -- record-memory-extractions --corpus {} --output {}",
                    self.corpus_dir.display(),
                    path.display()
                );
                let store = FixtureStore::<ExtractionFixtureRecord>::read_jsonl_any(
                    &path,
                    &super::recording::extraction_replay_versions(),
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
                    "cargo run -p xtask --features eval-tools -- record-memory-merges --corpus {} --output {}",
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
    super::recording::default_extractions_path(&manifest.corpus_id)
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

#[cfg(test)]
mod tests;
