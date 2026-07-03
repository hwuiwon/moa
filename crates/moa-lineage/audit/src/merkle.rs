//! Certificate-Transparency-style Merkle window helpers.
//!
//! The public audit root uses BLAKE3 over domain-separated leaf and node hashes.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use blake3::{Hash, Hasher};
use chrono::{DateTime, Utc};
use object_store::ObjectStore;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::{AuditError, Result};
use crate::signing::{AuditRootSignaturePayload, SigningKey};

/// Object Lock mode used when publishing audit manifests.
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ObjectLockMode {
    /// Development mode where privileged users may bypass retention.
    Governance,
    /// Production WORM mode; retention cannot be shortened or bypassed.
    Compliance,
}

impl ObjectLockMode {
    /// Returns the stable S3 header/database value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Governance => "GOVERNANCE",
            Self::Compliance => "COMPLIANCE",
        }
    }
}

/// Merkle root publisher configuration.
#[derive(Clone, Debug)]
pub struct RootPublisherConfig {
    /// Publish cadence.
    pub publish_interval: Duration,
    /// Maximum records in one window.
    pub max_window_records: usize,
    /// Maximum age of a pending window.
    pub max_window_age: Duration,
    /// Object Lock mode to request from the S3 backend.
    pub object_lock_mode: ObjectLockMode,
    /// Retention window in years.
    pub retention_years: i32,
}

impl Default for RootPublisherConfig {
    fn default() -> Self {
        Self {
            publish_interval: Duration::from_secs(300),
            max_window_records: 100_000,
            max_window_age: Duration::from_secs(900),
            object_lock_mode: ObjectLockMode::Governance,
            retention_years: 10,
        }
    }
}

/// JSON manifest stored in the audit-root object store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditRootManifest {
    /// Manifest schema version.
    pub version: String,
    /// Published root identifier.
    pub root_id: Uuid,
    /// Storage partition ID.
    pub storage_partition_id: String,
    /// Window start timestamp.
    pub window_start: DateTime<Utc>,
    /// Window end timestamp.
    pub window_end: DateTime<Utc>,
    /// Number of records in the window.
    pub record_count: u64,
    /// BLAKE3 Merkle root bytes.
    pub merkle_root_b64: String,
    /// Ed25519 signature bytes.
    pub signature_b64: String,
    /// Signing key label.
    pub signing_key_label: String,
    /// Object Lock mode requested.
    pub object_lock_mode: ObjectLockMode,
    /// Retain-until timestamp.
    pub retain_until: DateTime<Utc>,
}

/// Periodic Merkle root publisher.
pub struct MerkleRootPublisher {
    cfg: RootPublisherConfig,
    pool: sqlx::PgPool,
    store: Arc<dyn ObjectStore>,
    signing: SigningKey,
    storage_partition_id: String,
}

impl MerkleRootPublisher {
    /// Creates a publisher for one compliance-enabled storage partition.
    #[must_use]
    pub fn new(
        pool: sqlx::PgPool,
        store: Arc<dyn ObjectStore>,
        signing: SigningKey,
        storage_partition_id: impl Into<String>,
        cfg: RootPublisherConfig,
    ) -> Self {
        Self {
            cfg,
            pool,
            store,
            signing,
            storage_partition_id: storage_partition_id.into(),
        }
    }

    /// Runs the publisher until cancelled.
    pub async fn run(self, cancel: CancellationToken) {
        let mut interval = tokio::time::interval(self.cfg.publish_interval);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }
            if let Err(error) = self.publish_one_window().await {
                tracing::error!(%error, storage_partition_id = %self.storage_partition_id, "merkle root publish failed");
            }
        }
    }

    /// Publishes one available window and returns the inserted root id.
    pub async fn publish_one_window(&self) -> Result<Option<Uuid>> {
        let mut tx = self.pool.begin().await?;
        let lock_acquired: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_xact_lock(hashtext($1))")
                .bind(format!(
                    "audit-merkle-publisher:{}",
                    self.storage_partition_id
                ))
                .fetch_one(&mut *tx)
                .await?;
        if !lock_acquired {
            tx.rollback().await?;
            return Ok(None);
        }

        let rows = sqlx::query(
            r#"
            SELECT turn_id, record_kind, ts, integrity_hash
            FROM analytics.turn_lineage
            WHERE storage_partition_id = $1
              AND prev_hash IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1
                  FROM analytics.audit_roots r
                  WHERE r.storage_partition_id = analytics.turn_lineage.storage_partition_id
                    AND analytics.turn_lineage.ts >= r.window_start
                    AND analytics.turn_lineage.ts <= r.window_end
              )
            ORDER BY ts ASC, turn_id ASC, record_kind ASC
            LIMIT $2
            "#,
        )
        .bind(&self.storage_partition_id)
        .bind(i64::try_from(self.cfg.max_window_records).unwrap_or(i64::MAX))
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            tx.rollback().await?;
            return Ok(None);
        }

        let mut leaves = Vec::with_capacity(rows.len());
        let mut window_start = None;
        let mut window_end = None;
        for row in &rows {
            let ts: DateTime<Utc> = sqlx::Row::try_get(row, "ts")?;
            let hash: Vec<u8> = sqlx::Row::try_get(row, "integrity_hash")?;
            leaves.push(hash);
            window_start = Some(window_start.map_or(ts, |current: DateTime<Utc>| current.min(ts)));
            window_end = Some(window_end.map_or(ts, |current: DateTime<Utc>| current.max(ts)));
        }

        let root = blake3_merkle_root(&leaves)?;
        let window_start =
            window_start.ok_or_else(|| AuditError::Invalid("empty root window".to_string()))?;
        let window_end =
            window_end.ok_or_else(|| AuditError::Invalid("empty root window".to_string()))?;
        let root_id = stable_root_id(&self.storage_partition_id, window_start, window_end, &root);
        let retain_until = window_end
            + chrono::Duration::days(i64::from(self.cfg.retention_years).saturating_mul(365));
        let signature_payload = AuditRootSignaturePayload::new(
            root_id,
            self.storage_partition_id.clone(),
            window_start,
            window_end,
            leaves.len() as u64,
            root.as_bytes(),
            retain_until,
            self.cfg.object_lock_mode.as_str(),
            self.signing.label(),
        );
        let signature = self.signing.sign_audit_root(&signature_payload)?;
        let manifest = AuditRootManifest {
            version: "1".to_string(),
            root_id,
            storage_partition_id: self.storage_partition_id.clone(),
            window_start,
            window_end,
            record_count: leaves.len() as u64,
            merkle_root_b64: base64::engine::general_purpose::STANDARD.encode(root.as_bytes()),
            signature_b64: base64::engine::general_purpose::STANDARD.encode(&signature),
            signing_key_label: self.signing.label().to_string(),
            object_lock_mode: self.cfg.object_lock_mode,
            retain_until,
        };
        let object_path = audit_root_object_path(
            &self.storage_partition_id,
            window_start,
            window_end,
            root_id,
        );
        let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
        self.store
            .put(&object_path, manifest_bytes.clone().into())
            .await?;
        let uri = format!("object://{object_path}");

        sqlx::query(
            r#"
            INSERT INTO analytics.audit_roots (
                root_id, storage_partition_id, window_start, window_end, record_count,
                merkle_root, signature, signing_key_label, s3_object_uri,
                s3_object_etag, object_lock_mode, retain_until
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (root_id) DO NOTHING
            "#,
        )
        .bind(root_id)
        .bind(&self.storage_partition_id)
        .bind(window_start)
        .bind(window_end)
        .bind(leaves.len() as i64)
        .bind(root.as_bytes().as_slice())
        .bind(signature)
        .bind(self.signing.label())
        .bind(uri)
        .bind(blake3::hash(&manifest_bytes).to_hex().to_string())
        .bind(self.cfg.object_lock_mode.as_str())
        .bind(retain_until)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            r#"
            INSERT INTO analytics.compliance_storage_partition_state (storage_partition_id, last_root_id)
            VALUES ($1, $2)
            ON CONFLICT (storage_partition_id) DO UPDATE SET last_root_id = EXCLUDED.last_root_id
            "#,
        )
        .bind(&self.storage_partition_id)
        .bind(root_id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(root_id))
    }
}

/// Computes a BLAKE3 domain-separated Merkle root for stored integrity hashes.
pub fn blake3_merkle_root(leaves: &[Vec<u8>]) -> Result<Hash> {
    if leaves.is_empty() {
        return Err(AuditError::Invalid(
            "cannot compute a Merkle root for an empty window".to_string(),
        ));
    }
    let mut level = leaves
        .iter()
        .map(|leaf| leaf_hash(leaf))
        .collect::<Vec<_>>();
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let right = pair.get(1).unwrap_or(&pair[0]);
            next.push(node_hash(&pair[0], right));
        }
        level = next;
    }
    Ok(level[0])
}

fn stable_root_id(
    storage_partition_id: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    root: &Hash,
) -> Uuid {
    let mut hasher = Hasher::new();
    hasher.update(b"moa.audit-root.v1");
    hasher.update(storage_partition_id.as_bytes());
    hasher.update(&window_start.timestamp_millis().to_be_bytes());
    hasher.update(&window_end.timestamp_millis().to_be_bytes());
    hasher.update(root.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    Uuid::from_bytes(bytes)
}

fn audit_root_object_path(
    storage_partition_id: &str,
    window_start: DateTime<Utc>,
    window_end: DateTime<Utc>,
    root_id: Uuid,
) -> object_store::path::Path {
    object_store::path::Path::from(format!(
        "storage_partition={storage_partition_id}/window_start={}/window_end={}/root={root_id}.json",
        window_start.timestamp_millis(),
        window_end.timestamp_millis()
    ))
}

/// Builds an inclusion proof as sibling BLAKE3 hashes from leaf to root.
pub fn blake3_inclusion_proof(leaves: &[Vec<u8>], index: usize) -> Result<Vec<Vec<u8>>> {
    if leaves.is_empty() || index >= leaves.len() {
        return Err(AuditError::Invalid(
            "inclusion index out of range".to_string(),
        ));
    }
    let mut proof = Vec::new();
    let mut idx = index;
    let mut level = leaves
        .iter()
        .map(|leaf| leaf_hash(leaf))
        .collect::<Vec<_>>();
    while level.len() > 1 {
        let sibling = if idx.is_multiple_of(2) {
            level.get(idx + 1).unwrap_or(&level[idx])
        } else {
            &level[idx - 1]
        };
        proof.push(sibling.as_bytes().to_vec());
        idx /= 2;
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        for pair in level.chunks(2) {
            let right = pair.get(1).unwrap_or(&pair[0]);
            next.push(node_hash(&pair[0], right));
        }
        level = next;
    }
    Ok(proof)
}

/// Verifies a BLAKE3 inclusion proof.
pub fn verify_blake3_inclusion(
    leaf: &[u8],
    index: usize,
    proof: &[Vec<u8>],
    root: Hash,
) -> Result<()> {
    let mut idx = index;
    let mut current = leaf_hash(leaf);
    for sibling in proof {
        let sibling = hash_from_vec(sibling)?;
        current = if idx.is_multiple_of(2) {
            node_hash(&current, &sibling)
        } else {
            node_hash(&sibling, &current)
        };
        idx /= 2;
    }
    if current == root {
        Ok(())
    } else {
        Err(AuditError::Invalid(
            "BLAKE3 inclusion proof did not match root".to_string(),
        ))
    }
}

fn leaf_hash(leaf: &[u8]) -> Hash {
    let mut hasher = Hasher::new();
    hasher.update(&[0x00]);
    hasher.update(leaf);
    hasher.finalize()
}

fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = Hasher::new();
    hasher.update(&[0x01]);
    hasher.update(left.as_bytes());
    hasher.update(right.as_bytes());
    hasher.finalize()
}

fn hash_from_vec(bytes: &[u8]) -> Result<Hash> {
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AuditError::Invalid("expected a 32-byte Merkle hash".to_string()))?;
    Ok(Hash::from(array))
}

#[cfg(test)]
mod tests {
    use super::{
        audit_root_object_path, blake3_inclusion_proof, blake3_merkle_root, stable_root_id,
        verify_blake3_inclusion,
    };

    use chrono::{Duration as ChronoDuration, Utc};

    #[test]
    fn merkle_inclusion_round_trips() {
        let leaves = (0..8)
            .map(|idx| format!("record-{idx}").into_bytes())
            .collect::<Vec<_>>();
        let root = blake3_merkle_root(&leaves).expect("root");
        let proof = blake3_inclusion_proof(&leaves, 3).expect("proof");

        verify_blake3_inclusion(&leaves[3], 3, &proof, root).expect("proof verifies");
    }

    #[test]
    fn audit_root_identity_is_stable_for_replayed_window() {
        // Pins: retry after object-store success but before DB commit targets
        // the same manifest identity instead of publishing duplicate root objects.
        let leaves = vec![b"alpha".to_vec(), b"beta".to_vec()];
        let root = blake3_merkle_root(&leaves).expect("root");
        let window_start = Utc::now() - ChronoDuration::minutes(10);
        let window_end = window_start + ChronoDuration::minutes(5);

        let first = stable_root_id("partition-a", window_start, window_end, &root);
        let second = stable_root_id("partition-a", window_start, window_end, &root);

        assert_eq!(first, second);
        assert_eq!(
            audit_root_object_path("partition-a", window_start, window_end, first),
            audit_root_object_path("partition-a", window_start, window_end, second)
        );
    }
}
