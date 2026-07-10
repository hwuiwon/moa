//! Automated learning configuration.

use serde::{Deserialize, Serialize};

/// Runtime learning-loop configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct LearningConfig {
    /// Skill draft proposal generation controls.
    pub skills: SkillLearningConfig,
}

/// Skill self-learning proposal generation configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillLearningConfig {
    /// Minimum tool-call count a segment must contain before it is eligible for
    /// skill distillation.
    ///
    /// This is a cheap pre-LLM filter: a segment shorter than this cannot hold a
    /// reusable multi-step procedure worth distilling, so it is rejected before
    /// any paid distillation call. Set high enough to exclude trivial
    /// three-to-five-call tasks.
    pub min_tool_calls: usize,
}

impl Default for SkillLearningConfig {
    fn default() -> Self {
        Self { min_tool_calls: 8 }
    }
}
