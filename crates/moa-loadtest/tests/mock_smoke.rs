//! Integration coverage for the mock perf-gate profile.

use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
#[ignore = "requires a running Restate orchestrator with MOA_PROVIDERS_OVERRIDE=scripted:<fixture>"]
fn mock_short_profile_completes_within_budget_with_zero_errors() {
    if std::env::var("MOA_RUN_LOADTEST_REMOTE_SMOKE").as_deref() != Ok("1") {
        panic!("set MOA_RUN_LOADTEST_REMOTE_SMOKE=1 and MOA_RESTATE_INGRESS_URL to run this test");
    }
    let endpoint = std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://localhost:10010".into());
    let prom_out = repo_root().join(format!(
        "target/perf-gate/mock-short-{}.prom",
        uuid::Uuid::now_v7()
    ));
    let mut command = Command::new(env!("CARGO_BIN_EXE_perf_gate"));
    for key in [
        "MOA_AUTH_AUTH0_DOMAIN",
        "MOA_AUTH_AUTH0_AUDIENCE",
        "MOA_AUTH_AUTH0_CLIENT_ID_ENV",
        "MOA_AUTH_AUTH0_CLIENT_SECRET_ENV",
        "MOA_AUTH_OIDC_ISSUER",
        "MOA_AUTH_OIDC_AUDIENCE",
        "MOA_AUTH_OIDC_JWKS_URL",
    ] {
        command.env_remove(key);
    }

    let output = command
        .current_dir(repo_root())
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

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("moa-loadtest manifest should live below the workspace root")
        .to_path_buf()
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
