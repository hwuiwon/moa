//! Integration coverage for the mock perf-gate profile.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Returns `true` when `name` is set to a common truthy value (`1`, `true`,
/// `yes`, or `on`, case-insensitively after trimming), matching how live-test
/// flags are written in a developer's `.env`.
fn env_flag_enabled(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

#[test]
#[ignore = "requires a running Restate orchestrator with MOA_PROVIDERS_OVERRIDE=scripted:<fixture>"]
fn mock_short_profile_completes_within_budget_with_zero_errors() {
    if !env_flag_enabled("MOA_RUN_LOADTEST_REMOTE_SMOKE") {
        panic!("set MOA_RUN_LOADTEST_REMOTE_SMOKE=1 and MOA_RESTATE_INGRESS_URL to run this test");
    }
    let endpoint = std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://localhost:10010".into());
    let prom_out = repo_root().join(format!(
        "target/perf-gate/mock-short-{}.prom",
        uuid::Uuid::now_v7()
    ));
    let output = base_perf_gate_command()
        .args([
            "--profile",
            "mock-short",
            "--endpoint",
            &endpoint,
            "--duration",
            "5s",
            // The Restate path now includes live FGA checks on session creation
            // and turn start; keep the smoke budget above cold local authz cost.
            "--max-p95-ms",
            "2000",
            "--max-error-rate",
            "0",
            "--prom-out",
        ])
        .arg(&prom_out)
        .output()
        .expect("run perf_gate mock-short profile");

    assert!(
        output.status.success(),
        "perf_gate mock-short failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let snapshot = std::fs::read_to_string(&prom_out).expect("mock smoke prometheus snapshot");
    assert!(
        metric_value(&snapshot, "perf_gate_total_p95_ms") < 1_000.0,
        "expected P95 under 1s, snapshot:\n{snapshot}"
    );
    assert_eq!(metric_value(&snapshot, "perf_gate_error_rate"), 0.0);
}

#[test]
#[ignore = "requires a running Restate orchestrator with metrics enabled and MOA_PROVIDERS_OVERRIDE=scripted:<fixture>"]
fn mock_short_profile_reports_runtime_step_latency() {
    // Pins: ignored remote loadtest reports p50/p95/p99 for each documented turn step from runtime metrics.
    if !env_flag_enabled("MOA_RUN_LOADTEST_REMOTE_SMOKE") {
        panic!("set MOA_RUN_LOADTEST_REMOTE_SMOKE=1 and MOA_RESTATE_INGRESS_URL to run this test");
    }
    let metrics_endpoint = std::env::var("MOA_LOADTEST_METRICS_ENDPOINT")
        .expect("set MOA_LOADTEST_METRICS_ENDPOINT to the orchestrator Prometheus /metrics URL");
    let endpoint = std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://localhost:10010".into());
    let prom_out = repo_root().join(format!(
        "target/perf-gate/mock-short-step-latency-{}.prom",
        uuid::Uuid::now_v7()
    ));
    let output = base_perf_gate_command()
        .args([
            "--profile",
            "mock-short",
            "--endpoint",
            &endpoint,
            "--duration",
            "5s",
            "--max-p95-ms",
            "2000",
            "--max-error-rate",
            "0",
            "--metrics-endpoint",
            &metrics_endpoint,
            "--prom-out",
        ])
        .arg(&prom_out)
        .output()
        .expect("run perf_gate mock-short profile with step latency");

    assert!(
        output.status.success(),
        "perf_gate mock-short step-latency run failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let snapshot =
        std::fs::read_to_string(&prom_out).expect("mock smoke step latency prometheus snapshot");
    // Open-loop schedules derive turn counts from rate x duration, so pin the
    // relationship (steps sampled for every completed turn) instead of a
    // fixed count.
    let scheduled = metric_value(&snapshot, "perf_gate_requests_total");
    let completed = metric_value(&snapshot, "perf_gate_turns_completed");
    assert!(completed > 0.0, "no turns completed:\n{snapshot}");
    assert!(
        completed <= scheduled,
        "completed {completed} exceeds scheduled {scheduled}"
    );
    for step in
        moa_observability::TURN_LATENCY_REPORT_STEPS.map(moa_observability::TurnLatencyStep::as_str)
    {
        let samples = metric_value_with_step(&snapshot, "perf_gate_step_latency_samples", step);
        assert!(
            samples >= completed,
            "expected at least one {step} sample per completed turn; \
             samples={samples}, completed={completed}"
        );
        let p50 = metric_value_with_step(&snapshot, "perf_gate_step_latency_p50_ms", step);
        let p95 = metric_value_with_step(&snapshot, "perf_gate_step_latency_p95_ms", step);
        let p99 = metric_value_with_step(&snapshot, "perf_gate_step_latency_p99_ms", step);
        assert!(
            p50 <= p95 && p95 <= p99,
            "expected monotonic percentiles for {step}; p50={p50}, p95={p95}, p99={p99}"
        );
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("moa-loadtest manifest should live below the workspace root")
        .to_path_buf()
}

fn base_perf_gate_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_perf_gate"));
    for key in [
        "MOA_AUTH_AUTH0_DOMAIN",
        "MOA_AUTH_AUTH0_AUDIENCE",
        "MOA_AUTH_AUTH0_CLIENT_ID",
        "MOA_AUTH_AUTH0_CLIENT_SECRET",
        "MOA_AUTH_OIDC_ISSUER",
        "MOA_AUTH_OIDC_AUDIENCE",
        "MOA_AUTH_OIDC_JWKS_URL",
    ] {
        command.env_remove(key);
    }
    command.current_dir(repo_root());
    command
}

fn metric_value(snapshot: &str, metric: &str) -> f64 {
    snapshot
        .lines()
        .find_map(|line| {
            if !line.starts_with(metric) {
                return None;
            }
            line.split_whitespace().last()?.parse::<f64>().ok()
        })
        .unwrap_or_else(|| panic!("missing metric {metric} in snapshot:\n{snapshot}"))
}

fn metric_value_with_step(snapshot: &str, metric: &str, step: &str) -> f64 {
    snapshot
        .lines()
        .find_map(|line| {
            if !line.starts_with(metric) || !line.contains(&format!("step=\"{step}\"")) {
                return None;
            }
            line.split_whitespace().last()?.parse::<f64>().ok()
        })
        .unwrap_or_else(|| {
            panic!("missing metric {metric} for step {step} in snapshot:\n{snapshot}")
        })
}
