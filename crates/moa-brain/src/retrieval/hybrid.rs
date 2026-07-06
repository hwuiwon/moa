//! Production hybrid graph-memory retriever.
//!
//! This remains one module because `HybridRetriever` owns the graph, vector,
//! and reranker boundary while individual retrieval legs live in `legs`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use moa_core::RlsContext;
use moa_core::{MoaConfig, SessionId};
use moa_db::ScopedConn;
use moa_lineage_core::TurnId;
use moa_memory_graph::{
    GraphError, GraphStore, NodeIndexRow, NodeLabel, PiiClass, push_validity_filter,
};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{Error as VectorError, PgvectorStore, TurbopufferStore};
use moa_providers::{ConfiguredReranker, Reranker, build_reranker_from_config};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder};
use uuid::Uuid;

use crate::planning::Strategy;
use crate::retrieval::legs::{
    GRAPH_BUDGET, GRAPH_WEIGHT, LEXICAL_BUDGET, LEXICAL_WEIGHT, LegCandidate, RRF_K, VECTOR_BUDGET,
    VECTOR_WEIGHT, begin_scoped, bump_last_accessed, graph_expansion_leg_with_diagnostics,
    hydrate_nodes, lexical_leg, rrf_fuse, timed_leg, turbopuffer_bm25_leg,
    vector_leg as run_vector_leg, write_retrieval_lineage,
};
use crate::retrieval::ranking::{
    FeatureRanker, RankingConfig, normalize_tokens, ranking_fingerprint,
};

const MIN_FUSED_CANDIDATE_LIMIT: usize = 26;
const MAX_FUSED_CANDIDATE_LIMIT: usize = 100;
const FUSED_CANDIDATE_MULTIPLIER: usize = 2;
const PHASE_ONE_GRAPH_SEED_LIMIT: usize = MIN_FUSED_CANDIDATE_LIMIT;
const PHASE_ONE_SEED_DECAY: f64 = 0.85;
const SEMANTIC_ENTITY_GRAPH_SEED_LIMIT: usize = 8;
const SEMANTIC_ENTITY_EXACT_SEED_LIMIT: usize = 3;
const SEMANTIC_ENTITY_SEED_STRENGTH: f64 = 0.90;
const MAX_FINAL_HITS_PER_KNOWLEDGE_OBJECT: usize = 2;
const ARTICLE_GRAPH_DIAGNOSTIC_LIMIT: usize = 10;
const ARTICLE_GRAPH_MAX_REPEAT_CONTRIBUTION: f64 = 0.06;
const ARTICLE_GRAPH_MAX_GRAPH_CONTRIBUTION: f64 = 0.09;
const ARTICLE_GRAPH_MAX_ADJACENT_CONTRIBUTION: f64 = 0.04;
const TURBOPUFFER_BM25_BOOST_MULTIPLIER: f64 = 0.10;

/// Result type returned by hybrid retrieval.
pub type Result<T> = std::result::Result<T, RetrievalError>;

/// Error returned by hybrid retrieval.
#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    /// Graph traversal failed.
    #[error("graph retrieval: {0}")]
    Graph(#[from] GraphError),
    /// Vector KNN failed.
    #[error("vector retrieval: {0}")]
    Vector(#[from] VectorError),
    /// Postgres sidecar access failed.
    #[error("postgres retrieval: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Scoped Postgres connection setup failed.
    #[error("scope setup: {0}")]
    Scope(#[from] moa_core::MoaError),
    /// Reranking failed.
    #[error("rerank: {0}")]
    Rerank(String),
}

/// Retrieval request supplied by the query planner.
#[derive(Debug, Clone)]
pub struct RetrievalRequest {
    /// NER seed node ids for graph traversal.
    pub seeds: Vec<Uuid>,
    /// Query text used by lexical retrieval and reranking.
    pub query_text: String,
    /// Dense query embedding used by vector retrieval.
    pub query_embedding: Vec<f32>,
    /// Request memory scope used for sidecar RLS GUCs.
    pub scope: MemoryScope,
    /// Optional graph node label allowlist.
    pub label_filter: Option<Vec<NodeLabel>>,
    /// Maximum PII class visible to the caller.
    pub max_pii_class: PiiClass,
    /// Number of final candidates to return.
    pub k_final: usize,
    /// Whether to apply Cohere-compatible reranking after RRF.
    pub use_reranker: bool,
    /// Optional planner-selected strategy for leg weighting.
    pub strategy: Option<Strategy>,
    /// Optional application-time filter for bitemporal retrieval.
    pub as_of: Option<DateTime<Utc>>,
    /// Optional deterministic clock for post-hydration ranking features.
    pub ranking_reference_time: Option<DateTime<Utc>>,
    /// Optional turn context for fire-and-forget retrieval lineage capture.
    pub lineage: Option<LineageContext>,
    /// Whether retrieval legs should run without timeout budgets.
    pub disable_leg_timeouts: bool,
    /// Whether graph expansion should be skipped for this request.
    pub disable_graph_expansion: bool,
}

/// Graph retrieval policy selected for one retriever or diagnostic run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum GraphRetrievalPolicy {
    /// Do not run graph expansion as a ranking leg.
    Off,
    /// Reserve graph structure for post-selection context organization.
    ContextOnly,
    /// Preserve the pre-guardrail broad phase-one graph expansion behavior for A/B reports.
    LegacyBroadExpansion,
    /// Use graph only to rescue candidates from precise anchors.
    #[default]
    AnchoredRescue,
    /// Use graph evidence at article/document ranking time.
    ArticleGraph,
    /// Use entity-local graph search for anchored queries.
    EntityLocalSearch,
    /// Use bounded graph propagation for multi-hop retrieval.
    Propagation,
    /// Use precomputed graph community evidence for broad queries.
    Community,
}

impl GraphRetrievalPolicy {
    /// Returns the stable report and CLI label for this policy.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::ContextOnly => "context-only",
            Self::LegacyBroadExpansion => "legacy-broad-expansion",
            Self::AnchoredRescue => "anchored-rescue",
            Self::ArticleGraph => "article-graph",
            Self::EntityLocalSearch => "entity-local-search",
            Self::Propagation => "propagation",
            Self::Community => "community",
        }
    }

    /// Parses the stable CLI label for this policy.
    #[must_use]
    pub fn from_str_label(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "context-only" => Some(Self::ContextOnly),
            "legacy-broad-expansion" => Some(Self::LegacyBroadExpansion),
            "anchored-rescue" => Some(Self::AnchoredRescue),
            "article-graph" => Some(Self::ArticleGraph),
            "entity-local-search" => Some(Self::EntityLocalSearch),
            "propagation" => Some(Self::Propagation),
            "community" => Some(Self::Community),
            _ => None,
        }
    }

    const fn disables_graph_ranking(self) -> bool {
        matches!(self, Self::Off | Self::ContextOnly)
    }

    const fn allows_broad_phase_one_fallback(self) -> bool {
        matches!(self, Self::LegacyBroadExpansion)
    }

    const fn allows_semantic_entity_seeds(self) -> bool {
        matches!(
            self,
            Self::EntityLocalSearch | Self::Propagation | Self::Community
        )
    }

    const fn allows_graph_only_rescue_bonus(self) -> bool {
        matches!(self, Self::LegacyBroadExpansion)
    }
}

/// Source that admitted a graph traversal seed for one retrieval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GraphSeedSource {
    /// Planner-provided seed from query analysis.
    Planner,
    /// Exact phase-one vector or lexical seed.
    ExactPhaseOne,
    /// Broad top phase-one fallback seed.
    BroadFallback,
    /// Semantic entity seed.
    SemanticEntity,
}

/// Seed counts grouped by graph seed source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphSeedDiagnostics {
    /// Planner-provided seed count.
    pub planner: usize,
    /// Exact phase-one vector or lexical seed count.
    pub exact_phase_one: usize,
    /// Broad top phase-one fallback seed count.
    pub broad_fallback: usize,
    /// Semantic entity seed count.
    pub semantic_entity: usize,
}

/// Candidate counts grouped by graph leg overlap.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphCandidateCounts {
    /// Candidates found only by graph.
    pub graph_only: usize,
    /// Candidates found by graph and vector, but not lexical.
    pub vector_graph: usize,
    /// Candidates found by graph and lexical, but not vector.
    pub lexical_graph: usize,
    /// Candidates found by graph, vector, and lexical.
    pub all_legs: usize,
}

impl GraphCandidateCounts {
    /// Adds another count bucket into this one.
    pub fn add(&mut self, other: Self) {
        self.graph_only += other.graph_only;
        self.vector_graph += other.vector_graph;
        self.lexical_graph += other.lexical_graph;
        self.all_legs += other.all_legs;
    }
}

/// Per-feature contribution that produced one article-level score.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ArticleFeatureContributions {
    /// Highest hydrated chunk score before article-level aggregation.
    pub max_fused_score: f64,
    /// Lexical hit and title/heading token overlap contribution.
    pub lexical_title: f64,
    /// Contribution from multiple chunks from the same article.
    pub same_article_repeat: f64,
    /// Contribution from an exact title-like query match.
    pub exact_title_match: f64,
    /// Contribution from non-structural graph paths into article chunks.
    pub typed_graph_evidence: f64,
    /// Contribution from chunks near the best same-article chunk.
    pub adjacent_chunk_support: f64,
    /// Negative contribution for graph paths that are only structural.
    pub structural_only_penalty: f64,
}

impl ArticleFeatureContributions {
    /// Adds another feature contribution set into this one.
    pub fn add(&mut self, other: Self) {
        self.max_fused_score += other.max_fused_score;
        self.lexical_title += other.lexical_title;
        self.same_article_repeat += other.same_article_repeat;
        self.exact_title_match += other.exact_title_match;
        self.typed_graph_evidence += other.typed_graph_evidence;
        self.adjacent_chunk_support += other.adjacent_chunk_support;
        self.structural_only_penalty += other.structural_only_penalty;
    }

    fn total(self) -> f64 {
        self.max_fused_score
            + self.lexical_title
            + self.same_article_repeat
            + self.exact_title_match
            + self.typed_graph_evidence
            + self.adjacent_chunk_support
            + self.structural_only_penalty
    }
}

/// Article-level score explanation emitted by `ArticleGraph` retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArticleFeatureContribution {
    /// Tenant knowledge object that represents the article or document.
    pub object_uid: Uuid,
    /// Citation URI for the article when available.
    pub source_uri: Option<String>,
    /// Renderer-safe article title when available.
    pub source_title: Option<String>,
    /// Candidate chunk count grouped into this article.
    pub chunk_count: usize,
    /// First article rank before article-level aggregation.
    pub rank_before_article_graph: Option<usize>,
    /// Article rank after article-level aggregation.
    pub rank_after_article_graph: usize,
    /// Rank movement where negative means the article moved earlier.
    pub rank_delta_after_minus_before: Option<i64>,
    /// Final article score.
    pub score: f64,
    /// Feature contribution breakdown.
    pub features: ArticleFeatureContributions,
    /// Non-structural graph path count into article chunks.
    pub typed_graph_evidence_count: usize,
    /// Structural-only graph path count into article chunks.
    pub structural_only_graph_count: usize,
}

/// Article-level diagnostics emitted when `ArticleGraph` ranking runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArticleRankingDiagnostics {
    /// Whether article-level ranking ran for this request.
    pub enabled: bool,
    /// Number of grouped tenant knowledge articles.
    pub ranked_article_count: usize,
    /// Sum of feature contributions across ranked articles.
    pub feature_totals: ArticleFeatureContributions,
    /// Top article explanations after ranking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_articles: Vec<ArticleFeatureContribution>,
}

/// One raw graph traversal path used to explain graph retrieval impact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphPathTrace {
    /// Seed node that produced the path.
    pub seed_uid: Uuid,
    /// Source that admitted the seed into graph expansion.
    pub seed_source: Option<GraphSeedSource>,
    /// Candidate node reached by the path.
    pub candidate_uid: Uuid,
    /// One-based graph distance from seed to candidate.
    pub hop: u8,
    /// Edge labels along the discovered path in traversal order.
    pub edge_labels: Vec<String>,
    /// Traversal directions aligned with `edge_labels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_directions: Vec<String>,
}

/// Request-local graph diagnostics emitted by hybrid retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphRetrievalDiagnostics {
    /// Effective graph policy used by the request.
    pub policy: GraphRetrievalPolicy,
    /// Seed counts grouped by source.
    pub seed_counts: GraphSeedDiagnostics,
    /// Edge-label histogram across raw graph expansion paths.
    pub path_label_histogram: BTreeMap<String, usize>,
    /// Hop-count histogram across raw graph expansion paths.
    pub hop_histogram: BTreeMap<u8, usize>,
    /// Raw graph expansion paths with seed and candidate identity for harm analysis.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_traces: Vec<GraphPathTrace>,
    /// Candidate counts grouped by graph leg overlap after fusion.
    pub candidate_counts: GraphCandidateCounts,
    /// Graph leg latency in milliseconds, excluding vector and lexical legs.
    pub graph_latency_ms: u64,
    /// Raw graph path count returned by the graph store before scoring.
    pub raw_path_count: usize,
    /// Article-level ranking diagnostics when `ArticleGraph` is active.
    pub article_ranking: ArticleRankingDiagnostics,
}

impl Default for GraphRetrievalDiagnostics {
    fn default() -> Self {
        Self::new(GraphRetrievalPolicy::default())
    }
}

impl GraphRetrievalDiagnostics {
    /// Creates empty diagnostics for the selected graph policy.
    #[must_use]
    pub fn new(policy: GraphRetrievalPolicy) -> Self {
        Self {
            policy,
            seed_counts: GraphSeedDiagnostics::default(),
            path_label_histogram: BTreeMap::new(),
            hop_histogram: BTreeMap::new(),
            path_traces: Vec::new(),
            candidate_counts: GraphCandidateCounts::default(),
            graph_latency_ms: 0,
            raw_path_count: 0,
            article_ranking: ArticleRankingDiagnostics::default(),
        }
    }

    pub(crate) fn record_paths(&mut self, paths: GraphRetrievalDiagnostics) {
        self.raw_path_count += paths.raw_path_count;
        self.path_traces.extend(paths.path_traces);
        for (label, count) in paths.path_label_histogram {
            *self.path_label_histogram.entry(label).or_default() += count;
        }
        for (hop, count) in paths.hop_histogram {
            *self.hop_histogram.entry(hop).or_default() += count;
        }
    }
}

/// Retrieval hits plus request-local diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalOutput {
    /// Final ranked retrieval hits.
    pub hits: Vec<RetrievalHit>,
    /// Diagnostics for graph policy, seeds, paths, candidates, and latency.
    pub diagnostics: GraphRetrievalDiagnostics,
}

/// Per-turn context needed to record retrieved facts for quality scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineageContext {
    /// Session that issued the retrieval.
    pub session_id: SessionId,
    /// Durable lineage turn id when known.
    pub turn_id: Option<TurnId>,
    /// Monotonic turn sequence when known.
    pub turn_seq: i64,
}

/// Retrieval legs that contributed to one fused candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LegSources {
    /// Candidate came from graph traversal.
    pub graph: bool,
    /// Candidate came from vector KNN.
    pub vector: bool,
    /// Candidate came from lexical search.
    pub lexical: bool,
}

/// Lexical retrieval backend that produced lexical candidate attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LexicalBackend {
    /// Postgres `tsvector` search over the graph sidecar.
    PostgresTsvector,
    /// Turbopuffer BM25 search over indexed tenant knowledge chunks.
    TurbopufferBm25,
    /// A merged lexical candidate list from Turbopuffer BM25 and Postgres `tsvector`.
    Mixed,
}

impl LexicalBackend {
    /// Returns the stable label used in metrics and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PostgresTsvector => "postgres_tsvector",
            Self::TurbopufferBm25 => "turbopuffer_bm25",
            Self::Mixed => "mixed",
        }
    }
}

/// Source tier represented by a retrieval hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceTier {
    /// Tenant-owned synced knowledge-base content.
    TenantKnowledge,
    /// Contact-owned runtime memory admitted for the current session.
    UserMemory,
}

impl SourceTier {
    /// Returns the stable wire string for this source tier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TenantKnowledge => "tenant_knowledge",
            Self::UserMemory => "user_memory",
        }
    }
}

/// Full tenant knowledge chunk text and citation metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunkHydration {
    /// Stable knowledge chunk identifier.
    pub chunk_uid: Uuid,
    /// Knowledge document version containing the chunk.
    pub document_version_uid: Uuid,
    /// Source object containing the document version.
    pub object_uid: Uuid,
    /// Stable chunk content hash.
    pub chunk_hash: String,
    /// Chunk ordinal within the document version.
    pub ordinal: i32,
    /// Parser-derived heading path.
    pub heading_path: Vec<String>,
    /// Full normalized chunk text from `moa.knowledge_chunks`.
    pub text: String,
    /// Estimated token count stored by ingestion.
    pub token_count: i32,
    /// Redacted chunk metadata.
    pub metadata: Value,
    /// Optional source URI for citations.
    pub source_uri: Option<String>,
    /// Optional renderer-safe source title.
    pub source_title: Option<String>,
    /// Source object type.
    pub object_type: String,
}

/// One hydrated retrieval result.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalHit {
    /// Stable graph node uid.
    pub uid: Uuid,
    /// Retrieval score after the configured ranking stage.
    pub score: f64,
    /// Source legs that contributed to the score.
    pub legs: LegSources,
    /// Lexical backend selected for this hit when `legs.lexical` is true.
    pub lexical_backend: Option<LexicalBackend>,
    /// Source tier used for context assembly and query tracing.
    pub source_tier: SourceTier,
    /// Full tenant knowledge chunk payload when the hit is a knowledge chunk.
    pub knowledge_chunk: Option<KnowledgeChunkHydration>,
    /// Hydrated sidecar row.
    pub node: NodeIndexRow,
}

/// Hybrid retriever that fuses graph, vector, and lexical retrieval.
#[derive(Clone)]
pub struct HybridRetriever {
    pool: PgPool,
    graph: Arc<dyn GraphStore>,
    pgvector_source: Arc<PgvectorStore>,
    turbopuffer: Option<Arc<TurbopufferStore>>,
    reranker: Arc<dyn Reranker>,
    rerank_model: String,
    ranking_config: RankingConfig,
    assume_app_role: bool,
    lineage_enabled: bool,
    graph_policy: GraphRetrievalPolicy,
}

impl HybridRetriever {
    /// Creates a hybrid retriever with deterministic no-op reranking.
    #[must_use]
    pub fn new(
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        pgvector_source: Arc<PgvectorStore>,
    ) -> Self {
        let configured_reranker = ConfiguredReranker::noop();
        Self {
            pool,
            graph,
            pgvector_source,
            turbopuffer: None,
            reranker: configured_reranker.reranker,
            rerank_model: configured_reranker.model,
            ranking_config: RankingConfig::default(),
            assume_app_role: false,
            lineage_enabled: false,
            graph_policy: GraphRetrievalPolicy::default(),
        }
    }

    /// Creates a hybrid retriever from shared config.
    #[must_use]
    pub fn from_config(
        config: &MoaConfig,
        pool: PgPool,
        graph: Arc<dyn GraphStore>,
        pgvector_source: Arc<PgvectorStore>,
    ) -> Self {
        let configured = configured_reranker_or_noop(config);
        let turbopuffer = TurbopufferStore::from_config(config).ok().map(Arc::new);
        Self::new(pool, graph, pgvector_source)
            .with_turbopuffer(turbopuffer)
            .with_configured_reranker(configured)
            .with_ranking_config(RankingConfig::from(&config.memory.retrieval.ranking))
            .with_lineage_enabled(config.memory.retrieval.lineage_enabled)
    }

    /// Adds an optional Turbopuffer target backend for promoted storage partitions.
    #[must_use]
    pub fn with_turbopuffer(mut self, turbopuffer: Option<Arc<TurbopufferStore>>) -> Self {
        self.turbopuffer = turbopuffer;
        self
    }

    /// Overrides the reranker backend.
    #[must_use]
    pub fn with_reranker(mut self, reranker: Arc<dyn Reranker>) -> Self {
        self.reranker = reranker;
        self
    }

    /// Overrides the reranker backend and model from provider configuration.
    #[must_use]
    pub fn with_configured_reranker(mut self, configured: ConfiguredReranker) -> Self {
        self.reranker = configured.reranker;
        self.rerank_model = configured.model;
        self
    }

    /// Overrides the reranker model while preserving the configured backend.
    #[must_use]
    pub fn with_rerank_model(mut self, model: impl Into<String>) -> Self {
        self.rerank_model = model.into();
        self
    }

    /// Overrides the deterministic post-hydration ranking configuration.
    #[must_use]
    pub fn with_ranking_config(mut self, ranking_config: RankingConfig) -> Self {
        self.ranking_config = ranking_config;
        self
    }

    /// Overrides the graph retrieval policy used when requests do not explicitly disable graph expansion.
    #[must_use]
    pub fn with_graph_policy(mut self, graph_policy: GraphRetrievalPolicy) -> Self {
        self.graph_policy = graph_policy;
        self
    }

    /// Enables or disables fire-and-forget retrieval lineage sidecar writes.
    #[must_use]
    pub fn with_lineage_enabled(mut self, enabled: bool) -> Self {
        self.lineage_enabled = enabled;
        self
    }

    /// Returns the cache fingerprint for the configured ranking stage.
    #[must_use]
    pub fn ranking_fingerprint(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&ranking_fingerprint(&self.ranking_config));
        hasher.update(self.graph_policy.as_str().as_bytes());
        *hasher.finalize().as_bytes()
    }

    /// Assumes the `moa_app` role inside sidecar transactions.
    ///
    /// This is intended for integration tests that connect through the local owner role.
    #[must_use]
    pub fn with_assume_app_role(mut self, assume_app_role: bool) -> Self {
        self.assume_app_role = assume_app_role;
        self
    }

    /// Retrieves graph-memory candidates through graph, vector, and lexical legs.
    pub async fn retrieve(&self, req: RetrievalRequest) -> Result<Vec<RetrievalHit>> {
        Ok(self.retrieve_with_diagnostics(req).await?.hits)
    }

    /// Retrieves graph-memory candidates and request-local diagnostics.
    pub async fn retrieve_with_diagnostics(
        &self,
        req: RetrievalRequest,
    ) -> Result<RetrievalOutput> {
        let graph_policy = effective_graph_policy(self.graph_policy, &req);
        let mut diagnostics = GraphRetrievalDiagnostics::new(graph_policy);
        if req.k_final == 0 {
            return Ok(RetrievalOutput {
                hits: Vec::new(),
                diagnostics,
            });
        }

        let strategy = req.strategy.unwrap_or(Strategy::Both);
        let backend_state = if retrieval_needs_backend_state(&req) {
            self.vector_backend_state(&req).await?
        } else {
            VectorBackendState::default()
        };
        let vector_future = run_leg(
            req.disable_leg_timeouts,
            "vector",
            VECTOR_BUDGET,
            self.vector_leg(&req, &backend_state),
        );
        let lexical_future = run_leg(
            req.disable_leg_timeouts,
            "lexical",
            LEXICAL_BUDGET,
            self.lexical_leg(&req, &backend_state),
        );
        let (vector_hits, lexical_hits) = tokio::join!(vector_future, lexical_future);
        let vector_hits = vector_hits?;
        let mut lexical_hits = lexical_hits?;
        apply_lexical_boost_only_policy(&req, &vector_hits, &mut lexical_hits);
        let interim = rrf_fuse(
            &[],
            &vector_hits,
            &lexical_hits.candidates,
            weights_for(strategy),
        );
        let semantic_entity_seed_uids = if graph_policy.allows_semantic_entity_seeds() {
            semantic_entity_seed_uids(&self.pool, &req, self.assume_app_role).await?
        } else {
            Vec::new()
        };
        let graph_seed_rows = if graph_policy.disables_graph_ranking() {
            Vec::new()
        } else {
            hydrate_graph_seed_rows(
                &self.pool,
                &req,
                &interim,
                &semantic_entity_seed_uids,
                self.assume_app_role,
            )
            .await?
        };
        let graph_seed_plan = if graph_policy.disables_graph_ranking() {
            GraphSeedPlan::default()
        } else {
            interim_graph_seed_plan(
                graph_policy,
                &req.seeds,
                &semantic_entity_seed_uids,
                &interim,
                &graph_seed_rows,
                &req.query_text,
            )
        };
        diagnostics.seed_counts = graph_seed_plan.seed_counts;
        let graph_output = if graph_seed_plan.strengths.is_empty() {
            Default::default()
        } else {
            let graph_started = std::time::Instant::now();
            let mut output = run_leg(
                req.disable_leg_timeouts,
                "graph",
                GRAPH_BUDGET,
                graph_expansion_leg_with_diagnostics(
                    self.graph.as_ref(),
                    &req,
                    &graph_seed_plan.strengths,
                    &graph_seed_rows,
                    &graph_seed_plan.seed_sources,
                    graph_policy,
                ),
            )
            .await?;
            output.diagnostics.graph_latency_ms = duration_ms_u64(graph_started.elapsed());
            output
        };
        diagnostics.graph_latency_ms = graph_output.diagnostics.graph_latency_ms;
        diagnostics.record_paths(graph_output.diagnostics);
        let graph_hits = graph_output.candidates;
        let fusion_started = std::time::Instant::now();
        let mut fused = rrf_fuse(
            &graph_hits,
            &vector_hits,
            &lexical_hits.candidates,
            weights_for(strategy),
        );
        fused.truncate(fused_candidate_limit(req.k_final));
        diagnostics.candidate_counts = graph_candidate_counts(&fused);
        if fused.is_empty() {
            return Ok(RetrievalOutput {
                hits: Vec::new(),
                diagnostics,
            });
        }

        let fused_uids = fused.iter().map(|(uid, _, _)| *uid).collect::<Vec<_>>();
        let nodes = hydrate_nodes(
            &self.pool,
            &req.scope,
            &fused_uids,
            self.assume_app_role,
            req.as_of,
        )
        .await?;
        let mut hits = build_hits(fused, nodes);
        annotate_lexical_backend(&mut hits, lexical_hits.backend);
        hydrate_knowledge_chunks(&self.pool, &req.scope, &mut hits, self.assume_app_role).await?;
        rank_hydrated_hits_for_policy(
            &mut hits,
            &self.ranking_config,
            &req,
            graph_policy,
            vector_hits.first().map(|candidate| candidate.uid),
        );
        if graph_policy == GraphRetrievalPolicy::ArticleGraph {
            diagnostics.article_ranking = apply_article_graph_ranking(
                &mut hits,
                &req,
                &diagnostics.path_traces,
                vector_hits.first().map(|candidate| candidate.uid),
            );
        }
        let final_hits = if req.use_reranker && hits.len() > req.k_final {
            let reranked = self.rerank_hits(&req, &hits).await?;
            select_final_hits(reranked, &hits, req.k_final)
        } else {
            select_final_hits(hits, &[], req.k_final)
        };
        metrics::histogram!("moa_retrieval_rrf_rerank_seconds")
            .record(fusion_started.elapsed().as_secs_f64());

        if req.ranking_reference_time.is_none() {
            let touched_uids = final_hits.iter().map(|hit| hit.uid).collect::<Vec<_>>();
            let pool = self.pool.clone();
            let scope = req.scope.clone();
            let assume_app_role = self.assume_app_role;
            tokio::spawn(async move {
                if let Err(error) =
                    bump_last_accessed(pool, scope, touched_uids, assume_app_role).await
                {
                    tracing::debug!(error = %error, "failed to bump graph-memory access timestamps");
                }
            });
        }
        if self.lineage_enabled
            && let Some(lineage) = req.lineage
        {
            let ranked_uids = final_hits.iter().map(|hit| hit.uid).collect::<Vec<_>>();
            let pool = self.pool.clone();
            let scope = req.scope.clone();
            let assume_app_role = self.assume_app_role;
            let retrieved_at = Utc::now();
            tokio::spawn(async move {
                if let Err(error) = write_retrieval_lineage(
                    pool,
                    scope,
                    lineage,
                    ranked_uids,
                    retrieved_at,
                    assume_app_role,
                )
                .await
                {
                    tracing::debug!(error = %error, "failed to write graph-memory retrieval lineage");
                }
            });
        }

        Ok(RetrievalOutput {
            hits: final_hits,
            diagnostics,
        })
    }

    async fn vector_leg(
        &self,
        req: &RetrievalRequest,
        state: &VectorBackendState,
    ) -> Result<Vec<LegCandidate>> {
        if req.query_embedding.is_empty() {
            return Ok(Vec::new());
        }

        if req.as_of.is_some() {
            return run_vector_leg(self.pgvector_source.as_ref(), req).await;
        }

        let tenant_id = req.scope.tenant_id();
        if state.is_dual_read_active() {
            return self.dual_read_vector_leg(req).await;
        }
        if state.vector_backend == "turbopuffer" {
            let turbopuffer = self
                .turbopuffer
                .as_ref()
                .ok_or_else(|| turbopuffer_unavailable_for_request(req))?;
            let scoped_turbopuffer = turbopuffer.scoped_to_tenant(tenant_id);
            return run_vector_leg(&scoped_turbopuffer, req).await;
        }

        run_vector_leg(self.pgvector_source.as_ref(), req).await
    }

    async fn lexical_leg(
        &self,
        req: &RetrievalRequest,
        state: &VectorBackendState,
    ) -> Result<LexicalLegOutput> {
        if state.uses_turbopuffer_backend()
            && req.as_of.is_none()
            && request_allows_tenant_chunk_bm25(req)
        {
            let turbopuffer = self
                .turbopuffer
                .as_ref()
                .ok_or_else(|| turbopuffer_unavailable_for_request(req))?;
            let scoped_turbopuffer = turbopuffer.scoped_to_tenant(req.scope.tenant_id());
            match turbopuffer_bm25_leg(&scoped_turbopuffer, req).await {
                Ok(hits) => {
                    if request_is_tenant_chunk_only(req) {
                        record_lexical_backend(LexicalBackend::TurbopufferBm25, "success");
                        return Ok(LexicalLegOutput::new(hits, LexicalBackend::TurbopufferBm25));
                    }
                    let postgres_hits = lexical_leg(&self.pool, req, self.assume_app_role).await?;
                    record_lexical_backend(LexicalBackend::Mixed, "success");
                    return Ok(LexicalLegOutput::new(
                        merge_lexical_candidates(hits, postgres_hits),
                        LexicalBackend::Mixed,
                    ));
                }
                Err(error) => {
                    record_lexical_backend(LexicalBackend::TurbopufferBm25, "fallback");
                    tracing::warn!(
                        error = %error,
                        tenant_id = %req.scope.tenant_id(),
                        "Turbopuffer BM25 lexical leg failed; falling back to Postgres lexical"
                    );
                }
            }
        }

        let hits = lexical_leg(&self.pool, req, self.assume_app_role).await?;
        record_lexical_backend(LexicalBackend::PostgresTsvector, "success");
        Ok(LexicalLegOutput::new(
            hits,
            LexicalBackend::PostgresTsvector,
        ))
    }

    async fn dual_read_vector_leg(&self, req: &RetrievalRequest) -> Result<Vec<LegCandidate>> {
        let Some(turbopuffer) = &self.turbopuffer else {
            return Err(turbopuffer_unavailable_for_request(req));
        };

        let scoped_turbopuffer = turbopuffer.scoped_to_tenant(req.scope.tenant_id());
        let pg_future = run_vector_leg(self.pgvector_source.as_ref(), req);
        let tp_future = run_vector_leg(&scoped_turbopuffer, req);
        let (pg_result, tp_result) = tokio::join!(pg_future, tp_future);

        if let (Ok(pg_hits), Ok(tp_hits)) = (&pg_result, &tp_result) {
            metrics::histogram!("moa_vector_dualread_overlap")
                .record(leg_overlap(pg_hits, tp_hits, 10));
        }

        match (tp_result, pg_result) {
            (Ok(tp_hits), _) => Ok(tp_hits),
            (Err(error), Ok(pg_hits)) => {
                tracing::warn!(error = %error, "Turbopuffer vector dual-read leg failed; using pgvector result");
                Ok(pg_hits)
            }
            (Err(error), Err(_)) => Err(error),
        }
    }

    async fn vector_backend_state(&self, req: &RetrievalRequest) -> Result<VectorBackendState> {
        let scope = req.scope.to_rls_context();
        let mut conn = ScopedConn::begin(&self.pool, &scope).await?;
        if self.assume_app_role {
            sqlx::query("SET LOCAL ROLE moa_app")
                .execute(conn.as_mut())
                .await?;
        }
        let row = sqlx::query_as::<_, (String, String, Option<DateTime<Utc>>)>(
            r#"
                SELECT vector_backend, vector_backend_state, dual_read_until
                FROM moa.storage_partition_state
                WHERE tenant_id = $1
                "#,
        )
        .bind(req.scope.tenant_id().0)
        .fetch_optional(conn.as_mut())
        .await?;
        conn.commit().await?;
        Ok(row
            .map(
                |(vector_backend, vector_backend_state, dual_read_until)| VectorBackendState {
                    vector_backend,
                    vector_backend_state,
                    dual_read_until,
                },
            )
            .unwrap_or_default())
    }

    async fn rerank_hits(
        &self,
        req: &RetrievalRequest,
        hits: &[RetrievalHit],
    ) -> Result<Vec<RetrievalHit>> {
        let documents = hits.iter().map(rerank_document).collect::<Vec<_>>();
        let reranked = self
            .reranker
            .rerank(&self.rerank_model, &req.query_text, &documents, req.k_final)
            .await
            .map_err(|error| RetrievalError::Rerank(error.to_string()))?;
        let mut out = Vec::with_capacity(req.k_final.min(reranked.len()));
        for hit in reranked {
            if let Some(candidate) = hits.get(hit.index) {
                out.push(candidate.clone());
            }
        }
        if out.is_empty() {
            Ok(hits.iter().take(req.k_final).cloned().collect())
        } else {
            Ok(out)
        }
    }
}

fn fused_candidate_limit(k_final: usize) -> usize {
    if k_final == 0 {
        return 0;
    }
    k_final
        .saturating_mul(FUSED_CANDIDATE_MULTIPLIER)
        .clamp(MIN_FUSED_CANDIDATE_LIMIT, MAX_FUSED_CANDIDATE_LIMIT)
}

fn rerank_document(hit: &RetrievalHit) -> String {
    let Some(chunk) = &hit.knowledge_chunk else {
        return hit.node.name.clone();
    };
    let mut parts = Vec::new();
    if let Some(title) = chunk
        .source_title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty())
    {
        parts.push(format!("Title: {title}"));
    }
    if !chunk.heading_path.is_empty() {
        parts.push(format!("Section: {}", chunk.heading_path.join(" > ")));
    }
    parts.push(chunk.text.clone());
    parts.join("\n")
}

fn configured_reranker_or_noop(config: &MoaConfig) -> ConfiguredReranker {
    build_reranker_from_config(config).unwrap_or_else(|error| {
        tracing::warn!(
            error = %error,
            "graph-memory reranking disabled because reranker configuration is invalid"
        );
        ConfiguredReranker::noop()
    })
}

#[derive(Debug, Clone)]
struct VectorBackendState {
    vector_backend: String,
    vector_backend_state: String,
    dual_read_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default)]
struct LexicalLegOutput {
    candidates: Vec<LegCandidate>,
    backend: Option<LexicalBackend>,
}

impl LexicalLegOutput {
    fn new(candidates: Vec<LegCandidate>, backend: LexicalBackend) -> Self {
        Self {
            candidates,
            backend: Some(backend),
        }
    }
}

impl Default for VectorBackendState {
    fn default() -> Self {
        Self {
            vector_backend: "pgvector".to_string(),
            vector_backend_state: "steady".to_string(),
            dual_read_until: None,
        }
    }
}

impl VectorBackendState {
    fn is_dual_read_active(&self) -> bool {
        self.vector_backend_state == "dual_read"
            && self.dual_read_until.is_none_or(|until| until > Utc::now())
    }

    fn uses_turbopuffer_backend(&self) -> bool {
        self.vector_backend == "turbopuffer"
    }
}

fn retrieval_needs_backend_state(req: &RetrievalRequest) -> bool {
    !req.query_embedding.is_empty() || !req.query_text.trim().is_empty()
}

fn turbopuffer_unavailable_for_request(req: &RetrievalRequest) -> RetrievalError {
    VectorError::TurbopufferUnavailable {
        storage_partition_id: req
            .scope
            .to_rls_context()
            .storage_partition_id()
            .to_string(),
    }
    .into()
}

fn request_allows_tenant_chunk_bm25(req: &RetrievalRequest) -> bool {
    if req.strategy == Some(Strategy::VectorFirst) {
        return false;
    }
    req.label_filter
        .as_deref()
        .is_some_and(|labels| labels.contains(&NodeLabel::Chunk))
}

fn request_is_tenant_chunk_only(req: &RetrievalRequest) -> bool {
    req.label_filter
        .as_deref()
        .is_some_and(|labels| labels == [NodeLabel::Chunk])
}

fn merge_lexical_candidates(
    primary: Vec<LegCandidate>,
    fallback: Vec<LegCandidate>,
) -> Vec<LegCandidate> {
    let mut seen = HashSet::new();
    primary
        .into_iter()
        .chain(fallback)
        .filter_map(|candidate| seen.insert(candidate.uid).then_some(candidate.uid))
        .enumerate()
        .map(|(index, uid)| LegCandidate {
            uid,
            score: 1.0 / (RRF_K + index as f64 + 1.0),
        })
        .collect()
}

fn record_lexical_backend(backend: LexicalBackend, outcome: &'static str) {
    metrics::counter!(
        "moa_retrieval_lexical_backend_total",
        "backend" => backend.as_str(),
        "outcome" => outcome
    )
    .increment(1);
}

fn apply_lexical_boost_only_policy(
    req: &RetrievalRequest,
    vector_hits: &[LegCandidate],
    lexical_hits: &mut LexicalLegOutput,
) {
    if lexical_hits.backend != Some(LexicalBackend::TurbopufferBm25)
        || !request_is_tenant_chunk_only(req)
        || vector_hits.is_empty()
    {
        return;
    }
    let vector_uids = vector_hits
        .iter()
        .map(|candidate| candidate.uid)
        .collect::<HashSet<_>>();
    lexical_hits
        .candidates
        .retain(|candidate| vector_uids.contains(&candidate.uid));
    for candidate in &mut lexical_hits.candidates {
        candidate.score *= TURBOPUFFER_BM25_BOOST_MULTIPLIER;
    }
}

fn leg_overlap(left: &[LegCandidate], right: &[LegCandidate], k: usize) -> f64 {
    let left_set = left
        .iter()
        .take(k)
        .map(|hit| hit.uid)
        .collect::<HashSet<_>>();
    let right_set = right
        .iter()
        .take(k)
        .map(|hit| hit.uid)
        .collect::<HashSet<_>>();
    let denom = left_set.len().max(right_set.len()).max(1).min(k);
    left_set.intersection(&right_set).count() as f64 / denom as f64
}

fn weights_for(strategy: Strategy) -> (f64, f64, f64) {
    match strategy {
        Strategy::GraphFirst => (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT * 0.5),
        Strategy::VectorFirst | Strategy::Both => (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT),
    }
}

fn effective_graph_policy(
    retriever_policy: GraphRetrievalPolicy,
    req: &RetrievalRequest,
) -> GraphRetrievalPolicy {
    if req.disable_graph_expansion {
        GraphRetrievalPolicy::Off
    } else {
        retriever_policy
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct GraphSeedPlan {
    strengths: Vec<(Uuid, f64)>,
    seed_counts: GraphSeedDiagnostics,
    seed_sources: HashMap<Uuid, GraphSeedSource>,
}

#[cfg(test)]
fn interim_graph_seed_strengths(
    planner_seeds: &[Uuid],
    interim: &[(Uuid, f64, LegSources)],
    phase_one_rows: &[NodeIndexRow],
    query_text: &str,
) -> Vec<(Uuid, f64)> {
    interim_graph_seed_plan(
        GraphRetrievalPolicy::LegacyBroadExpansion,
        planner_seeds,
        &[],
        interim,
        phase_one_rows,
        query_text,
    )
    .strengths
}

fn interim_graph_seed_plan(
    policy: GraphRetrievalPolicy,
    planner_seeds: &[Uuid],
    semantic_entity_seed_uids: &[Uuid],
    interim: &[(Uuid, f64, LegSources)],
    phase_one_rows: &[NodeIndexRow],
    query_text: &str,
) -> GraphSeedPlan {
    let mut seen = HashSet::new();
    let mut strengths = Vec::new();
    let mut seed_counts = GraphSeedDiagnostics::default();
    let mut seed_sources = HashMap::new();
    for seed in planner_seeds {
        if seen.insert(*seed) {
            strengths.push((*seed, 1.0));
            seed_counts.planner += 1;
            seed_sources.insert(*seed, GraphSeedSource::Planner);
        }
    }
    for seed in
        exact_semantic_entity_seed_uids(semantic_entity_seed_uids, phase_one_rows, query_text)
    {
        if seen.insert(seed) {
            strengths.push((seed, SEMANTIC_ENTITY_SEED_STRENGTH));
            seed_counts.semantic_entity += 1;
            seed_sources.insert(seed, GraphSeedSource::SemanticEntity);
        }
    }

    let exact_seed_uids = exact_phase_one_seed_uids(phase_one_rows, query_text);
    let use_broad_phase_one = policy.allows_broad_phase_one_fallback()
        && planner_seeds.is_empty()
        && exact_seed_uids.is_empty();
    let mut phase_one = interim
        .iter()
        .take(PHASE_ONE_GRAPH_SEED_LIMIT)
        .enumerate()
        .filter_map(|(index, (uid, _, _))| {
            let is_exact = exact_seed_uids.contains(uid);
            (is_exact || use_broad_phase_one).then_some((*uid, index, is_exact))
        })
        .collect::<Vec<_>>();
    phase_one.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.1.cmp(&right.1)));
    for (index, (uid, _, is_exact)) in phase_one.into_iter().enumerate() {
        if seen.insert(uid) {
            strengths.push((uid, PHASE_ONE_SEED_DECAY.powi(index as i32)));
            if is_exact {
                seed_counts.exact_phase_one += 1;
                seed_sources.insert(uid, GraphSeedSource::ExactPhaseOne);
            } else {
                seed_counts.broad_fallback += 1;
                seed_sources.insert(uid, GraphSeedSource::BroadFallback);
            }
        }
    }
    GraphSeedPlan {
        strengths,
        seed_counts,
        seed_sources,
    }
}

async fn hydrate_graph_seed_rows(
    pool: &PgPool,
    req: &RetrievalRequest,
    interim: &[(Uuid, f64, LegSources)],
    semantic_entity_seed_uids: &[Uuid],
    assume_app_role: bool,
) -> Result<Vec<NodeIndexRow>> {
    let mut seen = HashSet::new();
    let mut uids = req
        .seeds
        .iter()
        .copied()
        .filter(|uid| seen.insert(*uid))
        .collect::<Vec<_>>();
    uids.extend(
        semantic_entity_seed_uids
            .iter()
            .copied()
            .filter(|uid| seen.insert(*uid)),
    );
    uids.extend(
        interim
            .iter()
            .take(PHASE_ONE_GRAPH_SEED_LIMIT)
            .map(|(uid, _, _)| *uid)
            .filter(|uid| seen.insert(*uid)),
    );
    hydrate_nodes(pool, &req.scope, &uids, assume_app_role, req.as_of).await
}

async fn semantic_entity_seed_uids(
    pool: &PgPool,
    req: &RetrievalRequest,
    assume_app_role: bool,
) -> Result<Vec<Uuid>> {
    if !request_is_tenant_chunk_query(req) {
        return Ok(Vec::new());
    }
    let terms = semantic_entity_seed_terms(&req.query_text);
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let tsquery = terms
        .iter()
        .map(|term| format!("'{}':*", term.replace('\'', "")))
        .collect::<Vec<_>>()
        .join(" | ");
    if tsquery.is_empty() {
        return Ok(Vec::new());
    }

    let mut conn = begin_scoped(pool, &req.scope, assume_app_role).await?;
    let mut builder = QueryBuilder::<Postgres>::new(
        r#"
        SELECT uid
        FROM moa.node_index
        WHERE "#,
    );
    push_validity_filter(&mut builder, None, req.as_of);
    builder.push(" AND scope = 'tenant' AND label = ");
    builder.push_bind(NodeLabel::Entity.as_str());
    builder.push(r#" AND (name_tsv @@ to_tsquery('simple', "#);
    builder.push_bind(tsquery.clone());
    builder.push(") OR properties_tsv @@ to_tsquery('simple', ");
    builder.push_bind(tsquery.clone());
    builder.push(
        r#"))
          AND CASE pii_class
                WHEN 'none' THEN 0
                WHEN 'pii' THEN 1
                WHEN 'phi' THEN 2
                WHEN 'restricted' THEN 3
                ELSE 4
              END <= "#,
    );
    builder.push_bind(semantic_seed_pii_rank(req.max_pii_class));
    builder.push(
        r#"
        ORDER BY (
            ts_rank(name_tsv, to_tsquery('simple', "#,
    );
    builder.push_bind(tsquery.clone());
    builder.push(")) + ts_rank(properties_tsv, to_tsquery('simple', ");
    builder.push_bind(tsquery);
    builder.push(
        r#"))
        ) DESC,
        uid ASC
        LIMIT "#,
    );
    builder.push_bind(SEMANTIC_ENTITY_GRAPH_SEED_LIMIT as i64);
    let rows = builder
        .build_query_scalar::<Uuid>()
        .fetch_all(conn.as_mut())
        .await?;
    conn.commit().await?;
    Ok(rows)
}

fn request_is_tenant_chunk_query(req: &RetrievalRequest) -> bool {
    matches!(req.scope, MemoryScope::Tenant { .. })
        && req
            .label_filter
            .as_deref()
            .is_some_and(|labels| labels == [NodeLabel::Chunk])
}

fn semantic_entity_seed_terms(query_text: &str) -> Vec<String> {
    const STOP_WORDS: &[&str] = &[
        "about", "after", "also", "before", "between", "could", "does", "from", "have", "help",
        "into", "make", "need", "should", "that", "their", "there", "these", "this", "using",
        "what", "when", "where", "which", "with", "your",
    ];
    normalize_tokens(query_text)
        .into_iter()
        .filter(|term| !STOP_WORDS.contains(&term.as_str()))
        .take(SEMANTIC_ENTITY_GRAPH_SEED_LIMIT)
        .collect()
}

const fn semantic_seed_pii_rank(class: PiiClass) -> i32 {
    match class {
        PiiClass::None => 0,
        PiiClass::Pii => 1,
        PiiClass::Phi => 2,
        PiiClass::Restricted => 3,
    }
}

fn exact_phase_one_seed_uids(rows: &[NodeIndexRow], query_text: &str) -> HashSet<Uuid> {
    let query_tokens = normalize_tokens(query_text);
    rows.iter()
        .filter(|row| {
            let name_tokens = normalize_tokens(&row.name);
            !name_tokens.is_empty() && name_tokens.iter().all(|token| query_tokens.contains(token))
        })
        .map(|row| row.uid)
        .collect()
}

fn exact_semantic_entity_seed_uids(
    semantic_entity_seed_uids: &[Uuid],
    rows: &[NodeIndexRow],
    query_text: &str,
) -> Vec<Uuid> {
    let query_tokens = normalize_tokens(query_text);
    let rows_by_uid = rows
        .iter()
        .map(|row| (row.uid, row))
        .collect::<HashMap<_, _>>();
    semantic_entity_seed_uids
        .iter()
        .filter_map(|uid| rows_by_uid.get(uid).copied())
        .filter(|row| row.label == NodeLabel::Entity)
        .filter(|row| {
            let name_tokens = normalize_tokens(&row.name);
            name_tokens.len() >= 2 && name_tokens.iter().all(|token| query_tokens.contains(token))
        })
        .map(|row| row.uid)
        .take(SEMANTIC_ENTITY_EXACT_SEED_LIMIT)
        .collect()
}

fn leg_or_empty<T>(
    name: &'static str,
    result: std::result::Result<Result<T>, tokio::time::error::Elapsed>,
) -> Result<T>
where
    T: Default,
{
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(error),
        Err(_) => {
            tracing::debug!(leg = name, "hybrid retrieval leg exceeded budget");
            Ok(T::default())
        }
    }
}

async fn run_leg<T, F>(
    disable_timeout: bool,
    name: &'static str,
    budget: std::time::Duration,
    future: F,
) -> Result<T>
where
    T: Default,
    F: std::future::Future<Output = Result<T>>,
{
    if disable_timeout {
        return future.await;
    }
    leg_or_empty(name, timed_leg(name, budget, future).await)
}

fn build_hits(fused: Vec<(Uuid, f64, LegSources)>, nodes: Vec<NodeIndexRow>) -> Vec<RetrievalHit> {
    let mut nodes_by_uid = nodes
        .into_iter()
        .map(|node| (node.uid, node))
        .collect::<HashMap<_, _>>();
    fused
        .into_iter()
        .filter_map(|(uid, score, legs)| {
            nodes_by_uid.remove(&uid).map(|node| RetrievalHit {
                uid,
                score,
                legs,
                lexical_backend: None,
                source_tier: source_tier_for_node(&node),
                knowledge_chunk: None,
                node,
            })
        })
        .collect()
}

fn annotate_lexical_backend(hits: &mut [RetrievalHit], backend: Option<LexicalBackend>) {
    for hit in hits {
        if hit.legs.lexical {
            hit.lexical_backend = backend;
        }
    }
}

fn graph_candidate_counts(fused: &[(Uuid, f64, LegSources)]) -> GraphCandidateCounts {
    let mut counts = GraphCandidateCounts::default();
    for (_, _, sources) in fused {
        match (sources.graph, sources.vector, sources.lexical) {
            (true, false, false) => counts.graph_only += 1,
            (true, true, false) => counts.vector_graph += 1,
            (true, false, true) => counts.lexical_graph += 1,
            (true, true, true) => counts.all_legs += 1,
            _ => {}
        }
    }
    counts
}

async fn hydrate_knowledge_chunks(
    pool: &PgPool,
    scope: &MemoryScope,
    hits: &mut [RetrievalHit],
    assume_app_role: bool,
) -> Result<()> {
    let chunk_uids = hits
        .iter()
        .filter(|hit| hit.source_tier == SourceTier::TenantKnowledge)
        .filter(|hit| hit.node.label == NodeLabel::Chunk)
        .map(|hit| hit.uid)
        .collect::<Vec<_>>();
    if chunk_uids.is_empty() {
        return Ok(());
    }

    let mut conn = ScopedConn::begin(pool, &RlsContext::tenant(scope.tenant_id())).await?;
    if assume_app_role {
        sqlx::query("SET LOCAL ROLE moa_app")
            .execute(conn.as_mut())
            .await?;
    }
    let rows = sqlx::query_as::<_, KnowledgeChunkRow>(
        r#"
        SELECT DISTINCT ON (c.graph_node_uid)
            c.graph_node_uid,
            c.chunk_uid,
            c.document_version_id AS document_version_uid,
            v.object_id AS object_uid,
            c.chunk_hash,
            c.ordinal,
            c.heading_path,
            c.text,
            c.token_count,
            c.metadata,
            o.source_uri,
            o.title AS source_title,
            o.object_type
        FROM moa.knowledge_chunks c
        JOIN moa.knowledge_document_versions v
          ON v.document_version_uid = c.document_version_id
        JOIN moa.knowledge_objects o
          ON o.object_uid = v.object_id
        WHERE c.tenant_id = $1
          AND c.graph_node_uid = ANY($2)
          AND o.status = 'active'
          AND c.metadata->>'active' IS DISTINCT FROM 'false'
        ORDER BY c.graph_node_uid, v.created_at DESC, c.ordinal ASC
        "#,
    )
    .bind(scope.tenant_id().0)
    .bind(&chunk_uids)
    .fetch_all(conn.as_mut())
    .await?;
    conn.commit().await?;

    let mut chunks_by_graph_uid = rows
        .into_iter()
        .map(|row| {
            (
                row.graph_node_uid,
                KnowledgeChunkHydration {
                    chunk_uid: row.chunk_uid,
                    document_version_uid: row.document_version_uid,
                    object_uid: row.object_uid,
                    chunk_hash: row.chunk_hash,
                    ordinal: row.ordinal,
                    heading_path: row.heading_path,
                    text: row.text,
                    token_count: row.token_count,
                    metadata: row.metadata,
                    source_uri: row.source_uri,
                    source_title: row.source_title,
                    object_type: row.object_type,
                },
            )
        })
        .collect::<HashMap<_, _>>();
    for hit in hits {
        if let Some(chunk) = chunks_by_graph_uid.remove(&hit.uid) {
            hit.knowledge_chunk = Some(chunk);
        }
    }
    Ok(())
}

#[derive(Debug, sqlx::FromRow)]
struct KnowledgeChunkRow {
    graph_node_uid: Uuid,
    chunk_uid: Uuid,
    document_version_uid: Uuid,
    object_uid: Uuid,
    chunk_hash: String,
    ordinal: i32,
    heading_path: Vec<String>,
    text: String,
    token_count: i32,
    metadata: Value,
    source_uri: Option<String>,
    source_title: Option<String>,
    object_type: String,
}

fn source_tier_for_node(node: &NodeIndexRow) -> SourceTier {
    if node.scope == "tenant" && is_tenant_knowledge_label(node.label) {
        SourceTier::TenantKnowledge
    } else {
        SourceTier::UserMemory
    }
}

fn is_tenant_knowledge_label(label: NodeLabel) -> bool {
    matches!(
        label,
        NodeLabel::Chunk | NodeLabel::Document | NodeLabel::ContactGroup
    )
}

#[cfg(test)]
fn rank_hydrated_hits(hits: &mut [RetrievalHit], config: &RankingConfig, req: &RetrievalRequest) {
    rank_hydrated_hits_for_policy(
        hits,
        config,
        req,
        GraphRetrievalPolicy::LegacyBroadExpansion,
        None,
    );
}

fn rank_hydrated_hits_for_policy(
    hits: &mut [RetrievalHit],
    config: &RankingConfig,
    req: &RetrievalRequest,
    graph_policy: GraphRetrievalPolicy,
    vector_rank_one: Option<Uuid>,
) {
    apply_feature_ranking(hits, config, req, graph_policy);
    preserve_vector_rank_one_for_policy(hits, graph_policy, vector_rank_one);
}

#[derive(Debug)]
struct ArticleAccumulator {
    object_uid: Uuid,
    source_uri: Option<String>,
    source_title: Option<String>,
    chunks: Vec<RetrievalHit>,
    rank_before_article_graph: usize,
    max_fused_score: f64,
    lexical_hit_count: usize,
    typed_graph_evidence_count: usize,
    structural_only_graph_count: usize,
    adjacent_support_count: usize,
    features: ArticleFeatureContributions,
    score: f64,
}

impl ArticleAccumulator {
    fn from_hit(hit: RetrievalHit, rank_before_article_graph: usize) -> Self {
        let chunk = hit
            .knowledge_chunk
            .as_ref()
            .expect("article accumulator is only built from hydrated knowledge chunks");
        Self {
            object_uid: chunk.object_uid,
            source_uri: chunk.source_uri.clone(),
            source_title: chunk.source_title.clone(),
            chunks: vec![hit],
            rank_before_article_graph,
            max_fused_score: f64::NEG_INFINITY,
            lexical_hit_count: 0,
            typed_graph_evidence_count: 0,
            structural_only_graph_count: 0,
            adjacent_support_count: 0,
            features: ArticleFeatureContributions::default(),
            score: 0.0,
        }
    }

    fn push(&mut self, hit: RetrievalHit, rank_before_article_graph: usize) {
        self.rank_before_article_graph = self
            .rank_before_article_graph
            .min(rank_before_article_graph);
        self.chunks.push(hit);
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CandidateGraphEvidence {
    typed_paths: usize,
    structural_only_paths: usize,
}

fn apply_article_graph_ranking(
    hits: &mut Vec<RetrievalHit>,
    req: &RetrievalRequest,
    path_traces: &[GraphPathTrace],
    vector_rank_one: Option<Uuid>,
) -> ArticleRankingDiagnostics {
    if !request_is_tenant_chunk_article_graph(req) || hits.is_empty() {
        return ArticleRankingDiagnostics::default();
    }

    let graph_evidence = graph_evidence_by_candidate(path_traces);
    let mut articles = HashMap::<Uuid, ArticleAccumulator>::new();
    let mut article_by_uid = HashMap::<Uuid, Uuid>::new();
    let mut passthrough = Vec::new();
    for (index, hit) in hits.drain(..).enumerate() {
        let rank = index + 1;
        let Some(chunk) = hit.knowledge_chunk.as_ref() else {
            passthrough.push((rank, hit));
            continue;
        };
        let object_uid = chunk.object_uid;
        article_by_uid.insert(hit.uid, object_uid);
        match articles.entry(object_uid) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().push(hit, rank);
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(ArticleAccumulator::from_hit(hit, rank));
            }
        }
    }

    if articles.is_empty() {
        hits.extend(
            passthrough
                .into_iter()
                .map(|(_, passthrough_hit)| passthrough_hit),
        );
        return ArticleRankingDiagnostics::default();
    }

    let query_tokens = normalize_tokens(&req.query_text);
    let mut ranked_articles = articles.into_values().collect::<Vec<_>>();
    for article in &mut ranked_articles {
        score_article(article, &query_tokens, &graph_evidence);
    }
    ranked_articles.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| {
                left.rank_before_article_graph
                    .cmp(&right.rank_before_article_graph)
            })
            .then_with(|| left.object_uid.cmp(&right.object_uid))
    });
    preserve_vector_article_rank_one(&mut ranked_articles, vector_rank_one, &article_by_uid);

    let mut diagnostics = ArticleRankingDiagnostics {
        enabled: true,
        ranked_article_count: ranked_articles.len(),
        feature_totals: ArticleFeatureContributions::default(),
        top_articles: Vec::new(),
    };
    let mut ordered_hits = Vec::new();
    for (index, mut article) in ranked_articles.into_iter().enumerate() {
        let article_rank = index + 1;
        diagnostics.feature_totals.add(article.features);
        if diagnostics.top_articles.len() < ARTICLE_GRAPH_DIAGNOSTIC_LIMIT {
            diagnostics
                .top_articles
                .push(article_feature_contribution(&article, article_rank));
        }
        order_article_chunks(&mut article);
        ordered_hits.extend(article.chunks);
    }
    passthrough.sort_by_key(|(rank, _)| *rank);
    ordered_hits.extend(
        passthrough
            .into_iter()
            .map(|(_, passthrough_hit)| passthrough_hit),
    );
    *hits = ordered_hits;
    diagnostics
}

fn request_is_tenant_chunk_article_graph(req: &RetrievalRequest) -> bool {
    matches!(req.scope, MemoryScope::Tenant { .. })
        && req
            .label_filter
            .as_deref()
            .is_some_and(|labels| labels == [NodeLabel::Chunk])
}

fn score_article(
    article: &mut ArticleAccumulator,
    query_tokens: &std::collections::BTreeSet<String>,
    graph_evidence: &HashMap<Uuid, CandidateGraphEvidence>,
) {
    article.max_fused_score = article
        .chunks
        .iter()
        .map(|hit| hit.score)
        .fold(f64::NEG_INFINITY, f64::max);
    article.lexical_hit_count = article.chunks.iter().filter(|hit| hit.legs.lexical).count();
    article.typed_graph_evidence_count = article
        .chunks
        .iter()
        .filter_map(|hit| graph_evidence.get(&hit.uid))
        .map(|evidence| evidence.typed_paths)
        .sum();
    article.structural_only_graph_count = article
        .chunks
        .iter()
        .filter_map(|hit| graph_evidence.get(&hit.uid))
        .map(|evidence| evidence.structural_only_paths)
        .sum();
    article.adjacent_support_count = adjacent_support_count(&article.chunks);
    article.features = article_feature_contributions(article, query_tokens);
    article.score = article.features.total();
}

fn article_feature_contributions(
    article: &ArticleAccumulator,
    query_tokens: &std::collections::BTreeSet<String>,
) -> ArticleFeatureContributions {
    let repeat_count = article.chunks.len().saturating_sub(1) as f64;
    let typed_graph_count = article.typed_graph_evidence_count as f64;
    let adjacent_support = article.adjacent_support_count as f64;
    let title_overlap = article_title_overlap(article, query_tokens);
    let lexical_bonus = if article.lexical_hit_count > 0 {
        0.025
    } else {
        0.0
    };
    let exact_title_match = if title_overlap >= 0.999 { 0.04 } else { 0.0 };
    let structural_penalty =
        if article.structural_only_graph_count > 0 && article.typed_graph_evidence_count == 0 {
            -0.04 * article.structural_only_graph_count.min(3) as f64
        } else {
            0.0
        };
    ArticleFeatureContributions {
        max_fused_score: article.max_fused_score,
        lexical_title: lexical_bonus + 0.05 * title_overlap,
        same_article_repeat: (0.015 * repeat_count).min(ARTICLE_GRAPH_MAX_REPEAT_CONTRIBUTION),
        exact_title_match,
        typed_graph_evidence: (0.03 * typed_graph_count).min(ARTICLE_GRAPH_MAX_GRAPH_CONTRIBUTION),
        adjacent_chunk_support: (0.01 * adjacent_support)
            .min(ARTICLE_GRAPH_MAX_ADJACENT_CONTRIBUTION),
        structural_only_penalty: structural_penalty,
    }
}

fn article_title_overlap(
    article: &ArticleAccumulator,
    query_tokens: &std::collections::BTreeSet<String>,
) -> f64 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let mut title_tokens = article
        .source_title
        .as_deref()
        .map(normalize_tokens)
        .unwrap_or_default();
    for chunk in article
        .chunks
        .iter()
        .filter_map(|hit| hit.knowledge_chunk.as_ref())
    {
        for heading in &chunk.heading_path {
            title_tokens.extend(normalize_tokens(heading));
        }
    }
    if title_tokens.is_empty() {
        return 0.0;
    }
    let overlap = title_tokens
        .iter()
        .filter(|token| query_tokens.contains(*token))
        .count();
    overlap as f64 / title_tokens.len() as f64
}

fn adjacent_support_count(chunks: &[RetrievalHit]) -> usize {
    let Some(best) = chunks
        .iter()
        .filter_map(|hit| hit.knowledge_chunk.as_ref().map(|chunk| (hit.score, chunk)))
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, chunk)| chunk)
    else {
        return 0;
    };
    chunks
        .iter()
        .filter_map(|hit| hit.knowledge_chunk.as_ref())
        .filter(|chunk| chunk.chunk_uid != best.chunk_uid)
        .filter(|chunk| {
            (chunk.ordinal - best.ordinal).abs() <= 1 || chunk.heading_path == best.heading_path
        })
        .count()
}

fn graph_evidence_by_candidate(
    path_traces: &[GraphPathTrace],
) -> HashMap<Uuid, CandidateGraphEvidence> {
    let mut evidence = HashMap::<Uuid, CandidateGraphEvidence>::new();
    for trace in path_traces {
        let entry = evidence.entry(trace.candidate_uid).or_default();
        if graph_path_is_structural_only(&trace.edge_labels) {
            entry.structural_only_paths += 1;
        } else {
            entry.typed_paths += 1;
        }
    }
    evidence
}

fn graph_path_is_structural_only(edge_labels: &[String]) -> bool {
    !edge_labels.is_empty()
        && edge_labels.iter().all(|label| {
            matches!(
                label.as_str(),
                "CONTAINS" | "HAS_DOCUMENT" | "HAS_CHUNK" | "contains"
            )
        })
}

fn preserve_vector_article_rank_one(
    ranked_articles: &mut [ArticleAccumulator],
    vector_rank_one: Option<Uuid>,
    article_by_uid: &HashMap<Uuid, Uuid>,
) {
    let Some(vector_rank_one) = vector_rank_one else {
        return;
    };
    let Some(vector_article) = article_by_uid.get(&vector_rank_one).copied() else {
        return;
    };
    let Some(top_article) = ranked_articles.first() else {
        return;
    };
    if top_article.object_uid == vector_article || top_article.typed_graph_evidence_count > 0 {
        return;
    }
    let Some(vector_index) = ranked_articles
        .iter()
        .position(|article| article.object_uid == vector_article)
    else {
        return;
    };
    ranked_articles[..=vector_index].rotate_right(1);
}

fn article_feature_contribution(
    article: &ArticleAccumulator,
    rank_after_article_graph: usize,
) -> ArticleFeatureContribution {
    ArticleFeatureContribution {
        object_uid: article.object_uid,
        source_uri: article.source_uri.clone(),
        source_title: article.source_title.clone(),
        chunk_count: article.chunks.len(),
        rank_before_article_graph: Some(article.rank_before_article_graph),
        rank_after_article_graph,
        rank_delta_after_minus_before: Some(
            rank_after_article_graph as i64 - article.rank_before_article_graph as i64,
        ),
        score: article.score,
        features: article.features,
        typed_graph_evidence_count: article.typed_graph_evidence_count,
        structural_only_graph_count: article.structural_only_graph_count,
    }
}

fn order_article_chunks(article: &mut ArticleAccumulator) {
    let best_ordinal = article
        .chunks
        .iter()
        .filter_map(|hit| {
            hit.knowledge_chunk
                .as_ref()
                .map(|chunk| (hit.score, chunk.ordinal))
        })
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .map(|(_, ordinal)| ordinal)
        .unwrap_or_default();
    article.chunks.sort_by(|left, right| {
        article_chunk_order_key(left, best_ordinal)
            .cmp(&article_chunk_order_key(right, best_ordinal))
            .then_with(|| right.score.total_cmp(&left.score))
            .then_with(|| left.uid.cmp(&right.uid))
    });
}

fn article_chunk_order_key(hit: &RetrievalHit, best_ordinal: i32) -> (u8, i32) {
    let Some(chunk) = &hit.knowledge_chunk else {
        return (2, i32::MAX);
    };
    let distance = (chunk.ordinal - best_ordinal).abs();
    if distance == 0 {
        (0, 0)
    } else if distance == 1 {
        (1, distance)
    } else {
        (2, distance)
    }
}

fn select_final_hits(
    primary: Vec<RetrievalHit>,
    fallback: &[RetrievalHit],
    k_final: usize,
) -> Vec<RetrievalHit> {
    let mut selected = Vec::with_capacity(k_final);
    let mut selected_uids = HashSet::new();
    let mut object_counts = HashMap::<Uuid, usize>::new();
    for hit in primary {
        push_capped_hit(
            hit,
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            k_final,
        );
        if selected.len() == k_final {
            return selected;
        }
    }
    for hit in fallback {
        push_capped_hit(
            hit.clone(),
            &mut selected,
            &mut selected_uids,
            &mut object_counts,
            k_final,
        );
        if selected.len() == k_final {
            break;
        }
    }
    selected
}

fn push_capped_hit(
    hit: RetrievalHit,
    selected: &mut Vec<RetrievalHit>,
    selected_uids: &mut HashSet<Uuid>,
    object_counts: &mut HashMap<Uuid, usize>,
    k_final: usize,
) {
    if selected.len() == k_final || !selected_uids.insert(hit.uid) {
        return;
    }
    if let Some(object_uid) = hit.knowledge_chunk.as_ref().map(|chunk| chunk.object_uid) {
        let count = object_counts.entry(object_uid).or_default();
        if *count >= MAX_FINAL_HITS_PER_KNOWLEDGE_OBJECT {
            selected_uids.remove(&hit.uid);
            return;
        }
        *count += 1;
    }
    selected.push(hit);
}

fn apply_feature_ranking(
    hits: &mut [RetrievalHit],
    config: &RankingConfig,
    req: &RetrievalRequest,
    graph_policy: GraphRetrievalPolicy,
) {
    let max_fused_score = hits.iter().map(|hit| hit.score).fold(0.0_f64, f64::max);
    let query_tokens = normalize_tokens(&req.query_text);
    let reference_time = req.ranking_reference_time.unwrap_or_else(Utc::now);
    let ranker = FeatureRanker::new(config, reference_time)
        .with_request_scope(&req.scope)
        .with_first_person_query(&req.query_text);
    for hit in hits.iter_mut() {
        let mut node = hit.node.clone();
        if let Some(chunk) = &hit.knowledge_chunk {
            node.name.clone_from(&chunk.text);
        }
        hit.score = ranker.score(hit.score, max_fused_score, &query_tokens, &node);
        if hit.legs.lexical && !hit.legs.vector && !hit.legs.graph {
            hit.score += config.weights.overlap;
        }
        if graph_policy.allows_graph_only_rescue_bonus()
            && hit.legs.graph
            && !hit.legs.vector
            && !hit.legs.lexical
        {
            hit.score += config.weights.graph_rescue;
        }
    }
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.uid.cmp(&right.uid))
    });
}

fn preserve_vector_rank_one_for_policy(
    hits: &mut [RetrievalHit],
    graph_policy: GraphRetrievalPolicy,
    vector_rank_one: Option<Uuid>,
) {
    if graph_policy != GraphRetrievalPolicy::AnchoredRescue || hits.len() < 2 {
        return;
    }
    let Some(vector_rank_one) = vector_rank_one else {
        return;
    };
    let Some(top) = hits.first() else {
        return;
    };
    if top.uid == vector_rank_one || !top.legs.graph || top.legs.vector || top.legs.lexical {
        return;
    }
    let Some(vector_index) = hits.iter().position(|hit| hit.uid == vector_rank_one) else {
        return;
    };
    hits[..=vector_index].rotate_right(1);
}

fn duration_ms_u64(elapsed: std::time::Duration) -> u64 {
    elapsed.as_millis().min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use chrono::Utc;
    use moa_core::TenantId;
    use moa_memory_graph::PiiClass;
    use moa_providers::{RerankHit, Reranker};
    use serde_json::Value;
    use uuid::Uuid;

    use super::*;

    fn tenant_scope() -> MemoryScope {
        MemoryScope::Tenant {
            tenant_id: TenantId::from(Uuid::from_u128(0x100)),
        }
    }

    fn lazy_pgvector_source(pool: &PgPool) -> Arc<PgvectorStore> {
        Arc::new(PgvectorStore::new(
            pool.clone(),
            RlsContext::tenant(TenantId::from(Uuid::from_u128(0x100))),
        ))
    }

    #[tokio::test]
    async fn reranker_reorders_candidates_when_enabled() {
        let pool = PgPool::connect_lazy("postgres://unused")
            .expect("lazy pool construction should not connect");
        let retriever = HybridRetriever::new(
            pool.clone(),
            Arc::new(EmptyGraph),
            lazy_pgvector_source(&pool),
        )
        .with_reranker(Arc::new(ReverseReranker));
        let req = RetrievalRequest {
            seeds: Vec::new(),
            query_text: "deploy provider".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: 1,
            use_reranker: true,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
        };
        let first = hit(Uuid::now_v7(), "workspace", 2.0);
        let second = hit(Uuid::now_v7(), "workspace", 1.0);

        let reranked = retriever
            .rerank_hits(&req, &[first.clone(), second.clone()])
            .await
            .expect("rerank should succeed");

        assert_eq!(reranked, vec![second]);
    }

    #[tokio::test]
    async fn reranker_receives_hydrated_chunk_text_for_knowledge_hits() {
        // Pins: provider rerankers see the full hydrated knowledge chunk rather
        // than the graph sidecar name, which is too thin for document reranking.
        let pool = PgPool::connect_lazy("postgres://unused")
            .expect("lazy pool construction should not connect");
        let observed = Arc::new(Mutex::new(Vec::new()));
        let retriever = HybridRetriever::new(
            pool.clone(),
            Arc::new(EmptyGraph),
            lazy_pgvector_source(&pool),
        )
        .with_reranker(Arc::new(RecordingReranker {
            documents: Arc::clone(&observed),
        }));
        let req = RetrievalRequest {
            seeds: Vec::new(),
            query_text: "how do I connect a custom domain?".to_string(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final: 1,
            use_reranker: true,
            strategy: None,
            as_of: None,
            ranking_reference_time: None,
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
        };
        let mut candidate = hit(Uuid::now_v7(), "tenant", 1.0);
        candidate.node.name = "thin sidecar name".to_string();
        candidate.knowledge_chunk = Some(knowledge_chunk(
            "Custom domains connect through DNS records in your site dashboard.",
        ));

        retriever
            .rerank_hits(&req, &[candidate])
            .await
            .expect("rerank should receive hydrated documents");

        let documents = observed.lock().expect("observed rerank documents");
        assert_eq!(documents.len(), 1);
        assert!(
            documents[0].contains("Custom domains connect through DNS records"),
            "reranker document should include chunk text: {}",
            documents[0]
        );
        assert!(
            !documents[0].contains("thin sidecar name"),
            "reranker document should not fall back to sidecar name when chunk text exists"
        );
    }

    #[test]
    fn feature_ranker_rescues_lexical_non_vector_hit_over_vector_noise() {
        // Pins: deterministic ranking can promote exact lexical hits that vector retrieval missed.
        let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("test timestamp should parse")
            .with_timezone(&Utc);
        let lexical_uid = Uuid::now_v7();
        let vector_uid = Uuid::now_v7();
        let mut lexical_hit = hit(lexical_uid, "workspace", 0.8);
        lexical_hit.legs = LegSources {
            graph: false,
            vector: false,
            lexical: true,
        };
        lexical_hit.node.name = "contact email".to_string();
        lexical_hit.node.valid_from = reference_time;
        lexical_hit.node.last_accessed_at = reference_time;
        lexical_hit.node.properties_summary = Some(serde_json::json!({
            "predicate": "contact_email",
            "object": "user@example.invalid"
        }));
        let mut vector_hit = hit(vector_uid, "workspace", 1.0);
        vector_hit.legs = LegSources {
            graph: false,
            vector: true,
            lexical: false,
        };
        vector_hit.node.name = "private repository".to_string();
        vector_hit.node.valid_from = reference_time;
        vector_hit.node.last_accessed_at = reference_time;
        let mut hits = vec![vector_hit, lexical_hit];

        rank_hydrated_hits(
            &mut hits,
            &RankingConfig::default(),
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "contact email".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: None,
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: Some(reference_time),
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
        );

        assert_eq!(hits[0].uid, lexical_uid);
    }

    #[test]
    fn feature_ranker_rescue_skips_graph_lexical_neighbors() {
        // Pins: lexical rescue is for lexical-only hits, not graph-expanded neighbors.
        let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("test timestamp should parse")
            .with_timezone(&Utc);
        let lexical_uid = Uuid::now_v7();
        let graph_lexical_uid = Uuid::now_v7();
        let mut lexical_hit = hit(lexical_uid, "workspace", 1.0);
        lexical_hit.legs = LegSources {
            graph: false,
            vector: false,
            lexical: true,
        };
        lexical_hit.node.valid_from = reference_time;
        lexical_hit.node.last_accessed_at = reference_time;
        let mut graph_lexical_hit = hit(graph_lexical_uid, "workspace", 1.0);
        graph_lexical_hit.legs = LegSources {
            graph: true,
            vector: false,
            lexical: true,
        };
        graph_lexical_hit.node.valid_from = reference_time;
        graph_lexical_hit.node.last_accessed_at = reference_time;
        let mut config = RankingConfig::default();
        config.weights.rrf = 0.0;
        config.weights.recency = 0.0;
        config.weights.access = 0.0;
        config.weights.subject_match = 0.0;
        config.weights.scope_tenant = 0.0;
        let mut hits = vec![graph_lexical_hit, lexical_hit];

        rank_hydrated_hits(
            &mut hits,
            &config,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "regional network".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: None,
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: Some(reference_time),
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
        );

        assert_eq!(hits[0].uid, lexical_uid);
    }

    #[test]
    fn feature_ranker_rescues_graph_only_expansion_hit() {
        // Pins: deterministic ranking can promote graph-only expansion hits that vector and lexical missed.
        let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("test timestamp should parse")
            .with_timezone(&Utc);
        let graph_uid = Uuid::now_v7();
        let vector_uid = Uuid::now_v7();
        let mut graph_hit = hit(graph_uid, "workspace", 0.8);
        graph_hit.legs = LegSources {
            graph: true,
            vector: false,
            lexical: false,
        };
        graph_hit.node.valid_from = reference_time;
        graph_hit.node.last_accessed_at = reference_time;
        let mut vector_hit = hit(vector_uid, "workspace", 1.0);
        vector_hit.legs = LegSources {
            graph: false,
            vector: true,
            lexical: false,
        };
        vector_hit.node.valid_from = reference_time;
        vector_hit.node.last_accessed_at = reference_time;
        let mut config = RankingConfig::default();
        config.weights.rrf = 0.0;
        config.weights.recency = 0.0;
        config.weights.access = 0.0;
        config.weights.subject_match = 0.0;
        config.weights.scope_tenant = 0.0;
        let mut hits = vec![vector_hit, graph_hit];

        rank_hydrated_hits(
            &mut hits,
            &config,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "library owner".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: None,
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: Some(reference_time),
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
        );

        assert_eq!(hits[0].uid, graph_uid);
    }

    #[test]
    fn anchored_rescue_preserves_vector_rank_one_over_graph_only_hit() {
        // Pins: AnchoredRescue graph-only evidence is not enough to demote the
        // vector winner; later graph policies must add an explicit threshold.
        let reference_time = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .expect("test timestamp should parse")
            .with_timezone(&Utc);
        let graph_uid = Uuid::from_u128(10);
        let vector_uid = Uuid::from_u128(20);
        let mut graph_hit = hit(graph_uid, "tenant", 1.0);
        graph_hit.legs = LegSources {
            graph: true,
            vector: false,
            lexical: false,
        };
        graph_hit.node.valid_from = reference_time;
        graph_hit.node.last_accessed_at = reference_time;
        let mut vector_hit = hit(vector_uid, "tenant", 0.1);
        vector_hit.legs = LegSources {
            graph: false,
            vector: true,
            lexical: false,
        };
        vector_hit.node.valid_from = reference_time;
        vector_hit.node.last_accessed_at = reference_time;
        let mut config = RankingConfig::default();
        config.weights.recency = 0.0;
        config.weights.access = 0.0;
        config.weights.subject_match = 0.0;
        config.weights.overlap = 0.0;
        config.weights.quality = 0.0;
        config.weights.scope_tenant = 0.0;
        let mut hits = vec![graph_hit, vector_hit];

        rank_hydrated_hits_for_policy(
            &mut hits,
            &config,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "library owner".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: Some(reference_time),
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            GraphRetrievalPolicy::AnchoredRescue,
            Some(vector_uid),
        );

        assert_eq!(hits[0].uid, vector_uid);
        assert_eq!(hits[1].uid, graph_uid);
    }

    #[test]
    fn article_graph_ranking_groups_chunks_and_reports_typed_graph_features() {
        // Pins: ArticleGraph ranks tenant knowledge at article level and reports
        // the feature movement that allowed typed graph evidence to beat the
        // initial vector article.
        let vector_uid = Uuid::from_u128(1);
        let graph_uid = Uuid::from_u128(2);
        let support_uid = Uuid::from_u128(3);
        let vector_article = Uuid::from_u128(10);
        let graph_article = Uuid::from_u128(20);
        let mut vector_hit = tenant_chunk_hit(
            vector_uid,
            vector_article,
            "Generic site settings",
            0,
            1.00,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let mut graph_hit = tenant_chunk_hit(
            graph_uid,
            graph_article,
            "Custom domain DNS records",
            4,
            0.98,
            LegSources {
                graph: true,
                vector: true,
                lexical: true,
            },
        );
        graph_hit
            .knowledge_chunk
            .as_mut()
            .expect("chunk")
            .heading_path = vec!["Custom domain DNS records".to_string()];
        let support_hit = tenant_chunk_hit(
            support_uid,
            graph_article,
            "Custom domain DNS records",
            5,
            0.75,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        vector_hit
            .knowledge_chunk
            .as_mut()
            .expect("chunk")
            .heading_path = vec!["Generic site settings".to_string()];
        let mut hits = vec![vector_hit, graph_hit, support_hit];
        let diagnostics = apply_article_graph_ranking(
            &mut hits,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "custom domain dns records".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 3,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            &[GraphPathTrace {
                seed_uid: Uuid::from_u128(99),
                seed_source: Some(GraphSeedSource::ExactPhaseOne),
                candidate_uid: graph_uid,
                hop: 1,
                edge_labels: vec!["MENTIONED_IN".to_string()],
                edge_directions: vec!["incoming".to_string()],
            }],
            Some(vector_uid),
        );

        assert_eq!(hits[0].uid, graph_uid);
        assert_eq!(hits[1].uid, support_uid);
        assert_eq!(hits[2].uid, vector_uid);
        assert!(diagnostics.enabled);
        assert_eq!(diagnostics.ranked_article_count, 2);
        assert_eq!(diagnostics.top_articles[0].object_uid, graph_article);
        assert_eq!(
            diagnostics.top_articles[0].rank_before_article_graph,
            Some(2)
        );
        assert_eq!(diagnostics.top_articles[0].rank_after_article_graph, 1);
        assert_eq!(
            diagnostics.top_articles[0].rank_delta_after_minus_before,
            Some(-1)
        );
        assert_eq!(diagnostics.top_articles[0].typed_graph_evidence_count, 1);
        assert!(diagnostics.feature_totals.typed_graph_evidence > 0.0);
        assert!(diagnostics.feature_totals.adjacent_chunk_support > 0.0);
    }

    #[test]
    fn article_graph_preserves_vector_article_without_typed_graph_evidence() {
        // Pins: ArticleGraph title and repeat signals are context organization
        // evidence, not enough by themselves to demote the vector rank-1 article.
        let vector_uid = Uuid::from_u128(11);
        let repeated_uid = Uuid::from_u128(12);
        let vector_article = Uuid::from_u128(110);
        let repeated_article = Uuid::from_u128(120);
        let vector_hit = tenant_chunk_hit(
            vector_uid,
            vector_article,
            "Vector winner",
            0,
            1.0,
            LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
        );
        let repeated_hit = tenant_chunk_hit(
            repeated_uid,
            repeated_article,
            "Custom domain DNS records",
            0,
            2.0,
            LegSources {
                graph: false,
                vector: true,
                lexical: true,
            },
        );
        let mut hits = vec![vector_hit, repeated_hit];

        let diagnostics = apply_article_graph_ranking(
            &mut hits,
            &RetrievalRequest {
                seeds: Vec::new(),
                query_text: "custom domain dns records".to_string(),
                query_embedding: Vec::new(),
                scope: tenant_scope(),
                label_filter: Some(vec![NodeLabel::Chunk]),
                max_pii_class: PiiClass::Restricted,
                k_final: 2,
                use_reranker: false,
                strategy: None,
                as_of: None,
                ranking_reference_time: None,
                lineage: None,
                disable_leg_timeouts: false,
                disable_graph_expansion: false,
            },
            &[],
            Some(vector_uid),
        );

        assert_eq!(hits[0].uid, vector_uid);
        assert_eq!(hits[1].uid, repeated_uid);
        assert_eq!(diagnostics.top_articles[0].object_uid, vector_article);
    }

    #[test]
    fn anchored_rescue_seed_selection_suppresses_broad_phase_one_fallback() {
        // Pins: AnchoredRescue does not seed graph traversal from generic top
        // vector or lexical candidates when there is no exact anchor.
        let interim = (1_u128..=3)
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let plan = interim_graph_seed_plan(
            GraphRetrievalPolicy::AnchoredRescue,
            &[],
            &[],
            &interim,
            &[],
            "generic query",
        );

        assert!(plan.strengths.is_empty());
        assert_eq!(plan.seed_counts, GraphSeedDiagnostics::default());
    }

    #[test]
    fn entity_local_search_seed_selection_accepts_semantic_entity_seeds() {
        // Pins: semantic entity anchors can start graph traversal without
        // re-enabling generic broad phase-one fallback.
        let semantic_seed = Uuid::from_u128(42);
        let interim = (1_u128..=3)
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();
        let mut semantic_row = node_row(semantic_seed, "custom domain");
        semantic_row.label = NodeLabel::Entity;

        let plan = interim_graph_seed_plan(
            GraphRetrievalPolicy::EntityLocalSearch,
            &[],
            &[semantic_seed],
            &interim,
            &[semantic_row],
            "custom domain",
        );

        assert_eq!(
            plan.strengths,
            vec![(semantic_seed, SEMANTIC_ENTITY_SEED_STRENGTH)]
        );
        assert_eq!(plan.seed_counts.semantic_entity, 1);
        assert_eq!(plan.seed_counts.broad_fallback, 0);
        assert_eq!(
            plan.seed_sources.get(&semantic_seed),
            Some(&GraphSeedSource::SemanticEntity)
        );
    }

    #[test]
    fn interim_seed_selection_keeps_planner_strength_without_broad_phase_one_fallback() {
        // Pins: planner NER seeds keep strength 1.0 and suppress broad phase-one fallback.
        let collision = Uuid::from_u128(1);
        let interim = (1_u128..=(PHASE_ONE_GRAPH_SEED_LIMIT as u128 + 4))
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let strengths = interim_graph_seed_strengths(&[collision], &interim, &[], "");

        assert_eq!(strengths, vec![(collision, 1.0)]);
    }

    #[test]
    fn graph_seed_selection_caps_broad_phase_one_when_planner_and_exact_seeds_empty() {
        // Pins: graph expansion can still run when NER finds no planner seeds.
        let interim = (1_u128..=(PHASE_ONE_GRAPH_SEED_LIMIT as u128 + 4))
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let strengths = interim_graph_seed_strengths(&[], &interim, &[], "");

        assert_eq!(strengths.len(), PHASE_ONE_GRAPH_SEED_LIMIT);
        assert_eq!(strengths[0], (Uuid::from_u128(1), 1.0));
        assert_eq!(strengths[1], (Uuid::from_u128(2), 0.85));
        let (last_uid, last_strength) = strengths.last().expect("last seed should exist");
        assert_eq!(
            *last_uid,
            Uuid::from_u128(PHASE_ONE_GRAPH_SEED_LIMIT as u128)
        );
        assert!(
            (*last_strength - PHASE_ONE_SEED_DECAY.powi((PHASE_ONE_GRAPH_SEED_LIMIT - 1) as i32))
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn default_graph_policy_suppresses_broad_phase_one_fallback() {
        // Pins: the production default uses AnchoredRescue guardrails and does
        // not seed graph traversal from generic phase-one vector/lexical hits.
        let interim = (1_u128..=3)
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let plan = interim_graph_seed_plan(
            GraphRetrievalPolicy::default(),
            &[],
            &[],
            &interim,
            &[],
            "generic query",
        );

        assert_eq!(
            GraphRetrievalPolicy::default(),
            GraphRetrievalPolicy::AnchoredRescue
        );
        assert!(plan.strengths.is_empty());
        assert_eq!(plan.seed_counts, GraphSeedDiagnostics::default());
    }

    #[test]
    fn explicit_legacy_graph_policy_preserves_broad_phase_one_fallback() {
        // Pins: the legacy A/B policy still exposes the old broad fallback
        // behavior without making it the default.
        let interim = (1_u128..=3)
            .map(|value| (Uuid::from_u128(value), 1.0, LegSources::default()))
            .collect::<Vec<_>>();

        let plan = interim_graph_seed_plan(
            GraphRetrievalPolicy::LegacyBroadExpansion,
            &[],
            &[],
            &interim,
            &[],
            "generic query",
        );

        assert_eq!(
            plan.strengths,
            vec![
                (Uuid::from_u128(1), 1.0),
                (Uuid::from_u128(2), PHASE_ONE_SEED_DECAY),
                (Uuid::from_u128(3), PHASE_ONE_SEED_DECAY.powi(2)),
            ]
        );
        assert_eq!(
            plan.seed_counts,
            GraphSeedDiagnostics {
                planner: 0,
                exact_phase_one: 0,
                broad_fallback: 3,
                semantic_entity: 0,
            }
        );
    }

    #[test]
    fn graph_candidate_counts_split_graph_overlap_buckets() {
        // Pins: report diagnostics distinguish graph-only, pairwise overlap,
        // and all-leg candidates instead of reporting one aggregate graph count.
        let fused = vec![
            (
                Uuid::from_u128(1),
                1.0,
                LegSources {
                    graph: true,
                    vector: false,
                    lexical: false,
                },
            ),
            (
                Uuid::from_u128(2),
                1.0,
                LegSources {
                    graph: true,
                    vector: true,
                    lexical: false,
                },
            ),
            (
                Uuid::from_u128(3),
                1.0,
                LegSources {
                    graph: true,
                    vector: false,
                    lexical: true,
                },
            ),
            (
                Uuid::from_u128(4),
                1.0,
                LegSources {
                    graph: true,
                    vector: true,
                    lexical: true,
                },
            ),
            (
                Uuid::from_u128(5),
                1.0,
                LegSources {
                    graph: false,
                    vector: true,
                    lexical: true,
                },
            ),
        ];

        assert_eq!(
            graph_candidate_counts(&fused),
            GraphCandidateCounts {
                graph_only: 1,
                vector_graph: 1,
                lexical_graph: 1,
                all_legs: 1,
            }
        );
    }

    #[test]
    fn interim_seed_selection_uses_exact_phase_one_subject_match_without_broad_siblings() {
        // Pins: exact entity mentions seed graph expansion without same-shape siblings.
        let sibling = Uuid::from_u128(10);
        let exact = Uuid::from_u128(11);
        let interim = vec![
            (sibling, 1.0, LegSources::default()),
            (exact, 0.9, LegSources::default()),
        ];
        let rows = vec![
            node_row(sibling, "audit-shipper-dep-0-4-0"),
            node_row(exact, "audit-shipper-dep-0-0-0"),
        ];

        let strengths = interim_graph_seed_strengths(
            &[],
            &interim,
            &rows,
            "Which team owns the library that audit-shipper-dep-0-0-0 depends on?",
        );

        assert_eq!(strengths, vec![(exact, 1.0)]);
    }

    #[test]
    fn interim_seed_selection_keeps_planner_first_and_exact_phase_one_only() {
        // Pins: planner seeds stay first while exact phase-one subjects pass through alone.
        let planner = Uuid::from_u128(9);
        let sibling = Uuid::from_u128(10);
        let exact = Uuid::from_u128(11);
        let interim = vec![
            (sibling, 1.0, LegSources::default()),
            (exact, 0.9, LegSources::default()),
        ];
        let rows = vec![
            node_row(sibling, "audit-shipper-dep-0-4-0"),
            node_row(exact, "audit-shipper-dep-0-0-0"),
        ];

        let strengths = interim_graph_seed_strengths(
            &[planner],
            &interim,
            &rows,
            "Which team owns the library that audit-shipper-dep-0-0-0 depends on?",
        );

        assert_eq!(strengths, vec![(planner, 1.0), (exact, 1.0)]);
    }

    #[test]
    fn strategy_weighting_unchanged_after_two_phase_restructure() {
        // Pins: GraphFirst still halves only the lexical fusion weight.
        assert_eq!(
            weights_for(Strategy::GraphFirst),
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT * 0.5)
        );
        assert_eq!(
            weights_for(Strategy::VectorFirst),
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT)
        );
        assert_eq!(
            weights_for(Strategy::Both),
            (GRAPH_WEIGHT, VECTOR_WEIGHT, LEXICAL_WEIGHT)
        );
    }

    #[test]
    fn turbopuffer_bm25_chunk_leg_is_boost_only() {
        // Pins: tenant knowledge BM25 may boost vector candidates, but it cannot
        // flood final chunk retrieval with BM25-only candidates.
        let shared = Uuid::from_u128(1);
        let lexical_only = Uuid::from_u128(2);
        let vector_hits = vec![leg_candidate(shared)];
        let mut lexical_hits = LexicalLegOutput::new(
            vec![leg_candidate(lexical_only), leg_candidate(shared)],
            LexicalBackend::TurbopufferBm25,
        );
        let mut req = vector_request();
        req.label_filter = Some(vec![NodeLabel::Chunk]);

        apply_lexical_boost_only_policy(&req, &vector_hits, &mut lexical_hits);

        assert_eq!(lexical_hits.candidates.len(), 1);
        assert_eq!(lexical_hits.candidates[0].uid, shared);
        assert_eq!(
            lexical_hits.candidates[0].score,
            leg_candidate(shared).score * TURBOPUFFER_BM25_BOOST_MULTIPLIER
        );
    }

    #[test]
    fn turbopuffer_bm25_lexical_only_request_keeps_candidates() {
        // Pins: exact lexical tenant-chunk requests without a query embedding do
        // not drop all BM25 candidates just because the vector leg is empty.
        let lexical_only = Uuid::from_u128(2);
        let mut lexical_hits = LexicalLegOutput::new(
            vec![leg_candidate(lexical_only)],
            LexicalBackend::TurbopufferBm25,
        );
        let mut req = vector_request();
        req.query_embedding.clear();
        req.label_filter = Some(vec![NodeLabel::Chunk]);

        apply_lexical_boost_only_policy(&req, &[], &mut lexical_hits);

        assert_eq!(lexical_hits.candidates, vec![leg_candidate(lexical_only)]);
    }

    #[test]
    fn postgres_lexical_leg_can_still_add_candidates() {
        // Pins: the boost-only rule is scoped to Turbopuffer BM25 tenant chunks;
        // existing Postgres lexical behavior for memory/exact matches is unchanged.
        let lexical_only = Uuid::from_u128(2);
        let mut lexical_hits = LexicalLegOutput::new(
            vec![leg_candidate(lexical_only)],
            LexicalBackend::PostgresTsvector,
        );
        let req = vector_request();

        apply_lexical_boost_only_policy(&req, &[], &mut lexical_hits);

        assert_eq!(lexical_hits.candidates, vec![leg_candidate(lexical_only)]);
    }

    #[test]
    fn vector_first_strategy_disables_turbopuffer_bm25() {
        // Pins: tenant-KB vector-first retrieval does not pay for, or fuse in,
        // the Turbopuffer BM25 leg after WixQA showed it hurts rank quality.
        let mut req = vector_request();
        req.label_filter = Some(vec![NodeLabel::Chunk]);
        req.strategy = Some(Strategy::VectorFirst);

        assert!(!request_allows_tenant_chunk_bm25(&req));
    }

    #[test]
    fn fused_candidate_limit_scales_with_requested_final_count() {
        // Pins: widened retrieval cutoffs are not silently capped at the old
        // fixed candidate pool, but production requests still have a hard cap.
        assert_eq!(fused_candidate_limit(0), 0);
        assert_eq!(fused_candidate_limit(5), MIN_FUSED_CANDIDATE_LIMIT);
        assert_eq!(fused_candidate_limit(25), 50);
        assert_eq!(fused_candidate_limit(50), 100);
        assert_eq!(fused_candidate_limit(500), 100);
    }

    #[tokio::test]
    async fn retrieve_returns_empty_when_k_final_is_zero() {
        // Pins: a zero-budget retrieval short-circuits before touching any leg.
        let retriever = lazy_retriever();
        let hits = retriever
            .retrieve(empty_corpus_request(0, false))
            .await
            .expect("k_final=0 should early-return");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn retrieve_returns_empty_for_empty_corpus() {
        // Pins: when every leg yields nothing the fused set is empty and retrieval
        // returns [] without hydrating nodes. Hermetic because an empty query and
        // empty embedding keep all three legs off the database.
        let retriever = lazy_retriever();
        let hits = retriever
            .retrieve(empty_corpus_request(5, false))
            .await
            .expect("empty corpus should return []");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn retrieve_does_not_invoke_reranker_for_empty_corpus() {
        // Pins: the billed reranker is not called when no candidates exceed
        // k_final (here the corpus is empty), even with use_reranker = true.
        let retriever = lazy_retriever().with_reranker(Arc::new(PanicReranker));
        let hits = retriever
            .retrieve(empty_corpus_request(5, true))
            .await
            .expect("empty corpus should return [] without reranking");
        assert!(hits.is_empty());
    }

    #[tokio::test]
    async fn turbopuffer_vector_leg_requires_configured_client() {
        // Pins: a Turbopuffer-selected cloud partition fails closed when the
        // client is missing instead of silently using pgvector.
        let retriever = lazy_retriever();
        let error = retriever
            .vector_leg(
                &vector_request(),
                &VectorBackendState {
                    vector_backend: "turbopuffer".to_string(),
                    vector_backend_state: "steady".to_string(),
                    dual_read_until: None,
                },
            )
            .await
            .expect_err("Turbopuffer backend selection should require a client");

        assert!(matches!(
            error,
            RetrievalError::Vector(VectorError::TurbopufferUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn turbopuffer_lexical_leg_requires_configured_client_for_chunk_bm25() {
        // Pins: tenant knowledge chunk BM25 is a Turbopuffer cloud path and must
        // not degrade to Postgres lexical when the client is absent.
        let retriever = lazy_retriever();
        let mut req = vector_request();
        req.query_embedding.clear();
        req.query_text = "deployment runbook".to_string();
        req.label_filter = Some(vec![NodeLabel::Chunk]);
        let error = retriever
            .lexical_leg(
                &req,
                &VectorBackendState {
                    vector_backend: "turbopuffer".to_string(),
                    vector_backend_state: "steady".to_string(),
                    dual_read_until: None,
                },
            )
            .await
            .expect_err("Turbopuffer BM25 backend selection should require a client");

        assert!(matches!(
            error,
            RetrievalError::Vector(VectorError::TurbopufferUnavailable { .. })
        ));
    }

    #[tokio::test]
    async fn turbopuffer_dual_read_requires_configured_client() {
        // Pins: promotion dual-read is part of the cloud Turbopuffer path and
        // should fail clearly if the client is missing.
        let retriever = lazy_retriever();
        let error = retriever
            .dual_read_vector_leg(&vector_request())
            .await
            .expect_err("dual-read should require a Turbopuffer client");

        assert!(matches!(
            error,
            RetrievalError::Vector(VectorError::TurbopufferUnavailable { .. })
        ));
    }

    #[test]
    fn leg_overlap_measures_top_k_set_intersection() {
        // Pins: dual-read overlap is the top-k uid intersection size over the
        // larger top-k set, and it honors the k cutoff.
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        let d = Uuid::from_u128(4);

        // Disjoint top-k -> zero overlap.
        assert_eq!(
            leg_overlap(
                &[leg_candidate(a), leg_candidate(b)],
                &[leg_candidate(c), leg_candidate(d)],
                10,
            ),
            0.0
        );
        // Identical top-k -> full overlap.
        assert_eq!(
            leg_overlap(
                &[leg_candidate(a), leg_candidate(b)],
                &[leg_candidate(a), leg_candidate(b)],
                10,
            ),
            1.0
        );
        // Partial overlap: {a,b} vs {a,c} over k=2 -> 1/2.
        assert_eq!(
            leg_overlap(
                &[leg_candidate(a), leg_candidate(b)],
                &[leg_candidate(a), leg_candidate(c)],
                2,
            ),
            0.5
        );
        // k cutoff: the lists diverge only past position 2, so the top-2 overlap
        // is full even though the full lists differ.
        assert_eq!(
            leg_overlap(
                &[leg_candidate(a), leg_candidate(b), leg_candidate(c)],
                &[leg_candidate(b), leg_candidate(a), leg_candidate(d)],
                2,
            ),
            1.0
        );
    }

    #[test]
    fn is_dual_read_active_respects_state_and_expiry() {
        // Pins: dual-read is active only in the dual_read state and only while the
        // dual_read_until deadline is in the future (or unset).
        let future = Utc::now() + chrono::Duration::hours(1);
        let past = Utc::now() - chrono::Duration::hours(1);

        assert!(
            VectorBackendState {
                vector_backend: "turbopuffer".to_string(),
                vector_backend_state: "dual_read".to_string(),
                dual_read_until: Some(future),
            }
            .is_dual_read_active(),
            "dual_read with a future deadline is active"
        );
        assert!(
            VectorBackendState {
                vector_backend: "turbopuffer".to_string(),
                vector_backend_state: "dual_read".to_string(),
                dual_read_until: None,
            }
            .is_dual_read_active(),
            "dual_read with no deadline is active"
        );
        assert!(
            !VectorBackendState {
                vector_backend: "turbopuffer".to_string(),
                vector_backend_state: "dual_read".to_string(),
                dual_read_until: Some(past),
            }
            .is_dual_read_active(),
            "an expired deadline ends dual-read"
        );
        assert!(
            !VectorBackendState {
                vector_backend: "pgvector".to_string(),
                vector_backend_state: "steady".to_string(),
                dual_read_until: Some(future),
            }
            .is_dual_read_active(),
            "the steady state is never dual-read"
        );
    }

    fn lazy_retriever() -> HybridRetriever {
        let pool = PgPool::connect_lazy("postgres://unused")
            .expect("lazy pool construction should not connect");
        HybridRetriever::new(
            pool.clone(),
            Arc::new(EmptyGraph),
            lazy_pgvector_source(&pool),
        )
    }

    fn empty_corpus_request(k_final: usize, use_reranker: bool) -> RetrievalRequest {
        RetrievalRequest {
            seeds: Vec::new(),
            query_text: String::new(),
            query_embedding: Vec::new(),
            scope: tenant_scope(),
            label_filter: None,
            max_pii_class: PiiClass::Restricted,
            k_final,
            use_reranker,
            strategy: None,
            as_of: None,
            ranking_reference_time: Some(Utc::now()),
            lineage: None,
            disable_leg_timeouts: false,
            disable_graph_expansion: false,
        }
    }

    fn vector_request() -> RetrievalRequest {
        RetrievalRequest {
            query_text: "deployment runbook".to_string(),
            query_embedding: vec![0.0; 1024],
            ..empty_corpus_request(5, false)
        }
    }

    fn leg_candidate(uid: Uuid) -> LegCandidate {
        LegCandidate { uid, score: 1.0 }
    }

    struct PanicReranker;

    #[async_trait]
    impl Reranker for PanicReranker {
        async fn rerank(
            &self,
            _model: &str,
            _query: &str,
            _documents: &[String],
            _top_n: usize,
        ) -> moa_core::Result<Vec<RerankHit>> {
            panic!("reranker must not be called when no candidates exceed k_final");
        }
    }

    struct RecordingReranker {
        documents: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl Reranker for RecordingReranker {
        async fn rerank(
            &self,
            _model: &str,
            _query: &str,
            documents: &[String],
            top_n: usize,
        ) -> moa_core::Result<Vec<RerankHit>> {
            *self
                .documents
                .lock()
                .expect("recording reranker document lock") = documents.to_vec();
            Ok((0..documents.len().min(top_n))
                .map(|index| RerankHit {
                    index,
                    relevance_score: 1.0,
                })
                .collect())
        }
    }

    fn hit(uid: Uuid, scope: &str, score: f64) -> RetrievalHit {
        RetrievalHit {
            uid,
            score,
            legs: LegSources {
                graph: false,
                vector: true,
                lexical: false,
            },
            lexical_backend: None,
            source_tier: SourceTier::UserMemory,
            knowledge_chunk: None,
            node: NodeIndexRow {
                uid,
                label: NodeLabel::Fact,
                storage_partition_id: Some("tenant".to_string()),
                contact_id: None,
                scope: scope.to_string(),
                name: format!("{scope} fact"),
                pii_class: PiiClass::None,
                valid_to: None,
                valid_from: Utc::now(),
                properties_summary: None,
                last_accessed_at: Utc::now(),
                quality_score: 0.5,
            },
        }
    }

    fn knowledge_chunk(text: &str) -> KnowledgeChunkHydration {
        KnowledgeChunkHydration {
            chunk_uid: Uuid::now_v7(),
            document_version_uid: Uuid::now_v7(),
            object_uid: Uuid::now_v7(),
            chunk_hash: "chunk-hash".to_string(),
            ordinal: 0,
            heading_path: vec!["Domains".to_string()],
            text: text.to_string(),
            token_count: 16,
            metadata: Value::Null,
            source_uri: Some("https://support.example.invalid/domain".to_string()),
            source_title: Some("Custom domains".to_string()),
            object_type: "article".to_string(),
        }
    }

    fn tenant_chunk_hit(
        uid: Uuid,
        object_uid: Uuid,
        source_title: &str,
        ordinal: i32,
        score: f64,
        legs: LegSources,
    ) -> RetrievalHit {
        let mut hit = hit(uid, "tenant", score);
        hit.legs = legs;
        hit.source_tier = SourceTier::TenantKnowledge;
        hit.node.label = NodeLabel::Chunk;
        hit.knowledge_chunk = Some(KnowledgeChunkHydration {
            chunk_uid: Uuid::from_u128(10_000 + uid.as_u128()),
            document_version_uid: Uuid::from_u128(20_000 + object_uid.as_u128()),
            object_uid,
            chunk_hash: format!("chunk-{uid}"),
            ordinal,
            heading_path: vec![source_title.to_string()],
            text: format!("{source_title} body"),
            token_count: 16,
            metadata: Value::Null,
            source_uri: Some(format!("https://support.example.invalid/{object_uid}")),
            source_title: Some(source_title.to_string()),
            object_type: "article".to_string(),
        });
        hit
    }

    fn node_row(uid: Uuid, name: &str) -> NodeIndexRow {
        NodeIndexRow {
            uid,
            label: NodeLabel::Fact,
            storage_partition_id: Some("tenant".to_string()),
            contact_id: None,
            scope: "tenant".to_string(),
            name: name.to_string(),
            pii_class: PiiClass::None,
            valid_to: None,
            valid_from: Utc::now(),
            properties_summary: None,
            last_accessed_at: Utc::now(),
            quality_score: 0.5,
        }
    }

    struct ReverseReranker;

    #[async_trait]
    impl Reranker for ReverseReranker {
        async fn rerank(
            &self,
            _model: &str,
            _query: &str,
            documents: &[String],
            top_n: usize,
        ) -> moa_core::Result<Vec<RerankHit>> {
            Ok((0..documents.len())
                .rev()
                .take(top_n)
                .map(|index| RerankHit {
                    index,
                    relevance_score: 1.0,
                })
                .collect())
        }
    }

    struct EmptyGraph;

    #[async_trait]
    impl GraphStore for EmptyGraph {
        async fn create_node(
            &self,
            _intent: moa_memory_graph::NodeWriteIntent,
        ) -> std::result::Result<Uuid, GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn supersede_node(
            &self,
            _old_uid: Uuid,
            _intent: moa_memory_graph::NodeWriteIntent,
        ) -> std::result::Result<Uuid, GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn invalidate_node(
            &self,
            _uid: Uuid,
            _reason: &str,
        ) -> std::result::Result<(), GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn hard_purge(
            &self,
            _uid: Uuid,
            _redaction_marker: &str,
        ) -> std::result::Result<(), GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn create_edge(
            &self,
            _intent: moa_memory_graph::EdgeWriteIntent,
        ) -> std::result::Result<Uuid, GraphError> {
            unreachable!("not used by retrieval tests")
        }

        async fn get_node(
            &self,
            _uid: Uuid,
        ) -> std::result::Result<Option<NodeIndexRow>, GraphError> {
            Ok(None)
        }

        async fn neighbors(
            &self,
            _seed: Uuid,
            _hops: u8,
            _edge_filter: Option<&[moa_memory_graph::EdgeLabel]>,
            _as_of: Option<DateTime<Utc>>,
        ) -> std::result::Result<Vec<NodeIndexRow>, GraphError> {
            Ok(Vec::new())
        }

        async fn expand_seeds(
            &self,
            _seeds: &[Uuid],
            _max_hops: u8,
            _as_of: Option<DateTime<Utc>>,
        ) -> std::result::Result<Vec<moa_memory_graph::GraphExpansionHit>, GraphError> {
            Ok(Vec::new())
        }

        async fn lookup_seeds(
            &self,
            _name: &str,
            _limit: i64,
            _as_of: Option<DateTime<Utc>>,
        ) -> std::result::Result<Vec<NodeIndexRow>, GraphError> {
            Ok(Vec::new())
        }
    }
}
