//! Ed25519 signing helpers for audit-root manifests.

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Signer, SigningKey as DalekSigningKey, Verifier, VerifyingKey};
use tokio::fs;

use crate::chain::canonical_json_bytes;
use crate::error::{AuditError, Result};

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

    /// Signs a Merkle root plus canonical workspace metadata.
    pub fn sign_root(&self, root: &[u8], workspace_id: &str) -> Result<Vec<u8>> {
        let metadata = serde_json::json!({
            "workspace_id": workspace_id,
            "signing_key_label": self.label,
        });
        let mut message = Vec::with_capacity(root.len() + 128);
        message.extend_from_slice(root);
        message.extend_from_slice(&canonical_json_bytes(&metadata)?);
        Ok(self.inner.sign(&message).to_bytes().to_vec())
    }

    /// Verifies a Merkle root signature.
    pub fn verify_root(&self, root: &[u8], workspace_id: &str, signature: &[u8]) -> Result<()> {
        let metadata = serde_json::json!({
            "workspace_id": workspace_id,
            "signing_key_label": self.label,
        });
        let mut message = Vec::with_capacity(root.len() + 128);
        message.extend_from_slice(root);
        message.extend_from_slice(&canonical_json_bytes(&metadata)?);
        let signature = Signature::try_from(signature).map_err(|_| AuditError::Signature)?;
        self.verifying
            .verify(&message, &signature)
            .map_err(|_| AuditError::Signature)
    }
}

/// Signing key vault abstraction.
#[async_trait::async_trait]
pub trait SigningKeyVault: Send + Sync {
    /// Loads a signing key by label.
    async fn get(&self, label: &str) -> Result<SigningKey>;
    /// Rotates a signing key label and returns the new key.
    async fn rotate(&self, label: &str) -> Result<SigningKey>;
    /// Lists known signing key labels.
    async fn list(&self) -> Result<Vec<String>>;
}

/// Local development signing vault backed by 32-byte seed files.
pub struct LocalSigningKeyVault {
    root: PathBuf,
}

impl LocalSigningKeyVault {
    /// Creates a local signing key vault rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, label: &str) -> PathBuf {
        self.root.join(format!("{label}.seed"))
    }
}

#[async_trait::async_trait]
impl SigningKeyVault for LocalSigningKeyVault {
    async fn get(&self, label: &str) -> Result<SigningKey> {
        let path = self.path_for(label);
        let seed = load_or_create_seed(&path, label).await?;
        Ok(SigningKey::from_seed(label.to_string(), seed))
    }

    async fn rotate(&self, label: &str) -> Result<SigningKey> {
        fs::create_dir_all(&self.root).await?;
        let seed = deterministic_seed(&format!("{label}:{}", uuid::Uuid::now_v7()));
        fs::write(self.path_for(label), seed).await?;
        Ok(SigningKey::from_seed(label.to_string(), seed))
    }

    async fn list(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let mut entries = match fs::read_dir(&self.root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("seed")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            {
                out.push(stem.to_string());
            }
        }
        out.sort();
        Ok(out)
    }
}

async fn load_or_create_seed(path: &Path, label: &str) -> Result<[u8; 32]> {
    match fs::read(path).await {
        Ok(bytes) => bytes
            .as_slice()
            .try_into()
            .map_err(|_| AuditError::Invalid("signing seed must be 32 bytes".to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).await?;
            }
            let seed = deterministic_seed(label);
            fs::write(path, seed).await?;
            Ok(seed)
        }
        Err(error) => Err(error.into()),
    }
}

fn deterministic_seed(label: &str) -> [u8; 32] {
    *blake3::hash(format!("moa-lineage-audit-dev-key:{label}").as_bytes()).as_bytes()
}

#[cfg(test)]
mod tests {
    use super::SigningKey;

    #[test]
    fn signing_roundtrip_rejects_tampering() {
        let key = SigningKey::from_seed("dev", [7_u8; 32]);
        let root = [9_u8; 32];
        let signature = key.sign_root(&root, "workspace").expect("sign");

        key.verify_root(&root, "workspace", &signature)
            .expect("verify");
        assert!(
            key.verify_root(&[8_u8; 32], "workspace", &signature)
                .is_err()
        );
    }
}
