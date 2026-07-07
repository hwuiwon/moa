//! Graph-memory retrieval for context assembly and planning.

pub mod cache;
mod graph_seed;
pub mod hybrid;
pub mod legs;
pub mod policy;
pub mod ranking;
mod source_rank;
pub mod types;

pub use cache::{CacheKey, CachedEntry, CachedHybridRetriever, PlannedRetriever, RetrievalBackend};
pub use hybrid::HybridRetriever;
pub use legs::{GRAPH_WEIGHT, LEXICAL_WEIGHT, RRF_K, VECTOR_WEIGHT, rrf_fuse};
pub use policy::GraphRetrievalPolicy;
pub use ranking::{
    FeatureRanker, RANKING_PIPELINE_VERSION, RankingConfig, RankingWeights, normalize_tokens,
    ranking_fingerprint,
};
pub use types::{
    GraphCandidateCounts, GraphPathTrace, GraphRetrievalDiagnostics, GraphSeedDiagnostics,
    GraphSeedSource, KnowledgeChunkHydration, LegSources, LexicalBackend, LineageContext, Result,
    RetrievalError, RetrievalHit, RetrievalOutput, RetrievalRequest,
    SourceObjectFeatureContribution, SourceObjectFeatureContributions,
    SourceObjectRankingDiagnostics, SourceTier,
};
