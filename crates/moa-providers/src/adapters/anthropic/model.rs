//! Anthropic model aliases and capability metadata.

use super::tools::native_web_search_tools;
use super::*;

pub(super) fn canonical_model_id(model: &str) -> Result<String> {
    match model {
        MODEL_HAIKU_4_5 => Ok(MODEL_HAIKU_4_5.to_string()),
        MODEL_OPUS_4_6 => Ok(MODEL_OPUS_4_6.to_string()),
        MODEL_SONNET_4_6 => Ok(MODEL_SONNET_4_6.to_string()),
        unsupported => Err(MoaError::Unsupported(format!(
            "unsupported Anthropic model '{unsupported}'"
        ))),
    }
}

pub(super) fn capabilities_for_model(model: &str) -> Result<ModelCapabilities> {
    match model {
        MODEL_HAIKU_4_5 => Ok(ModelCapabilities {
            model_id: ModelId::new(MODEL_HAIKU_4_5),
            context_window: 200_000,
            max_output: 16_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: Some(Duration::from_secs(300)),
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 0.8,
                output_per_mtok: 4.0,
                cached_input_per_mtok: Some(0.08),
                cache_write_5m_per_mtok: Some(1.0),
                cache_write_1h_per_mtok: Some(1.6),
            },
            native_tools: native_web_search_tools(),
        }),
        MODEL_OPUS_4_6 => Ok(ModelCapabilities {
            model_id: ModelId::new(MODEL_OPUS_4_6),
            context_window: 1_000_000,
            max_output: 128_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: Some(Duration::from_secs(300)),
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 5.0,
                output_per_mtok: 25.0,
                cached_input_per_mtok: Some(0.5),
                cache_write_5m_per_mtok: Some(6.25),
                cache_write_1h_per_mtok: Some(10.0),
            },
            native_tools: native_web_search_tools(),
        }),
        MODEL_SONNET_4_6 => Ok(ModelCapabilities {
            model_id: ModelId::new(MODEL_SONNET_4_6),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: Some(Duration::from_secs(300)),
            tool_call_format: ToolCallFormat::Anthropic,
            pricing: TokenPricing {
                input_per_mtok: 3.0,
                output_per_mtok: 15.0,
                cached_input_per_mtok: Some(0.3),
                cache_write_5m_per_mtok: Some(3.75),
                cache_write_1h_per_mtok: Some(6.0),
            },
            native_tools: native_web_search_tools(),
        }),
        unsupported => Err(MoaError::Unsupported(format!(
            "unsupported Anthropic model '{unsupported}'"
        ))),
    }
}
