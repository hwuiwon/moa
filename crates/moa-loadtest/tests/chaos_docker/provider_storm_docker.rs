//! Provider 429 storm through the real orchestrator: rate-limit errors must
//! degrade turns without corrupting history, then recover.

use moa_loadtest::scenarios::chaos;

use crate::chaos_docker_support::{require_chaos_env, stack_config, sweep_invariants};

#[tokio::test]
#[ignore = "chaos _docker lane: compose stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_provider_429_storm_degrades_then_recovers_docker() {
    // Pins: a 30-deep 429 budget on one keyed prompt visibly degrades turns
    // through the full orchestrator path — either as failed turns, or (when
    // gateway retries absorb the storm, the observed healthy behavior) as a
    // multi-x latency bulge that clears by the final window. History never
    // corrupts either way.
    require_chaos_env();
    let cfg = stack_config();
    let experiment = chaos::provider_storm();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    assert!(
        outcome.report.errors.failed_turns() > 0 || outcome.degradation_ratio() > 2.5,
        "storm never engaged: errors {:?}, degradation ratio {:.2}",
        outcome.report.errors,
        outcome.degradation_ratio()
    );
    outcome
        .assert_recovered()
        .expect("turns recover after the 429 budget drains");
    let violations = sweep_invariants(&outcome).await;
    assert!(
        violations.is_empty(),
        "durability invariants violated: {violations:#?}"
    );
}
