//! Model-routing task and tier enums shared across MOA crates.

use serde::{Deserialize, Serialize};

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
    /// Delegated subagent work.
    Subagent,
}

/// Stable high-level pricing tier used for analytics and event attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
            Self::Summarization
            | Self::Consolidation
            | Self::SkillDistillation
            | Self::Subagent => ModelTier::Auxiliary,
        }
    }
}

impl ModelTier {
    /// Returns the stable string form used in JSON payloads and analytics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Auxiliary => "auxiliary",
        }
    }
}
