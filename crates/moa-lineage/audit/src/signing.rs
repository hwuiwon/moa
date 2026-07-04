//! Ed25519 signing helpers for audit-root manifests.

use std::sync::RwLock;

use async_trait::async_trait;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey as DalekSigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
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

/// HTTP-backed audit-root signer that delegates key ownership to a signer service.
pub struct HttpAuditRootSigner {
    endpoint: reqwest::Url,
    key_id: String,
    bearer_token: String,
    client: reqwest::Client,
    verifying_key: RwLock<Option<[u8; 32]>>,
}

impl HttpAuditRootSigner {
    /// Creates an HTTP audit-root signer with a default Reqwest client.
    pub fn new(
        endpoint: impl Into<String>,
        key_id: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self> {
        Self::with_client(endpoint, key_id, bearer_token, reqwest::Client::new())
    }

    /// Creates an HTTP audit-root signer with an injected Reqwest client.
    pub fn with_client(
        endpoint: impl Into<String>,
        key_id: impl Into<String>,
        bearer_token: impl Into<String>,
        client: reqwest::Client,
    ) -> Result<Self> {
        let endpoint = endpoint.into();
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return Err(AuditError::Invalid(
                "audit signer endpoint is required".to_string(),
            ));
        }
        let endpoint = reqwest::Url::parse(endpoint).map_err(|error| {
            AuditError::Invalid(format!("audit signer endpoint is invalid: {error}"))
        })?;
        let key_id = key_id.into();
        let key_id = key_id.trim();
        if key_id.is_empty() {
            return Err(AuditError::Invalid(
                "audit signer key_id is required".to_string(),
            ));
        }
        let bearer_token = bearer_token.into();
        let bearer_token = bearer_token.trim();
        if bearer_token.is_empty() {
            return Err(AuditError::Invalid(
                "audit signer bearer token is required".to_string(),
            ));
        }
        Ok(Self {
            endpoint,
            key_id: key_id.to_string(),
            bearer_token: bearer_token.to_string(),
            client,
            verifying_key: RwLock::new(None),
        })
    }
}

#[async_trait]
impl AuditRootSigner for HttpAuditRootSigner {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn verifying_key(&self) -> Result<[u8; 32]> {
        let guard = self.verifying_key.read().map_err(|_| {
            AuditError::Invalid("audit signer verifying-key cache is poisoned".to_string())
        })?;
        guard.ok_or_else(|| {
            AuditError::Invalid(
                "http audit signer verifying key is unavailable before sign_root".to_string(),
            )
        })
    }

    async fn sign_root(&self, payload: &AuditRootSignaturePayload) -> Result<AuditRootSignature> {
        let canonical_payload = canonical_json_bytes(payload)?;
        let response = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(&self.bearer_token)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(canonical_payload.clone())
            .send()
            .await
            .map_err(http_signer_error)?
            .error_for_status()
            .map_err(http_signer_error)?
            .json::<HttpAuditRootSignerResponse>()
            .await
            .map_err(http_signer_error)?;
        if response.key_id != self.key_id {
            return Err(AuditError::Invalid(format!(
                "audit signer key_id mismatch: configured={}, response={}",
                self.key_id, response.key_id
            )));
        }
        let signature = decode_response_base64("signature_b64", &response.signature_b64)?;
        let verifying_key = decode_response_verifying_key(&response.verifying_key_b64)?;
        let signed = AuditRootSignature {
            key_id: response.key_id,
            verifying_key,
            signature,
        };
        signed.verify_payload(payload)?;
        let mut guard = self.verifying_key.write().map_err(|_| {
            AuditError::Invalid("audit signer verifying-key cache is poisoned".to_string())
        })?;
        *guard = Some(signed.verifying_key);
        Ok(signed)
    }
}

#[derive(Debug, Deserialize)]
struct HttpAuditRootSignerResponse {
    key_id: String,
    signature_b64: String,
    verifying_key_b64: String,
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

fn decode_response_base64(field: &'static str, value: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(value.trim())
        .map_err(|error| {
            AuditError::Invalid(format!(
                "audit signer response {field} is not base64: {error}"
            ))
        })
}

fn http_signer_error(error: reqwest::Error) -> AuditError {
    AuditError::Invalid(format!("audit signer http: {error}"))
}

fn decode_response_verifying_key(value: &str) -> Result<[u8; 32]> {
    let bytes = decode_response_base64("verifying_key_b64", value)?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| AuditError::Invalid("audit signer verifying key must be 32 bytes".to_string()))
}

#[cfg(test)]
mod tests {
    use super::{
        AuditRootSignaturePayload, AuditRootSigner, HttpAuditRootSigner, LocalAuditRootSigner,
        SigningKey,
    };
    use base64::Engine as _;
    use chrono::Utc;
    use moa_lineage_core::chain::canonical_json_bytes;
    use serde_json::json;
    use uuid::Uuid;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    #[tokio::test]
    async fn http_audit_root_signer_sends_canonical_payload_and_verifies_key_id() {
        // Pins: the HTTP signer sends canonical audit-root JSON and accepts only the configured key id.
        let key = SigningKey::from_seed("http-audit-root", [13_u8; 32]);
        let payload = audit_root_payload(key.label());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sign/audit-root"))
            .and(header("authorization", "Bearer signer-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(signer_response(
                &key,
                &payload,
                key.label(),
            )))
            .mount(&server)
            .await;
        let signer = HttpAuditRootSigner::new(
            format!("{}/sign/audit-root", server.uri()),
            key.label(),
            "signer-token",
        )
        .expect("http signer config should be valid");

        let signed = signer
            .sign_root(&payload)
            .await
            .expect("http signer should sign payload");

        assert_eq!(signed.key_id, key.label());
        assert_eq!(signed.verifying_key, key.verifying_key_bytes());
        signed
            .verify_payload(&payload)
            .expect("http signer response should verify against payload");
        let request = only_request(&server).await;
        assert_eq!(
            request.body,
            canonical_json_bytes(&payload).expect("payload should canonicalize")
        );
    }

    #[tokio::test]
    async fn http_audit_root_signer_rejects_mismatched_key_id() {
        // Pins: the HTTP signer fails closed when the signer service returns a different key id.
        let key = SigningKey::from_seed("http-audit-root", [14_u8; 32]);
        let payload = audit_root_payload(key.label());
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sign/audit-root"))
            .respond_with(ResponseTemplate::new(200).set_body_json(signer_response(
                &key,
                &payload,
                "other-key",
            )))
            .mount(&server)
            .await;
        let signer = HttpAuditRootSigner::new(
            format!("{}/sign/audit-root", server.uri()),
            key.label(),
            "signer-token",
        )
        .expect("http signer config should be valid");

        let error = signer
            .sign_root(&payload)
            .await
            .expect_err("mismatched key id should fail");

        assert!(
            matches!(error, crate::AuditError::Invalid(ref message) if message.contains("key_id mismatch")),
            "expected key_id mismatch, got {error:?}"
        );
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

    fn signer_response(
        key: &SigningKey,
        payload: &AuditRootSignaturePayload,
        key_id: &str,
    ) -> serde_json::Value {
        let signature = key
            .sign_audit_root(payload)
            .expect("test signing key should sign payload");
        json!({
            "key_id": key_id,
            "signature_b64": base64::engine::general_purpose::STANDARD.encode(signature),
            "verifying_key_b64": base64::engine::general_purpose::STANDARD.encode(key.verifying_key_bytes()),
        })
    }

    async fn only_request(server: &MockServer) -> wiremock::Request {
        let requests = server
            .received_requests()
            .await
            .expect("wiremock should expose captured signer requests");
        assert_eq!(requests.len(), 1, "expected exactly one signer request");
        requests
            .into_iter()
            .next()
            .expect("captured signer request should exist")
    }
}
