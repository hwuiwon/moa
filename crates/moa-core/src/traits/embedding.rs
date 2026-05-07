//! Shared text embedding provider abstraction.

use async_trait::async_trait;

use crate::error::Result;

/// Shared abstraction over text embedding backends.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Returns the configured embedding model identifier.
    fn model_id(&self) -> &str;

    /// Returns the fixed dimensionality produced by this embedding model.
    fn dimensions(&self) -> usize;

    /// Returns the model-version integer stored beside embeddings.
    fn model_version(&self) -> i32 {
        1
    }

    /// Returns the model name stored beside embeddings.
    fn model_name(&self) -> &str {
        self.model_id()
    }

    /// Returns the fixed output dimensionality.
    fn dimension(&self) -> usize {
        self.dimensions()
    }

    /// Computes embeddings for one or more UTF-8 inputs.
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>>;
}
