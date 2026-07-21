//! Envelope encryption over a [`KeyManagementProvider`].
//!
//! # AEAD choice: AES-256-GCM (RustCrypto `aes-gcm`)
//!
//! The record cipher is AES-256-GCM, via the vetted RustCrypto `aes-gcm` crate.
//! Rationale:
//!
//! - **Per-record DEK removes the nonce-reuse risk.** Each [`encrypt`] draws a
//!   fresh DEK that seals exactly one message, so AES-GCM's 96-bit random nonce
//!   cannot collide within a key. The birthday bound that makes random 96-bit
//!   nonces risky under key reuse simply does not apply here, which is the main
//!   reason an extended-nonce cipher (XChaCha20-Poly1305) is not needed.
//! - **Hardware acceleration and FIPS relevance.** AES-256-GCM runs on AES-NI on
//!   server CPUs and is the algorithm enterprise/BYOK customers expect for
//!   at-rest encryption.
//! - **KMS alignment.** AWS KMS `GenerateDataKey` yields AES-256 keys and KMS
//!   envelope encryption is itself AES-256-GCM, so the record layer matches the
//!   wrapping layer.
//! - **Ecosystem fit.** The workspace already vendors the RustCrypto stack
//!   (`hmac`, `sha2`, `argon2`, `digest`, `generic-array` with `zeroize`), so
//!   `aes-gcm` adds no foreign crypto toolchain and gives us `Zeroizing` DEK
//!   handling for free.
//!
//! Nonces come from the OS CSPRNG. The [`EncryptionContext`] is bound as
//! additional authenticated data at both the DEK-wrap and record-seal layers, so
//! a ciphertext sealed for one tenant/record/classification cannot be opened
//! under another.

use crate::aead;
use crate::error::Error;
use crate::kms::KeyManagementProvider;
use crate::types::{
    Ciphertext, DataKeyDecryptRequest, DecryptionRequest, EncryptionContext, EncryptionRequest,
    GeneratedDataKey, KeyHandle,
};
use uuid::Uuid;

/// Seal `plaintext` for the record identified by `ctx`.
///
/// Requests a fresh data key from `kms`, encrypts the plaintext once with
/// AES-256-GCM under that key with `ctx` bound as additional authenticated data,
/// and returns a [`Ciphertext`] whose every field is safe to persist. The
/// plaintext data key is dropped (and zeroized) before returning.
pub async fn encrypt<K>(
    kms: &K,
    plaintext: &[u8],
    ctx: &EncryptionContext,
) -> Result<Ciphertext, Error>
where
    K: KeyManagementProvider + ?Sized,
{
    let GeneratedDataKey {
        plaintext: dek,
        wrapped,
        handle,
    } = kms.generate_data_key(ctx).await?;

    let aad = ctx.aad();
    let nonce = aead::random_nonce();
    let ciphertext = aead::seal(dek.expose(), &nonce, plaintext, &aad)?;
    // `dek` is dropped here, zeroizing the plaintext key material.

    Ok(Ciphertext {
        wrapped_dek: wrapped,
        key_handle: handle,
        nonce,
        ciphertext,
        aad,
    })
}

/// Open a [`Ciphertext`] sealed by [`encrypt`].
///
/// `ctx` must describe the same tenant, record, and classification used to seal
/// the record. The AAD is re-derived from `ctx` and used as the cryptographic
/// binding; the stored [`Ciphertext::aad`] is only consulted for a fast, clear
/// [`Error::ContextMismatch`] before any key material is touched. Returns
/// [`Error::CryptoShredded`] if the wrapping key was destroyed.
pub async fn decrypt<K>(
    kms: &K,
    ciphertext: &Ciphertext,
    ctx: &EncryptionContext,
) -> Result<Vec<u8>, Error>
where
    K: KeyManagementProvider + ?Sized,
{
    let aad = ctx.aad();
    if aad != ciphertext.aad {
        return Err(Error::ContextMismatch);
    }

    let dek = kms
        .decrypt_data_key(&ciphertext.wrapped_dek, &ciphertext.key_handle, ctx)
        .await?;

    aead::open(
        dek.expose(),
        &ciphertext.nonce,
        &ciphertext.ciphertext,
        &aad,
    )
}

/// Seal one `(tenant, subject)` group with one batched KMS operation.
///
/// Results preserve request order. An empty input returns an empty output;
/// mixed tenant/subject groups are rejected by the KMS provider.
pub async fn encrypt_batch<K>(
    kms: &K,
    requests: &[EncryptionRequest],
) -> Result<Vec<Ciphertext>, Error>
where
    K: KeyManagementProvider + ?Sized,
{
    let contexts = requests
        .iter()
        .map(|request| request.context.clone())
        .collect::<Vec<_>>();
    let data_keys = kms.generate_data_keys(&contexts).await?;
    if data_keys.len() != requests.len() {
        return Err(Error::InvalidBatch(format!(
            "provider returned {} keys for {} records",
            data_keys.len(),
            requests.len()
        )));
    }

    requests
        .iter()
        .zip(data_keys)
        .map(|(request, generated)| {
            let aad = request.context.aad();
            let nonce = aead::random_nonce();
            let ciphertext = aead::seal(
                generated.plaintext.expose(),
                &nonce,
                &request.plaintext,
                &aad,
            )?;
            Ok(Ciphertext {
                wrapped_dek: generated.wrapped,
                key_handle: generated.handle,
                nonce,
                ciphertext,
                aad,
            })
        })
        .collect()
}

/// Open one `(tenant, subject)` group with one batched KMS operation.
///
/// Every stored AAD copy is checked before any key is requested. Results
/// preserve request order.
pub async fn decrypt_batch<K>(
    kms: &K,
    requests: &[DecryptionRequest],
) -> Result<Vec<Vec<u8>>, Error>
where
    K: KeyManagementProvider + ?Sized,
{
    for request in requests {
        if request.context.aad() != request.ciphertext.aad {
            return Err(Error::ContextMismatch);
        }
    }
    let key_requests = requests
        .iter()
        .map(|request| {
            DataKeyDecryptRequest::new(
                request.ciphertext.wrapped_dek.clone(),
                request.ciphertext.key_handle.clone(),
                request.context.clone(),
            )
        })
        .collect::<Vec<_>>();
    let data_keys = kms.decrypt_data_keys(&key_requests).await?;
    if data_keys.len() != requests.len() {
        return Err(Error::InvalidBatch(format!(
            "provider returned {} keys for {} records",
            data_keys.len(),
            requests.len()
        )));
    }

    requests
        .iter()
        .zip(data_keys)
        .map(|(request, data_key)| {
            aead::open(
                data_key.expose(),
                &request.ciphertext.nonce,
                &request.ciphertext.ciphertext,
                &request.ciphertext.aad,
            )
        })
        .collect()
}

/// Crypto-shred the key-encryption key named by `handle`.
///
/// After this returns, every data key wrapped under `handle` is permanently
/// un-unwrappable, so all ciphertext sealed with those keys is irrecoverable and
/// later [`decrypt`] calls fail with [`Error::CryptoShredded`]. The granularity
/// of erasure equals the granularity of the KEK behind `handle`. Prefer
/// [`crypto_shred_subject`] for the common case of forgetting one data subject:
/// it names the subject by identity rather than requiring the caller to know a
/// provider-internal handle string.
pub async fn crypto_shred<K>(kms: &K, handle: &KeyHandle) -> Result<(), Error>
where
    K: KeyManagementProvider + ?Sized,
{
    kms.destroy_key(handle).await
}

/// Crypto-shred one data subject within a tenant.
///
/// Destroys the KEK for the `(tenant_id, subject_id)` pair, so every record
/// sealed for that data subject becomes permanently irrecoverable and later
/// [`decrypt`] calls for those records fail with [`Error::CryptoShredded`].
/// Every other data subject in the same tenant — and every other tenant — is
/// untouched and keeps decrypting. This is idempotent: shredding a subject that
/// was already shredded, or never had a record, still succeeds.
///
/// This is the call storage/erasure code should make to forget a contact; it
/// takes subject identity directly, keeping the provider's KEK-handle shape an
/// internal detail.
pub async fn crypto_shred_subject<K>(
    kms: &K,
    tenant_id: Uuid,
    subject_id: Uuid,
) -> Result<(), Error>
where
    K: KeyManagementProvider + ?Sized,
{
    kms.destroy_subject_key(tenant_id, subject_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local::LocalKmsProvider;
    use uuid::Uuid;

    /// Fresh, unique context per test — keeps `LocalKmsProvider` state isolated
    /// so tests run concurrently under nextest.
    fn ctx() -> EncryptionContext {
        EncryptionContext::new(
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4().to_string(),
            "restricted",
        )
    }

    // Pins: envelope encrypt -> decrypt with the same context returns the exact
    // original plaintext.
    #[tokio::test]
    async fn encrypt_decrypt_round_trip_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let plaintext = b"member ssn 000-00-0000 and dob 1970-01-01";

        let sealed = encrypt(&kms, plaintext, &c).await.expect("encrypt");
        let opened = decrypt(&kms, &sealed, &c).await.expect("decrypt");

        assert_eq!(opened, plaintext);
    }

    // Pins: grouped envelope operations preserve order and use the provider's
    // batch contract for one tenant/subject.
    #[tokio::test]
    async fn batch_encrypt_decrypt_round_trip_offline() {
        let kms = LocalKmsProvider::new();
        let first = ctx();
        let second = EncryptionContext::new(
            first.tenant_id,
            first.subject_id,
            "record-two",
            "restricted",
        );
        let encrypt_requests = vec![
            EncryptionRequest::new(b"first".to_vec(), first.clone()),
            EncryptionRequest::new(b"second".to_vec(), second.clone()),
        ];
        let sealed = encrypt_batch(&kms, &encrypt_requests)
            .await
            .expect("batch encrypt");
        let decrypt_requests = vec![
            DecryptionRequest::new(sealed[0].clone(), first),
            DecryptionRequest::new(sealed[1].clone(), second),
        ];

        assert_eq!(
            decrypt_batch(&kms, &decrypt_requests)
                .await
                .expect("batch decrypt"),
            vec![b"first".to_vec(), b"second".to_vec()]
        );
    }

    // Pins: decrypting with a context whose record id differs (a ciphertext
    // swapped onto another row) is rejected before any key material is used.
    #[tokio::test]
    async fn decrypt_with_wrong_context_fails_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let sealed = encrypt(&kms, b"secret", &c).await.expect("encrypt");

        let mut wrong = c.clone();
        wrong.record_id = Uuid::new_v4().to_string();

        let err = decrypt(&kms, &sealed, &wrong)
            .await
            .expect_err("wrong context must be rejected");
        assert!(matches!(err, Error::ContextMismatch), "got {err:?}");
    }

    // Pins: the context binding is enforced by the AEAD itself, not merely by the
    // convenience equality check in `decrypt`. Unwrapping the DEK under a
    // mismatched context (different pii_class) fails at the KMS layer, so even if
    // the stored `aad` were tampered to pass the equality shortcut, the crypto
    // would still reject it.
    #[tokio::test]
    async fn dek_unwrap_with_wrong_context_fails_at_kms_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let sealed = encrypt(&kms, b"secret", &c).await.expect("encrypt");

        let mut wrong = c.clone();
        wrong.pii_class = "public".to_string();

        let err = kms
            .decrypt_data_key(&sealed.wrapped_dek, &sealed.key_handle, &wrong)
            .await
            .expect_err("mismatched aad must fail the unwrap");
        assert!(matches!(err, Error::Decryption), "got {err:?}");
    }

    // Pins: flipping a single ciphertext byte is caught by AES-GCM authentication
    // even with the correct context.
    #[tokio::test]
    async fn tampered_ciphertext_fails_aead_auth_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let mut sealed = encrypt(&kms, b"secret payload here", &c)
            .await
            .expect("encrypt");

        sealed.ciphertext[0] ^= 0x01;

        let err = decrypt(&kms, &sealed, &c)
            .await
            .expect_err("tampered ciphertext must fail");
        assert!(matches!(err, Error::Decryption), "got {err:?}");
    }

    // Pins: after crypto_shred, a full envelope decrypt fails specifically with
    // CryptoShredded (the erasure signal), not a generic decryption error.
    #[tokio::test]
    async fn crypto_shred_then_decrypt_fails_with_cryptoshredded_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let sealed = encrypt(&kms, b"erase me", &c).await.expect("encrypt");

        crypto_shred(&kms, &sealed.key_handle).await.expect("shred");

        let err = decrypt(&kms, &sealed, &c)
            .await
            .expect_err("decrypt after shred must fail");
        assert!(matches!(err, Error::CryptoShredded(_)), "got {err:?}");
    }

    // Pins: a wrapped DEK is unusable at the KMS layer after destroy_key.
    #[tokio::test]
    async fn wrapped_dek_unusable_after_destroy_key_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let dk = kms.generate_data_key(&c).await.expect("generate");

        kms.destroy_key(&dk.handle).await.expect("destroy");

        let err = kms
            .decrypt_data_key(&dk.wrapped, &dk.handle, &c)
            .await
            .expect_err("unwrap after destroy must fail");
        assert!(matches!(err, Error::CryptoShredded(_)), "got {err:?}");
    }

    // Pins: encrypting identical plaintext under an identical context twice yields
    // distinct nonces and distinct ciphertext bytes, and both still decrypt.
    #[tokio::test]
    async fn unique_nonces_produce_distinct_ciphertexts_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let plaintext = b"the same plaintext every time";

        let a = encrypt(&kms, plaintext, &c).await.expect("encrypt a");
        let b = encrypt(&kms, plaintext, &c).await.expect("encrypt b");

        assert_ne!(a.nonce, b.nonce, "nonces must be unique per encryption");
        assert_ne!(
            a.ciphertext, b.ciphertext,
            "identical plaintext must yield different ciphertext"
        );
        assert_eq!(decrypt(&kms, &a, &c).await.expect("decrypt a"), plaintext);
        assert_eq!(decrypt(&kms, &b, &c).await.expect("decrypt b"), plaintext);
    }

    // Pins: KMS generate -> unwrap returns identical DEK bytes under the same
    // context (the wrap/unwrap round-trip independent of record encryption).
    #[tokio::test]
    async fn generate_then_unwrap_dek_round_trip_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let dk = kms.generate_data_key(&c).await.expect("generate");

        let unwrapped = kms
            .decrypt_data_key(&dk.wrapped, &dk.handle, &c)
            .await
            .expect("unwrap");

        assert_eq!(dk.plaintext.expose(), unwrapped.expose());
    }

    // Pins: an unknown handle is reported as UnknownKey, distinct from the
    // CryptoShredded tombstone left by destroy_key.
    #[tokio::test]
    async fn unknown_handle_unwrap_reports_unknown_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let dk = kms.generate_data_key(&c).await.expect("generate");
        let bogus = KeyHandle::new("local-kek:00000000-0000-0000-0000-000000000000");

        let err = kms
            .decrypt_data_key(&dk.wrapped, &bogus, &c)
            .await
            .expect_err("unknown handle must fail");
        assert!(matches!(err, Error::UnknownKey(_)), "got {err:?}");
    }

    // Pins: the whole point of per-subject KEKs — crypto-shredding one data
    // subject makes that subject's records irrecoverable while another subject
    // *in the same tenant* keeps decrypting. Erasure isolation, not tenant-wide
    // erasure.
    #[tokio::test]
    async fn shredding_one_subject_spares_other_subjects_same_tenant_offline() {
        let kms = LocalKmsProvider::new();
        let tenant = Uuid::new_v4();
        let subject_a = Uuid::new_v4();
        let subject_b = Uuid::new_v4();

        let ctx_a =
            EncryptionContext::new(tenant, subject_a, Uuid::new_v4().to_string(), "restricted");
        let ctx_b =
            EncryptionContext::new(tenant, subject_b, Uuid::new_v4().to_string(), "restricted");

        let sealed_a = encrypt(&kms, b"subject A record", &ctx_a)
            .await
            .expect("encrypt a");
        let sealed_b = encrypt(&kms, b"subject B record", &ctx_b)
            .await
            .expect("encrypt b");

        // Erase only subject A.
        crypto_shred_subject(&kms, tenant, subject_a)
            .await
            .expect("shred subject a");

        // A is permanently irrecoverable...
        let err = decrypt(&kms, &sealed_a, &ctx_a)
            .await
            .expect_err("subject A must be shredded");
        assert!(matches!(err, Error::CryptoShredded(_)), "got {err:?}");

        // ...but B, in the same tenant, is untouched and still decrypts.
        let opened_b = decrypt(&kms, &sealed_b, &ctx_b)
            .await
            .expect("subject B still decrypts");
        assert_eq!(opened_b, b"subject B record");
    }

    // Pins: two records for the same data subject share one KEK (so a single
    // subject shred kills both) yet still get distinct per-record DEKs and
    // nonces (no key reuse across records).
    #[tokio::test]
    async fn same_subject_shares_kek_but_records_get_distinct_deks_offline() {
        let kms = LocalKmsProvider::new();
        let tenant = Uuid::new_v4();
        let subject = Uuid::new_v4();

        let ctx1 =
            EncryptionContext::new(tenant, subject, Uuid::new_v4().to_string(), "restricted");
        let ctx2 =
            EncryptionContext::new(tenant, subject, Uuid::new_v4().to_string(), "restricted");

        let r1 = encrypt(&kms, b"record one", &ctx1)
            .await
            .expect("encrypt 1");
        let r2 = encrypt(&kms, b"record two", &ctx2)
            .await
            .expect("encrypt 2");

        // One shared subject KEK wraps both records...
        assert_eq!(
            r1.key_handle, r2.key_handle,
            "records for one subject must share a KEK handle"
        );
        // ...but each record has its own DEK and its own nonce.
        assert_ne!(
            r1.wrapped_dek, r2.wrapped_dek,
            "each record must have a distinct DEK"
        );
        assert_ne!(r1.nonce, r2.nonce, "each record must have a distinct nonce");

        // Destroying the shared subject KEK erases both records at once.
        crypto_shred_subject(&kms, tenant, subject)
            .await
            .expect("shred subject");

        for (sealed, ctx) in [(&r1, &ctx1), (&r2, &ctx2)] {
            let err = decrypt(&kms, sealed, ctx)
                .await
                .expect_err("both records must be shredded");
            assert!(matches!(err, Error::CryptoShredded(_)), "got {err:?}");
        }
    }

    // Pins: a context carrying the wrong data subject is rejected at the envelope
    // layer before any key material is touched (ContextMismatch), so a ciphertext
    // cannot be reinterpreted as belonging to a different subject.
    #[tokio::test]
    async fn decrypt_with_wrong_subject_fails_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let sealed = encrypt(&kms, b"secret", &c).await.expect("encrypt");

        let mut wrong = c.clone();
        wrong.subject_id = Uuid::new_v4();

        let err = decrypt(&kms, &sealed, &wrong)
            .await
            .expect_err("wrong subject must be rejected");
        assert!(matches!(err, Error::ContextMismatch), "got {err:?}");
    }

    // Pins: the subject binding is enforced by the AEAD itself, not merely by the
    // envelope equality check. Unwrapping the DEK under the *correct* KEK handle
    // but a mismatched subject id in the context fails at the KMS layer, so even
    // if the stored `aad` were tampered to pass the equality shortcut, the crypto
    // would still reject the wrong subject.
    #[tokio::test]
    async fn dek_unwrap_with_wrong_subject_fails_at_kms_offline() {
        let kms = LocalKmsProvider::new();
        let c = ctx();
        let sealed = encrypt(&kms, b"secret", &c).await.expect("encrypt");

        let mut wrong = c.clone();
        wrong.subject_id = Uuid::new_v4();

        let err = kms
            .decrypt_data_key(&sealed.wrapped_dek, &sealed.key_handle, &wrong)
            .await
            .expect_err("mismatched subject must fail the unwrap");
        assert!(matches!(err, Error::Decryption), "got {err:?}");
    }
}
