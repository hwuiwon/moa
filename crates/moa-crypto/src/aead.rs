//! AES-256-GCM seal/open primitives and CSPRNG helpers.
//!
//! This is the one place the crate touches a symmetric AEAD. Record sealing and
//! the reviewed key-wrap framing in [`crate::key_wrap`] both go through these
//! primitives, so the algorithm choice and its invariants live in one module.

use crate::error::Error;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use rand::rngs::OsRng;

/// Length in bytes of an AES-256 key.
pub(crate) const KEY_LEN: usize = 32;

/// Length in bytes of an AES-GCM nonce (the standard 96-bit nonce).
pub(crate) const NONCE_LEN: usize = 12;

/// Draw a fresh 32-byte key from the operating-system CSPRNG.
pub(crate) fn random_key() -> [u8; KEY_LEN] {
    let mut bytes = [0u8; KEY_LEN];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Draw a fresh 96-bit nonce from the operating-system CSPRNG.
///
/// Record DEKs seal one message. KEKs can wrap a bounded number of child keys,
/// where uniqueness relies on the standard 96-bit random-nonce collision bound.
pub(crate) fn random_nonce() -> [u8; NONCE_LEN] {
    let mut bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut bytes);
    bytes
}

/// Construct an AES-256-GCM cipher from raw key bytes.
fn cipher(key: &[u8; KEY_LEN]) -> Aes256Gcm {
    Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key))
}

/// Seal `plaintext` under `key` and `nonce`, binding `aad`.
///
/// The returned bytes are the ciphertext with the 16-byte GCM authentication tag
/// appended.
pub(crate) fn seal(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, Error> {
    cipher(key)
        .encrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| Error::Encryption)
}

/// Open a `seal`ed body under `key` and `nonce`, requiring the same `aad`.
///
/// Returns [`Error::Decryption`] if authentication fails for any reason (wrong
/// key, wrong nonce, tampered ciphertext, or mismatched `aad`).
pub(crate) fn open(
    key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
    aad: &[u8],
) -> Result<Vec<u8>, Error> {
    cipher(key)
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| Error::Decryption)
}
