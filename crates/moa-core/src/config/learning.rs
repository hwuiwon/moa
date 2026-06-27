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

impl super::MoaEnvOverlay {
    /// Applies learning-loop environment overrides.
    pub(in crate::config) fn apply_learning_overlay(&self, config: &mut super::MoaConfig) {
        super::env_overlay::set_copy_if_some(
            &mut config.learning.skills.min_tool_calls,
            self.learning_skills_min_tool_calls,
        );
    }
}
