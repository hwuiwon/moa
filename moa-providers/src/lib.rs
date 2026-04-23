//! LLM provider implementations for MOA.

pub mod anthropic;
pub mod embedding;
mod factory;
pub mod gemini;
mod http;
mod instrumentation;
pub mod models;
pub mod openai;
mod openai_responses;
mod provider_tools;
mod retry;
mod router;
mod schema;
#[cfg(any(test, feature = "test-util"))]
pub mod scripted;
mod sse;

pub use anthropic::AnthropicProvider;
pub use embedding::{
    EmbeddingProvider, MockEmbedding, OpenAIEmbedding, build_embedding_provider_from_config,
};
pub use factory::{
    ProviderSelection, build_provider_from_config, build_provider_from_selection,
    resolve_provider_selection, resolve_rewriter_provider,
};
pub use gemini::GeminiProvider;
pub use models::{CATALOG, ProviderModel, by_provider, context_window, find};
pub use openai::OpenAIProvider;
pub use router::ModelRouter;
#[cfg(any(test, feature = "test-util"))]
pub use scripted::{ScriptedBlock, ScriptedProvider, ScriptedResponse};
