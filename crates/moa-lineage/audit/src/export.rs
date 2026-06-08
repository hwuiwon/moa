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
use serde_json::Value;
use uuid::Uuid;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::chain::canonical_json_bytes;
use crate::error::{AuditError, Result};
use crate::signing::SigningKey;

const PHI_REDACTION_TOKEN: &str = "[redacted:phi]";
const CLASSIFIED_PHI_VALUE_KEYS: &[&str] = &["field_value", "raw", "text", "value"];
const LINEAGE_PHI_FIELD_KEYS: &[&str] =
    &["answer_text", "cited_text", "query_original", "raw_text"];

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

/// Options for JSONL DSAR exports.
#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    /// Redact PHI-classified fields before writing the export.
    pub redact_phi: bool,
    /// Fixed export timestamp for deterministic callers and tests.
    pub exported_at: Option<DateTime<Utc>>,
}

/// Result metadata for a JSONL DSAR export.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DsarJsonlExport {
    /// Path to the JSONL records file.
    pub jsonl_path: String,
    /// Path to the signed manifest file.
    pub manifest_path: String,
    /// Number of records written.
    pub record_count: u64,
    /// BLAKE3 hash of the exact JSONL bytes.
    pub file_hash_b3: String,
    /// Signature over the canonical manifest claims hash.
    pub manifest_signature: Vec<u8>,
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

    /// Writes a filtered JSONL DSAR export and a signed sibling `manifest.json`.
    ///
    /// The input records are treated as the caller's already-isolated snapshot:
    /// concurrent writes after this method is called are not observed unless the
    /// caller includes them in `records`.
    pub async fn export_jsonl_records(
        &self,
        workspace_id: &str,
        user_id: &str,
        records: Vec<Value>,
        out_path: &Path,
        options: ExportOptions,
    ) -> Result<DsarJsonlExport> {
        let mut filtered = Vec::new();
        for mut record in records
            .into_iter()
            .filter(|record| record.get("user_id").and_then(Value::as_str) == Some(user_id))
        {
            if options.redact_phi {
                redact_phi_fields(&mut record);
            }
            filtered.push(record);
        }

        let mut jsonl = Vec::new();
        for record in &filtered {
            serde_json::to_writer(&mut jsonl, record)?;
            jsonl.push(b'\n');
        }
        tokio::fs::write(out_path, &jsonl).await?;

        let file_hash = blake3::hash(&jsonl).to_hex().to_string();
        let exported_at = options.exported_at.unwrap_or_else(Utc::now);
        let manifest_claims = serde_json::json!({
            "version": "1",
            "workspace_id": workspace_id,
            "user_id": user_id,
            "record_count": filtered.len() as u64,
            "file_hash_b3": file_hash,
            "timestamp": exported_at,
        });
        let signed_root = blake3::hash(&canonical_json_bytes(&manifest_claims)?);
        let signature = self
            .signing
            .sign_root(signed_root.as_bytes(), workspace_id)?;
        let manifest = serde_json::json!({
            "version": "1",
            "workspace_id": workspace_id,
            "user_id": user_id,
            "record_count": filtered.len() as u64,
            "file_hash_b3": file_hash,
            "timestamp": exported_at,
            "signed_root_b3": signed_root.to_hex().to_string(),
            "signing_key_label": self.signing.label(),
            "signature_b64": base64::engine::general_purpose::STANDARD.encode(&signature),
            "verifying_key_b64": base64::engine::general_purpose::STANDARD
                .encode(self.signing.verifying_key_bytes()),
        });
        let manifest_path = out_path.with_file_name("manifest.json");
        tokio::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?).await?;

        Ok(DsarJsonlExport {
            jsonl_path: out_path.display().to_string(),
            manifest_path: manifest_path.display().to_string(),
            record_count: filtered.len() as u64,
            file_hash_b3: file_hash,
            manifest_signature: signature,
        })
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

fn redact_phi_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let classified_phi = ["privacy_class", "classification", "class"]
                .iter()
                .any(|key| {
                    map.get(*key)
                        .and_then(Value::as_str)
                        .is_some_and(|value| value.eq_ignore_ascii_case("phi"))
                });
            for (key, child) in map.iter_mut() {
                if should_redact_phi_field(key, classified_phi) {
                    *child = Value::String(PHI_REDACTION_TOKEN.to_string());
                } else {
                    redact_phi_fields(child);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_phi_fields(item);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn should_redact_phi_field(key: &str, classified_phi: bool) -> bool {
    let key = key.to_ascii_lowercase();
    LINEAGE_PHI_FIELD_KEYS.contains(&key.as_str())
        || (classified_phi && CLASSIFIED_PHI_VALUE_KEYS.contains(&key.as_str()))
}

#[cfg(test)]
mod tests {
    use tempfile::NamedTempFile;

    use crate::export::{DsarExporter, ExportOptions};
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

    #[tokio::test]
    async fn jsonl_export_redacts_lineage_phi_fields_without_class_marker() {
        let file = NamedTempFile::new().expect("temp file");
        let key = SigningKey::from_seed("dev", [3_u8; 32]);
        let exporter = DsarExporter::new(key);

        let export = exporter
            .export_jsonl_records(
                "workspace",
                "user-1",
                vec![serde_json::json!({
                    "user_id": "user-1",
                    "query_original": "patient alice@example.com asked about SSN 123-45-6789",
                    "answer_text": "diagnosis details",
                    "citations": [
                        {
                            "cited_text": "lab result with PHI",
                            "source": "chart"
                        }
                    ],
                    "metadata": {
                        "privacy_class": "none",
                        "text": "non-PHI metadata text"
                    }
                })],
                file.path(),
                ExportOptions {
                    redact_phi: true,
                    exported_at: None,
                },
            )
            .await
            .expect("jsonl export");

        let exported =
            std::fs::read_to_string(&export.jsonl_path).expect("exported jsonl should be readable");
        assert!(exported.contains("\"query_original\":\"[redacted:phi]\""));
        assert!(exported.contains("\"answer_text\":\"[redacted:phi]\""));
        assert!(exported.contains("\"cited_text\":\"[redacted:phi]\""));
        assert!(exported.contains("non-PHI metadata text"));
        assert!(!exported.contains("alice@example.com"));
        assert!(!exported.contains("123-45-6789"));
    }
}
