//! Load-test report structures and renderers.

use crate::*;

const ENV_SOURCE_REVISION: &str = "MOA_LOADTEST_SOURCE_REVISION";
const ENV_SOURCE_STATE: &str = "MOA_LOADTEST_SOURCE_STATE";
const ENV_FOREGROUND_CONNECTIONS: &str = "MOA_LOADTEST_FOREGROUND_DB_CONNECTIONS";
const ENV_BACKGROUND_CONNECTIONS: &str = "MOA_LOADTEST_BACKGROUND_DB_CONNECTIONS";
const ENV_COMPOSE_PROJECT: &str = "MOA_LOADTEST_COMPOSE_PROJECT";
const ENV_STATE_IDENTITY: &str = "MOA_LOADTEST_STATE_IDENTITY";
const ENV_HARDWARE_ID: &str = "MOA_LOADTEST_HARDWARE_ID";

/// One completed session's summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReport {
    /// Session identifier.
    pub session_id: SessionId,
    /// Resolved profile for this session.
    pub profile: SessionProfileKind,
    /// Final session status.
    pub status: SessionStatus,
    /// Number of planned turns.
    pub planned_turns: usize,
    /// Number of completed turns observed by the harness.
    pub completed_turns: usize,
    /// Number of successful detached execution admissions.
    pub execution_admissions: usize,
    /// Session-scoped cache hit rate.
    pub cache_hit_rate: f64,
    /// Total session cost in cents.
    pub total_cost_cents: u64,
    /// Optional failure reason.
    pub failure_reason: Option<String>,
}

/// Percentile summary for one numeric metric.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PercentileSummary {
    /// Minimum sample value.
    pub min: f64,
    /// Arithmetic mean.
    pub mean: f64,
    /// Median.
    pub p50: f64,
    /// 95th percentile.
    pub p95: f64,
    /// 99th percentile.
    pub p99: f64,
    /// Maximum sample value.
    pub max: f64,
}

/// Aggregate latency summary for one turn step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepLatencyReport {
    /// Stable low-cardinality step label.
    pub step: String,
    /// Number of samples observed for this step.
    pub sample_count: u64,
    /// Approximate step latency summary in milliseconds.
    pub latency_ms: PercentileSummary,
}

/// Aggregate latency summary for one session event append phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAppendPhaseLatencyReport {
    /// Stable low-cardinality append phase label.
    pub phase: String,
    /// Number of samples observed for this phase.
    pub sample_count: u64,
    /// Approximate phase latency summary in milliseconds.
    pub latency_ms: PercentileSummary,
}

/// Durable event append counts for one event type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventAppendTypeReport {
    /// Stable event type label as exported by `moa_session_events_appended_total`.
    pub event_type: String,
    /// Number of rows appended for this event type during the measured run.
    pub rows: u64,
}

/// Durable event-log resource bill derived from runtime metrics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceBillReport {
    /// Total durable session-event rows appended during the measured run.
    pub durable_event_rows: u64,
    /// Durable event rows per successful answer or execution admission.
    pub durable_event_rows_per_successful_operation: f64,
    /// Durable `ProgressUpdate` rows appended during the measured run.
    pub progress_update_rows: u64,
    /// Durable `ProgressUpdate` rows per successful operation.
    pub progress_update_rows_per_successful_operation: f64,
    /// Durable event rows split by event type.
    pub event_rows_by_type: Vec<EventAppendTypeReport>,
}

/// Composite signals used to identify the first sustained capacity collapse.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapacitySignals {
    /// Successful answers or execution admissions divided by scheduled arrivals.
    pub success_ratio: f64,
    /// Arrivals the generator could not dispatch before its bounded wait expired.
    pub arrivals_dropped: u64,
    /// Other terminal turn failures, excluding bounded overload rejections/drops.
    pub terminal_failures: u64,
    /// Corrected p95 latency in milliseconds.
    pub corrected_p95_ms: f64,
    /// Corrected p99 latency in milliseconds.
    pub corrected_p99_ms: f64,
    /// Dispatch-delay p95 in milliseconds.
    pub dispatch_delay_p95_ms: f64,
    /// Live fleet admission leases observed at the final metrics scrape.
    pub admission_fleet_live: u64,
    /// Configured fleet admission limit.
    pub admission_fleet_limit: u64,
    /// Final live/limit admission ratio.
    pub admission_fleet_utilization: f64,
}

/// Failure counts split by kind so gates can budget each class separately.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ErrorTaxonomy {
    /// Turn start requests that failed outright.
    pub turn_start_failures: u64,
    /// Turns that exceeded the per-turn timeout.
    pub turn_timeouts: u64,
    /// Turns whose outcome was Failed.
    pub turn_failures: u64,
    /// Turns whose outcome was Cancelled.
    pub turn_cancellations: u64,
    /// Failed turns whose cooperative cleanup did not reach an idle session.
    pub turn_cleanup_failures: u64,
    /// Scheduled arrivals dropped because no session slot freed up within the
    /// pool-wait budget (the system, or a decayed pool, could not accept the
    /// offered load).
    pub arrivals_dropped: u64,
    /// Event-log reads that failed after a completed turn.
    pub event_load_failures: u64,
    /// Sessions that could not be created.
    pub session_setup_failures: u64,
    /// Error events observed in session event logs.
    pub event_error_events: u64,
    /// Tool error events observed in session event logs (excluding expected
    /// harness denials).
    pub tool_error_events: u64,
}

impl ErrorTaxonomy {
    /// Total failed turn attempts (start failures, timeouts, failures,
    /// cancellations, and dropped arrivals).
    pub fn failed_turns(&self) -> u64 {
        self.turn_start_failures
            + self.turn_timeouts
            + self.turn_failures
            + self.turn_cancellations
            + self.arrivals_dropped
    }
}

/// Percentiles for one measurement window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowReport {
    /// Window start offset from run start, in milliseconds.
    pub start_ms: f64,
    /// Window end offset from run start, in milliseconds.
    pub end_ms: f64,
    /// True when the window falls entirely inside warmup.
    pub warmup: bool,
    /// Turns completed inside this window.
    pub turns_completed: u64,
    /// Execution admissions inside this window.
    pub execution_admissions: u64,
    /// Turns failed inside this window.
    pub turn_errors: u64,
    /// Corrected turn latency inside this window.
    pub latency_corrected_ms: PercentileSummary,
    /// Corrected execution-admission latency inside this window.
    pub execution_admission_latency_corrected_ms: PercentileSummary,
}

/// Entry path exercised by one load-test report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadLane {
    /// Trusted, unscoped calls sent directly to Restate ingress.
    DirectIngress,
    /// Production edge authentication, Contacts, and SSE path.
    Edge,
}

/// Self-identifying configuration captured with every load-test report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoadTestRunManifest {
    /// Source revision supplied by the build/run environment, or `unknown`.
    pub source_revision: String,
    /// Source worktree state (`clean`, `dirty`, or `unknown`).
    pub source_state: String,
    /// Entry path exercised by this run.
    pub lane: LoadLane,
    /// Foreground database connections configured for each orchestrator replica.
    pub foreground_database_connections: u32,
    /// Background database connections configured for each orchestrator replica.
    pub background_database_connections: u32,
    /// Whether turn workflows append events directly inside named actions.
    pub direct_turn_event_append: bool,
    /// Compose project or deployment identity owning the test services.
    pub compose_project: String,
    /// Identity of the persistent state used by the run.
    pub state_identity: String,
    /// Operator-supplied hardware identity, with a portable local fallback.
    pub hardware_id: String,
    /// Concurrent session-pool size.
    pub sessions: usize,
    /// Synthetic tenant count.
    pub tenants: usize,
    /// Synthetic identities per tenant.
    pub identities_per_tenant: usize,
    /// Arrival-rate shape.
    pub shape: LoadShape,
    /// Inter-arrival process.
    pub arrival: ArrivalProcess,
    /// Optional end rate for ramp and stress runs.
    pub rate_end_qps: Option<f64>,
    /// Mean per-session think time in milliseconds.
    pub think_time_ms: u64,
    /// Per-turn timeout in milliseconds.
    pub turn_timeout_ms: u64,
    /// Schedule duration in milliseconds.
    pub schedule_duration_ms: u64,
    /// Deterministic workload seed.
    pub seed: u64,
}

impl LoadTestRunManifest {
    /// Captures the current process metadata and resolved load options.
    pub(crate) fn capture(options: &LoadTestOptions, config: &MoaConfig) -> Result<Self> {
        Self::capture_with(options, config, |name| std::env::var(name).ok())
    }

    fn capture_with(
        options: &LoadTestOptions,
        config: &MoaConfig,
        read_env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        let foreground_database_connections = manifest_u32(
            ENV_FOREGROUND_CONNECTIONS,
            config.database.max_connections,
            &read_env,
        )?;
        let background_database_connections = manifest_u32(
            ENV_BACKGROUND_CONNECTIONS,
            config.database.background_max_connections,
            &read_env,
        )?;
        let hardware_id = read_env(ENV_HARDWARE_ID).unwrap_or_else(|| {
            let parallelism = std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1);
            format!(
                "{}-{}-{parallelism}cpu",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        });

        Ok(Self {
            source_revision: manifest_text(ENV_SOURCE_REVISION, &read_env),
            source_state: manifest_text(ENV_SOURCE_STATE, &read_env),
            lane: if options.edge_endpoint.is_some() {
                LoadLane::Edge
            } else {
                LoadLane::DirectIngress
            },
            foreground_database_connections,
            background_database_connections,
            direct_turn_event_append: config.session.direct_turn_event_append,
            compose_project: manifest_text(ENV_COMPOSE_PROJECT, &read_env),
            state_identity: manifest_text(ENV_STATE_IDENTITY, &read_env),
            hardware_id,
            sessions: options.sessions,
            tenants: options.tenants,
            identities_per_tenant: options.identities_per_tenant,
            shape: options.shape,
            arrival: options.arrival,
            rate_end_qps: options.rate_end,
            think_time_ms: duration_millis(options.think_time),
            turn_timeout_ms: duration_millis(options.turn_timeout),
            schedule_duration_ms: duration_millis(options.duration),
            seed: options.seed,
        })
    }
}

fn manifest_text(name: &str, read_env: &impl Fn(&str) -> Option<String>) -> String {
    read_env(name)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn manifest_u32(
    name: &str,
    fallback: u32,
    read_env: &impl Fn(&str) -> Option<String>,
) -> Result<u32> {
    let Some(value) = read_env(name) else {
        return Ok(fallback);
    };
    value.parse::<u32>().map_err(|error| {
        MoaError::ValidationError(format!("{name} must be an unsigned integer: {error}"))
    })
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// Aggregate load-test report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadTestReport {
    /// Runtime and campaign identity needed to reproduce this report.
    pub run_manifest: LoadTestRunManifest,
    /// Execution mode.
    pub mode: LoadMode,
    /// Restate ingress endpoint used by the run.
    pub endpoint: String,
    /// Requested profile family.
    pub profile: SessionProfileKind,
    /// Offered turn-start rate in turns/second.
    pub requested_rate_qps: f64,
    /// Completed-turn throughput actually achieved (post-warmup window).
    pub achieved_rate_qps: f64,
    /// Detached execution-admission throughput achieved post-warmup.
    pub admission_rate_qps: f64,
    /// Combined successful answer and admission throughput post-warmup.
    pub successful_operation_rate_qps: f64,
    /// Sessions created over the run (pool churn included).
    pub sessions_started: usize,
    /// Completed sessions.
    pub sessions_completed: usize,
    /// Failed sessions.
    pub sessions_failed: usize,
    /// Scheduled turn arrivals.
    pub turns_scheduled: u64,
    /// Turns that completed successfully.
    pub turns_completed: u64,
    /// Detached execution runs admitted successfully.
    pub execution_admissions: u64,
    /// Exact sum of completed answers and execution admissions.
    pub successful_operations: u64,
    /// Failure counts by kind.
    pub errors: ErrorTaxonomy,
    /// Total observed tool calls.
    pub total_tool_calls: usize,
    /// Total auto-denied approvals.
    pub auto_denied_approvals: usize,
    /// Total run wall time in milliseconds.
    pub duration_ms: f64,
    /// Warmup prefix excluded from aggregates, in milliseconds.
    pub warmup_ms: f64,
    /// Coordinated-omission-corrected turn latency (measured from intended
    /// arrival). This is the SLO number.
    pub turn_latency_corrected_ms: PercentileSummary,
    /// Uncorrected service time (measured from actual dispatch).
    pub turn_latency_ms: PercentileSummary,
    /// Corrected execution-admission latency measured from intended arrival.
    pub execution_admission_latency_corrected_ms: PercentileSummary,
    /// Uncorrected execution-admission latency measured from dispatch.
    pub execution_admission_latency_ms: PercentileSummary,
    /// Delay between intended arrival and actual dispatch; sustained growth
    /// means the offered rate exceeds capacity.
    pub dispatch_delay_ms: PercentileSummary,
    /// Aggregate TTFT summary (measured from dispatch).
    pub ttft_ms: PercentileSummary,
    /// Edge-mode observation lag for the first durable response frame, measured
    /// from the event timestamp to client receipt when the SSE payload carries
    /// a server timestamp. Zero when unavailable.
    pub edge_observation_wait_ms: PercentileSummary,
    /// Aggregate per-step latency summaries from runtime metrics.
    pub step_latency_ms: Vec<StepLatencyReport>,
    /// Session event append transaction phase latency summaries from runtime metrics.
    #[serde(default)]
    pub event_append_phase_latency_ms: Vec<EventAppendPhaseLatencyReport>,
    /// Durable event-log resource bill from runtime metrics.
    #[serde(default)]
    pub resource_bill: ResourceBillReport,
    /// Throughput, tail, loss, rejection, and queue/admission collapse signals.
    #[serde(default)]
    pub capacity_signals: CapacitySignals,
    /// Aggregate cache-hit summary across sessions.
    pub cache_hit_rate: PercentileSummary,
    /// Total spend in cents.
    pub total_cost_cents: u64,
    /// Per-window latency/error series.
    pub windows: Vec<WindowReport>,
    /// Tenants generated for this run; scopes post-run invariant checks.
    pub tenant_ids: Vec<Uuid>,
    /// Embedded base64 HdrHistograms for lossless multi-worker merging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdr: Option<SerializedHistograms>,
    /// Per-session results.
    pub sessions: Vec<SessionReport>,
}

impl LoadTestReport {
    pub(crate) fn refresh_capacity_signals(
        &mut self,
        admission_fleet_live: Option<u64>,
        admission_fleet_limit: u64,
    ) {
        let success_ratio = if self.turns_scheduled == 0 {
            0.0
        } else {
            self.successful_operations as f64 / self.turns_scheduled as f64
        };
        let terminal_failures = self.errors.turn_start_failures
            + self.errors.turn_timeouts
            + self.errors.turn_failures
            + self.errors.turn_cancellations;
        let admission_fleet_live = admission_fleet_live.unwrap_or_default();
        self.capacity_signals = CapacitySignals {
            success_ratio,
            arrivals_dropped: self.errors.arrivals_dropped,
            terminal_failures,
            corrected_p95_ms: self.turn_latency_corrected_ms.p95,
            corrected_p99_ms: self.turn_latency_corrected_ms.p99,
            dispatch_delay_p95_ms: self.dispatch_delay_ms.p95,
            admission_fleet_live,
            admission_fleet_limit,
            admission_fleet_utilization: if admission_fleet_limit == 0 {
                0.0
            } else {
                admission_fleet_live as f64 / admission_fleet_limit as f64
            },
        };
    }

    /// Failed turn attempts over scheduled arrivals.
    pub fn turn_error_rate(&self) -> f64 {
        if self.turns_scheduled == 0 {
            return 0.0;
        }
        self.errors.failed_turns() as f64 / self.turns_scheduled as f64
    }

    /// Failed sessions over started sessions.
    pub fn session_failure_rate(&self) -> f64 {
        if self.sessions_started == 0 {
            return 0.0;
        }
        self.sessions_failed as f64 / self.sessions_started as f64
    }
}

/// Renders a human-readable load-test report.
pub fn render_human_report(report: &LoadTestReport) -> String {
    let mut output = String::new();
    let _ = writeln!(&mut output, "MOA Load Test Report");
    let _ = writeln!(&mut output, "====================");
    let _ = writeln!(
        &mut output,
        "Mode: {} | Endpoint: {} | Profile: {}",
        report.mode.as_str(),
        report.endpoint,
        report.profile.as_str()
    );
    let _ = writeln!(
        &mut output,
        "Lane: {:?} | Source: {} ({}) | DB pools: {} foreground + {} background | Event append: {} | State: {} / {} | Hardware: {}",
        report.run_manifest.lane,
        report.run_manifest.source_revision,
        report.run_manifest.source_state,
        report.run_manifest.foreground_database_connections,
        report.run_manifest.background_database_connections,
        if report.run_manifest.direct_turn_event_append {
            "direct action"
        } else {
            "SessionStore RPC"
        },
        report.run_manifest.compose_project,
        report.run_manifest.state_identity,
        report.run_manifest.hardware_id,
    );
    let _ = writeln!(
        &mut output,
        "Rate: {:.1}/s requested, {:.1}/s answers, {:.1}/s admissions, {:.1}/s successful operations | Duration: {:.2}s (warmup {:.1}s excluded)",
        report.requested_rate_qps,
        report.achieved_rate_qps,
        report.admission_rate_qps,
        report.successful_operation_rate_qps,
        report.duration_ms / 1_000.0,
        report.warmup_ms / 1_000.0
    );
    let _ = writeln!(&mut output);
    let _ = writeln!(
        &mut output,
        "Turn Latency (corrected, from intended arrival):\n  p50: {}  p95: {}  p99: {}  max: {}",
        format_millis(report.turn_latency_corrected_ms.p50),
        format_millis(report.turn_latency_corrected_ms.p95),
        format_millis(report.turn_latency_corrected_ms.p99),
        format_millis(report.turn_latency_corrected_ms.max)
    );
    let _ = writeln!(
        &mut output,
        "Turn Service Time (uncorrected):\n  p50: {}  p95: {}  p99: {}",
        format_millis(report.turn_latency_ms.p50),
        format_millis(report.turn_latency_ms.p95),
        format_millis(report.turn_latency_ms.p99)
    );
    let _ = writeln!(
        &mut output,
        "Execution Admission Latency (corrected):\n  p50: {}  p95: {}  p99: {}  max: {}",
        format_millis(report.execution_admission_latency_corrected_ms.p50),
        format_millis(report.execution_admission_latency_corrected_ms.p95),
        format_millis(report.execution_admission_latency_corrected_ms.p99),
        format_millis(report.execution_admission_latency_corrected_ms.max)
    );
    let _ = writeln!(
        &mut output,
        "Dispatch Delay:\n  p50: {}  p95: {}  p99: {}",
        format_millis(report.dispatch_delay_ms.p50),
        format_millis(report.dispatch_delay_ms.p95),
        format_millis(report.dispatch_delay_ms.p99)
    );
    let _ = writeln!(
        &mut output,
        "TTFT:\n  p50: {}  p95: {}  p99: {}",
        format_millis(report.ttft_ms.p50),
        format_millis(report.ttft_ms.p95),
        format_millis(report.ttft_ms.p99)
    );
    if report.edge_observation_wait_ms.max > 0.0 {
        let _ = writeln!(
            &mut output,
            "Edge Observation Wait:\n  p50: {}  p95: {}  p99: {}",
            format_millis(report.edge_observation_wait_ms.p50),
            format_millis(report.edge_observation_wait_ms.p95),
            format_millis(report.edge_observation_wait_ms.p99)
        );
    }
    if !report.step_latency_ms.is_empty() {
        let _ = writeln!(&mut output, "Step Latency:");
        for step in &report.step_latency_ms {
            let _ = writeln!(
                &mut output,
                "  {} (n={}): p50 {}  p95 {}  p99 {}",
                step.step,
                step.sample_count,
                format_millis(step.latency_ms.p50),
                format_millis(step.latency_ms.p95),
                format_millis(step.latency_ms.p99)
            );
        }
    }
    if !report.event_append_phase_latency_ms.is_empty() {
        let _ = writeln!(&mut output, "Event Append Phase Latency:");
        for phase in &report.event_append_phase_latency_ms {
            let _ = writeln!(
                &mut output,
                "  {} (n={}): p50 {}  p95 {}  p99 {}",
                phase.phase,
                phase.sample_count,
                format_millis(phase.latency_ms.p50),
                format_millis(phase.latency_ms.p95),
                format_millis(phase.latency_ms.p99)
            );
        }
    }
    if report.resource_bill.durable_event_rows > 0 {
        let _ = writeln!(
            &mut output,
            "Resource Bill:\n  durable event rows: {} ({:.2}/successful operation) | ProgressUpdate: {} ({:.2}/successful operation)",
            report.resource_bill.durable_event_rows,
            report
                .resource_bill
                .durable_event_rows_per_successful_operation,
            report.resource_bill.progress_update_rows,
            report
                .resource_bill
                .progress_update_rows_per_successful_operation
        );
    }
    let _ = writeln!(
        &mut output,
        "Cache Hit Rate:\n  mean: {:.1}%  min: {:.1}%  max: {:.1}%",
        report.cache_hit_rate.mean * 100.0,
        report.cache_hit_rate.min * 100.0,
        report.cache_hit_rate.max * 100.0
    );
    let _ = writeln!(
        &mut output,
        "Turns: {} scheduled, {} answers, {} admissions, {} successful operations | error rate: {:.4}",
        report.turns_scheduled,
        report.turns_completed,
        report.execution_admissions,
        report.successful_operations,
        report.turn_error_rate()
    );
    let _ = writeln!(
        &mut output,
        "Capacity: success {:.4} | corrected p95/p99 {:.1}/{:.1}ms | dispatch p95 {:.1}ms | admission {}/{} ({:.1}%)",
        report.capacity_signals.success_ratio,
        report.capacity_signals.corrected_p95_ms,
        report.capacity_signals.corrected_p99_ms,
        report.capacity_signals.dispatch_delay_p95_ms,
        report.capacity_signals.admission_fleet_live,
        report.capacity_signals.admission_fleet_limit,
        report.capacity_signals.admission_fleet_utilization * 100.0,
    );
    let _ = writeln!(
        &mut output,
        "Errors: start {} | timeout {} | failed {} | cancelled {} | cleanup {} | dropped {} | event-load {} | setup {} | event-errors {} | tool-errors {}",
        report.errors.turn_start_failures,
        report.errors.turn_timeouts,
        report.errors.turn_failures,
        report.errors.turn_cancellations,
        report.errors.turn_cleanup_failures,
        report.errors.arrivals_dropped,
        report.errors.event_load_failures,
        report.errors.session_setup_failures,
        report.errors.event_error_events,
        report.errors.tool_error_events
    );
    let _ = writeln!(
        &mut output,
        "Sessions: {} started, {} completed, {} failed",
        report.sessions_started, report.sessions_completed, report.sessions_failed
    );
    let _ = writeln!(
        &mut output,
        "Total cost: {}",
        format_cost(report.total_cost_cents)
    );
    let _ = writeln!(
        &mut output,
        "Tool calls: {} | Auto-denied approvals: {}",
        report.total_tool_calls, report.auto_denied_approvals
    );
    if !report.windows.is_empty() {
        let _ = writeln!(&mut output, "Windows (corrected p95 per {:.0}s):", {
            let first = &report.windows[0];
            (first.end_ms - first.start_ms) / 1_000.0
        });
        for window in &report.windows {
            let _ = writeln!(
                &mut output,
                "  [{:>6.1}s-{:>6.1}s]{} turns {:>6}  errors {:>4}  p50 {}  p95 {}  p99 {}",
                window.start_ms / 1_000.0,
                window.end_ms / 1_000.0,
                if window.warmup { " (warmup)" } else { "" },
                window.turns_completed,
                window.turn_errors,
                format_millis(window.latency_corrected_ms.p50),
                format_millis(window.latency_corrected_ms.p95),
                format_millis(window.latency_corrected_ms.p99)
            );
        }
    }
    output
}

/// Serializes the report as pretty JSON.
pub fn render_json_report(report: &LoadTestReport) -> Result<String> {
    serde_json::to_string_pretty(report)
        .map_err(|error| MoaError::SerializationError(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn options() -> LoadTestOptions {
        LoadTestOptions {
            mode: LoadMode::Mock,
            endpoint: "http://localhost:10010".to_string(),
            edge_endpoint: Some("http://localhost:10000".to_string()),
            sessions: 800,
            tenants: 8,
            identities_per_tenant: 2,
            profile: SessionProfileKind::Mixed,
            think_time: Duration::from_secs(2),
            rate: 5.0,
            shape: LoadShape::Ramp,
            rate_end: Some(200.0),
            spike_factor: 10.0,
            arrival: ArrivalProcess::Constant,
            duration: Duration::from_secs(600),
            warmup: Some(Duration::from_secs(30)),
            turn_timeout: Duration::from_secs(60),
            output: OutputFormat::Json,
            model: None,
            metrics_endpoint: Some("http://localhost:10023/metrics".to_string()),
            seed: 42,
        }
    }

    #[test]
    fn run_manifest_pins_lane_pool_and_campaign_identity() {
        // Pins: capacity JSON records the exact lane, database budgets, source,
        // state identity and schedule needed to reproduce it.
        let values = BTreeMap::from([
            (ENV_SOURCE_REVISION, "abc123"),
            (ENV_SOURCE_STATE, "dirty"),
            (ENV_FOREGROUND_CONNECTIONS, "20"),
            (ENV_BACKGROUND_CONNECTIONS, "1"),
            (ENV_COMPOSE_PROJECT, "capacity-pool20"),
            (ENV_STATE_IDENTITY, "capacity-pool20_moa-restate-data"),
            (ENV_HARDWARE_ID, "runner-a"),
        ]);

        let manifest =
            LoadTestRunManifest::capture_with(&options(), &MoaConfig::default(), |name| {
                values.get(name).map(|value| (*value).to_string())
            })
            .expect("valid manifest metadata should capture");

        assert_eq!(manifest.lane, LoadLane::Edge);
        assert_eq!(manifest.foreground_database_connections, 20);
        assert_eq!(manifest.background_database_connections, 1);
        assert_eq!(manifest.source_revision, "abc123");
        assert_eq!(manifest.source_state, "dirty");
        assert_eq!(manifest.compose_project, "capacity-pool20");
        assert_eq!(manifest.state_identity, "capacity-pool20_moa-restate-data");
        assert_eq!(manifest.hardware_id, "runner-a");
        assert_eq!(manifest.shape, LoadShape::Ramp);
        assert_eq!(manifest.rate_end_qps, Some(200.0));
        assert_eq!(manifest.schedule_duration_ms, 600_000);
        assert_eq!(manifest.turn_timeout_ms, 60_000);
    }

    #[test]
    fn run_manifest_rejects_invalid_pool_metadata() {
        // Pins: a malformed pool override fails the campaign before load starts
        // instead of emitting a report with a misleading fallback value.
        let error = LoadTestRunManifest::capture_with(&options(), &MoaConfig::default(), |name| {
            (name == ENV_FOREGROUND_CONNECTIONS).then(|| "many".to_string())
        })
        .expect_err("invalid pool metadata must fail");

        assert!(
            matches!(error, MoaError::ValidationError(ref message) if message.contains(ENV_FOREGROUND_CONNECTIONS)),
            "error must identify the invalid environment variable: {error}"
        );
    }
}
