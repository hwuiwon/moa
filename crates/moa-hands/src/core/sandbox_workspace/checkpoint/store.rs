//! Encrypted create-only object storage for portable workspace checkpoints.

use std::sync::Arc;

use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use futures_util::StreamExt;
use moa_core::{
    error::{MoaError, Result},
    types::{
        identifiers::{ProviderAccountId, SandboxWorkspaceId, TenantId, WorkspaceCheckpointId},
        sandbox_workspace::{ProviderStorageKind, ProviderStorageRef},
    },
};
use moa_crypto::{
    KeyHandle, KeyManagementProvider, WrappedDek,
    chunked::{
        ChunkEncryptionContext, ChunkedEnvelope, EncryptedChunk, decrypt_chunks, encrypt_chunks,
    },
};
use object_store::{ObjectStore, PutMode, PutOptions, PutPayload, path::Path};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::archive::{
    ArchiveLimits, CheckpointArchive, CheckpointArchiveManifest, restore_checkpoint_archive,
};
use super::versioning::{
    CheckpointBucketVersioningGate, CheckpointBucketVersioningObserver,
    build_checkpoint_store_and_observer,
};

const STORE_FORMAT_VERSION: u16 = 1;
const MANIFEST_OBJECT_NAME: &str = "manifest.v1.json";

/// Provider-observed checkpoint-bucket versioning state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservedCheckpointBucketVersioning {
    /// The provider reports that versioning is disabled or has never been enabled.
    Unversioned,
    /// The provider reports enabled object versioning.
    Enabled,
    /// The provider reports suspended object versioning.
    Suspended,
    /// The provider response did not authoritatively establish a supported state.
    Unknown,
}

/// Bounded inventory summary for one exact checkpoint object prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointPrefixInventory {
    /// Number of current objects beneath the exact prefix.
    pub object_count: u64,
    /// Aggregate stored bytes beneath the exact prefix.
    pub stored_bytes: u64,
    /// SHA-256 digest of sorted object locations and sizes.
    pub inventory_digest: String,
}

/// First empty observation retained while waiting out provider consistency.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointEmptyObservation {
    /// Time of the first empty observation.
    pub first_observed_at: DateTime<Utc>,
    /// Digest of the empty exact-prefix inventory.
    pub inventory_digest: String,
}

/// Two separated empty observations proving exact-prefix absence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointAbsenceProof {
    /// Time of the first empty observation.
    pub first_observed_at: DateTime<Utc>,
    /// Time of the confirming empty observation.
    pub last_observed_at: DateTime<Utc>,
    /// Stable empty inventory digest shared by both observations.
    pub inventory_digest: String,
}

/// Result of one bounded checkpoint-prefix absence observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointPrefixObservation {
    /// Objects remain and invalidate any prior empty observation.
    Present(CheckpointPrefixInventory),
    /// The prefix is empty but the consistency window has not elapsed.
    EmptyPending(CheckpointEmptyObservation),
    /// Two separated empty observations prove current-object absence.
    Absent(CheckpointAbsenceProof),
}

#[derive(Debug)]
struct CheckpointPrefixListing {
    inventory: CheckpointPrefixInventory,
    locations: Vec<Path>,
}

/// Typed identities and provider binding for one checkpoint publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointStoreContext {
    /// Immutable tenant owner.
    pub tenant_id: TenantId,
    /// Durable logical workspace and KMS data subject.
    pub workspace_id: SandboxWorkspaceId,
    /// Immutable checkpoint identity.
    pub checkpoint_id: WorkspaceCheckpointId,
    /// Provider account owning the portable storage binding.
    pub provider_account_id: ProviderAccountId,
    /// Persisted provider-account generation owning the storage reference.
    pub provider_account_generation: u64,
}

impl CheckpointStoreContext {
    fn encryption_context(self, format_version: u16) -> ChunkEncryptionContext {
        ChunkEncryptionContext {
            tenant_id: self.tenant_id.0,
            workspace_id: self.workspace_id.0,
            checkpoint_id: self.checkpoint_id.0,
            format_version,
        }
    }
}

/// Verified result of publishing an immutable portable checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedCheckpoint {
    /// Opaque provider reference persisted with checkpoint metadata.
    pub storage: ProviderStorageRef,
    /// SHA-256 digest of the canonical final manifest.
    pub manifest_sha256: String,
    /// Logical uncompressed bytes represented by the checkpoint.
    pub logical_bytes: u64,
}

/// Encrypted checkpoint object-store adapter.
#[derive(Clone)]
pub struct CheckpointObjectStore {
    store: Arc<dyn ObjectStore>,
    kms: Arc<dyn KeyManagementProvider>,
    prefix: String,
    limits: ArchiveLimits,
    deletion: moa_config::CheckpointDeletionConfig,
    bucket_versioning_gate: Arc<CheckpointBucketVersioningGate>,
}

impl CheckpointObjectStore {
    /// Returns the archive safety limits enforced by this durable store.
    #[must_use]
    pub const fn archive_limits(&self) -> ArchiveLimits {
        self.limits
    }

    /// Returns the configured separation required by two empty observations.
    #[must_use]
    pub const fn deletion_consistency_window(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.deletion.consistency_window_seconds)
    }

    /// Builds the checkpoint store from the shared typed object-store config.
    pub fn from_config(
        config: &moa_config::MoaConfig,
        kms: Arc<dyn KeyManagementProvider>,
    ) -> Result<Self> {
        Self::from_config_with_versioning_observer(config, kms).map(|(store, _observer)| store)
    }

    /// Builds the checkpoint store and an observer sharing the exact backend
    /// credential provider and freshness gate.
    pub fn from_config_with_versioning_observer(
        config: &moa_config::MoaConfig,
        kms: Arc<dyn KeyManagementProvider>,
    ) -> Result<(Self, CheckpointBucketVersioningObserver)> {
        config.object_store.validate()?;
        config.sandbox_checkpoints.validate()?;
        if !config.sandbox_checkpoints.enabled {
            return Err(MoaError::ConfigError(
                "sandbox checkpoint object store requested while persistence is disabled"
                    .to_string(),
            ));
        }
        let location = &config.sandbox_checkpoints.storage;
        let (store, observer, gate) = build_checkpoint_store_and_observer(config)?;
        let checkpoint = &config.sandbox_checkpoints;
        let store = Self::new_inner(
            store,
            kms,
            location.prefix.clone(),
            ArchiveLimits {
                max_entries: checkpoint.max_entries,
                max_path_depth: checkpoint.max_path_depth,
                max_file_bytes: checkpoint.max_file_bytes,
                max_total_bytes: checkpoint.max_total_bytes,
                max_chunk_bytes: checkpoint.max_chunk_bytes,
                max_compressed_chunk_bytes: checkpoint.max_compressed_chunk_bytes,
            },
            checkpoint.deletion.clone(),
            gate,
        )?;
        Ok((store, observer))
    }

    /// Builds an adapter over an injected store whose caller has already
    /// verified that object versioning has never been enabled.
    pub fn new(
        store: Arc<dyn ObjectStore>,
        kms: Arc<dyn KeyManagementProvider>,
        prefix: impl Into<String>,
        limits: ArchiveLimits,
        observed_bucket_versioning: ObservedCheckpointBucketVersioning,
    ) -> Result<Self> {
        if observed_bucket_versioning != ObservedCheckpointBucketVersioning::Unversioned {
            return Err(MoaError::ConfigError(
                "checkpoint bucket must be provider-verified as unversioned".to_string(),
            ));
        }
        Self::new_inner(
            store,
            kms,
            prefix,
            limits,
            moa_config::CheckpointDeletionConfig::default(),
            Arc::new(CheckpointBucketVersioningGate::preverified(
                std::time::Duration::MAX,
            )),
        )
    }

    fn new_inner(
        store: Arc<dyn ObjectStore>,
        kms: Arc<dyn KeyManagementProvider>,
        prefix: impl Into<String>,
        limits: ArchiveLimits,
        deletion: moa_config::CheckpointDeletionConfig,
        bucket_versioning_gate: Arc<CheckpointBucketVersioningGate>,
    ) -> Result<Self> {
        let prefix = prefix.into().trim_matches('/').to_string();
        if prefix.is_empty() {
            return Err(MoaError::ConfigError(
                "checkpoint object-store prefix must reserve a non-root namespace".to_string(),
            ));
        }
        if deletion.max_objects == 0
            || deletion.max_bytes == 0
            || deletion.consistency_window_seconds == 0
        {
            return Err(MoaError::ConfigError(
                "checkpoint object-store deletion bounds are inconsistent".to_string(),
            ));
        }
        Ok(Self {
            store,
            kms,
            prefix,
            limits,
            deletion,
            bucket_versioning_gate,
        })
    }

    /// Overrides deletion bounds for an injected store.
    pub fn with_deletion_config(
        mut self,
        deletion: moa_config::CheckpointDeletionConfig,
    ) -> Result<Self> {
        if deletion.max_objects == 0
            || deletion.max_bytes == 0
            || deletion.consistency_window_seconds == 0
        {
            return Err(MoaError::ConfigError(
                "checkpoint object-store deletion bounds are inconsistent".to_string(),
            ));
        }
        self.deletion = deletion;
        Ok(self)
    }

    /// Returns whether provider bucket-versioning verification has succeeded.
    #[must_use]
    pub fn bucket_versioning_verified(&self) -> bool {
        self.bucket_versioning_gate.is_verified()
    }

    /// Authenticates checkpoint-namespace reachability and create-only writes.
    ///
    /// The probe writes no tenant data and removes its unique marker before
    /// returning. Any ambiguous write, read-after-write mismatch, or cleanup
    /// failure blocks startup.
    pub async fn preflight_create_only_namespace(&self) -> Result<()> {
        self.require_verified_unversioned_bucket()?;
        let key = format!(
            "{}/.moa-preflight/{}",
            self.prefix.trim_end_matches('/'),
            uuid::Uuid::now_v7()
        );
        const MARKER: &[u8] = b"moa-checkpoint-preflight-v1";
        self.put_create_and_verify(&key, MARKER).await?;
        self.store
            .delete(&Path::from(key.as_str()))
            .await
            .map_err(map_object_store_error)?;
        Ok(())
    }

    /// Encrypts, publishes, and verifies a checkpoint manifest-last.
    ///
    /// Each object is written in create-only mode. Identical retries verify and
    /// reuse prior bytes; conflicting bytes at the same immutable key fail.
    pub async fn publish(
        &self,
        context: CheckpointStoreContext,
        archive: CheckpointArchive,
    ) -> Result<PublishedCheckpoint> {
        self.require_verified_unversioned_bucket()?;
        archive.manifest.validate(self.limits)?;
        if archive.compressed_chunks.len() != archive.manifest.chunks.len() {
            return Err(MoaError::ValidationError(
                "checkpoint compressed chunks do not match the archive manifest".to_string(),
            ));
        }
        let envelope = encrypt_chunks(
            self.kms.as_ref(),
            context.encryption_context(archive.manifest.format_version),
            &archive.compressed_chunks,
            self.limits.max_compressed_chunk_bytes,
        )
        .await
        .map_err(map_crypto_error)?;
        let root = self.object_root(context);
        let chunks = archive
            .manifest
            .chunks
            .iter()
            .zip(&envelope.chunks)
            .map(|(archive_chunk, encrypted)| StoredEncryptedChunk {
                index: encrypted.index,
                object_name: format!("chunks/{:08}.bin", encrypted.index),
                nonce_base64: base64::engine::general_purpose::STANDARD.encode(encrypted.nonce),
                plaintext_sha256: hex::encode(encrypted.plaintext_digest),
                ciphertext_bytes: encrypted.ciphertext.len() as u64,
                ciphertext_sha256: hex::encode(Sha256::digest(&encrypted.ciphertext)),
                compressed_sha256: archive_chunk.compressed_sha256.clone(),
            })
            .collect::<Vec<_>>();

        for (descriptor, encrypted) in chunks.iter().zip(&envelope.chunks) {
            let key = format!("{root}/{}", descriptor.object_name);
            self.put_create_and_verify(&key, &encrypted.ciphertext)
                .await?;
        }

        let manifest = StoredCheckpointManifest {
            store_format_version: STORE_FORMAT_VERSION,
            archive: archive.manifest,
            wrapped_dek_base64: base64::engine::general_purpose::STANDARD
                .encode(envelope.wrapped_dek.as_bytes()),
            key_handle: envelope.key_handle.as_str().to_string(),
            chunks,
        };
        let manifest_bytes = manifest.canonical_bytes()?;
        let manifest_sha256 = hex::encode(Sha256::digest(&manifest_bytes));
        self.put_create_and_verify(&format!("{root}/{MANIFEST_OBJECT_NAME}"), &manifest_bytes)
            .await?;

        Ok(PublishedCheckpoint {
            storage: ProviderStorageRef {
                provider_account_id: context.provider_account_id,
                provider_account_generation: context.provider_account_generation,
                kind: ProviderStorageKind::PortableCheckpoint,
                resource_id: root,
                workspace_locator: None,
            },
            manifest_sha256,
            logical_bytes: manifest.archive.logical_bytes,
        })
    }

    /// Downloads, authenticates, and restores a checkpoint into a fresh root.
    pub async fn restore(
        &self,
        context: CheckpointStoreContext,
        fresh_data_root: impl AsRef<std::path::Path>,
    ) -> Result<()> {
        self.require_verified_unversioned_bucket()?;
        let root = self.object_root(context);
        let manifest_bytes = self
            .get_required(&format!("{root}/{MANIFEST_OBJECT_NAME}"))
            .await?;
        let manifest = StoredCheckpointManifest::from_canonical_bytes(&manifest_bytes)?;
        manifest.archive.validate(self.limits)?;
        let mut encrypted_chunks = Vec::with_capacity(manifest.chunks.len());
        for (position, descriptor) in manifest.chunks.iter().enumerate() {
            if descriptor.index as usize != position
                || descriptor.plaintext_sha256 != descriptor.compressed_sha256
            {
                return Err(MoaError::ValidationError(
                    "checkpoint encrypted chunk metadata is inconsistent".to_string(),
                ));
            }
            let ciphertext = self
                .get_required(&format!("{root}/{}", descriptor.object_name))
                .await?;
            if ciphertext.len() != descriptor.ciphertext_bytes as usize
                || hex::encode(Sha256::digest(&ciphertext)) != descriptor.ciphertext_sha256
            {
                return Err(MoaError::ValidationError(
                    "checkpoint ciphertext digest mismatch".to_string(),
                ));
            }
            let nonce = decode_fixed::<{ moa_crypto::NONCE_LEN }>(
                &descriptor.nonce_base64,
                "checkpoint chunk nonce",
            )?;
            let plaintext_digest = decode_hex_fixed::<32>(
                &descriptor.plaintext_sha256,
                "checkpoint chunk plaintext digest",
            )?;
            encrypted_chunks.push(EncryptedChunk {
                index: descriptor.index,
                plaintext_digest,
                nonce,
                ciphertext,
            });
        }
        let envelope = ChunkedEnvelope {
            wrapped_dek: WrappedDek::new(decode_base64(
                &manifest.wrapped_dek_base64,
                "checkpoint wrapped DEK",
            )?),
            key_handle: KeyHandle::new(manifest.key_handle.clone()),
            chunks: encrypted_chunks,
        };
        let compressed_chunks = decrypt_chunks(
            self.kms.as_ref(),
            context.encryption_context(manifest.archive.format_version),
            &envelope,
            self.limits.max_compressed_chunk_bytes,
        )
        .await
        .map_err(map_crypto_error)?;
        let archive = CheckpointArchive {
            manifest: manifest.archive,
            compressed_chunks,
        };
        restore_checkpoint_archive(archive, fresh_data_root, self.limits).await
    }

    /// Deletes every current object beneath the exact checkpoint prefix.
    ///
    /// Enumeration is manifest-independent, so abandoned chunks and partial
    /// uploads are deleted even when no final manifest was published.
    pub async fn delete(&self, context: CheckpointStoreContext) -> Result<()> {
        self.require_verified_unversioned_bucket()?;
        let listing = self.list_exact_prefix(context).await?;
        for location in listing.locations {
            match self.store.delete(&location).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
                Err(error) => return Err(map_object_store_error(error)),
            }
        }
        Ok(())
    }

    /// Enumerates the exact prefix and advances a two-empty absence proof.
    ///
    /// A non-empty inventory returns [`CheckpointPrefixObservation::Present`]
    /// and the caller must discard any saved empty observation. A changed
    /// inventory digest likewise starts a new proof window.
    pub async fn observe_absence(
        &self,
        context: CheckpointStoreContext,
        prior: Option<&CheckpointEmptyObservation>,
        observed_at: DateTime<Utc>,
    ) -> Result<CheckpointPrefixObservation> {
        self.require_verified_unversioned_bucket()?;
        let inventory = self.list_exact_prefix(context).await?.inventory;
        if inventory.object_count != 0 {
            return Ok(CheckpointPrefixObservation::Present(inventory));
        }
        let pending = match prior {
            Some(prior) if prior.inventory_digest == inventory.inventory_digest => prior.clone(),
            _ => CheckpointEmptyObservation {
                first_observed_at: observed_at,
                inventory_digest: inventory.inventory_digest.clone(),
            },
        };
        let consistency_window = Duration::seconds(
            i64::try_from(self.deletion.consistency_window_seconds).map_err(|_| {
                MoaError::ConfigError(
                    "checkpoint absence window exceeds signed duration arithmetic".to_string(),
                )
            })?,
        );
        if observed_at >= pending.first_observed_at + consistency_window {
            return Ok(CheckpointPrefixObservation::Absent(
                CheckpointAbsenceProof {
                    first_observed_at: pending.first_observed_at,
                    last_observed_at: observed_at,
                    inventory_digest: pending.inventory_digest,
                },
            ));
        }
        Ok(CheckpointPrefixObservation::EmptyPending(pending))
    }

    /// Builds the opaque portable reference determined by one exact checkpoint context.
    #[must_use]
    pub fn storage_reference(&self, context: CheckpointStoreContext) -> ProviderStorageRef {
        ProviderStorageRef {
            provider_account_id: context.provider_account_id,
            provider_account_generation: context.provider_account_generation,
            kind: ProviderStorageKind::PortableCheckpoint,
            resource_id: self.object_root(context),
            workspace_locator: None,
        }
    }

    /// Returns whether an opaque portable reference names exactly this context.
    #[must_use]
    pub fn reference_matches(
        &self,
        context: CheckpointStoreContext,
        storage: &ProviderStorageRef,
    ) -> bool {
        storage == &self.storage_reference(context)
    }

    /// Inspects one exact, fully published checkpoint without restoring bytes.
    ///
    /// A missing manifest is an incomplete publication and returns `None`;
    /// malformed or mismatched references fail rather than being treated as the
    /// caller's checkpoint.
    pub async fn inspect_publication(
        &self,
        context: CheckpointStoreContext,
        storage: &ProviderStorageRef,
    ) -> Result<Option<PublishedCheckpoint>> {
        self.require_verified_unversioned_bucket()?;
        if !self.reference_matches(context, storage) {
            return Err(MoaError::ValidationError(
                "checkpoint storage reference does not match the exact workspace fence".to_string(),
            ));
        }
        let manifest_key = format!("{}/{MANIFEST_OBJECT_NAME}", storage.resource_id);
        let result = match self.store.get(&Path::from(manifest_key)).await {
            Ok(result) => result,
            Err(object_store::Error::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(map_object_store_error(error)),
        };
        let bytes = result.bytes().await.map_err(map_object_store_error)?;
        let manifest = StoredCheckpointManifest::from_canonical_bytes(&bytes)?;
        manifest.archive.validate(self.limits)?;
        Ok(Some(PublishedCheckpoint {
            storage: storage.clone(),
            manifest_sha256: hex::encode(Sha256::digest(&bytes)),
            logical_bytes: manifest.archive.logical_bytes,
        }))
    }

    /// Verifies that an opaque reference points at a complete canonical manifest.
    pub async fn verify_reference(&self, storage: &ProviderStorageRef) -> Result<bool> {
        self.require_verified_unversioned_bucket()?;
        if storage.kind != ProviderStorageKind::PortableCheckpoint
            || storage.workspace_locator.is_some()
            || !storage
                .resource_id
                .starts_with(&format!("{}/", self.prefix))
        {
            return Ok(false);
        }
        let key = format!("{}/{MANIFEST_OBJECT_NAME}", storage.resource_id);
        match self.store.get(&Path::from(key)).await {
            Ok(result) => {
                let bytes = result.bytes().await.map_err(map_object_store_error)?;
                Ok(StoredCheckpointManifest::from_canonical_bytes(&bytes).is_ok())
            }
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(map_object_store_error(error)),
        }
    }

    fn object_root(&self, context: CheckpointStoreContext) -> String {
        let mut digest = Sha256::new();
        digest.update(b"moa/checkpoint-object-root/v1");
        digest.update(context.tenant_id.0.as_bytes());
        digest.update(context.workspace_id.0.as_bytes());
        digest.update(context.checkpoint_id.0.as_bytes());
        digest.update(context.provider_account_generation.to_be_bytes());
        format!("{}/{}", self.prefix, hex::encode(digest.finalize()))
    }

    fn require_verified_unversioned_bucket(&self) -> Result<()> {
        if !self.bucket_versioning_verified() {
            return Err(MoaError::StorageError(
                "checkpoint bucket versioning state is unverified or changed".to_string(),
            ));
        }
        Ok(())
    }

    async fn list_exact_prefix(
        &self,
        context: CheckpointStoreContext,
    ) -> Result<CheckpointPrefixListing> {
        let root = self.object_root(context);
        let prefix = Path::from(root.as_str());
        let mut stream = self.store.list(Some(&prefix));
        let mut objects = Vec::new();
        let mut stored_bytes = 0_u64;
        while let Some(item) = stream.next().await {
            let metadata = item.map_err(map_object_store_error)?;
            if objects.len() == self.deletion.max_objects {
                return Err(MoaError::StorageError(
                    "checkpoint prefix object count exceeds the configured deletion bound"
                        .to_string(),
                ));
            }
            let size = u64::try_from(metadata.size).map_err(|_| {
                MoaError::StorageError(
                    "checkpoint object size exceeds supported arithmetic".to_string(),
                )
            })?;
            stored_bytes = stored_bytes.checked_add(size).ok_or_else(|| {
                MoaError::StorageError(
                    "checkpoint prefix stored-byte inventory overflowed".to_string(),
                )
            })?;
            if stored_bytes > self.deletion.max_bytes {
                return Err(MoaError::StorageError(
                    "checkpoint prefix bytes exceed the configured deletion bound".to_string(),
                ));
            }
            objects.push((metadata.location, size, metadata.e_tag, metadata.version));
        }
        objects.sort_by(|left, right| left.0.cmp(&right.0));
        let mut digest = Sha256::new();
        digest.update(b"moa/checkpoint-prefix-inventory/v1");
        digest.update(root.as_bytes());
        for (location, size, etag, version) in &objects {
            digest.update(location.as_ref().as_bytes());
            digest.update([0]);
            digest.update(size.to_be_bytes());
            digest.update([0]);
            digest.update(etag.as_deref().unwrap_or_default().as_bytes());
            digest.update([0]);
            digest.update(version.as_deref().unwrap_or_default().as_bytes());
        }
        let object_count = objects.len() as u64;
        let locations = objects
            .into_iter()
            .map(|(location, _, _, _)| location)
            .collect();
        Ok(CheckpointPrefixListing {
            inventory: CheckpointPrefixInventory {
                object_count,
                stored_bytes,
                inventory_digest: hex::encode(digest.finalize()),
            },
            locations,
        })
    }

    async fn put_create_and_verify(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let path = Path::from(key);
        let put = self
            .store
            .put_opts(
                &path,
                PutPayload::from(bytes.to_vec()),
                PutOptions {
                    mode: PutMode::Create,
                    ..PutOptions::default()
                },
            )
            .await;
        match put {
            Ok(_) | Err(object_store::Error::AlreadyExists { .. }) => {}
            Err(error) => return Err(map_object_store_error(error)),
        }
        let observed = self.get_required(key).await?;
        if observed != bytes {
            return Err(MoaError::StorageError(
                "create-only checkpoint key already contains different bytes".to_string(),
            ));
        }
        Ok(())
    }

    async fn get_required(&self, key: &str) -> Result<Vec<u8>> {
        self.store
            .get(&Path::from(key))
            .await
            .map_err(map_object_store_error)?
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(map_object_store_error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredEncryptedChunk {
    index: u32,
    object_name: String,
    nonce_base64: String,
    plaintext_sha256: String,
    ciphertext_bytes: u64,
    ciphertext_sha256: String,
    compressed_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpointManifest {
    store_format_version: u16,
    archive: CheckpointArchiveManifest,
    wrapped_dek_base64: String,
    key_handle: String,
    chunks: Vec<StoredEncryptedChunk>,
}

impl StoredCheckpointManifest {
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).map_err(|error| {
            MoaError::StorageError(format!("serialize checkpoint object manifest: {error}"))
        })
    }

    fn from_canonical_bytes(bytes: &[u8]) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(bytes).map_err(|error| {
            MoaError::ValidationError(format!("invalid checkpoint object manifest: {error}"))
        })?;
        if manifest.store_format_version != STORE_FORMAT_VERSION
            || manifest.key_handle.trim().is_empty()
            || manifest.canonical_bytes()? != bytes
        {
            return Err(MoaError::ValidationError(
                "checkpoint object manifest is not canonical or supported".to_string(),
            ));
        }
        Ok(manifest)
    }
}

fn decode_base64(value: &str, field: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|_| MoaError::ValidationError(format!("invalid {field}")))
}

fn decode_fixed<const N: usize>(value: &str, field: &str) -> Result<[u8; N]> {
    decode_base64(value, field)?
        .try_into()
        .map_err(|_| MoaError::ValidationError(format!("invalid {field} length")))
}

fn decode_hex_fixed<const N: usize>(value: &str, field: &str) -> Result<[u8; N]> {
    hex::decode(value)
        .map_err(|_| MoaError::ValidationError(format!("invalid {field}")))?
        .try_into()
        .map_err(|_| MoaError::ValidationError(format!("invalid {field} length")))
}

fn map_crypto_error(error: moa_crypto::Error) -> MoaError {
    MoaError::StorageError(format!("checkpoint encryption error: {error}"))
}

fn map_object_store_error(error: object_store::Error) -> MoaError {
    match error {
        object_store::Error::NotFound { path, .. } => {
            MoaError::StorageError(format!("checkpoint object not found: {path}"))
        }
        error => MoaError::StorageError(format!("checkpoint object-store error: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::sandbox_workspace::checkpoint::archive::build_checkpoint_archive;
    use moa_crypto::LocalKmsProvider;
    use object_store::memory::InMemory;
    use tempfile::TempDir;

    fn context() -> CheckpointStoreContext {
        CheckpointStoreContext {
            tenant_id: TenantId::new(),
            workspace_id: SandboxWorkspaceId::new(),
            checkpoint_id: WorkspaceCheckpointId::new(),
            provider_account_id: ProviderAccountId::new(),
            provider_account_generation: 1,
        }
    }

    fn limits() -> ArchiveLimits {
        ArchiveLimits {
            max_entries: 32,
            max_path_depth: 8,
            max_file_bytes: 1024,
            max_total_bytes: 2048,
            max_chunk_bytes: 8,
            max_compressed_chunk_bytes: 1024,
        }
    }

    // Pins: config alone cannot masquerade as a provider observation when the
    // observer returned alongside the store is dropped or never run.
    #[tokio::test]
    async fn config_constructed_store_stays_blocked_without_observer_offline() {
        let temporary = TempDir::new().expect("temporary unverified checkpoint root");
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).expect("create unverified checkpoint source");
        std::fs::write(source.join("marker"), b"must not publish")
            .expect("write unverified checkpoint marker");
        let archive = build_checkpoint_archive(&source, limits())
            .await
            .expect("build unverified checkpoint archive");
        let mut config = moa_config::MoaConfig {
            object_store: moa_config::ObjectStoreConfig {
                credential_mode: moa_config::ObjectStoreCredentialMode::Static,
                endpoint: Some("http://127.0.0.1:9".to_string()),
                access_key_id: Some("fixture-access".to_string()),
                secret_access_key: Some("fixture-secret".to_string()),
                allow_http: true,
                ..moa_config::ObjectStoreConfig::default()
            },
            ..moa_config::MoaConfig::default()
        };
        config.sandbox_checkpoints.max_entries = limits().max_entries;
        config.sandbox_checkpoints.max_path_depth = limits().max_path_depth;
        config.sandbox_checkpoints.max_file_bytes = limits().max_file_bytes;
        config.sandbox_checkpoints.max_total_bytes = limits().max_total_bytes;
        config.sandbox_checkpoints.max_chunk_bytes = limits().max_chunk_bytes;
        config.sandbox_checkpoints.max_compressed_chunk_bytes = limits().max_compressed_chunk_bytes;
        config.sandbox_checkpoints.deletion.max_bytes = limits().max_total_bytes;
        let store = CheckpointObjectStore::from_config(&config, Arc::new(LocalKmsProvider::new()))
            .expect("construct configured checkpoint store");

        let publish_error = store
            .publish(context(), archive)
            .await
            .expect_err("unobserved config store must not publish");
        let preflight_error = store
            .preflight_create_only_namespace()
            .await
            .expect_err("unobserved config store must not write a preflight marker");
        let delete_error = store
            .delete(context())
            .await
            .expect_err("unobserved config store must not delete");

        assert!(matches!(publish_error, MoaError::StorageError(_)));
        assert!(matches!(preflight_error, MoaError::StorageError(_)));
        assert!(matches!(delete_error, MoaError::StorageError(_)));
        assert!(!store.bucket_versioning_verified());
    }

    // Pins: manifest-last create-only publication survives compute-root loss and
    // restores exact committed bytes into a fresh root.
    #[tokio::test]
    async fn encrypted_checkpoint_survives_fresh_compute_restore_offline() {
        let temporary = TempDir::new().expect("temporary checkpoint roots");
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).expect("create source root");
        std::fs::write(source.join("marker"), b"durable marker").expect("write source marker");
        let archive = build_checkpoint_archive(&source, limits())
            .await
            .expect("build checkpoint archive");
        let store = CheckpointObjectStore::new(
            Arc::new(InMemory::new()),
            Arc::new(LocalKmsProvider::new()),
            "workspace-checkpoints",
            limits(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("construct checkpoint store");
        let context = context();

        let published = store
            .publish(context, archive)
            .await
            .expect("publish encrypted checkpoint");
        std::fs::remove_dir_all(&source).expect("destroy original compute root");
        let restored = temporary.path().join("restored");
        store
            .restore(context, &restored)
            .await
            .expect("restore checkpoint into fresh compute root");

        assert_eq!(
            published.storage.kind,
            ProviderStorageKind::PortableCheckpoint
        );
        assert_eq!(
            std::fs::read(restored.join("marker")).expect("read restored marker"),
            b"durable marker"
        );
    }

    // Pins: the same immutable checkpoint key may replay identical bytes but
    // can never be overwritten by different content.
    #[tokio::test]
    async fn create_only_publication_rejects_conflicting_retry_offline() {
        let temporary = TempDir::new().expect("temporary checkpoint roots");
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).expect("create source root");
        std::fs::write(source.join("marker"), b"first").expect("write first marker");
        let store = CheckpointObjectStore::new(
            Arc::new(InMemory::new()),
            Arc::new(LocalKmsProvider::new()),
            "workspace-checkpoints",
            limits(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("construct checkpoint store");
        let context = context();
        store
            .publish(
                context,
                build_checkpoint_archive(&source, limits())
                    .await
                    .expect("build first archive"),
            )
            .await
            .expect("publish first checkpoint");
        std::fs::write(source.join("marker"), b"second").expect("write conflicting marker");

        let error = store
            .publish(
                context,
                build_checkpoint_archive(&source, limits())
                    .await
                    .expect("build conflicting archive"),
            )
            .await
            .expect_err("immutable checkpoint retry must not overwrite bytes");

        assert!(matches!(error, MoaError::StorageError(_)));
    }

    // Pins: cleanup enumerates the exact checkpoint prefix rather than trusting
    // a final manifest, and absence requires two separated empty observations.
    #[tokio::test]
    async fn partial_checkpoint_prefix_is_deleted_and_proved_absent_offline() {
        let backend = Arc::new(InMemory::new());
        let store = CheckpointObjectStore::new(
            backend.clone(),
            Arc::new(LocalKmsProvider::new()),
            "workspace-checkpoints",
            limits(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("construct checkpoint store")
        .with_deletion_config(moa_config::CheckpointDeletionConfig {
            max_objects: 8,
            max_bytes: 1_024,
            consistency_window_seconds: 2,
        })
        .expect("configure bounded deletion");
        let context = context();
        let root = store.object_root(context);
        backend
            .put(
                &Path::from(format!("{root}/chunks/00000000.bin")),
                PutPayload::from_static(b"partial-chunk"),
            )
            .await
            .expect("seed manifest-less chunk");
        backend
            .put(
                &Path::from(format!("{root}/multipart/abandoned")),
                PutPayload::from_static(b"partial-upload"),
            )
            .await
            .expect("seed abandoned partial upload");

        store
            .delete(context)
            .await
            .expect("delete every manifest-less object");
        let first_at = DateTime::parse_from_rfc3339("2026-08-09T12:00:00Z")
            .expect("fixed timestamp")
            .with_timezone(&Utc);
        let first = store
            .observe_absence(context, None, first_at)
            .await
            .expect("record first empty observation");
        let CheckpointPrefixObservation::EmptyPending(first) = first else {
            panic!("first empty observation must remain pending");
        };
        let confirmed = store
            .observe_absence(context, Some(&first), first_at + Duration::seconds(2))
            .await
            .expect("record second empty observation");
        let CheckpointPrefixObservation::Absent(proof) = confirmed else {
            panic!("two separated empty observations must prove absence");
        };

        assert_eq!(proof.first_observed_at, first_at);
        assert_eq!(proof.last_observed_at, first_at + Duration::seconds(2));
        assert_eq!(proof.inventory_digest, first.inventory_digest);
    }

    // Pins: prefix enumeration fails closed instead of partially deleting when
    // the object count exceeds the configured maintenance bound.
    #[tokio::test]
    async fn checkpoint_prefix_object_bound_fails_closed_offline() {
        let backend = Arc::new(InMemory::new());
        let store = CheckpointObjectStore::new(
            backend.clone(),
            Arc::new(LocalKmsProvider::new()),
            "workspace-checkpoints",
            limits(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("construct checkpoint store")
        .with_deletion_config(moa_config::CheckpointDeletionConfig {
            max_objects: 1,
            max_bytes: 1_024,
            consistency_window_seconds: 1,
        })
        .expect("configure one-object bound");
        let context = context();
        let root = store.object_root(context);
        for suffix in ["partial-a", "partial-b"] {
            backend
                .put(
                    &Path::from(format!("{root}/{suffix}")),
                    PutPayload::from_static(b"partial"),
                )
                .await
                .expect("seed bounded prefix object");
        }

        let error = store
            .delete(context)
            .await
            .expect_err("over-bound prefix must not be partially deleted");

        assert!(matches!(error, MoaError::StorageError(_)));
        let remaining = backend
            .list(Some(&Path::from(root)))
            .collect::<Vec<_>>()
            .await;
        assert_eq!(remaining.len(), 2);
    }

    // Pins: an unknown or versioned bucket state blocks construction, and an
    // invalidated provider observation immediately blocks deletion.
    #[tokio::test]
    async fn changed_bucket_versioning_state_fails_closed_offline() {
        let unknown = CheckpointObjectStore::new(
            Arc::new(InMemory::new()),
            Arc::new(LocalKmsProvider::new()),
            "workspace-checkpoints",
            limits(),
            ObservedCheckpointBucketVersioning::Unknown,
        );
        let Err(unknown) = unknown else {
            panic!("unknown checkpoint bucket versioning must fail construction");
        };
        assert!(matches!(unknown, MoaError::ConfigError(_)));
        let store = CheckpointObjectStore::new(
            Arc::new(InMemory::new()),
            Arc::new(LocalKmsProvider::new()),
            "workspace-checkpoints",
            limits(),
            ObservedCheckpointBucketVersioning::Unversioned,
        )
        .expect("construct checkpoint store");
        let enabled = CheckpointObjectStore::new(
            Arc::new(InMemory::new()),
            Arc::new(LocalKmsProvider::new()),
            "workspace-checkpoints",
            limits(),
            ObservedCheckpointBucketVersioning::Enabled,
        );
        let Err(enabled) = enabled else {
            panic!("enabled checkpoint bucket must be rejected");
        };
        assert!(matches!(enabled, MoaError::ConfigError(_)));
        store.bucket_versioning_gate.invalidate();
        let blocked = store
            .delete(context())
            .await
            .expect_err("changed bucket state must block purge");
        assert!(matches!(blocked, MoaError::StorageError(_)));
    }
}
