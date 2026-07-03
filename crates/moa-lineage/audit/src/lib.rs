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
//! review is complete and linked from the architecture documentation. Internal
//! engineering forensics are not blocked by that gate.

pub mod admin;
pub mod error;
pub mod export;
pub mod merkle;
pub mod signing;
pub mod vault;

pub use error::{AuditError, Result};
pub use export::{DsarBundle, DsarExporter, DsarJsonlExport, ExportOptions, RootWindow};
pub use merkle::{
    AuditRootManifest, MerkleRootPublisher, ObjectLockMode, RootPublisherConfig,
    blake3_inclusion_proof, blake3_merkle_root, verify_blake3_inclusion,
};
pub use signing::{AuditRootSignaturePayload, SigningKey};
pub use vault::{PiiVault, PseudonymizationOutcome, RedactionEvent};
