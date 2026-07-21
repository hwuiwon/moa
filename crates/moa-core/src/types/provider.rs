//! Provider identifiers and model-routing enums shared across MOA crates.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::MoaError;

/// Stable provider family used in configuration, routing, and telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderId {
    /// OpenAI GPT and o-series models.
    OpenAI,
    /// Anthropic Claude models.
    Anthropic,
    /// Google Gemini models.
    Google,
}

impl ProviderId {
    /// Returns the canonical provider name used in config and telemetry.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAI => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
        }
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for ProviderId {
    type Err = MoaError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "openai" => Ok(Self::OpenAI),
            "anthropic" => Ok(Self::Anthropic),
            "google" => Ok(Self::Google),
            other => Err(MoaError::ConfigError(format!(
                "unsupported provider '{other}'"
            ))),
        }
    }
}

/// Stable logical task categories used for routing LLM work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTask {
    /// The user-facing primary agent loop.
    MainLoop,
    /// Session-history and checkpoint summarization.
    Summarization,
    /// Memory-maintenance and consolidation work.
    Consolidation,
    /// Skill distillation and improvement work.
    SkillDistillation,
    /// Delegated worker work.
    Worker,
}

/// Stable high-level pricing tier used for analytics and event attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum ModelTier {
    /// Frontier/user-facing work.
    Main,
    /// Lower-cost auxiliary work.
    Auxiliary,
}

impl ModelTask {
    /// Returns the pricing tier associated with this model task.
    #[must_use]
    pub fn tier(self) -> ModelTier {
        match self {
            Self::MainLoop => ModelTier::Main,
            Self::Summarization | Self::Consolidation | Self::SkillDistillation | Self::Worker => {
                ModelTier::Auxiliary
            }
        }
    }
}

impl ModelTier {
    /// Returns the stable string form used in JSON payloads and analytics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderId;

    #[test]
    fn provider_id_rejects_unknown_config_values() {
        // Pins: provider allow/deny policy deserialization cannot silently accept
        // an identifier that the routing registry does not own.
        let error = serde_json::from_str::<ProviderId>(r#""not-a-provider""#)
            .expect_err("unknown provider id must fail deserialization");

        assert!(error.to_string().contains("unknown variant"), "{error}");
    }
}
