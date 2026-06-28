//! LLM provider implementations for MOA.

mod adapters;
mod core;
mod registry;
mod routing;

pub mod embedding;
pub mod memory_llm;
pub mod rerank;

pub use adapters::anthropic::AnthropicProvider;
pub use adapters::anthropic::debug_build_anthropic_request_body;
pub use adapters::gemini::GeminiProvider;
pub use adapters::gemini::debug_build_gemini_request_body;
pub use adapters::openai_responses::OpenAIProvider;
pub use adapters::openai_responses::debug_build_openai_request_body;
#[cfg(any(test, feature = "scripted-provider"))]
pub use adapters::scripted::{ScriptedBlock, ScriptedProvider, ScriptedResponse};
pub use core::factory::{
    ProviderSelection, build_provider_from_config, build_provider_from_selection,
    resolve_provider_selection, resolve_rewriter_provider,
};
pub use core::models::{
    CATALOG, ProviderModel, by_provider, capabilities_for_provider_model, context_window, find,
    find_for_provider_model, find_model, pricing_for_model,
};
pub use core::router::ModelRouter;
#[cfg(any(test, feature = "mock-embedding"))]
pub use embedding::MockEmbedding;
pub use embedding::{
    CohereEmbedding, CohereV4Embedder, EmbedRole, EmbedderConstructionRole,
    GeminiEmbeddingEmbedder, OpenAIEmbedding, ZeroEntropyEmbedding, build_embedder_from_config,
    build_embedding_provider_from_config,
};
pub use memory_llm::{
    EXTRACTION_PROMPT_VERSION, LlmChatClient, LlmChatError, LlmEntityMergeClient, LlmExtractedFact,
    LlmFactExtractionChunk, LlmFactExtractionClient, MERGE_PROMPT_VERSION,
};
pub use registry::{ProviderRegistry, ResolvedProvider};
pub use rerank::{
    COHERE_DEFAULT_RERANK_MODEL, CohereReranker, ConfiguredReranker, NOOP_RERANK_MODEL,
    NoopReranker, RerankHit, Reranker, ZEROENTROPY_DEFAULT_RERANK_MODEL, ZeroEntropyRerankLatency,
    ZeroEntropyReranker, build_reranker_from_config,
};
pub use routing::{
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_OPENAI_MODEL, PROVIDER_DESCRIPTORS,
    ProviderDescriptor, ProviderId, infer_provider_id, provider_descriptor_by_name,
};
