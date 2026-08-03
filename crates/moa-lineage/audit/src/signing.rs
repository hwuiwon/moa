//! Ed25519 signing helpers for audit-root manifests.

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey as DalekSigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::error::{AuditError, Result};
use moa_core::canonical_json::canonical_json_bytes;

/// Stable `key_id()` reported by the per-tenant signer as a whole. Individual
/// roots carry a partition-specific label from [`per_tenant_key_label`].
const PER_TENANT_SIGNER_KEY_ID: &str = "audit-root:per-tenant";

/// Label prefix applied to per-tenant audit-root signing keys.
const PER_TENANT_KEY_LABEL_PREFIX: &str = "audit-root:";

/// Domain-separation tag mixed into per-tenant seed derivation so the root seed
/// cannot be confused with any other key material derived from the same bytes.
const PER_TENANT_SEED_DOMAIN: &[u8] = b"moa.lineage.audit.per-tenant-root.v1";

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

/// Signature bytes and public key material returned for one audit-root payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRootSignature {
    /// Stable signing key identifier.
    pub key_id: String,
    /// Ed25519 verifying key bytes.
    pub verifying_key: [u8; 32],
    /// Ed25519 signature bytes.
    pub signature: Vec<u8>,
}

impl AuditRootSignature {
    /// Verifies this signature against a canonical audit-root payload.
    pub fn verify_payload(&self, payload: &AuditRootSignaturePayload) -> Result<()> {
        verify_audit_root_signature(payload, &self.signature, &self.verifying_key)
    }
}

/// Object-safe signer used by audit-root publishing and verification surfaces.
#[async_trait]
pub trait AuditRootSigner: Send + Sync {
    /// Returns the stable signing key identifier.
    fn key_id(&self) -> &str;

    /// Returns the signing key label expected for one storage partition.
    ///
    /// Single-key signers ignore the partition and return their global
    /// [`key_id`](Self::key_id); per-tenant signers derive a partition-specific
    /// label so verification binds each root to its tenant's key.
    fn key_id_for(&self, _storage_partition_id: &str) -> String {
        self.key_id().to_string()
    }

    /// Returns the Ed25519 verifying key bytes when locally available.
    fn verifying_key(&self) -> Result<[u8; 32]>;

    /// Signs a canonical audit-root payload.
    async fn sign_root(&self, payload: &AuditRootSignaturePayload) -> Result<AuditRootSignature>;
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
        verify_audit_root_signature(payload, signature, &self.verifying_key_bytes())
    }
}

/// Local audit-root signer backed by an in-process Ed25519 signing key.
#[derive(Clone)]
pub struct LocalAuditRootSigner {
    signing_key: SigningKey,
}

impl LocalAuditRootSigner {
    /// Creates a local audit-root signer from an existing signing key.
    #[must_use]
    pub fn new(signing_key: SigningKey) -> Self {
        Self { signing_key }
    }
}

#[async_trait]
impl AuditRootSigner for LocalAuditRootSigner {
    fn key_id(&self) -> &str {
        self.signing_key.label()
    }

    fn verifying_key(&self) -> Result<[u8; 32]> {
        Ok(self.signing_key.verifying_key_bytes())
    }

    async fn sign_root(&self, payload: &AuditRootSignaturePayload) -> Result<AuditRootSignature> {
        Ok(AuditRootSignature {
            key_id: self.key_id().to_string(),
            verifying_key: self.signing_key.verifying_key_bytes(),
            signature: self.signing_key.sign_audit_root(payload)?,
        })
    }
}

/// Deployment root seed used to derive per-tenant audit-root signing keys.
///
/// The seed is a deployment secret. It is held in a zeroizing wrapper, is never
/// logged, and is never serialized; only derived per-tenant labels and key
/// identifiers are safe to record.
#[derive(Clone)]
pub struct AuditRootSeed(Zeroizing<[u8; 32]>);

impl AuditRootSeed {
    /// Wraps 32 bytes of deployment secret material as a root seed.
    #[must_use]
    pub fn from_bytes(seed: [u8; 32]) -> Self {
        Self(Zeroizing::new(seed))
    }
}

impl std::fmt::Debug for AuditRootSeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuditRootSeed(<redacted>)")
    }
}

/// Returns the per-tenant signing key label for one storage partition.
#[must_use]
pub fn per_tenant_key_label(storage_partition_id: &str) -> String {
    format!("{PER_TENANT_KEY_LABEL_PREFIX}{storage_partition_id}")
}

/// Deterministically derives a per-tenant Ed25519 seed from the deployment root
/// seed and a storage partition id.
///
/// Uses a BLAKE3 keyed hash (a PRF) with the root seed as the key and a
/// domain-separated storage partition id as the message, so distinct partitions
/// yield independent 32-byte seeds and the same inputs always reproduce the same
/// seed. Compromise of one tenant's derived key reveals nothing about the root
/// seed or any other tenant's key.
fn derive_tenant_seed(root_seed: &[u8; 32], storage_partition_id: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_keyed(root_seed);
    hasher.update(PER_TENANT_SEED_DOMAIN);
    hasher.update(&[0x00]);
    hasher.update(storage_partition_id.as_bytes());
    *hasher.finalize().as_bytes()
}

/// Per-tenant audit-root signer that derives a distinct Ed25519 key per storage
/// partition from a single deployment root seed.
///
/// Each tenant's published audit root is signed with a key derived from
/// `(root_seed, storage_partition_id)`, so one tenant's key compromise cannot
/// forge another tenant's trail, and each tenant can independently verify its
/// own chain. Derivation is deterministic, so no per-tenant key needs to be
/// stored: the same root seed reproduces the exact verifying key.
#[derive(Clone)]
pub struct PerTenantAuditRootSigner {
    root_seed: AuditRootSeed,
}

impl PerTenantAuditRootSigner {
    /// Creates a per-tenant signer from a deployment root seed.
    #[must_use]
    pub fn new(root_seed: AuditRootSeed) -> Self {
        Self { root_seed }
    }

    /// Derives the full Ed25519 signing key (label and key material) for one
    /// storage partition. This is the key an audit-root publisher signs with.
    #[must_use]
    pub fn signing_key_for(&self, storage_partition_id: &str) -> SigningKey {
        let seed = derive_tenant_seed(&self.root_seed.0, storage_partition_id);
        SigningKey::from_seed(per_tenant_key_label(storage_partition_id), seed)
    }

    /// Derives the Ed25519 verifying key bytes for one storage partition.
    #[must_use]
    pub fn verifying_key_for(&self, storage_partition_id: &str) -> [u8; 32] {
        self.signing_key_for(storage_partition_id)
            .verifying_key_bytes()
    }

    /// Verifies an audit-root signature against the tenant key derived from the
    /// payload's storage partition.
    ///
    /// A payload signed for one tenant fails verification here for any other
    /// tenant, and any payload mutation invalidates the signature.
    pub fn verify_root(&self, payload: &AuditRootSignaturePayload, signature: &[u8]) -> Result<()> {
        let verifying_key = self.verifying_key_for(&payload.storage_partition_id);
        verify_audit_root_signature(payload, signature, &verifying_key)
    }
}

#[async_trait]
impl AuditRootSigner for PerTenantAuditRootSigner {
    fn key_id(&self) -> &str {
        PER_TENANT_SIGNER_KEY_ID
    }

    fn key_id_for(&self, storage_partition_id: &str) -> String {
        per_tenant_key_label(storage_partition_id)
    }

    fn verifying_key(&self) -> Result<[u8; 32]> {
        Err(AuditError::Invalid(
            "per-tenant audit-root signer has no single verifying key; \
             derive one per storage partition with verifying_key_for"
                .to_string(),
        ))
    }

    async fn sign_root(&self, payload: &AuditRootSignaturePayload) -> Result<AuditRootSignature> {
        let signing_key = self.signing_key_for(&payload.storage_partition_id);
        Ok(AuditRootSignature {
            key_id: signing_key.label().to_string(),
            verifying_key: signing_key.verifying_key_bytes(),
            signature: signing_key.sign_audit_root(payload)?,
        })
    }
}

pub(crate) fn verify_audit_root_signature(
    payload: &AuditRootSignaturePayload,
    signature: &[u8],
    verifying_key: &[u8; 32],
) -> Result<()> {
    let verifying_key =
        VerifyingKey::from_bytes(verifying_key).map_err(|_| AuditError::Signature)?;
    let signature = Signature::try_from(signature).map_err(|_| AuditError::Signature)?;
    verifying_key
        .verify(&canonical_json_bytes(payload)?, &signature)
        .map_err(|_| AuditError::Signature)
}

#[cfg(test)]
mod tests {
    use super::{
        AuditRootSeed, AuditRootSignaturePayload, AuditRootSigner, LocalAuditRootSigner,
        PerTenantAuditRootSigner, SigningKey, verify_audit_root_signature,
    };
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

    #[tokio::test]
    async fn local_audit_root_signer_matches_existing_signing_key_signature() {
        // Pins: the local AuditRootSigner wrapper preserves existing SigningKey audit-root bytes.
        let key = SigningKey::from_seed("audit-root", [12_u8; 32]);
        let signer = LocalAuditRootSigner::new(key.clone());
        let payload = audit_root_payload(key.label());

        let expected_signature = key
            .sign_audit_root(&payload)
            .expect("existing SigningKey should sign payload");
        let signed = signer
            .sign_root(&payload)
            .await
            .expect("local signer should sign payload");

        assert_eq!(signed.key_id, key.label());
        assert_eq!(signed.signature, expected_signature);
        assert_eq!(signed.verifying_key, key.verifying_key_bytes());
        signed
            .verify_payload(&payload)
            .expect("local signer signature should verify");
    }

    fn audit_root_payload(key_id: &str) -> AuditRootSignaturePayload {
        AuditRootSignaturePayload::new(
            Uuid::now_v7(),
            "tenant-storage-partition",
            Utc::now(),
            Utc::now(),
            42,
            &[9_u8; 32],
            Utc::now(),
            "COMPLIANCE",
            key_id,
        )
    }

    fn per_tenant_payload(storage_partition_id: &str, label: &str) -> AuditRootSignaturePayload {
        AuditRootSignaturePayload::new(
            Uuid::now_v7(),
            storage_partition_id,
            Utc::now(),
            Utc::now(),
            42,
            &[9_u8; 32],
            Utc::now(),
            "COMPLIANCE",
            label,
        )
    }

    #[tokio::test]
    async fn per_tenant_signer_derives_distinct_keys_labels_and_signatures() {
        // Pins: one deployment root seed yields a distinct Ed25519 key, label,
        // and signature per storage partition, so one tenant's compromised key
        // cannot forge another tenant's audit root.
        let signer = PerTenantAuditRootSigner::new(AuditRootSeed::from_bytes([3_u8; 32]));

        let key_a = signer.verifying_key_for("tenant-a");
        let key_b = signer.verifying_key_for("tenant-b");
        assert_ne!(
            key_a, key_b,
            "distinct partitions must derive distinct keys"
        );

        let label_a = signer.key_id_for("tenant-a");
        let label_b = signer.key_id_for("tenant-b");
        assert_eq!(label_a, "audit-root:tenant-a");
        assert_ne!(
            label_a, label_b,
            "distinct partitions must derive distinct labels"
        );

        let payload_a = per_tenant_payload("tenant-a", &label_a);
        let payload_b = per_tenant_payload("tenant-b", &label_b);
        let signed_a = signer.sign_root(&payload_a).await.expect("sign tenant a");
        let signed_b = signer.sign_root(&payload_b).await.expect("sign tenant b");

        assert_ne!(
            signed_a.signature, signed_b.signature,
            "distinct partitions must produce distinct signatures"
        );
        assert_eq!(signed_a.key_id, label_a);
        assert_eq!(signed_a.verifying_key, key_a);

        signed_a
            .verify_payload(&payload_a)
            .expect("tenant a signature should verify under tenant a key");
        signed_b
            .verify_payload(&payload_b)
            .expect("tenant b signature should verify under tenant b key");
    }

    #[tokio::test]
    async fn per_tenant_signer_key_derivation_is_deterministic() {
        // Pins: the same (root seed, storage partition) reproduces the identical
        // verifying key, so no per-tenant key needs to be stored to verify.
        let seed = [7_u8; 32];
        let signer_one = PerTenantAuditRootSigner::new(AuditRootSeed::from_bytes(seed));
        let signer_two = PerTenantAuditRootSigner::new(AuditRootSeed::from_bytes(seed));

        assert_eq!(
            signer_one.verifying_key_for("tenant-a"),
            signer_two.verifying_key_for("tenant-a"),
            "same root seed + partition must reproduce the identical verifying key"
        );

        let label = signer_one.key_id_for("tenant-a");
        let payload = per_tenant_payload("tenant-a", &label);
        let signed = signer_one.sign_root(&payload).await.expect("sign");
        signer_two
            .verify_root(&payload, &signed.signature)
            .expect("independently reproduced key should verify the signature");
    }

    #[tokio::test]
    async fn per_tenant_signer_root_does_not_verify_under_other_tenant_key() {
        // Pins: a root signed for tenant A must not verify under tenant B's
        // derived key, so a per-tenant key compromise cannot forge cross-tenant.
        let signer = PerTenantAuditRootSigner::new(AuditRootSeed::from_bytes([9_u8; 32]));
        let label_a = signer.key_id_for("tenant-a");
        let payload_a = per_tenant_payload("tenant-a", &label_a);
        let signed_a = signer.sign_root(&payload_a).await.expect("sign tenant a");

        let key_b = signer.verifying_key_for("tenant-b");
        assert!(
            verify_audit_root_signature(&payload_a, &signed_a.signature, &key_b).is_err(),
            "a root signed for tenant A must not verify under tenant B's derived key"
        );
    }

    #[tokio::test]
    async fn per_tenant_signer_rejects_payload_tampering() {
        // Pins: mutating the signed payload (record_count) invalidates the
        // per-tenant signature under the tenant-aware verification path.
        let signer = PerTenantAuditRootSigner::new(AuditRootSeed::from_bytes([11_u8; 32]));
        let label = signer.key_id_for("tenant-a");
        let payload = per_tenant_payload("tenant-a", &label);
        let signed = signer.sign_root(&payload).await.expect("sign");

        let mut tampered = payload.clone();
        tampered.record_count += 1;

        assert!(
            signer.verify_root(&tampered, &signed.signature).is_err(),
            "record-count tampering must invalidate the per-tenant signature"
        );
        assert!(
            signed.verify_payload(&tampered).is_err(),
            "record-count tampering must invalidate the returned signature"
        );
    }
}
