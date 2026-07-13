//! Graph-memory retrieval for context assembly and planning.

pub mod admission;
pub mod cache;
pub(crate) mod enrichment;
mod graph_seed;
pub mod hybrid;
mod hydration;
pub mod legs;
pub mod policy;
pub mod ranking;
pub mod router;
mod source_rank;
pub mod types;

pub use admission::{MemoryAdmissionPolicy, RetrievalScopePlan, dedupe_and_rank_hits};
pub use cache::{CacheKey, CachedEntry, CachedHybridRetriever, PlannedRetriever, RetrievalBackend};
pub use hybrid::HybridRetriever;
pub use legs::{GRAPH_WEIGHT, LEXICAL_WEIGHT, RRF_K, VECTOR_WEIGHT, rrf_fuse};
pub use policy::GraphRetrievalPolicy;
pub use ranking::{
    FeatureRanker, RANKING_PIPELINE_VERSION, RankingConfig, RankingWeights, normalize_tokens,
    ranking_fingerprint,
};
pub use router::{RetrievalStrategy, decompose_query, route_query};
pub use types::{
    EvidenceWindowPolicy, GraphCandidateCounts, GraphPathTrace, GraphRetrievalDiagnostics,
    GraphSeedDiagnostics, GraphSeedSource, KnowledgeChunkHydration, KnowledgeChunkWindowPart,
    LegSources, LexicalBackend, LineageContext, RerankScore, Result, RetrievalError, RetrievalHit,
    RetrievalLineageHit, RetrievalOutput, RetrievalProvenance, RetrievalRequest,
    RetrievalStageTimings, SourceObjectFeatureContribution, SourceObjectFeatureContributions,
    SourceObjectRankingDiagnostics, SourceTier,
};
