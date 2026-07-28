//! Writer task lifecycle: ingress acceptance, claim polling, and bounded drain.
//!
//! The task has exactly two jobs. It moves best-effort ingress events into the
//! durable acceptance queue, and it claims committed queue rows and stores their
//! content. Neither is on any caller's critical path, because the durable API
//! commits its own batch before returning.

use std::sync::Arc;

use moa_lineage_core::LineageEvent;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::mpsc_sink::MpscSinkConfig;
use crate::store::LineageStore;
use crate::{Error, Result};

use super::acceptance::{ClaimedRow, LineageJournal, decode_claimed};
use super::retry::{FailureDisposition, classify_failure, write_dead_letter_in_tx};
use super::rows::PendingRow;
use super::storage::write_pending_rows;
use super::{SharedWriterState, WriterHandle, WriterState, WriterStats};

/// Spawns the lineage writer worker over a plain event channel.
///
/// The channel is best-effort ingress: events are moved into the durable queue
/// by the worker, and an event still in the channel when the process dies was
/// never accepted. Callers needing durability use
/// [`MpscSink::record_durable_events`](crate::MpscSink::record_durable_events),
/// which commits before it returns.
pub async fn spawn_writer(
    rx: mpsc::Receiver<LineageEvent>,
    config: MpscSinkConfig,
    store: LineageStore,
) -> Result<WriterHandle> {
    store.ensure_schema().await?;
    let journal = LineageJournal::new(store.postgres().clone(), config.lease_ttl);
    spawn_writer_task(rx, Arc::new(Notify::new()), config, store, journal)
}

/// Spawns the writer behind the production sink, sharing its wake signal.
pub(crate) fn spawn_writer_for_sink(
    rx: mpsc::Receiver<LineageEvent>,
    wake: Arc<Notify>,
    config: MpscSinkConfig,
    store: LineageStore,
    journal: LineageJournal,
) -> Result<WriterHandle> {
    spawn_writer_task(rx, wake, config, store, journal)
}

fn spawn_writer_task(
    rx: mpsc::Receiver<LineageEvent>,
    wake: Arc<Notify>,
    config: MpscSinkConfig,
    store: LineageStore,
    journal: LineageJournal,
) -> Result<WriterHandle> {
    let shutdown = CancellationToken::new();
    let shared = Arc::new(SharedWriterState::new());
    shared.set_state(WriterState::Running);

    let worker_shutdown = shutdown.clone();
    let worker_shared = shared.clone();
    let max_pending_age = config.max_pending_age;
    let join = tokio::spawn(async move {
        let outcome = run_writer(
            rx,
            wake,
            config,
            store,
            journal,
            worker_shutdown,
            worker_shared.clone(),
        )
        .await;
        match &outcome {
            Ok(_) => worker_shared.set_state(WriterState::Stopped),
            Err(error) => worker_shared.set_fatal(error.to_string()),
        }
        outcome
    });

    Ok(WriterHandle {
        shutdown,
        join: Mutex::new(Some(join)),
        shared,
        max_pending_age,
    })
}

async fn run_writer(
    mut rx: mpsc::Receiver<LineageEvent>,
    wake: Arc<Notify>,
    config: MpscSinkConfig,
    store: LineageStore,
    journal: LineageJournal,
    shutdown: CancellationToken,
    shared: Arc<SharedWriterState>,
) -> Result<WriterStats> {
    let mut ingress = Vec::with_capacity(config.batch_size);
    let mut poll = tokio::time::interval(config.batch_max_age);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            maybe_event = rx.recv() => {
                match maybe_event {
                    Some(event) => {
                        ingress.push(event);
                        if ingress.len() >= config.batch_size {
                            accept_ingress(&journal, &shared, &mut ingress).await;
                        }
                    }
                    // Every sender is gone, so no further ingress is possible.
                    // Fall through to the drain rather than spinning on a closed
                    // channel that returns `None` forever.
                    None => break,
                }
            }
            () = wake.notified() => {
                drain_once(&store, &journal, &shared, &config).await;
            }
            _ = poll.tick() => {
                accept_ingress(&journal, &shared, &mut ingress).await;
                drain_once(&store, &journal, &shared, &config).await;
            }
        }
    }

    shared.set_state(WriterState::Draining);
    // Take everything already queued in memory before the drain: those events
    // are not durable yet, and this is the last chance to commit them.
    while let Ok(event) = rx.try_recv() {
        ingress.push(event);
    }
    accept_ingress(&journal, &shared, &mut ingress).await;
    drain_until_idle(&store, &journal, &shared, &config).await;
    refresh_backlog(&journal, &shared).await;
    Ok(shared.stats())
}

/// Commits buffered best-effort ingress into the durable queue.
///
/// A failure here is counted and the buffer is dropped: these events were never
/// promised durability, and retaining them unboundedly in memory would trade a
/// telemetry gap for an out-of-memory kill that costs the accepted records too.
async fn accept_ingress(
    journal: &LineageJournal,
    shared: &SharedWriterState,
    ingress: &mut Vec<LineageEvent>,
) {
    if ingress.is_empty() {
        return;
    }
    let events = std::mem::take(ingress);
    let count = events.len() as u64;
    let mut rows = Vec::with_capacity(events.len());
    for event in events {
        match PendingRow::from_event(event) {
            Ok(row) => rows.push(row),
            Err(error) => {
                metrics::counter!("moa_lineage_malformed_total").increment(1);
                tracing::warn!(%error, "lineage ingress event could not be rendered as a row");
            }
        }
    }
    match journal.accept_batch(&rows).await {
        Ok(_) => shared.set_queue_reachable(true),
        Err(error) => {
            shared.set_queue_reachable(false);
            metrics::counter!(
                "moa_lineage_failed_total",
                "mode" => "best_effort",
                "reason" => "accept_failed"
            )
            .increment(count);
            tracing::warn!(%error, count, "lineage ingress batch could not be accepted");
        }
    }
}

/// Claims and stores one batch. Returns whether it did any work.
async fn drain_once(
    store: &LineageStore,
    journal: &LineageJournal,
    shared: &SharedWriterState,
    config: &MpscSinkConfig,
) -> bool {
    shared.record_claim_poll();
    let claimed = match journal.claim_batch(config.claim_batch_size).await {
        Ok(claimed) => {
            shared.set_queue_reachable(true);
            claimed
        }
        Err(error) => {
            shared.set_queue_reachable(false);
            tracing::warn!(%error, "lineage claim failed");
            return false;
        }
    };
    if claimed.is_empty() {
        refresh_backlog(journal, shared).await;
        return false;
    }

    store_claimed(store, journal, shared, &claimed).await;
    refresh_backlog(journal, shared).await;
    true
}

/// Drains until the queue reports nothing claimable or the drain budget expires.
///
/// The budget is what makes shutdown bounded. Expiring it is not data loss: the
/// rows are committed and unleased (every claim this writer takes is resolved
/// before it returns), so a successor claims them on its next poll.
async fn drain_until_idle(
    store: &LineageStore,
    journal: &LineageJournal,
    shared: &SharedWriterState,
    config: &MpscSinkConfig,
) {
    let deadline = tokio::time::Instant::now() + config.drain_timeout;
    loop {
        if tokio::time::Instant::now() >= deadline {
            metrics::counter!("moa_lineage_drain_timeout_total").increment(1);
            tracing::warn!(
                drain_timeout_ms = config.drain_timeout.as_millis() as u64,
                "lineage drain budget expired; committed rows remain for another replica"
            );
            return;
        }
        if !drain_once(store, journal, shared, config).await {
            return;
        }
    }
}

/// Stores one claimed batch, dequeuing it in the same transaction.
async fn store_claimed(
    store: &LineageStore,
    journal: &LineageJournal,
    shared: &SharedWriterState,
    claimed: &[ClaimedRow],
) {
    let journal_ids = claimed
        .iter()
        .map(|row| row.journal_id)
        .collect::<Vec<Uuid>>();
    let attempts = claimed.iter().map(|row| row.attempts).max().unwrap_or(1);

    let mut rows = Vec::with_capacity(claimed.len());
    for row in claimed {
        match decode_claimed(row) {
            Ok(decoded) => rows.push(decoded),
            Err(error) => {
                // An undecodable payload can never be stored, so the whole batch
                // is dead-lettered rather than retried: the readable rows are
                // preserved in the dead letter, and leaving the poison row
                // queued would stall everything behind it forever.
                dead_letter_batch(journal, shared, &journal_ids, &rows, &error, attempts).await;
                return;
            }
        }
    }

    match commit_batch(store, journal, &rows, &journal_ids).await {
        Ok(()) => {
            shared.set_queue_reachable(true);
            shared.record_written(rows.len() as u64);
        }
        Err(error) => match classify_failure(&error, attempts) {
            FailureDisposition::Retry { backoff } => {
                // Deliberately does NOT mark the queue unreachable. The claim,
                // the backlog probe and the deferral below all just succeeded
                // against it; what failed is the ROW STORE. Conflating the two
                // made one failing batch report "the acceptance queue is
                // unreachable", which is false and points an operator at the
                // wrong system. A store failure that actually matters escalates
                // through the backlog age instead, which is the signal that
                // distinguishes a blip from a stall.
                tracing::warn!(
                    %error,
                    attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    rows = rows.len(),
                    "lineage batch write failed; rows preserved for retry"
                );
                if let Err(defer_error) = journal.defer_claim(&journal_ids, backoff).await {
                    tracing::error!(
                        error = %defer_error,
                        "lineage retry deferral failed; the lease will expire instead"
                    );
                }
            }
            FailureDisposition::Permanent => {
                dead_letter_batch(journal, shared, &journal_ids, &rows, &error, attempts).await;
            }
        },
    }
}

/// Stores the batch and removes it from the queue in one transaction.
async fn commit_batch(
    store: &LineageStore,
    journal: &LineageJournal,
    rows: &[PendingRow],
    journal_ids: &[Uuid],
) -> Result<()> {
    let mut tx = journal.begin().await?;
    write_pending_rows(store, &mut tx, rows).await?;
    let dequeued = LineageJournal::dequeue_in_tx(&mut tx, journal_ids).await?;
    if dequeued != journal_ids.len() as u64 {
        // The claimed rows are leased to this writer, so nothing else should be
        // removing them. A short count means the queue disagrees with the claim,
        // and committing anyway would leave rows that are stored but still
        // queued, which the next claim would store a second time.
        return Err(Error::Invalid(format!(
            "lineage dequeue removed {dequeued} of {} claimed rows",
            journal_ids.len()
        )));
    }
    tx.commit().await?;
    Ok(())
}

/// Dead-letters a batch and dequeues it in one transaction.
async fn dead_letter_batch(
    journal: &LineageJournal,
    shared: &SharedWriterState,
    journal_ids: &[Uuid],
    rows: &[PendingRow],
    error: &Error,
    attempts: i32,
) {
    let outcome = async {
        let mut tx = journal.begin().await?;
        let dead_letter_id = write_dead_letter_in_tx(&mut tx, rows, error, attempts).await?;
        LineageJournal::dequeue_in_tx(&mut tx, journal_ids).await?;
        tx.commit().await?;
        Ok::<Uuid, Error>(dead_letter_id)
    }
    .await;

    match outcome {
        Ok(dead_letter_id) => {
            shared.set_queue_reachable(true);
            tracing::error!(
                dead_letter_id = %dead_letter_id,
                error = %error,
                attempts,
                row_count = rows.len(),
                "lineage batch moved to dead letter storage"
            );
        }
        Err(dlq_error) => {
            // The rows stay queued. That is the correct outcome: an unwritable
            // dead letter must not consume the records it was supposed to
            // preserve.
            shared.set_queue_reachable(false);
            tracing::error!(
                error = %dlq_error,
                original_error = %error,
                row_count = rows.len(),
                "lineage dead-letter commit failed; rows remain queued"
            );
        }
    }
}

async fn refresh_backlog(journal: &LineageJournal, shared: &SharedWriterState) {
    match journal.backlog().await {
        Ok(backlog) => {
            shared.set_queue_reachable(true);
            shared.record_backlog(backlog);
        }
        Err(error) => {
            shared.set_queue_reachable(false);
            tracing::warn!(%error, "lineage backlog probe failed");
        }
    }
}
