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
    /// Minimum tool-call count required before a segment can be considered.
    pub min_tool_calls: usize,
}

impl Default for SkillLearningConfig {
    fn default() -> Self {
        Self { min_tool_calls: 5 }
    }
}
