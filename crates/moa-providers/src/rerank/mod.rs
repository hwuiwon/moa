//! Reranker providers used after graph-memory candidate fusion.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::Result;

mod cohere;
mod factory;
mod zeroentropy;

pub use cohere::{COHERE_DEFAULT_RERANK_MODEL, CohereReranker};
pub use factory::{ConfiguredReranker, build_reranker_from_config};
pub use zeroentropy::{
    ZEROENTROPY_DEFAULT_RERANK_MODEL, ZeroEntropyRerankLatency, ZeroEntropyReranker,
};

/// Model id used by deterministic no-op reranking.
pub const NOOP_RERANK_MODEL: &str = "noop";

/// One rerank result with an index into the input document list.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RerankHit {
    /// Candidate index in the supplied document list.
    pub index: usize,
    /// Backend relevance score.
    pub relevance_score: f32,
}

/// Backend abstraction for candidate reranking.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Reranks document snippets for a query and returns selected indices.
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>>;
}

/// Deterministic reranker that preserves the incoming order.
#[derive(Debug, Clone, Default)]
pub struct NoopReranker;

impl NoopReranker {
    /// Returns the no-op reranker behind the shared trait object.
    #[must_use]
    pub fn shared() -> Arc<dyn Reranker> {
        Arc::new(Self)
    }
}

#[async_trait]
impl Reranker for NoopReranker {
    async fn rerank(
        &self,
        _model: &str,
        _query: &str,
        documents: &[String],
        top_n: usize,
    ) -> Result<Vec<RerankHit>> {
        Ok((0..documents.len().min(top_n))
            .map(|index| RerankHit {
                index,
                relevance_score: 1.0,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{NoopReranker, Reranker};

    #[tokio::test]
    async fn noop_reranker_preserves_order_and_limit() {
        // Pins: disabled reranking is deterministic and keeps the fused candidate order.
        let docs = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        let hits = NoopReranker
            .rerank("unused", "query", &docs, 2)
            .await
            .expect("noop rerank should succeed");

        assert_eq!(hits.iter().map(|hit| hit.index).collect::<Vec<_>>(), [0, 1]);
    }
}
