//! Counting wrappers for provider traits used by evaluation lanes.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::traits::EmbeddingProvider;
use moa_memory_graph::NodeIndexRow;
use moa_memory_ingest::{EntityMergeVerifier, ExtractedFact, FactExtractor, Result, TurnChunk};
use moa_providers::{RerankHit, Reranker};
use tokio::sync::Mutex;

use super::cost::{CostLedger, estimate_tokens_from_chars};

/// Shared cost ledger handle used by counting wrappers.
pub type SharedCostLedger = Arc<Mutex<CostLedger>>;

/// Embedding provider wrapper that records estimated input tokens.
#[derive(Clone)]
pub struct CountingEmbedder<T> {
    inner: T,
    ledger: SharedCostLedger,
}

impl<T> CountingEmbedder<T> {
    /// Creates a counting wrapper around an embedding provider.
    #[must_use]
    pub fn new(inner: T, ledger: SharedCostLedger) -> Self {
        Self { inner, ledger }
    }
}

#[async_trait]
impl<T> EmbeddingProvider for CountingEmbedder<T>
where
    T: EmbeddingProvider,
{
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_version(&self) -> i32 {
        self.inner.model_version()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
        let embeddings = self.inner.embed(inputs).await?;
        let tokens = inputs
            .iter()
            .map(|input| estimate_tokens_from_chars(input))
            .sum::<u64>();
        self.ledger.lock().await.record_embed(tokens);
        Ok(embeddings)
    }
}

/// Fact extractor wrapper that records estimated chat input and output tokens.
#[derive(Clone)]
pub struct CountingExtractor<T> {
    inner: T,
    ledger: SharedCostLedger,
}

impl<T> CountingExtractor<T> {
    /// Creates a counting wrapper around a fact extractor.
    #[must_use]
    pub fn new(inner: T, ledger: SharedCostLedger) -> Self {
        Self { inner, ledger }
    }
}

#[async_trait]
impl<T> FactExtractor for CountingExtractor<T>
where
    T: FactExtractor,
{
    async fn extract(&self, chunks: &[TurnChunk]) -> Result<Vec<ExtractedFact>> {
        let facts = self.inner.extract(chunks).await?;
        let input_tokens = chunks
            .iter()
            .map(|chunk| estimate_tokens_from_chars(&chunk.text))
            .sum::<u64>();
        let output_tokens = facts
            .iter()
            .map(|fact| {
                estimate_tokens_from_chars(&fact.subject)
                    + estimate_tokens_from_chars(&fact.predicate)
                    + estimate_tokens_from_chars(&fact.object)
                    + estimate_tokens_from_chars(&fact.summary)
            })
            .sum::<u64>();
        self.ledger
            .lock()
            .await
            .record_chat(input_tokens, output_tokens);
        Ok(facts)
    }
}

/// Entity merge verifier wrapper that records estimated chat tokens.
#[derive(Clone)]
pub struct CountingMergeVerifier<T> {
    inner: T,
    ledger: SharedCostLedger,
}

impl<T> CountingMergeVerifier<T> {
    /// Creates a counting wrapper around an entity merge verifier.
    #[must_use]
    pub fn new(inner: T, ledger: SharedCostLedger) -> Self {
        Self { inner, ledger }
    }
}

#[async_trait]
impl<T> EntityMergeVerifier for CountingMergeVerifier<T>
where
    T: EntityMergeVerifier,
{
    async fn should_merge(&self, mention: &str, candidate: &NodeIndexRow) -> Result<bool> {
        let verdict = self.inner.should_merge(mention, candidate).await?;
        let input_tokens =
            estimate_tokens_from_chars(mention) + estimate_tokens_from_chars(&candidate.name);
        self.ledger.lock().await.record_chat(input_tokens, 1);
        Ok(verdict)
    }
}

/// Reranker wrapper that records one search per delegated rerank call.
#[derive(Clone)]
pub struct CountingReranker<T> {
    inner: T,
    ledger: SharedCostLedger,
}

impl<T> CountingReranker<T> {
    /// Creates a counting wrapper around a reranker.
    #[must_use]
    pub fn new(inner: T, ledger: SharedCostLedger) -> Self {
        Self { inner, ledger }
    }
}

#[async_trait]
impl<T> Reranker for CountingReranker<T>
where
    T: Reranker,
{
    async fn rerank(
        &self,
        model: &str,
        query: &str,
        documents: &[String],
        top_n: usize,
    ) -> moa_core::Result<Vec<RerankHit>> {
        let hits = self.inner.rerank(model, query, documents, top_n).await?;
        self.ledger.lock().await.record_rerank(1);
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use moa_core::MoaError;
    use moa_memory_ingest::{ScriptedFactExtractor, TurnChunk};
    use moa_providers::NoopReranker;

    use super::*;

    #[tokio::test]
    async fn counting_embedder_delegates_and_records_tokens() {
        // Pins: counting wrappers charge the live lane while preserving the wrapped provider output.
        let provider = StaticEmbedder {
            expected_input: "hello world".to_string(),
            vector: vec![0.25, 0.75],
        };
        let ledger = Arc::new(Mutex::new(CostLedger::new(1.0)));
        let counting = CountingEmbedder::new(provider, ledger.clone());

        let embeddings = counting
            .embed(&["hello world".to_string()])
            .await
            .expect("embedding should delegate to static provider");

        assert_eq!(embeddings, vec![vec![0.25, 0.75]]);
        assert_eq!(ledger.lock().await.embed_input_tokens, 3);
    }

    #[tokio::test]
    async fn counting_extractor_records_estimated_chat_tokens() {
        // Pins: extractor counting records input and structured fact output text.
        let extractor = ScriptedFactExtractor::from_summaries(["The user prefers Rust."])
            .expect("scripted fixture should parse");
        let ledger = Arc::new(Mutex::new(CostLedger::new(1.0)));
        let counting = CountingExtractor::new(extractor, ledger.clone());

        let facts = counting
            .extract(&[TurnChunk {
                index: 0,
                text: "user: I prefer Rust.".to_string(),
                token_estimate: 5,
            }])
            .await
            .expect("scripted extractor should delegate");

        assert_eq!(facts.len(), 1);
        let ledger = ledger.lock().await;
        assert_eq!(ledger.chat_input_tokens, 5);
        assert!(ledger.chat_output_tokens > 0);
    }

    #[tokio::test]
    async fn counting_reranker_delegates_and_records_searches() {
        // Pins: rerank A/B reports count actual reranker calls.
        let ledger = Arc::new(Mutex::new(CostLedger::new(1.0)));
        let reranker = CountingReranker::new(NoopReranker, ledger.clone());

        let hits = reranker
            .rerank(
                "rerank-v4.0-fast",
                "query",
                &["a".to_string(), "b".to_string()],
                1,
            )
            .await
            .expect("noop reranker should delegate");

        assert_eq!(hits.len(), 1);
        assert_eq!(ledger.lock().await.rerank_calls, 1);
    }

    #[derive(Clone)]
    struct StaticEmbedder {
        expected_input: String,
        vector: Vec<f32>,
    }

    #[async_trait]
    impl EmbeddingProvider for StaticEmbedder {
        fn model_id(&self) -> &str {
            "static-test-embedder"
        }

        fn dimensions(&self) -> usize {
            self.vector.len()
        }

        async fn embed(&self, inputs: &[String]) -> moa_core::Result<Vec<Vec<f32>>> {
            if inputs.len() != 1
                || inputs.first().map(String::as_str) != Some(self.expected_input.as_str())
            {
                return Err(MoaError::ProviderError(format!(
                    "unexpected inputs: {inputs:?}"
                )));
            }
            Ok(vec![self.vector.clone()])
        }
    }
}
