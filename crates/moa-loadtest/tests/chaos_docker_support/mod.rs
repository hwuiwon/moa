//! Shared setup and invariant sweep for chaos `_docker` tests.

use std::path::PathBuf;
use std::time::Duration;

use moa_loadtest::scenarios::chaos::{ChaosExperiment, ChaosStackConfig, ExperimentOutcome};
use moa_test_support::invariants::{InvariantScope, InvariantViolation, check_invariants};
use sqlx::postgres::PgPoolOptions;

/// Fails fast with a clear message when the chaos lane is not enabled.
pub fn require_chaos_env() {
    if std::env::var("MOA_RUN_CHAOS_TESTS").is_err() {
        panic!(
            "chaos tests need the compose stack up and MOA_RUN_CHAOS_TESTS=1 \
             (plus MOA_AUTHZ_OPENFGA_STORE_ID/MODEL_ID); run via `make chaos-smoke`"
        );
    }
}

/// Stack config rooted at the workspace (two levels above this crate).
pub fn stack_config() -> ChaosStackConfig {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root resolves");
    ChaosStackConfig {
        project_dir: workspace_root,
        endpoint: std::env::var("MOA_RESTATE_INGRESS_URL")
            .unwrap_or_else(|_| "http://localhost:10010".to_string()),
        toxiproxy_url: std::env::var("MOA_TOXIPROXY_URL")
            .unwrap_or_else(|_| "http://localhost:10060".to_string()),
    }
}

/// Sweeps durability invariants scoped to the experiment's tenants.
pub async fn sweep_invariants(outcome: &ExperimentOutcome) -> Vec<InvariantViolation> {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("chaos invariant sweep connects to compose postgres");
    check_invariants(
        &pool,
        &InvariantScope {
            tenant_ids: outcome.report.tenant_ids.clone(),
            stuck_after: Duration::from_secs(60),
        },
    )
    .await
    .expect("invariant sweep queries succeed")
}

/// True when the fault left evidence: window errors, a throughput hole, or a
/// clear fault-phase latency bulge. Healthy systems absorb short partitions
/// and kills through retries/replay, so latency is often the only trace.
pub fn fault_visibly_landed(experiment: &ChaosExperiment, outcome: &ExperimentOutcome) -> bool {
    if outcome.fault_phase_disrupted(experiment.steady, experiment.fault_window) {
        return true;
    }
    let steady_p95 = outcome.phase_p95_ms(Duration::ZERO, experiment.steady);
    // Include one window of slack: stalled turns complete just after heal.
    let fault_p95 = outcome.phase_p95_ms(
        experiment.steady,
        experiment.steady + experiment.fault_window + Duration::from_secs(10),
    );
    fault_p95 > steady_p95 * 1.5 + 2_000.0
}

/// Standard post-experiment assertions shared by every scenario: the fault
/// actually landed, the system recovered, and no durability invariant broke.
pub async fn assert_experiment_clean(experiment: &ChaosExperiment, outcome: &ExperimentOutcome) {
    assert!(
        fault_visibly_landed(experiment, outcome),
        "{}: fault left no error, throughput, or latency evidence; experiment is vacuous",
        outcome.name
    );
    outcome
        .assert_recovered()
        .expect("system should recover after heal");
    let violations = sweep_invariants(outcome).await;
    assert!(
        violations.is_empty(),
        "{}: durability invariants violated: {violations:#?}",
        outcome.name
    );
}
