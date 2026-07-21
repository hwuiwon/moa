//! The key-management provider abstraction.

use crate::error::Error;
use crate::types::{
    DataKeyDecryptRequest, EncryptionContext, GeneratedDataKey, KeyHandle, PlaintextDek, WrappedDek,
};
use async_trait::async_trait;
use uuid::Uuid;

/// Rejects a provider batch unless every context shares one tenant and subject.
pub fn validate_single_subject_batch<'a>(
    contexts: impl IntoIterator<Item = &'a EncryptionContext>,
) -> Result<(), Error> {
    let mut contexts = contexts.into_iter();
    let Some(first) = contexts.next() else {
        return Ok(());
    };
    if contexts.any(|ctx| ctx.tenant_id != first.tenant_id || ctx.subject_id != first.subject_id) {
        return Err(Error::InvalidBatch(
            "all contexts must share one tenant and subject".to_string(),
        ));
    }
    Ok(())
}

/// Envelope key operations backed by a KMS/HSM.
///
/// Implementations wrap and unwrap per-record data-encryption keys (DEKs) under
/// a per-`(tenant, data subject)` key-encryption key (KEK) that never leaves the
/// provider, and can destroy a KEK to crypto-shred everything wrapped under it.
/// Because the KEK is scoped to one data subject within a tenant, destroying it
/// erases exactly that subject's records. The trait is deliberately minimal so
/// AWS KMS, GCP KMS, HashiCorp Vault, and the in-process
/// [`crate::LocalKmsProvider`] can all implement it.
///
/// All methods are async because real backends perform network I/O. The AEAD
/// record encryption itself is CPU-only and lives in [`crate::envelope`], which
/// composes these operations.
#[async_trait]
pub trait KeyManagementProvider: Send + Sync {
    /// Generate fresh data-encryption keys for one `(tenant, subject)` group.
    ///
    /// Implementations must reject mixed groups. Persistent providers use one
    /// scoped transaction and one KEK unwrap for the entire batch.
    async fn generate_data_keys(
        &self,
        contexts: &[EncryptionContext],
    ) -> Result<Vec<GeneratedDataKey>, Error>;

    /// Unwrap data-encryption keys for one `(tenant, subject)` group.
    ///
    /// Results preserve request order. Implementations must reject mixed groups
    /// and must not rely on an authoritative process-local KEK cache.
    async fn decrypt_data_keys(
        &self,
        requests: &[DataKeyDecryptRequest],
    ) -> Result<Vec<PlaintextDek>, Error>;

    /// Generate a fresh data-encryption key for one record.
    ///
    /// Selects (creating on first use) the KEK for the `(tenant_id, subject_id)`
    /// pair carried by `ctx`, then returns the plaintext DEK (for immediate
    /// single use, then drop), the KEK-wrapped DEK (to persist), and the
    /// [`KeyHandle`] of that per-subject wrapping key. The `ctx` is bound into
    /// the wrap so the DEK can only be unwrapped under the identical context.
    ///
    /// Returns [`Error::CryptoShredded`] if the wrapping key for `ctx`'s data
    /// subject has already been destroyed.
    async fn generate_data_key(&self, ctx: &EncryptionContext) -> Result<GeneratedDataKey, Error> {
        self.generate_data_keys(std::slice::from_ref(ctx))
            .await?
            .pop()
            .ok_or_else(|| Error::InvalidBatch("provider returned no data key".to_string()))
    }

    /// Unwrap a previously wrapped data key.
    ///
    /// `ctx` must equal the context passed to [`Self::generate_data_key`], or the
    /// bound-context check fails and the DEK is not returned. Returns
    /// [`Error::CryptoShredded`] if the wrapping key was destroyed, or
    /// [`Error::UnknownKey`] if `handle` was never registered.
    async fn decrypt_data_key(
        &self,
        wrapped: &WrappedDek,
        handle: &KeyHandle,
        ctx: &EncryptionContext,
    ) -> Result<PlaintextDek, Error> {
        let request = DataKeyDecryptRequest::new(wrapped.clone(), handle.clone(), ctx.clone());
        self.decrypt_data_keys(std::slice::from_ref(&request))
            .await?
            .pop()
            .ok_or_else(|| Error::InvalidBatch("provider returned no plaintext key".to_string()))
    }

    /// Destroy the key-encryption key named by `handle` (crypto-shred).
    ///
    /// After this returns, every DEK wrapped under `handle` is permanently
    /// un-unwrappable, so all ciphertext sealed with those DEKs is
    /// irrecoverable. Implementations must be idempotent: destroying an already
    /// destroyed or never-registered handle succeeds.
    async fn destroy_key(&self, handle: &KeyHandle) -> Result<(), Error>;

    /// Crypto-shred one data subject by destroying that subject's KEK.
    ///
    /// This is the erasure primitive storage calls to forget a single data
    /// subject (for example a contact): it destroys the KEK for the
    /// `(tenant_id, subject_id)` pair, leaving every other subject in the same
    /// tenant — and every other tenant — untouched. After it returns, every DEK
    /// wrapped under that subject's KEK is permanently un-unwrappable, so all of
    /// that subject's ciphertext is irrecoverable. Implementations must be
    /// idempotent: shredding an already-shredded or never-seen subject succeeds.
    ///
    /// There is no blanket default: each provider derives its own KEK handle
    /// from `(tenant_id, subject_id)` (a synthetic string for
    /// [`crate::LocalKmsProvider`], a key ARN or alias for a cloud KMS), and
    /// that mapping is provider-private, so the method is implemented concretely
    /// per backend rather than defaulted on the trait.
    async fn destroy_subject_key(&self, tenant_id: Uuid, subject_id: Uuid) -> Result<(), Error>;

    /// Whether keys persist across process restarts.
    ///
    /// Ephemeral providers such as [`crate::LocalKmsProvider`] keep the default
    /// `false`; durable/persistent backends (Postgres, AWS KMS, Vault) override
    /// to `true`. The composition root uses this to fail closed before sealing
    /// data that must outlive the process: encrypting persisted restricted data
    /// with an ephemeral KMS would lose the keys on restart, silently rendering
    /// that data undecryptable.
    fn is_durable(&self) -> bool {
        false
    }
}
