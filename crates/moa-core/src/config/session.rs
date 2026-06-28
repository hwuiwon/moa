//! Session storage configuration.

use crate::{MoaError, Result};
use serde::{Deserialize, Serialize};

/// Supported claim-check blob storage backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBlobBackend {
    /// Store claim-check payloads on the local filesystem.
    Local,
    /// Store claim-check payloads in Postgres.
    #[default]
    Postgres,
    /// Store claim-check payloads in an object store.
    ObjectStore,
}

/// Session storage configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Offload threshold in bytes for large event payload strings.
    pub blob_threshold_bytes: usize,
    /// Backend used for claim-check blob payloads.
    pub blob_backend: SessionBlobBackend,
    /// Root directory for local blob storage.
    pub blob_dir: Option<String>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            blob_threshold_bytes: 65_536,
            blob_backend: SessionBlobBackend::Postgres,
            blob_dir: None,
        }
    }
}

impl SessionConfig {
    /// Validates whether the configured claim-check blob backend is allowed for the runtime mode.
    pub fn validate_blob_backend(&self, cloud_enabled: bool) -> Result<()> {
        if !matches!(self.blob_backend, SessionBlobBackend::Local) || !cloud_enabled {
            return Ok(());
        }

        let has_explicit_path = self
            .blob_dir
            .as_deref()
            .map(str::trim)
            .is_some_and(|path| !path.is_empty() && path != ":memory:");
        if has_explicit_path {
            return Ok(());
        }

        Err(MoaError::ConfigError(
            "session.blob_backend = local requires session.blob_dir to be an explicit persistent path when cloud.enabled = true; use session.blob_backend = postgres for durable cloud claim-check payloads".to_string(),
        ))
    }
}

impl super::MoaEnvOverlay {
    /// Applies session storage environment overrides.
    pub(in crate::config) fn apply_session_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_option_if_some};

        set_copy_if_some(
            &mut config.session.blob_threshold_bytes,
            self.session_blob_threshold_bytes,
        );
        set_copy_if_some(&mut config.session.blob_backend, self.session_blob_backend);
        set_option_if_some(&mut config.session.blob_dir, &self.session_blob_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_local_blob_backend_without_path_fails_clearly() {
        // Pins: cloud startup cannot silently claim-check events to pod-local files.
        let config = SessionConfig {
            blob_backend: SessionBlobBackend::Local,
            blob_dir: None,
            ..SessionConfig::default()
        };

        let error = config
            .validate_blob_backend(true)
            .expect_err("cloud local blob storage without a path should fail");

        assert_eq!(
            error.to_string(),
            "configuration error: session.blob_backend = local requires session.blob_dir to be an explicit persistent path when cloud.enabled = true; use session.blob_backend = postgres for durable cloud claim-check payloads"
        );
    }

    #[test]
    fn cloud_local_blob_backend_with_explicit_path_is_allowed() {
        // Pins: explicit persistent paths keep the local backend available for controlled deployments.
        let config = SessionConfig {
            blob_backend: SessionBlobBackend::Local,
            blob_dir: Some("/var/lib/moa/blobs".to_string()),
            ..SessionConfig::default()
        };

        config
            .validate_blob_backend(true)
            .expect("cloud local blob storage with an explicit path should be allowed");
    }

    #[test]
    fn local_development_local_blob_backend_can_use_default_path() {
        // Pins: local development can still opt into the filesystem blob backend.
        let config = SessionConfig {
            blob_backend: SessionBlobBackend::Local,
            blob_dir: None,
            ..SessionConfig::default()
        };

        config
            .validate_blob_backend(false)
            .expect("local filesystem blob storage should remain available outside cloud mode");
    }
}
