//! Memory subsystem and embedding configuration.

use serde::{Deserialize, Serialize};

/// Memory bootstrap and maintenance configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Optional HTTP base URL for the PII classification sidecar.
    pub pii_service_url: Option<String>,
    /// Embedding model selector used for graph memory embedding backfills and queries.
    pub embedding_model: String,
    /// Graph-memory retrieval behavior.
    pub retrieval: MemoryRetrievalConfig,
    /// Fact extraction behavior for slow-path graph ingestion.
    pub extraction: MemoryExtractionConfig,
    /// Graph-memory vector embedding configuration.
    pub vector: MemoryVectorConfig,
    /// Standing memory digest rebuild and injection configuration.
    pub digest: MemoryDigestConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            pii_service_url: None,
            embedding_model: "openai:text-embedding-3-small".to_string(),
            retrieval: MemoryRetrievalConfig::default(),
            extraction: MemoryExtractionConfig::default(),
            vector: MemoryVectorConfig::default(),
            digest: MemoryDigestConfig::default(),
        }
    }
}

/// Standing contact/tenant digest configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryDigestConfig {
    /// Whether the brain context pipeline injects stored digest rows.
    pub enabled: bool,
    /// Maximum rendered digest size using the rough chars/4 token estimate.
    pub max_tokens: usize,
    /// Minimum interval between digest row rebuilds during consolidation.
    pub rebuild_min_interval_hours: i64,
}

impl Default for MemoryDigestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_tokens: 600,
            rebuild_min_interval_hours: 6,
        }
    }
}

/// Slow-path fact extraction configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryExtractionConfig {
    /// Whether model-backed fact extraction is enabled.
    pub enabled: bool,
    /// Provider model selector used for extraction and memory-ingest judging.
    pub model: String,
    /// Maximum facts accepted from one chunk.
    pub max_facts_per_chunk: usize,
    /// Provider request timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for MemoryExtractionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "gpt-5.4-mini".to_string(),
            max_facts_per_chunk: 12,
            timeout_ms: 10_000,
        }
    }
}

/// Graph-memory retrieval configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRetrievalConfig {
    /// Reranker model selector. `noop` preserves the fused order.
    pub reranker_model: String,
    /// Optional provider-specific reranker latency mode.
    pub reranker_latency: Option<String>,
    /// Whether retrieval writes narrow quality-scoring lineage rows.
    pub lineage_enabled: bool,
    /// Fraction of turns that write lineage rows when lineage is enabled.
    ///
    /// Sampling is deterministic per `(session, turn)`, so a given turn either
    /// always writes lineage or never does at a fixed rate. Values are clamped
    /// to `[0.0, 1.0]` at the write site; `1.0` records every turn.
    pub lineage_sample_rate: f64,
    /// Deterministic post-hydration ranking behavior.
    pub ranking: MemoryRankingConfig,
}

impl Default for MemoryRetrievalConfig {
    fn default() -> Self {
        Self {
            // 2026-07-11 bake-off: Cohere rerank-v4.0-fast tied zerank-2 on
            // quality (synthetic 500q MRR 0.876 vs 0.878, nDCG@10 0.889 vs
            // 0.891) and both beat noop decisively on every lane. Cohere is
            // the operator-selected default; zerank-2 measured lower
            // in-harness latency (486 ms vs 927 ms p95) and remains available
            // via `zeroentropy:zerank-2`. Falls back to noop with a warning
            // when MOA_COHERE_API_KEY is absent.
            reranker_model: "cohere:rerank-v4.0-fast".to_string(),
            reranker_latency: None,
            // Lineage rows are the dashboard's provenance source of truth, so
            // retrieval records them unless a deployment opts out.
            lineage_enabled: true,
            lineage_sample_rate: 1.0,
            ranking: MemoryRankingConfig::default(),
        }
    }
}

/// Deterministic ranking configuration for graph-memory retrieval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRankingConfig {
    /// Feature weights used by deterministic post-hydration ranking.
    pub weights: MemoryRankingWeights,
    /// Per-hop decay applied inside the scored graph walk.
    pub graph_walk_decay: f64,
    /// In-walk prune threshold: branches scoring below this stop expanding.
    pub graph_walk_prune_below: f64,
    /// Minimum summed activation an anchored-rescue graph candidate needs.
    pub graph_rescue_evidence_floor: f64,
    /// Minimum absolute lexical evidence a non-graph hit needs to enter the
    /// final injected window. `0.0` disables the floor.
    ///
    /// Fused scores are rank-relative, so nearest-neighbor legs fill the
    /// window even when nothing relevant exists; this floor is what lets a
    /// query with no supporting memory return nothing instead of noise.
    pub min_hit_evidence: f64,
    /// Whole-window abstain threshold on the best evidence in the final
    /// window: when no hit reaches this evidence (and none is graph-admitted),
    /// retrieval returns nothing instead of nearest-of-nothing noise. `0.0`
    /// disables abstention.
    ///
    /// This is the SOURCE the memory-retrieval stage reads to populate its
    /// per-request `EvidenceWindowPolicy`; it is not imposed by the shared
    /// retriever, so knowledge-lane retrievals with their own window policy are
    /// never clamped by it.
    ///
    /// Evidence is `max(lexical evidence, vector cosine)` per hit, so the
    /// useful threshold depends on the configured embedder's cosine floor for
    /// unrelated text; recalibrate when the embedder changes.
    pub abstain_below_window_evidence: f64,
    /// Final window size when a real reranker is active. `0` keeps the
    /// caller's `k_final`.
    ///
    /// This is the SOURCE the memory-retrieval stage reads to populate its
    /// per-request `EvidenceWindowPolicy`; it is not imposed by the shared
    /// retriever, so knowledge-lane retrievals sizing their own top-k window
    /// are never clamped by it.
    ///
    /// A reranker that reliably ranks gold at the top makes the tail slots
    /// pure noise: the 2026-07-11 live curve showed zerank-2 at window 3
    /// strictly dominates the unreranked window 4 on both recall and
    /// precision. Only applied on the reranked path — without a reranker the
    /// tail slots still carry recall.
    pub rerank_window: usize,
}

impl Default for MemoryRankingConfig {
    fn default() -> Self {
        Self {
            weights: MemoryRankingWeights::default(),
            graph_walk_decay: 0.5,
            graph_walk_prune_below: 0.05,
            graph_rescue_evidence_floor: 0.10,
            min_hit_evidence: 0.0,
            abstain_below_window_evidence: 0.68,
            rerank_window: 3,
        }
    }
}

/// Weights used by deterministic graph-memory ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryRankingWeights {
    /// Normalized reciprocal-rank fusion contribution.
    pub rrf: f64,
    /// Valid-from recency contribution.
    pub recency: f64,
    /// Last-access recency contribution.
    pub access: f64,
    /// Exact subject-token match contribution.
    pub subject_match: f64,
    /// Query-to-summary token overlap contribution.
    pub overlap: f64,
    /// Rescue bonus for candidates only graph expansion found.
    pub graph_rescue: f64,
    /// Bounded additive boost for candidates whose label matches a
    /// planner-inferred label hint.
    ///
    /// Soft signal only: a hinted label lifts a candidate over otherwise-equal
    /// noise during ranking, but never excludes non-matching candidates. This
    /// is deliberately separate from a scope-plan-supplied `label_filter`, which
    /// stays a hard retrieval filter.
    pub label_hint_boost: f64,
    /// Outcome-derived quality prior contribution.
    pub quality: f64,
    /// Additive score for contact-scoped rows.
    pub scope_user: f64,
    /// Additive score for tenant-scoped rows.
    pub scope_tenant: f64,
    /// Half-life in days for valid-from recency.
    pub recency_half_life_days: f64,
    /// Half-life in days for access recency.
    pub access_half_life_days: f64,
}

impl Default for MemoryRankingWeights {
    fn default() -> Self {
        Self {
            rrf: 1.0,
            recency: 0.3,
            access: 0.15,
            subject_match: 0.5,
            overlap: 0.35,
            graph_rescue: 0.6,
            label_hint_boost: 0.15,
            quality: 0.6,
            scope_user: 0.2,
            scope_tenant: 0.1,
            recency_half_life_days: 90.0,
            access_half_life_days: 14.0,
        }
    }
}

/// Graph-memory vector configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryVectorConfig {
    /// Embedder selection and credentials.
    pub embedder: VectorEmbedderConfig,
    /// Optional Turbopuffer backend configuration.
    pub turbopuffer: TurbopufferVectorConfig,
    /// Matryoshka (MRL) truncated-dim shortlist width for the pgvector KNN cascade.
    ///
    /// `None` (the default) keeps the single-stage full-dim search. `Some(d)`
    /// enables a two-stage cascade: a cheap shortlist ordered by the truncated
    /// `d`-dim prefix, then an exact full-dim rescore. `d` must be less than
    /// [`crate`]'s 1024-dim embedding width.
    ///
    /// Only enable this when the tenant's embedder is MRL-trained (e.g.
    /// `gemini:gemini-embedding-2`) so that a truncated prefix preserves
    /// semantic ordering -- that is the operator's responsibility; MOA does not
    /// verify it. The functional shortlist index in the base migration is built
    /// on a 512-dim prefix, so the shortlist stage is index-accelerated only when
    /// this is set to `512`; any other value forces a sequential shortlist scan
    /// unless the migration's index expression is updated to match.
    ///
    /// Default-on (`Some(512)`) was measured and DISCARDED on 2026-07-12: the
    /// hermetic lane's pseudo-embeddings are not MRL-ordered, so the shortlist
    /// discarded true neighbors (recall@4 0.976 -> 0.738, McNemar p ~= 0).
    /// Enabling by default needs a live-lane paired sweep with the real
    /// MRL-trained embedder plus a lane-aware exemption; until then: opt-in.
    pub mrl_shortlist_dims: Option<usize>,
}

/// Turbopuffer graph-memory vector backend configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurbopufferVectorConfig {
    /// Turbopuffer API key value loaded from runtime configuration.
    pub api_key: String,
    /// Optional Turbopuffer API base URL override.
    pub base_url: Option<String>,
    /// Optional namespace environment segment.
    pub environment: Option<String>,
    /// Whether the configured Turbopuffer account has a BAA for restricted data.
    pub baa_enabled: bool,
    /// Vector element type used for the Turbopuffer projection namespace.
    pub vector_type: TurbopufferVectorType,
}

/// Turbopuffer vector element type for read-side graph-memory projections.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurbopufferVectorType {
    /// Store vectors as 32-bit floats.
    F32,
    /// Store vectors as 16-bit floats.
    #[default]
    F16,
}

impl TurbopufferVectorType {
    /// Returns the stable config and namespace label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F16 => "f16",
        }
    }

    /// Returns the Turbopuffer schema type for a vector of this element type.
    #[must_use]
    pub fn schema_type(self, dimensions: usize) -> String {
        format!("[{}]{}", dimensions, self.as_str())
    }
}

/// Per-tenant embedder selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VectorEmbedderConfig {
    /// Embedder model name.
    pub name: String,
    /// Requested output dimensionality.
    pub output_dim: usize,
}

impl Default for VectorEmbedderConfig {
    fn default() -> Self {
        Self {
            name: "gemini:gemini-embedding-2".to_string(),
            output_dim: 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryVectorConfig;

    #[test]
    fn memory_vector_config_defaults_disable_mrl_shortlist() {
        // Pins: the Matryoshka cascade is opt-in. Default config keeps the
        // single-stage full-dim pgvector path with no truncated-prefix shortlist.
        assert_eq!(MemoryVectorConfig::default().mrl_shortlist_dims, None);
    }
}
