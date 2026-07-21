//! Embedding providers used by graph memory retrieval.

mod cache;
mod cohere;
mod factory;
mod gemini;
#[cfg(any(test, feature = "mock-embedding"))]
mod mock;
mod openai;
mod zeroentropy;

pub use cache::CachedEmbeddingProvider;
pub use cohere::{CohereEmbedding, CohereV4Embedder};
pub use factory::{build_embedder_from_config, build_embedding_provider_from_config};
pub use gemini::{EmbedderConstructionRole, GeminiEmbeddingEmbedder};
#[cfg(any(test, feature = "mock-embedding"))]
pub use mock::MockEmbedding;
pub use openai::OpenAIEmbedding;
pub use zeroentropy::ZeroEntropyEmbedding;
