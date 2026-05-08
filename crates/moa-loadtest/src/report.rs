//! Load-test report structures and renderers.

use crate::*;

/// One completed session's measurements.
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
    /// Total session wall time in milliseconds.
    pub duration_ms: f64,
    /// Session-scoped cache hit rate.
    pub cache_hit_rate: f64,
    /// Total session cost in cents.
    pub total_cost_cents: u64,
    /// Total tool calls observed across the session.
    pub tool_calls: usize,
    /// Total error events observed across the session.
    pub error_count: usize,
    /// Count of approvals auto-denied by the harness.
    pub auto_denied_approvals: usize,
    /// Turn-by-turn latency samples in milliseconds.
    pub turn_latency_ms: Vec<f64>,
    /// Turn-by-turn TTFT samples in milliseconds.
    pub ttft_ms: Vec<f64>,
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

/// Aggregate load-test report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadTestReport {
    /// Execution mode.
    pub mode: LoadMode,
    /// Restate ingress endpoint used by the run.
    pub endpoint: String,
    /// Requested profile family.
    pub profile: SessionProfileKind,
    /// Requested session count.
    pub sessions_requested: usize,
    /// Completed sessions.
    pub sessions_completed: usize,
    /// Failed sessions.
    pub sessions_failed: usize,
    /// Total observed error events.
    pub error_count: usize,
    /// Total observed tool calls.
    pub total_tool_calls: usize,
    /// Total auto-denied approvals.
    pub auto_denied_approvals: usize,
    /// Total run wall time in milliseconds.
    pub duration_ms: f64,
    /// Aggregate turn latency summary.
    pub latency_ms: PercentileSummary,
    /// Aggregate TTFT summary.
    pub ttft_ms: PercentileSummary,
    /// Aggregate cache-hit summary across sessions.
    pub cache_hit_rate: PercentileSummary,
    /// Total spend in cents.
    pub total_cost_cents: u64,
    /// Per-session results.
    pub sessions: Vec<SessionReport>,
}

/// Renders a human-readable load-test report.
pub fn render_human_report(report: &LoadTestReport) -> String {
    let mut output = String::new();
    let _ = writeln!(&mut output, "MOA Load Test Report");
    let _ = writeln!(&mut output, "====================");
    let _ = writeln!(
        &mut output,
        "Mode: {} | Endpoint: {} | Sessions: {} | Profile: {}",
        report.mode.as_str(),
        report.endpoint,
        report.sessions_requested,
        report.profile.as_str()
    );
    let _ = writeln!(
        &mut output,
        "Duration: {:.2}s",
        report.duration_ms / 1_000.0
    );
    let _ = writeln!(&mut output);
    let _ = writeln!(
        &mut output,
        "Turn Latency:\n  p50: {}  p95: {}  p99: {}",
        format_millis(report.latency_ms.p50),
        format_millis(report.latency_ms.p95),
        format_millis(report.latency_ms.p99)
    );
    let _ = writeln!(
        &mut output,
        "TTFT:\n  p50: {}  p95: {}  p99: {}",
        format_millis(report.ttft_ms.p50),
        format_millis(report.ttft_ms.p95),
        format_millis(report.ttft_ms.p99)
    );
    let _ = writeln!(
        &mut output,
        "Cache Hit Rate:\n  mean: {:.1}%  min: {:.1}%  max: {:.1}%",
        report.cache_hit_rate.mean * 100.0,
        report.cache_hit_rate.min * 100.0,
        report.cache_hit_rate.max * 100.0
    );
    let _ = writeln!(
        &mut output,
        "Sessions: {} completed, {} failed",
        report.sessions_completed, report.sessions_failed
    );
    let _ = writeln!(
        &mut output,
        "Total cost: {}",
        format_cost(report.total_cost_cents)
    );
    let _ = writeln!(
        &mut output,
        "Tool calls: {} | Errors: {} | Auto-denied approvals: {}",
        report.total_tool_calls, report.error_count, report.auto_denied_approvals
    );
    output
}

/// Serializes the report as pretty JSON.
pub fn render_json_report(report: &LoadTestReport) -> Result<String> {
    serde_json::to_string_pretty(report)
        .map_err(|error| MoaError::SerializationError(error.to_string()))
}
