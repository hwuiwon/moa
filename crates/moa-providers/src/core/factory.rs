//! Provider-selection helpers for MOA runtime wiring.

use std::sync::Arc;

use moa_core::{LLMProvider, MoaConfig, MoaError, ModelId, Result};

use crate::ProviderRegistry;
use crate::routing::ProviderId;

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
    let (provider_id, model_id) =
        ProviderRegistry::resolve_selection_from_config(config, model_override)?;

    Ok(ProviderSelection {
        provider_name: provider_id.as_str().to_string(),
        model_id: model_id.as_str().to_string(),
    })
}

/// Builds the configured provider using the config's effective default provider/model pair.
///
/// The result is wrapped with the configured LLM failover chain
/// (`models.fallback_models`), matching the main-loop router path.
pub fn build_provider_from_config(config: &MoaConfig) -> Result<Arc<dyn LLMProvider>> {
    let selection = resolve_provider_selection(config, None)?;
    let primary = build_provider_from_selection(config, &selection)?;
    ProviderRegistry::from_config(config).apply_main_failover(config, &selection.model_id, primary)
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
