//! Embedding provider selection and construction from runtime config.

use std::env;
use std::sync::Arc;

use moa_core::traits::EmbeddingProvider;
use moa_core::{MoaConfig, MoaError, Result};

use super::cohere::COHERE_DEFAULT_MODEL;
use super::gemini::GEMINI_V2_MODEL;
use super::model_from_config_with_provider_default;
use super::zeroentropy::ZEROENTROPY_DEFAULT_MODEL;
use super::{
    CohereEmbedding, EmbedRole, EmbedderConstructionRole, GeminiEmbeddingEmbedder, OpenAIEmbedding,
    ZeroEntropyEmbedding,
};

const OPENAI_PROVIDER_NAME: &str = "openai";
const COHERE_PROVIDER_NAME: &str = "cohere";
const GEMINI_PROVIDER_NAME: &str = "gemini";
const ZEROENTROPY_PROVIDER_NAME: &str = "zeroentropy";
const ZEROENTROPY_MODEL_ALIAS: &str = "zembed-1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmbeddingProviderKind {
    OpenAi,
    Cohere,
    Gemini,
    ZeroEntropy,
}

impl EmbeddingProviderKind {
    fn from_config_name(name: &str) -> Result<Option<Self>> {
        match normalize_provider_name(name).as_str() {
            "" | "disabled" => Ok(None),
            OPENAI_PROVIDER_NAME => Ok(Some(Self::OpenAi)),
            COHERE_PROVIDER_NAME => Ok(Some(Self::Cohere)),
            GEMINI_PROVIDER_NAME | GEMINI_V2_MODEL => Ok(Some(Self::Gemini)),
            ZEROENTROPY_PROVIDER_NAME | ZEROENTROPY_MODEL_ALIAS => Ok(Some(Self::ZeroEntropy)),
            unsupported => Err(MoaError::ConfigError(format!(
                "unsupported memory.embedding_provider '{unsupported}'"
            ))),
        }
    }

    fn build_with_env(
        self,
        config: &MoaConfig,
        env_lookup: &impl Fn(&str) -> std::result::Result<String, env::VarError>,
    ) -> Result<Arc<dyn EmbeddingProvider>> {
        match self {
            Self::OpenAi => Ok(Arc::new(OpenAIEmbedding::from_config_with_env(
                config, env_lookup,
            )?)),
            Self::Cohere => Ok(Arc::new(CohereEmbedding::from_config_with_model_env(
                config,
                model_from_config_with_provider_default(config, COHERE_DEFAULT_MODEL),
                env_lookup,
            )?)),
            Self::Gemini => Ok(Arc::new(build_gemini_embedder_with_env(
                config,
                EmbedderConstructionRole::Retrieval,
                env_lookup,
            )?)),
            Self::ZeroEntropy => Ok(Arc::new(ZeroEntropyEmbedding::from_config_with_model_env(
                config,
                model_from_config_with_provider_default(config, ZEROENTROPY_DEFAULT_MODEL),
                env_lookup,
            )?)),
        }
    }
}

fn normalize_provider_name(name: &str) -> String {
    name.trim().to_ascii_lowercase().replace('_', "-")
}

/// Builds a vector-space embedder from the tenant memory embedder configuration.
pub fn build_embedder_from_config(
    config: &MoaConfig,
    role: EmbedderConstructionRole,
) -> Result<Arc<dyn EmbeddingProvider>> {
    build_embedder_from_config_with_env(config, role, &|name| env::var(name))
}

fn build_embedder_from_config_with_env(
    config: &MoaConfig,
    role: EmbedderConstructionRole,
    env_lookup: &impl Fn(&str) -> std::result::Result<String, env::VarError>,
) -> Result<Arc<dyn EmbeddingProvider>> {
    let cfg = &config.memory.vector.embedder;
    match normalize_provider_name(&cfg.name).as_str() {
        COHERE_DEFAULT_MODEL => {
            let api_key = read_api_key("MOA_COHERE_API_KEY", &config.providers.cohere.api_key)?;
            Ok(Arc::new(
                CohereEmbedding::new(api_key, COHERE_DEFAULT_MODEL)?
                    .with_dimensions(cfg.output_dim)?,
            ))
        }
        GEMINI_V2_MODEL => Ok(Arc::new(build_gemini_embedder_with_env(
            config, role, env_lookup,
        )?)),
        other => Err(MoaError::ConfigError(format!("unknown embedder `{other}`"))),
    }
}

fn build_gemini_embedder_with_env(
    config: &MoaConfig,
    role: EmbedderConstructionRole,
    _env_lookup: &impl Fn(&str) -> std::result::Result<String, env::VarError>,
) -> Result<GeminiEmbeddingEmbedder> {
    let cfg = &config.memory.vector.embedder;
    let api_key = read_api_key("MOA_GOOGLE_API_KEY", &config.providers.google.api_key)?;
    let role = match role {
        EmbedderConstructionRole::Ingestion => EmbedRole::Document { title: None },
        EmbedderConstructionRole::Retrieval => parse_embed_role(&cfg.gemini.default_role)?,
    };
    GeminiEmbeddingEmbedder::new(api_key, cfg.output_dim, role)
}

fn read_api_key(env_name: &'static str, value: &str) -> Result<String> {
    moa_core::config::required_config_secret(env_name, value)
}

fn parse_embed_role(value: &str) -> Result<EmbedRole> {
    match normalize_config_key(value).as_str() {
        "search_query" => Ok(EmbedRole::SearchQuery),
        "document" => Ok(EmbedRole::Document { title: None }),
        "question_answering" => Ok(EmbedRole::QuestionAnsweringQuery),
        "fact_checking" => Ok(EmbedRole::FactCheckingQuery),
        "code_retrieval" => Ok(EmbedRole::CodeRetrievalQuery),
        "classification" => Ok(EmbedRole::Classification),
        "clustering" => Ok(EmbedRole::Clustering),
        "sentence_similarity" => Ok(EmbedRole::SentenceSimilarity),
        "raw" => Ok(EmbedRole::Raw),
        other => Err(MoaError::ConfigError(format!(
            "unknown gemini v2 embed role `{other}`"
        ))),
    }
}

fn normalize_config_key(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

/// Builds the configured embedding provider for semantic memory search.
pub fn build_embedding_provider_from_config(
    config: &MoaConfig,
) -> Result<Option<Arc<dyn EmbeddingProvider>>> {
    build_embedding_provider_from_config_with_env(config, &|name| env::var(name))
}

fn build_embedding_provider_from_config_with_env(
    config: &MoaConfig,
    env_lookup: &impl Fn(&str) -> std::result::Result<String, env::VarError>,
) -> Result<Option<Arc<dyn EmbeddingProvider>>> {
    let Some(provider) =
        EmbeddingProviderKind::from_config_name(&config.memory.embedding_provider)?
    else {
        return Ok(None);
    };

    match provider.build_with_env(config, env_lookup) {
        Ok(provider) => Ok(Some(provider)),
        Err(MoaError::MissingEnvironmentVariable(env_name)) => {
            tracing::warn!(
                env = %env_name,
                "semantic memory search disabled because the embedding API key is missing"
            );
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::env::VarError;

    use moa_core::MoaConfig;

    use super::{
        COHERE_DEFAULT_MODEL, EmbeddingProviderKind, ZEROENTROPY_DEFAULT_MODEL,
        build_embedder_from_config_with_env, build_embedding_provider_from_config_with_env,
        normalize_provider_name,
    };
    #[test]
    fn embedding_provider_kind_accepts_cohere_aliases() {
        // Pins: provider selection stays table-driven as more embedders are added.
        assert_eq!(
            EmbeddingProviderKind::from_config_name("cohere").expect("cohere should parse"),
            Some(EmbeddingProviderKind::Cohere)
        );
        assert_eq!(normalize_provider_name(" Cohere "), "cohere");
    }

    #[test]
    fn embedding_provider_kind_accepts_gemini_aliases() {
        // Pins: Gemini embedding provider selection is consolidated in moa-providers.
        assert_eq!(
            EmbeddingProviderKind::from_config_name("gemini").expect("gemini should parse"),
            Some(EmbeddingProviderKind::Gemini)
        );
        assert_eq!(
            EmbeddingProviderKind::from_config_name("gemini_embedding_2")
                .expect("gemini model alias should parse"),
            Some(EmbeddingProviderKind::Gemini)
        );
    }

    #[test]
    fn embedding_provider_kind_accepts_zeroentropy_aliases() {
        // Pins: provider selection accepts the documented ZeroEntropy model id and provider spelling.
        assert_eq!(
            EmbeddingProviderKind::from_config_name("zeroentropy")
                .expect("zeroentropy should parse"),
            Some(EmbeddingProviderKind::ZeroEntropy)
        );
        assert_eq!(
            EmbeddingProviderKind::from_config_name("zembed-1").expect("zembed alias should parse"),
            Some(EmbeddingProviderKind::ZeroEntropy)
        );
    }

    #[test]
    fn cohere_provider_from_config_uses_cohere_embed_v4_defaults() {
        // Pins: selecting Cohere without overriding the legacy OpenAI model default uses Embed v4.
        let mut config = MoaConfig::default();
        config.memory.embedding_provider = "cohere".to_string();
        config.providers.cohere.api_key = "test-key".to_string();

        let provider =
            build_embedding_provider_from_config_with_env(&config, &|_| Err(VarError::NotPresent))
                .expect("cohere provider config should build")
                .expect("cohere provider should be enabled");

        assert_eq!(provider.model_id(), COHERE_DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), 1_536);
    }

    #[test]
    fn zeroentropy_provider_from_config_uses_zembed_defaults() {
        // Pins: selecting ZeroEntropy without overriding the legacy OpenAI model default uses zembed-1.
        let mut config = MoaConfig::default();
        config.memory.embedding_provider = "zeroentropy".to_string();
        config.providers.zeroentropy.api_key = "test-key".to_string();

        let provider =
            build_embedding_provider_from_config_with_env(&config, &|_| Err(VarError::NotPresent))
                .expect("zeroentropy provider config should build")
                .expect("zeroentropy provider should be enabled");

        assert_eq!(provider.model_id(), ZEROENTROPY_DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), 1_280);
    }

    #[test]
    fn vector_embedder_from_config_uses_latest_cohere_model_id() {
        // Pins: graph-memory Cohere embedder reports the actual provider model id, not a legacy alias.
        let mut config = MoaConfig::default();
        config.memory.vector.embedder.name = COHERE_DEFAULT_MODEL.to_string();
        config.memory.vector.embedder.output_dim = 1_024;
        config.providers.cohere.api_key = "test-key".to_string();

        let provider = build_embedder_from_config_with_env(
            &config,
            super::EmbedderConstructionRole::Retrieval,
            &|_| Err(VarError::NotPresent),
        )
        .expect("cohere vector embedder config should build");

        assert_eq!(provider.model_id(), COHERE_DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), 1_024);
    }

    #[test]
    fn missing_embedding_api_key_disables_provider() {
        // Pins: missing embedding credentials are fail-closed at startup, not a latent runtime panic.
        let mut config = MoaConfig::default();
        config.memory.embedding_provider = "cohere".to_string();

        let provider =
            build_embedding_provider_from_config_with_env(&config, &|_| Err(VarError::NotPresent))
                .expect("missing credential should not fail startup");

        assert!(provider.is_none());
    }
}
