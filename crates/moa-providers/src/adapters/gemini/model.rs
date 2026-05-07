//! Gemini model aliases and capability metadata.

use super::tools::native_google_search_tools;
use super::*;

pub(super) fn canonical_model_id(model: &str) -> Result<String> {
    let model = model.trim();
    if model.starts_with("gemini-") {
        return Ok(model.to_string());
    }

    Err(MoaError::Unsupported(format!(
        "unsupported Google Gemini model '{model}'"
    )))
}

pub(super) fn capabilities_for_model(model: &str) -> ModelCapabilities {
    if model.starts_with("gemini-3.1-pro") {
        return ModelCapabilities {
            model_id: ModelId::new(model),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 2.0,
                output_per_mtok: 12.0,
                cached_input_per_mtok: Some(0.2),
            },
            native_tools: native_google_search_tools(),
        };
    }

    if model.starts_with("gemini-3.1-flash-lite") {
        return ModelCapabilities {
            model_id: ModelId::new(model),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 0.25,
                output_per_mtok: 1.5,
                cached_input_per_mtok: Some(0.025),
            },
            native_tools: native_google_search_tools(),
        };
    }

    if model.starts_with("gemini-3-flash") || model.starts_with("gemini-3.1-flash") {
        return ModelCapabilities {
            model_id: ModelId::new(model),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 0.5,
                output_per_mtok: 3.0,
                cached_input_per_mtok: Some(0.05),
            },
            native_tools: native_google_search_tools(),
        };
    }

    if model.starts_with("gemini-2.5-pro") {
        return ModelCapabilities {
            model_id: ModelId::new(model),
            context_window: 1_000_000,
            max_output: 65_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 1.25,
                output_per_mtok: 10.0,
                cached_input_per_mtok: Some(0.125),
            },
            native_tools: native_google_search_tools(),
        };
    }

    if model.starts_with("gemini-2.5-flash") {
        return ModelCapabilities {
            model_id: ModelId::new(model),
            context_window: 1_000_000,
            max_output: 65_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 0.3,
                output_per_mtok: 2.5,
                cached_input_per_mtok: Some(0.03),
            },
            native_tools: native_google_search_tools(),
        };
    }

    ModelCapabilities {
        model_id: ModelId::new(model),
        context_window: 1_000_000,
        max_output: 65_000,
        supports_tools: true,
        supports_vision: true,
        supports_prefix_caching: true,
        cache_ttl: None,
        tool_call_format: ToolCallFormat::Gemini,
        pricing: TokenPricing {
            input_per_mtok: 0.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: None,
        },
        native_tools: native_google_search_tools(),
    }
}
