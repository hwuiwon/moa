//! Session storage configuration.

use crate::{error::MoaError, error::Result};
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

    /// Validates attachment object storage.
    pub fn validate(&self) -> Result<()> {
        if self.bucket.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "session.attachments.bucket is required".to_string(),
            ));
        }

        if self.allow_http
            && !self
                .endpoint
                .as_deref()
                .is_some_and(is_local_attachment_endpoint)
        {
            return Err(MoaError::ConfigError(
                "session.attachments.allow_http is only allowed for local attachment endpoints"
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

fn is_local_attachment_endpoint(endpoint: &str) -> bool {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| {
            matches!(
                host.as_str(),
                "127.0.0.1" | "localhost" | "0.0.0.0" | "::1" | "rustfs"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

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

        assert_eq!(config.attachments.backend, SessionAttachmentBackend::S3);
        assert_eq!(config.attachments.bucket, "moa-session-attachments");
        assert_eq!(config.attachments.endpoint, None);
        assert!(!config.attachments.allow_http);
        config
            .validate()
            .expect("default S3 attachment settings should be valid");
    }

    #[test]
    fn local_rustfs_attachment_storage_is_explicit() {
        // Pins: local RustFS is a deliberate local-dev config, not a cloud default.
        let config = SessionAttachmentStorageConfig::local_rustfs();

        assert_eq!(config.endpoint.as_deref(), Some("http://127.0.0.1:9000"));
        assert!(config.allow_http);
        config
            .validate()
            .expect("local RustFS attachment config should be valid");
    }

    #[test]
    fn attachment_storage_rejects_remote_http_endpoint() {
        // Pins: plaintext attachment endpoints are only acceptable for local RustFS.
        let config = SessionAttachmentStorageConfig {
            endpoint: Some("http://object-store.internal:9000".to_string()),
            allow_http: true,
            ..SessionAttachmentStorageConfig::default()
        };
        let error = config
            .validate()
            .expect_err("remote HTTP attachment endpoint should fail");

        assert_eq!(
            error.to_string(),
            "configuration error: session.attachments.allow_http is only allowed for local attachment endpoints"
        );
    }
}
