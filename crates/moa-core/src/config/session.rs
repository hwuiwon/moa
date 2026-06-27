//! Session storage configuration.

use serde::{Deserialize, Serialize};

/// Session storage configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Offload threshold in bytes for large event payload strings.
    pub blob_threshold_bytes: usize,
    /// Root directory for local blob storage.
    pub blob_dir: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            blob_threshold_bytes: 65_536,
            blob_dir: "~/.moa/blobs".to_string(),
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies session storage environment overrides.
    pub(in crate::config) fn apply_session_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_if_some};

        set_copy_if_some(
            &mut config.session.blob_threshold_bytes,
            self.session_blob_threshold_bytes,
        );
        set_if_some(&mut config.session.blob_dir, &self.session_blob_dir);
    }
}
