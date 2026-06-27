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

impl super::MoaEnvOverlay {
    /// Applies permission-policy environment overrides.
    pub(in crate::config) fn apply_permissions_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_vec_if_some};

        set_copy_if_some(
            &mut config.permissions.default_effect,
            self.permissions_default_effect,
        );
        set_vec_if_some(
            &mut config.permissions.admin_review,
            &self.permissions_admin_review,
        );
        set_vec_if_some(
            &mut config.permissions.always_deny,
            &self.permissions_always_deny,
        );
    }
}
