//! Tenant compliance hash-chain folding and persisted chain state.

use chrono::{DateTime, Utc};
use moa_lineage_core::chain::{HashChain, hash_from_slice};
use sqlx::Row;
use uuid::Uuid;

use crate::Result;

use super::rows::LineageRow;

/// In-memory outcome of folding one compliance partition's hash chain over a flush batch.
pub(super) struct PartitionChainOutcome {
    /// Chain tip after the last newly linked row, when at least one row was linked.
    pub(super) final_hash: Option<Vec<u8>>,
    /// Timestamp of the last newly linked row, mirrored into the partition state.
    pub(super) last_ts: Option<DateTime<Utc>>,
    /// Number of rows newly linked into the chain (existing rows are reused, not re-linked).
    pub(super) new_rows: usize,
}

/// Applies compliance hash chaining to a flush batch with per-partition (not per-row) round trips.
///
/// The enabled-tenant check runs once for the whole batch. For each compliance-enabled partition
/// the advisory lock is taken once, the chain tip is read once `FOR UPDATE`, already-persisted
/// rows are fetched once for idempotent reuse, the chain is folded in memory, and the partition
/// state is updated once. This turns a `512`-row batch from `512+` serial round trips into a small
/// constant per distinct partition while producing the same chain as a per-row walk.
pub(super) async fn apply_compliance_hashes(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rows: &mut [LineageRow],
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let partitions: Vec<String> = rows
        .iter()
        .map(|row| row.storage_partition_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let enabled: std::collections::HashSet<String> = sqlx::query_scalar::<_, String>(
        r#"
        SELECT storage_partition_id
        FROM analytics.compliance_tenants
        WHERE storage_partition_id = ANY($1) AND enabled
        "#,
    )
    .bind(&partitions)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect();
    if enabled.is_empty() {
        return Ok(());
    }

    // Group row indices by enabled partition, preserving batch order within a partition so the
    // folded chain matches a per-row walk. Locking in sorted partition order keeps the advisory
    // lock acquisition order consistent across concurrent writers.
    let mut by_partition: std::collections::BTreeMap<String, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (idx, row) in rows.iter().enumerate() {
        if enabled.contains(&row.storage_partition_id) {
            by_partition
                .entry(row.storage_partition_id.clone())
                .or_default()
                .push(idx);
        }
    }

    for (partition, indices) in by_partition {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("compliance:{partition}"))
            .execute(&mut **tx)
            .await?;

        sqlx::query(
            r#"
            INSERT INTO analytics.compliance_storage_partition_state (storage_partition_id)
            VALUES ($1)
            ON CONFLICT (storage_partition_id) DO NOTHING
            "#,
        )
        .bind(&partition)
        .execute(&mut **tx)
        .await?;

        let prev_hash: Option<Vec<u8>> = sqlx::query_scalar(
            r#"
            SELECT last_integrity_hash
            FROM analytics.compliance_storage_partition_state
            WHERE storage_partition_id = $1
            FOR UPDATE
            "#,
        )
        .bind(&partition)
        .fetch_one(&mut **tx)
        .await?;

        let existing = fetch_existing_chain_rows(tx, &partition, rows, &indices).await?;
        let outcome = fold_partition_chain(prev_hash.as_deref(), rows, &indices, &existing)?;

        if outcome.new_rows > 0 {
            sqlx::query(
                r#"
                UPDATE analytics.compliance_storage_partition_state
                SET last_integrity_hash = $2,
                    last_ts = $3,
                    record_count = record_count + $4
                WHERE storage_partition_id = $1
                "#,
            )
            .bind(&partition)
            .bind(&outcome.final_hash)
            .bind(outcome.last_ts)
            .bind(outcome.new_rows as i64)
            .execute(&mut **tx)
            .await?;
        }
    }
    Ok(())
}

/// Map from a lineage row identity to its already-persisted `(integrity_hash, prev_hash)`.
pub(super) type ExistingChainRows =
    std::collections::HashMap<(Uuid, i16, DateTime<Utc>), (Vec<u8>, Option<Vec<u8>>)>;

/// Fetches already-persisted chain hashes for the batch's rows in one query for idempotent reuse.
async fn fetch_existing_chain_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    partition: &str,
    rows: &[LineageRow],
    indices: &[usize],
) -> Result<ExistingChainRows> {
    let turn_ids: Vec<Uuid> = indices.iter().map(|&idx| rows[idx].turn_id).collect();
    let record_kinds: Vec<i16> = indices.iter().map(|&idx| rows[idx].record_kind).collect();
    let timestamps: Vec<DateTime<Utc>> = indices.iter().map(|&idx| rows[idx].ts).collect();

    let existing_rows = sqlx::query(
        r#"
        SELECT turn_id, record_kind, ts, integrity_hash, prev_hash
        FROM analytics.turn_lineage
        WHERE storage_partition_id = $1
          AND (turn_id, record_kind, ts) IN (
            SELECT * FROM UNNEST($2::uuid[], $3::smallint[], $4::timestamptz[])
          )
        "#,
    )
    .bind(partition)
    .bind(&turn_ids)
    .bind(&record_kinds)
    .bind(&timestamps)
    .fetch_all(&mut **tx)
    .await?;

    let mut existing = ExistingChainRows::with_capacity(existing_rows.len());
    for row in existing_rows {
        let turn_id: Uuid = row.try_get("turn_id")?;
        let record_kind: i16 = row.try_get("record_kind")?;
        let ts: DateTime<Utc> = row.try_get("ts")?;
        let integrity_hash: Vec<u8> = row.try_get("integrity_hash")?;
        let prev_hash: Option<Vec<u8>> = row.try_get("prev_hash")?;
        existing.insert((turn_id, record_kind, ts), (integrity_hash, prev_hash));
    }
    Ok(existing)
}

/// Folds a compliance partition's hash chain over the batch's rows, in place.
///
/// Rows already present in `existing` reuse their stored hashes and do not advance the chain, so
/// replayed batches are idempotent. New rows are linked from the current tip and mutate the batch
/// rows' `integrity_hash`/`prev_hash` fields.
pub(super) fn fold_partition_chain(
    prev: Option<&[u8]>,
    rows: &mut [LineageRow],
    indices: &[usize],
    existing: &ExistingChainRows,
) -> Result<PartitionChainOutcome> {
    let mut prev = prev.map(hash_from_slice).transpose()?;
    let mut final_hash = None;
    let mut last_ts = None;
    let mut new_rows = 0;
    for &idx in indices {
        let key = (rows[idx].turn_id, rows[idx].record_kind, rows[idx].ts);
        if let Some((integrity_hash, prev_hash)) = existing.get(&key) {
            rows[idx].integrity_hash = integrity_hash.clone();
            rows[idx].prev_hash = prev_hash.clone();
            continue;
        }
        let (integrity_hash, prev_echo) = HashChain::link(prev, &rows[idx].payload)?;
        rows[idx].integrity_hash = integrity_hash.as_bytes().to_vec();
        rows[idx].prev_hash = prev_echo.map(|hash| hash.as_bytes().to_vec());
        prev = Some(integrity_hash);
        final_hash = Some(integrity_hash.as_bytes().to_vec());
        last_ts = Some(rows[idx].ts);
        new_rows += 1;
    }
    Ok(PartitionChainOutcome {
        final_hash,
        last_ts,
        new_rows,
    })
}
