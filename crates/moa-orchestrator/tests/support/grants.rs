//! OpenFGA grant helpers for Restate e2e tests.
//!
//! All grants share one raw-tuple writer and one live OpenFGA client; the
//! public helpers differ only in the relation and object they target.

use anyhow::{Context, Result};
use moa_authz::{FgaClient, FgaConfig};
use moa_authz_schema::TupleOp;
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::identifiers::{ConnectorConnectionId, SessionId};
use serde_json::json;

/// Grant the test identity tenant-admin access directly in live OpenFGA.
pub async fn grant_tenant_admin(
    identity: &Identity,
    tenant_id: impl std::fmt::Display,
) -> Result<()> {
    apply_raw_tuple(
        TupleOp::Write,
        &format!("operator:{}", identity.id),
        "admin",
        &format!("tenant:{tenant_id}"),
    )
    .await
    .context("grant test tenant admin")
}

/// Grant the test identity tenant-operator access directly in live OpenFGA.
pub async fn grant_tenant_operator(
    identity: &Identity,
    tenant_id: impl std::fmt::Display,
) -> Result<()> {
    apply_raw_tuple(
        TupleOp::Write,
        &format!("operator:{}", identity.id),
        "operator",
        &format!("tenant:{tenant_id}"),
    )
    .await
    .context("grant test tenant operator")
}

/// Grant the test identity direct participation in one session.
pub async fn grant_session_participant(identity: &Identity, session_id: SessionId) -> Result<()> {
    if let (IdentityType::Agent, Some(operator_id)) =
        (identity.identity_type, identity.acting_on_behalf_of)
    {
        apply_raw_tuple(
            TupleOp::Write,
            &format!("operator:{operator_id}"),
            "can_act_as",
            &format!("agent:{}", identity.id),
        )
        .await
        .context("grant test agent delegation")?;
    }
    apply_raw_tuple(
        TupleOp::Write,
        &format!("{}:{}", identity.identity_type.as_str(), identity.id),
        "participant",
        &format!("session:{session_id}"),
    )
    .await
    .context("grant test session participation")
}

/// Grant the test identity direct use of one tenant connector connection.
pub async fn grant_connector_connection_use(
    identity: &Identity,
    connection_id: ConnectorConnectionId,
) -> Result<()> {
    apply_raw_tuple(
        TupleOp::Write,
        &format!("{}:{}", identity.identity_type.as_str(), identity.id),
        "use",
        &format!("connector_connection:{connection_id}"),
    )
    .await
    .context("grant test connector connection use")
}

/// Write or delete a single raw tuple against live OpenFGA.
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

/// Build an OpenFGA client pointed at the local dev/live OpenFGA instance.
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
