//! Shared Prometheus-backed runtime metrics helpers for MOA.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

use moa_core::{
    config::MetricsConfig, error::MoaError, error::Result, types::action_policy::ActionClass,
    types::action_policy::ActionPolicyEffect, types::action_policy::ActionReviewStatus,
    types::identifiers::ModelId, types::identifiers::TenantId,
    types::observability::genai_operation_name, types::observability::genai_provider_name,
    types::provider::ModelTier, types::session::SessionStatus,
};

// Sub-10ms buckets exist because turn steps like snapshot_load and
// pipeline_compile sit in the 1-20ms range at baseline (docs/18-performance.md);
// without them loadtest percentile reports quantize to a useless 10ms floor.
const LATENCY_BUCKETS: &[f64] = &[
    0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
];
const CACHE_HIT_RATE_BUCKETS: &[f64] = &[0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];
const GENAI_CLIENT_DURATION_BUCKETS: &[f64] = &[
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];
const GENAI_CLIENT_TOKEN_BUCKETS: &[f64] = &[
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];
const GENAI_CLIENT_TOKEN_USAGE_METRIC: &str = "gen_ai.client.token.usage";
const GENAI_CLIENT_OPERATION_DURATION_METRIC: &str = "gen_ai.client.operation.duration";
const GENAI_CLIENT_TIME_TO_FIRST_CHUNK_METRIC: &str = "gen_ai.client.operation.time_to_first_chunk";

/// Prometheus metric name for aggregate turn-step duration samples.
pub const TURN_STEP_DURATION_METRIC: &str = "moa_turn_step_duration_seconds";

/// Prometheus metric name for session event append transaction phase timings.
pub const SESSION_EVENT_APPEND_PHASE_METRIC: &str = "moa_session_event_append_phase_seconds";

/// Turn steps reported by the loadtest step-latency view.
pub const TURN_LATENCY_REPORT_STEPS: [TurnLatencyStep; 6] = [
    TurnLatencyStep::SnapshotLoad,
    TurnLatencyStep::SnapshotWrite,
    TurnLatencyStep::PipelineCompile,
    TurnLatencyStep::LlmCall,
    TurnLatencyStep::ToolDispatch,
    TurnLatencyStep::EventPersist,
];

/// Low-cardinality turn-latency step labels shared by metrics producers and loadtest consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnLatencyStep {
    /// Time spent loading a cached turn snapshot.
    SnapshotLoad,
    /// Time spent writing a refreshed turn snapshot.
    SnapshotWrite,
    /// Time spent compiling the context pipeline.
    PipelineCompile,
    /// Time spent in the main LLM call.
    LlmCall,
    /// Time spent dispatching tools.
    ToolDispatch,
    /// Time spent persisting turn events.
    EventPersist,
    /// Time to first streamed LLM token.
    LlmTtft,
}

impl TurnLatencyStep {
    /// All turn-latency steps in a stable order, used to pre-build cached metric handles.
    const ALL: [TurnLatencyStep; 7] = [
        Self::SnapshotLoad,
        Self::SnapshotWrite,
        Self::PipelineCompile,
        Self::LlmCall,
        Self::ToolDispatch,
        Self::EventPersist,
        Self::LlmTtft,
    ];

    /// Returns the stable Prometheus label for this turn step.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotLoad => "snapshot_load",
            Self::SnapshotWrite => "snapshot_write",
            Self::PipelineCompile => "pipeline_compile",
            Self::LlmCall => "llm_call",
            Self::ToolDispatch => "tool_dispatch",
            Self::EventPersist => "event_persist",
            Self::LlmTtft => "llm_ttft",
        }
    }

    /// Returns the dense index of this step into [`TurnLatencyStep::ALL`].
    const fn index(self) -> usize {
        match self {
            Self::SnapshotLoad => 0,
            Self::SnapshotWrite => 1,
            Self::PipelineCompile => 2,
            Self::LlmCall => 3,
            Self::ToolDispatch => 4,
            Self::EventPersist => 5,
            Self::LlmTtft => 6,
        }
    }
}

/// Low-cardinality phases inside one session event append operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEventAppendPhase {
    /// Pre-transaction payload encoding and claim-check preparation.
    Prepare,
    /// Wait for a pooled PostgreSQL connection.
    AcquireConnection,
    /// Start a transaction on an acquired connection.
    BeginTransaction,
    /// `sessions ... FOR UPDATE` lock acquisition and session metadata load.
    LockSession,
    /// Lookup of previously persisted idempotency keys.
    DedupeLookup,
    /// Fetch of original event rows for dedupe hits.
    DedupeFetchRecords,
    /// Local construction of multi-row insert arrays.
    BuildInsertPayloads,
    /// Multi-row insert into the append-only event table.
    InsertEvents,
    /// Multi-row insert into the dedupe table.
    InsertDedupeRows,
    /// Session aggregate counter update.
    UpdateSessionAggregates,
    /// Transaction commit, including Postgres commit wait.
    Commit,
}

impl SessionEventAppendPhase {
    /// All session event append phases in a stable order for cached metric handles.
    const ALL: [SessionEventAppendPhase; 11] = [
        Self::Prepare,
        Self::AcquireConnection,
        Self::BeginTransaction,
        Self::LockSession,
        Self::DedupeLookup,
        Self::DedupeFetchRecords,
        Self::BuildInsertPayloads,
        Self::InsertEvents,
        Self::InsertDedupeRows,
        Self::UpdateSessionAggregates,
        Self::Commit,
    ];

    /// Returns the stable Prometheus label for this append phase.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepare => "prepare",
            Self::AcquireConnection => "acquire_connection",
            Self::BeginTransaction => "begin_transaction",
            Self::LockSession => "lock_session",
            Self::DedupeLookup => "dedupe_lookup",
            Self::DedupeFetchRecords => "dedupe_fetch_records",
            Self::BuildInsertPayloads => "build_insert_payloads",
            Self::InsertEvents => "insert_events",
            Self::InsertDedupeRows => "insert_dedupe_rows",
            Self::UpdateSessionAggregates => "update_session_aggregates",
            Self::Commit => "commit",
        }
    }

    /// Returns the dense index of this phase into [`SessionEventAppendPhase::ALL`].
    const fn index(self) -> usize {
        match self {
            Self::Prepare => 0,
            Self::AcquireConnection => 1,
            Self::BeginTransaction => 2,
            Self::LockSession => 3,
            Self::DedupeLookup => 4,
            Self::DedupeFetchRecords => 5,
            Self::BuildInsertPayloads => 6,
            Self::InsertEvents => 7,
            Self::InsertDedupeRows => 8,
            Self::UpdateSessionAggregates => 9,
            Self::Commit => 10,
        }
    }
}

static PROMETHEUS_ENDPOINT: OnceLock<SocketAddr> = OnceLock::new();

/// Initializes the global Prometheus exporter when metrics are enabled.
pub fn init_metrics(config: &MetricsConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }

    if PROMETHEUS_ENDPOINT.get().is_none() {
        let addr = parse_metrics_listen_addr(config)?;
        let builder = PrometheusBuilder::new()
            .with_http_listener(addr)
            .set_buckets(LATENCY_BUCKETS)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?
            .set_buckets_for_metric(
                Matcher::Full("moa_cache_hit_rate".to_string()),
                CACHE_HIT_RATE_BUCKETS,
            )
            .map_err(|error| MoaError::ConfigError(error.to_string()))?
            .set_buckets_for_metric(
                Matcher::Full(GENAI_CLIENT_OPERATION_DURATION_METRIC.to_string()),
                GENAI_CLIENT_DURATION_BUCKETS,
            )
            .map_err(|error| MoaError::ConfigError(error.to_string()))?
            .set_buckets_for_metric(
                Matcher::Full(GENAI_CLIENT_TIME_TO_FIRST_CHUNK_METRIC.to_string()),
                GENAI_CLIENT_DURATION_BUCKETS,
            )
            .map_err(|error| MoaError::ConfigError(error.to_string()))?
            .set_buckets_for_metric(
                Matcher::Full(GENAI_CLIENT_TOKEN_USAGE_METRIC.to_string()),
                GENAI_CLIENT_TOKEN_BUCKETS,
            )
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;

        builder
            .install()
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        register_metric_descriptions();
        let _ = PROMETHEUS_ENDPOINT.set(addr);
    }

    Ok(())
}

/// Returns the configured scrape URL when the metrics listen address parses successfully.
#[must_use]
pub fn metrics_endpoint_url(config: &MetricsConfig) -> Option<String> {
    parse_metrics_listen_addr(config)
        .ok()
        .map(format_metrics_endpoint_url)
}

/// Records one created session.
///
/// The tenant is intentionally not a label: a per-tenant UUID would make `moa_sessions_total`
/// unbounded cardinality. Per-tenant session counts belong in the event store, not Prometheus.
pub fn record_session_created(_tenant_id: &TenantId, status: &SessionStatus) {
    counter!(
        "moa_sessions_total",
        "status" => session_status_label(status)
    )
    .increment(1);
}

/// Sets the current number of active sessions.
pub fn record_sessions_active(count: u64) {
    gauge!("moa_sessions_active").set(count as f64);
}

/// Records one completed assistant turn.
pub fn record_turn_completed(model: &ModelId, model_tier: ModelTier) {
    counter!(
        "moa_turns_total",
        "model" => model.to_string(),
        "model_tier" => model_tier.as_str()
    )
    .increment(1);
}

/// Records GenAI client operation duration.
pub fn record_genai_client_operation_duration(
    provider: &str,
    request_model: &str,
    response_model: Option<&str>,
    error_type: Option<&str>,
    duration: Duration,
) {
    let provider = genai_provider_name(provider).to_string();
    let operation = genai_operation_name(&provider).to_string();
    match (response_model, error_type) {
        (Some(response_model), Some(error_type)) => {
            histogram!(
                GENAI_CLIENT_OPERATION_DURATION_METRIC,
                "gen_ai.operation.name" => operation,
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => request_model.to_string(),
                "gen_ai.response.model" => response_model.to_string(),
                "error.type" => error_type.to_string()
            )
            .record(duration.as_secs_f64());
        }
        (Some(response_model), None) => {
            histogram!(
                GENAI_CLIENT_OPERATION_DURATION_METRIC,
                "gen_ai.operation.name" => operation,
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => request_model.to_string(),
                "gen_ai.response.model" => response_model.to_string()
            )
            .record(duration.as_secs_f64());
        }
        (None, Some(error_type)) => {
            histogram!(
                GENAI_CLIENT_OPERATION_DURATION_METRIC,
                "gen_ai.operation.name" => operation,
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => request_model.to_string(),
                "error.type" => error_type.to_string()
            )
            .record(duration.as_secs_f64());
        }
        (None, None) => {
            histogram!(
                GENAI_CLIENT_OPERATION_DURATION_METRIC,
                "gen_ai.operation.name" => operation,
                "gen_ai.provider.name" => provider,
                "gen_ai.request.model" => request_model.to_string()
            )
            .record(duration.as_secs_f64());
        }
    }
}

/// Records GenAI client token usage when provider-reported counts are available.
pub fn record_genai_client_token_usage(
    provider: &str,
    request_model: &str,
    response_model: &str,
    token_type: &str,
    tokens: u64,
) {
    if tokens == 0 {
        return;
    }

    histogram!(
        GENAI_CLIENT_TOKEN_USAGE_METRIC,
        "gen_ai.operation.name" => genai_operation_name(provider).to_string(),
        "gen_ai.provider.name" => genai_provider_name(provider).to_string(),
        "gen_ai.request.model" => request_model.to_string(),
        "gen_ai.response.model" => response_model.to_string(),
        "gen_ai.token.type" => token_type.to_string()
    )
    .record(tokens as f64);
}

/// Records time to first streamed GenAI response chunk.
pub fn record_genai_client_time_to_first_chunk(
    provider: &str,
    request_model: &str,
    response_model: &str,
    duration: Duration,
) {
    histogram!(
        GENAI_CLIENT_TIME_TO_FIRST_CHUNK_METRIC,
        "gen_ai.operation.name" => genai_operation_name(provider).to_string(),
        "gen_ai.provider.name" => genai_provider_name(provider).to_string(),
        "gen_ai.request.model" => request_model.to_string(),
        "gen_ai.response.model" => response_model.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records the ratio of input tokens that were served from cache for one request.
pub fn record_cache_hit_rate(provider: &str, model: &str, ratio: f64) {
    histogram!(
        "moa_cache_hit_rate",
        "gen_ai.provider.name" => genai_provider_name(provider).to_string(),
        "gen_ai.request.model" => model.to_string()
    )
    .record(ratio.clamp(0.0, 1.0));
}

/// Records one LLM completion cost sample in cents.
pub fn record_llm_cost_cents(provider: &str, model: &str, cost_cents: u64) {
    if cost_cents == 0 {
        return;
    }

    counter!(
        "moa_llm_cost_cents_total",
        "gen_ai.provider.name" => genai_provider_name(provider).to_string(),
        "gen_ai.request.model" => model.to_string()
    )
    .increment(cost_cents);
}

/// Records one session-level error that should appear on operational dashboards.
pub fn record_session_error(scope: &str) {
    counter!(
        "moa_session_errors_total",
        "scope" => scope.to_string()
    )
    .increment(1);
}

/// Records one tool call completion and its latency.
pub fn record_tool_call(tool_name: &str, status: &str, duration: Duration) {
    let tool_name = tool_name_label(tool_name);
    counter!(
        "moa_tool_calls_total",
        "tool_name" => tool_name,
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        "moa_tool_call_duration_seconds",
        "tool_name" => tool_name
    )
    .record(duration.as_secs_f64());
}

/// Records one classified tool execution failure.
pub fn record_tool_failure(provider: &str, tool_name: &str, class: &str) {
    counter!(
        "moa_tool_failure_total",
        "class" => class.to_string(),
        "provider" => provider.to_string(),
        "tool" => tool_name_label(tool_name)
    )
    .increment(1);
}

/// Records one automatic sandbox re-provision.
pub fn record_tool_reprovision(provider: &str) {
    counter!(
        "moa_tool_reprovision_total",
        "provider" => provider.to_string()
    )
    .increment(1);
}

/// Records one automatic in-place retry attempt.
pub fn record_tool_retry(provider: &str, attempt: u32) {
    counter!(
        "moa_tool_retry_total",
        "attempt" => attempt.to_string(),
        "provider" => provider.to_string()
    )
    .increment(1);
}

/// Records one tool-output truncation event.
pub fn record_tool_output_truncated_metric(tool_name: &str) {
    counter!(
        "moa_tool_output_truncated_total",
        "tool_name" => tool_name_label(tool_name)
    )
    .increment(1);
}

/// Records one end-to-end turn latency sample.
pub fn record_turn_latency(duration: Duration) {
    histogram!("moa_turn_latency_seconds").record(duration.as_secs_f64());
}

/// Records one aggregate turn-step duration sample.
///
/// The per-step histogram handles are resolved once (on first record after the recorder is
/// installed) and cached, so the hot per-turn path avoids re-resolving a metric handle on every
/// call. The `step` label is one of a fixed, bounded set.
pub fn record_turn_step_duration(step: TurnLatencyStep, duration: Duration) {
    static TURN_STEP_HISTOGRAMS: OnceLock<[metrics::Histogram; 7]> = OnceLock::new();
    let histograms = TURN_STEP_HISTOGRAMS.get_or_init(|| {
        TurnLatencyStep::ALL
            .map(|step| histogram!(TURN_STEP_DURATION_METRIC, "step" => step.as_str()))
    });
    histograms[step.index()].record(duration.as_secs_f64());
}

/// Records one terminal turn-workflow outcome.
///
/// End-to-end workflow latency is already covered by `moa_turn_latency_seconds`;
/// this counter only tracks terminal outcome counts by scope, result, and tier.
pub fn record_turn_workflow_outcome(scope: &str, result: &str, model_tier: ModelTier) {
    counter!(
        "moa_turn_outcomes_total",
        "scope" => scope.to_string(),
        "result" => result.to_string(),
        "model_tier" => model_tier.as_str()
    )
    .increment(1);
}

/// Records one query-rewrite gate outcome.
pub fn record_query_rewrite_decision(decision: &str, reason: &str, llm_called: bool) {
    let llm_called = if llm_called { "true" } else { "false" };
    counter!(
        "moa_query_rewrite_decisions_total",
        "decision" => decision.to_string(),
        "reason" => reason.to_string(),
        "llm_called" => llm_called.to_string()
    )
    .increment(1);
}

/// Records one sandbox provisioning duration sample.
pub fn record_sandbox_provision_duration(provider: &str, tier: &str, duration: Duration) {
    histogram!(
        "moa_sandbox_provision_seconds",
        "provider" => provider.to_string(),
        "tier" => tier.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one appended session event, labeled by event type.
pub fn record_session_event_append(event_type: &str) {
    counter!(
        "moa_session_events_appended_total",
        "event_type" => event_type.to_string()
    )
    .increment(1);
}

/// Records one duration sample for a bounded session event append phase.
pub fn record_session_event_append_phase_duration(
    phase: SessionEventAppendPhase,
    duration: Duration,
) {
    static APPEND_PHASE_HISTOGRAMS: OnceLock<[metrics::Histogram; 11]> = OnceLock::new();
    let histograms = APPEND_PHASE_HISTOGRAMS.get_or_init(|| {
        SessionEventAppendPhase::ALL
            .map(|phase| histogram!(SESSION_EVENT_APPEND_PHASE_METRIC, "phase" => phase.as_str()))
    });
    histograms[phase.index()].record(duration.as_secs_f64());
}

/// Records the number of events returned by one session event load operation.
pub fn record_session_event_load(event_count: u64) {
    counter!("moa_session_event_load_events_total").increment(event_count);
}

/// Records one tool idempotency scan and the number of prior events scanned.
pub fn record_tool_idempotency_scan(event_type: &str, scanned_events: u64) {
    counter!(
        "moa_tool_idempotency_scan_events_total",
        "event_type" => event_type.to_string()
    )
    .increment(scanned_events);
}

/// Records one memory service operation.
pub fn record_memory_operation(
    operation: &str,
    status: &str,
    result_count: u64,
    duration: Duration,
) {
    counter!(
        "moa_memory_operations_total",
        "operation" => operation.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        "moa_memory_operation_duration_seconds",
        "operation" => operation.to_string(),
        "status" => status.to_string()
    )
    .record(duration.as_secs_f64());
    counter!(
        "moa_memory_operation_results_total",
        "operation" => operation.to_string(),
        "status" => status.to_string()
    )
    .increment(result_count);
}

/// Records one tenant knowledge sync-run lifecycle observation.
pub fn record_knowledge_sync_run(provider: &str, status: &str) {
    counter!(
        "moa_knowledge_sync_runs_total",
        "provider" => knowledge_metric_label(provider),
        "status" => knowledge_metric_label(status)
    )
    .increment(1);
}

/// Records tenant knowledge retrieval stage duration.
pub fn record_knowledge_retrieval_duration(stage: &str, status: &str, duration: Duration) {
    histogram!(
        "moa_knowledge_retrieval_duration_seconds",
        "stage" => knowledge_metric_label(stage),
        "status" => knowledge_metric_label(status)
    )
    .record(duration.as_secs_f64());
}

/// Records tenant knowledge retrieval hit contribution by source tier and leg.
pub fn record_knowledge_retrieval_hits(source_tier: &str, leg: &str, count: u64) {
    if count == 0 {
        return;
    }

    counter!(
        "moa_knowledge_retrieval_hits_total",
        "source_tier" => knowledge_metric_label(source_tier),
        "leg" => knowledge_metric_label(leg)
    )
    .increment(count);
}

/// Records the time spent validating an API key.
pub fn record_api_key_validation_duration(result: &str, duration: Duration) {
    histogram!(
        "moa_api_key_validation_seconds",
        "result" => result.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one experiment run lifecycle observation.
pub fn record_experiment_run(status: &str, target_kind: &str) {
    counter!(
        "moa_experiment_runs_total",
        "status" => status.to_string(),
        "target_kind" => target_kind.to_string()
    )
    .increment(1);
}

/// Records one experiment trial lifecycle observation.
pub fn record_experiment_trial(status: &str, stop_reason: Option<&str>, target_kind: &str) {
    counter!(
        "moa_experiment_trials_total",
        "status" => status.to_string(),
        "stop_reason" => stop_reason.unwrap_or("none").to_string(),
        "target_kind" => target_kind.to_string()
    )
    .increment(1);
}

/// Records one simulator turn submitted to a target.
pub fn record_simulation_turn(target_kind: &str) {
    counter!(
        "moa_simulation_turns_total",
        "target_kind" => target_kind.to_string()
    )
    .increment(1);
}

/// Records simulation token usage for a bounded participant role.
pub fn record_simulation_tokens(role: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }

    counter!(
        "moa_simulation_tokens_total",
        "role" => role.to_string()
    )
    .increment(tokens);
}

/// Records simulation model cost for a bounded participant role.
pub fn record_simulation_cost_cents(role: &str, cost_cents: u64) {
    if cost_cents == 0 {
        return;
    }

    counter!(
        "moa_simulation_cost_cents_total",
        "role" => role.to_string()
    )
    .increment(cost_cents);
}

/// Records score rows read from an experiment scoring surface.
pub fn record_experiment_score_rows(source: &str, rows: u64) {
    if rows == 0 {
        return;
    }

    counter!(
        "moa_experiment_score_rows_total",
        "source" => source.to_string()
    )
    .increment(rows);
}

/// Records learning candidates proposed from experiment evidence.
pub fn record_experiment_learning_candidates(status: &str, count: u64) {
    if count == 0 {
        return;
    }

    counter!(
        "moa_experiment_learning_candidates_total",
        "status" => status.to_string()
    )
    .increment(count);
}

/// Records one filed skill-learning candidate, labeled by source stage and kind.
///
/// `source` is the loop stage that filed it (`distilled`, `recurrence_mined`,
/// `mined`, or `rollback_monitor`); `kind` is the bounded operation within that
/// source (`created`, `improved`, `resynthesized`, `weakness`, or `regression`).
/// The `resynthesized` kind marks a distilled dedupe-hit that rewrote an open
/// draft; `recurrence_mined` marks a candidate filed because a task fingerprint
/// recurred across sessions rather than one session clearing the dispatch gate.
/// A zero count is a no-op so a filing pass that filed nothing never adds a
/// metric series.
pub fn record_skill_learning_candidates_filed(source: &str, kind: &str, count: u64) {
    if count == 0 {
        return;
    }

    counter!(
        "moa_skill_learning_candidates_filed_total",
        "source" => source.to_string(),
        "kind" => kind.to_string()
    )
    .increment(count);
}

/// Records one skill-learning review decision, labeled by action and outcome.
///
/// `action` is the review endpoint (`accept_skill`, `accept_rollback`, or
/// `reject`); `outcome` is its terminal result (`promoted`, `gate_rejected`,
/// `rejected`, or `error`).
pub fn record_skill_learning_review_decision(action: &str, outcome: &str) {
    counter!(
        "moa_skill_learning_review_decisions_total",
        "action" => action.to_string(),
        "outcome" => outcome.to_string()
    )
    .increment(1);
}

/// Records that action policy queued a tenant-admin review.
pub fn record_action_review_requested(effect: ActionPolicyEffect, action_class: ActionClass) {
    counter!(
        "moa_action_review_requests_total",
        "effect" => effect.as_str(),
        "action_class" => action_class.as_str()
    )
    .increment(1);
}

/// Records a tenant-admin action-review decision.
pub fn record_action_review_decision(status: ActionReviewStatus, action_class: ActionClass) {
    counter!(
        "moa_action_review_decisions_total",
        "status" => status.as_str(),
        "action_class" => action_class.as_str()
    )
    .increment(1);
}

/// Records how long a tenant action review waited before an admin decided it.
///
/// Observed at decision time as `decided_at - created_at`; labeled by action
/// class only so cardinality stays bounded. The tenant is intentionally not a
/// label.
pub fn record_approval_wait(action_class: ActionClass, wait: Duration) {
    histogram!(
        "moa_approval_wait_seconds",
        "action_class" => action_class.as_str()
    )
    .record(wait.as_secs_f64());
}

/// Canonical risk-level labels for the pending action-review depth gauge.
///
/// Every known label is reset each sample so a drained risk class reports zero
/// instead of holding its last non-zero value.
const ACTION_REVIEW_RISK_LEVELS: [&str; 3] = ["low", "medium", "high"];

/// Publishes the pending tenant action-review queue depth by bounded risk level.
///
/// `depth_by_risk` carries only the currently non-empty risk classes; the other
/// canonical labels are set to zero so the gauge never reports a stale backlog.
pub fn record_action_review_pending_depth(depth_by_risk: &[(String, i64)]) {
    for risk in ACTION_REVIEW_RISK_LEVELS {
        gauge!("moa_action_review_pending", "risk_level" => risk).set(0.0);
    }
    for (risk, depth) in depth_by_risk {
        gauge!("moa_action_review_pending", "risk_level" => risk.clone()).set(*depth as f64);
    }
}

/// Publishes the age in seconds of the oldest pending tenant action review.
pub fn record_action_review_oldest_pending_age(age_seconds: f64) {
    gauge!("moa_action_review_oldest_pending_age_seconds").set(age_seconds);
}

/// Publishes the pending builtin async-authorization approval queue depth.
pub fn record_builtin_approval_pending_depth(depth: u64) {
    gauge!("moa_builtin_approval_pending").set(depth as f64);
}

/// Publishes the age in seconds of the oldest pending builtin approval.
pub fn record_builtin_approval_oldest_pending_age(age_seconds: f64) {
    gauge!("moa_builtin_approval_oldest_pending_age_seconds").set(age_seconds);
}

/// Records a terminal builtin approval decision by bounded status.
///
/// `status` is one of `approved`, `denied`, or `timeout`.
pub fn record_builtin_approval_decision(status: &'static str) {
    counter!("moa_builtin_approval_decisions_total", "status" => status).increment(1);
}

/// Records how long a builtin approval waited from creation to decision.
pub fn record_builtin_approval_wait(wait: Duration) {
    histogram!("moa_builtin_approval_wait_seconds").record(wait.as_secs_f64());
}

fn parse_metrics_listen_addr(config: &MetricsConfig) -> Result<SocketAddr> {
    config.listen.parse::<SocketAddr>().map_err(|error| {
        MoaError::ConfigError(format!(
            "invalid metrics.listen `{}`: {error}",
            config.listen
        ))
    })
}

fn format_metrics_endpoint_url(addr: SocketAddr) -> String {
    let host = match addr.ip() {
        IpAddr::V4(ip) if ip == Ipv4Addr::UNSPECIFIED => "localhost".to_string(),
        IpAddr::V6(ip) if ip == Ipv6Addr::UNSPECIFIED => "localhost".to_string(),
        ip => ip.to_string(),
    };
    format!("http://{host}:{}/metrics", addr.port())
}

/// Built-in tool names that are safe to use verbatim as a metric label.
///
/// Every other tool name (tenant- or MCP-defined) is bucketed as `"other"` by
/// [`tool_name_label`] so tool metrics keep bounded cardinality.
const BUILTIN_TOOL_NAMES: &[&str] = &[
    "bash",
    "file_read",
    "file_write",
    "file_search",
    "file_outline",
    "grep",
    "str_replace",
    "memory_remember",
    "memory_forget",
    "memory_supersede",
    "memory_search",
    "session_search",
    "tool_result_read",
    "tool_result_search",
    "spawn_worker",
    "wait_worker",
    "message_worker",
    "list_workers",
    "cancel_worker",
    "provide_worker_input",
    "report_to_parent",
    "request_input",
];

/// Returns a bounded tool-name label: the name itself when it is a known built-in, else `"other"`.
fn tool_name_label(tool_name: &str) -> &'static str {
    BUILTIN_TOOL_NAMES
        .iter()
        .copied()
        .find(|builtin| *builtin == tool_name)
        .unwrap_or("other")
}

fn knowledge_metric_label(value: &str) -> String {
    let normalized = value
        .chars()
        .take(48)
        .map(|ch| match ch {
            'a'..='z' | '0'..='9' | '_' => ch,
            'A'..='Z' => ch.to_ascii_lowercase(),
            '-' | '.' | '/' | ' ' => '_',
            _ => '_',
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    if normalized.is_empty() {
        "unknown".to_string()
    } else {
        normalized
    }
}

#[cfg(test)]
fn knowledge_metric_names() -> &'static [&'static str] {
    &[
        "moa_knowledge_sync_runs_total",
        "moa_knowledge_records_total",
        "moa_knowledge_ingestion_step_duration_seconds",
        "moa_knowledge_parse_jobs_total",
        "moa_knowledge_chunks_total",
        "moa_knowledge_embeddings_total",
        "moa_knowledge_graph_writes_total",
        "moa_knowledge_retrieval_duration_seconds",
        "moa_knowledge_retrieval_hits_total",
    ]
}

fn register_metric_descriptions() {
    describe_gauge!("moa_sessions_active", "Currently active MOA sessions.");
    describe_counter!(
        "moa_sessions_total",
        "Total sessions created, labeled by initial status."
    );
    describe_counter!(
        "moa_turns_total",
        "Total assistant turns completed, labeled by model and routing tier."
    );
    describe_histogram!(
        GENAI_CLIENT_OPERATION_DURATION_METRIC,
        "GenAI client operation duration in seconds."
    );
    describe_histogram!(
        GENAI_CLIENT_TIME_TO_FIRST_CHUNK_METRIC,
        "Time to first streamed GenAI response chunk in seconds."
    );
    describe_histogram!(
        GENAI_CLIENT_TOKEN_USAGE_METRIC,
        "Provider-reported GenAI client token usage."
    );
    describe_counter!(
        "moa_tool_calls_total",
        "Total tool calls completed, labeled by tool name and status."
    );
    describe_counter!(
        "moa_tool_failure_total",
        "Total classified tool execution failures, labeled by class, provider, and tool."
    );
    describe_counter!(
        "moa_tool_reprovision_total",
        "Total automatic sandbox re-provisions, labeled by provider."
    );
    describe_counter!(
        "moa_tool_retry_total",
        "Total automatic in-place tool retries, labeled by provider and retry attempt."
    );
    describe_counter!(
        "moa_session_errors_total",
        "Total session-scoped error events surfaced by the orchestrator."
    );
    describe_counter!(
        "moa_llm_cost_cents_total",
        "Total LLM completion cost in cents."
    );
    describe_counter!(
        "moa_tool_output_truncated_total",
        "Number of successful tool calls whose outputs were truncated."
    );
    describe_counter!(
        "moa_compaction_tier_applied_total",
        "Number of times each compaction tier was applied."
    );
    describe_counter!(
        "moa_lineage_dropped_total",
        "Lineage events dropped because the hot-path channel was saturated."
    );
    describe_counter!(
        "moa_lineage_enqueued_total",
        "Lineage events accepted onto the hot-path channel toward the durable journal."
    );
    describe_counter!(
        "moa_lineage_flushed_total",
        "Lineage rows flushed from the durable journal into Postgres."
    );
    describe_gauge!(
        "moa_lineage_journal_depth",
        "Approximate lineage events pending in the durable journal."
    );
    describe_histogram!(
        "moa_turn_latency_seconds",
        "End-to-end turn latency in seconds."
    );
    describe_histogram!(
        TURN_STEP_DURATION_METRIC,
        "Aggregate per-turn step duration in seconds, labeled by documented turn step."
    );
    describe_counter!(
        "moa_turn_outcomes_total",
        "Terminal turn workflow outcomes, labeled by scope, result, and model tier."
    );
    describe_histogram!(
        "moa_tool_call_duration_seconds",
        "Tool execution duration in seconds."
    );
    describe_counter!(
        "moa_query_rewrite_decisions_total",
        "Query rewrite gate decisions, labeled by decision, reason, and LLM-call status."
    );
    describe_histogram!(
        "moa_sandbox_provision_seconds",
        "Sandbox provisioning duration in seconds."
    );
    describe_histogram!(
        "moa_cache_hit_rate",
        "Ratio of cached input tokens to total input tokens for one request."
    );
    describe_counter!(
        "moa_session_events_appended_total",
        "Session events appended to the durable event log, labeled by event type."
    );
    describe_histogram!(
        SESSION_EVENT_APPEND_PHASE_METRIC,
        "Session event append duration in seconds, labeled by bounded transaction phase."
    );
    describe_counter!(
        "moa_session_event_load_events_total",
        "Session events returned by durable event log load operations."
    );
    describe_counter!(
        "moa_tool_idempotency_scan_events_total",
        "Prior session events inspected by tool idempotency scans, labeled by event type."
    );
    describe_histogram!(
        "moa_api_key_validation_seconds",
        "API-key validation duration in seconds, labeled by result."
    );
    describe_counter!(
        "moa_memory_operations_total",
        "Memory service operations, labeled by operation and status."
    );
    describe_histogram!(
        "moa_memory_operation_duration_seconds",
        "Memory service operation duration in seconds, labeled by operation and status."
    );
    describe_counter!(
        "moa_memory_operation_results_total",
        "Memory service result rows returned, labeled by operation and status."
    );
    describe_counter!(
        "moa_knowledge_sync_runs_total",
        "Tenant knowledge sync-run lifecycle outcomes, labeled by provider and status."
    );
    describe_counter!(
        "moa_knowledge_records_total",
        "Tenant knowledge provider records observed, labeled by provider and action."
    );
    describe_histogram!(
        "moa_knowledge_ingestion_step_duration_seconds",
        "Tenant knowledge ingestion step duration in seconds, labeled by provider, parser, stage, and status."
    );
    describe_counter!(
        "moa_knowledge_parse_jobs_total",
        "Tenant knowledge parse job outcomes, labeled by parser and status."
    );
    describe_counter!(
        "moa_knowledge_chunks_total",
        "Tenant knowledge chunk actions, labeled by action."
    );
    describe_counter!(
        "moa_knowledge_embeddings_total",
        "Tenant knowledge embedding outcomes, labeled by status."
    );
    describe_counter!(
        "moa_knowledge_graph_writes_total",
        "Tenant knowledge graph write outcomes, labeled by write kind and status."
    );
    describe_histogram!(
        "moa_knowledge_retrieval_duration_seconds",
        "Tenant knowledge retrieval stage duration in seconds, labeled by stage and status."
    );
    describe_counter!(
        "moa_knowledge_retrieval_hits_total",
        "Tenant knowledge retrieval hits, labeled by source tier and retrieval leg."
    );
    describe_counter!(
        "moa_experiment_runs_total",
        "Experiment run lifecycle observations, labeled by terminal status and bounded target kind."
    );
    describe_counter!(
        "moa_experiment_trials_total",
        "Experiment trial lifecycle observations, labeled by status, bounded stop reason, and bounded target kind."
    );
    describe_counter!(
        "moa_simulation_turns_total",
        "Simulator turns submitted to experiment targets, labeled by bounded target kind."
    );
    describe_counter!(
        "moa_simulation_tokens_total",
        "Simulation token usage, labeled by bounded participant role."
    );
    describe_counter!(
        "moa_simulation_cost_cents_total",
        "Simulation model cost in cents, labeled by bounded participant role."
    );
    describe_counter!(
        "moa_experiment_score_rows_total",
        "Experiment score rows read by service surfaces, labeled by bounded source."
    );
    describe_counter!(
        "moa_experiment_learning_candidates_total",
        "Experiment learning candidates proposed, labeled by candidate status."
    );
    describe_counter!(
        "moa_skill_learning_candidates_filed_total",
        "Skill-learning candidates filed for review, labeled by source loop stage and bounded operation kind."
    );
    describe_counter!(
        "moa_skill_learning_review_decisions_total",
        "Skill-learning review decisions, labeled by review action and terminal outcome."
    );
    describe_counter!(
        "moa_action_review_requests_total",
        "Action reviews requested by policy evaluation, labeled by effect and action class."
    );
    describe_counter!(
        "moa_action_review_decisions_total",
        "Action review decisions, labeled by status and action class."
    );
    describe_histogram!(
        "moa_approval_wait_seconds",
        "Tenant action-review wait from creation to admin decision in seconds, labeled by action class."
    );
    describe_gauge!(
        "moa_action_review_pending",
        "Pending tenant action reviews awaiting an admin decision, labeled by risk level."
    );
    describe_gauge!(
        "moa_action_review_oldest_pending_age_seconds",
        "Age in seconds of the oldest pending tenant action review."
    );
    describe_gauge!(
        "moa_builtin_approval_pending",
        "Pending builtin async-authorization approvals awaiting a decision."
    );
    describe_gauge!(
        "moa_builtin_approval_oldest_pending_age_seconds",
        "Age in seconds of the oldest pending builtin async-authorization approval."
    );
    describe_counter!(
        "moa_builtin_approval_decisions_total",
        "Terminal builtin approval decisions, labeled by status (approved/denied/timeout)."
    );
    describe_histogram!(
        "moa_builtin_approval_wait_seconds",
        "Builtin approval wait from creation to decision in seconds."
    );
}

fn session_status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Created => "created",
        SessionStatus::Running => "running",
        SessionStatus::Paused => "paused",
        SessionStatus::Completed => "completed",
        SessionStatus::Cancelled => "cancelled",
        SessionStatus::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;
    use tokio::time::{Instant, sleep};

    use super::*;

    #[test]
    fn tool_name_label_buckets_unknown_tools_as_other() {
        // Pins: built-in tool names pass through as metric labels; tenant/MCP-defined names bucket
        // to "other" so tool metric cardinality stays bounded.
        assert_eq!(tool_name_label("bash"), "bash");
        assert_eq!(tool_name_label("spawn_worker"), "spawn_worker");
        assert_eq!(tool_name_label("memory_search"), "memory_search");
        assert_eq!(tool_name_label("acme_customer_lookup"), "other");
        assert_eq!(tool_name_label(""), "other");
    }

    #[test]
    fn tool_and_session_metrics_use_bounded_labels() {
        // Pins: tool metrics bucket unknown tool names to "other" and moa_sessions_total carries
        // no per-tenant label. Asserted against rendered Prometheus output.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            record_tool_call("bash", "ok", Duration::from_millis(1));
            record_tool_call("acme_customer_lookup", "ok", Duration::from_millis(1));
            record_tool_failure("mock", "acme_customer_lookup", "transient");
            record_session_created(&TenantId::new(), &SessionStatus::Created);
        });
        let rendered = handle.render();

        assert!(
            rendered.contains("tool_name=\"bash\""),
            "built-in tool name should pass through:\n{rendered}"
        );
        assert!(
            rendered.contains("tool_name=\"other\""),
            "unknown tool name should bucket to other:\n{rendered}"
        );
        assert!(
            !rendered.contains("acme_customer_lookup"),
            "raw tenant/MCP tool name must never appear as a label:\n{rendered}"
        );

        let session_series = rendered
            .lines()
            .filter(|line| line.starts_with("moa_sessions_total"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !session_series.is_empty(),
            "sessions metric should render:\n{rendered}"
        );
        assert!(
            !session_series.contains("tenant="),
            "moa_sessions_total must not carry a per-tenant label:\n{session_series}"
        );
    }

    #[test]
    fn metrics_endpoint_url_uses_localhost_for_unspecified_listener() {
        let url = metrics_endpoint_url(&MetricsConfig {
            enabled: true,
            listen: "0.0.0.0:9090".to_string(),
        });

        assert_eq!(url.as_deref(), Some("http://localhost:9090/metrics"));
    }

    #[tokio::test]
    async fn prometheus_endpoint_exports_recorded_metric_families() {
        let port = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind ephemeral test port")
            .local_addr()
            .expect("local addr")
            .port();
        let config = MetricsConfig {
            enabled: true,
            listen: format!("127.0.0.1:{port}"),
        };
        init_metrics(&config).expect("metrics exporter should initialize");

        record_genai_client_operation_duration(
            "mock",
            "gpt-5.4",
            Some("gpt-5.4"),
            None,
            Duration::from_millis(20),
        );
        record_genai_client_token_usage("mock", "gpt-5.4", "gpt-5.4", "input", 8);
        record_genai_client_token_usage("mock", "gpt-5.4", "gpt-5.4", "output", 4);
        record_genai_client_time_to_first_chunk(
            "mock",
            "gpt-5.4",
            "gpt-5.4",
            Duration::from_millis(5),
        );
        record_cache_hit_rate("mock", "gpt-5.4", 0.5);
        record_turn_latency(Duration::from_millis(25));
        record_turn_step_duration(TurnLatencyStep::PipelineCompile, Duration::from_millis(10));
        record_session_event_append("ToolCall");
        record_session_event_append_phase_duration(
            SessionEventAppendPhase::AcquireConnection,
            Duration::from_millis(2),
        );
        record_session_event_append_phase_duration(
            SessionEventAppendPhase::BeginTransaction,
            Duration::from_millis(1),
        );
        record_session_event_append_phase_duration(
            SessionEventAppendPhase::LockSession,
            Duration::from_millis(3),
        );
        record_session_event_load(2);
        record_tool_idempotency_scan("ToolResult", 5);
        record_api_key_validation_duration("failure", Duration::from_millis(6));
        record_memory_operation("search", "ok", 4, Duration::from_millis(2));
        record_experiment_run("accepted", "agent_loop");
        record_experiment_trial("completed", Some("max_turns"), "agent_loop");
        record_simulation_turn("agent_loop");
        record_simulation_tokens("simulator", 16);
        record_simulation_cost_cents("simulator", 1);
        record_experiment_score_rows("scores", 3);
        record_experiment_learning_candidates("proposed", 1);
        record_skill_learning_candidates_filed("distilled", "created", 1);
        record_skill_learning_review_decision("accept_skill", "promoted");
        record_action_review_requested(ActionPolicyEffect::AdminReview, ActionClass::LocalWrite);
        record_action_review_decision(ActionReviewStatus::Cleared, ActionClass::LocalWrite);
        record_action_review_decision(ActionReviewStatus::Timeout, ActionClass::CommandExecution);
        record_approval_wait(ActionClass::LocalWrite, Duration::from_secs(30));
        record_action_review_pending_depth(&[("high".to_string(), 2), ("low".to_string(), 1)]);
        record_action_review_oldest_pending_age(42.0);
        record_builtin_approval_pending_depth(3);
        record_builtin_approval_oldest_pending_age(7.0);
        record_builtin_approval_decision("timeout");
        record_builtin_approval_wait(Duration::from_secs(12));

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("http client");
        let url = metrics_endpoint_url(&config).expect("metrics url");
        let deadline = Instant::now() + Duration::from_secs(5);
        let scrape = loop {
            match client.get(&url).send().await {
                Ok(response) if response.status().is_success() => {
                    break response.text().await.expect("scrape body");
                }
                Ok(_) | Err(_) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(50)).await;
                }
                Ok(response) => panic!("unexpected scrape status: {}", response.status()),
                Err(error) => panic!("metrics scrape failed: {error}"),
            }
        };

        assert!(scrape.contains("gen_ai_client_operation_duration"));
        assert!(scrape.contains("gen_ai_client_token_usage"));
        assert!(scrape.contains("gen_ai_client_operation_time_to_first_chunk"));
        assert!(scrape.contains("moa_cache_hit_rate"));
        assert!(scrape.contains("moa_turn_latency_seconds"));
        assert!(scrape.contains("moa_turn_step_duration_seconds"));
        assert!(scrape.contains("moa_session_events_appended_total"));
        assert!(scrape.contains("moa_session_event_append_phase_seconds"));
        assert!(scrape.contains("phase=\"acquire_connection\""));
        assert!(scrape.contains("phase=\"begin_transaction\""));
        assert!(scrape.contains("moa_session_event_load_events_total"));
        assert!(scrape.contains("moa_tool_idempotency_scan_events_total"));
        assert!(scrape.contains("moa_memory_operation_results_total"));
        assert!(scrape.contains("moa_api_key_validation_seconds"));
        assert!(scrape.contains("moa_experiment_runs_total"));
        assert!(scrape.contains("moa_experiment_trials_total"));
        assert!(scrape.contains("moa_simulation_turns_total"));
        assert!(scrape.contains("moa_simulation_tokens_total"));
        assert!(scrape.contains("moa_simulation_cost_cents_total"));
        assert!(scrape.contains("moa_experiment_score_rows_total"));
        assert!(scrape.contains("moa_experiment_learning_candidates_total"));
        assert!(scrape.contains("moa_skill_learning_candidates_filed_total"));
        assert!(scrape.contains("moa_skill_learning_review_decisions_total"));
        assert!(scrape.contains("moa_action_review_requests_total"));
        assert!(scrape.contains("moa_action_review_decisions_total"));
        assert!(scrape.contains("status=\"timeout\""));
        assert!(scrape.contains("moa_approval_wait_seconds"));
        assert!(scrape.contains("moa_action_review_pending"));
        assert!(scrape.contains("risk_level=\"high\""));
        // A drained risk class is still emitted (reset to zero) rather than
        // dropped, so it never holds a stale backlog value.
        assert!(scrape.contains("risk_level=\"medium\""));
        assert!(scrape.contains("moa_action_review_oldest_pending_age_seconds"));
        assert!(scrape.contains("moa_builtin_approval_pending"));
        assert!(scrape.contains("moa_builtin_approval_oldest_pending_age_seconds"));
        assert!(scrape.contains("moa_builtin_approval_decisions_total"));
        assert!(scrape.contains("moa_builtin_approval_wait_seconds"));

        // Duplicate/dead metric families removed by the observability pruning must
        // not reappear in the exporter output.
        assert!(!scrape.contains("moa_session_event_loads_total"));
        assert!(!scrape.contains("moa_context_pipeline_construction_seconds"));
        assert!(!scrape.contains("moa_retrieval_embedder_construction_seconds"));
        assert!(!scrape.contains("moa_tool_idempotency_scan_seconds"));
        assert!(!scrape.contains("moa_experiment_trial_duration_seconds"));
        assert!(!scrape.contains("moa_skill_learning_time_in_review_seconds"));
    }

    #[test]
    fn experiment_metrics_export_descriptions_and_bounded_labels() {
        // Pins: every experiment/simulation metric exports a HELP description and only
        // bounded dashboard labels. Asserted against rendered Prometheus output (the real
        // exported descriptors), not the crate's own source text.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            record_experiment_run("accepted", "agent_loop");
            record_experiment_trial("completed", Some("max_turns"), "agent_loop");
            record_simulation_turn("agent_loop");
            record_simulation_tokens("simulator", 16);
            record_simulation_cost_cents("simulator", 1);
            record_experiment_score_rows("scores", 3);
            record_experiment_learning_candidates("proposed", 1);
        });
        let rendered = handle.render();

        let experiment_metrics = [
            "moa_experiment_runs_total",
            "moa_experiment_trials_total",
            "moa_simulation_turns_total",
            "moa_simulation_tokens_total",
            "moa_simulation_cost_cents_total",
            "moa_experiment_score_rows_total",
            "moa_experiment_learning_candidates_total",
        ];
        for metric in experiment_metrics {
            assert!(
                rendered.contains(&format!("# HELP {metric} ")),
                "metric {metric} should export a HELP description; rendered:\n{rendered}"
            );
        }

        // Bounded labels that SHOULD appear on the exported series.
        for label in [
            "status=",
            "target_kind=",
            "stop_reason=",
            "source=",
            "role=",
        ] {
            assert!(
                rendered.contains(label),
                "expected bounded experiment label `{label}` in rendered output:\n{rendered}"
            );
        }

        let experiment_series = rendered
            .lines()
            .filter(|line| {
                experiment_metrics
                    .iter()
                    .any(|metric| line.starts_with(metric))
            })
            .collect::<Vec<_>>();
        assert!(
            !experiment_series.is_empty(),
            "experiment metric series should be exported:\n{rendered}"
        );
        for forbidden in [
            "run_uid",
            "trial_uid",
            "session_id",
            "procedure_run_uid",
            "score_run_id",
            "trial_key",
            "artifact_revision",
            "prompt",
            "profile",
            "persona",
            "scenario",
            "transcript",
            "connector",
            "model_output",
        ] {
            for line in &experiment_series {
                assert!(
                    !line.contains(forbidden),
                    "experiment series `{line}` must not carry high-cardinality label `{forbidden}`"
                );
            }
        }
    }

    #[test]
    fn skill_learning_metrics_export_descriptions_and_bounded_labels() {
        // Pins: the skill-learning loop metrics export HELP descriptions and carry only the
        // bounded source/kind/action/outcome labels — never a per-tenant, per-candidate, or
        // per-skill identifier. Asserted against rendered Prometheus output, not source text.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            record_skill_learning_candidates_filed("distilled", "created", 1);
            record_skill_learning_candidates_filed("distilled", "resynthesized", 1);
            record_skill_learning_candidates_filed("recurrence_mined", "created", 1);
            record_skill_learning_candidates_filed("mined", "weakness", 2);
            record_skill_learning_candidates_filed("rollback_monitor", "regression", 1);
            // A zero-count filing pass must not add a series.
            record_skill_learning_candidates_filed("distilled", "improved", 0);
            record_skill_learning_review_decision("accept_skill", "gate_rejected");
            record_skill_learning_review_decision("reject", "rejected");
        });
        let rendered = handle.render();

        let skill_learning_metrics = [
            "moa_skill_learning_candidates_filed_total",
            "moa_skill_learning_review_decisions_total",
        ];
        for metric in skill_learning_metrics {
            assert!(
                rendered.contains(&format!("# HELP {metric} ")),
                "skill-learning metric {metric} should export a HELP description; rendered:\n{rendered}"
            );
        }

        for label in ["source=", "kind=", "action=", "outcome="] {
            assert!(
                rendered.contains(label),
                "expected bounded skill-learning label `{label}` in rendered output:\n{rendered}"
            );
        }

        // A distilled dedupe-hit that rewrote an open draft files under the
        // `resynthesized` kind, distinct from `created`/`improved`.
        assert!(
            rendered.contains("kind=\"resynthesized\""),
            "a re-synthesized draft must export the resynthesized kind:\n{rendered}"
        );

        // A recurrence-cron dispatch files under the `recurrence_mined` source,
        // distinct from single-session `distilled`.
        assert!(
            rendered.contains("source=\"recurrence_mined\""),
            "a recurrence-mined candidate must export the recurrence_mined source:\n{rendered}"
        );

        // A zero-count filing pass adds no series, so `improved` never appears.
        assert!(
            !rendered.contains("kind=\"improved\""),
            "zero-count filing must not export a series:\n{rendered}"
        );

        let skill_learning_series = rendered
            .lines()
            .filter(|line| {
                skill_learning_metrics
                    .iter()
                    .any(|metric| line.starts_with(metric))
            })
            .collect::<Vec<_>>();
        assert!(
            !skill_learning_series.is_empty(),
            "skill-learning metric series should be exported:\n{rendered}"
        );
        // Checked as `=`-suffixed label keys so the assertion targets labels, not
        // the metric name (which itself contains "skill").
        for forbidden in [
            "tenant=",
            "candidate_id=",
            "candidate_uid=",
            "skill=",
            "skill_name=",
            "artifact_uid=",
            "revision=",
            "session_id=",
            "experience_id=",
            "reviewer=",
        ] {
            for line in &skill_learning_series {
                assert!(
                    !line.contains(forbidden),
                    "skill-learning series `{line}` must not carry high-cardinality label `{forbidden}`"
                );
            }
        }
    }

    #[test]
    fn knowledge_metrics_export_descriptions_and_low_cardinality_labels() {
        // Pins: tenant knowledge metrics export HELP descriptions and only the bounded
        // Task-13 label set (no tenant, source, object, contact, or error-message labels).
        // Asserted against rendered Prometheus output, not the crate's own source text.
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_metric_descriptions();
            record_knowledge_sync_run("github", "succeeded");
            record_knowledge_retrieval_duration("vector", "ok", Duration::from_millis(2));
            record_knowledge_retrieval_hits("graph", "dense", 2);
        });
        let rendered = handle.render();

        for metric in knowledge_metric_names() {
            assert!(
                rendered.contains(&format!("# HELP {metric} ")),
                "knowledge metric {metric} should export a HELP description; rendered:\n{rendered}"
            );
        }

        // Only the metric series lines carry labels; `# HELP`/`# TYPE` lines start with `#`.
        let knowledge_series = rendered
            .lines()
            .filter(|line| line.starts_with("moa_knowledge_"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !knowledge_series.is_empty(),
            "knowledge metric series should be exported:\n{rendered}"
        );
        for required_label in ["provider=", "status=", "stage=", "source_tier=", "leg="] {
            assert!(
                knowledge_series.contains(required_label),
                "knowledge series should include bounded label `{required_label}`:\n{knowledge_series}"
            );
        }
        for forbidden in [
            "tenant_id",
            "source_uri",
            "object_id",
            "object_uid",
            "contact_id",
            "contact_uid",
            "error_message",
            "error_code",
            "provider_event_id",
            "parser_job_id",
            "access_token",
            "api_key",
        ] {
            assert!(
                !knowledge_series.contains(forbidden),
                "knowledge series must not carry high-cardinality label `{forbidden}`:\n{knowledge_series}"
            );
        }
    }
}
