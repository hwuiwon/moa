//! Async lineage writer worker.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use moa_lineage_core::LineageEvent;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::fjall_journal::Journal;
use crate::mpsc_sink::MpscSinkConfig;
use crate::{Result, ensure_schema};

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

    stats
        .journal_depth
        .store(journal.approximate_len() as u64, Ordering::Relaxed);
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
    stats
        .journal_depth
        .store(pending.len() as u64, Ordering::Relaxed);
    if pending.is_empty() {
        return Ok(());
    }

    let rows = pending
        .iter()
        .map(|(_, payload)| serde_json::from_slice::<LineageRow>(payload))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    write_rows_with_retry(pool, &rows).await?;
    if let (Some((lo, _)), Some((hi, _))) = (pending.first(), pending.last()) {
        journal.ack_range(*lo, *hi)?;
    }
    record_flush(stats, rows.len());
    stats
        .journal_depth
        .store(journal.approximate_len() as u64, Ordering::Relaxed);
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
        let row = LineageRow::from_event(evt)?;
        let payload = serde_json::to_vec(&row)?;
        journal.append(*seq, &payload)?;
        end_seq = *seq;
        *seq = (*seq).saturating_add(1);
        rows.push(row);
    }
    stats
        .journal_depth
        .store(journal.approximate_len() as u64, Ordering::Relaxed);

    write_rows_with_retry(pool, &rows).await?;
    journal.ack_range(start_seq, end_seq)?;
    record_flush(stats, rows.len());
    stats
        .journal_depth
        .store(journal.approximate_len() as u64, Ordering::Relaxed);
    Ok(())
}

async fn write_rows_with_retry(pool: &sqlx::PgPool, rows: &[LineageRow]) -> Result<()> {
    let mut delay = Duration::from_millis(100);
    loop {
        match write_rows(pool, rows).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(%error, retry_after_ms = delay.as_millis(), "lineage write failed");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(5));
            }
        }
    }
}

async fn write_rows(pool: &sqlx::PgPool, rows: &[LineageRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    let mut conn = pool.acquire().await?;
    sqlx::query("DROP TABLE IF EXISTS lineage_copy")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        r#"
        CREATE TEMP TABLE lineage_copy (
            turn_id        UUID        NOT NULL,
            session_id     UUID        NOT NULL,
            user_id        TEXT        NOT NULL,
            workspace_id   TEXT        NOT NULL,
            ts             TIMESTAMPTZ NOT NULL,
            tier           SMALLINT    NOT NULL,
            record_kind    SMALLINT    NOT NULL,
            payload        JSONB       NOT NULL,
            integrity_hash BYTEA       NOT NULL,
            prev_hash      BYTEA
        );
        "#,
    )
    .execute(&mut *conn)
    .await?;

    let copy_payload = render_copy_csv(rows);
    let mut copy = conn
        .copy_in_raw(
            r#"
            COPY lineage_copy (
                turn_id,
                session_id,
                user_id,
                workspace_id,
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
            workspace_id,
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
            workspace_id,
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
    .execute(&mut *conn)
    .await?;

    sqlx::query("DROP TABLE IF EXISTS lineage_copy")
        .execute(&mut *conn)
        .await?;
    Ok(())
}

fn render_copy_csv(rows: &[LineageRow]) -> String {
    let mut out = String::new();
    for row in rows {
        let fields = [
            csv_field(&row.turn_id.to_string()),
            csv_field(&row.session_id.to_string()),
            csv_field(&row.user_id),
            csv_field(&row.workspace_id),
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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct LineageRow {
    turn_id: Uuid,
    session_id: Uuid,
    user_id: String,
    workspace_id: String,
    ts: DateTime<Utc>,
    tier: i16,
    record_kind: i16,
    payload: serde_json::Value,
    integrity_hash: Vec<u8>,
    prev_hash: Option<Vec<u8>>,
}

impl LineageRow {
    fn from_event(evt: LineageEvent) -> Result<Self> {
        let mut payload = serde_json::to_value(&evt)?;
        sort_json_value(&mut payload);
        let integrity_hash = blake3::hash(payload.to_string().as_bytes())
            .as_bytes()
            .to_vec();
        let record_kind = evt.record_kind().as_i16();
        let fallback_ts = Utc::now();

        let (turn_id, session_id, user_id, workspace_id, ts) = match &evt {
            LineageEvent::Retrieval(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.workspace_id.to_string(),
                record.ts,
            ),
            LineageEvent::Context(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.workspace_id.to_string(),
                record.ts,
            ),
            LineageEvent::Generation(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.workspace_id.to_string(),
                record.ts,
            ),
            LineageEvent::Citation(record) => (
                record.turn_id.0,
                record.session_id.0,
                record.user_id.to_string(),
                record.workspace_id.to_string(),
                record.ts,
            ),
            LineageEvent::Eval(_) | LineageEvent::Decision(_) => (
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
            workspace_id,
            ts,
            tier: 1,
            record_kind,
            payload,
            integrity_hash,
            prev_hash: None,
        })
    }
}

fn sort_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                sort_json_value(item);
            }
        }
        serde_json::Value::Object(map) => {
            let mut entries = map
                .iter()
                .map(|(key, value)| {
                    let mut value = value.clone();
                    sort_json_value(&mut value);
                    (key.clone(), value)
                })
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            map.clear();
            for (key, value) in entries {
                map.insert(key, value);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::sort_json_value;

    #[test]
    fn canonical_sort_orders_nested_object_keys() {
        let mut value = serde_json::json!({
            "b": 1,
            "a": {
                "d": 2,
                "c": 3
            }
        });
        sort_json_value(&mut value);
        assert_eq!(value.to_string(), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }

    #[test]
    fn writer_stats_default_has_no_flush_timestamp() {
        assert_eq!(super::WriterStats::default().last_flush_unix_ms, None);
    }
}
