//! Shared Prometheus-backed runtime metrics helpers for MOA.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

use metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};
#[cfg(tokio_unstable)]
use tokio_metrics::RuntimeMonitor;
#[cfg(tokio_unstable)]
use tracing::debug;

use crate::config::MetricsConfig;
use crate::error::{MoaError, Result};
use crate::types::{ModelId, ModelTier, SessionStatus, WorkspaceId};

const LATENCY_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];
const CACHE_HIT_RATE_BUCKETS: &[f64] = &[0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0];
#[cfg(tokio_unstable)]
const TOKIO_MONITOR_INTERVAL: Duration = Duration::from_secs(5);

/// Prometheus metric name for aggregate turn-step duration samples.
pub const TURN_STEP_DURATION_METRIC: &str = "moa_turn_step_duration_seconds";

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
}

static PROMETHEUS_ENDPOINT: OnceLock<SocketAddr> = OnceLock::new();
#[cfg(tokio_unstable)]
static TOKIO_RUNTIME_MONITOR_STARTED: OnceLock<()> = OnceLock::new();

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
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;

        builder
            .install()
            .map_err(|error| MoaError::ProviderError(error.to_string()))?;
        register_metric_descriptions();
        let _ = PROMETHEUS_ENDPOINT.set(addr);
    }

    spawn_tokio_runtime_metrics_publisher();

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
pub fn record_session_created(workspace_id: &WorkspaceId, status: &SessionStatus) {
    counter!(
        "moa_sessions_total",
        "workspace" => workspace_id.to_string(),
        "status" => session_status_label(status).to_string()
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
        "model_tier" => model_tier.as_str().to_string()
    )
    .increment(1);
}

/// Records one outbound LLM API request.
pub fn record_llm_request(provider: &str, model: &str) {
    counter!(
        "moa_llm_requests_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .increment(1);
}

/// Records one completed LLM request duration sample.
pub fn record_llm_request_duration(provider: &str, model: &str, status: &str, duration: Duration) {
    histogram!(
        "moa_llm_request_duration_seconds",
        "provider" => provider.to_string(),
        "model" => model.to_string(),
        "status" => status.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one failed LLM request by bounded error class.
pub fn record_llm_failure(provider: &str, model: &str, class: &str) {
    counter!(
        "moa_llm_failures_total",
        "provider" => provider.to_string(),
        "model" => model.to_string(),
        "class" => class.to_string()
    )
    .increment(1);
}

/// Records uncached input tokens, including cache-write prompt tokens.
pub fn record_tokens_input_uncached(provider: &str, model: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }

    counter!(
        "moa_tokens_input_uncached_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .increment(tokens);
}

/// Records cached input tokens served from provider-side prefix caches.
pub fn record_tokens_input_cached(provider: &str, model: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }

    counter!(
        "moa_tokens_input_cached_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .increment(tokens);
}

/// Records output tokens emitted by an LLM response.
pub fn record_tokens_output(provider: &str, model: &str, tokens: u64) {
    if tokens == 0 {
        return;
    }

    counter!(
        "moa_tokens_output_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .increment(tokens);
}

/// Records the ratio of input tokens that were served from cache for one request.
pub fn record_cache_hit_rate(provider: &str, model: &str, ratio: f64) {
    histogram!(
        "moa_cache_hit_rate",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .record(ratio.clamp(0.0, 1.0));
}

/// Records the time to first token for one LLM request.
pub fn record_llm_ttft(provider: &str, model: &str, duration: Duration) {
    histogram!(
        "moa_llm_ttft_seconds",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records the total streaming duration for one LLM request.
pub fn record_llm_streaming_duration(provider: &str, model: &str, duration: Duration) {
    histogram!(
        "moa_llm_streaming_seconds",
        "provider" => provider.to_string(),
        "model" => model.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one LLM completion cost sample in cents.
pub fn record_llm_cost_cents(provider: &str, model: &str, cost_cents: u64) {
    if cost_cents == 0 {
        return;
    }

    counter!(
        "moa_llm_cost_cents_total",
        "provider" => provider.to_string(),
        "model" => model.to_string()
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

/// Records approval wait latency until a decision is made or timed out.
pub fn record_approval_wait(duration: Duration, outcome: &str) {
    histogram!(
        "moa_approval_wait_seconds",
        "outcome" => outcome.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one tool call completion and its latency.
pub fn record_tool_call(tool_name: &str, status: &str, duration: Duration) {
    counter!(
        "moa_tool_calls_total",
        "tool_name" => tool_name.to_string(),
        "status" => status.to_string()
    )
    .increment(1);
    histogram!(
        "moa_tool_call_duration_seconds",
        "tool_name" => tool_name.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one classified tool execution failure.
pub fn record_tool_failure(provider: &str, tool_name: &str, class: &str) {
    counter!(
        "moa_tool_failure_total",
        "class" => class.to_string(),
        "provider" => provider.to_string(),
        "tool" => tool_name.to_string()
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
        "tool_name" => tool_name.to_string()
    )
    .increment(1);
}

/// Records one applied compaction tier.
pub fn record_compaction_tier_applied(tier: u8) {
    counter!(
        "moa_compaction_tier_applied_total",
        "tier" => tier.to_string()
    )
    .increment(1);
}

/// Records one end-to-end turn latency sample.
pub fn record_turn_latency(duration: Duration) {
    histogram!("moa_turn_latency_seconds").record(duration.as_secs_f64());
}

/// Records one aggregate turn-step duration sample.
pub fn record_turn_step_duration(step: TurnLatencyStep, duration: Duration) {
    histogram!(
        TURN_STEP_DURATION_METRIC,
        "step" => step.as_str()
    )
    .record(duration.as_secs_f64());
}

/// Records one terminal turn-workflow outcome and its total workflow latency.
pub fn record_turn_workflow_outcome(
    scope: &str,
    result: &str,
    model_tier: ModelTier,
    duration: Duration,
) {
    counter!(
        "moa_turn_outcomes_total",
        "scope" => scope.to_string(),
        "result" => result.to_string(),
        "model_tier" => model_tier.as_str().to_string()
    )
    .increment(1);
    histogram!(
        "moa_turn_workflow_latency_seconds",
        "scope" => scope.to_string(),
        "result" => result.to_string(),
        "model_tier" => model_tier.as_str().to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one pipeline compilation duration sample.
pub fn record_pipeline_compile_duration_metric(duration: Duration) {
    histogram!("moa_pipeline_compile_seconds").record(duration.as_secs_f64());
}

/// Records one query-rewrite gate outcome.
pub fn record_query_rewrite_decision(
    decision: &str,
    reason: &str,
    llm_called: bool,
    duration: Duration,
) {
    let llm_called = if llm_called { "true" } else { "false" };
    counter!(
        "moa_query_rewrite_decisions_total",
        "decision" => decision.to_string(),
        "reason" => reason.to_string(),
        "llm_called" => llm_called.to_string()
    )
    .increment(1);
    histogram!(
        "moa_query_rewrite_duration_seconds",
        "decision" => decision.to_string(),
        "llm_called" => llm_called.to_string()
    )
    .record(duration.as_secs_f64());
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

/// Records the time spent beginning a scoped Postgres transaction.
pub fn record_scoped_transaction_begin_duration(duration: Duration) {
    histogram!("moa_scoped_transaction_begin_seconds").record(duration.as_secs_f64());
}

/// Records the time spent applying scoped Postgres GUC values.
pub fn record_scoped_guc_application_duration(duration: Duration) {
    histogram!("moa_scoped_guc_application_seconds").record(duration.as_secs_f64());
}

/// Records one appended session event, labeled by event type.
pub fn record_session_event_append(event_type: &str) {
    counter!(
        "moa_session_events_appended_total",
        "event_type" => event_type.to_string()
    )
    .increment(1);
}

/// Records one session event load operation and the number of events returned.
pub fn record_session_event_load(event_count: u64) {
    counter!("moa_session_event_loads_total").increment(1);
    histogram!("moa_session_event_load_events").record(event_count as f64);
}

/// Records decoded session event payload bytes.
pub fn record_session_event_decoded_bytes(bytes: u64) {
    if bytes == 0 {
        return;
    }

    counter!("moa_session_event_decoded_bytes_total").increment(bytes);
}

/// Records the time spent constructing a context pipeline.
pub fn record_context_pipeline_construction(duration: Duration) {
    histogram!("moa_context_pipeline_construction_seconds").record(duration.as_secs_f64());
}

/// Records the time spent constructing a retrieval embedder.
pub fn record_retrieval_embedder_construction(result: &str, duration: Duration) {
    histogram!(
        "moa_retrieval_embedder_construction_seconds",
        "result" => result.to_string()
    )
    .record(duration.as_secs_f64());
}

/// Records one tool idempotency scan and the number of prior events scanned.
pub fn record_tool_idempotency_scan(event_type: &str, scanned_events: u64, duration: Duration) {
    histogram!(
        "moa_tool_idempotency_scan_seconds",
        "event_type" => event_type.to_string()
    )
    .record(duration.as_secs_f64());
    histogram!(
        "moa_tool_idempotency_scan_events",
        "event_type" => event_type.to_string()
    )
    .record(scanned_events as f64);
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
    histogram!(
        "moa_memory_operation_results",
        "operation" => operation.to_string(),
        "status" => status.to_string()
    )
    .record(result_count as f64);
}

/// Records live broadcast events dropped because a receiver lagged.
pub fn record_broadcast_lag(channel: &str, policy: &str, dropped_events: u64) {
    if dropped_events == 0 {
        return;
    }

    counter!(
        "moa_broadcast_lag_events_dropped_total",
        "channel" => channel.to_string(),
        "policy" => policy.to_string()
    )
    .increment(dropped_events);
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

/// Records one terminal experiment trial duration.
pub fn record_experiment_trial_duration(target_kind: &str, status: &str, duration: Duration) {
    histogram!(
        "moa_experiment_trial_duration_seconds",
        "target_kind" => target_kind.to_string(),
        "status" => status.to_string()
    )
    .record(duration.as_secs_f64());
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

/// Records a trial that stopped because its target entered an approval wait.
pub fn record_experiment_approval_wait(target_kind: &str) {
    counter!(
        "moa_experiment_approval_waits_total",
        "target_kind" => target_kind.to_string()
    )
    .increment(1);
}

#[cfg(tokio_unstable)]
fn spawn_tokio_runtime_metrics_publisher() {
    if TOKIO_RUNTIME_MONITOR_STARTED.get().is_some() {
        return;
    }

    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        debug!("tokio runtime metrics not started because no runtime handle is active");
        return;
    };

    let monitor = RuntimeMonitor::new(&handle);
    tokio::spawn(async move {
        let mut intervals = monitor.intervals();
        loop {
            if let Some(interval) = intervals.next() {
                gauge!("tokio_workers_count").set(interval.workers_count as f64);
                counter!("tokio_total_park_count").increment(interval.total_park_count);
                gauge!("tokio_global_queue_depth").set(interval.global_queue_depth as f64);
                gauge!("tokio_worker_mean_poll_time_us")
                    .set(interval.mean_poll_duration.as_micros() as f64);
                counter!("tokio_budget_forced_yield_count")
                    .increment(interval.budget_forced_yield_count);
            }
            tokio::time::sleep(TOKIO_MONITOR_INTERVAL).await;
        }
    });
    let _ = TOKIO_RUNTIME_MONITOR_STARTED.set(());
}

#[cfg(not(tokio_unstable))]
fn spawn_tokio_runtime_metrics_publisher() {}

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

fn register_metric_descriptions() {
    describe_gauge!("moa_sessions_active", "Currently active MOA sessions.");
    describe_gauge!(
        "tokio_workers_count",
        "Number of worker threads in the active Tokio runtime."
    );
    describe_counter!(
        "tokio_total_park_count",
        "Total number of worker parks observed across runtime sampling intervals."
    );
    describe_gauge!(
        "tokio_global_queue_depth",
        "Current depth of the Tokio runtime global scheduler queue."
    );
    describe_gauge!(
        "tokio_worker_mean_poll_time_us",
        "Mean Tokio worker poll time in microseconds."
    );
    describe_counter!(
        "tokio_budget_forced_yield_count",
        "Number of task budget forced yields observed across runtime sampling intervals."
    );
    describe_counter!(
        "moa_sessions_total",
        "Total sessions created, labeled by workspace and initial status."
    );
    describe_counter!(
        "moa_turns_total",
        "Total assistant turns completed, labeled by model and routing tier."
    );
    describe_counter!(
        "moa_llm_requests_total",
        "Total outbound LLM API requests, labeled by provider and model."
    );
    describe_counter!(
        "moa_llm_failures_total",
        "Total failed outbound LLM API requests, labeled by provider, model, and bounded error class."
    );
    describe_histogram!(
        "moa_llm_request_duration_seconds",
        "Total outbound LLM request duration in seconds, labeled by provider, model, and status."
    );
    describe_counter!(
        "moa_tokens_input_cached_total",
        "Total cached input tokens served from provider-side caches."
    );
    describe_counter!(
        "moa_tokens_input_uncached_total",
        "Total non-cached input tokens, including cache-write prompt tokens."
    );
    describe_counter!(
        "moa_tokens_output_total",
        "Total output tokens emitted by provider responses."
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
        "moa_broadcast_lag_events_dropped_total",
        "Live broadcast events dropped because a subscriber lagged behind, labeled by channel and handling policy."
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
        "moa_lineage_recorded_total",
        "Lineage events accepted by the hot-path channel."
    );
    describe_counter!(
        "moa_lineage_flushed_total",
        "Lineage rows flushed from the durable journal into Postgres."
    );
    describe_gauge!(
        "moa_lineage_journal_depth",
        "Approximate lineage events pending in the durable journal."
    );
    describe_gauge!(
        "moa_grounding_verified_rate",
        "Latest citation verifier outcome per workspace, encoded as 0 or 1."
    );
    describe_counter!(
        "moa_zero_recall_count",
        "Retrieval operations that returned an empty top-K."
    );
    describe_counter!(
        "moa_turn_count",
        "Retrieval-scoped turn count used for lineage zero-recall alerting."
    );
    describe_gauge!(
        "moa_cost_micros_per_turn",
        "Latest generation cost per turn in micros of USD."
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
        "moa_turn_workflow_latency_seconds",
        "End-to-end turn workflow latency in seconds, labeled by scope, result, and model tier."
    );
    describe_histogram!(
        "moa_llm_ttft_seconds",
        "Time to first token for LLM requests in seconds."
    );
    describe_histogram!(
        "moa_llm_streaming_seconds",
        "Total LLM request streaming duration in seconds."
    );
    describe_histogram!(
        "moa_approval_wait_seconds",
        "Approval wait duration in seconds."
    );
    describe_histogram!(
        "moa_tool_call_duration_seconds",
        "Tool execution duration in seconds."
    );
    describe_histogram!(
        "moa_pipeline_compile_seconds",
        "Context pipeline compilation duration in seconds."
    );
    describe_counter!(
        "moa_query_rewrite_decisions_total",
        "Query rewrite gate decisions, labeled by decision, reason, and LLM-call status."
    );
    describe_histogram!(
        "moa_query_rewrite_duration_seconds",
        "Query rewrite stage duration in seconds, labeled by decision and LLM-call status."
    );
    describe_histogram!(
        "moa_sandbox_provision_seconds",
        "Sandbox provisioning duration in seconds."
    );
    describe_histogram!(
        "moa_cache_hit_rate",
        "Ratio of cached input tokens to total input tokens for one request."
    );
    describe_histogram!(
        "moa_scoped_transaction_begin_seconds",
        "Scoped Postgres transaction begin duration in seconds."
    );
    describe_histogram!(
        "moa_scoped_guc_application_seconds",
        "Scoped Postgres GUC application duration in seconds."
    );
    describe_counter!(
        "moa_session_events_appended_total",
        "Session events appended to the durable event log, labeled by event type."
    );
    describe_counter!(
        "moa_session_event_loads_total",
        "Session event load operations executed against the durable event log."
    );
    describe_histogram!(
        "moa_session_event_load_events",
        "Number of session events returned by one durable event log load."
    );
    describe_counter!(
        "moa_session_event_decoded_bytes_total",
        "Decoded session event payload bytes loaded from the durable event log."
    );
    describe_histogram!(
        "moa_context_pipeline_construction_seconds",
        "Context pipeline construction duration in seconds."
    );
    describe_histogram!(
        "moa_retrieval_embedder_construction_seconds",
        "Retrieval embedder construction duration in seconds, labeled by result."
    );
    describe_histogram!(
        "moa_tool_idempotency_scan_seconds",
        "Tool idempotency prior-event scan duration in seconds, labeled by event type."
    );
    describe_histogram!(
        "moa_tool_idempotency_scan_events",
        "Number of prior session events inspected by one tool idempotency scan."
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
    describe_histogram!(
        "moa_memory_operation_results",
        "Memory service result counts, labeled by operation and status."
    );
    describe_counter!(
        "moa_experiment_runs_total",
        "Experiment run lifecycle observations, labeled by terminal status and bounded target kind."
    );
    describe_counter!(
        "moa_experiment_trials_total",
        "Experiment trial lifecycle observations, labeled by status, bounded stop reason, and bounded target kind."
    );
    describe_histogram!(
        "moa_experiment_trial_duration_seconds",
        "Terminal experiment trial duration in seconds, labeled by bounded status and target kind."
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
        "moa_experiment_approval_waits_total",
        "Experiment trials that stopped on approval waits, labeled by bounded target kind."
    );
}

fn session_status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Created => "created",
        SessionStatus::Running => "running",
        SessionStatus::Paused => "paused",
        SessionStatus::WaitingApproval => "waiting_approval",
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

        record_llm_request("mock", "gpt-5.4");
        record_tokens_input_uncached("mock", "gpt-5.4", 8);
        record_tokens_output("mock", "gpt-5.4", 4);
        record_cache_hit_rate("mock", "gpt-5.4", 0.5);
        record_turn_latency(Duration::from_millis(25));
        record_turn_step_duration(TurnLatencyStep::PipelineCompile, Duration::from_millis(10));
        record_scoped_transaction_begin_duration(Duration::from_millis(1));
        record_scoped_guc_application_duration(Duration::from_millis(2));
        record_session_event_append("ToolCall");
        record_session_event_load(2);
        record_session_event_decoded_bytes(128);
        record_context_pipeline_construction(Duration::from_millis(3));
        record_retrieval_embedder_construction("success", Duration::from_millis(4));
        record_tool_idempotency_scan("ToolResult", 5, Duration::from_millis(5));
        record_api_key_validation_duration("failure", Duration::from_millis(6));
        record_experiment_run("accepted", "agent_loop");
        record_experiment_trial("completed", Some("max_turns"), "agent_loop");
        record_experiment_trial_duration("agent_loop", "completed", Duration::from_millis(7));
        record_simulation_turn("agent_loop");
        record_simulation_tokens("simulator", 16);
        record_simulation_cost_cents("simulator", 1);
        record_experiment_score_rows("scores", 3);
        record_experiment_learning_candidates("proposed", 1);
        record_experiment_approval_wait("agent_loop");

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

        assert!(scrape.contains("moa_llm_requests_total"));
        assert!(scrape.contains("moa_tokens_input_uncached_total"));
        assert!(scrape.contains("moa_tokens_output_total"));
        assert!(scrape.contains("moa_cache_hit_rate"));
        assert!(scrape.contains("moa_turn_latency_seconds"));
        assert!(scrape.contains("moa_turn_step_duration_seconds"));
        assert!(scrape.contains("moa_scoped_transaction_begin_seconds"));
        assert!(scrape.contains("moa_scoped_guc_application_seconds"));
        assert!(scrape.contains("moa_session_events_appended_total"));
        assert!(scrape.contains("moa_session_event_loads_total"));
        assert!(scrape.contains("moa_session_event_load_events"));
        assert!(scrape.contains("moa_session_event_decoded_bytes_total"));
        assert!(scrape.contains("moa_context_pipeline_construction_seconds"));
        assert!(scrape.contains("moa_retrieval_embedder_construction_seconds"));
        assert!(scrape.contains("moa_tool_idempotency_scan_seconds"));
        assert!(scrape.contains("moa_tool_idempotency_scan_events"));
        assert!(scrape.contains("moa_api_key_validation_seconds"));
        assert!(scrape.contains("moa_experiment_runs_total"));
        assert!(scrape.contains("moa_experiment_trials_total"));
        assert!(scrape.contains("moa_experiment_trial_duration_seconds"));
        assert!(scrape.contains("moa_simulation_turns_total"));
        assert!(scrape.contains("moa_simulation_tokens_total"));
        assert!(scrape.contains("moa_simulation_cost_cents_total"));
        assert!(scrape.contains("moa_experiment_score_rows_total"));
        assert!(scrape.contains("moa_experiment_learning_candidates_total"));
        assert!(scrape.contains("moa_experiment_approval_waits_total"));

        #[cfg(tokio_unstable)]
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            let tokio_scrape = loop {
                let response = client.get(&url).send().await.expect("tokio metrics scrape");
                let body = response.text().await.expect("tokio scrape body");
                if body.contains("tokio_workers_count")
                    && body.contains("tokio_global_queue_depth")
                    && body.contains("tokio_worker_mean_poll_time_us")
                {
                    break body;
                }
                if Instant::now() >= deadline {
                    panic!("tokio runtime metrics never appeared in scrape output");
                }
                sleep(Duration::from_millis(50)).await;
            };

            assert!(tokio_scrape.contains("tokio_workers_count"));
            assert!(tokio_scrape.contains("tokio_global_queue_depth"));
            assert!(tokio_scrape.contains("tokio_worker_mean_poll_time_us"));
        }
    }

    #[test]
    fn experiment_metric_descriptions_exist_and_labels_stay_bounded() {
        // Pins: experiment metrics remain an aggregate dashboard surface, not a drilldown ID index.
        let source = include_str!("runtime_metrics.rs");
        for metric in [
            "moa_experiment_runs_total",
            "moa_experiment_trials_total",
            "moa_experiment_trial_duration_seconds",
            "moa_simulation_turns_total",
            "moa_simulation_tokens_total",
            "moa_simulation_cost_cents_total",
            "moa_experiment_score_rows_total",
            "moa_experiment_learning_candidates_total",
            "moa_experiment_approval_waits_total",
        ] {
            let described = source.contains(&format!("describe_counter!(\n        \"{metric}\""))
                || source.contains(&format!("describe_histogram!(\n        \"{metric}\""));
            assert!(
                described,
                "runtime metric {metric} should have a description"
            );
        }

        let experiment_metrics_source = source
            .split("pub fn record_experiment_run")
            .nth(1)
            .expect("experiment metric helpers should exist")
            .split("#[cfg(tokio_unstable)]")
            .next()
            .expect("experiment metric helper section should end before runtime publisher");
        for forbidden in [
            "run_uid",
            "trial_uid",
            "session_id",
            "workflow_run_uid",
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
            assert!(
                !experiment_metrics_source.contains(forbidden),
                "experiment metric helpers must not use high-cardinality label `{forbidden}`"
            );
        }
    }
}
