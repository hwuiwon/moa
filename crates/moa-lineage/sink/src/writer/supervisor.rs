//! Writer task lifecycle, bounded receiver draining, and flush scheduling.

use std::sync::Arc;

use moa_lineage_core::LineageEvent;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::Result;
use crate::mpsc_sink::MpscSinkConfig;
use crate::store::LineageStore;

use super::journal::{DurableJournal, flush_events, record_journal_metrics, replay_pending};
use super::{SharedWriterStats, WriterHandle, WriterStats};

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
