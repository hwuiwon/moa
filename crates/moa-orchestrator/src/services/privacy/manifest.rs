//! Privacy export manifest signing and archive construction.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use ed25519_dalek::{Signer as DalekSigner, SigningKey};
use flate2::Compression;
use flate2::write::GzEncoder;
use restate_sdk::prelude::*;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tar::Builder;
use tokio::process::Command;
use uuid::Uuid;

use super::approval::decode_key_material;
use super::context::PrivacyExportContext;
use super::{handler_error, usize_to_u64};

const EXPORT_SIGNING_KEY_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX";
const EXPORT_SIGNING_KEY_ID_ENV: &str = "MOA_PRIVACY_EXPORT_SIGNING_KEY_ID";

/// Ed25519 signer for generated privacy export manifests.
pub struct Ed25519ManifestSigner {
    /// Stable key identifier recorded in manifests.
    pub key_id: String,
    /// Ed25519 signing key.
    pub signing_key: SigningKey,
}

impl Ed25519ManifestSigner {
    /// Builds a manifest signer from configured signing key environment.
    pub fn from_env() -> Result<Self, HandlerError> {
        let raw = std::env::var(EXPORT_SIGNING_KEY_ENV)
            .map_err(|_| TerminalError::new(format!("{EXPORT_SIGNING_KEY_ENV} is required")))?;
        let key_id = std::env::var(EXPORT_SIGNING_KEY_ID_ENV)
            .unwrap_or_else(|_| "moa-privacy-export-ops".to_string());
        Self::from_signing_key_material(key_id, &raw)
    }

    /// Builds a manifest signer from hex or base64 private key material.
    pub fn from_signing_key_material(key_id: String, raw: &str) -> Result<Self, HandlerError> {
        let bytes = decode_key_material(raw)?;
        let seed = match bytes.len() {
            32 => bytes,
            64 => bytes[..32].to_vec(),
            len => {
                return Err(TerminalError::new_with_code(
                    400,
                    format!("export signing key must be 32 or 64 bytes, got {len}"),
                )
                .into());
            }
        };
        let seed: [u8; 32] = seed.as_slice().try_into().map_err(|_| {
            TerminalError::new_with_code(400, "export signing key must be 32 bytes")
        })?;
        Ok(Self {
            key_id,
            signing_key: SigningKey::from_bytes(&seed),
        })
    }

    /// Returns the manifest key identifier.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Returns the Ed25519 public key as hex.
    #[must_use]
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.signing_key.verifying_key().to_bytes())
    }

    /// Signs exact manifest bytes.
    pub fn sign(&self, bytes: &[u8]) -> Vec<u8> {
        self.signing_key.sign(bytes).to_bytes().to_vec()
    }
}

#[derive(Debug, Serialize)]
struct Manifest<'a> {
    version: u8,
    created_at: String,
    subject_user_id: &'a str,
    subjects: Vec<ManifestSubject<'a>>,
    storage_partition: Option<&'a str>,
    encryption: &'static str,
    signature: ManifestSignature<'a>,
    files: Vec<ManifestFile>,
    counts: BTreeMap<&'static str, usize>,
}

#[derive(Debug, Serialize)]
struct ManifestSubject<'a> {
    user_id: &'a str,
    provenance: &'static str,
}

#[derive(Debug, Serialize)]
struct ManifestSignature<'a> {
    algorithm: &'static str,
    signature_file: &'static str,
    key_id: &'a str,
    public_key_hex: String,
}

#[derive(Debug, Serialize)]
struct ManifestFile {
    name: String,
    size: u64,
    sha256: String,
    blake3: String,
}

/// Writes and signs `manifest.json`, returning the manifest JSON value.
pub async fn write_manifest(
    export_dir: &Path,
    signer: &Ed25519ManifestSigner,
    ctx: &PrivacyExportContext,
    counts: &BTreeMap<&'static str, usize>,
) -> Result<Value, HandlerError> {
    let mut files = Vec::new();
    let mut entries = tokio::fs::read_dir(export_dir)
        .await
        .map_err(handler_error)?;
    while let Some(entry) = entries.next_entry().await.map_err(handler_error)? {
        let path = entry.path();
        if !entry.file_type().await.map_err(handler_error)?.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name == "manifest.json" || name == "manifest.sig" {
            continue;
        }
        let bytes = tokio::fs::read(&path).await.map_err(handler_error)?;
        files.push(ManifestFile {
            name: name.to_string(),
            size: usize_to_u64(bytes.len()),
            sha256: sha256_hex(&bytes),
            blake3: blake3::hash(&bytes).to_hex().to_string(),
        });
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));

    let manifest = Manifest {
        version: 1,
        created_at: Utc::now().to_rfc3339(),
        subject_user_id: &ctx.subject_user_id,
        subjects: ctx
            .subjects
            .iter()
            .map(|subject| ManifestSubject {
                user_id: subject.user_id.as_str(),
                provenance: subject.provenance.as_str(),
            })
            .collect(),
        storage_partition: ctx.storage_partition.as_deref(),
        encryption: "none",
        signature: ManifestSignature {
            algorithm: "Ed25519",
            signature_file: "manifest.sig",
            key_id: signer.key_id(),
            public_key_hex: signer.public_key_hex(),
        },
        files,
        counts: counts.clone(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(handler_error)?;
    tokio::fs::write(export_dir.join("manifest.json"), &manifest_bytes)
        .await
        .map_err(handler_error)?;
    tokio::fs::write(
        export_dir.join("manifest.sig"),
        signer.sign(&manifest_bytes),
    )
    .await
    .map_err(handler_error)?;
    serde_json::from_slice(&manifest_bytes).map_err(handler_error)
}

/// Creates a gzipped tar archive from an export directory and returns its bytes.
pub async fn finalize_archive_to_bytes(
    export_dir: &Path,
    pgp_recipient: Option<&str>,
) -> Result<Vec<u8>, HandlerError> {
    let parent = export_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    let target = parent.join("subject.tgz");
    let export_dir_for_archive = export_dir.to_path_buf();
    let target_for_archive = target.clone();
    tokio::task::spawn_blocking(move || -> Result<(), HandlerError> {
        let file = std::fs::File::create(&target_for_archive).map_err(handler_error)?;
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = Builder::new(encoder);
        archive
            .append_dir_all("export", &export_dir_for_archive)
            .map_err(handler_error)?;
        let encoder = archive.into_inner().map_err(handler_error)?;
        encoder.finish().map_err(handler_error)?;
        Ok(())
    })
    .await
    .map_err(handler_error)??;

    if let Some(recipient) = pgp_recipient {
        let encrypted = encrypt_with_gpg(&target, &parent, recipient).await?;
        return tokio::fs::read(encrypted).await.map_err(handler_error);
    }

    tokio::fs::read(target).await.map_err(handler_error)
}

async fn encrypt_with_gpg(
    target: &Path,
    parent: &Path,
    recipient: &str,
) -> Result<PathBuf, HandlerError> {
    let recipient_path = parent.join("recipient.asc");
    tokio::fs::write(&recipient_path, recipient)
        .await
        .map_err(handler_error)?;
    let output = parent.join("subject.tgz.gpg");
    let status = Command::new("gpg")
        .arg("--batch")
        .arg("--yes")
        .arg("--encrypt")
        .arg("--recipient-file")
        .arg(&recipient_path)
        .arg("--output")
        .arg(&output)
        .arg(target)
        .status()
        .await
        .map_err(handler_error)?;
    if !status.success() {
        return Err(
            TerminalError::new(format!("gpg encryption failed with status {status}")).into(),
        );
    }
    Ok(output)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Creates a temporary directory for one privacy export run.
pub(super) async fn create_temp_dir(prefix: &str) -> Result<PathBuf, HandlerError> {
    let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::now_v7()));
    tokio::fs::create_dir_all(&path)
        .await
        .map_err(handler_error)?;
    Ok(path)
}

/// Removes a temporary privacy export directory and logs cleanup failures.
pub(super) async fn cleanup_temp_dir(path: &Path) {
    if let Err(error) = tokio::fs::remove_dir_all(path).await {
        tracing::warn!(path = %path.display(), %error, "failed to remove privacy export staging directory");
    }
}
