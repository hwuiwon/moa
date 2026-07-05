//! Provider-selection helpers for MOA runtime wiring.

use std::sync::Arc;

use moa_core::{LLMProvider, MoaConfig, MoaError, ModelId, Result};

use crate::ProviderRegistry;
use crate::routing::ProviderId;

/// Resolves the effective provider and model from config plus an optional user override.
pub fn resolve_provider_selection(
    config: &MoaConfig,
    model_override: Option<&str>,
) -> Result<(ProviderId, ModelId)> {
    ProviderRegistry::resolve_selection_from_config(config, model_override)
}

/// Builds the configured provider using the config's effective default provider/model pair.
///
/// The result is wrapped with the configured LLM failover chain
/// (`models.fallback_models`), matching the main-loop router path.
pub fn build_provider_from_config(config: &MoaConfig) -> Result<Arc<dyn LLMProvider>> {
    let registry = ProviderRegistry::from_config(config);
    let (provider_id, model_id) = resolve_provider_selection(config, None)?;
    let primary = registry.provider_for_id(provider_id, &model_id)?.provider;
    registry.apply_main_failover(config, model_id.as_str(), primary)
}

/// Builds a provider for an explicit model override and returns the canonical model id.
///
/// The result is wrapped with the configured LLM failover chain
/// (`models.fallback_models`), matching the main-loop router path while letting
/// auxiliary model users keep their own model selector.
pub fn build_provider_from_model(
    config: &MoaConfig,
    model_override: Option<&str>,
) -> Result<(Arc<dyn LLMProvider>, ModelId)> {
    let registry = ProviderRegistry::from_config(config);
    let (provider_id, model_id) = resolve_provider_selection(config, model_override)?;
    let primary = registry.provider_for_id(provider_id, &model_id)?.provider;
    let provider = registry.apply_main_failover(config, model_id.as_str(), primary)?;
    Ok((provider, model_id))
}

/// Builds one provider instance from an explicit provider/model selection.
pub fn build_provider_from_selection(
    config: &MoaConfig,
    provider_id: ProviderId,
    model_id: &ModelId,
) -> Result<Arc<dyn LLMProvider>> {
    ProviderRegistry::from_config(config)
        .provider_for_id(provider_id, model_id)
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
        let (provider_id, model_id) =
            resolve_provider_selection(&MoaConfig::default(), Some("gpt-5.4")).unwrap();
        assert_eq!(provider_id.as_str(), PROVIDER_OPENAI);
        assert_eq!(model_id.as_str(), "gpt-5.4");
    }

    #[test]
    fn infers_anthropic_for_claude_models() {
        let (provider_id, _) =
            resolve_provider_selection(&MoaConfig::default(), Some("claude-sonnet-4-6")).unwrap();
        assert_eq!(provider_id.as_str(), PROVIDER_ANTHROPIC);
    }

    #[test]
    fn infers_google_for_gemini_models() {
        let (provider_id, _) =
            resolve_provider_selection(&MoaConfig::default(), Some("gemini-3-flash-preview"))
                .unwrap();
        assert_eq!(provider_id.as_str(), PROVIDER_GOOGLE);
    }

    #[test]
    fn explicit_provider_prefix_overrides_inference() {
        let (provider_id, model_id) = resolve_provider_selection(
            &MoaConfig::default(),
            Some("google:gemini-3-flash-preview"),
        )
        .unwrap();
        assert_eq!(provider_id.as_str(), PROVIDER_GOOGLE);
        assert_eq!(model_id.as_str(), "gemini-3-flash-preview");
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
