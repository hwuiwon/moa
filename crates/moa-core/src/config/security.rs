//! Permission posture configuration.

use serde::{Deserialize, Serialize};

/// Permission posture configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    /// Default posture for approvals.
    pub default_posture: String,
    /// Tools approved automatically.
    pub auto_approve: Vec<String>,
    /// Tools always denied.
    pub always_deny: Vec<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            default_posture: "approve".to_string(),
            auto_approve: vec!["file_read".to_string(), "file_search".to_string()],
            always_deny: Vec::new(),
        }
    }
}
