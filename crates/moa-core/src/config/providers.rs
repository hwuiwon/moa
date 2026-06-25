//! Provider and model routing configuration.

use serde::{Deserialize, Serialize};

/// General runtime settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Default provider key.
    pub default_provider: String,
    /// Requested reasoning effort.
    pub reasoning_effort: String,
    /// Whether provider-native web search should be offered to supported models.
    pub web_search_enabled: bool,
    /// Optional repository-workspace instructions injected into the prompt.
    pub workspace_instructions: Option<String>,
    /// Optional user-level preferences injected into the prompt.
    pub user_instructions: Option<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".to_string(),
            reasoning_effort: "medium".to_string(),
            web_search_enabled: true,
            workspace_instructions: None,
            user_instructions: None,
        }
    }
}

/// Tiered model-routing settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// Default model for the primary user-facing agent loop.
    pub main: String,
    /// Optional lower-cost model for auxiliary tasks.
    pub auxiliary: Option<String>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            main: "gpt-5.4".to_string(),
            auxiliary: None,
        }
    }
}

/// Provider credential environment mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderCredentialConfig {
    /// Environment variable containing the API key.
    pub api_key_env: String,
}

impl ProviderCredentialConfig {
    /// Creates a provider credential config with a single environment variable name.
    pub fn new(api_key_env: impl Into<String>) -> Self {
        Self {
            api_key_env: api_key_env.into(),
        }
    }
}

impl Default for ProviderCredentialConfig {
    fn default() -> Self {
        Self::new("")
    }
}

/// Provider-specific configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    /// Anthropic credentials.
    pub anthropic: ProviderCredentialConfig,
    /// `OpenAI` credentials.
    pub openai: ProviderCredentialConfig,
    /// Google Gemini credentials.
    pub google: ProviderCredentialConfig,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            anthropic: ProviderCredentialConfig::new("ANTHROPIC_API_KEY"),
            openai: ProviderCredentialConfig::new("OPENAI_API_KEY"),
            google: ProviderCredentialConfig::new("GOOGLE_API_KEY"),
        }
    }
}
