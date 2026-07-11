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
