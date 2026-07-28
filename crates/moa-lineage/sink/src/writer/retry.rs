//! Failure classification, retry backoff, and durable dead-letter fallback.
//!
//! Retry lives in the queue, not in the process. A recoverable failure defers
//! the claimed rows with a bounded backoff and releases the lease, so the rows
//! survive the failure and any replica may take the next attempt. A permanent
//! failure — one that will fail identically forever, such as an undecodable
//! payload or a constraint the row can never satisfy — is dead-lettered and
//! dequeued in a single transaction.
//!
//! The previous in-process retry loop slept inside a live lease, which meant a
//! transient Postgres outage pinned a writer for the length of its own backoff
//! while holding rows no other replica could take.

use std::time::Duration;

use uuid::Uuid;

use crate::clickhouse::is_retryable_clickhouse_error;
use crate::{Error, Result};

use super::rows::PendingRow;

/// Maximum claims for one row before a recoverable failure is treated as
/// permanent.
///
/// Without a cap a row that fails a retryable-looking check forever would be
/// re-claimed forever, and the backlog it sits at the head of would never drain.
pub(super) const MAX_CLAIM_ATTEMPTS: i32 = 8;

/// First retry delay. Doubles per attempt up to [`MAX_RETRY_BACKOFF`].
const BASE_RETRY_BACKOFF: Duration = Duration::from_millis(250);
/// Ceiling on retry backoff, so a long outage still re-probes regularly.
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// What the drain should do with a claimed batch after a failed write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FailureDisposition {
    /// Preserve the rows and try again after `backoff`.
    Retry {
        /// Delay before the rows become claimable again.
        backoff: Duration,
    },
    /// The rows can never be written; dead-letter and dequeue them.
    Permanent,
}

/// Classifies a failed batch write into retry or dead-letter.
///
/// `attempts` is the claim count already recorded on the rows, so a row that
/// keeps failing a retryable check still terminates instead of cycling forever.
pub(super) fn classify_failure(error: &Error, attempts: i32) -> FailureDisposition {
    if !is_retryable_write_error(error) || attempts >= MAX_CLAIM_ATTEMPTS {
        return FailureDisposition::Permanent;
    }
    FailureDisposition::Retry {
        backoff: retry_backoff(attempts),
    }
}

/// Returns the deferral delay for a row that has been claimed `attempts` times.
fn retry_backoff(attempts: i32) -> Duration {
    let exponent = attempts.clamp(1, MAX_CLAIM_ATTEMPTS) - 1;
    let scaled = BASE_RETRY_BACKOFF.saturating_mul(1_u32 << exponent.min(16));
    scaled.min(MAX_RETRY_BACKOFF)
}

/// Summary columns carried on a dead-letter row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeadLetterSummary {
    pub(super) row_count: usize,
    pub(super) first_storage_partition_id: Option<String>,
    pub(super) first_session_id: Option<Uuid>,
    pub(super) first_turn_id: Option<Uuid>,
}

/// Writes a dead-letter row inside the caller's transaction.
///
/// Takes a transaction rather than a pool so the dead-letter commit and the
/// dequeue of the same rows are one atomic step. Splitting them would allow a
/// crash to leave a dead-lettered batch still queued, which re-dead-letters it
/// on the next claim, or a dequeued batch with no dead letter, which loses it.
pub(super) async fn write_dead_letter_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rows: &[PendingRow],
    error: &Error,
    attempts: i32,
) -> Result<Uuid> {
    let dead_letter_id = stable_dead_letter_id(rows)?;
    let summary = dead_letter_summary(rows);
    let row_count = i32::try_from(summary.row_count)
        .map_err(|_| Error::Invalid("dead-letter row count overflow".to_string()))?;
    let rows_json = serde_json::to_value(rows)?;

    sqlx::query(
        r#"
        INSERT INTO analytics.lineage_dead_letters (
            dead_letter_id,
            error,
            attempts,
            row_count,
            first_storage_partition_id,
            first_session_id,
            first_turn_id,
            rows
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        ON CONFLICT (dead_letter_id) DO UPDATE
        SET error = EXCLUDED.error,
            attempts = EXCLUDED.attempts,
            row_count = EXCLUDED.row_count,
            first_storage_partition_id = EXCLUDED.first_storage_partition_id,
            first_session_id = EXCLUDED.first_session_id,
            first_turn_id = EXCLUDED.first_turn_id,
            rows = EXCLUDED.rows
        "#,
    )
    .bind(dead_letter_id)
    .bind(error.to_string())
    .bind(attempts)
    .bind(row_count)
    .bind(summary.first_storage_partition_id)
    .bind(summary.first_session_id)
    .bind(summary.first_turn_id)
    .bind(rows_json)
    .execute(&mut **tx)
    .await?;

    metrics::counter!("moa_lineage_dead_lettered_total").increment(summary.row_count as u64);
    // Batch-level commit counter: one increment per durably committed dead-letter
    // batch (distinct from the row counter above), so operators can track
    // dead-letter volume and drive redrive/alerting off the table.
    metrics::counter!("moa_lineage_dead_letter_commits_total").increment(1);
    Ok(dead_letter_id)
}

pub(super) fn stable_dead_letter_id(rows: &[PendingRow]) -> Result<Uuid> {
    let rows_json = serde_json::to_vec(rows)?;
    let hash = blake3::hash(&rows_json);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    Ok(Uuid::from_bytes(bytes))
}

pub(super) fn dead_letter_summary(rows: &[PendingRow]) -> DeadLetterSummary {
    let first = rows.first();
    DeadLetterSummary {
        row_count: rows.len(),
        first_storage_partition_id: first.map(PendingRow::storage_partition_id),
        first_session_id: first.and_then(PendingRow::session_id),
        first_turn_id: first.and_then(PendingRow::turn_id),
    }
}

pub(super) fn is_retryable_write_error(error: &Error) -> bool {
    match error {
        Error::Sqlx(sqlx::Error::Io(_))
        | Error::Sqlx(sqlx::Error::Tls(_))
        | Error::Sqlx(sqlx::Error::PoolTimedOut)
        | Error::Sqlx(sqlx::Error::PoolClosed)
        | Error::Sqlx(sqlx::Error::Protocol(_)) => true,
        Error::Sqlx(sqlx::Error::Database(db_error)) => db_error
            .code()
            .as_deref()
            .is_some_and(is_retryable_postgres_sqlstate),
        Error::ClickHouse(clickhouse_error) => is_retryable_clickhouse_error(clickhouse_error),
        _ => false,
    }
}

pub(super) fn is_retryable_postgres_sqlstate(code: &str) -> bool {
    code.starts_with("08")
        || matches!(
            code,
            "40001" | "40P01" | "53300" | "53400" | "57P01" | "57P02" | "57P03"
        )
}
