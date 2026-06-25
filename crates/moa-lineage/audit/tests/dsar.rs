//! Out-of-line tests for DSAR JSONL exports and signed manifests.

mod support;

use std::sync::Arc;

use base64::Engine as _;
use chrono::{TimeZone, Utc};
use moa_lineage_audit::{DsarExporter, ExportOptions, SigningKey};
use serde_json::Value;
use support::fixture_jsonl;
use tempfile::TempDir;
use tokio::sync::Mutex;

#[tokio::test]
async fn dsar_export_for_user_with_5_records_writes_5_jsonl_lines_with_correct_schema() {
    let temp = TempDir::new().expect("tempdir should be created");
    let out_path = temp.path().join("user-001.jsonl");
    let exporter = exporter();

    exporter
        .export_jsonl_records(
            "tenant-001",
            "user-001",
            fixture_jsonl("audit_records_with_user_data.jsonl"),
            &out_path,
            fixed_options(false),
        )
        .await
        .expect("DSAR export should succeed");
    let lines = read_jsonl(&out_path);

    assert_eq!(lines.len(), 5);
    for line in lines {
        assert!(line.get("record_id").is_some(), "record_id is required");
        assert!(line.get("timestamp").is_some(), "timestamp is required");
        assert!(line.get("event_type").is_some(), "event_type is required");
        assert!(line.get("payload").is_some(), "payload is required");
        assert_eq!(line["user_id"], "user-001");
    }
}

#[tokio::test]
async fn dsar_export_for_unknown_user_writes_empty_file_with_zero_lines() {
    let temp = TempDir::new().expect("tempdir should be created");
    let out_path = temp.path().join("user-999.jsonl");
    let exporter = exporter();

    let export = exporter
        .export_jsonl_records(
            "tenant-001",
            "user-999",
            fixture_jsonl("audit_records_with_user_data.jsonl"),
            &out_path,
            fixed_options(false),
        )
        .await
        .expect("unknown user export should succeed");

    assert_eq!(export.record_count, 0);
    assert!(out_path.exists());
    assert_eq!(
        std::fs::read_to_string(&out_path).expect("JSONL file should be readable"),
        ""
    );
}

/// Tampering guard: record field `user_id=user-002` must never appear in user-001 output.
#[tokio::test]
async fn dsar_export_excludes_records_belonging_to_other_users() {
    let temp = TempDir::new().expect("tempdir should be created");
    let out_path = temp.path().join("user-001.jsonl");
    let exporter = exporter();

    exporter
        .export_jsonl_records(
            "tenant-001",
            "user-001",
            fixture_jsonl("audit_records_with_user_data.jsonl"),
            &out_path,
            fixed_options(false),
        )
        .await
        .expect("DSAR export should succeed");

    let output = std::fs::read_to_string(&out_path).expect("JSONL file should be readable");
    assert!(!output.contains("user-002"));
}

/// Redaction: `payload.fact.value` is PHI-classified and must become `[redacted:phi]`.
#[tokio::test]
async fn dsar_export_includes_redaction_for_phi_class_fields_when_redaction_enabled() {
    let temp = TempDir::new().expect("tempdir should be created");
    let out_path = temp.path().join("user-001.jsonl");
    let exporter = exporter();

    exporter
        .export_jsonl_records(
            "tenant-001",
            "user-001",
            fixture_jsonl("audit_records_with_user_data.jsonl"),
            &out_path,
            fixed_options(true),
        )
        .await
        .expect("DSAR export should succeed");
    let lines = read_jsonl(&out_path);
    let redacted = lines
        .iter()
        .find(|line| line["record_id"] == "usr1-004")
        .expect("PHI fixture record should be exported");

    assert_eq!(
        redacted["payload"]["fact"]["value"],
        serde_json::json!("[redacted:phi]")
    );
    assert!(
        !serde_json::to_string(redacted)
            .expect("redacted record should serialize")
            .contains("hypertension")
    );
}

#[tokio::test]
async fn dsar_export_signed_manifest_accompanies_jsonl_with_valid_signature() {
    let temp = TempDir::new().expect("tempdir should be created");
    let out_path = temp.path().join("user-001.jsonl");
    let key = signing_key();
    let exporter = DsarExporter::new(key.clone());

    let export = exporter
        .export_jsonl_records(
            "tenant-001",
            "user-001",
            fixture_jsonl("audit_records_with_user_data.jsonl"),
            &out_path,
            fixed_options(false),
        )
        .await
        .expect("DSAR export should succeed");
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&export.manifest_path).expect("manifest should be readable"),
    )
    .expect("manifest should parse");
    let jsonl_bytes = std::fs::read(&out_path).expect("JSONL should be readable");

    assert_eq!(manifest["record_count"], 5);
    assert_eq!(manifest["user_id"], "user-001");
    assert_eq!(
        manifest["file_hash_b3"],
        blake3::hash(&jsonl_bytes).to_hex().to_string()
    );
    assert_eq!(
        manifest["file_hash_b3"].as_str().expect("file hash string"),
        export.file_hash_b3
    );

    let signed_root = hex::decode(
        manifest["signed_root_b3"]
            .as_str()
            .expect("signed root should exist"),
    )
    .expect("signed root should decode");
    let signature = base64::engine::general_purpose::STANDARD
        .decode(
            manifest["signature_b64"]
                .as_str()
                .expect("signature should exist"),
        )
        .expect("signature should decode");
    key.verify_root(&signed_root, "tenant-001", &signature)
        .expect("manifest signature should verify");
}

/// Snapshot contract: a user-001 record appended after the input Vec is cloned is excluded.
#[tokio::test]
async fn dsar_export_with_concurrent_writes_to_audit_log_produces_consistent_snapshot() {
    let temp = TempDir::new().expect("tempdir should be created");
    let out_path = temp.path().join("user-001.jsonl");
    let exporter = exporter();
    let audit_log = Arc::new(Mutex::new(fixture_jsonl(
        "audit_records_with_user_data.jsonl",
    )));
    let records_at_export_start = audit_log.lock().await.clone();
    let writer_log = audit_log.clone();

    let (export, _) = tokio::join!(
        exporter.export_jsonl_records(
            "tenant-001",
            "user-001",
            records_at_export_start,
            &out_path,
            fixed_options(false),
        ),
        async move {
            writer_log.lock().await.push(serde_json::json!({
                "record_id": "usr1-006",
                "timestamp": "2026-05-07T12:00:08Z",
                "event_type": "late_write",
                "user_id": "user-001",
                "payload": {"message": "late record"}
            }));
        }
    );
    export.expect("DSAR export should succeed");

    let lines = read_jsonl(&out_path);
    assert_eq!(lines.len(), 5);
    assert!(
        lines.iter().all(|line| line["record_id"] != "usr1-006"),
        "late concurrent write should not appear in the export snapshot"
    );
    assert_eq!(audit_log.lock().await.len(), 9);
}

fn exporter() -> DsarExporter {
    DsarExporter::new(signing_key())
}

fn signing_key() -> SigningKey {
    SigningKey::from_seed("dsar-test", [4_u8; 32])
}

fn fixed_options(redact_phi: bool) -> ExportOptions {
    ExportOptions {
        redact_phi,
        exported_at: Some(
            Utc.with_ymd_and_hms(2026, 5, 7, 12, 0, 0)
                .single()
                .expect("fixed timestamp should be valid"),
        ),
    }
}

fn read_jsonl(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("JSONL file should be readable")
        .lines()
        .map(|line| serde_json::from_str(line).expect("JSONL line should parse"))
        .collect()
}
