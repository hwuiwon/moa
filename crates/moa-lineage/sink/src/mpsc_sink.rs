//! Bounded hot-path ingress plus the durable acceptance boundary.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moa_core::{error::MoaError, traits::LineageHandle};
use moa_lineage_core::{LineageEvent, LineageSink};
use tokio::sync::{Notify, mpsc};

use crate::store::LineageStore;
use crate::writer::{LineageJournal, PendingRow, WriterHandle, spawn_writer_for_sink};
use crate::{Result, WriterStats};

/// Upper bound on one durable acceptance commit before it is treated as failed.
///
/// Guards the hot turn path against a stalled Postgres. The commit is a single
/// multi-row insert into an unindexed-by-payload queue table, so this bound is
/// generous; exceeding it means the database is not serving writes, which the
/// caller must learn about rather than block on.
const DURABLE_ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for the production mpsc lineage sink.
#[derive(Clone, Debug)]
pub struct MpscSinkConfig {
    /// Best-effort ingress channel depth. 8192 is the recommended default.
    pub channel_capacity: usize,
    /// Maximum ingress events accepted into the queue per commit.
    pub batch_size: usize,
    /// Maximum age for a partial ingress batch, and the writer's poll cadence.
    pub batch_max_age: Duration,
    /// Maximum queue rows claimed per drain.
    pub claim_batch_size: usize,
    /// How long a claim lease is held before it expires and the rows return.
    pub lease_ttl: Duration,
    /// Oldest claimable backlog age tolerated before readiness fails.
    pub max_pending_age: Duration,
    /// Upper bound on the shutdown drain.
    pub drain_timeout: Duration,
}

impl Default for MpscSinkConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 8192,
            batch_size: 512,
            batch_max_age: Duration::from_secs(2),
            claim_batch_size: 512,
            lease_ttl: Duration::from_secs(60),
            max_pending_age: Duration::from_secs(300),
            drain_timeout: Duration::from_secs(30),
        }
    }
}

impl From<&moa_config::LineageConfig> for MpscSinkConfig {
    fn from(config: &moa_config::LineageConfig) -> Self {
        Self {
            channel_capacity: config.channel_capacity,
            batch_size: config.batch_size,
            batch_max_age: Duration::from_secs(config.batch_max_age_secs),
            claim_batch_size: config.claim_batch_size,
            lease_ttl: Duration::from_secs(config.lease_ttl_secs),
            max_pending_age: Duration::from_secs(config.max_pending_age_secs),
            drain_timeout: Duration::from_secs(config.drain_timeout_secs),
        }
    }
}

/// Production hot-path lineage sink.
///
/// Two paths with two different contracts, and the type makes the difference
/// visible. [`LineageSink::record`] is fire-and-forget telemetry: it hands the
/// event to a bounded channel and counts a drop when that channel is full.
/// [`LineageHandle::record_durable_batch`] commits the batch to
/// `analytics.lineage_journal` and returns only after that commit.
///
/// The wake signal deliberately carries no payload. A `Notify` cannot hold an
/// event, so "the only copy of this record is in the channel" is not a state
/// this sink can be in: losing a wake costs at most one poll interval of
/// latency, never a record.
#[derive(Clone)]
pub struct MpscSink {
    ingress: mpsc::Sender<LineageEvent>,
    wake: Arc<Notify>,
    journal: LineageJournal,
    dropped: Arc<AtomicU64>,
}

impl MpscSink {
    /// Spawns the writer task and returns the hot-path sink plus worker handle.
    pub async fn spawn(
        config: MpscSinkConfig,
        store: LineageStore,
    ) -> Result<(Self, WriterHandle)> {
        store.ensure_schema().await?;
        let (ingress, rx) = mpsc::channel(config.channel_capacity);
        let journal = LineageJournal::new(store.postgres().clone(), config.lease_ttl);
        let wake = Arc::new(Notify::new());
        let writer_handle =
            spawn_writer_for_sink(rx, wake.clone(), config.clone(), store, journal.clone())?;
        Ok((
            Self {
                ingress,
                wake,
                journal,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            writer_handle,
        ))
    }

    /// Enqueues an event onto the best-effort ingress channel.
    ///
    /// Best-effort by contract: the writer batches enqueued events and commits
    /// them to the queue, so the hot path neither blocks nor spawns per-event
    /// work. When the channel is saturated the event is dropped and counted.
    fn enqueue(&self, evt: LineageEvent) {
        let event_class = lineage_event_class(&evt);
        match self.ingress.try_send(evt) {
            Ok(()) => {
                metrics::counter!(
                    "moa_lineage_enqueued_total",
                    "mode" => "best_effort",
                    "event_class" => event_class
                )
                .increment(1);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                metrics::counter!(
                    "moa_lineage_dropped_total",
                    "mode" => "best_effort",
                    "event_class" => event_class
                )
                .increment(1);
                tracing::warn!(
                    event_class,
                    "lineage event dropped because the writer channel is saturated"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                metrics::counter!(
                    "moa_lineage_failed_total",
                    "mode" => "best_effort",
                    "event_class" => event_class,
                    "reason" => "channel_closed"
                )
                .increment(1);
                tracing::error!(event_class, "lineage writer channel is closed; event lost");
            }
        }
    }

    /// Commits a batch of events to the durable queue and returns after commit.
    ///
    /// Returning means every event in `events` is committed in Postgres. Any
    /// replica can now finish them; this process dying loses nothing.
    pub async fn record_durable_events(
        &self,
        events: Vec<LineageEvent>,
    ) -> Result<Vec<uuid::Uuid>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let classes: Vec<&'static str> = events.iter().map(lineage_event_class).collect();
        let mut rows = Vec::with_capacity(events.len());
        for event in events {
            rows.push(PendingRow::from_event(event)?);
        }
        let journal_ids = self.journal.accept_batch(&rows).await?;
        for event_class in &classes {
            metrics::counter!(
                "moa_lineage_enqueued_total",
                "mode" => "durable",
                "event_class" => *event_class
            )
            .increment(1);
        }
        // Wake the drain so the committed rows are stored promptly. The signal
        // is an optimization only: a missed wake costs one poll interval.
        self.wake.notify_one();
        Ok(journal_ids)
    }

    async fn record_durable_batch_json(
        &self,
        events_json: Vec<serde_json::Value>,
    ) -> moa_core::error::Result<()> {
        if events_json.is_empty() {
            return Ok(());
        }
        let events = events_json
            .into_iter()
            .map(serde_json::from_value::<LineageEvent>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // Bound the durable commit so an unresponsive database cannot hang the
        // hot turn path. Timeout and commit errors are surfaced to the caller
        // (which logs and continues) and counted, never silently swallowed.
        let started = std::time::Instant::now();
        let result =
            tokio::time::timeout(DURABLE_ACCEPT_TIMEOUT, self.record_durable_events(events)).await;
        metrics::histogram!("moa_lineage_durable_append_seconds")
            .record(started.elapsed().as_secs_f64());
        match result {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(MoaError::StorageError(error.to_string())),
            Err(_) => {
                metrics::counter!(
                    "moa_lineage_failed_total",
                    "mode" => "durable",
                    "reason" => "accept_timeout"
                )
                .increment(1);
                Err(MoaError::StorageError(format!(
                    "lineage durable acceptance timed out after {DURABLE_ACCEPT_TIMEOUT:?}"
                )))
            }
        }
    }
}

impl LineageSink for MpscSink {
    fn record(&self, evt: LineageEvent) {
        self.enqueue(evt);
    }

    fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn lineage_event_class(evt: &LineageEvent) -> &'static str {
    match evt {
        LineageEvent::Decision(_) => "audit",
        LineageEvent::Eval(_) => "score",
        _ => "lineage",
    }
}

impl LineageHandle for MpscSink {
    fn record(&self, evt_json: serde_json::Value) {
        match serde_json::from_value::<LineageEvent>(evt_json) {
            Ok(evt) => LineageSink::record(self, evt),
            Err(error) => {
                metrics::counter!("moa_lineage_malformed_total").increment(1);
                tracing::warn!(%error, "malformed lineage event");
            }
        }
    }

    fn record_durable_batch<'a>(
        &'a self,
        events_json: Vec<serde_json::Value>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = moa_core::error::Result<()>> + Send + 'a>>
    {
        Box::pin(async move { self.record_durable_batch_json(events_json).await })
    }

    fn record_span_attributes(&self, span: &tracing::Span, evt_json: &serde_json::Value) {
        emit_lineage_span_attributes(span, evt_json);
    }

    fn dropped_count(&self) -> u64 {
        LineageSink::dropped_count(self)
    }
}

/// Span-attributes-only lineage sink for `MOA_LINEAGE_SINK=otel`.
#[derive(Debug, Default, Clone, Copy)]
pub struct OtelSink;

impl OtelSink {
    /// Creates a span-attributes-only lineage sink.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl LineageSink for OtelSink {
    fn record(&self, _evt: LineageEvent) {}

    fn dropped_count(&self) -> u64 {
        0
    }
}

impl LineageHandle for OtelSink {
    fn record(&self, evt_json: serde_json::Value) {
        emit_lineage_span_attributes(&tracing::Span::current(), &evt_json);
    }

    fn record_span_attributes(&self, span: &tracing::Span, evt_json: &serde_json::Value) {
        emit_lineage_span_attributes(span, evt_json);
    }
}

/// Disabled-cost lineage sink exported from the production sink crate.
///
/// Kept as a `pub` type for potential out-of-tree consumers. Production code
/// standardizes on [`moa_core::traits::NullLineageHandle`] for the disabled handle, so
/// this is a plain unit struct with no inner wrapper.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink;

impl NullSink {
    /// Creates a null sink.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl LineageSink for NullSink {
    fn record(&self, _evt: LineageEvent) {}

    fn dropped_count(&self) -> u64 {
        0
    }
}

impl LineageHandle for NullSink {
    fn record(&self, _evt_json: serde_json::Value) {}
}

fn emit_lineage_span_attributes(span: &tracing::Span, evt_json: &serde_json::Value) {
    match serde_json::from_value::<LineageEvent>(evt_json.clone()) {
        Ok(LineageEvent::Retrieval(record)) => {
            crate::otel::emit_retrieval_attrs(span, &record);
        }
        Ok(LineageEvent::Context(record)) => {
            crate::otel::emit_context_attrs(span, &record);
        }
        Ok(LineageEvent::Generation(record)) => {
            crate::otel::emit_generation_attrs(span, &record);
        }
        Ok(_) => {}
        Err(error) => {
            metrics::counter!("moa_lineage_malformed_total").increment(1);
            tracing::warn!(%error, "malformed lineage event for span attributes");
        }
    }
}

impl From<&WriterHandle> for WriterStats {
    fn from(handle: &WriterHandle) -> Self {
        handle.stats()
    }
}
