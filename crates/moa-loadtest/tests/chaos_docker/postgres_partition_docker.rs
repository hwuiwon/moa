//! Postgres partition (Toxiproxy overlay): a full network hole must not
//! lose or duplicate events.

use moa_loadtest::scenarios::chaos::{self, Toxiproxy};

use crate::chaos_docker_support::{assert_experiment_clean, require_chaos_env, stack_config};

#[tokio::test]
#[ignore = "chaos _docker lane: chaos overlay stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_postgres_partition_drains_backlog_without_event_loss_docker() {
    // Pins: a 10s Postgres partition stalls/fails turns; after the proxy is
    // re-enabled the backlog drains, the last window is error-free, and the
    // event log has no gaps or duplicates.
    require_chaos_env();
    let cfg = stack_config();
    let toxiproxy = Toxiproxy::new(&cfg.toxiproxy_url).expect("toxiproxy client");
    assert!(
        toxiproxy.available().await,
        "toxiproxy API unreachable; bring the stack up with \
         `docker compose -f docker-compose.yml -f docker-compose.chaos.yml up -d`"
    );
    let experiment = chaos::postgres_partition();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    assert_experiment_clean(&experiment, &outcome).await;
}
