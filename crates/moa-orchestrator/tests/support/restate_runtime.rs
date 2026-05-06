//! Helpers for spawning isolated Restate-backed orchestrator test runtimes.

use std::net::TcpListener;

use anyhow::{Context, Result};
use tokio::sync::Mutex;

/// Serializes ignored Restate e2e tests that share the same local Restate server.
pub static RESTATE_E2E_LOCK: Mutex<()> = Mutex::const_new(());

/// Freshly reserved ports for one orchestrator test process.
#[derive(Debug, Clone, Copy)]
pub struct OrchestratorPorts {
    /// Restate handler ingress port passed to `moa-orchestrator --port`.
    pub restate: u16,
    /// Probe server port passed to `moa-orchestrator --health-port`.
    pub health: u16,
}

/// Reserves a unique pair of localhost ports for one orchestrator test process.
pub fn reserve_orchestrator_ports() -> Result<OrchestratorPorts> {
    Ok(OrchestratorPorts {
        restate: reserve_port().context("reserve Restate handler port")?,
        health: reserve_port().context("reserve health probe port")?,
    })
}

fn reserve_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("bind ephemeral localhost listener")?;
    let port = listener
        .local_addr()
        .context("read ephemeral listener address")?
        .port();
    drop(listener);
    Ok(port)
}
