//! Provider-selection helpers for MOA runtime wiring.

use std::sync::Arc;

use moa_core::{LLMProvider, MoaConfig, MoaError, Result};

use crate::{AnthropicProvider, OpenAIProvider, OpenRouterProvider};

const PROVIDER_ANTHROPIC: &str = "anthropic";
const PROVIDER_OPENAI: &str = "openai";
const PROVIDER_OPENROUTER: &str = "openrouter";

/// Resolved provider/model choice used to construct one provider instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSelection {
    /// Canonical provider name.
    pub provider_name: String,
    /// Canonical model identifier for that provider.
    pub model_id: String,
}

/// Resolves the effective provider and model from config plus an optional user override.
pub fn resolve_provider_selection(
    config: &MoaConfig,
    model_override: Option<&str>,
) -> Result<ProviderSelection> {
    let requested = model_override
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(config.general.default_model.as_str());
    let default_provider = config.general.default_provider.trim();

    if let Some((provider_name, model_id)) = split_explicit_provider(requested) {
        return Ok(ProviderSelection {
            provider_name: provider_name.to_string(),
            model_id: normalize_model_for_provider(provider_name, model_id),
        });
    }

    let provider_name = infer_provider_name(requested).unwrap_or(default_provider);
    validate_provider_name(provider_name)?;

    Ok(ProviderSelection {
        provider_name: provider_name.to_string(),
        model_id: normalize_model_for_provider(provider_name, requested),
    })
}

/// Builds the configured provider using the config's effective default provider/model pair.
pub fn build_provider_from_config(config: &MoaConfig) -> Result<Arc<dyn LLMProvider>> {
    let selection = resolve_provider_selection(config, None)?;
    build_provider_from_selection(config, &selection)
}

/// Builds one provider instance from an explicit provider/model selection.
pub fn build_provider_from_selection(
    config: &MoaConfig,
    selection: &ProviderSelection,
) -> Result<Arc<dyn LLMProvider>> {
    let provider: Arc<dyn LLMProvider> = match selection.provider_name.as_str() {
        PROVIDER_ANTHROPIC => Arc::new(AnthropicProvider::from_config_with_model(
            config,
            selection.model_id.clone(),
        )?),
        PROVIDER_OPENAI => Arc::new(OpenAIProvider::from_config_with_model(
            config,
            selection.model_id.clone(),
        )?),
        PROVIDER_OPENROUTER => Arc::new(OpenRouterProvider::from_config_with_model(
            config,
            selection.model_id.clone(),
        )?),
        unsupported => {
            return Err(MoaError::ConfigError(format!(
                "unsupported provider '{unsupported}'"
            )));
        }
    };

    Ok(provider)
}

fn split_explicit_provider(model: &str) -> Option<(&str, &str)> {
    let (provider_name, model_id) = model.split_once(':')?;
    let provider_name = provider_name.trim();
    let model_id = model_id.trim();

    if model_id.is_empty() || !matches_provider_name(provider_name) {
        return None;
    }

    Some((provider_name, model_id))
}

fn infer_provider_name(model: &str) -> Option<&'static str> {
    if model.contains('/') {
        return Some(PROVIDER_OPENROUTER);
    }

    if model.starts_with("claude-") {
        return Some(PROVIDER_ANTHROPIC);
    }

    if is_openai_model(model) {
        return Some(PROVIDER_OPENAI);
    }

    None
}

fn normalize_model_for_provider(provider_name: &str, model: &str) -> String {
    let model = model.trim();
    match provider_name {
        PROVIDER_ANTHROPIC => model
            .strip_prefix("anthropic/")
            .unwrap_or(model)
            .to_string(),
        PROVIDER_OPENAI => model.strip_prefix("openai/").unwrap_or(model).to_string(),
        PROVIDER_OPENROUTER => normalize_openrouter_model(model),
        _ => model.to_string(),
    }
}

fn normalize_openrouter_model(model: &str) -> String {
    if model.contains('/') {
        return model.to_string();
    }

    if model.starts_with("claude-") {
        return format!("anthropic/{model}");
    }

    if is_openai_model(model) {
        return format!("openai/{model}");
    }

    model.to_string()
}

fn is_openai_model(model: &str) -> bool {
    model.starts_with("gpt-")
        || model.starts_with("chatgpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
}

fn validate_provider_name(provider_name: &str) -> Result<()> {
    if matches_provider_name(provider_name) {
        return Ok(());
    }

    Err(MoaError::ConfigError(format!(
        "unsupported provider '{provider_name}'"
    )))
}

fn matches_provider_name(provider_name: &str) -> bool {
    matches!(
        provider_name,
        PROVIDER_ANTHROPIC | PROVIDER_OPENAI | PROVIDER_OPENROUTER
    )
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use super::{
        PROVIDER_ANTHROPIC, PROVIDER_OPENAI, PROVIDER_OPENROUTER, resolve_provider_selection,
    };

    #[test]
    fn infers_openai_for_gpt_models() {
        let selection = resolve_provider_selection(&MoaConfig::default(), Some("gpt-5.4")).unwrap();
        assert_eq!(selection.provider_name, PROVIDER_OPENAI);
        assert_eq!(selection.model_id, "gpt-5.4");
    }

    #[test]
    fn infers_anthropic_for_claude_models() {
        let selection =
            resolve_provider_selection(&MoaConfig::default(), Some("claude-sonnet-4-6")).unwrap();
        assert_eq!(selection.provider_name, PROVIDER_ANTHROPIC);
    }

    #[test]
    fn infers_openrouter_for_vendor_prefixed_models() {
        let selection =
            resolve_provider_selection(&MoaConfig::default(), Some("openai/gpt-5.4")).unwrap();
        assert_eq!(selection.provider_name, PROVIDER_OPENROUTER);
    }

    #[test]
    fn explicit_provider_prefix_overrides_inference() {
        let selection =
            resolve_provider_selection(&MoaConfig::default(), Some("openrouter:gpt-5.4")).unwrap();
        assert_eq!(selection.provider_name, PROVIDER_OPENROUTER);
        assert_eq!(selection.model_id, "openai/gpt-5.4");
    }
}
