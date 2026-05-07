//! LLM provider implementations for MOA.

mod adapters;
mod core;

pub mod anthropic {
    //! Backwards-compatible Anthropic adapter exports.

    pub use crate::adapters::anthropic::*;
}

pub mod embedding;

pub mod gemini {
    //! Backwards-compatible Gemini adapter exports.

    pub use crate::adapters::gemini::*;
}

pub mod models {
    //! Backwards-compatible provider model catalog exports.

    pub use crate::core::models::*;
}

pub mod openai {
    //! Backwards-compatible OpenAI adapter exports.

    pub use crate::adapters::openai_chat::*;
}

#[cfg(any(test, feature = "test-util"))]
pub mod scripted {
    //! Backwards-compatible scripted provider exports.

    pub use crate::adapters::scripted::*;
}

pub use adapters::anthropic::AnthropicProvider;
pub use adapters::gemini::GeminiProvider;
pub use adapters::openai_chat::OpenAIProvider;
#[cfg(any(test, feature = "test-util"))]
pub use adapters::scripted::{ScriptedBlock, ScriptedProvider, ScriptedResponse};
pub use core::factory::{
    ProviderSelection, build_provider_from_config, build_provider_from_selection,
    resolve_provider_selection, resolve_rewriter_provider,
};
pub use core::models::{CATALOG, ProviderModel, by_provider, context_window, find};
pub use core::router::ModelRouter;
pub use embedding::{
    EmbeddingProvider, MockEmbedding, OpenAIEmbedding, build_embedding_provider_from_config,
};
