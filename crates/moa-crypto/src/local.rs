//! In-memory key-management provider for development and tests.

use crate::error::Error;
use crate::key_wrap::{WRAPPING_KEY_LEN, generate_key, unwrap_key, wrap_key};
use crate::kms::{KeyManagementProvider, validate_single_subject_batch};
use crate::types::{
    DataKeyDecryptRequest, EncryptionContext, GeneratedDataKey, KeyHandle, PlaintextDek, WrappedDek,
};
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;
use uuid::Uuid;
use zeroize::Zeroizing;

/// State of one key-encryption key held by the local provider.
///
/// A destroyed KEK is kept as a tombstone rather than removed so that unwrap
/// attempts against it can be answered with [`Error::CryptoShredded`] instead of
/// the weaker [`Error::UnknownKey`].
enum KekState {
    /// Live key material.
    Active(Zeroizing<[u8; WRAPPING_KEY_LEN]>),
    /// The KEK was crypto-shredded; its material has been zeroized.
    Destroyed,
}

/// A [`KeyManagementProvider`] that keeps KEKs in process memory.
///
/// Intended only for local development and tests: KEKs never leave the process
/// and are lost on restart. It faithfully implements crypto-shred — destroying a
/// KEK drops (and zeroizes) its material so wrapped DEKs become permanently
/// unusable — which lets the same tests exercise erasure semantics that a real
/// KMS backend will provide in production.
///
/// KEKs are keyed per `(tenant_id, subject_id)` and created lazily on first use,
/// realizing the tenant → data subject → record hierarchy: a tenant holds many
/// data subjects, each subject has one wrapping key, and each record has its own
/// DEK wrapped by its subject's KEK. Destroying one subject's KEK
/// ([`Self::destroy_subject_key`]) crypto-shreds exactly that subject's records
/// while every other subject in the same tenant keeps decrypting.
#[derive(Default)]
pub struct LocalKmsProvider {
    keks: RwLock<HashMap<KeyHandle, KekState>>,
}

impl LocalKmsProvider {
    /// Create an empty provider with no keys yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The deterministic KEK handle for a `(tenant, data subject)` pair.
    ///
    /// The exact string is an internal detail of this provider (callers erase a
    /// subject through [`Self::destroy_subject_key`] or
    /// [`crate::crypto_shred_subject`], not by constructing this handle).
    /// Deterministic derivation means a subject that has been crypto-shredded can
    /// never resurrect a working KEK under the same handle within one process,
    /// which is the correct irreversible-erasure behavior for the dev backend.
    fn handle_for(tenant_id: Uuid, subject_id: Uuid) -> KeyHandle {
        KeyHandle::new(format!("local-kek:{tenant_id}:{subject_id}"))
    }

    /// Tombstone the KEK named by `handle`, zeroizing any live material.
    ///
    /// Shared by [`KeyManagementProvider::destroy_key`] and
    /// [`KeyManagementProvider::destroy_subject_key`]. Inserting a tombstone even
    /// for an unknown handle makes any straggler wrapped DEK referencing it fail
    /// as crypto-shredded rather than unknown.
    async fn tombstone(&self, handle: KeyHandle) {
        let mut guard = self.keks.write().await;
        // Replacing the entry drops the `Zeroizing` KEK, scrubbing its bytes.
        guard.insert(handle.clone(), KekState::Destroyed);
        tracing::info!(key_handle = %handle, "crypto-shred: destroyed key-encryption key");
    }
}

#[async_trait]
impl KeyManagementProvider for LocalKmsProvider {
    async fn generate_data_keys(
        &self,
        contexts: &[EncryptionContext],
    ) -> Result<Vec<GeneratedDataKey>, Error> {
        let Some(first) = contexts.first() else {
            return Ok(Vec::new());
        };
        validate_single_subject_batch(contexts.iter())?;
        let handle = Self::handle_for(first.tenant_id, first.subject_id);

        // Copy the live KEK material out under the lock, creating it lazily. A
        // shredded subject cannot obtain a new key under the same handle.
        let kek = {
            let mut guard = self.keks.write().await;
            let entry = guard
                .entry(handle.clone())
                .or_insert_with(|| KekState::Active(generate_key()));
            match entry {
                KekState::Destroyed => return Err(Error::CryptoShredded(handle)),
                KekState::Active(k) => Zeroizing::new(**k),
            }
        };

        contexts
            .iter()
            .map(|ctx| {
                let dek = generate_key();
                let wrapped = wrap_key(&kek, dek.as_ref(), &ctx.aad())?;
                Ok(GeneratedDataKey {
                    plaintext: PlaintextDek::new(*dek),
                    wrapped: WrappedDek::new(wrapped),
                    handle: handle.clone(),
                })
            })
            .collect()
    }

    async fn decrypt_data_keys(
        &self,
        requests: &[DataKeyDecryptRequest],
    ) -> Result<Vec<PlaintextDek>, Error> {
        let Some(first) = requests.first() else {
            return Ok(Vec::new());
        };
        validate_single_subject_batch(requests.iter().map(|request| &request.context))?;
        if requests
            .iter()
            .any(|request| request.handle != first.handle)
        {
            return Err(Error::InvalidBatch(
                "all requests must use the same key handle".to_string(),
            ));
        }

        let handle = &first.handle;
        let kek = {
            let guard = self.keks.read().await;
            match guard.get(handle) {
                Some(KekState::Active(k)) => Zeroizing::new(**k),
                Some(KekState::Destroyed) => return Err(Error::CryptoShredded(handle.clone())),
                None => return Err(Error::UnknownKey(handle.clone())),
            }
        };

        requests
            .iter()
            .map(|request| {
                let dek = unwrap_key(&kek, request.wrapped.as_bytes(), &request.context.aad())?;
                PlaintextDek::from_unwrapped(dek.to_vec())
            })
            .collect()
    }

    async fn destroy_key(&self, handle: &KeyHandle) -> Result<(), Error> {
        self.tombstone(handle.clone()).await;
        Ok(())
    }

    async fn destroy_subject_key(&self, tenant_id: Uuid, subject_id: Uuid) -> Result<(), Error> {
        // Erase only this subject's KEK; other subjects in the same tenant keep
        // their own handles and keep decrypting.
        self.tombstone(Self::handle_for(tenant_id, subject_id))
            .await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pins: LocalKmsProvider is ephemeral, so it reports is_durable() == false —
    // the signal the composition root uses to refuse sealing persisted restricted
    // data with an in-memory KMS (keys would be lost on restart).
    #[test]
    fn local_kms_is_not_durable_offline() {
        assert!(!LocalKmsProvider::new().is_durable());
    }
}
