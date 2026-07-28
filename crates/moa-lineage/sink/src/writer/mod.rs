//! Async lineage writer worker and the handle that owns it.

mod acceptance;
mod compliance;
mod retry;
mod rows;
mod storage;
mod supervisor;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::Result;

pub(crate) use acceptance::{JournalBacklog, LineageJournal};
pub(crate) use rows::{LineageRow, PendingRow};
pub use supervisor::spawn_writer;
pub(crate) use supervisor::spawn_writer_for_sink;

/// Sentinel for "queue empty", so the age gauge needs no lock.
const NO_PENDING_AGE: u64 = u64::MAX;

/// Lifecycle state of the lineage writer task.
///
/// Exposed so a readiness probe can distinguish "this replica is not accepting
/// new work" from "this process is unhealthy". Only the first two states are
/// reachable while the task is still doing work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterState {
    /// The task is claiming and storing normally.
    Running,
    /// Shutdown was requested; the task is finishing committed work.
    Draining,
    /// The task ended abnormally. Its work is still durable in the queue, but
    /// this replica will not make progress on it.
    Failed,
    /// The task ended cleanly and will do no further work.
    Stopped,
}

impl WriterState {
    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Draining,
            2 => Self::Failed,
            3 => Self::Stopped,
            _ => Self::Running,
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Draining => 1,
            Self::Failed => 2,
            Self::Stopped => 3,
        }
    }

    /// Every state, so a transition can zero the ones it left.
    const ALL: [Self; 4] = [Self::Running, Self::Draining, Self::Failed, Self::Stopped];

    /// Returns the stable label used in logs and metrics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Draining => "draining",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        }
    }
}

/// Writer runtime statistics returned by a completed drain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriterStats {
    /// Rows written successfully by this writer instance.
    pub written: u64,
    /// Rows still accepted-but-unstored in the queue when the drain ended.
    ///
    /// A non-zero value at shutdown is not data loss: those rows are committed
    /// in Postgres and another replica will claim them.
    pub pending: u64,
    /// Unix timestamp in milliseconds for the latest successful store.
    pub last_flush_unix_ms: Option<u64>,
}

/// Everything a readiness probe needs from the writer.
#[derive(Debug, Clone, PartialEq)]
pub struct WriterHealth {
    /// Current lifecycle state.
    pub state: WriterState,
    /// Why the task ended abnormally, when it did.
    pub fatal_error: Option<String>,
    /// Rows written successfully by this writer instance.
    pub written: u64,
    /// Accepted-but-unstored rows in the queue, across all replicas.
    pub pending: u64,
    /// Age in seconds of the oldest accepted-but-unstored row.
    pub oldest_pending_age_seconds: Option<f64>,
    /// Unix timestamp in milliseconds of the last successful claim poll.
    pub last_claim_unix_ms: Option<u64>,
    /// Unix timestamp in milliseconds of the last successful store.
    pub last_flush_unix_ms: Option<u64>,
    /// Whether the last interaction with the ACCEPTANCE QUEUE reached Postgres.
    ///
    /// Scoped to the queue on purpose. A failed row-store write is not an
    /// unreachable queue, and reporting it as one sends an operator to the wrong
    /// system; a store failure that matters escalates through the backlog age.
    pub queue_reachable: bool,
}

#[derive(Default)]
pub(super) struct SharedWriterState {
    state: AtomicU8,
    written: AtomicU64,
    pending: AtomicU64,
    oldest_pending_age_ms: AtomicU64,
    last_claim_unix_ms: AtomicU64,
    last_flush_unix_ms: AtomicU64,
    queue_reachable: AtomicBool,
    fatal_error: std::sync::Mutex<Option<String>>,
}

impl SharedWriterState {
    fn new() -> Self {
        Self {
            oldest_pending_age_ms: AtomicU64::new(NO_PENDING_AGE),
            queue_reachable: AtomicBool::new(true),
            ..Self::default()
        }
    }

    pub(super) fn set_state(&self, state: WriterState) {
        self.state.store(state.code(), Ordering::Relaxed);
        // Zero the states this transition left. Setting only the new series to
        // 1.0 leaves the previous one at 1.0 forever, so after a single
        // Running -> Failed transition BOTH read 1 and any
        // `state="failed"` alert latches until the process restarts - which is
        // exactly the alert an operator most needs to be able to trust when it
        // clears. The label set is fixed and small, so emitting all four costs
        // nothing and leaves no series behind.
        for other in WriterState::ALL {
            metrics::gauge!("moa_lineage_writer_state", "state" => other.as_str())
                .set(if other == state { 1.0 } else { 0.0 });
        }
    }

    pub(super) fn state(&self) -> WriterState {
        WriterState::from_code(self.state.load(Ordering::Relaxed))
    }

    pub(super) fn set_fatal(&self, error: String) {
        if let Ok(mut slot) = self.fatal_error.lock() {
            *slot = Some(error);
        }
        self.set_state(WriterState::Failed);
    }

    pub(super) fn record_written(&self, rows: u64) {
        self.written.fetch_add(rows, Ordering::Relaxed);
        self.last_flush_unix_ms.store(
            chrono::Utc::now().timestamp_millis().max(1) as u64,
            Ordering::Relaxed,
        );
        metrics::counter!("moa_lineage_written_total").increment(rows);
        metrics::counter!("moa_lineage_flushed_total").increment(rows);
    }

    pub(super) fn record_claim_poll(&self) {
        self.last_claim_unix_ms.store(
            chrono::Utc::now().timestamp_millis().max(1) as u64,
            Ordering::Relaxed,
        );
    }

    pub(super) fn record_backlog(&self, backlog: JournalBacklog) {
        self.pending.store(backlog.pending, Ordering::Relaxed);
        self.oldest_pending_age_ms.store(
            backlog
                .oldest_pending_age_seconds
                .map_or(NO_PENDING_AGE, |age| (age * 1000.0).max(0.0) as u64),
            Ordering::Relaxed,
        );
        metrics::gauge!("moa_lineage_journal_depth").set(backlog.pending as f64);
        metrics::gauge!("moa_lineage_journal_oldest_age_seconds")
            .set(backlog.oldest_pending_age_seconds.unwrap_or(0.0));
    }

    pub(super) fn set_queue_reachable(&self, reachable: bool) {
        self.queue_reachable.store(reachable, Ordering::Relaxed);
    }

    fn health(&self) -> WriterHealth {
        let oldest = self.oldest_pending_age_ms.load(Ordering::Relaxed);
        WriterHealth {
            state: self.state(),
            fatal_error: self
                .fatal_error
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().cloned()),
            written: self.written.load(Ordering::Relaxed),
            pending: self.pending.load(Ordering::Relaxed),
            oldest_pending_age_seconds: (oldest != NO_PENDING_AGE)
                .then(|| (oldest as f64) / 1000.0),
            last_claim_unix_ms: nonzero(self.last_claim_unix_ms.load(Ordering::Relaxed)),
            last_flush_unix_ms: nonzero(self.last_flush_unix_ms.load(Ordering::Relaxed)),
            queue_reachable: self.queue_reachable.load(Ordering::Relaxed),
        }
    }

    fn stats(&self) -> WriterStats {
        WriterStats {
            written: self.written.load(Ordering::Relaxed),
            pending: self.pending.load(Ordering::Relaxed),
            last_flush_unix_ms: nonzero(self.last_flush_unix_ms.load(Ordering::Relaxed)),
        }
    }
}

fn nonzero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

/// Owns the lineage writer task for the lifetime of this handle.
///
/// The `JoinHandle` lives here and nowhere else, and [`Drop`] aborts the task if
/// it is still running. There is therefore no way to end up with a detached
/// writer that outlives the runtime that built it — which is exactly the shape
/// that let accepted records disappear during a rollout.
pub struct WriterHandle {
    shutdown: CancellationToken,
    join: Mutex<Option<tokio::task::JoinHandle<Result<WriterStats>>>>,
    shared: Arc<SharedWriterState>,
    max_pending_age: Duration,
}

impl WriterHandle {
    /// Requests graceful shutdown, drains committed work, and returns final stats.
    ///
    /// Idempotent: a second call returns the last known stats rather than
    /// failing, so a shutdown path that runs twice is harmless.
    pub async fn shutdown(&self) -> Result<WriterStats> {
        self.shutdown.cancel();
        let Some(join) = self.join.lock().await.take() else {
            return Ok(self.shared.stats());
        };
        match join.await {
            Ok(result) => result,
            Err(error) => {
                self.shared
                    .set_fatal(format!("lineage writer task ended abnormally: {error}"));
                Err(error.into())
            }
        }
    }

    /// Returns the latest writer statistics snapshot.
    #[must_use]
    pub fn stats(&self) -> WriterStats {
        self.shared.stats()
    }

    /// Returns everything a probe needs to judge this writer.
    #[must_use]
    pub fn health(&self) -> WriterHealth {
        self.shared.health()
    }

    /// Returns why this writer is not ready to serve, or `None` when it is.
    ///
    /// Readiness, not liveness. Every condition here means "stop sending this
    /// replica work"; none of them means "restart this process". Restarting a
    /// replica whose backlog is over age would drop its leases and slow the very
    /// drain that is already behind.
    #[must_use]
    pub fn unready_reason(&self) -> Option<String> {
        let health = self.shared.health();
        match health.state {
            WriterState::Failed => {
                return Some(format!(
                    "lineage writer failed: {}",
                    health
                        .fatal_error
                        .unwrap_or_else(|| "no error recorded".to_string())
                ));
            }
            WriterState::Stopped => {
                return Some("lineage writer stopped".to_string());
            }
            WriterState::Draining => {
                return Some("lineage writer draining".to_string());
            }
            WriterState::Running => {}
        }
        if !health.queue_reachable {
            return Some("lineage acceptance queue is unreachable".to_string());
        }
        let max_age = self.max_pending_age.as_secs_f64();
        match health.oldest_pending_age_seconds {
            Some(age) if age > max_age => Some(format!(
                "oldest accepted lineage row is {age:.1}s old, over the {max_age:.1}s limit"
            )),
            _ => None,
        }
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Ok(mut slot) = self.join.try_lock()
            && let Some(join) = slot.take()
        {
            join.abort();
        }
    }
}

#[cfg(test)]
mod tests;
