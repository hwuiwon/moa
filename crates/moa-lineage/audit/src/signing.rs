//! Ed25519 signing helpers for audit-root manifests.

use base64::Engine as _;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey as DalekSigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use uuid::Uuid;

use crate::error::{AuditError, Result};
use moa_lineage_core::chain::canonical_json_bytes;

/// Canonical payload signed for a published audit root.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct AuditRootSignaturePayload {
    /// Published root identifier.
    pub root_id: Uuid,
    /// Storage partition ID covered by the root.
    pub storage_partition_id: String,
    /// Root window start timestamp.
    pub window_start: DateTime<Utc>,
    /// Root window end timestamp.
    pub window_end: DateTime<Utc>,
    /// Number of records covered by the root.
    pub record_count: u64,
    /// BLAKE3 Merkle root bytes encoded as base64.
    pub merkle_root_b64: String,
    /// Retain-until timestamp recorded for the object-lock manifest.
    pub retain_until: DateTime<Utc>,
    /// Object Lock mode requested for the root manifest.
    pub object_lock_mode: String,
    /// Signing key label expected to verify this payload.
    pub signing_key_label: String,
}

impl AuditRootSignaturePayload {
    /// Builds the canonical payload for audit-root signatures.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        root_id: Uuid,
        storage_partition_id: impl Into<String>,
        window_start: DateTime<Utc>,
        window_end: DateTime<Utc>,
        record_count: u64,
        merkle_root: &[u8],
        retain_until: DateTime<Utc>,
        object_lock_mode: impl Into<String>,
        signing_key_label: impl Into<String>,
    ) -> Self {
        Self {
            root_id,
            storage_partition_id: storage_partition_id.into(),
            window_start,
            window_end,
            record_count,
            merkle_root_b64: base64::engine::general_purpose::STANDARD.encode(merkle_root),
            retain_until,
            object_lock_mode: object_lock_mode.into(),
            signing_key_label: signing_key_label.into(),
        }
    }
}

/// Ed25519 signing key handle.
#[derive(Clone)]
pub struct SigningKey {
    label: String,
    inner: DalekSigningKey,
    verifying: VerifyingKey,
}

impl SigningKey {
    /// Creates a signing key from a 32-byte seed.
    #[must_use]
    pub fn from_seed(label: impl Into<String>, seed: [u8; 32]) -> Self {
        let inner = DalekSigningKey::from_bytes(&seed);
        let verifying = inner.verifying_key();
        Self {
            label: label.into(),
            inner,
            verifying,
        }
    }

    /// Returns this key's stable label.
    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the verifying key bytes.
    #[must_use]
    pub fn verifying_key_bytes(&self) -> [u8; 32] {
        self.verifying.to_bytes()
    }

    /// Signs a published audit-root metadata payload.
    pub fn sign_audit_root(&self, payload: &AuditRootSignaturePayload) -> Result<Vec<u8>> {
        Ok(self
            .inner
            .sign(&canonical_json_bytes(payload)?)
            .to_bytes()
            .to_vec())
    }

    /// Signs an arbitrary byte message with this Ed25519 key.
    pub fn sign_message(&self, message: &[u8]) -> Vec<u8> {
        self.inner.sign(message).to_bytes().to_vec()
    }

    /// Verifies an arbitrary byte message signature with this key's public key.
    pub fn verify_message(&self, message: &[u8], signature: &[u8]) -> Result<()> {
        let signature = Signature::try_from(signature).map_err(|_| AuditError::Signature)?;
        self.verifying
            .verify(message, &signature)
            .map_err(|_| AuditError::Signature)
    }

    /// Verifies a published audit-root metadata signature.
    pub fn verify_audit_root(
        &self,
        payload: &AuditRootSignaturePayload,
        signature: &[u8],
    ) -> Result<()> {
        let signature = Signature::try_from(signature).map_err(|_| AuditError::Signature)?;
        self.verifying
            .verify(&canonical_json_bytes(payload)?, &signature)
            .map_err(|_| AuditError::Signature)
    }
}

#[cfg(test)]
mod tests {
    use super::{AuditRootSignaturePayload, SigningKey};
    use chrono::Utc;
    use uuid::Uuid;

    #[test]
    fn signing_roundtrip_rejects_tampering() {
        let key = SigningKey::from_seed("dev", [7_u8; 32]);
        let message = [9_u8; 32];
        let signature = key.sign_message(&message);

        key.verify_message(&message, &signature).expect("verify");
        assert!(key.verify_message(&[8_u8; 32], &signature).is_err());
    }

    #[test]
    fn audit_root_signature_binds_window_and_object_lock_metadata() {
        // Pins: audit-root signatures cover more than the Merkle root and storage-partition label.
        let key = SigningKey::from_seed("audit-root", [11_u8; 32]);
        let payload = AuditRootSignaturePayload::new(
            Uuid::now_v7(),
            "tenant-storage-partition",
            Utc::now(),
            Utc::now(),
            42,
            &[9_u8; 32],
            Utc::now(),
            "COMPLIANCE",
            key.label(),
        );
        let signature = key.sign_audit_root(&payload).expect("sign");
        key.verify_audit_root(&payload, &signature)
            .expect("signature should verify");

        let mut tampered = payload.clone();
        tampered.record_count += 1;

        assert!(
            key.verify_audit_root(&tampered, &signature).is_err(),
            "record-count tampering must invalidate the audit-root signature"
        );
    }
}
