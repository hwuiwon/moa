//! Shared retrieval request, result, and diagnostics types.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use moa_core::types::security::SensitivityClass;
use moa_core::types::{
    identifiers::SessionId,
    memory::{InformationBarrierClearances, SourceAclContext},
};
use moa_lineage_core::TurnId;
use moa_memory_graph::{Error, NodeIndexRow, NodeLabel};
use moa_memory_types::MemoryScope;
use moa_memory_vector::{Error as VectorError, QueryEmbedding};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::planning::Strategy;
use crate::retrieval::policy::GraphRetrievalPolicy;

/// Result type returned by hybrid retrieval.
pub type Result<T> = std::result::Result<T, RetrievalError>;

/// Error returned by hybrid retrieval.
#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    /// Graph traversal failed.
    #[error("graph retrieval: {0}")]
    Graph(#[from] Error),
    /// Vector KNN failed.
    #[error("vector retrieval: {0}")]
    Vector(#[from] VectorError),
    /// Postgres sidecar access failed.
    #[error("postgres retrieval: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// Scoped Postgres connection setup failed.
    #[error("scope setup: {0}")]
    Scope(#[from] moa_core::error::MoaError),
    /// The query embedder does not match the partition's active generation.
    #[error(
        "storage partition {storage_partition_id} serves generation embedded by `{generation_model}`, but the query was embedded by `{query_model}`"
    )]
    GenerationEmbedderMismatch {
        /// Storage partition whose generation was consulted.
        storage_partition_id: String,
        /// Embedding identity of the served generation.
        generation_model: String,
        /// Embedding identity of the query.
        query_model: String,
    },
}

/// Evidence-window policy applied to the final hits of one retrieval.
///
/// These knobs are calibrated per retrieval PATH, not globally: the memory
/// evidence window is a small injected block that benefits from a tight
/// reranked window and whole-window abstention, while a knowledge-lane top-k
/// retrieval sizes its own window and must never be clamped by memory-lane
/// values. They therefore ride the request instead of the shared retriever —
/// the retriever must not impose them. A retriever-global window policy is what
/// clamped a knowledge-lane `k = 10` retrieval to the memory-lane
/// `rerank_window = 3` in the 2026-07-11 MultiHop-RAG incident, cutting
/// recall@10 from 0.227 to 0.144.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EvidenceWindowPolicy {
    /// Final window size when a real reranker is active (`0` keeps `k_final`).
    pub rerank_window: usize,
    /// Whole-window abstain threshold on best evidence (`0.0` disables).
    pub abstain_below_window_evidence: f64,
}

/// Retrieval request supplied by the query planner.
#[derive(Debug, Clone)]
pub struct RetrievalRequest {
    /// NER seed node ids for graph traversal.
    pub seeds: Vec<Uuid>,
    /// Query text used by lexical retrieval and reranking.
    pub query_text: String,
    /// Dense query embedding and its model identity, or `None` for lexical-only retrieval.
    pub query_embedding: Option<QueryEmbedding>,
    /// Request memory scope used for sidecar RLS GUCs.
    pub scope: MemoryScope,
    /// The caller's resolved provider-source admission context.
    ///
    /// Resolved once, durably, at the retrieval entry point from authenticated
    /// session/contact identity plus verified provider bindings — never from a
    /// request payload and never refreshed inside a leg. Every leg passes its
    /// bounded opaque fingerprints as bind parameters to the shared SQL
    /// admission predicate, and the aggregate fingerprint plus ACL epoch are
    /// part of cache identity. Required with no default: a retrieval that had to
    /// infer it would infer the permissive answer.
    pub source_acl: SourceAclContext,
    /// Information barriers the caller is cleared for (need-to-know).
    ///
    /// Sourced from the running agent's knowledge policy at the retrieval entry
    /// point and installed as the `moa.cleared_barriers` GUC by every scoped
    /// retrieval leg, so a cleared agent sees nodes tagged with a cleared
    /// barrier. Empty fails closed: barriered nodes stay hidden.
    pub cleared_barriers: InformationBarrierClearances,
    /// Optional HARD graph node label allowlist.
    ///
    /// Only ever set from structured, explicit input (a scope plan's
    /// `label_filter`). Candidates whose label is not in this list are dropped
    /// by every retrieval leg. A planner-inferred label guess must NOT populate
    /// this field — use [`Self::label_boost`] instead so a wrong guess degrades
    /// ranking rather than silently excluding the answer.
    pub label_filter: Option<Vec<NodeLabel>>,
    /// Optional SOFT planner-inferred label hint.
    ///
    /// Candidates whose label matches receive a bounded additive ranking boost
    /// (`RankingWeights::label_hint_boost`), but candidates with other labels
    /// are still retrieved and ranked. This is the low-risk home for the
    /// keyword-guessed label the planner infers from query wording.
    pub label_boost: Option<Vec<NodeLabel>>,
    /// Maximum PII class visible to the caller.
    pub max_pii_class: SensitivityClass,
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
    /// Evidence-window policy calibrated for this retrieval path.
    pub window_policy: EvidenceWindowPolicy,
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

/// Per-feature contribution that produced one source-object score.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceObjectFeatureContributions {
    /// Highest hydrated chunk score before source-object aggregation.
    pub max_fused_score: f64,
    /// Lexical hit and title/heading token overlap contribution.
    pub lexical_title: f64,
    /// Disabled coherence contribution from multiple chunks from the same source object.
    pub same_source_object_repeat: f64,
    /// Contribution from an exact title-like query match.
    pub exact_title_match: f64,
    /// Contribution from non-structural graph paths into source object chunks.
    pub typed_graph_evidence: f64,
    /// Disabled coherence contribution from chunks near the best same-source-object chunk.
    pub adjacent_chunk_support: f64,
    /// Negative contribution for graph paths that are only structural.
    pub structural_only_penalty: f64,
}

impl SourceObjectFeatureContributions {
    /// Adds another feature contribution set into this one.
    pub fn add(&mut self, other: Self) {
        self.max_fused_score += other.max_fused_score;
        self.lexical_title += other.lexical_title;
        self.same_source_object_repeat += other.same_source_object_repeat;
        self.exact_title_match += other.exact_title_match;
        self.typed_graph_evidence += other.typed_graph_evidence;
        self.adjacent_chunk_support += other.adjacent_chunk_support;
        self.structural_only_penalty += other.structural_only_penalty;
    }

    /// Returns the total contribution across source-object ranking features.
    #[must_use]
    pub(crate) fn total(self) -> f64 {
        self.max_fused_score
            + self.lexical_title
            + self.same_source_object_repeat
            + self.exact_title_match
            + self.typed_graph_evidence
            + self.adjacent_chunk_support
            + self.structural_only_penalty
    }
}

/// Source-object score explanation emitted by `SourceGraph` retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceObjectFeatureContribution {
    /// Tenant knowledge object that represents the source object.
    pub object_uid: Uuid,
    /// Citation URI for the source object when available.
    pub source_uri: Option<String>,
    /// Renderer-safe source title when available.
    pub source_title: Option<String>,
    /// Candidate chunk count grouped into this source object.
    pub chunk_count: usize,
    /// First source object rank before source-object aggregation.
    pub rank_before_source_graph: Option<usize>,
    /// Source object rank after source-object aggregation.
    pub rank_after_source_graph: usize,
    /// Rank movement where negative means the source object moved earlier.
    pub rank_delta_after_minus_before: Option<i64>,
    /// Final source object score.
    pub score: f64,
    /// Feature contribution breakdown.
    pub features: SourceObjectFeatureContributions,
    /// Non-structural graph path count into source object chunks.
    pub typed_graph_evidence_count: usize,
    /// Structural-only graph path count into source object chunks.
    pub structural_only_graph_count: usize,
}

/// Source-object diagnostics emitted when `SourceGraph` ranking runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SourceObjectRankingDiagnostics {
    /// Whether source-object ranking ran for this request.
    pub enabled: bool,
    /// Number of grouped tenant knowledge source objects.
    pub ranked_source_object_count: usize,
    /// Sum of feature contributions across ranked source objects.
    pub feature_totals: SourceObjectFeatureContributions,
    /// Top source-object explanations after ranking.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_source_objects: Vec<SourceObjectFeatureContribution>,
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
    /// Source-object ranking diagnostics when `SourceGraph` is active.
    pub source_object_ranking: SourceObjectRankingDiagnostics,
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
            source_object_ranking: SourceObjectRankingDiagnostics::default(),
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
    /// Observation-only provenance threaded into retrieval lineage.
    pub provenance: RetrievalProvenance,
}

/// Per-stage retrieval latency in milliseconds.
///
/// Captured for retrieval spans and lineage. Legs that did not run stay zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetrievalStageTimings {
    /// Vector-leg wall-clock latency.
    pub vector_ms: u32,
    /// Lexical-leg wall-clock latency.
    pub lexical_ms: u32,
    /// Graph-expansion-leg wall-clock latency.
    pub graph_ms: u32,
    /// Reciprocal-rank fusion latency.
    pub fusion_ms: u32,
    /// Rerank latency when a reranker ran.
    pub rerank_ms: u32,
}

impl RetrievalStageTimings {
    /// Sums another stage-timing set into this one, saturating on overflow.
    pub fn add(&mut self, other: Self) {
        self.vector_ms = self.vector_ms.saturating_add(other.vector_ms);
        self.lexical_ms = self.lexical_ms.saturating_add(other.lexical_ms);
        self.graph_ms = self.graph_ms.saturating_add(other.graph_ms);
        self.fusion_ms = self.fusion_ms.saturating_add(other.fusion_ms);
        self.rerank_ms = self.rerank_ms.saturating_add(other.rerank_ms);
    }
}

/// One reranked candidate's resolved position and score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankScore {
    /// Reranked candidate graph node uid.
    pub uid: Uuid,
    /// Pre-rerank index of the candidate in the fused window.
    pub original_index: u16,
    /// Backend relevance score assigned by the reranker.
    pub relevance_score: f32,
}

/// Observation-only provenance captured during one hybrid retrieval.
///
/// Threaded to retrieval lineage so the persisted record carries real per-stage
/// timings, reranker scores and model, and graph paths instead of constants.
/// Nothing here influences ranking; the values are captured after ranking has
/// already decided the result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetrievalProvenance {
    /// Per-stage latency for the exercised legs.
    pub timings: RetrievalStageTimings,
    /// Resolved reranker model when a reranker ran and produced scores.
    pub rerank_model: Option<String>,
    /// Per-candidate reranker scores when a reranker ran.
    pub rerank_scores: Vec<RerankScore>,
    /// Raw graph traversal paths that contributed candidates.
    pub graph_paths: Vec<GraphPathTrace>,
    /// Candidates rejected by the memory admission policy for this turn.
    pub admission_rejected: usize,
}

impl RetrievalProvenance {
    /// Folds another provenance set into this one for multi-scope/sub-query fan-in.
    ///
    /// Timings sum, reranker scores and graph paths concatenate, the first
    /// resolved reranker model wins, and admission rejections accumulate.
    pub fn merge(&mut self, other: RetrievalProvenance) {
        self.timings.add(other.timings);
        if self.rerank_model.is_none() {
            self.rerank_model = other.rerank_model;
        }
        self.rerank_scores.extend(other.rerank_scores);
        self.graph_paths.extend(other.graph_paths);
        self.admission_rejected = self
            .admission_rejected
            .saturating_add(other.admission_rejected);
    }
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

/// One ranked hit recorded into the `moa.retrieval_lineage` sidecar table.
///
/// Chunk provenance is denormalized here so dashboard queries resolve a
/// retrieval row to its source document without joining through
/// `moa.knowledge_chunks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrievalLineageHit {
    /// Retrieved graph node uid.
    pub uid: Uuid,
    /// Tenant knowledge chunk uid when the hit is a knowledge chunk.
    pub chunk_uid: Option<Uuid>,
    /// Knowledge document version containing the chunk, when known.
    pub document_version_uid: Option<Uuid>,
}

impl RetrievalLineageHit {
    /// Extracts the lineage row payload from one ranked retrieval hit.
    #[must_use]
    pub fn from_hit(hit: &RetrievalHit) -> Self {
        let chunk = hit.knowledge_chunk.as_ref();
        Self {
            uid: hit.uid,
            chunk_uid: chunk.map(|chunk| chunk.chunk_uid),
            document_version_uid: chunk.map(|chunk| chunk.document_version_uid),
        }
    }
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

/// One ordinal-adjacent neighbor chunk that expands a matched chunk's context.
///
/// Parent-document ("small-to-big") retrieval matches on a small chunk but
/// renders the neighbors around it as expanded context. Each part carries the
/// neighbor's ordinal within the document version and its normalized text. The
/// matched chunk itself is never represented here.
#[derive(Debug, Clone, PartialEq)]
pub struct KnowledgeChunkWindowPart {
    /// Neighbor chunk ordinal within the same document version.
    pub ordinal: i32,
    /// Full normalized neighbor chunk text from `moa.knowledge_chunks`.
    pub text: String,
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
    /// Ordinal-adjacent sibling chunks that expand the matched chunk's context.
    ///
    /// Populated by parent-document retrieval with the matched chunk's neighbors
    /// (ordinal ±1, same document version) in ascending ordinal order. Excludes
    /// the matched chunk itself and is empty when no neighbors were hydrated.
    pub context_window: Vec<KnowledgeChunkWindowPart>,
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
    /// Raw vector cosine similarity when the vector leg surfaced this hit.
    ///
    /// Absolute (not rank-relative), so the injection evidence floor can
    /// separate genuinely similar hits from nearest-of-nothing noise.
    pub similarity: Option<f64>,
    /// Lexical backend selected for this hit when `legs.lexical` is true.
    pub lexical_backend: Option<LexicalBackend>,
    /// Source tier used for context assembly and query tracing.
    pub source_tier: SourceTier,
    /// Full tenant knowledge chunk payload when the hit is a knowledge chunk.
    pub knowledge_chunk: Option<KnowledgeChunkHydration>,
    /// Hydrated sidecar row.
    pub node: NodeIndexRow,
}
