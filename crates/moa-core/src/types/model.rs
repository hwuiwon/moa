//! Model capability, pricing, and credential types.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::identifiers::ModelId;

/// Provider-specific tool call encoding.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallFormat {
    /// Anthropic tool use blocks.
    Anthropic,
    /// OpenAI-compatible tool calls.
    OpenAiCompatible,
    /// Gemini function call and function response parts.
    Gemini,
}

/// Provider token pricing metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenPricing {
    /// Input token price per million tokens.
    pub input_per_mtok: f64,
    /// Output token price per million tokens.
    pub output_per_mtok: f64,
    /// Cached input token price per million tokens.
    #[serde(default)]
    pub cached_input_per_mtok: Option<f64>,
    /// Five-minute prompt cache creation price per million tokens.
    #[serde(default)]
    pub cache_write_5m_per_mtok: Option<f64>,
    /// One-hour prompt cache creation price per million tokens.
    #[serde(default)]
    pub cache_write_1h_per_mtok: Option<f64>,
}

impl TokenPricing {
    /// Returns the default prompt cache creation rate per million tokens.
    #[must_use]
    pub fn cache_write_per_mtok(&self) -> f64 {
        self.cache_write_5m_per_mtok.unwrap_or(self.input_per_mtok)
    }

    /// Returns the completion cost in whole US dollars for the given token usage.
    ///
    /// This is the single canonical cost formula shared by the orchestrator LLM
    /// gateway and the brain harness. Uncached input, cache-write, cache-read,
    /// and output tokens are each priced against their own per-million rate, so
    /// cache-write tokens carry the creation premium rather than the standard
    /// input rate. Callers scale and round into cents or micros via
    /// [`TokenPricing::cost_cents`] / [`TokenPricing::cost_micros`].
    #[must_use]
    pub fn cost_dollars(&self, usage: &super::completion::TokenUsage) -> f64 {
        let input_cost = usage.input_tokens_uncached as f64 / 1_000_000.0 * self.input_per_mtok;
        let cache_write_cost =
            usage.input_tokens_cache_write as f64 / 1_000_000.0 * self.cache_write_per_mtok();
        let cache_read_cost = usage.input_tokens_cache_read as f64 / 1_000_000.0
            * self.cached_input_per_mtok.unwrap_or(self.input_per_mtok);
        let output_cost = usage.output_tokens as f64 / 1_000_000.0 * self.output_per_mtok;
        input_cost + cache_write_cost + cache_read_cost + output_cost
    }

    /// Returns the completion cost in whole cents, rounding to the nearest cent.
    #[must_use]
    pub fn cost_cents(&self, usage: &super::completion::TokenUsage) -> u32 {
        (self.cost_dollars(usage) * 100.0).round() as u32
    }

    /// Returns the completion cost in micros of USD (1 USD = 1_000_000 micros).
    ///
    /// Unlike [`TokenPricing::cost_cents`], this preserves sub-cent precision so
    /// lineage records the true cost of small turns instead of rounding to zero.
    #[must_use]
    pub fn cost_micros(&self, usage: &super::completion::TokenUsage) -> u64 {
        (self.cost_dollars(usage) * 1_000_000.0).round() as u64
    }
}

/// One tool implemented natively by the model provider instead of MOA.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderNativeTool {
    /// Provider-specific tool type identifier.
    pub tool_type: String,
    /// Human-readable tool name.
    pub name: String,
    /// Optional provider-specific configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Value>,
}

/// LLM model capability metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelCapabilities {
    /// Model identifier.
    pub model_id: ModelId,
    /// Maximum prompt context window.
    pub context_window: usize,
    /// Maximum output tokens.
    pub max_output: usize,
    /// Whether the model supports tool use.
    pub supports_tools: bool,
    /// Whether the model supports vision inputs.
    pub supports_vision: bool,
    /// Whether the provider supports prompt prefix caching.
    pub supports_prefix_caching: bool,
    /// Prompt cache time-to-live when known.
    pub cache_ttl: Option<Duration>,
    /// Tool call encoding style.
    pub tool_call_format: ToolCallFormat,
    /// Token pricing metadata.
    pub pricing: TokenPricing,
    /// Provider-native tools that the model can invoke without MOA routing them.
    #[serde(default)]
    pub native_tools: Vec<ProviderNativeTool>,
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            model_id: ModelId::new(""),
            context_window: 0,
            max_output: 0,
            supports_tools: false,
            supports_vision: false,
            supports_prefix_caching: false,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::OpenAiCompatible,
            pricing: TokenPricing {
                input_per_mtok: 0.0,
                output_per_mtok: 0.0,
                cached_input_per_mtok: None,
                cache_write_5m_per_mtok: None,
                cache_write_1h_per_mtok: None,
            },
            native_tools: Vec::new(),
        }
    }
}

/// Stored credential material.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    /// Bearer token.
    Bearer(String),
    /// OAuth credential.
    OAuth {
        /// Access token.
        access_token: String,
        /// Refresh token when available.
        refresh_token: Option<String>,
        /// Expiration timestamp when known.
        expires_at: Option<DateTime<Utc>>,
    },
    /// API key credential.
    ApiKey {
        /// Header name for the key.
        header: String,
        /// Header value.
        value: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::completion::TokenUsage;

    fn sonnet_pricing() -> TokenPricing {
        TokenPricing {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cached_input_per_mtok: Some(0.30),
            cache_write_5m_per_mtok: Some(3.75),
            cache_write_1h_per_mtok: None,
        }
    }

    #[test]
    fn cost_prices_cache_write_at_creation_rate_not_input_rate() {
        // Pins: the single canonical formula prices cache-write tokens at the
        // cache-creation premium (3.75/Mtok), not the standard input rate. This
        // is the behavior the divergent brain copy got wrong before the collapse.
        let usage = TokenUsage {
            input_tokens_uncached: 1_000_000,
            input_tokens_cache_write: 1_000_000,
            input_tokens_cache_read: 1_000_000,
            output_tokens: 1_000_000,
        };
        let pricing = sonnet_pricing();
        // 3.0 (input) + 3.75 (cache-write premium) + 0.30 (cache-read) + 15.0 (output)
        // = 22.05 USD.
        assert!((pricing.cost_dollars(&usage) - 22.05).abs() < 1e-9);
        assert_eq!(pricing.cost_cents(&usage), 2205);
        assert_eq!(pricing.cost_micros(&usage), 22_050_000);
    }

    #[test]
    fn cost_cents_and_micros_derive_from_the_same_dollar_figure() {
        // Pins: cents and micros never disagree — both round the one dollar
        // value, so micros carries sub-cent precision that cents rounds away.
        let usage = TokenUsage {
            input_tokens_uncached: 1_234,
            input_tokens_cache_write: 0,
            input_tokens_cache_read: 5_678,
            output_tokens: 910,
        };
        let pricing = sonnet_pricing();
        let dollars = pricing.cost_dollars(&usage);
        assert_eq!(pricing.cost_cents(&usage), (dollars * 100.0).round() as u32);
        assert_eq!(
            pricing.cost_micros(&usage),
            (dollars * 1_000_000.0).round() as u64
        );
    }
}
