//! Graph-memory retrieval for context assembly and planning.

pub mod cache;
pub mod hybrid;
pub mod legs;
pub mod ranking;

pub use cache::{
    CacheKey, CachedEntry, CachedHybridRetriever, CachedHybridRetrieverConfig, PlannedRetriever,
    RetrievalBackend,
};
pub use hybrid::{
    HybridRetriever, KnowledgeChunkHydration, LegSources, LexicalBackend, LineageContext, Result,
    RetrievalError, RetrievalHit, RetrievalRequest, SourceTier,
};
pub use legs::{GRAPH_WEIGHT, LEXICAL_WEIGHT, RRF_K, VECTOR_WEIGHT, rrf_fuse};
pub use ranking::{
    FeatureRanker, RANKING_PIPELINE_VERSION, RankingConfig, RankingWeights, normalize_tokens,
    ranking_fingerprint,
};
