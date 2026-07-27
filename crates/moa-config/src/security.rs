//! Permission posture configuration.

use moa_core::types::action_policy::ActionPolicyEffect;
use serde::{Deserialize, Serialize};

/// Deployment security posture selected explicitly by the operator.
///
/// The profile is the single switch that decides whether a deployment may run
/// tools on the local host with permissive defaults, or must fail closed onto a
/// real cloud sandbox with an explicit tenant grant for every action. It is
/// never inferred from the presence of other configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecurityProfile {
    /// Development posture: local hands are permitted and permissive permission
    /// defaults are allowed.
    #[default]
    Local,
    /// Production posture: tool execution requires a deny-by-default permission
    /// posture, a persistent rule owner, and a non-local sandbox backend with
    /// present credentials.
    Cloud,
}

impl SecurityProfile {
    /// Returns the stable wire/manifest name for this profile.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Cloud => "cloud",
        }
    }

    /// Returns whether this profile requires the fail-closed cloud contract.
    #[must_use]
    pub fn is_cloud(self) -> bool {
        matches!(self, Self::Cloud)
    }
}

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
