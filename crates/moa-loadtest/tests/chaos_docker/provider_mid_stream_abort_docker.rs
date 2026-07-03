//! Mid-stream provider aborts: partially streamed turns must fail cleanly
//! with consistent history and never duplicate assistant output.

use moa_loadtest::scenarios::chaos;

use crate::chaos_docker_support::{require_chaos_env, stack_config, sweep_invariants};

#[tokio::test]
#[ignore = "chaos _docker lane: compose stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_provider_mid_stream_abort_fails_turns_cleanly_docker() {
    // Pins: streams that die after the first block end as failed turns; the
    // event log stays gapless with no duplicated sequence numbers, and turns
    // succeed again once the abort budget is exhausted.
    require_chaos_env();
    let cfg = stack_config();
    let experiment = chaos::provider_mid_stream_abort();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    assert!(
        outcome.report.errors.failed_turns() > 0,
        "abort fault never engaged: {:?}",
        outcome.report.errors
    );
    outcome
        .assert_recovered()
        .expect("turns recover after the abort budget drains");
    let violations = sweep_invariants(&outcome).await;
    assert!(
        violations.is_empty(),
        "durability invariants violated: {violations:#?}"
    );
}
