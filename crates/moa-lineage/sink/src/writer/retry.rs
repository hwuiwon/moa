//! Bounded writer retries, error classification, and durable dead-letter fallback.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use uuid::Uuid;

use crate::clickhouse::is_retryable_clickhouse_error;
use crate::store::LineageStore;
use crate::{Error, Result};

use super::rows::PendingRow;
use super::storage::write_pending_rows;

const WRITE_RETRY_MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteDisposition {
    Written,
    DeadLettered,
}

#[derive(Debug)]
struct WriteFailure {
    error: Error,
    attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeadLetterSummary {
    pub(super) row_count: usize,
    pub(super) first_storage_partition_id: Option<String>,
    pub(super) first_session_id: Option<Uuid>,
    pub(super) first_turn_id: Option<Uuid>,
}

pub(super) async fn write_pending_rows_or_dead_letter(
    store: &LineageStore,
    rows: &[PendingRow],
) -> Result<WriteDisposition> {
    match write_pending_rows_with_retry(store, rows).await {
        Ok(()) => Ok(WriteDisposition::Written),
        Err(failure) => {
            let dead_letter_id = write_dead_letter_batch(store.postgres(), rows, &failure).await?;
            tracing::error!(
                dead_letter_id = %dead_letter_id,
                error = %failure.error,
                attempts = failure.attempts,
                row_count = rows.len(),
                "lineage batch moved to dead letter storage"
            );
            Ok(WriteDisposition::DeadLettered)
        }
    }
}

async fn write_pending_rows_with_retry(
    store: &LineageStore,
    rows: &[PendingRow],
) -> std::result::Result<(), WriteFailure> {
    let backoff = ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(100))
        .with_max_delay(Duration::from_secs(5))
        .with_max_times(WRITE_RETRY_MAX_ATTEMPTS as usize - 1);
    let attempts = AtomicU32::new(0);

    (|| async {
        attempts.fetch_add(1, Ordering::Relaxed);
        write_pending_rows(store, rows).await
    })
    .retry(backoff)
    .when(is_retryable_write_error)
    .notify(|error, delay| {
        tracing::warn!(
            %error,
            attempt = attempts.load(Ordering::Relaxed),
            max_attempts = WRITE_RETRY_MAX_ATTEMPTS,
            retry_after_ms = delay.as_millis(),
            "lineage write failed"
        );
    })
    .await
    .map_err(|error| {
        let attempts = attempts.load(Ordering::Relaxed);
        tracing::error!(
            %error,
            attempts,
            max_attempts = WRITE_RETRY_MAX_ATTEMPTS,
            "lineage write failed permanently"
        );
        WriteFailure { error, attempts }
    })
}

async fn write_dead_letter_batch(
    pool: &sqlx::PgPool,
    rows: &[PendingRow],
    failure: &WriteFailure,
) -> Result<Uuid> {
    let dead_letter_id = stable_dead_letter_id(rows)?;
    let summary = dead_letter_summary(rows);
    let attempts = i32::try_from(failure.attempts)
        .map_err(|_| Error::Invalid("dead-letter attempt count overflow".to_string()))?;
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
    .bind(failure.error.to_string())
    .bind(attempts)
    .bind(row_count)
    .bind(summary.first_storage_partition_id)
    .bind(summary.first_session_id)
    .bind(summary.first_turn_id)
    .bind(rows_json)
    .execute(pool)
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
        first_storage_partition_id: first.map(row_storage_partition_id),
        first_session_id: first.and_then(row_session_id),
        first_turn_id: first.and_then(row_turn_id),
    }
}

fn row_storage_partition_id(row: &PendingRow) -> String {
    match row {
        PendingRow::Lineage(row) => row.storage_partition_id.clone(),
        PendingRow::Score(row) => row.storage_partition_id.clone(),
    }
}

fn row_session_id(row: &PendingRow) -> Option<Uuid> {
    match row {
        PendingRow::Lineage(row) => Some(row.session_id),
        PendingRow::Score(row) => row.session_id,
    }
}

fn row_turn_id(row: &PendingRow) -> Option<Uuid> {
    match row {
        PendingRow::Lineage(row) => Some(row.turn_id),
        PendingRow::Score(row) => row.turn_id,
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
