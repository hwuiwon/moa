//! Mock perf-gate profile backed by the generic session harness.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::*;

const DEFAULT_VIRTUAL_USERS: usize = 5;
const DEFAULT_DURATION: Duration = Duration::from_secs(30);
const DEFAULT_RATE_QPS: f64 = 10.0;
const DEFAULT_MAX_P95_MS: u64 = 5_000;
const DEFAULT_MAX_ERROR_RATE: f64 = 0.01;
const DEFAULT_ENDPOINT: &str = "http://localhost:10010";
const TURN_TIMEOUT: Duration = Duration::from_secs(60);
const STEP_LATENCY_PROM_METRICS: &[&str] = &[
    "perf_gate_step_latency_p50_ms",
    "perf_gate_step_latency_p95_ms",
    "perf_gate_step_latency_p99_ms",
    "perf_gate_step_latency_samples",
];

/// Mock smoke performance gate configuration.
#[derive(Debug, Clone)]
pub struct MockSmokeConfig {
    /// Concurrent session pool size.
    pub virtual_users: usize,
    /// Load window duration.
    pub duration: Duration,
    /// Offered turn-start rate in turns/second.
    pub rate: f64,
    /// Hard aggregate corrected-P95 turn-latency budget in milliseconds.
    pub max_p95_ms: u64,
    /// Maximum allowed turn error rate over scheduled arrivals.
    pub max_error_rate: f64,
    /// Prometheus textfile output path.
    pub prom_out: PathBuf,
    /// Restate ingress endpoint fronting `moa-orchestrator`.
    pub endpoint: String,
    /// Optional Prometheus metrics endpoint for per-step latency collection.
    pub metrics_endpoint: Option<String>,
}

impl Default for MockSmokeConfig {
    fn default() -> Self {
        Self {
            virtual_users: DEFAULT_VIRTUAL_USERS,
            duration: DEFAULT_DURATION,
            rate: DEFAULT_RATE_QPS,
            max_p95_ms: DEFAULT_MAX_P95_MS,
            max_error_rate: DEFAULT_MAX_ERROR_RATE,
            prom_out: PathBuf::from("target/perf-gate/snapshot.prom"),
            endpoint: DEFAULT_ENDPOINT.to_string(),
            metrics_endpoint: None,
        }
    }
}

/// Runs the mock smoke performance gate.
pub async fn run_mock_smoke_gate(cfg: MockSmokeConfig) -> Result<()> {
    validate_config(&cfg)?;

    let report = match run_loadtest(LoadTestOptions {
        mode: LoadMode::Mock,
        endpoint: cfg.endpoint.clone(),
        edge_endpoint: None,
        sessions: cfg.virtual_users,
        tenants: 2,
        identities_per_tenant: 1,
        profile: SessionProfileKind::Short,
        think_time: Duration::ZERO,
        rate: cfg.rate,
        shape: LoadShape::Steady,
        rate_end: None,
        spike_factor: 10.0,
        arrival: ArrivalProcess::Constant,
        duration: cfg.duration,
        warmup: None,
        turn_timeout: TURN_TIMEOUT,
        output: OutputFormat::Json,
        model: None,
        metrics_endpoint: cfg.metrics_endpoint.clone(),
        seed: 42,
    })
    .await
    {
        Ok(report) => report,
        Err(error) => {
            let snapshot = format!(
                "# TYPE perf_gate_mock_infrastructure_error gauge\nperf_gate_mock_infrastructure_error 1\n# error: {}\n",
                sanitize_prom_comment(&error.to_string())
            );
            write_snapshot(&cfg.prom_out, &snapshot).await?;
            return Err(error).context("mock-short loadtest failed");
        }
    };

    let snapshot = render_prometheus(&report);
    write_snapshot(&cfg.prom_out, &snapshot).await?;
    write_stdout(&print_summary_table(&cfg, &report))?;
    enforce_gates(&cfg, &report)
}

fn validate_config(cfg: &MockSmokeConfig) -> Result<()> {
    if !(1..=1_000).contains(&cfg.virtual_users) {
        bail!(
            "mock-short requires between 1 and 1000 virtual users; got {}",
            cfg.virtual_users
        );
    }
    if cfg.duration.is_zero() {
        bail!("mock-short duration must be greater than zero");
    }
    if cfg.rate <= 0.0 || !cfg.rate.is_finite() {
        bail!("mock-short rate must be a positive finite number");
    }
    if !(0.0..=1.0).contains(&cfg.max_error_rate) {
        bail!(
            "mock-short max error rate must be between 0 and 1; got {}",
            cfg.max_error_rate
        );
    }
    Ok(())
}

fn enforce_gates(cfg: &MockSmokeConfig, report: &LoadTestReport) -> Result<()> {
    let mut breaches = Vec::new();
    if report.sessions_failed > 0 {
        breaches.push(format!("{} sessions failed", report.sessions_failed));
    }
    if report.turn_latency_corrected_ms.p95 > cfg.max_p95_ms as f64 {
        breaches.push(format!(
            "corrected P95 {:.1} ms > budget {} ms",
            report.turn_latency_corrected_ms.p95, cfg.max_p95_ms
        ));
    }
    let error_rate = report.turn_error_rate();
    if error_rate > cfg.max_error_rate {
        breaches.push(format!(
            "turn error rate {:.4} > budget {:.4}",
            error_rate, cfg.max_error_rate
        ));
    }

    if breaches.is_empty() {
        write_stderr("all mock-short gates green\n")?;
        Ok(())
    } else {
        for breach in &breaches {
            write_stderr(&format!("{breach}\n"))?;
        }
        bail!("mock-short gate failed: {}", breaches.join("; "))
    }
}

fn print_summary_table(cfg: &MockSmokeConfig, report: &LoadTestReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "perf_gate mock-short summary");
    let _ = writeln!(out, "| Metric | Value |");
    let _ = writeln!(out, "| --- | ---: |");
    let _ = writeln!(out, "| Virtual users | {} |", cfg.virtual_users);
    let _ = writeln!(
        out,
        "| Duration window | {:.1}s |",
        cfg.duration.as_secs_f64()
    );
    let _ = writeln!(
        out,
        "| Rate | {:.1}/s requested, {:.1}/s achieved |",
        report.requested_rate_qps, report.achieved_rate_qps
    );
    let _ = writeln!(
        out,
        "| Sessions completed | {} |",
        report.sessions_completed
    );
    let _ = writeln!(out, "| Sessions failed | {} |", report.sessions_failed);
    let _ = writeln!(
        out,
        "| Turn P95 (corrected) | {:.1} ms |",
        report.turn_latency_corrected_ms.p95
    );
    let _ = writeln!(
        out,
        "| Turn P95 (service) | {:.1} ms |",
        report.turn_latency_ms.p95
    );
    let _ = writeln!(
        out,
        "| Dispatch delay P95 | {:.1} ms |",
        report.dispatch_delay_ms.p95
    );
    let _ = writeln!(out, "| TTFT P95 | {:.1} ms |", report.ttft_ms.p95);
    for step in &report.step_latency_ms {
        let _ = writeln!(
            out,
            "| Step `{}` P95 | {:.1} ms |",
            step.step, step.latency_ms.p95
        );
    }
    let _ = writeln!(out, "| Turn error rate | {:.4} |", report.turn_error_rate());
    out
}

fn render_prometheus(report: &LoadTestReport) -> String {
    let mut snapshot = String::new();
    let _ = writeln!(snapshot, "# TYPE perf_gate_total_p95_ms gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_total_p95_ms {}",
        report.turn_latency_corrected_ms.p95
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_service_p95_ms gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_service_p95_ms {}",
        report.turn_latency_ms.p95
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_dispatch_delay_p95_ms gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_dispatch_delay_p95_ms {}",
        report.dispatch_delay_ms.p95
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_mock_ttft_p95_ms gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_mock_ttft_p95_ms {}",
        report.ttft_ms.p95
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_error_rate gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_error_rate {}",
        report.turn_error_rate()
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_requests_total gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_requests_total {}",
        report.turns_scheduled
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_turns_completed gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_turns_completed {}",
        report.turns_completed
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_mock_sessions_failed gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_mock_sessions_failed {}",
        report.sessions_failed
    );
    if !report.step_latency_ms.is_empty() {
        for metric in STEP_LATENCY_PROM_METRICS {
            let _ = writeln!(snapshot, "# TYPE {metric} gauge");
        }
        for step in &report.step_latency_ms {
            let label = escape_prom_label(&step.step);
            for (metric, value) in [
                ("perf_gate_step_latency_p50_ms", step.latency_ms.p50),
                ("perf_gate_step_latency_p95_ms", step.latency_ms.p95),
                ("perf_gate_step_latency_p99_ms", step.latency_ms.p99),
                ("perf_gate_step_latency_samples", step.sample_count as f64),
            ] {
                write_step_latency_gauge(&mut snapshot, metric, &label, value);
            }
        }
    }
    snapshot
}

fn write_step_latency_gauge(snapshot: &mut String, metric: &str, label: &str, value: f64) {
    let _ = writeln!(snapshot, "{metric}{{step=\"{label}\"}} {value}");
}

async fn write_snapshot(path: &PathBuf, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "failed to create perf snapshot directory {}",
                parent.display()
            )
        })?;
    }
    tokio::fs::write(path, body)
        .await
        .with_context(|| format!("failed to write perf snapshot {}", path.display()))
}

fn write_stdout(message: &str) -> Result<()> {
    use std::io::Write as _;

    std::io::stdout()
        .write_all(message.as_bytes())
        .context("failed to write mock-short summary")
}

fn write_stderr(message: &str) -> Result<()> {
    use std::io::Write as _;

    std::io::stderr()
        .write_all(message.as_bytes())
        .context("failed to write mock-short gate status")
}

fn sanitize_prom_comment(value: &str) -> String {
    value.replace('\n', " ")
}

fn escape_prom_label(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', r#"\""#)
}
