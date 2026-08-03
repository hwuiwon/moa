//! Redacted observability helpers for tenant knowledge ingestion.

use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    domain::{IngestionStepStatus, KnowledgeIngestionStep},
    error::Error,
};

/// Safe step outcome recorded by the ingestion pipeline.
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
    /// Step duration in milliseconds.
    pub duration_ms: Option<u64>,
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
            duration_ms: None,
        }
    }

    /// Creates a completed step outcome with safe counters.
    #[must_use]
    pub fn completed_with_counters(counters: Value) -> Self {
        Self {
            counters,
            ..Self::completed()
        }
    }

    /// Creates a completed step outcome with safe counters and summary.
    #[must_use]
    pub fn completed_with_counters_and_summary(
        counters: Value,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            counters,
            summary: Some(summary.into()),
            ..Self::completed()
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
            duration_ms: None,
        }
    }
}

/// Stable failure classification for support-visible retry decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureClassification {
    /// Actionable safe error code.
    pub error_code: &'static str,
    /// Whether the failure can be retried without operator or source-data changes.
    pub retryable: bool,
}

impl FailureClassification {
    /// Returns the retry classification label used in sync status and summaries.
    #[must_use]
    pub const fn retry_label(self) -> &'static str {
        if self.retryable {
            "retryable"
        } else {
            "terminal"
        }
    }
}

/// Classifies a tenant knowledge failure into a safe error code and retry decision.
#[must_use]
pub fn classify_failure(stage: &str, error: &Error) -> FailureClassification {
    let prefix = failure_prefix(stage);
    let codes = stage_failure_codes(prefix);
    match error {
        Error::UnsupportedFormat(_) => FailureClassification {
            error_code: "parser_unsupported_format",
            retryable: false,
        },
        Error::HttpStatus { status, .. } if retryable_http_status(*status) => {
            FailureClassification {
                error_code: codes.http_retryable,
                retryable: true,
            }
        }
        Error::HttpStatus { .. } => FailureClassification {
            error_code: codes.http_terminal,
            retryable: false,
        },
        Error::Transport(_) => FailureClassification {
            error_code: codes.transport_retryable,
            retryable: true,
        },
        Error::Provider { .. } => FailureClassification {
            error_code: "provider_error_retryable",
            retryable: true,
        },
        Error::Parser { .. } => FailureClassification {
            error_code: "parser_error_retryable",
            retryable: true,
        },
        Error::Decode(_) => FailureClassification {
            error_code: codes.decode_terminal,
            retryable: false,
        },
        Error::Config(_) => FailureClassification {
            error_code: codes.config_terminal,
            retryable: false,
        },
        Error::Repository(_) | Error::Database { .. } if prefix == "graph" => {
            FailureClassification {
                error_code: "graph_write_failed_retryable",
                retryable: true,
            }
        }
        Error::Repository(_) | Error::Database { .. } if prefix == "embedder" => {
            FailureClassification {
                error_code: "embedder_failed_retryable",
                retryable: true,
            }
        }
        // A provider-contract violation (wrong vector count) will not self-heal on
        // a plain retry, so it is terminal rather than retryable.
        Error::EmbeddingCardinalityMismatch { .. } => FailureClassification {
            error_code: "embedder_cardinality_mismatch",
            retryable: false,
        },
        Error::Repository(_) | Error::Database { .. } => FailureClassification {
            error_code: "repository_failed_retryable",
            retryable: true,
        },
    }
}

/// Builds a failed outcome with a stable safe summary.
#[must_use]
pub fn failed_outcome(classification: FailureClassification) -> StepOutcome {
    StepOutcome {
        status: IngestionStepStatus::Failed,
        counters: json!({}),
        summary: Some(format!("{} failure", classification.retry_label())),
        retry_count: 0,
        error_code: Some(classification.error_code.to_string()),
        duration_ms: None,
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

/// Records redacted ingestion progress to metrics and tracing.
pub fn record_step_observability(labels: StepLabels<'_>, outcome: &StepOutcome) {
    let status: &'static str = match outcome.status {
        IngestionStepStatus::Started => "started",
        IngestionStepStatus::Completed => "completed",
        IngestionStepStatus::Failed => "failed",
        IngestionStepStatus::Skipped => "skipped",
    };
    tracing::Span::current().record("status", status);
    tracing::Span::current().record(
        "error_code",
        outcome.error_code.as_deref().unwrap_or("none"),
    );
    metrics::histogram!(
        "moa_knowledge_ingestion_step_duration_seconds",
        "provider" => labels.provider.to_string(),
        "parser" => labels.parser.to_string(),
        "stage" => labels.stage,
        "status" => status
    )
    .record(outcome.duration_seconds());
    emit_counter_metrics(labels, status, &outcome.counters);
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
}

impl StepOutcome {
    fn duration_seconds(&self) -> f64 {
        self.duration_ms
            .map(|duration_ms| duration_ms as f64 / 1000.0)
            .unwrap_or(0.0)
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
    let ended_at = Utc::now();
    let duration_ms = outcome.duration_ms.unwrap_or(0);
    let started_at =
        ended_at - chrono::Duration::milliseconds(duration_ms.min(i64::MAX as u64) as i64);
    KnowledgeIngestionStep {
        step_uid: Uuid::now_v7(),
        sync_run_uid,
        object_uid,
        step: step.into(),
        status: outcome.status,
        started_at,
        ended_at: Some(ended_at),
        duration_ms: Some(duration_ms),
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
                        "records_seen"
                            | "records_listed"
                            | "records_changed"
                            | "records_deleted"
                            | "records_ingested"
                            | "records_failed"
                            | "records_pruned"
                            | "objects_parsed"
                            | "bytes_fetched"
                            | "parser_pages"
                            | "parser_items"
                            | "blocks_total"
                            | "blocks_new"
                            | "blocks_deleted"
                            | "chunks_total"
                            | "chunks_new"
                            | "chunks_deleted"
                            | "chunks_embedded"
                            | "cache_hits"
                            | "cache_misses"
                            | "entities_extracted"
                            | "relations_extracted"
                            | "semantic_chunk_links"
                            | "failures"
                            | "embeddings_created"
                            | "embeddings_reused"
                            | "graph_nodes_upserted"
                            | "graph_edges_upserted"
                            | "contact_groups"
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

fn emit_counter_metrics(labels: StepLabels<'_>, status: &str, counters: &Value) {
    let records_listed = safe_counter(counters, "records_listed");
    if records_listed > 0 {
        metrics::counter!(
            "moa_knowledge_records_total",
            "provider" => labels.provider.to_string(),
            "action" => "listed"
        )
        .increment(records_listed);
    }
    let records_seen = safe_counter(counters, "records_seen");
    if records_seen > 0 {
        metrics::counter!(
            "moa_knowledge_records_total",
            "provider" => labels.provider.to_string(),
            "action" => "seen"
        )
        .increment(records_seen);
    }
    for (key, action) in [
        ("records_changed", "changed"),
        ("records_deleted", "deleted"),
        ("records_ingested", "ingested"),
    ] {
        let count = safe_counter(counters, key);
        if count > 0 {
            metrics::counter!(
                "moa_knowledge_records_total",
                "provider" => labels.provider.to_string(),
                "action" => action
            )
            .increment(count);
        }
    }
    let records_failed =
        safe_counter(counters, "records_failed").max(u64::from(status == "failed"));
    if records_failed > 0 {
        metrics::counter!(
            "moa_knowledge_records_total",
            "provider" => labels.provider.to_string(),
            "action" => "failed"
        )
        .increment(records_failed);
    }
    if labels.stage == "parse_completed" || labels.stage == "parse_submitted" {
        metrics::counter!(
            "moa_knowledge_parse_jobs_total",
            "parser" => labels.parser.to_string(),
            "status" => status.to_string()
        )
        .increment(1);
    }
    for (key, action) in [
        ("chunks_total", "total"),
        ("chunks_new", "created"),
        ("chunks_deleted", "deleted"),
        ("chunks_embedded", "embedded"),
    ] {
        let count = safe_counter(counters, key);
        if count > 0 {
            metrics::counter!(
                "moa_knowledge_chunks_total",
                "action" => action
            )
            .increment(count);
        }
    }
    for (key, metric_status) in [
        ("embeddings_created", "created"),
        ("embeddings_reused", "reused"),
    ] {
        let count = safe_counter(counters, key);
        if count > 0 {
            metrics::counter!(
                "moa_knowledge_embeddings_total",
                "status" => metric_status
            )
            .increment(count);
        }
    }
    for (key, kind) in [
        ("graph_nodes_upserted", "node"),
        ("graph_edges_upserted", "edge"),
    ] {
        let count = safe_counter(counters, key);
        if count > 0 {
            metrics::counter!(
                "moa_knowledge_graph_writes_total",
                "kind" => kind,
                "status" => status.to_string()
            )
            .increment(count);
        }
    }
}

fn safe_counter(counters: &Value, key: &str) -> u64 {
    counters.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn failure_prefix(stage: &str) -> &'static str {
    match stage {
        "provider_triggered" | "provider_records_listed" | "content_fetched" => "provider",
        "parse_submitted" | "parse_completed" => "parser",
        "embedded" => "embedder",
        "graph_upserted" | "vector_indexed" => "graph",
        _ => "knowledge",
    }
}

fn retryable_http_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429 | 500..=599)
}

/// Stage-prefixed failure codes selected by [`classify_failure`].
struct StageFailureCodes {
    http_retryable: &'static str,
    http_terminal: &'static str,
    transport_retryable: &'static str,
    decode_terminal: &'static str,
    config_terminal: &'static str,
}

/// Returns the failure-code set for a stage prefix, defaulting to `knowledge`.
fn stage_failure_codes(prefix: &str) -> StageFailureCodes {
    macro_rules! stage_codes {
        ($prefix:literal) => {
            StageFailureCodes {
                http_retryable: concat!($prefix, "_http_retryable"),
                http_terminal: concat!($prefix, "_http_terminal"),
                transport_retryable: concat!($prefix, "_transport_retryable"),
                decode_terminal: concat!($prefix, "_decode_terminal"),
                config_terminal: concat!($prefix, "_config_terminal"),
            }
        };
    }
    match prefix {
        "provider" => stage_codes!("provider"),
        "parser" => stage_codes!("parser"),
        "embedder" => stage_codes!("embedder"),
        "graph" => stage_codes!("graph"),
        _ => stage_codes!("knowledge"),
    }
}
