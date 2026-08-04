//! In-process content-addressed embedding cache.
//!
//! An embedding is a pure function of the fully-configured provider and the input
//! text, so results can be cached by a content digest and reused across turns and
//! ingestion batches: only cache misses reach the wrapped provider. This keeps
//! re-ingestion of overlapping text and repeated-query embedding cost near zero
//! without changing similarity-search semantics, and without a TTL — an immutable
//! mapping needs only capacity-bounded LRU eviction.
//!
//! The cache is process-local, bounded, and **private to one provider instance**.
//! The digest folds in the model id, version, and output dimension, but NOT the
//! provider's retrieval role (query vs document input type), which the
//! [`EmbeddingProvider`] trait does not expose. A given instance always wraps a
//! single fully-configured provider (one model, one role, one dimension), so this
//! is correct; the cache must therefore never be shared across providers that
//! differ in role or dimension.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::error::{MoaError, Result};
use moa_core::traits::EmbeddingProvider;
use moka::future::Cache;

/// Wraps an [`EmbeddingProvider`] with a bounded content-addressed vector cache.
pub struct CachedEmbeddingProvider {
    inner: Arc<dyn EmbeddingProvider>,
    cache: Cache<[u8; 32], Arc<[f32]>>,
}

impl CachedEmbeddingProvider {
    /// Wraps `inner` with an LRU cache holding at most `capacity` vectors.
    ///
    /// `capacity` must be positive; callers disable caching by not wrapping the
    /// provider at all (see [`super::factory`]).
    #[must_use]
    pub fn new(inner: Arc<dyn EmbeddingProvider>, capacity: u64) -> Self {
        let cache = Cache::builder().max_capacity(capacity).build();
        Self { inner, cache }
    }

    /// Computes the content-addressed cache key for one input.
    ///
    /// The digest folds in the provider's model id, version, and output dimension
    /// so keys never collide across those axes and a model change invalidates prior
    /// entries automatically.
    fn digest(&self, text: &str) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.inner.model_id().as_bytes());
        hasher.update(&[0]);
        hasher.update(&self.inner.model_version().to_le_bytes());
        hasher.update(&self.inner.dimensions().to_le_bytes());
        hasher.update(&[0]);
        hasher.update(text.as_bytes());
        *hasher.finalize().as_bytes()
    }
}

#[async_trait]
impl EmbeddingProvider for CachedEmbeddingProvider {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn model_version(&self) -> i32 {
        self.inner.model_version()
    }

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        // Resolve each input against the cache; remember the positions that miss so
        // only those texts are sent to the provider.
        let mut results: Vec<Option<Vec<f32>>> = Vec::with_capacity(inputs.len());
        let mut miss_positions: Vec<usize> = Vec::new();
        let mut miss_texts: Vec<String> = Vec::new();
        for (position, text) in inputs.iter().enumerate() {
            match self.cache.get(&self.digest(text)).await {
                Some(vector) => results.push(Some(vector.to_vec())),
                None => {
                    results.push(None);
                    miss_positions.push(position);
                    miss_texts.push(text.clone());
                }
            }
        }

        if !miss_texts.is_empty() {
            let embedded = self.inner.embed(&miss_texts).await?;
            for (position, vector) in miss_positions.iter().zip(embedded) {
                self.cache
                    .insert(
                        self.digest(&inputs[*position]),
                        Arc::from(vector.as_slice()),
                    )
                    .await;
                results[*position] = Some(vector);
            }
        }

        // Every slot is filled unless the provider returned fewer vectors than it
        // was asked for; surface that as an error rather than panicking.
        results
            .into_iter()
            .map(|slot| slot.ok_or_else(cache_invariant_error))
            .collect()
    }
}

/// Builds the error returned when the provider under-delivers embeddings.
fn cache_invariant_error() -> MoaError {
    MoaError::ProviderError("embedding provider returned fewer vectors than inputs".to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use moa_core::error::Result;
    use moa_core::traits::EmbeddingProvider;

    use super::CachedEmbeddingProvider;

    /// Deterministic embedder that records how many texts it actually embedded.
    struct CountingEmbedder {
        embedded_inputs: AtomicUsize,
        calls: AtomicUsize,
    }

    impl CountingEmbedder {
        fn new() -> Self {
            Self {
                embedded_inputs: AtomicUsize::new(0),
                calls: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl EmbeddingProvider for CountingEmbedder {
        fn model_id(&self) -> &str {
            "counting-embedder"
        }

        fn dimensions(&self) -> usize {
            3
        }

        async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.embedded_inputs
                .fetch_add(inputs.len(), Ordering::SeqCst);
            // Vector derived from text length so equal texts map to equal vectors.
            Ok(inputs
                .iter()
                .map(|text| {
                    let len = text.len() as f32;
                    vec![len, len + 1.0, len + 2.0]
                })
                .collect())
        }
    }

    #[tokio::test]
    async fn only_misses_reach_the_inner_provider() {
        // Pins: a repeated text embeds once, then serves from cache — the core
        // cost-saving behavior. The second call must not reach the inner provider.
        let inner = Arc::new(CountingEmbedder::new());
        let cache = CachedEmbeddingProvider::new(inner.clone(), 128);

        let first = cache
            .embed(&[String::from("alpha")])
            .await
            .expect("first embed");
        let second = cache
            .embed(&[String::from("alpha")])
            .await
            .expect("second embed");

        assert_eq!(first, second);
        assert_eq!(
            inner.embedded_inputs.load(Ordering::SeqCst),
            1,
            "the second identical embed must be a cache hit"
        );
    }

    #[tokio::test]
    async fn batch_serves_cached_hits_and_preserves_order() {
        // Pins: a batch mixing a cache hit and misses reassembles in input order and
        // equals a direct embed, without re-embedding the cached text.
        let inner = Arc::new(CountingEmbedder::new());
        let cache = CachedEmbeddingProvider::new(inner.clone(), 128);

        // Warm the cache with "b" so the batch mixes a hit ("b") and misses.
        cache.embed(&[String::from("b")]).await.expect("warm");

        let batch = vec![String::from("aa"), String::from("b"), String::from("ccc")];
        let out = cache.embed(&batch).await.expect("batch embed");

        assert_eq!(out[0], vec![2.0, 3.0, 4.0]);
        assert_eq!(out[1], vec![1.0, 2.0, 3.0]);
        assert_eq!(out[2], vec![3.0, 4.0, 5.0]);

        // Warm ("b") = 1, plus batch misses "aa" and "ccc" = 2; "b" is not re-embedded.
        assert_eq!(inner.embedded_inputs.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn empty_input_makes_no_provider_call() {
        // Pins: an empty batch short-circuits without touching the provider.
        let inner = Arc::new(CountingEmbedder::new());
        let cache = CachedEmbeddingProvider::new(inner.clone(), 128);

        let out = cache.embed(&[]).await.expect("empty embed");

        assert!(out.is_empty());
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
    }
}
