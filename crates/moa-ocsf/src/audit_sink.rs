//! Background, batched, non-fatal OCSF audit writer.
//!
//! The edge authentication and authorization-denial paths must never fail or
//! block on an audit write. This module owns a bounded queue drained by a single
//! consumer task that signs and multi-row-inserts `security_events` in batches.
//! When the queue is saturated, events are dropped (with a counter) rather than
//! applying backpressure to request handling. A write failure is logged and the
//! batch is dropped; it never propagates to a request.
//!
//! The writer is instance-owned. [`AuditRuntime`] holds the consumer task's
//! `JoinHandle` and its cancellation token, and hands out [`AuditEmitter`]
//! clones to the components that produce events. There is no process global and
//! no detached task: a runtime that is dropped cancels and aborts its writer,
//! and a runtime that is shut down drains everything already queued before the
//! process exits. The previous `OnceLock` shape could not do either — the first
//! caller to initialize it won for the process lifetime, and nothing could join
//! the task, so a SIGTERM discarded whatever was still queued.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder};
use tokio::runtime::Handle;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::emit::EventColumns;
use crate::signing;

/// Bounded queue depth. Beyond this, events are dropped and counted.
const QUEUE_CAPACITY: usize = 4096;
/// Maximum rows flushed in a single multi-row INSERT.
const BATCH_MAX_ROWS: usize = 256;
/// Maximum time a partial batch waits before flushing.
const BATCH_MAX_AGE: Duration = Duration::from_millis(500);

/// Failure to start the background audit writer.
#[derive(Debug, thiserror::Error)]
pub enum AuditRuntimeError {
    /// No Tokio runtime was available to host the writer task.
    #[error("no tokio runtime available to host the security audit writer")]
    NoRuntime,
}

/// One audit event awaiting a background signed insert.
pub(crate) struct QueuedAudit {
    pub(crate) tenant_id: Uuid,
    pub(crate) value: Value,
    pub(crate) target_resource_uid: Option<String>,
}

/// Handle used to enqueue audit events on an owned background writer.
///
/// Cheap to clone; every clone feeds the same bounded queue. Holding one is the
/// only way to enqueue, which is what makes the writer's ownership visible in
/// every signature that can produce an audit event.
#[derive(Clone)]
pub struct AuditEmitter {
    tx: mpsc::Sender<QueuedAudit>,
    dropped: Arc<AtomicU64>,
}

impl AuditEmitter {
    pub(crate) fn enqueue(&self, item: QueuedAudit) {
        if self.tx.try_send(item).is_err() {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            record_dropped_metric("queue_full", 1);
            tracing::warn!(
                dropped_total = dropped,
                "security audit queue full; dropping event"
            );
        }
    }

    /// Number of audit events dropped by this writer since it started.
    ///
    /// A non-zero value means the audit trail is incomplete because the queue was
    /// saturated or a batch write failed.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for AuditEmitter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuditEmitter")
            .field("dropped", &self.dropped_count())
            .finish()
    }
}

/// Emits the alertable audit-drop counter, labeled by the reason the event was
/// dropped.
fn record_dropped_metric(reason: &'static str, count: u64) {
    metrics::counter!("moa_ocsf_audit_events_dropped_total", "reason" => reason).increment(count);
}

/// Owns one background audit writer task for the lifetime of this value.
///
/// Dropping it cancels and aborts the writer; [`AuditRuntime::shutdown`] instead
/// stops admission, drains what is already queued, and joins the task. Both
/// paths are explicit, and neither leaves a task running that nothing can see.
pub struct AuditRuntime {
    emitter: AuditEmitter,
    shutdown: CancellationToken,
    join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl AuditRuntime {
    /// Starts the background audit writer against `pool`.
    ///
    /// Fallible on purpose. The previous global initializer logged a warning and
    /// left the writer uninstalled when no runtime was present, so every audit
    /// event for the process lifetime became a silent counted drop and nothing
    /// at startup said so.
    pub fn start(pool: PgPool) -> Result<Self, AuditRuntimeError> {
        if Handle::try_current().is_err() {
            return Err(AuditRuntimeError::NoRuntime);
        }
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        let shutdown = CancellationToken::new();
        let worker_shutdown = shutdown.clone();
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = dropped.clone();
        let join =
            tokio::spawn(async move { consume(pool, rx, worker_shutdown, worker_dropped).await });
        Ok(Self {
            emitter: AuditEmitter { tx, dropped },
            shutdown,
            join: Mutex::new(Some(join)),
        })
    }

    /// Returns a clonable handle for enqueuing events on this writer.
    #[must_use]
    pub fn emitter(&self) -> AuditEmitter {
        self.emitter.clone()
    }

    /// Stops admission, drains everything already queued, and joins the writer.
    ///
    /// Idempotent. Returns the number of events this writer dropped, so a
    /// shutdown log can state plainly whether the audit trail is complete.
    pub async fn shutdown(&self) -> u64 {
        self.shutdown.cancel();
        if let Some(join) = self.join.lock().await.take()
            && let Err(error) = join.await
        {
            tracing::warn!(%error, "security audit writer ended abnormally during shutdown");
        }
        self.emitter.dropped_count()
    }
}

impl Drop for AuditRuntime {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Ok(mut slot) = self.join.try_lock()
            && let Some(join) = slot.take()
        {
            join.abort();
        }
    }
}

/// Drain the queue in batches until the channel closes or shutdown completes.
async fn consume(
    pool: PgPool,
    mut rx: mpsc::Receiver<QueuedAudit>,
    shutdown: CancellationToken,
    dropped: Arc<AtomicU64>,
) {
    while let Some(mut batch) = next_batch(&mut rx, BATCH_MAX_ROWS, BATCH_MAX_AGE, &shutdown).await
    {
        flush_batch(&pool, &mut batch, &dropped).await;
    }
}

/// Collect the next batch: block for the first item, then accumulate until the
/// batch is full, the age deadline passes, or the channel closes.
///
/// After `shutdown` is cancelled this stops waiting for new events but keeps
/// returning whatever is ALREADY queued, in batches, until the queue is empty.
/// That is the difference between draining the audit trail on SIGTERM and
/// discarding it: an event that reached the queue was accepted for persistence,
/// and abandoning it at the cancellation point would silently lose records the
/// caller has already stopped being able to observe.
///
/// Returns `None` only when the queue is empty AND either the channel is closed
/// or shutdown has been requested.
async fn next_batch(
    rx: &mut mpsc::Receiver<QueuedAudit>,
    max_rows: usize,
    max_age: Duration,
    shutdown: &CancellationToken,
) -> Option<Vec<QueuedAudit>> {
    let first = if shutdown.is_cancelled() {
        rx.try_recv().ok()?
    } else {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => rx.try_recv().ok()?,
            item = rx.recv() => item?,
        }
    };
    let mut batch = Vec::with_capacity(max_rows);
    batch.push(first);
    let deadline = tokio::time::Instant::now() + max_age;
    while batch.len() < max_rows {
        if shutdown.is_cancelled() {
            // Take only what is already buffered; nothing new is being awaited.
            match rx.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
            continue;
        }
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(item)) => batch.push(item),
            Ok(None) => break, // channel closed: flush what we have
            Err(_) => break,   // age deadline reached
        }
    }
    Some(batch)
}

/// A signed row ready for a multi-row INSERT.
struct SignedRow {
    columns: EventColumns,
    tenant_id: Uuid,
    target_resource_uid: Option<String>,
    event_jcs: Vec<u8>,
    signature_hex: String,
    signing_key_id: Uuid,
}

/// Sign every queued event and insert the batch in one statement. Signing or
/// insert failures are logged and counted; they never propagate.
async fn flush_batch(pool: &PgPool, batch: &mut Vec<QueuedAudit>, dropped_counter: &AtomicU64) {
    if batch.is_empty() {
        return;
    }
    let mut rows = Vec::with_capacity(batch.len());
    for item in batch.drain(..) {
        match signing::sign_cached(pool, item.tenant_id, &item.value).await {
            Ok((signing_key_id, signature_hex, event_jcs)) => rows.push(SignedRow {
                columns: EventColumns::from_value(&item.value),
                tenant_id: item.tenant_id,
                target_resource_uid: item.target_resource_uid,
                event_jcs,
                signature_hex,
                signing_key_id,
            }),
            Err(error) => {
                let dropped = dropped_counter.fetch_add(1, Ordering::Relaxed) + 1;
                record_dropped_metric("signing_failed", 1);
                tracing::warn!(error = %error, dropped_total = dropped, "security audit signing failed; dropping event");
            }
        }
    }
    if rows.is_empty() {
        return;
    }
    if let Err(error) = insert_rows(pool, &rows).await {
        let count = rows.len() as u64;
        let dropped = dropped_counter.fetch_add(count, Ordering::Relaxed) + count;
        record_dropped_metric("insert_failed", count);
        tracing::warn!(error = %error, dropped_total = dropped, batch = rows.len(), "security audit batch insert failed; dropping events");
    }
}

/// Insert a batch of signed rows in a single multi-row statement.
async fn insert_rows(pool: &PgPool, rows: &[SignedRow]) -> Result<(), sqlx::Error> {
    let mut builder: QueryBuilder<Postgres> = QueryBuilder::new(
        "INSERT INTO security_events \
         (id, tenant_id, class_uid, activity_id, category_uid, severity_id, \
          type_uid, actor_user_uid, actor_session_uid, target_resource_uid, \
          event_jcs, signature_hex, signing_key_id, occurred_at) ",
    );
    builder.push_values(rows, |mut row, signed| {
        row.push_bind(signed.columns.id)
            .push_bind(signed.tenant_id)
            .push_bind(signed.columns.class_uid)
            .push_bind(signed.columns.activity_id)
            .push_bind(signed.columns.category_uid)
            .push_bind(signed.columns.severity_id)
            .push_bind(signed.columns.type_uid)
            .push_bind(signed.columns.actor_user_uid.as_deref())
            .push_bind(signed.columns.actor_session_uid.as_deref())
            .push_bind(signed.target_resource_uid.as_deref())
            .push_bind(signed.event_jcs.as_slice())
            .push_bind(signed.signature_hex.as_str())
            .push_bind(signed.signing_key_id)
            .push_bind(signed.columns.occurred_at);
    });
    builder.build().execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(tenant: u128) -> QueuedAudit {
        QueuedAudit {
            tenant_id: Uuid::from_u128(tenant),
            value: serde_json::json!({ "class_uid": 3002, "activity_id": 1 }),
            target_resource_uid: None,
        }
    }

    fn test_emitter(capacity: usize) -> (AuditEmitter, mpsc::Receiver<QueuedAudit>) {
        let (tx, rx) = mpsc::channel(capacity);
        (
            AuditEmitter {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn next_batch_flushes_on_size_before_age() {
        // Pins: a batch that reaches max_rows returns immediately without waiting
        // for the age deadline.
        let (emitter, mut rx) = test_emitter(16);
        for tenant in 0..5 {
            emitter.enqueue(queued(tenant));
        }
        let batch = next_batch(
            &mut rx,
            3,
            Duration::from_secs(30),
            &CancellationToken::new(),
        )
        .await
        .expect("size-triggered batch");
        assert_eq!(batch.len(), 3, "size flush caps the batch at max_rows");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn next_batch_flushes_on_age_when_under_size() {
        // Pins: a partial batch flushes once the age deadline elapses even though
        // it never reaches max_rows.
        let (emitter, mut rx) = test_emitter(16);
        emitter.enqueue(queued(1));
        let batch = next_batch(
            &mut rx,
            64,
            Duration::from_millis(200),
            &CancellationToken::new(),
        )
        .await
        .expect("age-triggered batch");
        assert_eq!(batch.len(), 1, "age flush returns the partial batch");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn next_batch_returns_none_when_channel_closed_and_empty() {
        // Pins: the consumer loop terminates when every emitter is dropped.
        let (emitter, mut rx) = test_emitter(4);
        drop(emitter);
        assert!(
            next_batch(
                &mut rx,
                8,
                Duration::from_millis(50),
                &CancellationToken::new()
            )
            .await
            .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_cancelled_writer_still_returns_events_that_were_already_queued() {
        // Pins the SIGTERM drain. Events accepted before cancellation must still
        // be handed to the flusher; abandoning them at the cancellation point
        // would discard audit records that the request path was already told it
        // no longer needed to worry about. This is the difference between a
        // drain and a drop, and only the queued-before-cancel case shows it.
        let (emitter, mut rx) = test_emitter(16);
        for tenant in 0..3 {
            emitter.enqueue(queued(tenant));
        }
        let shutdown = CancellationToken::new();
        shutdown.cancel();

        let batch = next_batch(&mut rx, 64, Duration::from_millis(1), &shutdown)
            .await
            .expect("a cancelled writer must still surface already-queued events");
        assert_eq!(
            batch.len(),
            3,
            "every event queued before cancellation must be drained, got {}",
            batch.len()
        );
        assert!(
            next_batch(&mut rx, 64, Duration::from_millis(1), &shutdown)
                .await
                .is_none(),
            "once drained, a cancelled writer must stop rather than wait for more"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_drops_and_counts_when_queue_full() {
        // Pins: a saturated bounded queue drops overflow and increments the drop
        // counter instead of blocking the caller.
        let (emitter, _rx) = test_emitter(1);

        emitter.enqueue(queued(1)); // fills the single buffer slot
        emitter.enqueue(queued(2)); // dropped
        emitter.enqueue(queued(3)); // dropped

        assert_eq!(
            emitter.dropped_count(),
            2,
            "two events beyond capacity are dropped and counted"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_reports_the_writers_own_drop_count() {
        // Pins: the drop counter belongs to the instance, not the process. A
        // shared static made two runtimes in one process report each other's
        // losses, so no shutdown log could say whether ITS audit trail was
        // complete.
        let (first, _first_rx) = test_emitter(1);
        let (second, _second_rx) = test_emitter(1);

        first.enqueue(queued(1));
        first.enqueue(queued(2)); // dropped on the first emitter only

        assert_eq!(first.dropped_count(), 1);
        assert_eq!(
            second.dropped_count(),
            0,
            "a second writer in the same process must not inherit the first's drops"
        );
    }
}
