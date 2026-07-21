//! Core value types for envelope encryption.
//!
//! These types are backend-agnostic: the same [`KeyHandle`], [`WrappedDek`], and
//! [`Ciphertext`] shapes are produced by the local dev KMS and by real KMS
//! backends, so storage and retrieval code depends only on this module.

use crate::aead::{KEY_LEN as AEAD_KEY_LEN, NONCE_LEN as AEAD_NONCE_LEN};
use crate::error::Error;
use std::fmt;
use uuid::Uuid;
use zeroize::Zeroizing;

/// Length in bytes of a data-encryption key (AES-256 → 32 bytes).
pub const DEK_LEN: usize = AEAD_KEY_LEN;

/// Length in bytes of an AES-GCM nonce (96 bits → 12 bytes).
pub const NONCE_LEN: usize = AEAD_NONCE_LEN;

/// Domain-separation prefix mixed into every additional-authenticated-data (AAD)
/// value. Bumping this string cleanly invalidates all previously sealed
/// ciphertext, since the AAD would no longer match.
const AAD_DOMAIN: &[u8] = b"moa-crypto/aad/v1";

/// Version byte prefixing the [`Ciphertext::to_bytes`] framing, so the on-disk
/// layout can evolve without ambiguity.
const CIPHERTEXT_CODEC_VERSION: u8 = 1;

/// Opaque identifier for the key-encryption key (KEK) that wrapped a data key.
///
/// For [`crate::LocalKmsProvider`] this is a synthetic per-`(tenant, subject)`
/// string; for a real KMS backend it is the provider's key identifier (for
/// example an AWS KMS key ARN). Its exact shape is an internal detail of each
/// provider and is not a contract. It is stored alongside ciphertext so
/// decryption can locate the wrapping key, and it is the unit of crypto-shred:
/// destroying the key behind a handle erases every record wrapped under it (in
/// this crate, one data subject's records within one tenant).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyHandle(String);

impl KeyHandle {
    /// Construct a key handle from any string-like value.
    pub fn new(handle: impl Into<String>) -> Self {
        Self(handle.into())
    }

    /// Borrow the handle as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for KeyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A data-encryption key encrypted ("wrapped") by a KEK. Safe to persist: it is
/// useless without access to the wrapping key inside the KMS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedDek(Vec<u8>);

impl WrappedDek {
    /// Wrap raw wrapped-key bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// Borrow the raw wrapped-key bytes for persistence or unwrapping.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A plaintext data-encryption key.
///
/// The 32 bytes of key material are held in [`Zeroizing`] so they are scrubbed
/// from memory on drop. This type deliberately does **not** implement `Clone`,
/// any serialization, or a value-revealing `Debug`; it must never be logged or
/// persisted. Persist the [`WrappedDek`] instead.
pub struct PlaintextDek(Zeroizing<[u8; DEK_LEN]>);

impl PlaintextDek {
    /// Wrap raw 32-byte key material as a plaintext DEK.
    pub fn new(bytes: [u8; DEK_LEN]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Borrow the raw key bytes for a single AEAD operation. Callers must not
    /// copy, log, or persist the returned slice.
    pub fn expose(&self) -> &[u8; DEK_LEN] {
        &self.0
    }

    /// Build a plaintext DEK from freshly unwrapped bytes, validating the length.
    ///
    /// The input vector is moved into [`Zeroizing`] first so the transient copy
    /// is scrubbed even on the length-mismatch error path.
    pub(crate) fn from_unwrapped(bytes: Vec<u8>) -> Result<Self, Error> {
        let bytes = Zeroizing::new(bytes);
        let arr: [u8; DEK_LEN] =
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| Error::InvalidKeyLength {
                    expected: DEK_LEN,
                    actual: bytes.len(),
                })?;
        Ok(Self::new(arr))
    }
}

impl fmt::Debug for PlaintextDek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PlaintextDek").field(&"<redacted>").finish()
    }
}

/// The result of [`crate::KeyManagementProvider::generate_data_key`]: a fresh
/// plaintext DEK for immediate one-time use, its KEK-wrapped form for storage,
/// and the handle of the wrapping key.
#[derive(Debug)]
pub struct GeneratedDataKey {
    /// Plaintext DEK for the caller to encrypt one record with, then drop.
    pub plaintext: PlaintextDek,
    /// KEK-wrapped DEK to persist next to the ciphertext.
    pub wrapped: WrappedDek,
    /// Handle of the wrapping key, needed to unwrap `wrapped` later.
    pub handle: KeyHandle,
}

/// One request to unwrap a persisted data-encryption key in a provider batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataKeyDecryptRequest {
    /// KEK-wrapped data key stored alongside the record ciphertext.
    pub wrapped: WrappedDek,
    /// Handle identifying the per-subject KEK.
    pub handle: KeyHandle,
    /// Context originally bound when the data key was wrapped.
    pub context: EncryptionContext,
}

impl DataKeyDecryptRequest {
    /// Build a data-key unwrap request.
    #[must_use]
    pub fn new(wrapped: WrappedDek, handle: KeyHandle, context: EncryptionContext) -> Self {
        Self {
            wrapped,
            handle,
            context,
        }
    }
}

/// One owned plaintext/context pair for [`crate::encrypt_batch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptionRequest {
    /// Plaintext bytes to seal.
    pub plaintext: Vec<u8>,
    /// Tenant, subject, record, and classification binding.
    pub context: EncryptionContext,
}

impl EncryptionRequest {
    /// Build an owned record-encryption request.
    #[must_use]
    pub fn new(plaintext: impl Into<Vec<u8>>, context: EncryptionContext) -> Self {
        Self {
            plaintext: plaintext.into(),
            context,
        }
    }
}

/// One owned ciphertext/context pair for [`crate::decrypt_batch`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecryptionRequest {
    /// Persisted envelope ciphertext to open.
    pub ciphertext: Ciphertext,
    /// Context expected to match the sealed record.
    pub context: EncryptionContext,
}

impl DecryptionRequest {
    /// Build an owned record-decryption request.
    #[must_use]
    pub fn new(ciphertext: Ciphertext, context: EncryptionContext) -> Self {
        Self {
            ciphertext,
            context,
        }
    }
}

/// A sealed record produced by [`crate::encrypt`].
///
/// Every field is safe to persist. Decryption requires the matching
/// [`EncryptionContext`] supplied by the caller — the `aad` field is a stored
/// copy of the bound context bytes for auditing and fast mismatch rejection, but
/// the cryptographic binding comes from re-deriving the AAD from the caller's
/// context, never from trusting this stored copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ciphertext {
    /// The record DEK, wrapped by the KEK named in `key_handle`.
    pub wrapped_dek: WrappedDek,
    /// Handle of the KEK that wrapped `wrapped_dek`.
    pub key_handle: KeyHandle,
    /// Per-record random AES-GCM nonce.
    pub nonce: [u8; NONCE_LEN],
    /// AES-256-GCM ciphertext with the authentication tag appended.
    pub ciphertext: Vec<u8>,
    /// The additional authenticated data bound at seal time (derived from the
    /// [`EncryptionContext`]).
    pub aad: Vec<u8>,
}

impl Ciphertext {
    /// Encode into a single self-describing byte blob.
    ///
    /// For callers that persist ciphertext as one opaque value (for example a
    /// token vault's `secret` column) rather than one column per field. The
    /// layout is a version byte, then each variable-length field as a big-endian
    /// `u32` length prefix followed by its bytes, with the fixed-width nonce
    /// inline. Every field is already safe to persist.
    pub fn to_bytes(&self) -> Vec<u8> {
        fn push_len_prefixed(out: &mut Vec<u8>, field: &[u8]) {
            out.extend_from_slice(&(field.len() as u32).to_be_bytes());
            out.extend_from_slice(field);
        }

        let handle = self.key_handle.as_str().as_bytes();
        let mut out = Vec::with_capacity(
            1 + 4 * 4
                + NONCE_LEN
                + self.wrapped_dek.as_bytes().len()
                + handle.len()
                + self.ciphertext.len()
                + self.aad.len(),
        );
        out.push(CIPHERTEXT_CODEC_VERSION);
        push_len_prefixed(&mut out, self.wrapped_dek.as_bytes());
        push_len_prefixed(&mut out, handle);
        out.extend_from_slice(&self.nonce);
        push_len_prefixed(&mut out, &self.ciphertext);
        push_len_prefixed(&mut out, &self.aad);
        out
    }

    /// Decode a blob produced by [`Ciphertext::to_bytes`].
    ///
    /// Returns [`Error::MalformedCiphertext`] on an unknown version, truncation,
    /// a non-UTF-8 key handle, or trailing bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let mut cur = ByteCursor::new(bytes);
        if cur.take_array::<1>()?[0] != CIPHERTEXT_CODEC_VERSION {
            return Err(Error::MalformedCiphertext);
        }
        let wrapped_dek = WrappedDek::new(cur.take_len_prefixed()?.to_vec());
        let key_handle = std::str::from_utf8(cur.take_len_prefixed()?)
            .map_err(|_| Error::MalformedCiphertext)?;
        let nonce = cur.take_array::<NONCE_LEN>()?;
        let ciphertext = cur.take_len_prefixed()?.to_vec();
        let aad = cur.take_len_prefixed()?.to_vec();
        if !cur.is_exhausted() {
            return Err(Error::MalformedCiphertext);
        }
        Ok(Self {
            wrapped_dek,
            key_handle: KeyHandle::new(key_handle),
            nonce,
            ciphertext,
            aad,
        })
    }
}

/// Bounds-checked forward reader over a byte slice, used by
/// [`Ciphertext::from_bytes`]. Every read is length-validated so decoding
/// untrusted input never panics.
struct ByteCursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    /// Wrap a slice at position zero.
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Take exactly `n` bytes, or fail if fewer remain.
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::MalformedCiphertext)?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(Error::MalformedCiphertext)?;
        self.pos = end;
        Ok(slice)
    }

    /// Take a fixed-size array of `N` bytes.
    fn take_array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::MalformedCiphertext)
    }

    /// Take a big-endian `u32` length prefix followed by that many bytes.
    fn take_len_prefixed(&mut self) -> Result<&'a [u8], Error> {
        let len = u32::from_be_bytes(self.take_array::<4>()?) as usize;
        self.take(len)
    }

    /// Whether all bytes have been consumed.
    fn is_exhausted(&self) -> bool {
        self.pos == self.buf.len()
    }
}

/// The binding context for a single record's encryption.
///
/// The key hierarchy is tenant → data subject → record: a record's DEK is
/// wrapped by the KEK of the `(tenant_id, subject_id)` pair, so destroying that
/// one subject's KEK crypto-shreds exactly that subject's records and no others.
///
/// This context's serialized form is used as AEAD additional authenticated data
/// at both the DEK-wrap and record-seal layers, so ciphertext cannot be swapped
/// between tenants, data subjects, records, or classifications: opening requires
/// the exact same tenant id, subject id, record id, and `pii_class`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptionContext {
    /// Owning tenant. Tenant remains MOA's outer isolation boundary.
    pub tenant_id: Uuid,
    /// The data subject (for example a contact) whose per-subject KEK wraps this
    /// record's DEK. Destroying this subject's KEK is the erasure primitive: it
    /// crypto-shreds every record sealed for this `(tenant_id, subject_id)` pair
    /// without touching other subjects in the same tenant.
    pub subject_id: Uuid,
    /// Stable identity of the record being sealed (for example a memory node
    /// uid or a vault entry id). Opaque to this crate.
    pub record_id: String,
    /// Privacy classification / purpose label for the record (for example
    /// `restricted`). Bound so a record cannot be reinterpreted under a
    /// different classification.
    pub pii_class: String,
}

impl EncryptionContext {
    /// Build an encryption context for a record.
    ///
    /// `subject_id` selects the per-subject KEK that wraps this record's DEK;
    /// records sharing a `(tenant_id, subject_id)` pair share a KEK (so they are
    /// erased together) but still each get their own DEK and nonce.
    pub fn new(
        tenant_id: Uuid,
        subject_id: Uuid,
        record_id: impl Into<String>,
        pii_class: impl Into<String>,
    ) -> Self {
        Self {
            tenant_id,
            subject_id,
            record_id: record_id.into(),
            pii_class: pii_class.into(),
        }
    }

    /// Serialize the context into unambiguous AEAD additional authenticated
    /// data.
    ///
    /// Each variable-length field is length-prefixed (big-endian `u64`) after a
    /// fixed domain-separation prefix, so distinct contexts can never produce the
    /// same byte string (for example `record_id = "ab"` cannot collide with a
    /// `pii_class` that starts with `"b"`). The subject id is bound too, so a
    /// ciphertext sealed for one data subject cannot be opened under another.
    pub(crate) fn aad(&self) -> Vec<u8> {
        fn push_field(out: &mut Vec<u8>, field: &[u8]) {
            out.extend_from_slice(&(field.len() as u64).to_be_bytes());
            out.extend_from_slice(field);
        }

        let mut out = Vec::with_capacity(
            AAD_DOMAIN.len() + 4 * 8 + 32 + self.record_id.len() + self.pii_class.len(),
        );
        out.extend_from_slice(AAD_DOMAIN);
        push_field(&mut out, self.tenant_id.as_bytes());
        push_field(&mut out, self.subject_id.as_bytes());
        push_field(&mut out, self.record_id.as_bytes());
        push_field(&mut out, self.pii_class.as_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Ciphertext {
        Ciphertext {
            wrapped_dek: WrappedDek::new(vec![1, 2, 3, 4, 5]),
            key_handle: KeyHandle::new("local-kek:tenant-xyz:subject-abc"),
            nonce: [7u8; NONCE_LEN],
            ciphertext: vec![9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
            aad: vec![0xaa, 0xbb, 0xcc],
        }
    }

    // Pins: Ciphertext survives a to_bytes -> from_bytes round-trip byte-for-byte
    // (the blob form the token vault persists in one column).
    #[test]
    fn ciphertext_codec_round_trip_offline() {
        let ct = sample();
        let decoded = Ciphertext::from_bytes(&ct.to_bytes()).expect("decode");
        assert_eq!(decoded, ct);
    }

    // Pins: decoding a truncated blob fails cleanly rather than panicking.
    #[test]
    fn ciphertext_codec_rejects_truncation_offline() {
        let bytes = sample().to_bytes();
        let err = Ciphertext::from_bytes(&bytes[..bytes.len() - 1]).expect_err("must reject");
        assert!(matches!(err, Error::MalformedCiphertext), "got {err:?}");
    }

    // Pins: trailing bytes after a well-formed blob are rejected (no silent
    // acceptance of appended data).
    #[test]
    fn ciphertext_codec_rejects_trailing_bytes_offline() {
        let mut bytes = sample().to_bytes();
        bytes.push(0x00);
        let err = Ciphertext::from_bytes(&bytes).expect_err("must reject");
        assert!(matches!(err, Error::MalformedCiphertext), "got {err:?}");
    }

    // Pins: an unknown leading version byte is rejected, so the layout can evolve
    // without a decoder silently misreading old blobs.
    #[test]
    fn ciphertext_codec_rejects_unknown_version_offline() {
        let mut bytes = sample().to_bytes();
        bytes[0] = 0xff;
        let err = Ciphertext::from_bytes(&bytes).expect_err("must reject");
        assert!(matches!(err, Error::MalformedCiphertext), "got {err:?}");
    }
}
