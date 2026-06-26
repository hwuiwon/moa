//! Redacted observability helpers for tenant knowledge ingestion.

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    domain::{IngestionStepStatus, KnowledgeIngestionStep},
    error::Result,
};

/// Safe step outcome recorded by ingestion observers.
#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    /// Step status.
    pub status: IngestionStepStatus,
    /// Safe counters.
    pub counters: Value,
    /// Safe summary.
    pub summary: Option<String>,
    /// Retry count.
    pub retry_count: u32,
    /// Error code.
    pub error_code: Option<String>,
}

impl StepOutcome {
    /// Creates a completed step outcome.
    #[must_use]
    pub fn completed() -> Self {
        Self {
            status: IngestionStepStatus::Completed,
            counters: Value::Null,
            summary: None,
            retry_count: 0,
            error_code: None,
        }
    }

    /// Creates a failed step outcome.
    #[must_use]
    pub fn failed(error_code: impl Into<String>) -> Self {
        Self {
            status: IngestionStepStatus::Failed,
            counters: Value::Null,
            summary: None,
            retry_count: 0,
            error_code: Some(error_code.into()),
        }
    }
}

/// Low-cardinality labels attached to ingestion metrics and spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepLabels<'a> {
    /// Linked-account provider identifier.
    pub provider: &'a str,
    /// Parser identifier.
    pub parser: &'a str,
    /// Ingestion stage.
    pub stage: &'static str,
    /// Whether the failure is retryable.
    pub retryable: bool,
    /// Typed error code or `none`.
    pub error_code: &'a str,
}

/// Sink for redacted ingestion progress.
#[async_trait]
pub trait IngestionObserver: Send + Sync {
    /// Records one ingestion step.
    async fn record_step(
        &self,
        sync_run_uid: Uuid,
        object_uid: Option<Uuid>,
        labels: StepLabels<'_>,
        outcome: StepOutcome,
    ) -> Result<()>;
}

/// Metrics and tracing observer that does not persist payloads.
#[derive(Debug, Clone, Copy, Default)]
pub struct MetricsIngestionObserver;

#[async_trait]
impl IngestionObserver for MetricsIngestionObserver {
    async fn record_step(
        &self,
        _sync_run_uid: Uuid,
        _object_uid: Option<Uuid>,
        labels: StepLabels<'_>,
        outcome: StepOutcome,
    ) -> Result<()> {
        let status: &'static str = match outcome.status {
            IngestionStepStatus::Started => "started",
            IngestionStepStatus::Completed => "completed",
            IngestionStepStatus::Failed => "failed",
            IngestionStepStatus::Skipped => "skipped",
        };
        metrics::counter!(
            "moa_knowledge_ingestion_steps_total",
            "provider" => labels.provider.to_string(),
            "parser" => labels.parser.to_string(),
            "stage" => labels.stage,
            "status" => status,
            "retryable" => labels.retryable.to_string(),
            "error_code" => labels.error_code.to_string(),
        )
        .increment(1);
        tracing::info!(
            provider = labels.provider,
            parser = labels.parser,
            stage = labels.stage,
            status,
            retry_count = outcome.retry_count,
            retryable = labels.retryable,
            error_code = labels.error_code,
            "knowledge ingestion step recorded"
        );
        Ok(())
    }
}

/// Builds a redacted ingestion step row for repository persistence.
#[must_use]
pub fn build_step_row(
    sync_run_uid: Uuid,
    object_uid: Option<Uuid>,
    step: impl Into<String>,
    outcome: StepOutcome,
) -> KnowledgeIngestionStep {
    let now = Utc::now();
    KnowledgeIngestionStep {
        step_uid: Uuid::now_v7(),
        sync_run_uid,
        object_uid,
        step: step.into(),
        status: outcome.status,
        started_at: now,
        ended_at: Some(now),
        duration_ms: Some(0),
        counters: sanitize_counters(outcome.counters),
        summary: outcome.summary,
        retry_count: outcome.retry_count,
        error_code: outcome.error_code,
    }
}

fn sanitize_counters(counters: Value) -> Value {
    match counters {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .filter(|(key, _)| {
                    matches!(
                        key.as_str(),
                        "records_listed"
                            | "records_changed"
                            | "records_deleted"
                            | "bytes_fetched"
                            | "parser_pages"
                            | "parser_items"
                            | "blocks_total"
                            | "blocks_new"
                            | "blocks_deleted"
                            | "chunks_total"
                            | "chunks_new"
                            | "chunks_deleted"
                            | "embeddings_created"
                            | "embeddings_reused"
                            | "graph_nodes_upserted"
                            | "graph_edges_upserted"
                            | "vector_rows_upserted"
                            | "vector_rows_deleted"
                            | "contact_group_memberships_changed"
                    )
                })
                .collect(),
        ),
        _ => json!({}),
    }
}
