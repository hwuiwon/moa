//! Session storage configuration.

use moa_core::error::{MoaError, Result};
use serde::{Deserialize, Serialize};

use crate::ObjectStoreLocationConfig;

/// Supported claim-check blob storage backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionBlobBackend {
    /// Store claim-check payloads on the local filesystem.
    Local,
    /// Store claim-check payloads in Postgres.
    #[default]
    Postgres,
}

/// Object storage configuration for user-visible session attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionAttachmentStorageConfig {
    /// Owner-specific bucket and namespace within the shared object store.
    pub storage: ObjectStoreLocationConfig,
}

impl Default for SessionAttachmentStorageConfig {
    fn default() -> Self {
        Self {
            storage: ObjectStoreLocationConfig {
                bucket: "moa-session-attachments".to_string(),
                prefix: "session-attachments".to_string(),
            },
        }
    }
}

impl SessionAttachmentStorageConfig {
    /// Validates attachment object storage.
    pub fn validate(&self) -> Result<()> {
        self.storage.validate("session.attachments.storage")
    }
}

/// Session storage configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Uses named TurnExecution actions for event appends instead of SessionStore RPC.
    /// Kept off until a controlled capacity A/B selects the direct path.
    pub direct_turn_event_append: bool,
    /// Offload threshold in bytes for large event payload strings.
    pub blob_threshold_bytes: usize,
    /// Backend used for claim-check blob payloads.
    pub blob_backend: SessionBlobBackend,
    /// Root directory for local blob storage.
    pub blob_dir: Option<String>,
    /// User-visible attachment object storage settings.
    pub attachments: SessionAttachmentStorageConfig,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            direct_turn_event_append: false,
            blob_threshold_bytes: 65_536,
            blob_backend: SessionBlobBackend::Postgres,
            blob_dir: None,
            attachments: SessionAttachmentStorageConfig::default(),
        }
    }
}

impl SessionConfig {
    /// Validates whether the configured claim-check blob backend is durable enough.
    pub fn validate_blob_backend(&self) -> Result<()> {
        if !matches!(self.blob_backend, SessionBlobBackend::Local) {
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
            "session.blob_backend = local requires session.blob_dir to be an explicit persistent path; use session.blob_backend = postgres for durable claim-check payloads".to_string(),
        ))
    }

    /// Validates session storage configuration.
    pub fn validate(&self) -> Result<()> {
        self.validate_blob_backend()?;
        self.attachments.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_turn_event_append_is_an_explicit_ab_switch() {
        // Pins: the replay-sensitive direct path is never enabled implicitly;
        // capacity campaigns opt into it deliberately for like-for-like A/B.
        assert!(!SessionConfig::default().direct_turn_event_append);
    }

    #[test]
    fn local_blob_backend_without_path_fails_clearly() {
        // Pins: startup cannot silently claim-check events to pod-local default files.
        let config = SessionConfig {
            blob_backend: SessionBlobBackend::Local,
            blob_dir: None,
            ..SessionConfig::default()
        };

        let error = config
            .validate_blob_backend()
            .expect_err("local blob storage without a path should fail");

        assert_eq!(
            error.to_string(),
            "configuration error: session.blob_backend = local requires session.blob_dir to be an explicit persistent path; use session.blob_backend = postgres for durable claim-check payloads"
        );
    }

    #[test]
    fn local_blob_backend_with_explicit_path_is_allowed() {
        // Pins: explicit persistent paths keep the local backend available for controlled deployments.
        let config = SessionConfig {
            blob_backend: SessionBlobBackend::Local,
            blob_dir: Some("/var/lib/moa/blobs".to_string()),
            ..SessionConfig::default()
        };

        config
            .validate_blob_backend()
            .expect("local blob storage with an explicit path should be allowed");
    }

    #[test]
    fn default_attachment_storage_is_cloud_safe_s3() {
        // Pins: config defaults do not point cloud deployments at a pod-local endpoint.
        let config = SessionConfig::default();

        assert_eq!(config.attachments.storage.bucket, "moa-session-attachments");
        assert_eq!(config.attachments.storage.prefix, "session-attachments");
        config
            .validate()
            .expect("default S3 attachment settings should be valid");
    }

    #[test]
    fn local_rustfs_attachment_storage_is_explicit() {
        // Pins: attachment config owns only its bucket namespace; transport is shared.
        let config = SessionAttachmentStorageConfig::default();

        assert_eq!(config.storage.bucket, "moa-session-attachments");
        assert_eq!(config.storage.prefix, "session-attachments");
        config
            .validate()
            .expect("attachment namespace should be valid");
    }
}
