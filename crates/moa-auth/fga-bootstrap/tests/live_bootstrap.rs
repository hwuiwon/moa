//! Live integration test against a running OpenFGA instance.
//!
//! This is double-gated behind `#[ignore]` and
//! `MOA_RUN_LIVE_OPENFGA_TESTS=1` per AGENTS.md.

use std::process::Command;

#[test]
#[ignore = "requires MOA_RUN_LIVE_OPENFGA_TESTS=1 and running OpenFGA"]
fn bootstrap_is_idempotent_across_two_runs() {
    if std::env::var("MOA_RUN_LIVE_OPENFGA_TESTS").as_deref() != Ok("1") {
        return;
    }

    let binary = env!("CARGO_BIN_EXE_moa-fga-bootstrap");
    let output_path =
        std::env::temp_dir().join(format!("moa-fga-bootstrap-live-{}.env", std::process::id()));

    // Pins: bootstrap can create or reuse a named store and emits a concrete store ID.
    let first = Command::new(binary)
        .env("MOA_AUTHZ_OPENFGA_STORE_NAME", "moa-test-idempotent")
        .env(
            "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
            default_preshared_key_if_unset(),
        )
        .env("MOA_FGA_ENV_OUTPUT", &output_path)
        .output()
        .expect("first bootstrap run should execute");
    assert!(
        first.status.success(),
        "first run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr),
    );
    let first_stdout =
        String::from_utf8(first.stdout).expect("first bootstrap stdout should be UTF-8");
    let first_store_id = grep_env(&first_stdout, "MOA_AUTHZ_OPENFGA_STORE_ID");

    // Pins: bootstrap reuses the same store on the second run.
    let second = Command::new(binary)
        .env("MOA_AUTHZ_OPENFGA_STORE_NAME", "moa-test-idempotent")
        .env(
            "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
            default_preshared_key_if_unset(),
        )
        .env("MOA_FGA_ENV_OUTPUT", &output_path)
        .output()
        .expect("second bootstrap run should execute");
    assert!(
        second.status.success(),
        "second run failed: stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr),
    );
    let second_stdout =
        String::from_utf8(second.stdout).expect("second bootstrap stdout should be UTF-8");
    let second_store_id = grep_env(&second_stdout, "MOA_AUTHZ_OPENFGA_STORE_ID");

    assert_eq!(
        first_store_id, second_store_id,
        "bootstrap is not idempotent on store creation"
    );
}

fn grep_env(stdout: &str, key: &str) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("expected {key} in stdout, got:\n{stdout}"))
        .to_string()
}

fn default_preshared_key_if_unset() -> String {
    std::env::var("MOA_AUTHZ_OPENFGA_PRESHARED_KEY")
        .unwrap_or_else(|_| "localdev-preshared-key-do-not-use-in-prod".to_string())
}
