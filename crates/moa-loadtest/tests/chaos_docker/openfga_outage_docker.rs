//! OpenFGA outage: authz must fail closed during the outage and the outbox
//! must drain with no dead letters after recovery.

use moa_loadtest::scenarios::chaos;

use crate::chaos_docker_support::{require_chaos_env, stack_config, sweep_invariants};

#[tokio::test]
#[ignore = "chaos _docker lane: compose stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_openfga_outage_fails_closed_and_outbox_drains_docker() {
    // Pins: while OpenFGA is stopped, new work is denied (session setup or
    // turn failures — never silent allows), and after restart the authz
    // outbox is fully drained with an empty dead-letter set.
    require_chaos_env();
    let cfg = stack_config();
    let experiment = chaos::openfga_outage();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    let errors = &outcome.report.errors;
    assert!(
        errors.session_setup_failures > 0
            || outcome.fault_phase_disrupted(experiment.steady, experiment.fault_window),
        "openfga outage produced no visible denial; fail-closed behavior unverified: {errors:?}"
    );
    outcome
        .assert_recovered()
        .expect("authz recovers after OpenFGA restart");
    let violations = sweep_invariants(&outcome).await;
    assert!(
        violations.is_empty(),
        "outbox/durability invariants violated: {violations:#?}"
    );
}
