//! Bounded hot-path mpsc sink implementation.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use moa_core::{LineageHandle, NullLineageHandle};
use moa_lineage_core::{LineageEvent, LineageSink};
use tokio::sync::mpsc;

use crate::writer::{WriterHandle, spawn_writer};
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

    /// Spawns a sink and writer against the provided SQL pool.
    pub async fn spawn(self, pool: sqlx::PgPool) -> Result<(MpscSink, WriterHandle)> {
        MpscSink::spawn(self.config, pool).await
    }
}

/// Production hot-path lineage sink.
#[derive(Clone)]
pub struct MpscSink {
    tx: mpsc::Sender<LineageEvent>,
    dropped: Arc<AtomicU64>,
}

impl MpscSink {
    /// Spawns the writer task and returns the hot-path sink plus worker handle.
    pub async fn spawn(config: MpscSinkConfig, pool: sqlx::PgPool) -> Result<(Self, WriterHandle)> {
        let (tx, rx) = mpsc::channel(config.channel_capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        let writer_handle = spawn_writer(rx, config, pool).await?;
        Ok((Self { tx, dropped }, writer_handle))
    }
}

impl LineageSink for MpscSink {
    fn record(&self, evt: LineageEvent) {
        if self.tx.try_send(evt).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("moa_lineage_dropped_total").increment(1);
        } else {
            metrics::counter!("moa_lineage_recorded_total").increment(1);
        }
    }

    fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
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

    fn dropped_count(&self) -> u64 {
        LineageSink::dropped_count(self)
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

impl From<&WriterHandle> for WriterStats {
    fn from(handle: &WriterHandle) -> Self {
        handle.stats()
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use moa_core::{MemoryScope, SessionId, UserId, WorkspaceId};
    use moa_lineage_core::{
        BackendIntrospection, LineageEvent, RetrievalLineage, RetrievalStage, StageTimings, TurnId,
    };
    use uuid::Uuid;

    use super::*;

    #[test]
    fn mpsc_sink_drops_when_channel_is_full() {
        let (tx, _rx) = mpsc::channel(1);
        let sink = MpscSink {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        };

        LineageSink::record(&sink, sample_event());
        LineageSink::record(&sink, sample_event());

        assert_eq!(LineageSink::dropped_count(&sink), 1);
    }

    #[test]
    fn null_sink_never_records_drops() {
        let sink = NullSink::new();

        LineageSink::record(&sink, sample_event());

        assert_eq!(LineageSink::dropped_count(&sink), 0);
    }

    fn sample_event() -> LineageEvent {
        let workspace_id = WorkspaceId::new("test-workspace");
        LineageEvent::Retrieval(RetrievalLineage {
            turn_id: TurnId::new_v7(),
            session_id: SessionId::new(),
            workspace_id: workspace_id.clone(),
            user_id: UserId::new("test-user"),
            scope: MemoryScope::Workspace { workspace_id },
            ts: Utc::now(),
            query_original: "test query".to_string(),
            query_expansions: Vec::new(),
            vector_hits: Vec::new(),
            graph_paths: Vec::new(),
            fusion_scores: Vec::new(),
            rerank_scores: Vec::new(),
            top_k: vec![Uuid::now_v7()],
            timings: StageTimings::default(),
            introspection: BackendIntrospection::default(),
            stage: RetrievalStage::Single,
        })
    }
}
