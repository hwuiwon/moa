//! Reranker provider selection and construction from runtime config.

use std::sync::Arc;

use moa_core::{MoaConfig, MoaError, Result};

use super::cohere::COHERE_DEFAULT_RERANK_MODEL;
use super::zeroentropy::{ZEROENTROPY_DEFAULT_RERANK_MODEL, ZeroEntropyRerankLatency};
use super::{CohereReranker, NOOP_RERANK_MODEL, NoopReranker, Reranker, ZeroEntropyReranker};

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

impl RerankerProviderKind {
    fn from_config(provider: &str, model: &str) -> Result<Option<Self>> {
        if let Some(provider) = Self::from_provider_name(provider)? {
            return Ok(Some(provider));
        }
        Self::from_model_name(model)
    }

    fn from_provider_name(name: &str) -> Result<Option<Self>> {
        match normalize_provider_name(name).as_str() {
            "" | "disabled" | "noop" => Ok(None),
            COHERE_PROVIDER_NAME => Ok(Some(Self::Cohere)),
            ZEROENTROPY_PROVIDER_NAME
            | ZEROENTROPY_DEFAULT_RERANK_MODEL
            | "zerank-1"
            | "zerank-1-small" => Ok(Some(Self::ZeroEntropy)),
            unsupported => Err(MoaError::ConfigError(format!(
                "unsupported memory.retrieval.reranker_provider '{unsupported}'"
            ))),
        }
    }

    fn from_model_name(name: &str) -> Result<Option<Self>> {
        match normalize_provider_name(name).as_str() {
            "" | "disabled" | "noop" => Ok(None),
            "rerank-v4.0-fast" | "rerank-v4.0-pro" | "rerank-v3.5" => Ok(Some(Self::Cohere)),
            ZEROENTROPY_DEFAULT_RERANK_MODEL | "zerank-1" | "zerank-1-small" => {
                Ok(Some(Self::ZeroEntropy))
            }
            unsupported => Err(MoaError::ConfigError(format!(
                "unsupported memory.retrieval.reranker_model '{unsupported}' without an explicit reranker_provider"
            ))),
        }
    }

    fn provider_name(self) -> &'static str {
        match self {
            Self::Cohere => COHERE_PROVIDER_NAME,
            Self::ZeroEntropy => ZEROENTROPY_PROVIDER_NAME,
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            Self::Cohere => COHERE_DEFAULT_RERANK_MODEL,
            Self::ZeroEntropy => ZEROENTROPY_DEFAULT_RERANK_MODEL,
        }
    }
}

/// Builds the configured reranker for graph-memory retrieval.
pub fn build_reranker_from_config(config: &MoaConfig) -> Result<ConfiguredReranker> {
    let Some(provider) = RerankerProviderKind::from_config(
        &config.memory.retrieval.reranker_provider,
        &config.memory.retrieval.reranker_model,
    )?
    else {
        return Ok(ConfiguredReranker::noop());
    };

    let model = model_from_config_with_provider_default(
        &config.memory.retrieval.reranker_model,
        provider.default_model(),
    );
    match build_provider(provider, config) {
        Ok(reranker) => Ok(ConfiguredReranker {
            reranker,
            model,
            provider: provider.provider_name().to_string(),
        }),
        Err(MoaError::MissingEnvironmentVariable(env_name)) => {
            tracing::warn!(
                env = %env_name,
                provider = provider.provider_name(),
                "graph-memory reranking disabled because the provider API key is missing"
            );
            Ok(ConfiguredReranker::noop())
        }
        Err(error) => Err(error),
    }
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

fn model_from_config_with_provider_default(model: &str, provider_default: &str) -> String {
    let model = model.trim();
    if model.is_empty() || model.eq_ignore_ascii_case(NOOP_RERANK_MODEL) {
        provider_default.to_string()
    } else {
        model.to_string()
    }
}

fn normalize_provider_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use super::{
        COHERE_DEFAULT_RERANK_MODEL, RerankerProviderKind, ZEROENTROPY_DEFAULT_RERANK_MODEL,
        build_reranker_from_config, normalize_provider_name,
    };

    #[test]
    fn reranker_provider_kind_accepts_zeroentropy_aliases() {
        // Pins: provider selection stays table-driven as more rerankers are added.
        assert_eq!(
            RerankerProviderKind::from_config("zeroentropy", "").expect("zeroentropy should parse"),
            Some(RerankerProviderKind::ZeroEntropy)
        );
        assert_eq!(
            RerankerProviderKind::from_config("", "zerank-2").expect("model alias should parse"),
            Some(RerankerProviderKind::ZeroEntropy)
        );
        assert_eq!(normalize_provider_name("ZeroEntropy"), "zeroentropy");
    }

    #[test]
    fn reranker_provider_kind_accepts_cohere_model_aliases() {
        // Pins: setting only a Cohere rerank model is enough to select the Cohere provider.
        assert_eq!(
            RerankerProviderKind::from_config("noop", "rerank-v4.0-fast")
                .expect("cohere model should parse"),
            Some(RerankerProviderKind::Cohere)
        );
    }

    #[test]
    fn cohere_reranker_from_config_uses_default_model() {
        // Pins: selecting Cohere with the noop default model uses the configured Cohere Rerank v4 default.
        let mut config = MoaConfig::default();
        config.memory.retrieval.reranker_provider = "cohere".to_string();
        config.providers.cohere.api_key = "test-key".to_string();

        let configured =
            build_reranker_from_config(&config).expect("cohere reranker config should build");

        assert_eq!(configured.provider, "cohere");
        assert_eq!(configured.model, COHERE_DEFAULT_RERANK_MODEL);
    }

    #[test]
    fn zeroentropy_reranker_from_config_uses_default_model_and_latency() {
        // Pins: selecting ZeroEntropy with the noop default model uses zerank-2 and accepts latency.
        let mut config = MoaConfig::default();
        config.memory.retrieval.reranker_provider = "zeroentropy".to_string();
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
        config.memory.retrieval.reranker_provider = "zeroentropy".to_string();

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
        config.memory.retrieval.reranker_provider = "cohere".to_string();
        config.memory.retrieval.reranker_latency = Some("fast".to_string());
        config.providers.cohere.api_key = "test-key".to_string();

        let error = match build_reranker_from_config(&config) {
            Ok(_) => panic!("latency should be rejected"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("reranker_latency"));
    }
}
