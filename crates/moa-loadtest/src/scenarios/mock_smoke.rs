//! Mock perf-gate profile backed by the generic session harness.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::*;

const DEFAULT_VIRTUAL_USERS: usize = 5;
const DEFAULT_DURATION: Duration = Duration::from_secs(30);
const DEFAULT_MAX_P95_MS: u64 = 5_000;
const DEFAULT_MAX_ERROR_RATE: f64 = 0.01;
const DEFAULT_TTFT: Duration = Duration::from_millis(50);
const DEFAULT_TURN_DURATION: Duration = Duration::from_millis(200);
const DEFAULT_ENDPOINT: &str = "http://localhost:10010";

/// Mock smoke performance gate configuration.
#[derive(Debug, Clone)]
pub struct MockSmokeConfig {
    /// Number of concurrent virtual users.
    pub virtual_users: usize,
    /// Load window duration.
    pub duration: Duration,
    /// Hard aggregate P95 turn-latency budget in milliseconds.
    pub max_p95_ms: u64,
    /// Maximum allowed turn error rate.
    pub max_error_rate: f64,
    /// Prometheus textfile output path.
    pub prom_out: PathBuf,
    /// Synthetic delay before the first streamed provider block.
    pub ttft: Duration,
    /// Synthetic delay before one provider response completes.
    pub turn_duration: Duration,
    /// Restate ingress endpoint fronting `moa-orchestrator`.
    pub endpoint: String,
}

impl Default for MockSmokeConfig {
    fn default() -> Self {
        Self {
            virtual_users: DEFAULT_VIRTUAL_USERS,
            duration: DEFAULT_DURATION,
            max_p95_ms: DEFAULT_MAX_P95_MS,
            max_error_rate: DEFAULT_MAX_ERROR_RATE,
            prom_out: PathBuf::from("target/perf-gate/snapshot.prom"),
            ttft: DEFAULT_TTFT,
            turn_duration: DEFAULT_TURN_DURATION,
            endpoint: DEFAULT_ENDPOINT.to_string(),
        }
    }
}

/// Runs the mock smoke performance gate.
pub async fn run_mock_smoke_gate(cfg: MockSmokeConfig) -> Result<()> {
    validate_config(&cfg)?;

    let report = match run_loadtest(LoadTestOptions {
        mode: LoadMode::Mock,
        endpoint: cfg.endpoint.clone(),
        sessions: cfg.virtual_users,
        profile: SessionProfileKind::Short,
        inter_message_delay: Duration::ZERO,
        turn_timeout: cfg.duration.max(Duration::from_secs(1)),
        output: OutputFormat::Json,
        model: None,
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

    let error_rate = error_rate(&report);
    let snapshot = render_prometheus(&report, error_rate);
    write_snapshot(&cfg.prom_out, &snapshot).await?;
    write_stdout(&print_summary_table(&cfg, &report, error_rate))?;
    enforce_gates(&cfg, &report, error_rate)
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
    if !(0.0..=1.0).contains(&cfg.max_error_rate) {
        bail!(
            "mock-short max error rate must be between 0 and 1; got {}",
            cfg.max_error_rate
        );
    }
    if cfg.turn_duration < cfg.ttft {
        bail!(
            "mock-short total turn duration {:?} must be greater than or equal to TTFT {:?}",
            cfg.turn_duration,
            cfg.ttft
        );
    }
    Ok(())
}

fn error_rate(report: &LoadTestReport) -> f64 {
    let planned_turns = report
        .sessions
        .iter()
        .map(|session| session.planned_turns)
        .sum::<usize>()
        .max(1);
    let failures = report.error_count + report.sessions_failed;
    failures as f64 / planned_turns as f64
}

fn enforce_gates(cfg: &MockSmokeConfig, report: &LoadTestReport, error_rate: f64) -> Result<()> {
    let mut breaches = Vec::new();
    if report.sessions_failed > 0 {
        breaches.push(format!("{} sessions failed", report.sessions_failed));
    }
    if report.latency_ms.p95 > cfg.max_p95_ms as f64 {
        breaches.push(format!(
            "P95 {:.1} ms > budget {} ms",
            report.latency_ms.p95, cfg.max_p95_ms
        ));
    }
    if error_rate > cfg.max_error_rate {
        breaches.push(format!(
            "error rate {:.4} > budget {:.4}",
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

fn print_summary_table(cfg: &MockSmokeConfig, report: &LoadTestReport, error_rate: f64) -> String {
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
        "| Sessions completed | {} |",
        report.sessions_completed
    );
    let _ = writeln!(out, "| Sessions failed | {} |", report.sessions_failed);
    let _ = writeln!(out, "| Turn P95 | {:.1} ms |", report.latency_ms.p95);
    let _ = writeln!(out, "| TTFT P95 | {:.1} ms |", report.ttft_ms.p95);
    let _ = writeln!(out, "| Error rate | {:.4} |", error_rate);
    out
}

fn render_prometheus(report: &LoadTestReport, error_rate: f64) -> String {
    let total_turns = report
        .sessions
        .iter()
        .map(|session| session.planned_turns)
        .sum::<usize>();
    let mut snapshot = String::new();
    let _ = writeln!(snapshot, "# TYPE perf_gate_total_p95_ms gauge");
    let _ = writeln!(snapshot, "perf_gate_total_p95_ms {}", report.latency_ms.p95);
    let _ = writeln!(snapshot, "# TYPE perf_gate_mock_ttft_p95_ms gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_mock_ttft_p95_ms {}",
        report.ttft_ms.p95
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_error_rate gauge");
    let _ = writeln!(snapshot, "perf_gate_error_rate {error_rate}");
    let _ = writeln!(snapshot, "# TYPE perf_gate_requests_total gauge");
    let _ = writeln!(snapshot, "perf_gate_requests_total {total_turns}");
    let _ = writeln!(snapshot, "# TYPE perf_gate_mock_sessions_failed gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_mock_sessions_failed {}",
        report.sessions_failed
    );
    snapshot
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
