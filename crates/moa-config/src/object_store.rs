//! Shared redacted configuration for S3-compatible and GCS object storage.

use std::fmt;

use moa_core::error::{MoaError, Result};
use serde::{Deserialize, Serialize};

/// Supported durable object-store backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectStoreBackend {
    /// Amazon S3 or an S3-compatible endpoint such as RustFS.
    #[default]
    S3,
    /// Google Cloud Storage.
    Gcs,
}

/// Credential source used by durable object-store clients.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectStoreCredentialMode {
    /// Use the SDK's ambient/default credential chain.
    #[default]
    Ambient,
    /// Use explicit access-key or service-account material from configuration.
    Static,
    /// Use the pod/service account identity supplied by the deployment platform.
    WorkloadIdentity,
}

/// Shared transport and credential configuration for durable object storage.
///
/// Buckets and prefixes remain use-case-specific locations. Endpoint,
/// credentials, region, transport posture, and environment semantics have one
/// owner so attachments and portable checkpoints cannot silently diverge.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObjectStoreConfig {
    /// Object-store backend.
    pub backend: ObjectStoreBackend,
    /// Credential source. Production sandbox checkpoints require workload identity.
    pub credential_mode: ObjectStoreCredentialMode,
    /// AWS/S3-compatible region.
    pub region: Option<String>,
    /// Optional S3-compatible endpoint.
    pub endpoint: Option<String>,
    /// Optional explicit S3 access key.
    pub access_key_id: Option<String>,
    /// Optional explicit S3 secret key.
    pub secret_access_key: Option<String>,
    /// Allows plaintext HTTP only for loopback or in-cluster RustFS endpoints.
    pub allow_http: bool,
    /// Uses virtual-hosted-style S3 requests when true.
    pub virtual_hosted_style: bool,
    /// Optional GCS service-account file path.
    pub gcp_service_account_path: Option<String>,
    /// Optional inline GCS service-account JSON.
    pub gcp_service_account_key: Option<String>,
    /// Optional GCS application-credentials file path.
    pub gcp_application_credentials_path: Option<String>,
}

impl Default for ObjectStoreConfig {
    fn default() -> Self {
        Self {
            backend: ObjectStoreBackend::S3,
            credential_mode: ObjectStoreCredentialMode::Ambient,
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

impl fmt::Debug for ObjectStoreConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObjectStoreConfig")
            .field("backend", &self.backend)
            .field("credential_mode", &self.credential_mode)
            .field("region", &self.region)
            .field("endpoint", &self.endpoint)
            .field(
                "access_key_id",
                &self.access_key_id.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "secret_access_key",
                &self.secret_access_key.as_ref().map(|_| "<redacted>"),
            )
            .field("allow_http", &self.allow_http)
            .field("virtual_hosted_style", &self.virtual_hosted_style)
            .field("gcp_service_account_path", &self.gcp_service_account_path)
            .field(
                "gcp_service_account_key",
                &self.gcp_service_account_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "gcp_application_credentials_path",
                &self.gcp_application_credentials_path,
            )
            .finish()
    }
}

impl ObjectStoreConfig {
    /// Returns the local RustFS transport used by compose and local Kubernetes.
    #[must_use]
    pub fn local_rustfs() -> Self {
        Self {
            credential_mode: ObjectStoreCredentialMode::Static,
            endpoint: Some("http://127.0.0.1:9000".to_string()),
            access_key_id: Some("moaadmin".to_string()),
            secret_access_key: Some("moa-local-dev-secret".to_string()),
            allow_http: true,
            ..Self::default()
        }
    }

    /// Validates transport and backend-specific credential settings.
    pub fn validate(&self) -> Result<()> {
        let has_s3_static = self.access_key_id.is_some() || self.secret_access_key.is_some();
        let has_gcs_static = self.gcp_service_account_path.is_some()
            || self.gcp_service_account_key.is_some()
            || self.gcp_application_credentials_path.is_some();
        if self.access_key_id.is_some() != self.secret_access_key.is_some() {
            return Err(MoaError::ConfigError(
                "object_store access_key_id and secret_access_key must be configured together"
                    .to_string(),
            ));
        }
        match self.credential_mode {
            ObjectStoreCredentialMode::Ambient => {}
            ObjectStoreCredentialMode::Static if !(has_s3_static || has_gcs_static) => {
                return Err(MoaError::ConfigError(
                    "object_store static credential mode requires an explicit credential pair or service-account source"
                        .to_string(),
                ));
            }
            ObjectStoreCredentialMode::Static => {}
            ObjectStoreCredentialMode::WorkloadIdentity if has_s3_static || has_gcs_static => {
                return Err(MoaError::ConfigError(
                    "object_store workload_identity mode forbids static credential material"
                        .to_string(),
                ));
            }
            ObjectStoreCredentialMode::WorkloadIdentity => {}
        }
        if self.allow_http
            && !self
                .endpoint
                .as_deref()
                .is_some_and(is_allowed_http_endpoint)
        {
            return Err(MoaError::ConfigError(
                "object_store.allow_http is only allowed for loopback or in-cluster RustFS endpoints"
                    .to_string(),
            ));
        }
        if matches!(self.backend, ObjectStoreBackend::Gcs)
            && (self.access_key_id.is_some() || self.secret_access_key.is_some())
        {
            return Err(MoaError::ConfigError(
                "object_store GCS backend cannot use S3 access-key settings".to_string(),
            ));
        }
        Ok(())
    }
}

/// One owner-specific bucket and key-prefix inside the shared object store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObjectStoreLocationConfig {
    /// Bucket containing objects for this use case.
    pub bucket: String,
    /// Key prefix reserved for this use case.
    pub prefix: String,
}

impl ObjectStoreLocationConfig {
    /// Validates that the bucket and prefix form a non-root namespace.
    pub fn validate(&self, config_path: &str) -> Result<()> {
        if self.bucket.trim().is_empty() {
            return Err(MoaError::ConfigError(format!(
                "{config_path}.bucket is required"
            )));
        }
        if self.prefix.trim_matches('/').is_empty() {
            return Err(MoaError::ConfigError(format!(
                "{config_path}.prefix must reserve a non-root namespace"
            )));
        }
        Ok(())
    }
}

fn is_allowed_http_endpoint(endpoint: &str) -> bool {
    url::Url::parse(endpoint)
        .ok()
        .filter(|url| url.scheme() == "http")
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| {
            matches!(
                host.as_str(),
                "127.0.0.1" | "localhost" | "0.0.0.0" | "::1" | "rustfs"
            ) || host.ends_with(".svc")
                || host.ends_with(".svc.cluster.local")
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_redacts_inline_object_store_credentials() {
        // Pins: config diagnostics cannot disclose durable object-store secrets.
        let config = ObjectStoreConfig {
            access_key_id: Some("access-secret".to_string()),
            secret_access_key: Some("secret-secret".to_string()),
            gcp_service_account_key: Some("gcp-secret".to_string()),
            ..ObjectStoreConfig::default()
        };

        let rendered = format!("{config:?}");

        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains("secret-secret"));
        assert!(!rendered.contains("gcp-secret"));
        assert_eq!(rendered.matches("<redacted>").count(), 3);
    }

    #[test]
    fn workload_identity_rejects_static_credentials() {
        // Pins: a production workload-identity deployment cannot silently fall
        // back to a long-lived key mounted or serialized with runtime config.
        let config = ObjectStoreConfig {
            credential_mode: ObjectStoreCredentialMode::WorkloadIdentity,
            access_key_id: Some("access-secret".to_string()),
            secret_access_key: Some("secret-secret".to_string()),
            ..ObjectStoreConfig::default()
        };

        assert_eq!(
            config
                .validate()
                .expect_err("workload identity must reject static keys")
                .to_string(),
            "configuration error: object_store workload_identity mode forbids static credential material"
        );
    }

    #[test]
    fn public_plaintext_http_endpoint_is_rejected() {
        // Pins: allow_http cannot turn off TLS for public object-store traffic.
        let config = ObjectStoreConfig {
            endpoint: Some("http://objects.example.com".to_string()),
            allow_http: true,
            ..ObjectStoreConfig::default()
        };

        let error = config
            .validate()
            .expect_err("public plaintext object storage must fail closed");

        assert_eq!(
            error.to_string(),
            "configuration error: object_store.allow_http is only allowed for loopback or in-cluster RustFS endpoints"
        );
    }
}
