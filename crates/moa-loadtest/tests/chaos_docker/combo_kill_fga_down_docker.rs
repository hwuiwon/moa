//! Multi-failure combo: orchestrator SIGKILL while OpenFGA is already down.

use moa_loadtest::scenarios::chaos;

use crate::chaos_docker_support::{require_chaos_env, stack_config, sweep_invariants};

#[tokio::test]
#[ignore = "chaos _docker lane: compose stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_orchestrator_kill_while_openfga_down_replays_cleanly_docker() {
    // Pins: killing the orchestrator during an authz outage still replays
    // in-flight turns exactly once after both services return (OpenFGA is
    // healed first so orchestrator readiness never races a dead authz
    // backend), with a drained outbox and no dead letters.
    require_chaos_env();
    let cfg = stack_config();
    let experiment = chaos::combo_orchestrator_kill_while_openfga_down();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    assert!(
        outcome.fault_phase_disrupted(experiment.steady, experiment.fault_window)
            || outcome.report.errors.session_setup_failures > 0,
        "combined faults never visibly landed: {:?}",
        outcome.report.errors
    );
    outcome
        .assert_recovered()
        .expect("system recovers after both services return");
    let violations = sweep_invariants(&outcome).await;
    assert!(
        violations.is_empty(),
        "durability invariants violated: {violations:#?}"
    );
}
