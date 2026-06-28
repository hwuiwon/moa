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

/// Supported object stores for user-visible session attachments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAttachmentBackend {
    /// Store attachment bytes in an S3-compatible object store.
    #[default]
    S3,
    /// Store attachment bytes in Google Cloud Storage.
    Gcs,
}

/// Object storage configuration for user-visible session attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionAttachmentStorageConfig {
    /// Object store backend.
    pub backend: SessionAttachmentBackend,
    /// Bucket that stores attachment objects.
    pub bucket: String,
    /// Prefix used for all MOA attachment objects in the bucket.
    pub prefix: String,
    /// AWS/S3-compatible region.
    pub region: Option<String>,
    /// Optional S3-compatible endpoint. Local compose sets this to RustFS.
    pub endpoint: Option<String>,
    /// Optional explicit S3 access key.
    pub access_key_id: Option<String>,
    /// Optional explicit S3 secret key.
    pub secret_access_key: Option<String>,
    /// Allows HTTP endpoints for local S3-compatible development.
    pub allow_http: bool,
    /// Uses virtual-hosted-style S3 requests when true.
    pub virtual_hosted_style: bool,
    /// Optional GCS service account file path.
    pub gcp_service_account_path: Option<String>,
    /// Optional inline GCS service account JSON.
    pub gcp_service_account_key: Option<String>,
    /// Optional GCS application credentials file path.
    pub gcp_application_credentials_path: Option<String>,
}

impl Default for SessionAttachmentStorageConfig {
    fn default() -> Self {
        Self {
            backend: SessionAttachmentBackend::S3,
            bucket: "moa-session-attachments".to_string(),
            prefix: "session-attachments".to_string(),
            region: Some("us-east-1".to_string()),
            endpoint: None,
            access_key_id: None,
            secret_access_key: None,
            allow_http: false,
            virtual_hosted_style: false,
            gcp_service_account_path: None,
            gcp_service_account_key: None,
            gcp_application_credentials_path: None,
        }
    }
}

impl SessionAttachmentStorageConfig {
    /// Returns the local RustFS configuration used by compose-backed development tests.
    pub fn local_rustfs() -> Self {
        Self {
            endpoint: Some("http://127.0.0.1:9000".to_string()),
            access_key_id: Some("moaadmin".to_string()),
            secret_access_key: Some("moa-local-dev-secret".to_string()),
            allow_http: true,
            ..Self::default()
        }
    }

    /// Validates attachment object storage for the current runtime mode.
    pub fn validate(&self, cloud_enabled: bool) -> Result<()> {
        if self.bucket.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "session.attachments.bucket is required".to_string(),
            ));
        }

        if !cloud_enabled {
            return Ok(());
        }

        if self.allow_http {
            return Err(MoaError::ConfigError(
                "session.attachments.allow_http must be false when cloud.enabled = true"
                    .to_string(),
            ));
        }

        if self
            .endpoint
            .as_deref()
            .is_some_and(is_local_attachment_endpoint)
        {
            return Err(MoaError::ConfigError(
                "session.attachments.endpoint must not point at localhost when cloud.enabled = true"
                    .to_string(),
            ));
        }

        Ok(())
    }
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
    /// User-visible attachment object storage settings.
    pub attachments: SessionAttachmentStorageConfig,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            blob_threshold_bytes: 65_536,
            blob_backend: SessionBlobBackend::Postgres,
            blob_dir: None,
            attachments: SessionAttachmentStorageConfig::default(),
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

    /// Validates session storage configuration for the current runtime mode.
    pub fn validate(&self, cloud_enabled: bool) -> Result<()> {
        self.validate_blob_backend(cloud_enabled)?;
        self.attachments.validate(cloud_enabled)
    }
}

impl super::MoaEnvOverlay {
    /// Applies session storage environment overrides.
    pub(in crate::config) fn apply_session_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::{set_copy_if_some, set_if_some, set_option_if_some};

        set_copy_if_some(
            &mut config.session.blob_threshold_bytes,
            self.session_blob_threshold_bytes,
        );
        set_copy_if_some(&mut config.session.blob_backend, self.session_blob_backend);
        set_option_if_some(&mut config.session.blob_dir, &self.session_blob_dir);
        set_copy_if_some(
            &mut config.session.attachments.backend,
            self.session_attachment_backend,
        );
        set_if_some(
            &mut config.session.attachments.bucket,
            &self.session_attachment_bucket,
        );
        set_if_some(
            &mut config.session.attachments.prefix,
            &self.session_attachment_prefix,
        );
        set_option_if_some(
            &mut config.session.attachments.region,
            &self.session_attachment_region,
        );
        set_option_if_some(
            &mut config.session.attachments.endpoint,
            &self.session_attachment_endpoint,
        );
        set_option_if_some(
            &mut config.session.attachments.access_key_id,
            &self.session_attachment_access_key_id,
        );
        set_option_if_some(
            &mut config.session.attachments.secret_access_key,
            &self.session_attachment_secret_access_key,
        );
        set_copy_if_some(
            &mut config.session.attachments.allow_http,
            self.session_attachment_allow_http,
        );
        set_copy_if_some(
            &mut config.session.attachments.virtual_hosted_style,
            self.session_attachment_virtual_hosted_style,
        );
        set_option_if_some(
            &mut config.session.attachments.gcp_service_account_path,
            &self.session_attachment_gcp_service_account_path,
        );
        set_option_if_some(
            &mut config.session.attachments.gcp_service_account_key,
            &self.session_attachment_gcp_service_account_key,
        );
        set_option_if_some(
            &mut config.session.attachments.gcp_application_credentials_path,
            &self.session_attachment_gcp_application_credentials_path,
        );
    }
}

fn is_local_attachment_endpoint(endpoint: &str) -> bool {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "0.0.0.0" | "::1"))
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

    #[test]
    fn default_attachment_storage_is_cloud_safe_s3() {
        // Pins: config defaults do not point cloud deployments at a pod-local endpoint.
        let config = SessionConfig::default();

        assert_eq!(config.attachments.backend, SessionAttachmentBackend::S3);
        assert_eq!(config.attachments.bucket, "moa-session-attachments");
        assert_eq!(config.attachments.endpoint, None);
        assert!(!config.attachments.allow_http);
        config
            .validate(true)
            .expect("default cloud S3 attachment settings should be valid");
    }

    #[test]
    fn local_rustfs_attachment_storage_is_explicit() {
        // Pins: local RustFS is a deliberate local-dev config, not a cloud default.
        let config = SessionAttachmentStorageConfig::local_rustfs();

        assert_eq!(config.endpoint.as_deref(), Some("http://127.0.0.1:9000"));
        assert!(config.allow_http);
        config
            .validate(false)
            .expect("local RustFS attachment config should be valid outside cloud mode");
    }

    #[test]
    fn cloud_attachment_storage_rejects_local_rustfs() {
        // Pins: Kubernetes cloud deployments cannot silently store attachment bytes in a local endpoint.
        let config = SessionConfig {
            attachments: SessionAttachmentStorageConfig::local_rustfs(),
            ..SessionConfig::default()
        };
        let error = config
            .validate(true)
            .expect_err("cloud mode should reject local RustFS attachment config");

        assert_eq!(
            error.to_string(),
            "configuration error: session.attachments.allow_http must be false when cloud.enabled = true"
        );
    }
}
