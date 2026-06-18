//! Helpers for spawning isolated Restate-backed orchestrator test runtimes.

use std::net::TcpListener;

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use moa_authz::{FgaClient, FgaConfig};
use moa_authz_schema::TupleOp;
use moa_core::SessionId;
use moa_core::traits::{Identity, IdentityType};
use reqwest::StatusCode;
use serde_json::json;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Serializes ignored Restate e2e tests that share the same local Restate server.
pub static RESTATE_E2E_LOCK: Mutex<()> = Mutex::const_new(());

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

/// Reserves a unique pair of localhost ports for one orchestrator test process.
pub fn reserve_orchestrator_ports() -> Result<OrchestratorPorts> {
    Ok(OrchestratorPorts {
        restate: reserve_port().context("reserve Restate handler port")?,
        health: reserve_port().context("reserve health probe port")?,
        scim: reserve_port().context("reserve SCIM endpoint port")?,
    })
}

/// Return the deployment URL Restate should use to discover a spawned test server.
///
/// When Restate runs in Docker, `127.0.0.1` points at the container itself.
/// Set `MOA_RESTATE_DEPLOYMENT_HOST=host.docker.internal` for compose-based
/// live e2e runs so Restate can reach the host-spawned orchestrator process.
pub fn deployment_endpoint_url(port: u16) -> String {
    let host =
        std::env::var("MOA_RESTATE_DEPLOYMENT_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    format!("http://{host}:{port}")
}

/// Return the Restate ingress URL used by e2e tests.
///
/// The compose stack exposes ingress at `10010`; a host `restate-server`
/// usually exposes it at `8080`. Set `MOA_RESTATE_INGRESS_URL` to select the
/// active topology.
pub fn restate_ingress_url() -> String {
    std::env::var("MOA_RESTATE_INGRESS_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:10010".to_string())
}

/// Return the Restate admin URL used by e2e tests.
pub fn restate_admin_url() -> String {
    std::env::var("MOA_RESTATE_ADMIN_URL").unwrap_or_else(|_| "http://127.0.0.1:10011".to_string())
}

/// Register a spawned test deployment with Restate admin over HTTP.
pub async fn register_deployment(admin_url: &str, deployment_uri: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let body = json!({ "uri": deployment_uri });
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match client
            .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
            .json(&body)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if response.status() == StatusCode::CONFLICT => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(
                    status = %response.status(),
                    deployment_uri,
                    "waiting to register Restate deployment"
                );
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(
                    %error,
                    deployment_uri,
                    "waiting to register Restate deployment"
                );
            }
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                bail!("register deployment {deployment_uri} returned {status}: {text}");
            }
            Err(error) => return Err(error).context("register deployment with Restate admin"),
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Return a fresh user identity suitable for direct Restate e2e calls.
pub fn test_user_identity() -> Identity {
    Identity {
        identity_type: IdentityType::User,
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

/// Attach trusted MOA identity headers to a Restate ingress request.
pub fn with_identity(
    request: reqwest::RequestBuilder,
    identity: &Identity,
) -> reqwest::RequestBuilder {
    request
        .header("x-moa-identity-type", "user")
        .header("x-moa-identity-id", identity.id.to_string())
        .header("x-moa-tenant-id", identity.tenant_id.to_string())
}

/// Grant the test identity workspace membership directly in live OpenFGA.
pub async fn grant_workspace_member(
    identity: &Identity,
    workspace_id: impl std::fmt::Display,
) -> Result<()> {
    apply_raw_tuple(
        TupleOp::Write,
        &format!("user:{}", identity.id),
        "member",
        &format!("workspace:{workspace_id}"),
    )
    .await
    .context("grant test workspace membership")
}

/// Grant the test identity workspace editor privileges directly in live OpenFGA.
pub async fn grant_workspace_editor(
    identity: &Identity,
    workspace_id: impl std::fmt::Display,
) -> Result<()> {
    apply_raw_tuple(
        TupleOp::Write,
        &format!("user:{}", identity.id),
        "editor",
        &format!("workspace:{workspace_id}"),
    )
    .await
    .context("grant test workspace editor")
}

/// Grant the test identity direct participation in one session.
pub async fn grant_session_participant(identity: &Identity, session_id: SessionId) -> Result<()> {
    apply_raw_tuple(
        TupleOp::Write,
        &format!("user:{}", identity.id),
        "participant",
        &format!("session:{session_id}"),
    )
    .await
    .context("grant test session participation")
}

async fn apply_raw_tuple(op: TupleOp, user: &str, relation: &str, object: &str) -> Result<()> {
    let fga = live_fga_client()?;
    let body = match op {
        TupleOp::Write => json!({
            "authorization_model_id": fga.model_id(),
            "writes": {
                "tuple_keys": [{
                    "user": user,
                    "relation": relation,
                    "object": object,
                }],
            },
        }),
        TupleOp::Delete => json!({
            "authorization_model_id": fga.model_id(),
            "deletes": {
                "tuple_keys": [{
                    "user": user,
                    "relation": relation,
                    "object": object,
                }],
            },
        }),
    };
    fga.apply_raw(body).await.context("apply raw OpenFGA tuple")
}

fn live_fga_client() -> Result<FgaClient> {
    FgaClient::new(FgaConfig {
        url: std::env::var("MOA_AUTHZ_OPENFGA_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:10030".to_string()),
        preshared_key: std::env::var("MOA_AUTHZ_OPENFGA_PRESHARED_KEY")
            .unwrap_or_else(|_| "localdev-preshared-key-do-not-use-in-prod".to_string()),
        store_id: std::env::var("MOA_AUTHZ_OPENFGA_STORE_ID")
            .context("MOA_AUTHZ_OPENFGA_STORE_ID")?,
        model_id: std::env::var("MOA_AUTHZ_OPENFGA_MODEL_ID")
            .context("MOA_AUTHZ_OPENFGA_MODEL_ID")?,
        timeout_ms: 5000,
    })
    .context("build live OpenFGA client")
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
