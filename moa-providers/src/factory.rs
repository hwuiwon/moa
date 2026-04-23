//! Provider-selection helpers for MOA runtime wiring.

use std::sync::Arc;

use moa_core::{LLMProvider, MoaConfig, MoaError, Result};

use crate::{AnthropicProvider, GeminiProvider, OpenAIProvider};

const PROVIDER_ANTHROPIC: &str = "anthropic";
const PROVIDER_OPENAI: &str = "openai";
const PROVIDER_GOOGLE: &str = "google";
const REWRITER_ANTHROPIC_MODEL: &str = "claude-haiku-4-5";
const REWRITER_OPENAI_MODEL: &str = "gpt-5.4-mini";
const REWRITER_GOOGLE_MODEL: &str = "gemini-3.1-flash-lite-preview";

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

    if requested.contains('/') {
        return Err(MoaError::ConfigError(
            "vendor-prefixed model ids are not supported; use direct model ids for anthropic, openai, or google".to_string(),
        ));
    }

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
        PROVIDER_GOOGLE => Arc::new(GeminiProvider::from_config_with_model(
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

/// Builds the configured query-rewriter provider, preferring explicit and auxiliary models.
pub fn resolve_rewriter_provider(config: &MoaConfig) -> Result<Arc<dyn LLMProvider>> {
    let model = config
        .query_rewrite
        .model
        .as_deref()
        .or(config.models.auxiliary.as_deref())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| default_rewriter_model(config));
    let selection = resolve_provider_selection(config, Some(&model))?;
    build_provider_from_selection(config, &selection)
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
    if model.starts_with("claude-") {
        return Some(PROVIDER_ANTHROPIC);
    }

    if model.starts_with("gemini-") {
        return Some(PROVIDER_GOOGLE);
    }

    if is_openai_model(model) {
        return Some(PROVIDER_OPENAI);
    }

    None
}

fn default_rewriter_model(config: &MoaConfig) -> String {
    match config.general.default_provider.trim() {
        PROVIDER_ANTHROPIC => REWRITER_ANTHROPIC_MODEL.to_string(),
        PROVIDER_GOOGLE => REWRITER_GOOGLE_MODEL.to_string(),
        PROVIDER_OPENAI => REWRITER_OPENAI_MODEL.to_string(),
        _ => match infer_provider_name(config.models.main.as_str()) {
            Some(PROVIDER_ANTHROPIC) => REWRITER_ANTHROPIC_MODEL.to_string(),
            Some(PROVIDER_GOOGLE) => REWRITER_GOOGLE_MODEL.to_string(),
            _ => REWRITER_OPENAI_MODEL.to_string(),
        },
    }
}

fn normalize_model_for_provider(provider_name: &str, model: &str) -> String {
    let _provider_name = provider_name;
    model.trim().to_string()
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
        PROVIDER_ANTHROPIC | PROVIDER_OPENAI | PROVIDER_GOOGLE
    )
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use super::{
        PROVIDER_ANTHROPIC, PROVIDER_GOOGLE, PROVIDER_OPENAI, default_rewriter_model,
        resolve_provider_selection,
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
    fn infers_google_for_gemini_models() {
        let selection =
            resolve_provider_selection(&MoaConfig::default(), Some("gemini-2.5-flash")).unwrap();
        assert_eq!(selection.provider_name, PROVIDER_GOOGLE);
    }

    #[test]
    fn explicit_provider_prefix_overrides_inference() {
        let selection =
            resolve_provider_selection(&MoaConfig::default(), Some("google:gemini-2.5-flash"))
                .unwrap();
        assert_eq!(selection.provider_name, PROVIDER_GOOGLE);
        assert_eq!(selection.model_id, "gemini-2.5-flash");
    }

    #[test]
    fn rejects_vendor_prefixed_model_ids() {
        let error = resolve_provider_selection(&MoaConfig::default(), Some("openai/gpt-5.4"))
            .expect_err("vendor-prefixed model ids should be rejected");
        assert!(
            error
                .to_string()
                .contains("vendor-prefixed model ids are not supported")
        );
    }

    #[test]
    fn default_rewriter_model_prefers_provider_family_small_model() {
        let mut config = MoaConfig::default();
        config.general.default_provider = "anthropic".to_string();
        assert_eq!(default_rewriter_model(&config), "claude-haiku-4-5");

        config.general.default_provider = "google".to_string();
        assert_eq!(
            default_rewriter_model(&config),
            "gemini-3.1-flash-lite-preview"
        );

        config.general.default_provider = "openai".to_string();
        assert_eq!(default_rewriter_model(&config), "gpt-5.4-mini");
    }
}
