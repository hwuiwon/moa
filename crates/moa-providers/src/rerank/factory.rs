//! Reranker provider selection and construction from runtime config.

use std::sync::Arc;

use moa_core::{MoaConfig, MoaError, Result};

use super::cohere::COHERE_DEFAULT_RERANK_MODEL;
#[cfg(test)]
use super::zeroentropy::ZEROENTROPY_DEFAULT_RERANK_MODEL;
use super::zeroentropy::ZeroEntropyRerankLatency;
use super::{CohereReranker, NOOP_RERANK_MODEL, NoopReranker, Reranker, ZeroEntropyReranker};
use crate::model_selection::{normalize_provider_name, split_explicit_provider_model};

const COHERE_PROVIDER_NAME: &str = "cohere";
const ZEROENTROPY_PROVIDER_NAME: &str = "zeroentropy";

/// A reranker backend paired with the model selected for runtime calls.
#[derive(Clone)]
pub struct ConfiguredReranker {
    /// Reranker backend implementation.
    pub reranker: Arc<dyn Reranker>,
    /// Model id passed to the backend.
    pub model: String,
    /// Provider id used for observability.
    pub provider: String,
}

impl ConfiguredReranker {
    /// Builds a deterministic no-op reranker configuration.
    #[must_use]
    pub fn noop() -> Self {
        Self {
            reranker: NoopReranker::shared(),
            model: NOOP_RERANK_MODEL.to_string(),
            provider: "noop".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RerankerProviderKind {
    Cohere,
    ZeroEntropy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedRerankerProvider {
    provider: RerankerProviderKind,
    model: String,
}

impl RerankerProviderKind {
    fn from_provider_name(name: &str) -> Result<Option<Self>> {
        match normalize_provider_name(name).as_str() {
            "" | "disabled" | "noop" => Ok(None),
            COHERE_PROVIDER_NAME => Ok(Some(Self::Cohere)),
            ZEROENTROPY_PROVIDER_NAME => Ok(Some(Self::ZeroEntropy)),
            unsupported => Err(MoaError::ConfigError(format!(
                "unsupported memory.retrieval.reranker_model provider '{unsupported}'"
            ))),
        }
    }

    fn provider_name(self) -> &'static str {
        match self {
            Self::Cohere => COHERE_PROVIDER_NAME,
            Self::ZeroEntropy => ZEROENTROPY_PROVIDER_NAME,
        }
    }
}

/// Builds the configured reranker for graph-memory retrieval.
pub fn build_reranker_from_config(config: &MoaConfig) -> Result<ConfiguredReranker> {
    let Some(resolved) = resolve_reranker_model(&config.memory.retrieval.reranker_model)? else {
        return Ok(ConfiguredReranker::noop());
    };

    match build_provider(resolved.provider, config) {
        Ok(reranker) => Ok(ConfiguredReranker {
            reranker,
            model: resolved.model,
            provider: resolved.provider.provider_name().to_string(),
        }),
        Err(MoaError::MissingEnvironmentVariable(env_name)) => {
            tracing::warn!(
                env = %env_name,
                provider = resolved.provider.provider_name(),
                "graph-memory reranking disabled because the provider API key is missing"
            );
            Ok(ConfiguredReranker::noop())
        }
        Err(error) => Err(error),
    }
}

fn resolve_reranker_model(model_name: &str) -> Result<Option<ResolvedRerankerProvider>> {
    let normalized = normalize_provider_name(model_name);
    if normalized.is_empty() || normalized == "disabled" || normalized == NOOP_RERANK_MODEL {
        return Ok(None);
    }
    if let Some(explicit) =
        split_explicit_provider_model(model_name, "memory.retrieval.reranker_model")?
    {
        let Some(provider) = RerankerProviderKind::from_provider_name(explicit.provider)? else {
            return Ok(None);
        };
        return Ok(Some(ResolvedRerankerProvider {
            provider,
            model: explicit.model.to_string(),
        }));
    }

    Err(MoaError::ConfigError(format!(
        "memory.retrieval.reranker_model must use provider:model, such as cohere:{COHERE_DEFAULT_RERANK_MODEL}"
    )))
}

fn build_provider(provider: RerankerProviderKind, config: &MoaConfig) -> Result<Arc<dyn Reranker>> {
    match provider {
        RerankerProviderKind::Cohere => {
            ensure_no_zeroentropy_latency(config, provider)?;
            let api_key = moa_core::config::required_config_secret(
                "MOA_COHERE_API_KEY",
                &config.providers.cohere.api_key,
            )?;
            Ok(Arc::new(CohereReranker::new(api_key)?))
        }
        RerankerProviderKind::ZeroEntropy => {
            let api_key = moa_core::config::required_config_secret(
                "MOA_ZEROENTROPY_API_KEY",
                &config.providers.zeroentropy.api_key,
            )?;
            let latency = config
                .memory
                .retrieval
                .reranker_latency
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(ZeroEntropyRerankLatency::parse)
                .transpose()?;
            Ok(Arc::new(
                ZeroEntropyReranker::new(api_key)?.with_latency(latency),
            ))
        }
    }
}

fn ensure_no_zeroentropy_latency(config: &MoaConfig, provider: RerankerProviderKind) -> Result<()> {
    if provider != RerankerProviderKind::ZeroEntropy
        && config
            .memory
            .retrieval
            .reranker_latency
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(MoaError::ConfigError(format!(
            "memory.retrieval.reranker_latency is only supported for {ZEROENTROPY_PROVIDER_NAME}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use super::{
        COHERE_DEFAULT_RERANK_MODEL, RerankerProviderKind, ZEROENTROPY_DEFAULT_RERANK_MODEL,
        build_reranker_from_config, resolve_reranker_model,
    };
    use crate::model_selection::normalize_provider_name;

    #[test]
    fn reranker_provider_kind_accepts_supported_provider_prefixes() {
        // Pins: provider:model parsing accepts provider ids, not model aliases in provider position.
        assert_eq!(
            RerankerProviderKind::from_provider_name("zeroentropy")
                .expect("zeroentropy should parse"),
            Some(RerankerProviderKind::ZeroEntropy)
        );
        assert_eq!(
            RerankerProviderKind::from_provider_name("cohere").expect("cohere should parse"),
            Some(RerankerProviderKind::Cohere)
        );
        assert_eq!(normalize_provider_name("ZeroEntropy"), "zeroentropy");
    }

    #[test]
    fn cohere_reranker_from_config_uses_selector_model() {
        // Pins: selecting Cohere through provider:model avoids a separate provider env.
        let mut config = MoaConfig::default();
        config.memory.retrieval.reranker_model = "cohere:rerank-v4.0-fast".to_string();
        config.providers.cohere.api_key = "test-key".to_string();

        let configured =
            build_reranker_from_config(&config).expect("cohere reranker config should build");

        assert_eq!(configured.provider, "cohere");
        assert_eq!(configured.model, COHERE_DEFAULT_RERANK_MODEL);
    }

    #[test]
    fn zeroentropy_reranker_from_config_uses_selector_model_and_latency() {
        // Pins: selecting ZeroEntropy through provider:model uses zerank-2 and accepts latency.
        let mut config = MoaConfig::default();
        config.memory.retrieval.reranker_model = "zeroentropy:zerank-2".to_string();
        config.memory.retrieval.reranker_latency = Some("fast".to_string());
        config.providers.zeroentropy.api_key = "test-key".to_string();

        let configured =
            build_reranker_from_config(&config).expect("zeroentropy reranker config should build");

        assert_eq!(configured.provider, "zeroentropy");
        assert_eq!(configured.model, ZEROENTROPY_DEFAULT_RERANK_MODEL);
    }

    #[test]
    fn missing_reranker_key_disables_provider() {
        // Pins: missing reranker credentials disable reranking instead of failing startup.
        let mut config = MoaConfig::default();
        config.memory.retrieval.reranker_model = "zeroentropy:zerank-2".to_string();

        let configured = build_reranker_from_config(&config)
            .expect("missing credential should not fail startup");

        assert_eq!(configured.provider, "noop");
        assert_eq!(configured.model, "noop");
    }

    #[test]
    fn default_reranker_from_config_is_noop() {
        // Pins: the runtime always enables the reranker slot, so the default backend must be no-op.
        let config = MoaConfig::default();

        let configured = build_reranker_from_config(&config).expect("noop config should build");

        assert_eq!(configured.provider, "noop");
        assert_eq!(configured.model, "noop");
    }

    #[test]
    fn zeroentropy_latency_is_provider_specific() {
        // Pins: provider-specific options do not silently affect other reranker backends.
        let mut config = MoaConfig::default();
        config.memory.retrieval.reranker_model = "cohere:rerank-v4.0".to_string();
        config.memory.retrieval.reranker_latency = Some("fast".to_string());
        config.providers.cohere.api_key = "test-key".to_string();

        let error = match build_reranker_from_config(&config) {
            Ok(_) => panic!("latency should be rejected"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("reranker_latency"));
    }

    #[test]
    fn provider_model_reranker_selector_strips_prefix() {
        // Pins: a single provider:model value drives reranker provider selection.
        let resolved = resolve_reranker_model("zeroentropy:zerank-2")
            .expect("selector should parse")
            .expect("provider should resolve");

        assert_eq!(resolved.provider, RerankerProviderKind::ZeroEntropy);
        assert_eq!(resolved.model, ZEROENTROPY_DEFAULT_RERANK_MODEL);
    }

    #[test]
    fn reranker_selector_rejects_bare_model_names() {
        // Pins: reranker provider is encoded in the model selector, so bare model names fail fast.
        let error = resolve_reranker_model(ZEROENTROPY_DEFAULT_RERANK_MODEL)
            .expect_err("bare reranker model should be rejected");

        assert!(error.to_string().contains("provider:model"));
    }
}
