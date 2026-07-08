//! Bounded hot-path mpsc sink implementation.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moa_core::{LineageHandle, MoaError};
use moa_lineage_core::{LineageEvent, LineageSink};
use tokio::sync::mpsc;

use crate::store::LineageStore;
use crate::writer::{DurableJournal, WriterCommand, WriterHandle, spawn_writer_for_sink};
use crate::{Result, WriterStats};

/// Configuration for the production mpsc lineage sink.
#[derive(Clone, Debug)]
pub struct MpscSinkConfig {
    /// Channel depth. 8192 is the recommended default.
    pub channel_capacity: usize,
    /// Maximum rows written per batch.
    pub batch_size: usize,
    /// Maximum age for a partial batch.
    pub batch_max_age: Duration,
    /// fjall journal directory.
    pub journal_path: PathBuf,
}

impl Default for MpscSinkConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 8192,
            batch_size: 512,
            batch_max_age: Duration::from_secs(2),
            journal_path: "/var/lib/moa/lineage-journal".into(),
        }
    }
}

impl From<&moa_core::LineageConfig> for MpscSinkConfig {
    fn from(config: &moa_core::LineageConfig) -> Self {
        Self {
            channel_capacity: config.channel_capacity,
            batch_size: config.batch_size,
            batch_max_age: Duration::from_secs(config.batch_max_age_secs),
            journal_path: expand_home(&config.journal_path),
        }
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// Builder for `MpscSink`.
#[derive(Debug, Default)]
pub struct MpscSinkBuilder {
    config: MpscSinkConfig,
}

impl MpscSinkBuilder {
    /// Creates a builder with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the channel capacity.
    #[must_use]
    pub fn channel_capacity(mut self, channel_capacity: usize) -> Self {
        self.config.channel_capacity = channel_capacity;
        self
    }

    /// Overrides the worker batch size.
    #[must_use]
    pub fn batch_size(mut self, batch_size: usize) -> Self {
        self.config.batch_size = batch_size;
        self
    }

    /// Overrides the worker batch max age.
    #[must_use]
    pub fn batch_max_age(mut self, batch_max_age: Duration) -> Self {
        self.config.batch_max_age = batch_max_age;
        self
    }

    /// Overrides the fjall journal path.
    #[must_use]
    pub fn journal_path(mut self, journal_path: PathBuf) -> Self {
        self.config.journal_path = journal_path;
        self
    }

    /// Spawns a sink and writer against the provided lineage store.
    pub async fn spawn(self, store: LineageStore) -> Result<(MpscSink, WriterHandle)> {
        MpscSink::spawn(self.config, store).await
    }
}

/// Production hot-path lineage sink.
#[derive(Clone)]
pub struct MpscSink {
    tx: mpsc::Sender<WriterCommand>,
    journal: DurableJournal,
    dropped: Arc<AtomicU64>,
}

impl MpscSink {
    /// Spawns the writer task and returns the hot-path sink plus worker handle.
    pub async fn spawn(
        config: MpscSinkConfig,
        store: LineageStore,
    ) -> Result<(Self, WriterHandle)> {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let journal = DurableJournal::open(&config.journal_path)?;
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_handle =
            spawn_writer_for_sink(rx, config.clone(), store, journal.clone()).await?;
        Ok((
            Self {
                tx,
                journal,
                dropped,
            },
            writer_handle,
        ))
    }

    /// Enqueues an event to the single journal-writer task with one bounded `try_send`.
    ///
    /// Best-effort by contract: the writer batches enqueued events and group-commits them to the
    /// journal, so the hot path neither blocks nor spawns per-event work. When the writer channel
    /// is saturated the event is dropped and counted. Callers that require guaranteed durability
    /// use [`MpscSink::record_durable_event`], which awaits journal acceptance instead.
    fn enqueue(&self, evt: LineageEvent) {
        let event_class = lineage_event_class(&evt);
        match self.tx.try_send(WriterCommand::Event(Box::new(evt))) {
            Ok(()) => {
                metrics::counter!(
                    "moa_lineage_enqueued_total",
                    "mode" => "durable",
                    "event_class" => event_class
                )
                .increment(1);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                metrics::counter!(
                    "moa_lineage_dropped_total",
                    "mode" => "durable",
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
                    "mode" => "durable",
                    "event_class" => event_class,
                    "reason" => "channel_closed"
                )
                .increment(1);
                tracing::error!(event_class, "lineage writer channel is closed; event lost");
            }
        }
    }

    /// Records an event and returns after it has reached the durable journal.
    pub async fn record_durable_event(&self, evt: LineageEvent) -> Result<u64> {
        let event_class = lineage_event_class(&evt);
        let seq = self.journal.append_accepted_event(evt).await?;
        record_journal_notification(&self.tx, seq, event_class);
        Ok(seq)
    }

    async fn record_durable_json(&self, evt_json: serde_json::Value) -> moa_core::Result<()> {
        let evt = serde_json::from_value::<LineageEvent>(evt_json)?;
        self.record_durable_event(evt)
            .await
            .map(|_| ())
            .map_err(|error| MoaError::StorageError(error.to_string()))
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

fn record_journal_notification(
    tx: &mpsc::Sender<WriterCommand>,
    seq: u64,
    event_class: &'static str,
) {
    match tx.try_send(WriterCommand::Journaled(seq)) {
        Ok(()) => {
            metrics::counter!(
                "moa_lineage_enqueued_total",
                "mode" => "durable",
                "event_class" => event_class
            )
            .increment(1);
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            metrics::counter!(
                "moa_lineage_backpressure_total",
                "mode" => "durable",
                "event_class" => event_class
            )
            .increment(1);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            metrics::counter!(
                "moa_lineage_failed_total",
                "mode" => "durable",
                "event_class" => event_class,
                "reason" => "channel_closed"
            )
            .increment(1);
            tracing::error!(
                seq,
                event_class,
                "lineage event journaled but writer channel is closed"
            );
        }
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

    fn record_durable<'a>(
        &'a self,
        evt_json: serde_json::Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = moa_core::Result<()>> + Send + 'a>>
    {
        Box::pin(async move { self.record_durable_json(evt_json).await })
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
/// standardizes on [`moa_core::NullLineageHandle`] for the disabled handle, so
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
            moa_lineage_otel::emit_retrieval_attrs(span, &record);
        }
        Ok(LineageEvent::Context(record)) => {
            moa_lineage_otel::emit_context_attrs(span, &record);
        }
        Ok(LineageEvent::Generation(record)) => {
            moa_lineage_otel::emit_generation_attrs(span, &record);
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{SessionId, StoragePartitionId, TenantId, UserId};
    use moa_lineage_core::{
        BackendIntrospection, LineageEvent, RetrievalLineage, RetrievalStage, StageTimings, TurnId,
    };
    use moa_memory_types::MemoryScope;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn record_enqueues_bounded_event_command_without_dropping() {
        // Pins: fire-and-forget record enqueues exactly one WriterCommand::Event with a bounded
        // try_send (no per-event task) and does not drop while the channel has capacity.
        let (tx, mut rx) = mpsc::channel(4);
        let journal = test_journal();
        let sink = MpscSink {
            tx,
            journal,
            dropped: Arc::new(AtomicU64::new(0)),
        };

        LineageSink::record(&sink, sample_event());

        assert_eq!(LineageSink::dropped_count(&sink), 0);
        assert!(
            matches!(rx.try_recv(), Ok(WriterCommand::Event(_))),
            "record should enqueue a WriterCommand::Event for the writer to group-commit"
        );
    }

    #[test]
    fn record_drops_event_when_channel_is_full() {
        // Pins: a saturated writer channel drops the event and increments the bounded drop
        // counter instead of blocking the hot path or spawning unbounded work.
        let (tx, _rx) = mpsc::channel(1);
        let journal = test_journal();
        let sink = MpscSink {
            tx,
            journal,
            dropped: Arc::new(AtomicU64::new(0)),
        };

        LineageSink::record(&sink, sample_event());
        LineageSink::record(&sink, sample_event());

        assert_eq!(LineageSink::dropped_count(&sink), 1);
    }

    #[tokio::test]
    async fn record_durable_event_returns_after_journal_acceptance() {
        // Pins: the awaitable durability path resolves only after fjall acceptance.
        let (tx, _rx) = mpsc::channel(1);
        let journal = test_journal();
        let sink = MpscSink {
            tx,
            journal: journal.clone(),
            dropped: Arc::new(AtomicU64::new(0)),
        };

        let seq = sink
            .record_durable_event(sample_event())
            .await
            .expect("durable record should append");

        assert_eq!(seq, 1);
        assert_eq!(LineageSink::dropped_count(&sink), 0);
        assert_eq!(
            journal
                .approximate_len()
                .expect("journal depth should be readable"),
            1
        );
    }

    #[test]
    fn null_sink_never_records_drops() {
        let sink = NullSink::new();

        LineageSink::record(&sink, sample_event());

        assert_eq!(LineageSink::dropped_count(&sink), 0);
    }

    fn test_journal() -> DurableJournal {
        let path = std::env::temp_dir().join(format!("moa-lineage-sink-{}", Uuid::now_v7()));
        DurableJournal::open(&path).expect("test journal should open")
    }

    fn sample_event() -> LineageEvent {
        let tenant_id = TenantId::from(Uuid::from_u128(1));
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
        LineageEvent::Retrieval(RetrievalLineage {
            turn_id: TurnId::new_v7(),
            session_id: SessionId::new(),
            storage_partition_id: storage_partition_id.clone(),
            user_id: UserId::new("test-user"),
            scope: MemoryScope::Tenant { tenant_id },
            ts: Utc::now(),
            query_original: "test query".to_string(),
            query_expansions: Vec::new(),
            vector_hits: Vec::new(),
            graph_paths: Vec::new(),
            fusion_scores: Vec::new(),
            rerank_scores: Vec::new(),
            top_k: vec![Uuid::now_v7()],
            searched_scopes: Vec::new(),
            selected_hits: Vec::new(),
            filters: serde_json::Value::Null,
            timings: StageTimings::default(),
            introspection: BackendIntrospection::default(),
            stage: RetrievalStage::Single,
        })
    }
}
