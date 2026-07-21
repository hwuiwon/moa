//! Provider-family and model-name routing descriptors.

use std::sync::Arc;

use moa_core::{config::MoaConfig, traits::LLMProvider, types::provider::ProviderId};

use crate::{AnthropicProvider, GeminiProvider, OpenAIProvider};

/// Default Anthropic model used when Anthropic is the first configured provider.
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-6";
/// Default OpenAI model used when OpenAI is the first configured provider.
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-5.4";
/// Default Google model used when Google is the first configured provider.
pub const DEFAULT_GOOGLE_MODEL: &str = "gemini-3-flash-preview";
/// Default Anthropic model for query rewriting.
pub const REWRITER_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
/// Default OpenAI model for query rewriting.
pub const REWRITER_OPENAI_MODEL: &str = "gpt-5.4-nano";
/// Default Google model for query rewriting.
pub const REWRITER_GOOGLE_MODEL: &str = "gemini-3-flash-preview";

/// Factory used to construct a provider from runtime config and a model id.
pub type ConfigProviderFactory =
    fn(&MoaConfig, &str) -> moa_core::error::Result<Arc<dyn LLMProvider>>;

/// Accessor for a provider's API-key setting.
pub type ApiKeyAccessor = for<'a> fn(&'a MoaConfig) -> &'a str;

/// Predicate used to infer a provider from a model-name prefix.
pub type ModelInferencePredicate = fn(&str) -> bool;

/// Provider registration descriptor used by routing and registry construction.
#[derive(Clone, Copy)]
pub struct ProviderDescriptor {
    /// Stable provider id used as the registry key.
    pub id: ProviderId,
    /// Default model used when this provider supplies main-loop requests.
    pub default_model: &'static str,
    /// Default model used when this provider supplies query-rewrite requests.
    pub rewriter_default_model: &'static str,
    /// Explicit model prefix accepted in config, such as `openai:gpt-5.4`.
    pub explicit_prefix: &'static str,
    /// Predicate for inferring this provider from model ids.
    pub infer_model: ModelInferencePredicate,
    /// Lower values win when resolving the default main-loop provider.
    pub default_priority: u8,
    /// Lower values win when resolving the default query-rewrite provider.
    pub rewriter_priority: u8,
    /// Config accessor for this provider's loaded API key.
    pub api_key: ApiKeyAccessor,
    /// Provider factory using full runtime config.
    pub build_from_config: ConfigProviderFactory,
}

/// All built-in provider descriptors.
pub const PROVIDER_DESCRIPTORS: &[ProviderDescriptor] =
    &[OPENAI_DESCRIPTOR, ANTHROPIC_DESCRIPTOR, GOOGLE_DESCRIPTOR];

const OPENAI_DESCRIPTOR: ProviderDescriptor = ProviderDescriptor {
    id: ProviderId::OpenAI,
    default_model: DEFAULT_OPENAI_MODEL,
    rewriter_default_model: REWRITER_OPENAI_MODEL,
    explicit_prefix: "openai",
    infer_model: is_openai_model,
    default_priority: 0,
    rewriter_priority: 0,
    api_key: openai_api_key,
    build_from_config: build_openai_provider_from_config,
};

const ANTHROPIC_DESCRIPTOR: ProviderDescriptor = ProviderDescriptor {
    id: ProviderId::Anthropic,
    default_model: DEFAULT_ANTHROPIC_MODEL,
    rewriter_default_model: REWRITER_ANTHROPIC_MODEL,
    explicit_prefix: "anthropic",
    infer_model: is_anthropic_model,
    default_priority: 1,
    rewriter_priority: 1,
    api_key: anthropic_api_key,
    build_from_config: build_anthropic_provider_from_config,
};

const GOOGLE_DESCRIPTOR: ProviderDescriptor = ProviderDescriptor {
    id: ProviderId::Google,
    default_model: DEFAULT_GOOGLE_MODEL,
    rewriter_default_model: REWRITER_GOOGLE_MODEL,
    explicit_prefix: "google",
    infer_model: is_google_model,
    default_priority: 2,
    rewriter_priority: 2,
    api_key: google_api_key,
    build_from_config: build_google_provider_from_config,
};

/// Returns the provider descriptor for a stable provider name.
#[must_use]
pub fn provider_descriptor_by_name(provider_name: &str) -> Option<&'static ProviderDescriptor> {
    provider_name.trim().parse().ok().map(provider_descriptor)
}

/// Returns the provider-owned routing descriptor for `id`.
#[must_use]
pub const fn provider_descriptor(id: ProviderId) -> &'static ProviderDescriptor {
    match id {
        ProviderId::OpenAI => &OPENAI_DESCRIPTOR,
        ProviderId::Anthropic => &ANTHROPIC_DESCRIPTOR,
        ProviderId::Google => &GOOGLE_DESCRIPTOR,
    }
}

/// Splits a `provider:model` override into provider id and model id.
#[must_use]
pub fn split_explicit_provider(model: &str) -> Option<(ProviderId, &str)> {
    let (provider, model_id) = model.split_once(':')?;
    let model_id = model_id.trim();
    if model_id.is_empty() {
        return None;
    }

    let descriptor = provider_descriptor_by_name(provider)?;
    Some((descriptor.id, model_id))
}

/// Infers a provider id from a model name using the catalog and descriptors.
#[must_use]
pub fn infer_provider_id(model: &str) -> Option<ProviderId> {
    if let Some(catalog_model) = crate::core::models::find_model(model) {
        return provider_descriptor_by_name(catalog_model.provider).map(|descriptor| descriptor.id);
    }

    PROVIDER_DESCRIPTORS
        .iter()
        .find(|descriptor| (descriptor.infer_model)(model))
        .map(|descriptor| descriptor.id)
}

fn anthropic_api_key(config: &MoaConfig) -> &str {
    &config.providers.anthropic.api_key
}

fn openai_api_key(config: &MoaConfig) -> &str {
    &config.providers.openai.api_key
}

fn google_api_key(config: &MoaConfig) -> &str {
    &config.providers.google.api_key
}

fn build_anthropic_provider_from_config(
    config: &MoaConfig,
    model: &str,
) -> moa_core::error::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(AnthropicProvider::from_config_with_model(
        config, model,
    )?))
}

fn build_openai_provider_from_config(
    config: &MoaConfig,
    model: &str,
) -> moa_core::error::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(OpenAIProvider::from_config_with_model(
        config, model,
    )?))
}

fn build_google_provider_from_config(
    config: &MoaConfig,
    model: &str,
) -> moa_core::error::Result<Arc<dyn LLMProvider>> {
    Ok(Arc::new(GeminiProvider::from_config_with_model(
        config, model,
    )?))
}

fn is_anthropic_model(model: &str) -> bool {
    model.starts_with("claude-")
}

fn is_google_model(model: &str) -> bool {
    model.starts_with("gemini-")
}

fn is_openai_model(model: &str) -> bool {
    model.starts_with("gpt-")
        || model.starts_with("chatgpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

#[cfg(test)]
mod tests {
    use moa_core::types::provider::ProviderId;

    use crate::core::models::CATALOG;

    use super::{infer_provider_id, provider_descriptor_by_name, split_explicit_provider};

    #[test]
    fn descriptors_cover_catalog_provider_names() {
        // Pins: the model catalog and routing descriptors use the same provider ids.
        for model in CATALOG {
            assert!(
                provider_descriptor_by_name(model.provider).is_some(),
                "catalog provider {} has no routing descriptor",
                model.provider
            );
        }
    }

    #[test]
    fn explicit_prefixes_resolve_to_trimmed_provider_and_model() {
        assert_eq!(
            split_explicit_provider("openai: gpt-5.4 "),
            Some((ProviderId::OpenAI, "gpt-5.4"))
        );
        assert_eq!(
            split_explicit_provider("anthropic:claude-sonnet-4-6"),
            Some((ProviderId::Anthropic, "claude-sonnet-4-6"))
        );
        assert_eq!(
            split_explicit_provider("google:gemini-3-flash-preview"),
            Some((ProviderId::Google, "gemini-3-flash-preview"))
        );
        assert_eq!(split_explicit_provider("openai:"), None);
        assert_eq!(split_explicit_provider("unknown:gpt-5.4"), None);
    }

    #[test]
    fn model_inference_uses_catalog_and_known_family_prefixes() {
        assert_eq!(infer_provider_id("gpt-5.4"), Some(ProviderId::OpenAI));
        assert_eq!(
            infer_provider_id("claude-sonnet-4-6"),
            Some(ProviderId::Anthropic)
        );
        assert_eq!(
            infer_provider_id("gemini-3-flash-preview"),
            Some(ProviderId::Google)
        );
        assert_eq!(infer_provider_id("unowned-model-family"), None);
    }
}
