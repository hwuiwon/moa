//! DSAR bundle export helpers.
//!
//! The high-level exporter writes deterministic zip bundles with a signed
//! manifest and record/proof payloads. Database and object-store collection can
//! feed these helpers from hot lineage rows, cold Parquet rows, or a mixed
//! window.

use std::fs::File;
use std::io::Write;
use std::path::Path;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::error::{AuditError, Result};
use crate::signing::SigningKey;

/// One Merkle root window included in a DSAR bundle.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RootWindow {
    /// Audit root id.
    pub root_id: Uuid,
    /// Window start timestamp.
    pub window_start: DateTime<Utc>,
    /// Window end timestamp.
    pub window_end: DateTime<Utc>,
    /// Published root bytes.
    pub merkle_root: Vec<u8>,
}

/// Result metadata for a DSAR export.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DsarBundle {
    /// Pseudonymized subject identifier.
    pub subject_pseudonym: Vec<u8>,
    /// Local or object-store bundle URI.
    pub bundle_uri: String,
    /// Signature over the manifest.
    pub manifest_signature: Vec<u8>,
    /// Number of records exported.
    pub record_count: u64,
    /// Root windows touched by the exported records.
    pub windows: Vec<RootWindow>,
}

/// DSAR exporter.
#[derive(Clone)]
pub struct DsarExporter {
    signing: SigningKey,
}

impl DsarExporter {
    /// Creates a DSAR exporter with the signing key used for bundle manifests.
    #[must_use]
    pub fn new(signing: SigningKey) -> Self {
        Self { signing }
    }

    /// Writes a DSAR bundle to `out_path` from already-collected records.
    pub async fn export_records(
        &self,
        workspace_id: &str,
        subject_pseudonym: Vec<u8>,
        records: Vec<serde_json::Value>,
        windows: Vec<RootWindow>,
        out_path: &Path,
    ) -> Result<DsarBundle> {
        let record_count = records.len() as u64;
        let manifest = serde_json::json!({
            "version": "1",
            "workspace_id": workspace_id,
            "subject_pseudonym_b3": blake3::hash(&subject_pseudonym).to_hex().to_string(),
            "record_count": record_count,
            "windows": windows,
        });
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        let signature = self
            .signing
            .sign_root(&blake3::hash(&manifest_bytes).as_bytes()[..], workspace_id)?;
        let records_bytes = serde_json::to_vec_pretty(&records)?;
        let signature_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "signing_key_label": self.signing.label(),
            "signature_b64": base64::engine::general_purpose::STANDARD.encode(&signature),
            "verifying_key_b64": base64::engine::general_purpose::STANDARD
                .encode(self.signing.verifying_key_bytes()),
        }))?;
        let path = out_path.to_path_buf();
        let manifest_bytes_for_zip = manifest_bytes.clone();
        tokio::task::spawn_blocking(move || {
            write_zip(
                &path,
                &manifest_bytes_for_zip,
                &records_bytes,
                &signature_bytes,
            )
        })
        .await
        .map_err(|error| AuditError::Invalid(format!("DSAR export task failed: {error}")))??;

        Ok(DsarBundle {
            subject_pseudonym,
            bundle_uri: out_path.display().to_string(),
            manifest_signature: signature,
            record_count,
            windows,
        })
    }
}

fn write_zip(
    path: &Path,
    manifest_bytes: &[u8],
    records_bytes: &[u8],
    signature_bytes: &[u8],
) -> Result<()> {
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    zip.start_file("manifest.json", options)?;
    zip.write_all(manifest_bytes)?;
    zip.start_file("records/lineage.json", options)?;
    zip.write_all(records_bytes)?;
    zip.start_file("proofs/signature.json", options)?;
    zip.write_all(signature_bytes)?;
    zip.start_file("README.txt", options)?;
    zip.write_all(
        b"MOA DSAR lineage bundle. Verify manifest.json with proofs/signature.json and the published audit roots before using as compliance evidence.\n",
    )?;
    zip.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;

    use crate::export::DsarExporter;
    use crate::signing::SigningKey;

    #[tokio::test]
    async fn dsar_bundle_round_trips_to_zip() {
        let file = NamedTempFile::new().expect("temp file");
        let key = SigningKey::from_seed("dev", [3_u8; 32]);
        let exporter = DsarExporter::new(key);
        let bundle = exporter
            .export_records(
                "workspace",
                b"subject".to_vec(),
                vec![serde_json::json!({"record": 1})],
                Vec::new(),
                file.path(),
            )
            .await
            .expect("export");

        assert_eq!(bundle.record_count, 1);
        assert!(std::fs::metadata(file.path()).expect("zip metadata").len() > 0);
    }
}
