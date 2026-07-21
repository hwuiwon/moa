//! The single crate error type.

use crate::types::KeyHandle;
use thiserror::Error;

/// Errors surfaced by envelope encryption and the key-management providers.
///
/// Decryption-side failures are intentionally coarse: the AEAD layer never
/// reveals *why* an open failed (wrong key, wrong nonce, tampered ciphertext, or
/// mismatched additional data all collapse to [`Error::Decryption`]) so callers
/// cannot use error variants as a decryption oracle. The two erasure-specific
/// variants, [`Error::CryptoShredded`] and [`Error::UnknownKey`], are separated
/// because they describe key-lifecycle state rather than payload validity.
#[derive(Debug, Error)]
pub enum Error {
    /// The wrapping key has been destroyed. Data keys wrapped under this handle
    /// can no longer be unwrapped, so any ciphertext sealed with them is
    /// permanently irrecoverable. This is the expected outcome of a crypto-shred.
    #[error(
        "key handle {0} has been crypto-shredded; wrapped data keys can no longer be unwrapped"
    )]
    CryptoShredded(KeyHandle),

    /// No wrapping key is registered for this handle. Distinct from
    /// [`Error::CryptoShredded`]: the key was never known, as opposed to
    /// deliberately destroyed.
    #[error("unknown key handle {0}")]
    UnknownKey(KeyHandle),

    /// AEAD sealing failed. Surfaced only on catastrophic conditions such as a
    /// plaintext exceeding the AEAD's internal length bounds.
    #[error("AEAD encryption failed")]
    Encryption,

    /// AEAD opening failed. The ciphertext, key, nonce, or bound context is
    /// invalid, or the ciphertext was tampered with. Deliberately does not
    /// distinguish these cases.
    #[error("AEAD decryption failed: ciphertext, key, nonce, or bound context is invalid")]
    Decryption,

    /// The caller-supplied [`crate::EncryptionContext`] does not match the
    /// context bound into the ciphertext, so the record identity, tenant, or
    /// classification differs. Rejected before any key material is touched.
    #[error("encryption context does not match the sealed ciphertext")]
    ContextMismatch,

    /// Unwrapped key material was not the expected data-key length.
    #[error("data key must be {expected} bytes, got {actual}")]
    InvalidKeyLength {
        /// Expected data-key length in bytes.
        expected: usize,
        /// Actual length observed.
        actual: usize,
    },

    /// A wrapped data key was too short to contain its nonce prefix and sealed
    /// body, so it is structurally invalid.
    #[error("wrapped data key is malformed")]
    MalformedWrappedKey,

    /// A serialized [`crate::Ciphertext`] blob could not be decoded: unknown
    /// codec version, truncation, a non-UTF-8 key handle, or trailing bytes.
    #[error("serialized ciphertext is malformed")]
    MalformedCiphertext,

    /// A batch mixed tenant/subject groups or otherwise violated the provider's
    /// one-KEK-per-batch contract.
    #[error("invalid key-management batch: {0}")]
    InvalidBatch(String),

    /// The key-management backend failed (I/O, storage, or configuration).
    ///
    /// The message is for diagnostics and audit only; implementations must never
    /// place key material, plaintext, or wrapped-key bytes in it. This is the
    /// variant a persistent provider (Postgres, AWS KMS, Vault) maps its
    /// transport and root-key-load failures into.
    #[error("key-management backend error: {0}")]
    Backend(String),
}
