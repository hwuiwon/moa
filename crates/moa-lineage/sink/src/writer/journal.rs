//! Durable journal persistence, ordered replay, acknowledgement, and health metrics.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, MutexGuard as StdMutexGuard};

use chrono::Utc;
use moa_lineage_core::LineageEvent;

use crate::fjall_journal::Journal;
use crate::store::LineageStore;
use crate::{Error, Result};

use super::SharedWriterStats;
use super::retry::{WriteDisposition, write_pending_rows_or_dead_letter};
use super::rows::{PendingRow, decode_pending_row, pending_row_ts};

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

    /// Appends a batch of events under one durability sync and returns their
    /// durable sequence numbers in input order (group commit).
    ///
    /// Backs the awaitable durable-batch path so one emission point pays a single
    /// fsync for all its lineage events instead of one per event.
    pub(crate) async fn append_accepted_events(
        &self,
        events: Vec<LineageEvent>,
    ) -> Result<Vec<u64>> {
        Ok(self
            .append_event_rows(events)
            .await?
            .into_iter()
            .map(|(seq, _)| seq)
            .collect())
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
    pub(super) async fn append_event_rows(
        &self,
        events: Vec<LineageEvent>,
    ) -> Result<Vec<(u64, PendingRow)>> {
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

    /// Returns the number of durability syncs issued by the underlying journal.
    ///
    /// Exposed for group-commit tests to assert that a batch append performs one fsync.
    #[cfg(test)]
    pub(crate) fn persist_count(&self) -> Result<u64> {
        Ok(self.lock()?.persist_count())
    }
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

pub(super) async fn replay_pending(
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

pub(super) async fn flush_events(
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

pub(super) fn should_ack_journal(disposition: WriteDisposition) -> bool {
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
pub(super) async fn record_journal_metrics(
    journal: &DurableJournal,
    stats: &SharedWriterStats,
) -> Result<()> {
    record_journal_depth(stats, journal.approximate_len_async().await? as u64);
    let oldest_age = journal.oldest_pending_age_seconds().await?;
    metrics::gauge!("moa_lineage_journal_oldest_age_seconds").set(oldest_age.unwrap_or(0.0));
    Ok(())
}
