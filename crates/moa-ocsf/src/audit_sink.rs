//! Background, batched, non-fatal OCSF audit writer.
//!
//! The edge authentication and authorization-denial paths must never fail or
//! block on an audit write. This module owns a bounded queue drained by a single
//! consumer task that signs and multi-row-inserts `security_events` in batches.
//! When the queue is saturated, events are dropped (with a counter) rather than
//! applying backpressure to request handling. A write failure is logged and the
//! batch is dropped; it never propagates to a request.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::emit::EventColumns;
use crate::signing;

/// Bounded queue depth. Beyond this, events are dropped and counted.
const QUEUE_CAPACITY: usize = 4096;
/// Maximum rows flushed in a single multi-row INSERT.
const BATCH_MAX_ROWS: usize = 256;
/// Maximum time a partial batch waits before flushing.
const BATCH_MAX_AGE: Duration = Duration::from_millis(500);

static SINK: OnceLock<AuditSink> = OnceLock::new();

/// One audit event awaiting a background signed insert.
struct QueuedAudit {
    tenant_id: Uuid,
    value: Value,
    target_resource_uid: Option<String>,
}

/// Handle to the background audit writer: a bounded sender plus a drop counter.
struct AuditSink {
    tx: mpsc::Sender<QueuedAudit>,
    dropped: &'static AtomicU64,
}

impl AuditSink {
    fn enqueue(&self, item: QueuedAudit) {
        if self.tx.try_send(item).is_err() {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            record_dropped_metric("queue_full", 1);
            tracing::warn!(
                dropped_total = dropped,
                "security audit queue full; dropping event"
            );
        }
    }
}

/// Emits the alertable audit-drop counter alongside the in-process atomic.
///
/// The `DROPPED` atomic still backs the synchronous [`dropped_audit_count`]
/// accessor; this counter makes the same loss visible to metrics/alerting,
/// labeled by the reason the event was dropped.
fn record_dropped_metric(reason: &'static str, count: u64) {
    metrics::counter!("moa_ocsf_audit_events_dropped_total", "reason" => reason).increment(count);
}

/// Total number of audit events dropped since process start.
///
/// A non-zero value means the audit trail is incomplete because the queue was
/// saturated or a batch write failed.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Number of audit events dropped since process start (queue-full + write failures).
#[must_use]
pub fn dropped_audit_count() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Initialize the background audit writer against `pool`.
///
/// Idempotent: the first call wins. If no Tokio runtime is available the writer
/// is left uninitialized and enqueues become counted drops, so callers on
/// non-async paths never panic.
pub fn init_background_audit(pool: PgPool) {
    if Handle::try_current().is_err() {
        tracing::warn!("no tokio runtime available; background security audit disabled");
        return;
    }
    let _ = SINK.get_or_init(|| {
        let (tx, rx) = mpsc::channel(QUEUE_CAPACITY);
        tokio::spawn(consume(pool, rx));
        AuditSink {
            tx,
            dropped: &DROPPED,
        }
    });
}

/// Enqueue a pre-serialized event for background persistence.
pub(crate) fn enqueue(tenant_id: Uuid, value: Value, target_resource_uid: Option<String>) {
    let Some(sink) = SINK.get() else {
        // Uninitialized (e.g. no runtime configured): drop rather than fail.
        DROPPED.fetch_add(1, Ordering::Relaxed);
        record_dropped_metric("uninitialized", 1);
        return;
    };
    sink.enqueue(QueuedAudit {
        tenant_id,
        value,
        target_resource_uid,
    });
}

/// Drain the queue in batches until the channel closes.
async fn consume(pool: PgPool, mut rx: mpsc::Receiver<QueuedAudit>) {
    while let Some(mut batch) = next_batch(&mut rx, BATCH_MAX_ROWS, BATCH_MAX_AGE).await {
        flush_batch(&pool, &mut batch).await;
    }
}

/// Collect the next batch: block for the first item, then accumulate until the
/// batch is full, the age deadline passes, or the channel closes. Returns `None`
/// only when the channel is closed and empty.
async fn next_batch(
    rx: &mut mpsc::Receiver<QueuedAudit>,
    max_rows: usize,
    max_age: Duration,
) -> Option<Vec<QueuedAudit>> {
    let first = rx.recv().await?;
    let mut batch = Vec::with_capacity(max_rows);
    batch.push(first);
    let deadline = tokio::time::Instant::now() + max_age;
    while batch.len() < max_rows {
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
async fn flush_batch(pool: &PgPool, batch: &mut Vec<QueuedAudit>) {
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
                let dropped = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
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
        let dropped = DROPPED.fetch_add(count, Ordering::Relaxed) + count;
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

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn next_batch_flushes_on_size_before_age() {
        // Pins: a batch that reaches max_rows returns immediately without waiting
        // for the age deadline.
        let (tx, mut rx) = mpsc::channel(16);
        for tenant in 0..5 {
            tx.try_send(queued(tenant)).expect("send queued audit");
        }
        let batch = next_batch(&mut rx, 3, Duration::from_secs(30))
            .await
            .expect("size-triggered batch");
        assert_eq!(batch.len(), 3, "size flush caps the batch at max_rows");
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn next_batch_flushes_on_age_when_under_size() {
        // Pins: a partial batch flushes once the age deadline elapses even though
        // it never reaches max_rows.
        let (tx, mut rx) = mpsc::channel(16);
        tx.try_send(queued(1)).expect("send queued audit");
        let batch = next_batch(&mut rx, 64, Duration::from_millis(200))
            .await
            .expect("age-triggered batch");
        assert_eq!(batch.len(), 1, "age flush returns the partial batch");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn next_batch_returns_none_when_channel_closed_and_empty() {
        // Pins: the consumer loop terminates when the sender is dropped.
        let (tx, mut rx) = mpsc::channel::<QueuedAudit>(4);
        drop(tx);
        assert!(
            next_batch(&mut rx, 8, Duration::from_millis(50))
                .await
                .is_none()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enqueue_drops_and_counts_when_queue_full() {
        // Pins: a saturated bounded queue drops overflow and increments the drop
        // counter instead of blocking the caller.
        let counter: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
        let (tx, _rx) = mpsc::channel(1);
        let sink = AuditSink {
            tx,
            dropped: counter,
        };

        sink.enqueue(queued(1)); // fills the single buffer slot
        sink.enqueue(queued(2)); // dropped
        sink.enqueue(queued(3)); // dropped

        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "two events beyond capacity are dropped and counted"
        );
    }
}
