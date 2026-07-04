//! Embedding provider selection and construction from runtime config.
//!
//! # No cross-provider failover for embeddings — by design
//!
//! Unlike the LLM chat path (see [`crate::FailoverLLMProvider`]), embeddings must
//! NOT fail over to a different provider or model on rate limiting. A stored
//! vector index is populated by exactly one embedding model; vectors from a
//! different model live in a different, incompatible geometric space, so serving
//! a query embedding from a fallback model would silently corrupt similarity
//! search against the persisted index. Embedding providers therefore rely only on
//! per-minute pacing ([`PacerConfig`](crate::PacerConfig)), bounded concurrency,
//! and in-call retry/backoff to ride out rate limits — never substitution. This
//! is a compile-level guarantee: no embedding builder here accepts or wires a
//! fallback chain.

use std::sync::Arc;

use moa_core::traits::EmbeddingProvider;
use moa_core::{MoaConfig, MoaError, Result};

use super::cohere::COHERE_DEFAULT_MODEL;
use super::gemini::GEMINI_V2_MODEL;
#[cfg(test)]
use super::zeroentropy::ZEROENTROPY_DEFAULT_MODEL;
use super::{
    CohereEmbedding, EmbedderConstructionRole, GeminiEmbeddingEmbedder, OpenAIEmbedding,
    ZeroEntropyEmbedding,
};
use crate::core::pacer::PacerConfig;
use crate::model_selection::{normalize_provider_name, split_explicit_provider_model};

const OPENAI_PROVIDER_NAME: &str = "openai";
const COHERE_PROVIDER_NAME: &str = "cohere";
const GEMINI_PROVIDER_NAME: &str = "gemini";
const ZEROENTROPY_PROVIDER_NAME: &str = "zeroentropy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingProviderKind {
    OpenAi,
    Cohere,
    Gemini,
    ZeroEntropy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedEmbeddingProvider {
    provider: EmbeddingProviderKind,
    model: String,
}

impl EmbeddingProviderKind {
    fn from_provider_name(name: &str, field_name: &str) -> Result<Option<Self>> {
        match normalize_provider_name(name).as_str() {
            "disabled" => Ok(None),
            OPENAI_PROVIDER_NAME => Ok(Some(Self::OpenAi)),
            COHERE_PROVIDER_NAME => Ok(Some(Self::Cohere)),
            GEMINI_PROVIDER_NAME => Ok(Some(Self::Gemini)),
            ZEROENTROPY_PROVIDER_NAME => Ok(Some(Self::ZeroEntropy)),
            unsupported => Err(MoaError::ConfigError(format!(
                "unsupported {field_name} provider '{unsupported}'"
            ))),
        }
    }

    fn provider_name(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_PROVIDER_NAME,
            Self::Cohere => COHERE_PROVIDER_NAME,
            Self::Gemini => GEMINI_PROVIDER_NAME,
            Self::ZeroEntropy => ZEROENTROPY_PROVIDER_NAME,
        }
    }

    fn build_semantic(
        self,
        config: &MoaConfig,
        model: String,
    ) -> Result<Arc<dyn EmbeddingProvider>> {
        match self {
            Self::OpenAi => {
                let api_key = read_api_key("MOA_OPENAI_API_KEY", &config.providers.openai.api_key)?;
                let mut provider = OpenAIEmbedding::new(api_key, model)?;
                if let Some(pacer) =
                    embed_pacer_override(config.providers.openai.max_inputs_per_min)
                {
                    provider = provider.with_rate_limits(pacer);
                }
                if let Some(max) =
                    concurrency_override(config.providers.openai.max_concurrent_requests)
                {
                    provider = provider.with_max_concurrent_requests(max);
                }
                Ok(Arc::new(provider))
            }
            Self::Cohere => {
                let mut provider = CohereEmbedding::from_config_with_model(config, model)?;
                if let Some(pacer) =
                    embed_pacer_override(config.providers.cohere.max_inputs_per_min)
                {
                    provider = provider.with_rate_limits(pacer);
                }
                if let Some(max) =
                    concurrency_override(config.providers.cohere.max_concurrent_requests)
                {
                    provider = provider.with_max_concurrent_requests(max);
                }
                Ok(Arc::new(provider))
            }
            Self::Gemini => {
                if model != GEMINI_V2_MODEL {
                    return Err(MoaError::ConfigError(format!(
                        "gemini embedding provider only supports {GEMINI_V2_MODEL}, got {model}"
                    )));
                }
                Ok(Arc::new(build_gemini_embedder(
                    config,
                    EmbedderConstructionRole::Retrieval,
                )?))
            }
            Self::ZeroEntropy => {
                let mut provider = ZeroEntropyEmbedding::from_config_with_model(config, model)?;
                if let Some(pacer) =
                    embed_pacer_override(config.providers.zeroentropy.max_inputs_per_min)
                {
                    provider = provider.with_rate_limits(pacer);
                }
                if let Some(max) =
                    concurrency_override(config.providers.zeroentropy.max_concurrent_requests)
                {
                    provider = provider.with_max_concurrent_requests(max);
                }
                Ok(Arc::new(provider))
            }
        }
    }

    fn build_vector(
        self,
        config: &MoaConfig,
        model: String,
        role: EmbedderConstructionRole,
    ) -> Result<Arc<dyn EmbeddingProvider>> {
        let cfg = &config.memory.vector.embedder;
        match self {
            Self::OpenAi => {
                let api_key = read_api_key("MOA_OPENAI_API_KEY", &config.providers.openai.api_key)?;
                let mut provider = OpenAIEmbedding::new(api_key, model)?;
                if provider.dimensions() != cfg.output_dim {
                    return Err(MoaError::ConfigError(format!(
                        "openai embedding output dimension is {}; configure memory.vector.embedder.output_dim to match",
                        provider.dimensions()
                    )));
                }
                if let Some(pacer) =
                    embed_pacer_override(config.providers.openai.max_inputs_per_min)
                {
                    provider = provider.with_rate_limits(pacer);
                }
                if let Some(max) =
                    concurrency_override(config.providers.openai.max_concurrent_requests)
                {
                    provider = provider.with_max_concurrent_requests(max);
                }
                Ok(Arc::new(provider))
            }
            Self::Cohere => {
                let api_key = read_api_key("MOA_COHERE_API_KEY", &config.providers.cohere.api_key)?;
                let mut provider =
                    CohereEmbedding::new(api_key, model)?.with_dimensions(cfg.output_dim)?;
                if let Some(pacer) =
                    embed_pacer_override(config.providers.cohere.max_inputs_per_min)
                {
                    provider = provider.with_rate_limits(pacer);
                }
                if let Some(max) =
                    concurrency_override(config.providers.cohere.max_concurrent_requests)
                {
                    provider = provider.with_max_concurrent_requests(max);
                }
                Ok(Arc::new(provider))
            }
            Self::Gemini => {
                if model != GEMINI_V2_MODEL {
                    return Err(MoaError::ConfigError(format!(
                        "gemini vector embedder only supports {GEMINI_V2_MODEL}, got {model}"
                    )));
                }
                Ok(Arc::new(build_gemini_embedder(config, role)?))
            }
            Self::ZeroEntropy => {
                let api_key = read_api_key(
                    "MOA_ZEROENTROPY_API_KEY",
                    &config.providers.zeroentropy.api_key,
                )?;
                let mut provider =
                    ZeroEntropyEmbedding::new(api_key, model)?.with_dimensions(cfg.output_dim)?;
                if let Some(pacer) =
                    embed_pacer_override(config.providers.zeroentropy.max_inputs_per_min)
                {
                    provider = provider.with_rate_limits(pacer);
                }
                if let Some(max) =
                    concurrency_override(config.providers.zeroentropy.max_concurrent_requests)
                {
                    provider = provider.with_max_concurrent_requests(max);
                }
                Ok(Arc::new(provider))
            }
        }
    }
}

/// Maps a configured `max_inputs_per_min` override to an embed pacer config.
///
/// Embeddings are limited by inputs/min, so an override replaces the provider's
/// default input-rate pacing; `None` leaves the provider default in place.
fn embed_pacer_override(max_inputs_per_min: Option<u32>) -> Option<PacerConfig> {
    max_inputs_per_min.map(PacerConfig::inputs_per_min)
}

/// Maps a configured `max_concurrent_requests` override to an in-flight ceiling.
///
/// `None` leaves the provider's default embedding concurrency window in place; a
/// configured value overrides it (0 is treated as unbounded by the limiter).
fn concurrency_override(max_concurrent_requests: Option<u32>) -> Option<usize> {
    max_concurrent_requests.map(|max| max as usize)
}

/// Builds a vector-space embedder from the tenant memory embedder configuration.
pub fn build_embedder_from_config(
    config: &MoaConfig,
    role: EmbedderConstructionRole,
) -> Result<Arc<dyn EmbeddingProvider>> {
    let cfg = &config.memory.vector.embedder;
    let resolved = resolve_embedding_model(&cfg.name, "memory.vector.embedder.name")?
        .ok_or_else(|| MoaError::ConfigError("memory vector embedder is disabled".to_string()))?;
    resolved.provider.build_vector(config, resolved.model, role)
}

fn build_gemini_embedder(
    config: &MoaConfig,
    role: EmbedderConstructionRole,
) -> Result<GeminiEmbeddingEmbedder> {
    let cfg = &config.memory.vector.embedder;
    let api_key = read_api_key("MOA_GOOGLE_API_KEY", &config.providers.google.api_key)?;
    let mut embedder = GeminiEmbeddingEmbedder::new(api_key, cfg.output_dim, role)?;
    if let Some(pacer) = embed_pacer_override(config.providers.google.max_inputs_per_min) {
        embedder = embedder.with_rate_limits(pacer);
    }
    if let Some(max) = concurrency_override(config.providers.google.max_concurrent_requests) {
        embedder = embedder.with_max_concurrent_requests(max);
    }
    Ok(embedder)
}

fn read_api_key(env_name: &'static str, value: &str) -> Result<String> {
    moa_core::config::required_config_secret(env_name, value)
}

/// Builds the configured embedding provider for semantic memory search.
pub fn build_embedding_provider_from_config(
    config: &MoaConfig,
) -> Result<Option<Arc<dyn EmbeddingProvider>>> {
    let Some(resolved) =
        resolve_embedding_model(&config.memory.embedding_model, "memory.embedding_model")?
    else {
        return Ok(None);
    };

    match resolved.provider.build_semantic(config, resolved.model) {
        Ok(provider) => Ok(Some(provider)),
        Err(MoaError::MissingEnvironmentVariable(env_name)) => {
            tracing::warn!(
                env = %env_name,
                provider = resolved.provider.provider_name(),
                "semantic memory search disabled because the embedding API key is missing"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn resolve_embedding_model(
    model_name: &str,
    field_name: &str,
) -> Result<Option<ResolvedEmbeddingProvider>> {
    let normalized = normalize_provider_name(model_name);
    if normalized.is_empty() || normalized == "disabled" {
        return Ok(None);
    }
    if let Some(explicit) = split_explicit_provider_model(model_name, field_name)? {
        let Some(provider) =
            EmbeddingProviderKind::from_provider_name(explicit.provider, field_name)?
        else {
            return Ok(None);
        };
        return Ok(Some(ResolvedEmbeddingProvider {
            provider,
            model: explicit.model.to_string(),
        }));
    }

    Err(MoaError::ConfigError(format!(
        "{field_name} must use provider:model, such as cohere:{COHERE_DEFAULT_MODEL}"
    )))
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use super::{
        COHERE_DEFAULT_MODEL, EmbeddingProviderKind, GEMINI_V2_MODEL, ZEROENTROPY_DEFAULT_MODEL,
        build_embedder_from_config, build_embedding_provider_from_config, resolve_embedding_model,
    };
    use crate::model_selection::normalize_provider_name;
    #[test]
    fn embed_pacer_override_maps_configured_inputs_limit() {
        // Pins: a configured max_inputs_per_min becomes an inputs/min pacer
        // override; an unset value leaves the provider default in place.
        assert_eq!(super::embed_pacer_override(None), None);
        assert_eq!(
            super::embed_pacer_override(Some(500)),
            Some(crate::core::pacer::PacerConfig::inputs_per_min(500))
        );
    }

    #[test]
    fn concurrency_override_maps_configured_in_flight_ceiling() {
        // Pins: a configured max_concurrent_requests becomes the embedder's
        // in-flight ceiling; an unset value leaves the provider default in place.
        assert_eq!(super::concurrency_override(None), None);
        assert_eq!(super::concurrency_override(Some(4)), Some(4));
    }

    #[test]
    fn embedding_provider_kind_accepts_supported_provider_prefixes() {
        // Pins: provider:model parsing accepts provider ids, not model aliases in provider position.
        assert_eq!(
            EmbeddingProviderKind::from_provider_name("cohere", "memory.embedding_model")
                .expect("cohere should parse"),
            Some(EmbeddingProviderKind::Cohere)
        );
        assert_eq!(
            EmbeddingProviderKind::from_provider_name("gemini", "memory.embedding_model")
                .expect("gemini should parse"),
            Some(EmbeddingProviderKind::Gemini)
        );
        assert_eq!(
            EmbeddingProviderKind::from_provider_name("zeroentropy", "memory.embedding_model")
                .expect("zeroentropy should parse"),
            Some(EmbeddingProviderKind::ZeroEntropy)
        );
        assert_eq!(normalize_provider_name(" ZeroEntropy "), "zeroentropy");
    }

    #[test]
    fn cohere_provider_from_config_uses_selector_model() {
        // Pins: selecting Cohere through provider:model uses Embed v4 without a separate provider env.
        let mut config = MoaConfig::default();
        config.memory.embedding_model = "cohere:embed-v4.0".to_string();
        config.providers.cohere.api_key = "test-key".to_string();

        let provider = build_embedding_provider_from_config(&config)
            .expect("cohere provider config should build")
            .expect("cohere provider should be enabled");

        assert_eq!(provider.model_id(), COHERE_DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), 1_536);
    }

    #[test]
    fn zeroentropy_provider_from_config_uses_selector_model() {
        // Pins: selecting ZeroEntropy through provider:model uses zembed-1 without a separate provider env.
        let mut config = MoaConfig::default();
        config.memory.embedding_model = "zeroentropy:zembed-1".to_string();
        config.providers.zeroentropy.api_key = "test-key".to_string();

        let provider = build_embedding_provider_from_config(&config)
            .expect("zeroentropy provider config should build")
            .expect("zeroentropy provider should be enabled");

        assert_eq!(provider.model_id(), ZEROENTROPY_DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), 1_280);
    }

    #[test]
    fn vector_embedder_from_config_uses_latest_cohere_model_id() {
        // Pins: graph-memory Cohere embedder accepts provider:model selectors.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = "cohere:embed-v4.0".to_string();
        config.memory.vector.embedder.output_dim = 1_024;
        config.providers.cohere.api_key = "test-key".to_string();

        let provider =
            build_embedder_from_config(&config, super::EmbedderConstructionRole::Retrieval)
                .expect("cohere vector embedder config should build");

        assert_eq!(provider.model_id(), COHERE_DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), 1_024);
    }

    #[test]
    fn missing_embedding_api_key_disables_provider() {
        // Pins: missing embedding credentials are fail-closed at startup, not a latent runtime panic.
        let mut config = MoaConfig::default();
        config.memory.embedding_model = "cohere:embed-v4.0".to_string();

        let provider = build_embedding_provider_from_config(&config)
            .expect("missing credential should not fail startup");

        assert!(provider.is_none());
    }

    #[test]
    fn provider_model_embedding_selector_strips_prefix() {
        // Pins: a single provider:model value drives embedding provider selection.
        let resolved = resolve_embedding_model("zeroentropy:zembed-1", "memory.embedding_model")
            .expect("selector should parse")
            .expect("provider should resolve");

        assert_eq!(resolved.provider, EmbeddingProviderKind::ZeroEntropy);
        assert_eq!(resolved.model, ZEROENTROPY_DEFAULT_MODEL);
    }

    #[test]
    fn embedding_selector_rejects_bare_model_names() {
        // Pins: embedding provider is encoded in the model selector, so bare model names fail fast.
        let error = resolve_embedding_model(COHERE_DEFAULT_MODEL, "memory.embedding_model")
            .expect_err("bare embedding model should be rejected");

        assert!(error.to_string().contains("provider:model"));
    }

    #[test]
    fn disabled_embedding_selector_disables_provider() {
        // Pins: disabled remains the explicit opt-out for semantic memory embeddings.
        let resolved = resolve_embedding_model("disabled", "memory.embedding_model")
            .expect("disabled should parse");

        assert!(resolved.is_none());
    }

    #[test]
    fn gemini_selector_preserves_provider_model() {
        // Pins: Gemini embedding uses the same provider:model shape as the LLM model router.
        let resolved =
            resolve_embedding_model("gemini:gemini-embedding-2", "memory.embedding_model")
                .expect("selector should parse")
                .expect("provider should resolve");

        assert_eq!(resolved.provider, EmbeddingProviderKind::Gemini);
        assert_eq!(resolved.model, GEMINI_V2_MODEL);
    }
}
