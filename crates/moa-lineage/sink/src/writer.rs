//! Async lineage writer worker.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use backon::{ExponentialBuilder, Retryable};
use chrono::{DateTime, Utc};
use moa_lineage_core::chain::{HashChain, canonical_payload_hash, hash_from_slice};
use moa_lineage_core::{LineageEvent, ScoreRecord, ScoreSource, ScoreTarget, ScoreValue};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::fjall_journal::Journal;
use crate::mpsc_sink::MpscSinkConfig;
use crate::{Error, Result, ensure_schema};

const WRITE_RETRY_MAX_ATTEMPTS: u32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteDisposition {
    Written,
    DeadLettered,
}

#[derive(Debug)]
struct WriteFailure {
    error: Error,
    attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeadLetterSummary {
    row_count: usize,
    first_storage_partition_id: Option<String>,
    first_session_id: Option<Uuid>,
    first_turn_id: Option<Uuid>,
}

/// Writer runtime statistics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterStats {
    /// Rows written successfully.
    pub written: u64,
    /// Rows currently known to be pending in the journal.
    pub journal_depth: u64,
    /// Unix timestamp in milliseconds for the latest successful flush.
    pub last_flush_unix_ms: Option<u64>,
}

#[derive(Default)]
struct SharedWriterStats {
    written: AtomicU64,
    journal_depth: AtomicU64,
    last_flush_unix_ms: AtomicU64,
}

impl SharedWriterStats {
    fn snapshot(&self) -> WriterStats {
        let last_flush = self.last_flush_unix_ms.load(Ordering::Relaxed);
        WriterStats {
            written: self.written.load(Ordering::Relaxed),
            journal_depth: self.journal_depth.load(Ordering::Relaxed),
            last_flush_unix_ms: (last_flush > 0).then_some(last_flush),
        }
    }
}

/// Handle for graceful lineage writer shutdown.
pub struct WriterHandle {
    shutdown: CancellationToken,
    join: Arc<Mutex<Option<tokio::task::JoinHandle<Result<WriterStats>>>>>,
    stats: Arc<SharedWriterStats>,
}

impl WriterHandle {
    /// Requests graceful shutdown, drains pending events, and returns final stats.
    pub async fn shutdown(&self) -> Result<WriterStats> {
        self.shutdown.cancel();
        let Some(join) = self.join.lock().await.take() else {
            return Ok(self.stats());
        };
        join.await?
    }

    /// Returns the latest writer statistics snapshot.
    #[must_use]
    pub fn stats(&self) -> WriterStats {
        self.stats.snapshot()
    }
}

/// Spawned lineage writer marker.
pub struct LineageWriter;

/// Spawns the lineage writer worker.
pub async fn spawn_writer(
    rx: mpsc::Receiver<LineageEvent>,
    config: MpscSinkConfig,
    pool: sqlx::PgPool,
) -> Result<WriterHandle> {
    ensure_schema(&pool).await?;

    let shutdown = CancellationToken::new();
    let stats = Arc::new(SharedWriterStats::default());
    let worker_shutdown = shutdown.clone();
    let worker_stats = stats.clone();
    let join =
        tokio::spawn(
            async move { run_writer(rx, config, pool, worker_shutdown, worker_stats).await },
        );

    Ok(WriterHandle {
        shutdown,
        join: Arc::new(Mutex::new(Some(join))),
        stats,
    })
}

async fn run_writer(
    mut rx: mpsc::Receiver<LineageEvent>,
    config: MpscSinkConfig,
    pool: sqlx::PgPool,
    shutdown: CancellationToken,
    stats: Arc<SharedWriterStats>,
) -> Result<WriterStats> {
    let journal = Journal::open(&config.journal_path)?;
    replay_pending(&journal, &pool, &stats).await?;

    let mut seq = next_sequence(&journal)?;
    let mut batch = Vec::with_capacity(config.batch_size);
    let mut flush_interval = tokio::time::interval(config.batch_max_age);
    flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                while let Ok(evt) = rx.try_recv() {
                    batch.push(evt);
                    if batch.len() >= config.batch_size {
                        flush_events(&journal, &pool, &stats, &mut seq, &mut batch).await?;
                    }
                }
                flush_events(&journal, &pool, &stats, &mut seq, &mut batch).await?;
                break;
            }
            maybe_evt = rx.recv() => {
                match maybe_evt {
                    Some(evt) => {
                        batch.push(evt);
                        if batch.len() >= config.batch_size {
                            flush_events(&journal, &pool, &stats, &mut seq, &mut batch).await?;
                        }
                    }
                    None => {
                        flush_events(&journal, &pool, &stats, &mut seq, &mut batch).await?;
                        break;
                    }
                }
            }
            _ = flush_interval.tick() => {
                flush_events(&journal, &pool, &stats, &mut seq, &mut batch).await?;
            }
        }
    }

    record_journal_depth(&stats, journal.approximate_len() as u64);
    Ok(stats.snapshot())
}

fn next_sequence(journal: &Journal) -> Result<u64> {
    let next = journal
        .replay()?
        .into_iter()
        .map(|(seq, _)| seq)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    Ok(next)
}

async fn replay_pending(
    journal: &Journal,
    pool: &sqlx::PgPool,
    stats: &Arc<SharedWriterStats>,
) -> Result<()> {
    let pending = journal.replay()?;
    record_journal_depth(stats, pending.len() as u64);
    if pending.is_empty() {
        return Ok(());
    }

    let rows = pending
        .iter()
        .map(|(_, payload)| decode_pending_row(payload))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let disposition = write_pending_rows_or_dead_letter(pool, &rows).await?;
    if should_ack_journal(disposition)
        && let (Some((lo, _)), Some((hi, _))) = (pending.first(), pending.last())
    {
        journal.ack_range(*lo, *hi)?;
    }
    if disposition == WriteDisposition::Written {
        record_flush(stats, rows.len());
    }
    record_journal_depth(stats, journal.approximate_len() as u64);
    Ok(())
}

async fn flush_events(
    journal: &Journal,
    pool: &sqlx::PgPool,
    stats: &Arc<SharedWriterStats>,
    seq: &mut u64,
    batch: &mut Vec<LineageEvent>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let events = std::mem::take(batch);
    let mut rows = Vec::with_capacity(events.len());
    let start_seq = *seq;
    let mut end_seq = start_seq;
    for evt in events {
        let row = PendingRow::from_event(evt)?;
        let payload = serde_json::to_vec(&row)?;
        journal.append(*seq, &payload)?;
        end_seq = *seq;
        *seq = (*seq).saturating_add(1);
        rows.push(row);
    }
    record_journal_depth(stats, journal.approximate_len() as u64);

    let disposition = write_pending_rows_or_dead_letter(pool, &rows).await?;
    if should_ack_journal(disposition) {
        journal.ack_range(start_seq, end_seq)?;
    }
    if disposition == WriteDisposition::Written {
        record_flush(stats, rows.len());
    }
    record_journal_depth(stats, journal.approximate_len() as u64);
    Ok(())
}

fn should_ack_journal(disposition: WriteDisposition) -> bool {
    disposition == WriteDisposition::Written
}

async fn write_pending_rows_or_dead_letter(
    pool: &sqlx::PgPool,
    rows: &[PendingRow],
) -> Result<WriteDisposition> {
    match write_pending_rows_with_retry(pool, rows).await {
        Ok(()) => Ok(WriteDisposition::Written),
        Err(failure) => {
            let dead_letter_id = write_dead_letter_batch(pool, rows, &failure).await?;
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
    pool: &sqlx::PgPool,
    rows: &[PendingRow],
) -> std::result::Result<(), WriteFailure> {
    let backoff = ExponentialBuilder::default()
        .with_min_delay(Duration::from_millis(100))
        .with_max_delay(Duration::from_secs(5))
        .with_max_times(WRITE_RETRY_MAX_ATTEMPTS as usize - 1);
    let attempts = AtomicU32::new(0);

    (|| async {
        attempts.fetch_add(1, Ordering::Relaxed);
        write_pending_rows(pool, rows).await
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
    Ok(dead_letter_id)
}

fn stable_dead_letter_id(rows: &[PendingRow]) -> Result<Uuid> {
    let rows_json = serde_json::to_vec(rows)?;
    let hash = blake3::hash(&rows_json);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&hash.as_bytes()[..16]);
    Ok(Uuid::from_bytes(bytes))
}

fn dead_letter_summary(rows: &[PendingRow]) -> DeadLetterSummary {
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

fn is_retryable_write_error(error: &Error) -> bool {
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
        _ => false,
    }
}

fn is_retryable_postgres_sqlstate(code: &str) -> bool {
    code.starts_with("08")
        || matches!(
            code,
            "40001" | "40P01" | "53300" | "53400" | "57P01" | "57P02" | "57P03"
        )
}

async fn write_pending_rows(pool: &sqlx::PgPool, rows: &[PendingRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut lineage_rows = Vec::new();
    let mut score_rows = Vec::new();
    for row in rows {
        match row {
            PendingRow::Lineage(row) => lineage_rows.push(row.clone()),
            PendingRow::Score(row) => score_rows.push(row.clone()),
        }
    }

    write_rows(pool, &lineage_rows).await?;
    write_score_rows(pool, &score_rows).await?;
    Ok(())
}

async fn write_rows(pool: &sqlx::PgPool, rows: &[LineageRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut rows = rows.to_vec();
    let mut tx = pool.begin().await?;
    apply_compliance_hashes(&mut tx, &mut rows).await?;

    sqlx::query("DROP TABLE IF EXISTS lineage_copy")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        r#"
        CREATE TEMP TABLE lineage_copy (
            turn_id        UUID        NOT NULL,
            session_id     UUID        NOT NULL,
            user_id        TEXT        NOT NULL,
            storage_partition_id   TEXT        NOT NULL,
            ts             TIMESTAMPTZ NOT NULL,
            tier           SMALLINT    NOT NULL,
            record_kind    SMALLINT    NOT NULL,
            payload        JSONB       NOT NULL,
            integrity_hash BYTEA       NOT NULL,
            prev_hash      BYTEA
        );
        "#,
    )
    .execute(&mut *tx)
    .await?;

    let copy_payload = render_copy_csv(&rows);
    let mut copy = (*tx)
        .copy_in_raw(
            r#"
            COPY lineage_copy (
                turn_id,
                session_id,
                user_id,
                storage_partition_id,
                ts,
                tier,
                record_kind,
                payload,
                integrity_hash,
                prev_hash
            )
            FROM STDIN WITH (FORMAT csv, NULL '\N')
            "#,
        )
        .await?;
    if let Err(error) = copy.send(copy_payload.as_bytes()).await {
        let _ = copy.abort("lineage copy failed").await;
        return Err(error.into());
    }
    copy.finish().await?;

    sqlx::query(
        r#"
        INSERT INTO analytics.turn_lineage (
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier,
            record_kind,
            payload,
            integrity_hash,
            prev_hash
        )
        SELECT
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier,
            record_kind,
            payload,
            integrity_hash,
            prev_hash
        FROM lineage_copy
        ON CONFLICT (turn_id, record_kind, ts) DO UPDATE
        SET payload = EXCLUDED.payload,
            integrity_hash = EXCLUDED.integrity_hash,
            prev_hash = COALESCE(EXCLUDED.prev_hash, analytics.turn_lineage.prev_hash)
        "#,
    )
    .execute(&mut *tx)
    .await?;

    sqlx::query("DROP TABLE IF EXISTS lineage_copy")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn apply_compliance_hashes(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    rows: &mut [LineageRow],
) -> Result<()> {
    for row in rows {
        let enabled = sqlx::query_scalar::<_, bool>(
            r#"
            SELECT COALESCE((
                SELECT enabled
                FROM analytics.compliance_tenants
                WHERE storage_partition_id = $1
            ), FALSE)
            "#,
        )
        .bind(&row.storage_partition_id)
        .fetch_one(&mut **tx)
        .await?;
        if !enabled {
            continue;
        }

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("compliance:{}", row.storage_partition_id))
            .execute(&mut **tx)
            .await?;

        if let Some(existing) = sqlx::query(
            r#"
            SELECT integrity_hash, prev_hash
            FROM analytics.turn_lineage
            WHERE turn_id = $1 AND record_kind = $2 AND ts = $3
            "#,
        )
        .bind(row.turn_id)
        .bind(row.record_kind)
        .bind(row.ts)
        .fetch_optional(&mut **tx)
        .await?
        {
            row.integrity_hash = existing.try_get("integrity_hash")?;
            row.prev_hash = existing.try_get("prev_hash")?;
            continue;
        }

        sqlx::query(
            r#"
            INSERT INTO analytics.compliance_storage_partition_state (storage_partition_id)
            VALUES ($1)
            ON CONFLICT (storage_partition_id) DO NOTHING
            "#,
        )
        .bind(&row.storage_partition_id)
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
        .bind(&row.storage_partition_id)
        .fetch_one(&mut **tx)
        .await?;
        let prev = prev_hash.as_deref().map(hash_from_slice).transpose()?;
        let (integrity_hash, prev) = HashChain::link(prev, &row.payload)?;
        row.integrity_hash = integrity_hash.as_bytes().to_vec();
        row.prev_hash = prev.map(|hash| hash.as_bytes().to_vec());

        sqlx::query(
            r#"
            UPDATE analytics.compliance_storage_partition_state
            SET last_integrity_hash = $2,
                last_ts = $3,
                record_count = record_count + 1
            WHERE storage_partition_id = $1
            "#,
        )
        .bind(&row.storage_partition_id)
        .bind(&row.integrity_hash)
        .bind(row.ts)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn render_copy_csv(rows: &[LineageRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let fields = [
            csv_field(&row.turn_id.to_string()),
            csv_field(&row.session_id.to_string()),
            csv_field(&row.user_id),
            csv_field(&row.storage_partition_id),
            csv_field(&row.ts.to_rfc3339()),
            csv_field(&row.tier.to_string()),
            csv_field(&row.record_kind.to_string()),
            csv_field(&row.payload.to_string()),
            csv_field(&bytea_hex(&row.integrity_hash)),
            row.prev_hash
                .as_ref()
                .map(|hash| csv_field(&bytea_hex(hash)))
                .unwrap_or_else(|| "\\N".to_string()),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

async fn write_score_rows(pool: &sqlx::PgPool, rows: &[ScoreRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut conn = pool.acquire().await?;
    sqlx::query("DROP TABLE IF EXISTS lineage_scores_copy")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        r#"
        CREATE TEMP TABLE lineage_scores_copy (
            score_id           UUID             NOT NULL,
            ts                 TIMESTAMPTZ      NOT NULL,
            storage_partition_id       TEXT             NOT NULL,
            user_id            TEXT,
            target_kind        TEXT             NOT NULL,
            turn_id            UUID,
            session_id         UUID,
            run_id             UUID,
            item_id            UUID,
            dataset_id         UUID,
            name               TEXT             NOT NULL,
            value_type         TEXT             NOT NULL,
            value_numeric      DOUBLE PRECISION,
            value_boolean      BOOLEAN,
            value_categorical  TEXT,
            source             TEXT             NOT NULL,
            model_or_evaluator TEXT             NOT NULL,
            comment            TEXT
        );
        "#,
    )
    .execute(&mut *conn)
    .await?;

    let copy_payload = render_score_copy_csv(rows);
    let mut copy = conn
        .copy_in_raw(
            r#"
            COPY lineage_scores_copy (
                score_id,
                ts,
                storage_partition_id,
                user_id,
                target_kind,
                turn_id,
                session_id,
                run_id,
                item_id,
                dataset_id,
                name,
                value_type,
                value_numeric,
                value_boolean,
                value_categorical,
                source,
                model_or_evaluator,
                comment
            )
            FROM STDIN WITH (FORMAT csv, NULL '\N')
            "#,
        )
        .await?;
    if let Err(error) = copy.send(copy_payload.as_bytes()).await {
        let _ = copy.abort("lineage score copy failed").await;
        return Err(error.into());
    }
    copy.finish().await?;

    sqlx::query(
        r#"
        INSERT INTO analytics.scores (
            score_id,
            ts,
            storage_partition_id,
            user_id,
            target_kind,
            turn_id,
            session_id,
            run_id,
            item_id,
            dataset_id,
            name,
            value_type,
            value_numeric,
            value_boolean,
            value_categorical,
            source,
            model_or_evaluator,
            comment
        )
        SELECT
            score_id,
            ts,
            storage_partition_id,
            user_id,
            target_kind,
            turn_id,
            session_id,
            run_id,
            item_id,
            dataset_id,
            name,
            value_type,
            value_numeric,
            value_boolean,
            value_categorical,
            source,
            model_or_evaluator,
            comment
        FROM lineage_scores_copy
        ON CONFLICT (score_id, ts) DO UPDATE
        SET storage_partition_id = EXCLUDED.storage_partition_id,
            user_id = EXCLUDED.user_id,
            target_kind = EXCLUDED.target_kind,
            turn_id = EXCLUDED.turn_id,
            session_id = EXCLUDED.session_id,
            run_id = EXCLUDED.run_id,
            item_id = EXCLUDED.item_id,
            dataset_id = EXCLUDED.dataset_id,
            name = EXCLUDED.name,
            value_type = EXCLUDED.value_type,
            value_numeric = EXCLUDED.value_numeric,
            value_boolean = EXCLUDED.value_boolean,
            value_categorical = EXCLUDED.value_categorical,
            source = EXCLUDED.source,
            model_or_evaluator = EXCLUDED.model_or_evaluator,
            comment = EXCLUDED.comment
        "#,
    )
    .execute(&mut *conn)
    .await?;

    sqlx::query("DROP TABLE IF EXISTS lineage_scores_copy")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

fn render_score_copy_csv(rows: &[ScoreRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let fields = [
            csv_field(&row.score_id.to_string()),
            csv_field(&row.ts.to_rfc3339()),
            csv_field(&row.storage_partition_id),
            nullable_csv(row.user_id.as_deref()),
            csv_field(&row.target_kind),
            nullable_uuid_csv(row.turn_id),
            nullable_uuid_csv(row.session_id),
            nullable_uuid_csv(row.run_id),
            nullable_uuid_csv(row.item_id),
            nullable_uuid_csv(row.dataset_id),
            csv_field(&row.name),
            csv_field(&row.value_type),
            nullable_csv(row.value_numeric.map(|value| value.to_string()).as_deref()),
            nullable_csv(row.value_boolean.map(|value| value.to_string()).as_deref()),
            nullable_csv(row.value_categorical.as_deref()),
            csv_field(&row.source),
            csv_field(&row.model_or_evaluator),
            nullable_csv(row.comment.as_deref()),
        ];
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    out
}

fn nullable_csv(value: Option<&str>) -> String {
    value.map(csv_field).unwrap_or_else(|| "\\N".to_string())
}

fn nullable_uuid_csv(value: Option<Uuid>) -> String {
    value
        .map(|value| csv_field(&value.to_string()))
        .unwrap_or_else(|| "\\N".to_string())
}

fn csv_field(value: &str) -> String {
    let escaped = value.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn bytea_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + bytes.len().saturating_mul(2));
    out.push_str("\\x");
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn record_flush(stats: &SharedWriterStats, rows: usize) {
    stats.written.fetch_add(rows as u64, Ordering::Relaxed);
    stats.last_flush_unix_ms.store(
        Utc::now().timestamp_millis().max(1) as u64,
        Ordering::Relaxed,
    );
    metrics::counter!("moa_lineage_written_total").increment(rows as u64);
    metrics::counter!("moa_lineage_flushed_total").increment(rows as u64);
}

fn record_journal_depth(stats: &SharedWriterStats, depth: u64) {
    stats.journal_depth.store(depth, Ordering::Relaxed);
    metrics::gauge!("moa_lineage_journal_depth").set(depth as f64);
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "table", content = "row", rename_all = "snake_case")]
enum PendingRow {
    Lineage(LineageRow),
    Score(ScoreRow),
}

impl PendingRow {
    fn from_event(evt: LineageEvent) -> Result<Self> {
        match evt {
            LineageEvent::Eval(record) => Ok(Self::Score(ScoreRow::from_record(record))),
            other => Ok(Self::Lineage(LineageRow::from_event(other)?)),
        }
    }
}

fn decode_pending_row(payload: &[u8]) -> std::result::Result<PendingRow, serde_json::Error> {
    serde_json::from_slice::<PendingRow>(payload)
        .or_else(|_| serde_json::from_slice::<LineageRow>(payload).map(PendingRow::Lineage))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LineageRow {
    turn_id: Uuid,
    session_id: Uuid,
    user_id: String,
    storage_partition_id: String,
    ts: DateTime<Utc>,
    tier: i16,
    record_kind: i16,
    payload: serde_json::Value,
    integrity_hash: Vec<u8>,
    prev_hash: Option<Vec<u8>>,
}

impl LineageRow {
    fn from_event(evt: LineageEvent) -> Result<Self> {
        let payload = serde_json::to_value(&evt)?;
        let integrity_hash = canonical_payload_hash(&payload)?.as_bytes().to_vec();
        let record_kind = evt.record_kind().as_i16();
        let fallback_ts = Utc::now();

        let (turn_id, session_id, user_id, storage_partition_id, ts) = match &evt {
            LineageEvent::Retrieval(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Context(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Generation(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Citation(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Decision(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.storage_partition_id.to_string(),
                record.ts,
            ),
            LineageEvent::Eval(_) => (
                Uuid::now_v7(),
                Uuid::nil(),
                "unknown".to_string(),
                "unknown".to_string(),
                fallback_ts,
            ),
        };

        Ok(Self {
            turn_id,
            session_id,
            user_id,
            storage_partition_id,
            ts,
            tier: 1,
            record_kind,
            payload,
            integrity_hash,
            prev_hash: None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ScoreRow {
    score_id: Uuid,
    ts: DateTime<Utc>,
    storage_partition_id: String,
    user_id: Option<String>,
    target_kind: String,
    turn_id: Option<Uuid>,
    session_id: Option<Uuid>,
    run_id: Option<Uuid>,
    item_id: Option<Uuid>,
    dataset_id: Option<Uuid>,
    name: String,
    value_type: String,
    value_numeric: Option<f64>,
    value_boolean: Option<bool>,
    value_categorical: Option<String>,
    source: String,
    model_or_evaluator: String,
    comment: Option<String>,
}

impl ScoreRow {
    fn from_record(record: ScoreRecord) -> Self {
        let (target_kind, turn_id, session_id, target_run_id, item_id) = match record.target {
            ScoreTarget::Turn { turn_id } => {
                ("turn".to_string(), Some(turn_id.0), None, None, None)
            }
            ScoreTarget::Session { session_id } => {
                ("session".to_string(), None, Some(session_id.0), None, None)
            }
            ScoreTarget::DatasetRunItem { run_id, item_id } => (
                "dataset_run_item".to_string(),
                None,
                None,
                Some(run_id),
                Some(item_id),
            ),
        };
        let (value_type, value_numeric, value_boolean, value_categorical) = match record.value {
            ScoreValue::Numeric(value) => ("numeric".to_string(), Some(value), None, None),
            ScoreValue::Boolean(value) => ("boolean".to_string(), None, Some(value), None),
            ScoreValue::Categorical(value) => ("categorical".to_string(), None, None, Some(value)),
        };

        Self {
            score_id: record.score_id,
            ts: record.ts,
            storage_partition_id: record.storage_partition_id.to_string(),
            user_id: record.user_id.map(|user_id| user_id.to_string()),
            target_kind,
            turn_id,
            session_id,
            run_id: record.run_id.or(target_run_id),
            item_id,
            dataset_id: record.dataset_id,
            name: record.name,
            value_type,
            value_numeric,
            value_boolean,
            value_categorical,
            source: score_source_to_db(record.source).to_string(),
            model_or_evaluator: record.model_or_evaluator,
            comment: record.comment,
        }
    }
}

fn score_source_to_db(source: ScoreSource) -> &'static str {
    match source {
        ScoreSource::OnlineJudge => "online_judge",
        ScoreSource::OfflineReplay => "offline_replay",
        ScoreSource::Human => "human",
        ScoreSource::External => "external",
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::StoragePartitionId;
    use moa_lineage_core::{
        LineageEvent, ScoreRecord, ScoreSource, ScoreTarget, ScoreValue, TurnId,
    };
    use uuid::Uuid;

    #[test]
    fn pending_row_routes_eval_events_to_scores() {
        let score_id = Uuid::now_v7();
        let row = super::PendingRow::from_event(LineageEvent::Eval(ScoreRecord {
            score_id,
            ts: Utc::now(),
            target: ScoreTarget::Turn {
                turn_id: TurnId::new_v7(),
            },
            storage_partition_id: StoragePartitionId::new("tenant"),
            user_id: None,
            name: "retrieval_zero_recall".to_string(),
            value: ScoreValue::Boolean(false),
            source: ScoreSource::OnlineJudge,
            model_or_evaluator: "retriever".to_string(),
            run_id: None,
            dataset_id: None,
            comment: None,
        }))
        .expect("score row should build");

        match row {
            super::PendingRow::Score(row) => {
                assert_eq!(row.score_id, score_id);
                assert_eq!(row.name, "retrieval_zero_recall");
                assert_eq!(row.value_type, "boolean");
                assert_eq!(row.value_boolean, Some(false));
            }
            super::PendingRow::Lineage(_) => panic!("eval events must not enter turn_lineage"),
        }
    }

    #[test]
    fn write_retry_classification_is_sqlstate_aware() {
        // Pins: lineage writer retries transient database failures but not
        // permanent ones. The attempt count is bounded structurally by the
        // backon `with_max_times(WRITE_RETRY_MAX_ATTEMPTS - 1)` budget.
        assert!(super::is_retryable_postgres_sqlstate("08006"));
        assert!(super::is_retryable_postgres_sqlstate("40001"));
        assert!(!super::is_retryable_postgres_sqlstate("23505"));

        let permanent = super::Error::Invalid("poison row".to_string());
        assert!(!super::is_retryable_write_error(&permanent));

        let transient = super::Error::Sqlx(sqlx::Error::PoolTimedOut);
        assert!(super::is_retryable_write_error(&transient));
    }

    #[test]
    fn dead_letter_summary_uses_first_row_metadata() {
        // Pins: dead-letter records carry searchable metadata from the batch head.
        let turn_id = Uuid::now_v7();
        let session_id = Uuid::now_v7();
        let row = super::PendingRow::Lineage(super::LineageRow {
            turn_id,
            session_id,
            user_id: "user-1".to_string(),
            storage_partition_id: "partition-1".to_string(),
            ts: Utc::now(),
            tier: 1,
            record_kind: 1,
            payload: serde_json::json!({"kind": "test"}),
            integrity_hash: vec![7; 32],
            prev_hash: None,
        });

        let summary = super::dead_letter_summary(&[row]);

        assert_eq!(summary.row_count, 1);
        assert_eq!(
            summary.first_storage_partition_id.as_deref(),
            Some("partition-1")
        );
        assert_eq!(summary.first_session_id, Some(session_id));
        assert_eq!(summary.first_turn_id, Some(turn_id));
    }

    #[test]
    fn dead_letter_disposition_does_not_ack_journal() {
        // Pins: poison batches remain pending so compliance verification can see the gap.
        assert!(super::should_ack_journal(super::WriteDisposition::Written));
        assert!(!super::should_ack_journal(
            super::WriteDisposition::DeadLettered
        ));
    }

    #[test]
    fn stable_dead_letter_id_dedupes_replayed_poison_batch() {
        // Pins: leaving a poison batch pending does not create unbounded duplicate dead letters.
        let row = super::PendingRow::Lineage(super::LineageRow {
            turn_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            user_id: "user-1".to_string(),
            storage_partition_id: "partition-1".to_string(),
            ts: Utc::now(),
            tier: 1,
            record_kind: 1,
            payload: serde_json::json!({"kind": "test"}),
            integrity_hash: vec![7; 32],
            prev_hash: None,
        });

        let first = super::stable_dead_letter_id(std::slice::from_ref(&row))
            .expect("dead-letter id should compute");
        let second = super::stable_dead_letter_id(&[row]).expect("dead-letter id should compute");

        assert_eq!(first, second);
    }
}
