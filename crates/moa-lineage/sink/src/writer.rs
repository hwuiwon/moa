//! Async lineage writer worker.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};
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

use crate::clickhouse::is_retryable_clickhouse_error;
use crate::fjall_journal::Journal;
use crate::mpsc_sink::MpscSinkConfig;
use crate::store::LineageStore;
use crate::{Error, Result};

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

/// A fjall-backed durable journal shared by the hot-path sink and writer.
#[derive(Clone)]
pub(crate) struct DurableJournal {
    inner: Arc<StdMutex<Journal>>,
    next_seq: Arc<AtomicU64>,
    replay_cursor: Arc<AtomicU64>,
}

impl DurableJournal {
    /// Opens or creates the shared durable journal.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let journal = Journal::open(path)?;
        let next_seq = next_sequence(&journal)?;
        Ok(Self {
            inner: Arc::new(StdMutex::new(journal)),
            next_seq: Arc::new(AtomicU64::new(next_seq)),
            // Cursor 0 forces the first replay to scan every retained row for crash recovery;
            // journaled sequence numbers always start at 1.
            replay_cursor: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Appends an event to fjall and returns its durable sequence number.
    pub(crate) async fn append_accepted_event(&self, evt: LineageEvent) -> Result<u64> {
        let journal = self.clone();
        tokio::task::spawn_blocking(move || {
            let (seq, _) = journal.append_event_row_sync(evt)?;
            Ok(seq)
        })
        .await?
    }

    /// Appends one event under the journal lock so that fjall insert order matches sequence
    /// order. This invariant lets the writer's low-water-mark replay cursor advance safely: a
    /// lower sequence can never become visible after a higher one has already been scanned.
    fn append_event_row_sync(&self, evt: LineageEvent) -> Result<(u64, PendingRow)> {
        let row = PendingRow::from_event(evt)?;
        let payload = serde_json::to_vec(&row)?;
        let journal = self.lock()?;
        let seq = self.next_seq.fetch_add(1, Ordering::AcqRel);
        journal.append(seq, &payload)?;
        drop(journal);
        record_journal_acceptance(1);
        Ok((seq, row))
    }

    /// Appends a batch of events under one journal lock and one durability sync (group commit).
    ///
    /// Payloads are serialized before the lock is taken; only sequence assignment and the batched
    /// fjall insert happen under the lock, so the whole batch shares a single fsync window while
    /// preserving the insert-order-equals-sequence-order invariant.
    async fn append_event_rows(&self, events: Vec<LineageEvent>) -> Result<Vec<(u64, PendingRow)>> {
        let journal = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut rows = Vec::with_capacity(events.len());
            let mut payloads = Vec::with_capacity(events.len());
            for event in events {
                let row = PendingRow::from_event(event)?;
                payloads.push(serde_json::to_vec(&row)?);
                rows.push(row);
            }

            let guard = journal.lock()?;
            let mut entries = Vec::with_capacity(payloads.len());
            let mut out = Vec::with_capacity(payloads.len());
            for (row, payload) in rows.into_iter().zip(payloads) {
                let seq = journal.next_seq.fetch_add(1, Ordering::AcqRel);
                entries.push((seq, payload));
                out.push((seq, row));
            }
            guard.append_batch(&entries)?;
            drop(guard);
            record_journal_acceptance(entries.len() as u64);
            Ok(out)
        })
        .await?
    }

    async fn replay_from(&self, after_seq: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        let journal = self.clone();
        tokio::task::spawn_blocking(move || journal.lock()?.replay_from(after_seq)).await?
    }

    fn replay_cursor(&self) -> u64 {
        self.replay_cursor.load(Ordering::Acquire)
    }

    fn advance_replay_cursor(&self, seq: u64) {
        self.replay_cursor.fetch_max(seq, Ordering::AcqRel);
    }

    async fn ack_range(&self, lo: u64, hi: u64) -> Result<()> {
        let journal = self.clone();
        tokio::task::spawn_blocking(move || journal.lock()?.ack_range(lo, hi)).await?
    }

    async fn ack_sequences(&self, seqs: &[u64]) -> Result<()> {
        if seqs.is_empty() {
            return Ok(());
        }

        let mut sorted = seqs.to_vec();
        sorted.sort_unstable();
        let mut range_start = sorted[0];
        let mut previous = sorted[0];
        for seq in sorted.into_iter().skip(1) {
            if seq == previous.saturating_add(1) {
                previous = seq;
                continue;
            }
            self.ack_range(range_start, previous).await?;
            range_start = seq;
            previous = seq;
        }
        self.ack_range(range_start, previous).await
    }

    /// Returns the approximate number of unacknowledged journal rows.
    pub(crate) fn approximate_len(&self) -> Result<usize> {
        Ok(self.lock()?.approximate_len())
    }

    async fn approximate_len_async(&self) -> Result<usize> {
        let journal = self.clone();
        tokio::task::spawn_blocking(move || journal.approximate_len()).await?
    }

    /// Returns the age in seconds of the oldest unacknowledged journal row.
    ///
    /// `None` when the journal is empty. Reads the lowest-sequence pending row and
    /// derives the age from its event timestamp, so it reports how long the
    /// oldest still-pending lineage record has waited to reach the row store.
    async fn oldest_pending_age_seconds(&self) -> Result<Option<f64>> {
        let journal = self.clone();
        let oldest = tokio::task::spawn_blocking(move || journal.lock()?.oldest_entry()).await??;
        let Some((_, payload)) = oldest else {
            return Ok(None);
        };
        let row = decode_pending_row(&payload)
            .map_err(|error| Error::Invalid(format!("decode oldest journal row: {error}")))?;
        let age_ms = (Utc::now() - pending_row_ts(&row)).num_milliseconds();
        Ok(Some((age_ms.max(0) as f64) / 1000.0))
    }

    fn lock(&self) -> Result<StdMutexGuard<'_, Journal>> {
        self.inner
            .lock()
            .map_err(|_| Error::Invalid("lineage journal lock poisoned".to_string()))
    }
}

pub(crate) enum WriterCommand {
    /// Raw event that can be dropped before acceptance in lossy telemetry mode.
    Event(Box<LineageEvent>),
    /// Notification that an event has already been appended to the journal.
    Journaled(u64),
}

enum WriterReceiver {
    Raw(mpsc::Receiver<LineageEvent>),
    Commands(mpsc::Receiver<WriterCommand>),
}

impl WriterReceiver {
    async fn recv(&mut self) -> Option<WriterCommand> {
        match self {
            Self::Raw(rx) => rx
                .recv()
                .await
                .map(|event| WriterCommand::Event(Box::new(event))),
            Self::Commands(rx) => rx.recv().await,
        }
    }

    fn try_recv(&mut self) -> std::result::Result<WriterCommand, mpsc::error::TryRecvError> {
        match self {
            Self::Raw(rx) => rx
                .try_recv()
                .map(|event| WriterCommand::Event(Box::new(event))),
            Self::Commands(rx) => rx.try_recv(),
        }
    }
}

/// Spawns the lineage writer worker.
pub async fn spawn_writer(
    rx: mpsc::Receiver<LineageEvent>,
    config: MpscSinkConfig,
    store: LineageStore,
) -> Result<WriterHandle> {
    store.ensure_schema().await?;
    let journal = DurableJournal::open(&config.journal_path)?;
    spawn_writer_task(WriterReceiver::Raw(rx), config, store, journal)
}

/// Spawns the writer for the production sink command channel.
pub(crate) async fn spawn_writer_for_sink(
    rx: mpsc::Receiver<WriterCommand>,
    config: MpscSinkConfig,
    store: LineageStore,
    journal: DurableJournal,
) -> Result<WriterHandle> {
    store.ensure_schema().await?;
    spawn_writer_task(WriterReceiver::Commands(rx), config, store, journal)
}

fn spawn_writer_task(
    rx: WriterReceiver,
    config: MpscSinkConfig,
    store: LineageStore,
    journal: DurableJournal,
) -> Result<WriterHandle> {
    let shutdown = CancellationToken::new();
    let stats = Arc::new(SharedWriterStats::default());
    let worker_shutdown = shutdown.clone();
    let worker_stats = stats.clone();
    let join = tokio::spawn(async move {
        run_writer(rx, config, store, journal, worker_shutdown, worker_stats).await
    });

    Ok(WriterHandle {
        shutdown,
        join: Arc::new(Mutex::new(Some(join))),
        stats,
    })
}

async fn run_writer(
    mut rx: WriterReceiver,
    config: MpscSinkConfig,
    store: LineageStore,
    journal: DurableJournal,
    shutdown: CancellationToken,
    stats: Arc<SharedWriterStats>,
) -> Result<WriterStats> {
    replay_pending(&journal, &store, &stats).await?;

    let mut batch = Vec::with_capacity(config.batch_size);
    let mut flush_interval = tokio::time::interval(config.batch_max_age);
    flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                while let Ok(command) = rx.try_recv() {
                    handle_writer_command(command, &journal, &store, &stats, &mut batch, config.batch_size).await?;
                }
                flush_events(&journal, &store, &stats, &mut batch).await?;
                replay_pending(&journal, &store, &stats).await?;
                break;
            }
            maybe_command = rx.recv() => {
                match maybe_command {
                    Some(command) => {
                        handle_writer_command(command, &journal, &store, &stats, &mut batch, config.batch_size).await?;
                    }
                    None => {
                        flush_events(&journal, &store, &stats, &mut batch).await?;
                        replay_pending(&journal, &store, &stats).await?;
                        break;
                    }
                }
            }
            _ = flush_interval.tick() => {
                flush_events(&journal, &store, &stats, &mut batch).await?;
                replay_pending(&journal, &store, &stats).await?;
            }
        }
    }

    record_journal_metrics(&journal, &stats).await?;
    Ok(stats.snapshot())
}

async fn handle_writer_command(
    command: WriterCommand,
    journal: &DurableJournal,
    store: &LineageStore,
    stats: &Arc<SharedWriterStats>,
    batch: &mut Vec<LineageEvent>,
    batch_size: usize,
) -> Result<()> {
    match command {
        WriterCommand::Event(evt) => {
            batch.push(*evt);
            if batch.len() >= batch_size {
                flush_events(journal, store, stats, batch).await?;
            }
        }
        WriterCommand::Journaled(seq) => {
            metrics::counter!("moa_lineage_journal_notifications_total").increment(1);
            tracing::trace!(seq, "received journaled lineage event notification");
            flush_events(journal, store, stats, batch).await?;
            replay_pending(journal, store, stats).await?;
        }
    }
    Ok(())
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
    journal: &DurableJournal,
    store: &LineageStore,
    stats: &Arc<SharedWriterStats>,
) -> Result<()> {
    let cursor = journal.replay_cursor();
    let pending = journal.replay_from(cursor).await?;
    if pending.is_empty() {
        record_journal_metrics(journal, stats).await?;
        return Ok(());
    }

    let rows = pending
        .iter()
        .map(|(_, payload)| decode_pending_row(payload))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let disposition = write_pending_rows_or_dead_letter(store, &rows).await?;
    let (Some((lo, _)), Some((hi, _))) = (pending.first(), pending.last()) else {
        return Ok(());
    };
    if should_ack_journal(disposition) {
        journal.ack_range(*lo, *hi).await?;
    }
    // Advance the low-water mark past this batch regardless of disposition so retained
    // (dead-lettered) rows below the cursor are not re-scanned or re-attempted on every
    // subsequent notification, keeping replay incremental instead of O(N^2).
    journal.advance_replay_cursor(*hi);
    if disposition == WriteDisposition::Written {
        record_flush(stats, rows.len());
    }
    record_journal_metrics(journal, stats).await?;
    Ok(())
}

async fn flush_events(
    journal: &DurableJournal,
    store: &LineageStore,
    stats: &Arc<SharedWriterStats>,
    batch: &mut Vec<LineageEvent>,
) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }

    let events = std::mem::take(batch);
    let mut rows = Vec::with_capacity(events.len());
    let mut seqs = Vec::with_capacity(events.len());
    for (seq, row) in journal.append_event_rows(events).await? {
        seqs.push(seq);
        rows.push(row);
    }
    record_journal_metrics(journal, stats).await?;

    let disposition = write_pending_rows_or_dead_letter(store, &rows).await?;
    if should_ack_journal(disposition) {
        journal.ack_sequences(&seqs).await?;
    }
    if disposition == WriteDisposition::Written {
        record_flush(stats, rows.len());
    }
    record_journal_metrics(journal, stats).await?;
    Ok(())
}

fn should_ack_journal(disposition: WriteDisposition) -> bool {
    // Both a successful write and a dead-letter acknowledge (remove) the journal
    // sequences. A `DeadLettered` disposition is only returned AFTER the
    // dead-letter row is durably committed to Postgres (see
    // `write_pending_rows_or_dead_letter`), so acking here can never lose
    // records: a crash between the DLQ commit and the ack replays the same
    // journal bytes on restart, which re-derives the identical content-addressed
    // `dead_letter_id` and upserts it (`ON CONFLICT DO UPDATE`) rather than
    // duplicating the row. The dead-letter table is therefore the sole
    // retention/redrive owner; leaving poison rows in the journal only grew local
    // storage without bound and re-dead-lettered them on every restart.
    matches!(
        disposition,
        WriteDisposition::Written | WriteDisposition::DeadLettered
    )
}

async fn write_pending_rows_or_dead_letter(
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
        Error::ClickHouse(clickhouse_error) => is_retryable_clickhouse_error(clickhouse_error),
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

async fn write_pending_rows(store: &LineageStore, rows: &[PendingRow]) -> Result<()> {
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

    match store {
        LineageStore::Postgres(pool) => write_rows(pool, &lineage_rows).await?,
        LineageStore::ClickHouse {
            clickhouse,
            postgres,
        } => {
            warn_on_unchained_compliance_partitions(postgres, &lineage_rows).await?;
            clickhouse.insert_lineage_rows(&lineage_rows).await?;
        }
    }
    write_score_rows(store.postgres(), &score_rows).await?;
    Ok(())
}

/// Warns when a compliance-enabled partition's rows land in ClickHouse.
///
/// The audit hash chain needs a transactional fold over the row store, which
/// the ClickHouse backend does not provide. Rows keep their per-row canonical
/// `integrity_hash`, but `prev_hash` linking (and therefore Merkle roots and
/// `moa lineage verify`) requires the Postgres backend.
async fn warn_on_unchained_compliance_partitions(
    pool: &sqlx::PgPool,
    rows: &[LineageRow],
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
    let enabled: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT storage_partition_id
        FROM analytics.compliance_tenants
        WHERE storage_partition_id = ANY($1) AND enabled
        "#,
    )
    .bind(&partitions)
    .fetch_all(pool)
    .await?;
    if enabled.is_empty() {
        return Ok(());
    }

    let unchained_rows = rows
        .iter()
        .filter(|row| enabled.contains(&row.storage_partition_id))
        .count();
    metrics::counter!("moa_lineage_compliance_chain_skipped_total")
        .increment(unchained_rows as u64);
    tracing::warn!(
        partitions = ?enabled,
        rows = unchained_rows,
        "compliance hash chaining is unavailable on the clickhouse lineage backend; \
         rows written without prev_hash links"
    );
    Ok(())
}

async fn write_rows(pool: &sqlx::PgPool, rows: &[LineageRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut rows = rows.to_vec();
    let mut tx = pool.begin().await?;
    apply_compliance_hashes(&mut tx, &mut rows).await?;

    // Reuse a persistent per-connection staging table instead of dropping and recreating one
    // per drain: `ON COMMIT DELETE ROWS` empties it at each commit, so there is no catalog churn
    // and prepared-statement caching for the COPY/INSERT is preserved.
    sqlx::query(
        r#"
        CREATE TEMP TABLE IF NOT EXISTS lineage_copy (
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
        ) ON COMMIT DELETE ROWS;
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

    tx.commit().await?;
    Ok(())
}

/// In-memory outcome of folding one compliance partition's hash chain over a flush batch.
struct PartitionChainOutcome {
    /// Chain tip after the last newly linked row, when at least one row was linked.
    final_hash: Option<Vec<u8>>,
    /// Timestamp of the last newly linked row, mirrored into the partition state.
    last_ts: Option<DateTime<Utc>>,
    /// Number of rows newly linked into the chain (existing rows are reused, not re-linked).
    new_rows: usize,
}

/// Applies compliance hash chaining to a flush batch with per-partition (not per-row) round trips.
///
/// The enabled-tenant check runs once for the whole batch. For each compliance-enabled partition
/// the advisory lock is taken once, the chain tip is read once `FOR UPDATE`, already-persisted
/// rows are fetched once for idempotent reuse, the chain is folded in memory, and the partition
/// state is updated once. This turns a `512`-row batch from `512+` serial round trips into a small
/// constant per distinct partition while producing the same chain as a per-row walk.
async fn apply_compliance_hashes(
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
type ExistingChainRows =
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
fn fold_partition_chain(
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

    let mut tx = pool.begin().await?;
    // Reuse a persistent per-connection staging table (see `write_rows`): `ON COMMIT DELETE ROWS`
    // clears it at commit, avoiding per-drain catalog churn and preserving statement caching.
    sqlx::query(
        r#"
        CREATE TEMP TABLE IF NOT EXISTS lineage_scores_copy (
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
        ) ON COMMIT DELETE ROWS;
        "#,
    )
    .execute(&mut *tx)
    .await?;

    let copy_payload = render_score_copy_csv(rows);
    let mut copy = (*tx)
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
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
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

fn record_journal_acceptance(rows: u64) {
    metrics::counter!("moa_lineage_accepted_total", "durability" => "journal").increment(rows);
}

fn record_journal_depth(stats: &SharedWriterStats, depth: u64) {
    stats.journal_depth.store(depth, Ordering::Relaxed);
    metrics::gauge!("moa_lineage_journal_depth").set(depth as f64);
}

/// Refreshes the journal-health gauges: pending depth and oldest-unacked age.
///
/// After the F16 fix both successful and dead-lettered batches are acknowledged,
/// so a healthy writer keeps depth and oldest-age near zero; a rising oldest-age
/// means rows are stuck pending (the row store is failing without dead-lettering
/// yet), which the depth gauge alone cannot distinguish from steady throughput.
async fn record_journal_metrics(journal: &DurableJournal, stats: &SharedWriterStats) -> Result<()> {
    record_journal_depth(stats, journal.approximate_len_async().await? as u64);
    let oldest_age = journal.oldest_pending_age_seconds().await?;
    metrics::gauge!("moa_lineage_journal_oldest_age_seconds").set(oldest_age.unwrap_or(0.0));
    Ok(())
}

fn pending_row_ts(row: &PendingRow) -> DateTime<Utc> {
    match row {
        PendingRow::Lineage(row) => row.ts,
        PendingRow::Score(row) => row.ts,
    }
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

/// Journaled `turn_lineage` row shared by the Postgres and ClickHouse writers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LineageRow {
    pub(crate) turn_id: Uuid,
    pub(crate) session_id: Uuid,
    pub(crate) user_id: String,
    pub(crate) storage_partition_id: String,
    pub(crate) ts: DateTime<Utc>,
    pub(crate) tier: i16,
    pub(crate) record_kind: i16,
    pub(crate) payload: serde_json::Value,
    pub(crate) integrity_hash: Vec<u8>,
    pub(crate) prev_hash: Option<Vec<u8>>,
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
    use moa_lineage_core::chain::HashChain;
    use moa_lineage_core::{
        LineageEvent, ScoreRecord, ScoreSource, ScoreTarget, ScoreValue, TurnId,
    };
    use uuid::Uuid;

    fn test_lineage_row(partition: &str, payload: serde_json::Value) -> super::LineageRow {
        super::LineageRow {
            turn_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            user_id: "chain-user".to_string(),
            storage_partition_id: partition.to_string(),
            ts: Utc::now(),
            tier: 1,
            record_kind: 1,
            payload,
            integrity_hash: Vec::new(),
            prev_hash: None,
        }
    }

    #[test]
    fn append_batch_groups_appends_into_one_persist() {
        // Pins: a batched append performs exactly one durability sync regardless of batch size
        // (group commit), while single appends sync per row.
        use crate::fjall_journal::Journal;

        let dir = std::env::temp_dir().join(format!("moa-lineage-groupcommit-{}", Uuid::now_v7()));
        let journal = Journal::open(&dir).expect("journal should open");

        let baseline = journal.persist_count();
        let entries: Vec<(u64, Vec<u8>)> = (1..=5).map(|seq| (seq, vec![seq as u8; 8])).collect();
        journal
            .append_batch(&entries)
            .expect("batch append should sync");
        assert_eq!(
            journal.persist_count() - baseline,
            1,
            "a five-row batch append must sync exactly once"
        );
        assert_eq!(journal.approximate_len(), 5);

        let before_single = journal.persist_count();
        journal
            .append(6, b"one")
            .expect("single append should sync");
        journal
            .append(7, b"two")
            .expect("single append should sync");
        assert_eq!(
            journal.persist_count() - before_single,
            2,
            "single appends must sync once per row"
        );
    }

    #[test]
    fn fold_partition_chain_matches_sequential_link_walk() {
        // Pins: the batched in-memory fold yields the same per-row integrity/prev hashes and the
        // same final tip as a straight per-row HashChain::link walk from the same starting tip.
        let payloads: Vec<serde_json::Value> = (0..4)
            .map(|n| serde_json::json!({ "event": "e", "n": n }))
            .collect();
        let partition = "chain-partition";
        let mut rows: Vec<super::LineageRow> = payloads
            .iter()
            .map(|payload| test_lineage_row(partition, payload.clone()))
            .collect();
        let indices: Vec<usize> = (0..rows.len()).collect();
        let existing = super::ExistingChainRows::new();

        let outcome = super::fold_partition_chain(None, &mut rows, &indices, &existing)
            .expect("fold should succeed");
        assert_eq!(outcome.new_rows, 4);

        let mut prev = None;
        let mut expected_final = None;
        for (row, payload) in rows.iter().zip(&payloads) {
            let (integrity, prev_echo) = HashChain::link(prev, payload).expect("link");
            assert_eq!(row.integrity_hash, integrity.as_bytes().to_vec());
            assert_eq!(
                row.prev_hash,
                prev_echo.map(|hash| hash.as_bytes().to_vec())
            );
            prev = Some(integrity);
            expected_final = Some(integrity.as_bytes().to_vec());
        }
        assert_eq!(outcome.final_hash, expected_final);
        assert_eq!(outcome.last_ts, Some(rows[3].ts));
    }

    #[test]
    fn fold_partition_chain_reuses_existing_rows_without_advancing() {
        // Pins: a row already present in turn_lineage reuses its stored hashes and does not
        // advance the chain tip, so replayed batches are idempotent.
        let payload_a = serde_json::json!({ "event": "a" });
        let payload_b = serde_json::json!({ "event": "b" });
        let partition = "chain-partition";
        let mut rows = vec![
            test_lineage_row(partition, payload_a),
            test_lineage_row(partition, payload_b.clone()),
        ];
        let indices = vec![0_usize, 1];

        let mut existing = super::ExistingChainRows::new();
        existing.insert(
            (rows[0].turn_id, rows[0].record_kind, rows[0].ts),
            (vec![9_u8; 32], Some(vec![8_u8; 32])),
        );

        let outcome = super::fold_partition_chain(None, &mut rows, &indices, &existing)
            .expect("fold should succeed");

        assert_eq!(rows[0].integrity_hash, vec![9_u8; 32]);
        assert_eq!(rows[0].prev_hash, Some(vec![8_u8; 32]));
        assert_eq!(outcome.new_rows, 1);
        let (expected, _) = HashChain::link(None, &payload_b).expect("link");
        assert_eq!(rows[1].integrity_hash, expected.as_bytes().to_vec());
        assert_eq!(outcome.final_hash, Some(expected.as_bytes().to_vec()));
    }

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
    fn dead_letter_disposition_acks_journal() {
        // Pins (F16): both successful writes and dead-letters acknowledge the journal
        // sequences. Dead-lettered rows are durably committed to the DLQ table first,
        // so acking removes them from the journal (bounding local storage) instead of
        // retaining and re-dead-lettering them on every restart.
        assert!(super::should_ack_journal(super::WriteDisposition::Written));
        assert!(super::should_ack_journal(
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
