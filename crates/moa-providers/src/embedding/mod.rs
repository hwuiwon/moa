//! Embedding providers used by graph memory retrieval.

use moa_core::MoaConfig;

mod cohere;
mod factory;
mod gemini;
#[cfg(any(test, feature = "mock-embedding"))]
mod mock;
mod openai;
mod zeroentropy;

use openai::OPENAI_DEFAULT_MODEL;

pub use cohere::{CohereEmbedding, CohereV4Embedder};
pub use factory::{build_embedder_from_config, build_embedding_provider_from_config};
pub use gemini::{EmbedRole, EmbedderConstructionRole, GeminiEmbeddingEmbedder};
#[cfg(any(test, feature = "mock-embedding"))]
pub use mock::MockEmbedding;
pub use openai::OpenAIEmbedding;
pub use zeroentropy::ZeroEntropyEmbedding;

fn model_from_config_with_provider_default(config: &MoaConfig, provider_default: &str) -> String {
    let model = config.memory.embedding_model.trim();
    if model.is_empty() || provider_default != OPENAI_DEFAULT_MODEL && model == OPENAI_DEFAULT_MODEL
    {
        provider_default.to_string()
    } else {
        model.to_string()
    }
}
