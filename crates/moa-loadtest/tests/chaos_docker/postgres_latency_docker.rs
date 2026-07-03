//! Postgres latency injection (Toxiproxy overlay): the pool must absorb
//! +200ms per read without collapsing.

use moa_loadtest::scenarios::chaos::{self, Toxiproxy};

use crate::chaos_docker_support::{require_chaos_env, stack_config, sweep_invariants};

#[tokio::test]
#[ignore = "chaos _docker lane: chaos overlay stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_postgres_latency_slows_turns_without_failures_docker() {
    // Pins: +200ms/read on the Postgres route visibly raises corrected p95
    // during the fault phase, produces no invariant violations, and latency
    // recovers once the toxic is removed.
    require_chaos_env();
    let cfg = stack_config();
    let toxiproxy = Toxiproxy::new(&cfg.toxiproxy_url).expect("toxiproxy client");
    assert!(
        toxiproxy.available().await,
        "toxiproxy API unreachable; bring the stack up with \
         `docker compose -f docker-compose.yml -f docker-compose.chaos.yml up -d`"
    );
    let experiment = chaos::postgres_latency();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    let steady_p95 = outcome.phase_p95_ms(std::time::Duration::ZERO, experiment.steady);
    let fault_p95 = outcome.phase_p95_ms(
        experiment.steady,
        experiment.steady + experiment.fault_window,
    );
    assert!(
        fault_p95 > steady_p95 + 150.0,
        "latency toxic did not propagate: steady p95 {steady_p95:.0}ms vs fault p95 {fault_p95:.0}ms"
    );
    outcome
        .assert_recovered()
        .expect("latency returns to healthy turns after the toxic is removed");
    let violations = sweep_invariants(&outcome).await;
    assert!(
        violations.is_empty(),
        "durability invariants violated: {violations:#?}"
    );
}
