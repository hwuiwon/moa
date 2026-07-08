//! Shared retrieval request, result, and diagnostics types.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use moa_core::SessionId;
use moa_lineage_core::TurnId;
use moa_memory_graph::{GraphError, NodeIndexRow, NodeLabel, PiiClass};
use moa_memory_types::MemoryScope;
use moa_memory_vector::Error as VectorError;
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
    /// Lexical backend selected for this hit when `legs.lexical` is true.
    pub lexical_backend: Option<LexicalBackend>,
    /// Source tier used for context assembly and query tracing.
    pub source_tier: SourceTier,
    /// Full tenant knowledge chunk payload when the hit is a knowledge chunk.
    pub knowledge_chunk: Option<KnowledgeChunkHydration>,
    /// Hydrated sidecar row.
    pub node: NodeIndexRow,
}
