//! # moa-crypto — envelope encryption, BYOK, and crypto-shred foundation
//!
//! This crate is the cryptographic foundation for MOA's defense-in-depth
//! encryption of restricted data. It reverses the deferral recorded in ADR 0001
//! by giving later slices a vetted primitive to seal `pii_class`-tagged rows and
//! to cryptographically erase them.
//!
//! It provides envelope encryption over a pluggable [`KeyManagementProvider`] so
//! restricted records can be sealed with a per-record data-encryption key (DEK)
//! that is itself wrapped by a per-data-subject key-encryption key (KEK). The KEK
//! lives in a KMS/HSM (AWS KMS, GCP KMS, HashiCorp Vault — bring-your-own-key)
//! or, for development and tests, the in-process [`LocalKmsProvider`].
//!
//! ## Key hierarchy: tenant → data subject → record
//!
//! Keys nest in three levels. A tenant is the outer isolation boundary; within
//! it, each **data subject** (for example a contact) has one KEK; and each
//! record has its own DEK wrapped by its subject's KEK. Because the KEK is
//! scoped to a single `(tenant_id, subject_id)` pair, destroying it
//! cryptographically erases exactly that one data subject's records — the
//! foundation for per-subject right-to-erasure — while every other subject in
//! the tenant keeps decrypting. See [`crypto_shred_subject`].
//!
//! ## Envelope model
//!
//! Each [`encrypt`] call asks the KMS for a fresh DEK
//! ([`KeyManagementProvider::generate_data_key`]), which returns the plaintext
//! DEK, the KEK-wrapped DEK, and the [`KeyHandle`] identifying the wrapping key.
//! The plaintext DEK seals exactly one record with AES-256-GCM and is then
//! dropped (and zeroized); only the [`WrappedDek`] is persisted next to the
//! ciphertext. [`decrypt`] reverses the flow: it asks the KMS to unwrap the DEK,
//! then opens the AEAD payload.
//!
//! Because a DEK seals exactly one record, no key is ever reused across
//! messages, so AES-256-GCM's 96-bit random nonce cannot collide within a key.
//! See [`envelope`] for the full AEAD rationale.
//!
//! ## Context binding
//!
//! [`EncryptionContext`] (tenant id, subject id, record id, `pii_class`) is bound
//! as the AEAD additional authenticated data at both the DEK-wrap and
//! record-seal layers, so ciphertext cannot be replayed under a different tenant,
//! data subject, record, or classification. [`decrypt`] re-derives the context
//! from the caller and will not open a payload sealed under a different context.
//!
//! ## Crypto-shred
//!
//! Destroying a KEK makes every DEK wrapped under it permanently un-unwrappable,
//! so all ciphertext sealed with those DEKs becomes irrecoverable without ever
//! touching the ciphertext rows; unwrapping a destroyed key returns
//! [`Error::CryptoShredded`]. The subject-scoped entry point
//! [`crypto_shred_subject`] (backed by
//! [`KeyManagementProvider::destroy_subject_key`]) erases one data subject by
//! their `(tenant_id, subject_id)` identity — the primitive storage calls
//! to forget a contact. [`crypto_shred`] /
//! [`KeyManagementProvider::destroy_key`] remains for erasing a specific KEK
//! handle directly.

#![deny(missing_docs)]

mod aead;
pub mod envelope;
pub mod error;
pub mod key_wrap;
pub mod kms;
pub mod local;
pub mod types;

pub use envelope::{
    crypto_shred, crypto_shred_subject, decrypt, decrypt_batch, encrypt, encrypt_batch,
};
pub use error::Error;
pub use kms::KeyManagementProvider;
pub use local::LocalKmsProvider;
pub use types::{
    Ciphertext, DEK_LEN, DataKeyDecryptRequest, DecryptionRequest, EncryptionContext,
    EncryptionRequest, GeneratedDataKey, KeyHandle, NONCE_LEN, PlaintextDek, WrappedDek,
};
