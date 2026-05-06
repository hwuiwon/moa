//! Opt-in compliance audit tier for MOA lineage.
//!
//! This crate contains the engineering primitives for compliance-grade lineage:
//! canonical BLAKE3 chain hashes, Merkle root publishing, Ed25519 signing,
//! PII pseudonymization, typed decision records, DSAR bundle export, and local
//! verification helpers.
//!
//! # Attestation gate
//!
//! Do not represent this implementation as compliance evidence to customers,
//! auditors, regulators, or certification bodies until external cryptographic
//! review is complete and linked from the architecture documentation. The
//! `ct-merkle` crate used for RFC 6962 proof shape support is explicitly not
//! audited by its authors. Internal engineering forensics are not blocked by
//! that gate.

pub mod chain;
pub mod decision;
pub mod error;
pub mod export;
pub mod merkle;
pub mod signing;
pub mod vault;

pub use chain::{HashChain, canonical_json_bytes, canonical_payload_hash, hash_from_slice};
pub use decision::{
    AclFilterDecision, DecisionKind, DecisionRecord, PiiRedactionDecision, PrivacyEraseDecision,
    PrivacyExportDecision, ScopeEnforcementDecision,
};
pub use error::{AuditError, Result};
pub use export::{DsarBundle, DsarExporter, RootWindow};
pub use merkle::{
    AuditRootManifest, MerkleRootPublisher, ObjectLockMode, RootPublisherConfig,
    blake3_inclusion_proof, blake3_merkle_root, ct_sha256_root, verify_blake3_inclusion,
};
pub use signing::{LocalSigningKeyVault, SigningKey, SigningKeyVault};
pub use vault::{PiiVault, PseudonymizationOutcome, RedactionEvent};
