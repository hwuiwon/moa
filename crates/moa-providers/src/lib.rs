//! LLM provider implementations for MOA.

mod adapters;
mod core;

pub mod embedding;

pub use adapters::anthropic::AnthropicProvider;
pub use adapters::anthropic::debug_build_anthropic_request_body;
pub use adapters::gemini::GeminiProvider;
pub use adapters::gemini::debug_build_gemini_request_body;
pub use adapters::openai_chat::OpenAIProvider;
pub use adapters::openai_chat::debug_build_openai_request_body;
#[cfg(any(test, feature = "test-util"))]
pub use adapters::scripted::{ScriptedBlock, ScriptedProvider, ScriptedResponse};
pub use core::factory::{
    ProviderSelection, build_provider_from_config, build_provider_from_selection,
    resolve_provider_selection, resolve_rewriter_provider,
};
pub use core::models::{CATALOG, ProviderModel, by_provider, context_window, find};
pub use core::router::ModelRouter;
pub use embedding::{MockEmbedding, OpenAIEmbedding, build_embedding_provider_from_config};
