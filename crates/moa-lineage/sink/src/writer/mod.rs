//! Async lineage writer worker.

mod compliance;
mod journal;
mod retry;
mod rows;
mod storage;
mod supervisor;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::Result;

pub(crate) use journal::DurableJournal;
pub(crate) use rows::LineageRow;
pub use supervisor::spawn_writer;
pub(crate) use supervisor::{WriterCommand, spawn_writer_for_sink};

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
pub(super) struct SharedWriterStats {
    pub(super) written: AtomicU64,
    pub(super) journal_depth: AtomicU64,
    pub(super) last_flush_unix_ms: AtomicU64,
}

impl SharedWriterStats {
    pub(super) fn snapshot(&self) -> WriterStats {
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
    pub(super) shutdown: CancellationToken,
    pub(super) join: Arc<Mutex<Option<tokio::task::JoinHandle<Result<WriterStats>>>>>,
    pub(super) stats: Arc<SharedWriterStats>,
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

#[cfg(test)]
mod tests;
