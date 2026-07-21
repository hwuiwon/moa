//! # moa-kms — persistent, self-hosted key management for envelope encryption
//!
//! This crate provides a durable
//! [`KeyManagementProvider`](moa_crypto::KeyManagementProvider) backed by Postgres so
//! MOA's envelope encryption and crypto-shred survive process restarts. It is the
//! production counterpart to `moa_crypto`'s in-process
//! [`LocalKmsProvider`](moa_crypto::LocalKmsProvider), whose KEKs live only in
//! memory and are lost on restart (making encrypted data unreadable and
//! crypto-shred a no-op after a restart).
//!
//! ## Placement
//!
//! `moa_crypto` is a dependency-free leaf holding the trait, value types, and
//! AEAD. This crate adds the persistent backend, so it depends on `moa_crypto`
//! (trait + types), `moa-db` (row-level-security-scoped connections), and `sqlx`.
//! The graph store and erase path keep depending only on the `moa_crypto` trait
//! (`Arc<dyn KeyManagementProvider>`); the concrete [`PostgresKmsProvider`] is
//! injected at the composition root.
//!
//! ## Key hierarchy: root key → per-subject KEK → per-record DEK
//!
//! A deployment [`RootKeyRing`] (the "key-encryption keys of KEKs") is loaded
//! from a mounted directory and NEVER lands in the database. Each
//! `(tenant_id, subject_id)` pair owns one key-encryption key (KEK), stored in
//! `moa.kek` wrapped under the root key with AES-256-GCM (AAD binds
//! tenant|subject|kek id). Each record's data-encryption key (DEK) is wrapped
//! under its subject's KEK. Because KEKs persist in Postgres, a fresh provider
//! after a restart — same pool, same root key — unwraps DEKs sealed before it.
//!
//! ## Crypto-shred
//!
//! Destroying a subject's KEK
//! ([`destroy_subject_key`](moa_crypto::KeyManagementProvider::destroy_subject_key))
//! tombstones its `moa.kek` row (sets `destroyed_at`, zeroes `wrapped_kek`), so
//! every DEK wrapped under it becomes permanently un-unwrappable and all of that
//! subject's ciphertext is durably irrecoverable — the erasure primitive the
//! privacy erase path calls.
//!
//! ## Root-key rotation
//!
//! Shared Postgres state selects the active generation for new KEKs. Bounded,
//! restart-safe jobs rewrap historical KEKs while every pod keeps all referenced
//! generations mounted. No correctness state or plaintext KEK cache is local to
//! a Kubernetes replica.

#![deny(missing_docs)]

pub mod error;
pub mod provider;
pub mod root_key;

pub use error::KmsError;
pub use provider::{PostgresKmsProvider, RootKeyState};
pub use root_key::{ROOT_KEY_LEN, RootKeyRing};
