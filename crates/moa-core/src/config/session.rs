//! Session storage and local daemon configuration.

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

/// Local daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Unix socket path used by the daemon control plane.
    pub socket_path: String,
    /// PID file written by the daemon process.
    pub pid_file: String,
    /// Log file written by the daemon process.
    pub log_file: String,
    /// Whether interactive clients should auto-connect when the daemon is running.
    pub auto_connect: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: "~/.moa/daemon/daemon.sock".to_string(),
            pid_file: "~/.moa/daemon/daemon.pid".to_string(),
            log_file: "~/.moa/daemon/daemon.log".to_string(),
            auto_connect: true,
        }
    }
}
