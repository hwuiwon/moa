//! Port helpers for spawned Restate-backed orchestrator tests.

use std::net::TcpListener;

use anyhow::{Context, Result};

/// Freshly reserved ports for one orchestrator test process.
#[derive(Debug, Clone, Copy)]
pub struct OrchestratorPorts {
    /// Restate handler ingress port passed to `moa-orchestrator --port`.
    pub restate: u16,
    /// Probe server port passed to `moa-orchestrator --health-port`.
    pub health: u16,
    /// SCIM endpoint port passed to `moa-orchestrator --scim-port`.
    pub scim: u16,
}

/// Reserves a unique set of localhost ports for one orchestrator test process.
pub fn reserve_orchestrator_ports() -> Result<OrchestratorPorts> {
    Ok(OrchestratorPorts {
        restate: reserve_port().context("reserve Restate handler port")?,
        health: reserve_port().context("reserve health probe port")?,
        scim: reserve_port().context("reserve SCIM endpoint port")?,
    })
}

/// Return the deployment URL Restate should use to discover a spawned test server.
pub fn deployment_endpoint_url(port: u16) -> String {
    let host =
        std::env::var("MOA_RESTATE_DEPLOYMENT_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    format!("http://{host}:{port}")
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
