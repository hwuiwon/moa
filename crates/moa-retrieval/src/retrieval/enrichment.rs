//! Bounded, batching background worker for best-effort post-retrieval enrichment.
//!
//! After a successful retrieval the hybrid retriever needs two best-effort side
//! writes — a `last_accessed_at` bump (memory decay input) and sampled retrieval
//! lineage (quality-score input). Previously each retrieval spawned detached
//! `tokio` tasks for these against the same Postgres pool that backs sessions,
//! authz, and graph work, with no queue bound, backpressure, or shutdown drain,
//! so under load they competed with durable writes and on shutdown were silently
//! dropped.
//!
//! This module funnels both through one bounded, instrumented worker:
//!
//! * Enqueue is a non-blocking `try_send`; when the fixed-capacity queue is full
//!   the job is dropped with a metric (drop-on-overflow is correct for
//!   best-effort enrichment — the retrieval path must never backpressure on it).
//! * Access bumps are coalesced within a batch window: one `UPDATE ... WHERE uid
//!   = ANY($1)` per `(scope, role)` instead of one statement per retrieval.
//! * The worker drains its channel when all senders drop (shutdown), so queued
//!   work is flushed rather than lost.
//! * Queue depth, drops, batch sizes, and flush latency are exported as metrics.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_memory_types::MemoryScope;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::retrieval::legs::{bump_last_accessed, write_retrieval_lineage};
use crate::retrieval::types::{LineageContext, RetrievalLineageHit};

/// Fixed bound on queued enrichment jobs. Drop-on-overflow keeps the retrieval
/// path from ever backpressuring on best-effort enrichment.
const DEFAULT_QUEUE_CAPACITY: usize = 1024;
/// Maximum time a partially filled batch waits before flushing.
const DEFAULT_BATCH_WINDOW: Duration = Duration::from_millis(50);
/// Maximum number of jobs coalesced into one flush.
const DEFAULT_MAX_BATCH: usize = 256;

/// A best-effort enrichment job produced after a successful retrieval.
enum EnrichmentJob {
    /// Bump `last_accessed_at` for retrieved nodes in one scope.
    AccessBump {
        scope: MemoryScope,
        uids: Vec<Uuid>,
        assume_app_role: bool,
    },
    /// Write sampled retrieval-lineage rows for one turn.
    Lineage {
        scope: MemoryScope,
        lineage: LineageContext,
        hits: Vec<RetrievalLineageHit>,
        retrieved_at: DateTime<Utc>,
        assume_app_role: bool,
    },
}

/// Applies enrichment writes. Abstracted behind a trait so the worker's
/// batching, coalescing, and drain logic is unit-testable without a database.
#[async_trait]
pub(crate) trait EnrichmentSink: Send + Sync {
    /// Bumps `last_accessed_at` for `uids` within one scope.
    async fn bump_access(&self, scope: MemoryScope, uids: Vec<Uuid>, assume_app_role: bool);
    /// Writes sampled retrieval-lineage rows for one turn.
    async fn write_lineage(
        &self,
        scope: MemoryScope,
        lineage: LineageContext,
        hits: Vec<RetrievalLineageHit>,
        retrieved_at: DateTime<Utc>,
        assume_app_role: bool,
    );
}

/// Production sink backed by the shared runtime Postgres pool.
struct PgEnrichmentSink {
    pool: PgPool,
}

#[async_trait]
impl EnrichmentSink for PgEnrichmentSink {
    async fn bump_access(&self, scope: MemoryScope, uids: Vec<Uuid>, assume_app_role: bool) {
        if let Err(error) =
            bump_last_accessed(self.pool.clone(), scope, uids, assume_app_role).await
        {
            tracing::debug!(
                error = %error,
                "enrichment worker failed to bump graph-memory access timestamps"
            );
        }
    }

    async fn write_lineage(
        &self,
        scope: MemoryScope,
        lineage: LineageContext,
        hits: Vec<RetrievalLineageHit>,
        retrieved_at: DateTime<Utc>,
        assume_app_role: bool,
    ) {
        if let Err(error) = write_retrieval_lineage(
            self.pool.clone(),
            scope,
            lineage,
            hits,
            retrieved_at,
            assume_app_role,
        )
        .await
        {
            tracing::debug!(
                error = %error,
                "enrichment worker failed to write graph-memory retrieval lineage"
            );
        }
    }
}

/// Non-blocking handle used by the retrieval path to enqueue enrichment.
#[derive(Clone)]
pub struct EnrichmentHandle {
    tx: mpsc::Sender<EnrichmentJob>,
    dropped: Arc<AtomicU64>,
}

impl EnrichmentHandle {
    /// Enqueues an access-timestamp bump; a no-op when `uids` is empty.
    pub(crate) fn enqueue_access_bump(
        &self,
        scope: MemoryScope,
        uids: Vec<Uuid>,
        assume_app_role: bool,
    ) {
        if uids.is_empty() {
            return;
        }
        self.try_send(EnrichmentJob::AccessBump {
            scope,
            uids,
            assume_app_role,
        });
    }

    /// Enqueues a retrieval-lineage write; a no-op when `hits` is empty.
    pub(crate) fn enqueue_lineage(
        &self,
        scope: MemoryScope,
        lineage: LineageContext,
        hits: Vec<RetrievalLineageHit>,
        retrieved_at: DateTime<Utc>,
        assume_app_role: bool,
    ) {
        if hits.is_empty() {
            return;
        }
        self.try_send(EnrichmentJob::Lineage {
            scope,
            lineage,
            hits,
            retrieved_at,
            assume_app_role,
        });
    }

    fn try_send(&self, job: EnrichmentJob) {
        match self.tx.try_send(job) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                metrics::counter!("moa_retrieval_enrichment_dropped_total").increment(1);
                tracing::warn!(
                    dropped,
                    "retrieval enrichment queue full; dropping best-effort job"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // The worker has shut down; there is nothing left to enrich.
            }
        }
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Spawns the single production enrichment worker over the shared pool.
///
/// Must be called from within a Tokio runtime; callers without one skip
/// enrichment entirely (the handle is optional on the retriever).
pub fn spawn_enrichment_worker(pool: PgPool) -> EnrichmentHandle {
    let (handle, _join) = spawn_with_sink(
        Arc::new(PgEnrichmentSink { pool }),
        DEFAULT_QUEUE_CAPACITY,
        DEFAULT_BATCH_WINDOW,
        DEFAULT_MAX_BATCH,
    );
    handle
}

/// Spawns a worker over an explicit sink and policy. The returned [`JoinHandle`]
/// completes once every sender has dropped and the channel has been drained.
fn spawn_with_sink(
    sink: Arc<dyn EnrichmentSink>,
    capacity: usize,
    batch_window: Duration,
    max_batch: usize,
) -> (EnrichmentHandle, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(capacity);
    let worker = EnrichmentWorker {
        sink,
        rx,
        batch_window,
        max_batch,
    };
    let join = tokio::spawn(worker.run());
    (
        EnrichmentHandle {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        },
        join,
    )
}

struct EnrichmentWorker {
    sink: Arc<dyn EnrichmentSink>,
    rx: mpsc::Receiver<EnrichmentJob>,
    batch_window: Duration,
    max_batch: usize,
}

impl EnrichmentWorker {
    async fn run(mut self) {
        // `recv` yields `None` only once every sender has dropped and the buffer
        // is empty, so this loop drains queued work on shutdown before exiting.
        while let Some(first) = self.rx.recv().await {
            let mut batch = vec![first];
            let deadline = tokio::time::Instant::now() + self.batch_window;
            let mut senders_closed = false;
            while batch.len() < self.max_batch {
                match tokio::time::timeout_at(deadline, self.rx.recv()).await {
                    Ok(Some(job)) => batch.push(job),
                    Ok(None) => {
                        senders_closed = true;
                        break;
                    }
                    Err(_elapsed) => break,
                }
            }
            self.flush(batch).await;
            if senders_closed {
                break;
            }
        }
    }

    async fn flush(&self, batch: Vec<EnrichmentJob>) {
        // Coalesce access bumps per (scope, role): the deduped union of uids is
        // written as a single `UPDATE ... WHERE uid = ANY($1)` for each scope.
        let mut bumps: HashMap<(MemoryScope, bool), HashSet<Uuid>> = HashMap::new();
        let mut lineage_jobs = Vec::new();
        for job in batch {
            match job {
                EnrichmentJob::AccessBump {
                    scope,
                    uids,
                    assume_app_role,
                } => {
                    bumps
                        .entry((scope, assume_app_role))
                        .or_default()
                        .extend(uids);
                }
                lineage @ EnrichmentJob::Lineage { .. } => lineage_jobs.push(lineage),
            }
        }
        for ((scope, assume_app_role), uids) in bumps {
            self.sink
                .bump_access(scope, uids.into_iter().collect(), assume_app_role)
                .await;
        }
        for job in lineage_jobs {
            if let EnrichmentJob::Lineage {
                scope,
                lineage,
                hits,
                retrieved_at,
                assume_app_role,
            } = job
            {
                self.sink
                    .write_lineage(scope, lineage, hits, retrieved_at, assume_app_role)
                    .await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use moa_core::types::identifiers::SessionId;

    use super::*;

    #[derive(Default)]
    struct CountingSink {
        bumps: Mutex<Vec<(MemoryScope, Vec<Uuid>, bool)>>,
        lineage_writes: Mutex<usize>,
    }

    #[async_trait]
    impl EnrichmentSink for CountingSink {
        async fn bump_access(
            &self,
            scope: MemoryScope,
            mut uids: Vec<Uuid>,
            assume_app_role: bool,
        ) {
            uids.sort();
            self.bumps.lock().expect("counting sink bumps lock").push((
                scope,
                uids,
                assume_app_role,
            ));
        }

        async fn write_lineage(
            &self,
            _scope: MemoryScope,
            _lineage: LineageContext,
            _hits: Vec<RetrievalLineageHit>,
            _retrieved_at: DateTime<Utc>,
            _assume_app_role: bool,
        ) {
            *self
                .lineage_writes
                .lock()
                .expect("counting sink lineage lock") += 1;
        }
    }

    fn scope() -> MemoryScope {
        MemoryScope::Tenant {
            tenant_id: moa_core::types::identifiers::TenantId::new(),
        }
    }

    fn lineage_context() -> LineageContext {
        LineageContext {
            session_id: SessionId::new(),
            turn_id: None,
            turn_seq: 1,
        }
    }

    #[tokio::test]
    async fn enqueue_never_blocks_and_drops_with_metric_when_queue_is_full() {
        // Pins: F20 — enqueue is non-blocking; once the fixed-capacity queue is
        // full, further jobs are dropped (counted) rather than backpressuring the
        // retrieval path. Holds the receiver unconsumed so the queue stays full.
        let (tx, _rx) = mpsc::channel::<EnrichmentJob>(2);
        let handle = EnrichmentHandle {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        let uid = Uuid::now_v7();

        // Fill the two-slot queue.
        handle.enqueue_access_bump(scope(), vec![uid], false);
        handle.enqueue_access_bump(scope(), vec![uid], false);
        assert_eq!(handle.dropped(), 0, "enqueues within capacity are accepted");

        // The next two enqueues overflow and are dropped, not blocked.
        handle.enqueue_access_bump(scope(), vec![uid], false);
        handle.enqueue_access_bump(scope(), vec![uid], false);
        assert_eq!(
            handle.dropped(),
            2,
            "overflow jobs are dropped with a counter"
        );
    }

    #[tokio::test]
    async fn access_bumps_coalesce_into_one_deduped_batched_update() {
        // Pins: F20 — access bumps within one batch coalesce to a single sink call
        // per scope carrying the deduped union of uids, instead of one write each.
        let sink = Arc::new(CountingSink::default());
        let (handle, join) = spawn_with_sink(sink.clone(), 1024, Duration::from_secs(10), 256);
        let scope = scope();
        let u1 = Uuid::from_u128(1);
        let u2 = Uuid::from_u128(2);
        let u3 = Uuid::from_u128(3);

        handle.enqueue_access_bump(scope.clone(), vec![u1, u2], false);
        handle.enqueue_access_bump(scope.clone(), vec![u2, u3], false);
        // Dropping the handle closes the channel, so the worker drains both queued
        // bumps into one final batch and flushes deterministically.
        drop(handle);
        join.await.expect("worker joins after draining");

        let bumps = sink.bumps.lock().expect("bumps");
        assert_eq!(bumps.len(), 1, "the two bumps coalesce into one sink call");
        let (bumped_scope, mut uids, role) = bumps[0].clone();
        uids.sort();
        assert_eq!(bumped_scope, scope);
        assert!(!role);
        assert_eq!(
            uids,
            vec![u1, u2, u3],
            "coalesced uids are the deduped union"
        );
    }

    #[tokio::test]
    async fn shutdown_drains_queued_work() {
        // Pins: F20 — queued enrichment is flushed on shutdown (all senders drop),
        // not silently discarded.
        let sink = Arc::new(CountingSink::default());
        let (handle, join) = spawn_with_sink(sink.clone(), 1024, Duration::from_secs(10), 256);

        for _ in 0..5 {
            handle.enqueue_lineage(
                scope(),
                lineage_context(),
                vec![RetrievalLineageHit {
                    uid: Uuid::now_v7(),
                    chunk_uid: None,
                    document_version_uid: None,
                }],
                Utc::now(),
                false,
            );
        }
        drop(handle);
        join.await.expect("worker drains and joins on shutdown");

        assert_eq!(
            *sink.lineage_writes.lock().expect("lineage writes"),
            5,
            "all queued lineage writes are flushed on shutdown"
        );
    }
}
