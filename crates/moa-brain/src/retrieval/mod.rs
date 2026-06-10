//! Graph-memory retrieval for context assembly and planning.

pub mod cache;
pub mod hybrid;
pub mod legs;
pub mod ranking;
pub mod reranker;

pub use cache::{
    CacheKey, CachedEntry, CachedHybridRetriever, CachedHybridRetrieverConfig, RetrievalBackend,
};
pub use hybrid::{
    HybridRetriever, LegSources, Result, RetrievalError, RetrievalHit, RetrievalRequest,
};
pub use legs::{GRAPH_WEIGHT, LEXICAL_WEIGHT, RRF_K, VECTOR_WEIGHT, rrf_fuse};
pub use ranking::{
    FeatureRanker, RANKING_PIPELINE_VERSION, RankingConfig, RankingMode, RankingWeights,
    normalize_tokens, ranking_fingerprint,
};
pub use reranker::{CohereReranker, NoopReranker, RerankHit, Reranker};
