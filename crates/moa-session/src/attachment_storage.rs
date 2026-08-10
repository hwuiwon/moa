//! Object-store adapter for durable session attachment bytes.

use std::sync::Arc;

use moa_config::MoaConfig;
use moa_config::ObjectStoreBackend;
use moa_core::{
    error::MoaError, error::Result, types::identifiers::SessionAttachmentId,
    types::identifiers::SessionId, types::identifiers::TenantId,
};
use object_store::{
    ObjectStore, PutMode, PutOptions, PutPayload, aws::AmazonS3Builder, aws::S3ConditionalPut,
    gcp::GoogleCloudStorageBuilder, path::Path,
};

/// Outcome of one create-only session attachment object write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttachmentObjectWrite {
    /// This request created the object.
    Created,
    /// An object already occupied the key and was left untouched.
    AlreadyPresent,
}

/// Shared object-store handle and key prefix for session attachments.
#[derive(Clone)]
pub(crate) struct AttachmentObjectStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl AttachmentObjectStore {
    /// Builds an attachment object store from typed MOA config.
    pub(crate) fn from_config(config: &MoaConfig) -> Result<Self> {
        config.object_store.validate()?;
        config.session.attachments.validate()?;
        let attachments = &config.session.attachments;
        let object_store = &config.object_store;
        let store: Arc<dyn ObjectStore> = match object_store.backend {
            ObjectStoreBackend::S3 => {
                let mut builder = AmazonS3Builder::from_env()
                    .with_bucket_name(attachments.storage.bucket.clone())
                    .with_allow_http(object_store.allow_http)
                    .with_virtual_hosted_style_request(object_store.virtual_hosted_style)
                    // Attachment slots are written create-only, which S3 expresses as an
                    // `If-None-Match: *` precondition. Without this the store rejects every
                    // conditional put as unsupported and no upload can be stored at all.
                    .with_conditional_put(S3ConditionalPut::ETagMatch);

                if let Some(region) = non_empty(&object_store.region) {
                    builder = builder.with_region(region);
                }
                if let Some(endpoint) = non_empty(&object_store.endpoint) {
                    builder = builder.with_endpoint(endpoint);
                }
                if let Some(access_key_id) = non_empty(&object_store.access_key_id) {
                    builder = builder.with_access_key_id(access_key_id);
                }
                if let Some(secret_access_key) = non_empty(&object_store.secret_access_key) {
                    builder = builder.with_secret_access_key(secret_access_key);
                }

                Arc::new(builder.build().map_err(|error| {
                    MoaError::ConfigError(format!(
                        "failed to build session attachment S3 object store: {error}"
                    ))
                })?)
            }
            ObjectStoreBackend::Gcs => {
                let mut builder = GoogleCloudStorageBuilder::from_env()
                    .with_bucket_name(attachments.storage.bucket.clone());

                if let Some(path) = non_empty(&object_store.gcp_service_account_path) {
                    builder = builder.with_service_account_path(path);
                }
                if let Some(key) = non_empty(&object_store.gcp_service_account_key) {
                    builder = builder.with_service_account_key(key);
                }
                if let Some(path) = non_empty(&object_store.gcp_application_credentials_path) {
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
            prefix: normalize_prefix(&attachments.storage.prefix),
        })
    }

    /// Stores one attachment object only when its key is still free.
    ///
    /// Create-only, so a retried upload can never overwrite the bytes an earlier
    /// request stored in the same deterministic slot before Postgres has decided
    /// whether the retry is a legitimate replay or a conflict. Backends without
    /// conditional put support surface that as a storage error rather than silently
    /// degrading to an unconditional overwrite.
    pub(crate) async fn put_if_absent(
        &self,
        object_key: &str,
        content: &[u8],
    ) -> Result<AttachmentObjectWrite> {
        let result = self
            .store
            .put_opts(
                &Path::from(object_key),
                PutPayload::from(content.to_vec()),
                PutOptions {
                    mode: PutMode::Create,
                    ..PutOptions::default()
                },
            )
            .await;
        match result {
            Ok(_) => Ok(AttachmentObjectWrite::Created),
            Err(object_store::Error::AlreadyExists { .. }) => {
                Ok(AttachmentObjectWrite::AlreadyPresent)
            }
            Err(error) => Err(map_object_store_error(error)),
        }
    }

    /// Replaces one attachment object whose slot this caller already owns in Postgres.
    pub(crate) async fn overwrite(&self, object_key: &str, content: &[u8]) -> Result<()> {
        self.store
            .put(&Path::from(object_key), PutPayload::from(content.to_vec()))
            .await
            .map_err(map_object_store_error)?;
        Ok(())
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

    /// Returns the deterministic object key for one attachment slot.
    pub(crate) fn object_key(
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
