//! Provider-selection helpers for MOA runtime wiring.

use std::sync::Arc;

use moa_core::{LLMProvider, MoaConfig, MoaError, ModelId, Result};

use crate::ProviderRegistry;
use crate::routing::{
    ProviderId, infer_provider_id, provider_descriptor_by_name, split_explicit_provider,
};

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
        .unwrap_or(config.models.main.as_str());
    let default_provider = config.general.default_provider.trim();

    if requested.contains('/') {
        return Err(MoaError::ConfigError(
            "vendor-prefixed model ids are not supported; use direct model ids for anthropic, openai, or google".to_string(),
        ));
    }

    if let Some((provider_id, model_id)) = split_explicit_provider(requested) {
        let provider_name = provider_id.as_str();
        return Ok(ProviderSelection {
            provider_name: provider_name.to_string(),
            model_id: normalize_model_for_provider(provider_name, model_id),
        });
    }

    let provider_name = infer_provider_id(requested)
        .map(ProviderId::as_str)
        .unwrap_or(default_provider);
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
    let provider_id = selection.provider_name.parse::<ProviderId>()?;
    ProviderRegistry::from_config(config)
        .provider_for_id(provider_id, &ModelId::new(selection.model_id.clone()))
        .map(|resolved| resolved.provider)
}

/// Builds the configured query-rewriter provider, preferring explicit and auxiliary models.
pub fn resolve_rewriter_provider(config: &MoaConfig) -> Result<Arc<dyn LLMProvider>> {
    let mut query_rewrite = config.query_rewrite.clone();
    query_rewrite.enabled = true;
    if query_rewrite.model.is_none() {
        query_rewrite.model = config.models.auxiliary.clone();
    }

    ProviderRegistry::from_config(config)
        .resolve_rewriter_provider(&query_rewrite)?
        .ok_or_else(|| {
            MoaError::ConfigError("query rewriter provider is not configured".to_string())
        })
}

fn normalize_model_for_provider(provider_name: &str, model: &str) -> String {
    let _provider_name = provider_name;
    model.trim().to_string()
}

fn validate_provider_name(provider_name: &str) -> Result<()> {
    if provider_descriptor_by_name(provider_name).is_some() {
        return Ok(());
    }

    Err(MoaError::ConfigError(format!(
        "unsupported provider '{provider_name}'"
    )))
}

#[cfg(test)]
mod tests {
    use moa_core::MoaConfig;

    use crate::core::models::{PROVIDER_ANTHROPIC, PROVIDER_GOOGLE, PROVIDER_OPENAI};

    use super::resolve_provider_selection;

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
            resolve_provider_selection(&MoaConfig::default(), Some("gemini-3-flash-preview"))
                .unwrap();
        assert_eq!(selection.provider_name, PROVIDER_GOOGLE);
    }

    #[test]
    fn explicit_provider_prefix_overrides_inference() {
        let selection = resolve_provider_selection(
            &MoaConfig::default(),
            Some("google:gemini-3-flash-preview"),
        )
        .unwrap();
        assert_eq!(selection.provider_name, PROVIDER_GOOGLE);
        assert_eq!(selection.model_id, "gemini-3-flash-preview");
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
}
