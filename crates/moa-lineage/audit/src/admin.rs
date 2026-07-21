//! Admin helpers for compliance lineage audit reads, verification, export, and erasure.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::signing::verify_audit_root_signature;
use crate::{
    AuditError, AuditRootSignaturePayload, AuditRootSigner, DsarBundle, DsarExporter, PiiVault,
    Result, SigningKey, blake3_merkle_root,
};
use moa_lineage_core::chain::{HashChain, hash_from_slice};

/// One compliance lineage row used by hash-chain verification.
#[derive(Debug, Clone)]
pub struct ComplianceRow {
    /// Turn identifier for the row.
    pub turn_id: Uuid,
    /// Numeric lineage record kind.
    pub record_kind: i16,
    /// Timestamp when the row was captured.
    pub ts: DateTime<Utc>,
    /// Canonical lineage payload.
    pub payload: Value,
    /// Stored integrity hash.
    pub integrity_hash: Vec<u8>,
    /// Stored previous-row hash.
    pub prev_hash: Option<Vec<u8>>,
}

/// Verification result for a lineage hash-chain window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerificationReport {
    /// Number of records verified.
    pub records: usize,
}

/// Stored audit root window metadata.
#[derive(Debug, Clone)]
pub struct AuditRootRow {
    /// Audit root identifier.
    pub root_id: Uuid,
    /// Storage partition covered by the root.
    pub storage_partition_id: String,
    /// Root window start timestamp.
    pub window_start: DateTime<Utc>,
    /// Root window end timestamp.
    pub window_end: DateTime<Utc>,
    /// Stored Merkle root bytes.
    pub merkle_root: Vec<u8>,
    /// Number of records covered by the root.
    pub record_count: u64,
    /// Stored audit-root signature bytes.
    pub signature: Vec<u8>,
    /// Signing key label recorded for this root.
    pub signing_key_label: String,
    /// Stored manifest hash or object ETag recorded at publish time.
    pub s3_object_etag: String,
    /// Object Lock mode recorded for the root manifest.
    pub object_lock_mode: String,
    /// Retain-until timestamp recorded for the root manifest.
    pub retain_until: DateTime<Utc>,
}

impl AuditRootRow {
    /// Returns the canonical signature payload represented by this row.
    #[must_use]
    pub fn signature_payload(&self) -> AuditRootSignaturePayload {
        AuditRootSignaturePayload::new(
            self.root_id,
            self.storage_partition_id.clone(),
            self.window_start,
            self.window_end,
            self.record_count,
            &self.merkle_root,
            self.retain_until,
            self.object_lock_mode.clone(),
            self.signing_key_label.clone(),
        )
    }
}

/// Loads a published audit root by root UUID or S3 object URI.
pub async fn load_audit_root(
    pool: &PgPool,
    storage_partition_id: &str,
    id_or_uri: &str,
) -> Result<AuditRootRow> {
    let row = if let Ok(root_id) = Uuid::parse_str(id_or_uri) {
        sqlx::query(
            r#"
            SELECT root_id, storage_partition_id, window_start, window_end, record_count,
                   merkle_root, signature, signing_key_label, s3_object_etag,
                   object_lock_mode, retain_until
            FROM analytics.audit_roots
            WHERE storage_partition_id = $1 AND root_id = $2
            "#,
        )
        .bind(storage_partition_id)
        .bind(root_id)
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query(
            r#"
            SELECT root_id, storage_partition_id, window_start, window_end, record_count,
                   merkle_root, signature, signing_key_label, s3_object_etag,
                   object_lock_mode, retain_until
            FROM analytics.audit_roots
            WHERE storage_partition_id = $1 AND s3_object_uri = $2
            "#,
        )
        .bind(storage_partition_id)
        .bind(id_or_uri)
        .fetch_one(pool)
        .await?
    };
    let record_count: i64 = row.try_get("record_count")?;
    let record_count = u64::try_from(record_count)
        .map_err(|_| AuditError::Invalid("audit root record_count is negative".to_string()))?;
    Ok(AuditRootRow {
        root_id: row.try_get("root_id")?,
        storage_partition_id: row.try_get("storage_partition_id")?,
        window_start: row.try_get("window_start")?,
        window_end: row.try_get("window_end")?,
        merkle_root: row.try_get("merkle_root")?,
        record_count,
        signature: row.try_get("signature")?,
        signing_key_label: row.try_get("signing_key_label")?,
        s3_object_etag: row.try_get("s3_object_etag")?,
        object_lock_mode: row.try_get("object_lock_mode")?,
        retain_until: row.try_get("retain_until")?,
    })
}

/// Loads compliance rows for a relative hot-store interval.
pub async fn load_compliance_rows_for_interval(
    pool: &PgPool,
    storage_partition_id: &str,
    since: &str,
) -> Result<Vec<ComplianceRow>> {
    load_compliance_rows(
        sqlx::query(
            r#"
            SELECT turn_id, record_kind, ts, payload, integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE storage_partition_id = $1
              AND prev_hash IS NOT NULL
              AND ts > now() - ($2::text)::interval
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            "#,
        )
        .bind(storage_partition_id)
        .bind(since)
        .fetch_all(pool)
        .await?,
    )
}

/// Loads compliance rows for an exact audit-root window.
pub async fn load_compliance_rows_for_window(
    pool: &PgPool,
    storage_partition_id: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
) -> Result<Vec<ComplianceRow>> {
    load_compliance_rows(
        sqlx::query(
            r#"
            SELECT turn_id, record_kind, ts, payload, integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE storage_partition_id = $1
              AND prev_hash IS NOT NULL
              AND ts >= $2
              AND ts <= $3
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            "#,
        )
        .bind(storage_partition_id)
        .bind(window_start)
        .bind(window_end)
        .fetch_all(pool)
        .await?,
    )
}

/// Counts visible dead-lettered lineage rows for one storage partition and optional root window.
pub async fn count_lineage_dead_letter_rows(
    pool: &PgPool,
    storage_partition_id: &str,
    window: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> Result<u64> {
    let (window_start, window_end) = match window {
        Some((start, end)) => (Some(start), Some(end)),
        None => (None, None),
    };
    let count: Option<i64> = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM analytics.lineage_dead_letters dead
        CROSS JOIN LATERAL jsonb_array_elements(dead.rows) AS pending(row_json)
        WHERE COALESCE(
            pending.row_json -> 'row' ->> 'storage_partition_id',
            dead.first_storage_partition_id
        ) = $1
          AND (
            $2::timestamptz IS NULL
            OR NULLIF(pending.row_json -> 'row' ->> 'ts', '')::timestamptz >= $2
          )
          AND (
            $3::timestamptz IS NULL
            OR NULLIF(pending.row_json -> 'row' ->> 'ts', '')::timestamptz <= $3
          )
        "#,
    )
    .bind(storage_partition_id)
    .bind(window_start)
    .bind(window_end)
    .fetch_one(pool)
    .await?;
    let count = count.unwrap_or_default();
    u64::try_from(count)
        .map_err(|_| AuditError::Invalid("dead-letter row count is negative".to_string()))
}

/// Verifies hash-chain links and an optional Merkle root for compliance rows.
pub fn verify_compliance_rows(
    rows: Vec<ComplianceRow>,
    expected_root: Option<Vec<u8>>,
) -> Result<VerificationReport> {
    let mut leaves = Vec::with_capacity(rows.len());
    let mut previous_integrity: Option<&[u8]> = None;
    for (index, row) in rows.iter().enumerate() {
        if let (Some(previous), Some(prev_hash)) = (previous_integrity, row.prev_hash.as_deref())
            && prev_hash != previous
        {
            return Err(AuditError::ChainMismatch {
                index,
                message: format!(
                    "chain link mismatch at turn={} kind={} ts={}",
                    row.turn_id, row.record_kind, row.ts
                ),
            });
        }
        let prev = row.prev_hash.as_deref().map(hash_from_slice).transpose()?;
        let (actual, _) = HashChain::link(prev, &row.payload)?;
        if actual.as_bytes() != row.integrity_hash.as_slice() {
            return Err(AuditError::ChainMismatch {
                index,
                message: format!(
                    "chain mismatch at turn={} kind={} ts={}",
                    row.turn_id, row.record_kind, row.ts
                ),
            });
        }
        previous_integrity = Some(&row.integrity_hash);
        leaves.push(row.integrity_hash.clone());
    }
    if let Some(expected_root) = expected_root {
        let actual_root = blake3_merkle_root(&leaves)?;
        if actual_root.as_bytes() != expected_root.as_slice() {
            return Err(AuditError::Invalid(
                "merkle root mismatch for verified window".to_string(),
            ));
        }
    }
    Ok(VerificationReport {
        records: rows.len(),
    })
}

/// Verifies compliance rows against a stored audit root and its signature.
pub async fn verify_audit_root_window(
    rows: Vec<ComplianceRow>,
    root: &AuditRootRow,
    signing: &dyn AuditRootSigner,
) -> Result<VerificationReport> {
    let expected_label = signing.key_id_for(&root.storage_partition_id);
    if root.signing_key_label != expected_label {
        return Err(AuditError::Invalid(format!(
            "audit root signing key label mismatch: stored={}, configured={expected_label}",
            root.signing_key_label,
        )));
    }
    if root.object_lock_mode.trim().is_empty() {
        return Err(AuditError::Invalid(
            "audit root object_lock_mode is empty".to_string(),
        ));
    }
    if root.s3_object_etag.trim().is_empty() {
        return Err(AuditError::Invalid(
            "audit root manifest hash/etag is empty".to_string(),
        ));
    }

    let report = verify_compliance_rows(rows, Some(root.merkle_root.clone()))?;
    let verified_count = u64::try_from(report.records)
        .map_err(|_| AuditError::Invalid("verified record count overflow".to_string()))?;
    if verified_count != root.record_count {
        return Err(AuditError::Invalid(format!(
            "audit root record count mismatch: stored={}, verified={verified_count}",
            root.record_count
        )));
    }
    let payload = root.signature_payload();
    let signed = signing.sign_root(&payload).await?;
    signed.verify_payload(&payload)?;
    verify_audit_root_signature(&payload, &root.signature, &signed.verifying_key)?;
    Ok(report)
}

/// Writes a DSAR bundle from already-collected lineage records.
pub async fn export_dsar_bundle(
    signing: SigningKey,
    storage_partition_id: &str,
    subject: &str,
    records: Vec<Value>,
    bundle_path: &Path,
) -> Result<DsarBundle> {
    let exporter = DsarExporter::new(signing);
    exporter
        .export_records(
            storage_partition_id,
            subject.as_bytes().to_vec(),
            records,
            Vec::new(),
            bundle_path,
        )
        .await
}

/// Marks a lineage PII-vault subject pseudonym as erased.
pub async fn erase_subject_pseudonym(
    pool: &PgPool,
    storage_partition_id: &str,
    subject_pseudonym: &[u8],
    secret: Vec<u8>,
    key_handle: &str,
) -> Result<u64> {
    let vault = PiiVault::with_pool(pool.clone(), secret, key_handle);
    vault
        .erase_subject(storage_partition_id, subject_pseudonym)
        .await
}

fn load_compliance_rows(rows: Vec<sqlx::postgres::PgRow>) -> Result<Vec<ComplianceRow>> {
    rows.into_iter()
        .map(|row| {
            Ok(ComplianceRow {
                turn_id: row.try_get("turn_id")?,
                record_kind: row.try_get("record_kind")?,
                ts: row.try_get("ts")?,
                payload: row.try_get("payload")?,
                integrity_hash: row.try_get("integrity_hash")?,
                prev_hash: row.try_get("prev_hash")?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{AuditRootRow, ComplianceRow, verify_audit_root_window, verify_compliance_rows};
    use crate::blake3_merkle_root;
    use crate::{
        AuditError, AuditRootSeed, AuditRootSigner, LocalAuditRootSigner, PerTenantAuditRootSigner,
    };
    use chrono::Utc;
    use moa_lineage_core::chain::HashChain;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn verify_compliance_rows_pins_chain_and_merkle_root() {
        // Pins: compliance audit verification checks exact hash-chain links and the published Merkle root.
        let first_payload = json!({"record": {"kind": "first"}});
        let second_payload = json!({"record": {"kind": "second"}});
        let (first_hash, first_prev) =
            HashChain::link(None, &first_payload).expect("first hash should compute");
        let (second_hash, second_prev) =
            HashChain::link(Some(first_hash), &second_payload).expect("second hash should compute");
        let leaves = vec![
            first_hash.as_bytes().to_vec(),
            second_hash.as_bytes().to_vec(),
        ];
        let root = blake3_merkle_root(&leaves).expect("root should compute");
        let rows = vec![
            ComplianceRow {
                turn_id: Uuid::new_v4(),
                record_kind: 1,
                ts: Utc::now(),
                payload: first_payload,
                integrity_hash: first_hash.as_bytes().to_vec(),
                prev_hash: first_prev.map(|hash| hash.as_bytes().to_vec()),
            },
            ComplianceRow {
                turn_id: Uuid::new_v4(),
                record_kind: 2,
                ts: Utc::now(),
                payload: second_payload,
                integrity_hash: second_hash.as_bytes().to_vec(),
                prev_hash: second_prev.map(|hash| hash.as_bytes().to_vec()),
            },
        ];

        let report = verify_compliance_rows(rows, Some(root.as_bytes().to_vec()))
            .expect("valid chain should verify");

        assert_eq!(report.records, 2);
    }

    #[test]
    fn verify_compliance_rows_rejects_broken_chain_link() {
        // Pins: compliance audit verification detects a stored prev_hash that does not match the previous row.
        let first_payload = json!({"record": {"kind": "first"}});
        let second_payload = json!({"record": {"kind": "second"}});
        let (first_hash, first_prev) =
            HashChain::link(None, &first_payload).expect("first hash should compute");
        let (second_hash, _) =
            HashChain::link(Some(first_hash), &second_payload).expect("second hash should compute");
        let rows = vec![
            ComplianceRow {
                turn_id: Uuid::new_v4(),
                record_kind: 1,
                ts: Utc::now(),
                payload: first_payload,
                integrity_hash: first_hash.as_bytes().to_vec(),
                prev_hash: first_prev.map(|hash| hash.as_bytes().to_vec()),
            },
            ComplianceRow {
                turn_id: Uuid::new_v4(),
                record_kind: 2,
                ts: Utc::now(),
                payload: second_payload,
                integrity_hash: second_hash.as_bytes().to_vec(),
                prev_hash: Some([9_u8; 32].to_vec()),
            },
        ];

        let error = verify_compliance_rows(rows, None)
            .expect_err("broken chain link should fail verification");

        assert!(matches!(error, AuditError::ChainMismatch { index: 1, .. }));
    }

    #[tokio::test]
    async fn verify_audit_root_window_rejects_record_count_mismatch() {
        // Pins: audit-root verification checks the signed/stored record count, not just Merkle bytes.
        let key = crate::SigningKey::from_seed("audit-root", [5_u8; 32]);
        let payload = json!({"record": {"kind": "only"}});
        let (hash, prev_hash) = HashChain::link(None, &payload).expect("hash should compute");
        let root_hash =
            blake3_merkle_root(&[hash.as_bytes().to_vec()]).expect("root should compute");
        let now = Utc::now();
        let mut root = AuditRootRow {
            root_id: Uuid::now_v7(),
            storage_partition_id: "tenant-storage-partition".to_string(),
            window_start: now,
            window_end: now,
            merkle_root: root_hash.as_bytes().to_vec(),
            record_count: 2,
            signature: Vec::new(),
            signing_key_label: key.label().to_string(),
            s3_object_etag: "manifest-hash".to_string(),
            object_lock_mode: "COMPLIANCE".to_string(),
            retain_until: now,
        };
        root.signature = key
            .sign_audit_root(&root.signature_payload())
            .expect("signature should compute");

        let rows = vec![ComplianceRow {
            turn_id: Uuid::now_v7(),
            record_kind: 1,
            ts: now,
            payload,
            integrity_hash: hash.as_bytes().to_vec(),
            prev_hash: prev_hash.map(|hash| hash.as_bytes().to_vec()),
        }];

        let signer = LocalAuditRootSigner::new(key);
        let error = verify_audit_root_window(rows, &root, &signer)
            .await
            .expect_err("record-count mismatch should fail");

        assert!(
            matches!(error, AuditError::Invalid(message) if message.contains("record count mismatch"))
        );
    }

    #[tokio::test]
    async fn verify_audit_root_window_rejects_metadata_signature_tampering() {
        // Pins: audit-root signature verification covers object-lock metadata.
        let key = crate::SigningKey::from_seed("audit-root", [6_u8; 32]);
        let payload = json!({"record": {"kind": "only"}});
        let (hash, prev_hash) = HashChain::link(None, &payload).expect("hash should compute");
        let root_hash =
            blake3_merkle_root(&[hash.as_bytes().to_vec()]).expect("root should compute");
        let now = Utc::now();
        let mut root = AuditRootRow {
            root_id: Uuid::now_v7(),
            storage_partition_id: "tenant-storage-partition".to_string(),
            window_start: now,
            window_end: now,
            merkle_root: root_hash.as_bytes().to_vec(),
            record_count: 1,
            signature: Vec::new(),
            signing_key_label: key.label().to_string(),
            s3_object_etag: "manifest-hash".to_string(),
            object_lock_mode: "COMPLIANCE".to_string(),
            retain_until: now,
        };
        root.signature = key
            .sign_audit_root(&root.signature_payload())
            .expect("signature should compute");
        root.object_lock_mode = "GOVERNANCE".to_string();

        let rows = vec![ComplianceRow {
            turn_id: Uuid::now_v7(),
            record_kind: 1,
            ts: now,
            payload,
            integrity_hash: hash.as_bytes().to_vec(),
            prev_hash: prev_hash.map(|hash| hash.as_bytes().to_vec()),
        }];

        let signer = LocalAuditRootSigner::new(key);
        let error = verify_audit_root_window(rows, &root, &signer)
            .await
            .expect_err("metadata tampering should fail");

        assert!(matches!(error, crate::AuditError::Signature));
    }

    #[tokio::test]
    async fn verify_audit_root_window_accepts_per_tenant_signed_root() {
        // Pins: a root signed with a tenant's derived key verifies under the
        // per-tenant signer, including the partition-scoped signing-key label.
        let signer = PerTenantAuditRootSigner::new(AuditRootSeed::from_bytes([15_u8; 32]));
        let partition = "tenant-storage-partition";
        let signing_key = signer.signing_key_for(partition);
        let payload = json!({"record": {"kind": "only"}});
        let (hash, prev_hash) = HashChain::link(None, &payload).expect("hash should compute");
        let root_hash =
            blake3_merkle_root(&[hash.as_bytes().to_vec()]).expect("root should compute");
        let now = Utc::now();
        let mut root = AuditRootRow {
            root_id: Uuid::now_v7(),
            storage_partition_id: partition.to_string(),
            window_start: now,
            window_end: now,
            merkle_root: root_hash.as_bytes().to_vec(),
            record_count: 1,
            signature: Vec::new(),
            signing_key_label: signing_key.label().to_string(),
            s3_object_etag: "manifest-hash".to_string(),
            object_lock_mode: "COMPLIANCE".to_string(),
            retain_until: now,
        };
        root.signature = signing_key
            .sign_audit_root(&root.signature_payload())
            .expect("signature should compute");

        let rows = vec![ComplianceRow {
            turn_id: Uuid::now_v7(),
            record_kind: 1,
            ts: now,
            payload,
            integrity_hash: hash.as_bytes().to_vec(),
            prev_hash: prev_hash.map(|hash| hash.as_bytes().to_vec()),
        }];

        let report = verify_audit_root_window(rows, &root, &signer)
            .await
            .expect("per-tenant signed root should verify");

        assert_eq!(report.records, 1);
    }

    #[tokio::test]
    async fn verify_audit_root_window_rejects_root_signed_by_other_tenant_key() {
        // Pins: verification does not weaken cross-tenant — a root carrying tenant
        // A's label but signed with another tenant's derived key fails the
        // signature check even though the label check passes.
        let signer = PerTenantAuditRootSigner::new(AuditRootSeed::from_bytes([16_u8; 32]));
        let partition = "tenant-a";
        let other_tenant_key = signer.signing_key_for("tenant-b");
        let payload = json!({"record": {"kind": "only"}});
        let (hash, prev_hash) = HashChain::link(None, &payload).expect("hash should compute");
        let root_hash =
            blake3_merkle_root(&[hash.as_bytes().to_vec()]).expect("root should compute");
        let now = Utc::now();
        let mut root = AuditRootRow {
            root_id: Uuid::now_v7(),
            storage_partition_id: partition.to_string(),
            window_start: now,
            window_end: now,
            merkle_root: root_hash.as_bytes().to_vec(),
            record_count: 1,
            signature: Vec::new(),
            // Attacker sets the correct per-tenant label so the label check passes.
            signing_key_label: signer.key_id_for(partition),
            s3_object_etag: "manifest-hash".to_string(),
            object_lock_mode: "COMPLIANCE".to_string(),
            retain_until: now,
        };
        // ...but signs with tenant B's derived key.
        root.signature = other_tenant_key
            .sign_audit_root(&root.signature_payload())
            .expect("signature should compute");

        let rows = vec![ComplianceRow {
            turn_id: Uuid::now_v7(),
            record_kind: 1,
            ts: now,
            payload,
            integrity_hash: hash.as_bytes().to_vec(),
            prev_hash: prev_hash.map(|hash| hash.as_bytes().to_vec()),
        }];

        let error = verify_audit_root_window(rows, &root, &signer)
            .await
            .expect_err("a cross-tenant forged signature must fail verification");

        assert!(matches!(error, AuditError::Signature));
    }
}
