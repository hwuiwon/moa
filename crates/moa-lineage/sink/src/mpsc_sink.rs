//! Bounded hot-path mpsc sink implementation.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moa_core::{LineageHandle, NullLineageHandle};
use moa_lineage_core::{LineageEvent, LineageSink};
use tokio::sync::mpsc;

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
    /// Allows non-audit telemetry events to drop under channel pressure.
    pub lossy_telemetry: bool,
}

impl Default for MpscSinkConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 8192,
            batch_size: 512,
            batch_max_age: Duration::from_secs(2),
            journal_path: "/var/lib/moa/lineage-journal".into(),
            lossy_telemetry: false,
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
            lossy_telemetry: false,
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

    /// Enables lossy channel behavior for non-audit telemetry events.
    #[must_use]
    pub fn lossy_telemetry(mut self, lossy_telemetry: bool) -> Self {
        self.config.lossy_telemetry = lossy_telemetry;
        self
    }

    /// Spawns a sink and writer against the provided SQL pool.
    pub async fn spawn(self, pool: sqlx::PgPool) -> Result<(MpscSink, WriterHandle)> {
        MpscSink::spawn(self.config, pool).await
    }
}

/// Production hot-path lineage sink.
#[derive(Clone)]
pub struct MpscSink {
    tx: mpsc::Sender<WriterCommand>,
    journal: DurableJournal,
    dropped: Arc<AtomicU64>,
    lossy_telemetry: bool,
}

impl MpscSink {
    /// Spawns the writer task and returns the hot-path sink plus worker handle.
    pub async fn spawn(config: MpscSinkConfig, pool: sqlx::PgPool) -> Result<(Self, WriterHandle)> {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let journal = DurableJournal::open(&config.journal_path)?;
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_handle =
            spawn_writer_for_sink(rx, config.clone(), pool, journal.clone()).await?;
        Ok((
            Self {
                tx,
                journal,
                dropped,
                lossy_telemetry: config.lossy_telemetry,
            },
            writer_handle,
        ))
    }

    fn record_durable(&self, evt: LineageEvent) {
        let event_class = lineage_event_class(&evt);
        let seq = match self.journal.append_accepted_event(evt) {
            Ok(seq) => seq,
            Err(error) => {
                metrics::counter!(
                    "moa_lineage_failed_total",
                    "mode" => "durable",
                    "event_class" => event_class,
                    "reason" => "journal_append"
                )
                .increment(1);
                tracing::error!(%error, event_class, "lineage event failed before durable journal append");
                return;
            }
        };

        match self.tx.try_send(WriterCommand::Journaled(seq)) {
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

    fn record_lossy_telemetry(&self, evt: LineageEvent) {
        match self.tx.try_send(WriterCommand::Event(Box::new(evt))) {
            Ok(()) => {
                metrics::counter!(
                    "moa_lineage_enqueued_total",
                    "mode" => "lossy_telemetry",
                    "event_class" => "telemetry"
                )
                .increment(1);
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                metrics::counter!(
                    "moa_lineage_dropped_total",
                    "mode" => "lossy_telemetry",
                    "event_class" => "telemetry"
                )
                .increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                metrics::counter!(
                    "moa_lineage_failed_total",
                    "mode" => "lossy_telemetry",
                    "event_class" => "telemetry",
                    "reason" => "channel_closed"
                )
                .increment(1);
                tracing::error!("lossy telemetry lineage writer channel is closed");
            }
        }
    }
}

impl LineageSink for MpscSink {
    fn record(&self, evt: LineageEvent) {
        if self.lossy_telemetry && is_telemetry_event(&evt) {
            self.record_lossy_telemetry(evt);
        } else {
            self.record_durable(evt);
        }
    }

    fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

fn is_telemetry_event(evt: &LineageEvent) -> bool {
    !matches!(evt, LineageEvent::Decision(_))
}

fn lineage_event_class(evt: &LineageEvent) -> &'static str {
    if is_telemetry_event(evt) {
        "telemetry"
    } else {
        "audit"
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
#[derive(Debug, Default, Clone, Copy)]
pub struct NullSink {
    inner: NullLineageHandle,
}

impl NullSink {
    /// Creates a null sink.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: NullLineageHandle,
        }
    }
}

impl LineageSink for NullSink {
    fn record(&self, _evt: LineageEvent) {}

    fn dropped_count(&self) -> u64 {
        0
    }
}

impl LineageHandle for NullSink {
    fn record(&self, evt_json: serde_json::Value) {
        self.inner.record(evt_json);
    }
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
        BackendIntrospection, DecisionKind, DecisionRecord, LineageEvent, PiiRedactionDecision,
        RetrievalLineage, RetrievalStage, StageTimings, TurnId,
    };
    use moa_memory_types::MemoryScope;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn lossy_telemetry_drops_when_channel_is_full() {
        // Pins: only explicit lossy telemetry mode uses the lineage dropped counter.
        let (tx, _rx) = mpsc::channel(1);
        let journal = test_journal();
        let sink = MpscSink {
            tx,
            journal,
            dropped: Arc::new(AtomicU64::new(0)),
            lossy_telemetry: true,
        };

        LineageSink::record(&sink, sample_event());
        LineageSink::record(&sink, sample_event());

        assert_eq!(LineageSink::dropped_count(&sink), 1);
    }

    #[test]
    fn audit_event_full_channel_is_journaled_not_dropped() {
        // Pins: audit-grade lineage reaches fjall before it is accepted, even
        // when the notification channel is already full.
        let (tx, _rx) = mpsc::channel(1);
        let journal = test_journal();
        let sink = MpscSink {
            tx,
            journal: journal.clone(),
            dropped: Arc::new(AtomicU64::new(0)),
            lossy_telemetry: true,
        };

        LineageSink::record(&sink, sample_event());
        LineageSink::record(&sink, sample_decision_event());
        LineageSink::record(&sink, sample_event());

        assert_eq!(LineageSink::dropped_count(&sink), 1);
        assert_eq!(
            journal
                .approximate_len()
                .expect("journal depth should be readable"),
            1
        );
    }

    #[test]
    fn default_sink_journals_telemetry_instead_of_dropping() {
        // Pins: lossy telemetry has to be explicit; the default sink journals
        // telemetry events under channel pressure.
        let (tx, _rx) = mpsc::channel(1);
        let journal = test_journal();
        let sink = MpscSink {
            tx,
            journal: journal.clone(),
            dropped: Arc::new(AtomicU64::new(0)),
            lossy_telemetry: false,
        };

        LineageSink::record(&sink, sample_event());
        LineageSink::record(&sink, sample_event());

        assert_eq!(LineageSink::dropped_count(&sink), 0);
        assert_eq!(
            journal
                .approximate_len()
                .expect("journal depth should be readable"),
            2
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

    fn sample_decision_event() -> LineageEvent {
        let tenant_id = TenantId::from(Uuid::from_u128(1));
        let storage_partition_id = StoragePartitionId::for_tenant(tenant_id);
        LineageEvent::Decision(DecisionRecord {
            turn_id: TurnId::new_v7(),
            session_id: SessionId::new(),
            storage_partition_id,
            user_id: UserId::new("test-user"),
            ts: Utc::now(),
            kind: DecisionKind::PiiRedaction(PiiRedactionDecision {
                subject_pseudonym: Some("subject-1".to_string()),
                fields: vec!["email".to_string()],
                detector: "test-detector".to_string(),
                redacted: true,
            }),
            policy_version: "test-policy-v1".to_string(),
            integrity_hash: vec![7; 32],
        })
    }
}
