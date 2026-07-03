//! Orchestrator SIGKILL mid-turn: durable replay must finish or fail every
//! started turn exactly once.

use moa_loadtest::scenarios::chaos;

use crate::chaos_docker_support::{assert_experiment_clean, require_chaos_env, stack_config};

#[tokio::test]
#[ignore = "chaos _docker lane: compose stack + MOA_RUN_CHAOS_TESTS=1"]
async fn chaos_orchestrator_kill_mid_turn_replays_without_duplicate_history_docker() {
    // Pins: after a SIGKILL with turns in flight, Restate replays
    // TurnExecution journals on restart — every started turn reaches a
    // terminal outcome, the event log stays gapless with no duplicated
    // sequence numbers or tool calls, and throughput recovers.
    require_chaos_env();
    let cfg = stack_config();
    let experiment = chaos::orchestrator_kill_mid_turn();

    let outcome = chaos::run_experiment(&experiment, &cfg)
        .await
        .expect("experiment run completes");

    assert_experiment_clean(&experiment, &outcome).await;
}
