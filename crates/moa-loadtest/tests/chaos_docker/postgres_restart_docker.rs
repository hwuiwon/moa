//! Postgres restart under load: connection retry plus Restate handler
//! retries must bridge the outage without losing or duplicating events.

use moa_loadtest::scenarios::chaos;

use crate::chaos_docker_support::{assert_experiment_clean, require_chaos_env, stack_config};

#[tokio::test]
#[ignore = "chaos _docker lane: compose stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_postgres_restart_bridges_outage_without_event_loss_docker() {
    // Pins: a Postgres restart mid-load stalls or fails turns during the
    // outage, but after pg_isready the backlog drains, the final window is
    // error-free, and the event log has no gaps or duplicates.
    require_chaos_env();
    let cfg = stack_config();
    let experiment = chaos::postgres_restart();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    assert_experiment_clean(&experiment, &outcome).await;
}
