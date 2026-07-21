//! Knowledge synchronization run and ingestion-step domain types.

use chrono::{DateTime, Utc};
use moa_core::types::identifiers::TenantId;
use moa_core::types::memory::InformationBarrierId;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// One local sync and ingestion attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeSyncRun {
    /// Sync-run identifier.
    pub sync_run_uid: Uuid,
    /// Owning tenant.
    pub tenant_id: TenantId,
    /// Linked connection.
    pub connection_uid: Uuid,
    /// Parser selected for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser: Option<String>,
    /// Optional provider-record limit for this run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_records: Option<u32>,
    /// Connection information barrier snapshotted when this run was claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub information_barrier: Option<InformationBarrierId>,
    /// Current run status.
    pub status: SyncRunStatus,
    /// Number of source records observed.
    pub records_seen: u64,
    /// Number of records whose content changed.
    #[serde(default)]
    pub records_changed: u64,
    /// Number of provider-deleted records.
    #[serde(default)]
    pub records_deleted: u64,
    /// Number of records ingested.
    pub records_ingested: u64,
    /// Number of records failed.
    pub records_failed: u64,
    /// Number of parser jobs or local parse operations completed.
    #[serde(default)]
    pub objects_parsed: u64,
    /// Number of chunks embedded.
    #[serde(default)]
    pub chunks_embedded: u64,
    /// Number of graph nodes upserted.
    #[serde(default)]
    pub graph_nodes_upserted: u64,
    /// Number of graph edges upserted.
    #[serde(default)]
    pub graph_edges_upserted: u64,
    /// Latest safe failure code for the run, when failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Run start time.
    pub started_at: DateTime<Utc>,
    /// Run finish time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
}

/// Counter update accumulated while processing one sync run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KnowledgeSyncCounters {
    /// Number of source records observed.
    pub records_seen: u64,
    /// Number of records whose content changed.
    pub records_changed: u64,
    /// Number of provider-deleted records.
    pub records_deleted: u64,
    /// Number of records successfully ingested.
    pub records_ingested: u64,
    /// Number of records that failed ingestion.
    pub records_failed: u64,
    /// Number of objects parsed.
    pub objects_parsed: u64,
    /// Number of chunks embedded.
    pub chunks_embedded: u64,
    /// Number of graph nodes upserted.
    pub graph_nodes_upserted: u64,
    /// Number of graph edges upserted.
    pub graph_edges_upserted: u64,
}

/// Sync-run status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncRunStatus {
    /// Accepted but not yet doing provider work.
    Queued,
    /// Provider-side sync was requested and has not completed.
    ProviderSyncing,
    /// Provider-side sync completed and local ingestion is not yet running.
    ProviderSynced,
    /// Parser job is queued or waiting on an external parser callback.
    ParsePending,
    /// Local parsing, embedding, graph, or vector work is running.
    Ingesting,
    /// Run completed successfully.
    Completed,
    /// Run failed but the classified failure is safe to retry.
    FailedRetryable,
    /// Run failed and should not retry without operator or data changes.
    FailedTerminal,
    /// Run was canceled before completion.
    Canceled,
}

impl SyncRunStatus {
    /// Returns the stable database status identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::ProviderSyncing => "provider_syncing",
            Self::ProviderSynced => "provider_synced",
            Self::ParsePending => "parse_pending",
            Self::Ingesting => "ingesting",
            Self::Completed => "completed",
            Self::FailedRetryable => "failed_retryable",
            Self::FailedTerminal => "failed_terminal",
            Self::Canceled => "canceled",
        }
    }
}

/// Ingestion step status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestionStepStatus {
    /// Step has started.
    Started,
    /// Step completed.
    Completed,
    /// Step failed.
    Failed,
    /// Step was skipped.
    Skipped,
}

impl IngestionStepStatus {
    /// Returns the stable database status identifier.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
        }
    }
}

/// Ingestion step row safe for storage and traces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KnowledgeIngestionStep {
    /// Step identifier.
    pub step_uid: Uuid,
    /// Sync run.
    pub sync_run_uid: Uuid,
    /// Optional object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_uid: Option<Uuid>,
    /// Step name.
    pub step: String,
    /// Step status.
    pub status: IngestionStepStatus,
    /// Start timestamp.
    pub started_at: DateTime<Utc>,
    /// End timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
    /// Duration in milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Safe counters.
    #[serde(default)]
    pub counters: Value,
    /// Safe summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Retry count.
    pub retry_count: u32,
    /// Typed error code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}
