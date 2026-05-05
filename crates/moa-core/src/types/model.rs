//! Model capability, pricing, and credential types.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ModelId;

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
    pub cached_input_per_mtok: Option<f64>,
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
            },
            native_tools: Vec::new(),
        }
    }
}

impl ModelCapabilities {
    /// Returns a builder for constructing a [`ModelCapabilities`] value.
    #[must_use]
    pub fn builder() -> ModelCapabilitiesBuilder {
        ModelCapabilitiesBuilder::default()
    }
}

/// Builder for [`ModelCapabilities`].
#[derive(Default)]
pub struct ModelCapabilitiesBuilder {
    model_id: Option<ModelId>,
    context_window: Option<usize>,
    max_output: Option<usize>,
    supports_tools: Option<bool>,
    supports_vision: Option<bool>,
    supports_prefix_caching: Option<bool>,
    cache_ttl: Option<Duration>,
    tool_call_format: Option<ToolCallFormat>,
    pricing: Option<TokenPricing>,
    native_tools: Option<Vec<ProviderNativeTool>>,
}

impl ModelCapabilitiesBuilder {
    /// Sets the model identifier.
    #[must_use]
    pub fn model_id(mut self, v: impl Into<ModelId>) -> Self {
        self.model_id = Some(v.into());
        self
    }

    /// Sets the maximum prompt context window.
    #[must_use]
    pub fn context_window(mut self, v: usize) -> Self {
        self.context_window = Some(v);
        self
    }

    /// Sets the maximum output tokens.
    #[must_use]
    pub fn max_output(mut self, v: usize) -> Self {
        self.max_output = Some(v);
        self
    }

    /// Sets whether the model supports tool use.
    #[must_use]
    pub fn supports_tools(mut self, v: bool) -> Self {
        self.supports_tools = Some(v);
        self
    }

    /// Sets whether the model supports vision inputs.
    #[must_use]
    pub fn supports_vision(mut self, v: bool) -> Self {
        self.supports_vision = Some(v);
        self
    }

    /// Sets whether the provider supports prompt prefix caching.
    #[must_use]
    pub fn supports_prefix_caching(mut self, v: bool) -> Self {
        self.supports_prefix_caching = Some(v);
        self
    }

    /// Sets the prompt cache time-to-live.
    #[must_use]
    pub fn cache_ttl(mut self, v: Option<Duration>) -> Self {
        self.cache_ttl = v;
        self
    }

    /// Sets the tool call encoding style.
    #[must_use]
    pub fn tool_call_format(mut self, v: ToolCallFormat) -> Self {
        self.tool_call_format = Some(v);
        self
    }

    /// Sets the token pricing metadata.
    #[must_use]
    pub fn pricing(mut self, v: TokenPricing) -> Self {
        self.pricing = Some(v);
        self
    }

    /// Sets the provider-native tools list.
    #[must_use]
    pub fn native_tools(mut self, v: Vec<ProviderNativeTool>) -> Self {
        self.native_tools = Some(v);
        self
    }

    /// Constructs the [`ModelCapabilities`], using defaults for any unset fields.
    pub fn build(self) -> ModelCapabilities {
        let defaults = ModelCapabilities::default();
        ModelCapabilities {
            model_id: self.model_id.unwrap_or(defaults.model_id),
            context_window: self.context_window.unwrap_or(defaults.context_window),
            max_output: self.max_output.unwrap_or(defaults.max_output),
            supports_tools: self.supports_tools.unwrap_or(defaults.supports_tools),
            supports_vision: self.supports_vision.unwrap_or(defaults.supports_vision),
            supports_prefix_caching: self
                .supports_prefix_caching
                .unwrap_or(defaults.supports_prefix_caching),
            cache_ttl: self.cache_ttl,
            tool_call_format: self.tool_call_format.unwrap_or(defaults.tool_call_format),
            pricing: self.pricing.unwrap_or(defaults.pricing),
            native_tools: self.native_tools.unwrap_or(defaults.native_tools),
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
