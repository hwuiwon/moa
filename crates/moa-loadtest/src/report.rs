//! Load-test report structures and renderers.

use crate::*;

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
    /// Turns failed inside this window.
    pub turn_errors: u64,
    /// Corrected turn latency inside this window.
    pub latency_corrected_ms: PercentileSummary,
}

/// Aggregate load-test report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadTestReport {
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
    /// Delay between intended arrival and actual dispatch; sustained growth
    /// means the offered rate exceeds capacity.
    pub dispatch_delay_ms: PercentileSummary,
    /// Aggregate TTFT summary (measured from dispatch).
    pub ttft_ms: PercentileSummary,
    /// Aggregate per-step latency summaries from runtime metrics.
    pub step_latency_ms: Vec<StepLatencyReport>,
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
        "Rate: {:.1}/s requested, {:.1}/s achieved | Duration: {:.2}s (warmup {:.1}s excluded)",
        report.requested_rate_qps,
        report.achieved_rate_qps,
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
    let _ = writeln!(
        &mut output,
        "Cache Hit Rate:\n  mean: {:.1}%  min: {:.1}%  max: {:.1}%",
        report.cache_hit_rate.mean * 100.0,
        report.cache_hit_rate.min * 100.0,
        report.cache_hit_rate.max * 100.0
    );
    let _ = writeln!(
        &mut output,
        "Turns: {} scheduled, {} completed | error rate: {:.4}",
        report.turns_scheduled,
        report.turns_completed,
        report.turn_error_rate()
    );
    let _ = writeln!(
        &mut output,
        "Errors: start {} | timeout {} | failed {} | cancelled {} | dropped {} | event-load {} | setup {} | event-errors {} | tool-errors {}",
        report.errors.turn_start_failures,
        report.errors.turn_timeouts,
        report.errors.turn_failures,
        report.errors.turn_cancellations,
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
