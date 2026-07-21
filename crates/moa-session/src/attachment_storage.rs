//! Object-store adapter for durable session attachment bytes.

use std::sync::Arc;

use moa_config::MoaConfig;
use moa_config::SessionAttachmentBackend;
use moa_core::{
    error::MoaError, error::Result, types::identifiers::SessionAttachmentId,
    types::identifiers::SessionId, types::identifiers::TenantId,
};
use object_store::{
    ObjectStore, PutPayload, aws::AmazonS3Builder, gcp::GoogleCloudStorageBuilder, path::Path,
};

/// Shared object-store handle and key prefix for session attachments.
#[derive(Clone)]
pub(crate) struct AttachmentObjectStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl AttachmentObjectStore {
    /// Builds an attachment object store from typed MOA config.
    pub(crate) fn from_config(config: &MoaConfig) -> Result<Self> {
        config.session.attachments.validate()?;
        let attachments = &config.session.attachments;
        let store: Arc<dyn ObjectStore> = match attachments.backend {
            SessionAttachmentBackend::S3 => {
                let mut builder = AmazonS3Builder::from_env()
                    .with_bucket_name(attachments.bucket.clone())
                    .with_allow_http(attachments.allow_http)
                    .with_virtual_hosted_style_request(attachments.virtual_hosted_style);

                if let Some(region) = non_empty(&attachments.region) {
                    builder = builder.with_region(region);
                }
                if let Some(endpoint) = non_empty(&attachments.endpoint) {
                    builder = builder.with_endpoint(endpoint);
                }
                if let Some(access_key_id) = non_empty(&attachments.access_key_id) {
                    builder = builder.with_access_key_id(access_key_id);
                }
                if let Some(secret_access_key) = non_empty(&attachments.secret_access_key) {
                    builder = builder.with_secret_access_key(secret_access_key);
                }

                Arc::new(builder.build().map_err(|error| {
                    MoaError::ConfigError(format!(
                        "failed to build session attachment S3 object store: {error}"
                    ))
                })?)
            }
            SessionAttachmentBackend::Gcs => {
                let mut builder = GoogleCloudStorageBuilder::from_env()
                    .with_bucket_name(attachments.bucket.clone());

                if let Some(path) = non_empty(&attachments.gcp_service_account_path) {
                    builder = builder.with_service_account_path(path);
                }
                if let Some(key) = non_empty(&attachments.gcp_service_account_key) {
                    builder = builder.with_service_account_key(key);
                }
                if let Some(path) = non_empty(&attachments.gcp_application_credentials_path) {
                    builder = builder.with_application_credentials(path);
                }

                Arc::new(builder.build().map_err(|error| {
                    MoaError::ConfigError(format!(
                        "failed to build session attachment GCS object store: {error}"
                    ))
                })?)
            }
        };

        Ok(Self {
            store,
            prefix: normalize_prefix(&attachments.prefix),
        })
    }

    /// Stores one attachment object.
    pub(crate) async fn put(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        attachment_id: SessionAttachmentId,
        content: Vec<u8>,
    ) -> Result<String> {
        let object_key = self.object_key(tenant_id, session_id, attachment_id);
        self.store
            .put(&Path::from(object_key.as_str()), PutPayload::from(content))
            .await
            .map_err(map_object_store_error)?;
        Ok(object_key)
    }

    /// Loads one attachment object.
    pub(crate) async fn get(&self, object_key: &str) -> Result<Vec<u8>> {
        let bytes = self
            .store
            .get(&Path::from(object_key))
            .await
            .map_err(map_object_store_error)?
            .bytes()
            .await
            .map_err(map_object_store_error)?;
        Ok(bytes.to_vec())
    }

    /// Deletes one attachment object.
    pub(crate) async fn delete(&self, object_key: &str) -> Result<()> {
        match self.store.delete(&Path::from(object_key)).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(map_object_store_error(error)),
        }
    }

    fn object_key(
        &self,
        tenant_id: TenantId,
        session_id: SessionId,
        attachment_id: SessionAttachmentId,
    ) -> String {
        let suffix =
            format!("tenants/{tenant_id}/sessions/{session_id}/attachments/{attachment_id}");
        if self.prefix.is_empty() {
            suffix
        } else {
            format!("{}/{suffix}", self.prefix)
        }
    }
}

fn normalize_prefix(prefix: &str) -> String {
    prefix.trim_matches('/').to_string()
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn map_object_store_error(error: object_store::Error) -> MoaError {
    match error {
        object_store::Error::NotFound { path, .. } => {
            MoaError::SessionAttachmentObjectNotFound(path)
        }
        error => MoaError::StorageError(format!("session attachment object store error: {error}")),
    }
}
