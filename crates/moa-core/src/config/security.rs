//! Permission posture configuration.

use crate::ActionPolicyEffect;
use serde::{Deserialize, Serialize};

/// Permission posture configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    /// Default effect when neither persisted rules nor tool-specific config match.
    pub default_effect: ActionPolicyEffect,
    /// Tools that require tenant-admin review.
    pub admin_review: Vec<String>,
    /// Tools always denied.
    pub always_deny: Vec<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            default_effect: ActionPolicyEffect::Allow,
            admin_review: Vec::new(),
            always_deny: Vec::new(),
        }
    }
}
