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
