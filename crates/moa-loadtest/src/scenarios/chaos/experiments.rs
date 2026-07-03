//! The standard chaos experiment matrix.
//!
//! Each function returns one hypothesis-driven experiment. Hypotheses:
//!
//! - `orchestrator_kill_mid_turn`: SIGKILL during in-flight turns; Restate
//!   replays `TurnExecution` journals after restart, so every started turn
//!   still reaches a terminal outcome and no session event is duplicated.
//! - `postgres_restart`: the session store reconnects with backoff and
//!   Restate retries handlers, so the backlog drains with no lost events.
//! - `openfga_outage`: authz fails closed while OpenFGA is down (session
//!   setup fails, nothing is silently allowed) and the outbox drains after
//!   recovery with no dead letters.
//! - `provider_storm`: a burst of provider 429s degrades turns but never
//!   corrupts the event log; the system recovers once the storm passes.
//! - `provider_mid_stream_abort`: streams that die after the first block end
//!   as failed turns with consistent history, never duplicated output.

use std::time::Duration;

use super::{ChaosExperiment, Fault};

/// SIGKILL the orchestrator with turns in flight.
pub fn orchestrator_kill_mid_turn() -> ChaosExperiment {
    ChaosExperiment {
        name: "orchestrator_kill_mid_turn",
        provider_script: Some("/loadtest-scripts/realistic.json"),
        fault: Fault::KillService("moa-orchestrator"),
        steady: Duration::from_secs(20),
        fault_window: Duration::from_secs(10),
        recovery: Duration::from_secs(60),
        rate: 5.0,
        sessions: 20,
    }
}

/// Restart Postgres under load.
pub fn postgres_restart() -> ChaosExperiment {
    ChaosExperiment {
        name: "postgres_restart",
        provider_script: Some("/loadtest-scripts/realistic.json"),
        fault: Fault::RestartService("postgres"),
        steady: Duration::from_secs(20),
        fault_window: Duration::from_secs(15),
        recovery: Duration::from_secs(60),
        rate: 5.0,
        sessions: 20,
    }
}

/// Stop OpenFGA for the fault window.
pub fn openfga_outage() -> ChaosExperiment {
    ChaosExperiment {
        name: "openfga_outage",
        provider_script: Some("/loadtest-scripts/realistic.json"),
        fault: Fault::StopService("openfga"),
        steady: Duration::from_secs(20),
        fault_window: Duration::from_secs(20),
        recovery: Duration::from_secs(60),
        rate: 5.0,
        sessions: 20,
    }
}

/// Provider 429 storm driven by the fault-scripted provider; no container
/// fault is injected.
pub fn provider_storm() -> ChaosExperiment {
    ChaosExperiment {
        name: "provider_storm",
        provider_script: Some("/loadtest-scripts/chaos-provider-storm.json"),
        fault: Fault::ProviderScript,
        steady: Duration::from_secs(10),
        fault_window: Duration::from_secs(0),
        recovery: Duration::from_secs(80),
        rate: 4.0,
        sessions: 16,
    }
}

/// Provider streams abort after the first block for one keyed prompt.
pub fn provider_mid_stream_abort() -> ChaosExperiment {
    ChaosExperiment {
        name: "provider_mid_stream_abort",
        provider_script: Some("/loadtest-scripts/chaos-mid-stream-abort.json"),
        fault: Fault::ProviderScript,
        steady: Duration::from_secs(10),
        fault_window: Duration::from_secs(0),
        recovery: Duration::from_secs(60),
        rate: 4.0,
        sessions: 16,
    }
}
