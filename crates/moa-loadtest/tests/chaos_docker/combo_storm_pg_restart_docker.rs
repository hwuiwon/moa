//! Multi-failure combo: provider 429 storm overlapping a Postgres restart.

use moa_loadtest::scenarios::chaos;

use crate::chaos_docker_support::{require_chaos_env, stack_config, sweep_invariants};

#[tokio::test]
#[ignore = "chaos _docker lane: compose stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_provider_storm_overlapping_postgres_restart_stays_consistent_docker() {
    // Pins: two simultaneous failure domains (provider rate limits + session
    // store outage) still cannot corrupt the event log, and the system
    // recovers once both clear.
    require_chaos_env();
    let cfg = stack_config();
    let experiment = chaos::combo_provider_storm_during_postgres_restart();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    assert!(
        outcome.report.errors.failed_turns() > 0,
        "combined faults never engaged: {:?}",
        outcome.report.errors
    );
    outcome
        .assert_recovered()
        .expect("system recovers after both faults clear");
    let violations = sweep_invariants(&outcome).await;
    assert!(
        violations.is_empty(),
        "durability invariants violated: {violations:#?}"
    );
}
