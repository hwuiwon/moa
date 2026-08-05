//! LLM provider implementations for MOA.

mod adapters;
mod cancellation;
mod core;
mod failover;
mod governance;
mod model_selection;
mod provider_policy;
mod registry;
mod routing;

pub mod embedding;
pub mod rerank;

pub use adapters::anthropic::AnthropicProvider;
pub use adapters::anthropic::debug_build_anthropic_request_body;
pub use adapters::gemini::GeminiProvider;
pub use adapters::gemini::debug_build_gemini_request_body;
pub use adapters::openai_responses::OpenAIProvider;
pub use adapters::openai_responses::debug_build_openai_request_body;
#[cfg(any(test, feature = "scripted-provider"))]
pub use adapters::scripted::{
    ScriptedBlock, ScriptedFault, ScriptedProvider, ScriptedResponse, ScriptedTiming,
};
pub use cancellation::{CancellableLLMProvider, guarded_stream};
pub use core::factory::{
    build_provider_from_config, build_provider_from_model, build_provider_from_selection,
    resolve_provider_selection, resolve_rewriter_provider,
};
pub use core::models::{
    CATALOG, CapabilityTier, ProviderModel, by_provider, capabilities_for_provider_model,
    cheapest_chat_model, context_window, find, find_for_provider_model, find_model,
    pricing_for_model,
};
pub use core::pacer::PacerConfig;
pub use core::router::ModelRouter;
pub use core::schema::{compile_for_openai_strict, openai_strict_violations};
#[cfg(any(test, feature = "mock-embedding"))]
pub use embedding::MockEmbedding;
pub use embedding::{
    CohereEmbedding, CohereV4Embedder, EmbedderConstructionRole, GeminiEmbeddingEmbedder,
    OpenAIEmbedding, ZeroEntropyEmbedding, build_embedder_from_config,
    build_embedding_provider_from_config,
};
pub use failover::FailoverLLMProvider;
pub use governance::{CachingPiiClassifier, GovernedLLMProvider};
pub use provider_policy::{
    DeploymentProviderPolicy, ProviderCapabilities, ProviderPolicyExclusion,
};
pub use registry::{ProviderRegistry, ResolvedProvider};
pub use rerank::{
    COHERE_DEFAULT_RERANK_MODEL, CohereReranker, ConfiguredReranker, NOOP_RERANK_MODEL,
    NoopReranker, RerankHit, Reranker, ZEROENTROPY_DEFAULT_RERANK_MODEL, ZeroEntropyRerankLatency,
    ZeroEntropyReranker, build_reranker_from_config,
};
pub use routing::{
    DEFAULT_ANTHROPIC_MODEL, DEFAULT_GOOGLE_MODEL, DEFAULT_OPENAI_MODEL, PROVIDER_DESCRIPTORS,
    ProviderDescriptor, infer_provider_id, provider_descriptor, provider_descriptor_by_name,
};

/// Maximum retries owned by one configured LLM provider candidate.
pub const LLM_PROVIDER_MAX_RETRIES: usize = 3;

/// Maximum HTTP attempts made by one configured LLM provider candidate.
pub const LLM_PROVIDER_ATTEMPTS_PER_CANDIDATE: usize = LLM_PROVIDER_MAX_RETRIES + 1;

/// Returns the maximum provider HTTP attempts for one logical completion.
///
/// The enclosing Restate `ctx.run` is configured for one attempt. Consequently
/// provider failover is the sole retry owner and the total upper bound is the
/// number of configured candidates multiplied by four attempts per candidate.
#[must_use]
pub const fn llm_provider_attempt_upper_bound(candidate_count: usize) -> usize {
    candidate_count.saturating_mul(LLM_PROVIDER_ATTEMPTS_PER_CANDIDATE)
}

#[cfg(test)]
mod retry_budget_tests {
    use std::time::Duration;

    use moa_core::error::{FailureProvenance, MoaError};

    use super::*;

    #[test]
    fn failover_attempt_bound_is_candidates_times_four() {
        // Pins: Restate cannot multiply the provider-owned retry and failover budget.
        for candidates in 0..=8 {
            assert_eq!(
                llm_provider_attempt_upper_bound(candidates),
                candidates * 4,
                "candidate count {candidates} must have a fixed four-attempt budget"
            );
        }
    }

    #[test]
    fn provider_failure_families_keep_typed_retry_provenance() {
        // Pins: the Restate boundary never needs to infer provider retryability
        // from an error message.
        let cases = [
            (
                MoaError::ProviderTransport("connection reset".into()),
                FailureProvenance::Transient,
            ),
            (
                MoaError::ProviderTimeout("first byte deadline".into()),
                FailureProvenance::Transient,
            ),
            (
                MoaError::ProviderQuirk("invalid response shape".into()),
                FailureProvenance::Permanent,
            ),
            (
                MoaError::RateLimited {
                    retries: LLM_PROVIDER_MAX_RETRIES,
                    message: "limited".into(),
                },
                FailureProvenance::Transient,
            ),
            (
                MoaError::HttpStatus {
                    status: 422,
                    retry_after: None,
                    message: "bad request".into(),
                },
                FailureProvenance::Permanent,
            ),
            (
                MoaError::HttpStatus {
                    status: 503,
                    retry_after: Some(Duration::from_secs(1)),
                    message: "unavailable".into(),
                },
                FailureProvenance::Transient,
            ),
        ];

        for (error, expected) in cases {
            assert_eq!(
                error.failure_provenance(),
                expected,
                "unexpected provenance for {error:?}"
            );
        }
    }
}
