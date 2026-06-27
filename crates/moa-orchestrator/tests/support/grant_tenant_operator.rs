//! OpenFGA grant helper for tenant operator access.

use anyhow::{Context, Result};
use moa_authz::{FgaClient, FgaConfig};
use moa_authz_schema::TupleOp;
use moa_core::traits::Identity;
use serde_json::json;

/// Grant the test identity tenant-operator access directly in live OpenFGA.
pub async fn grant_tenant_operator(
    identity: &Identity,
    tenant_id: impl std::fmt::Display,
) -> Result<()> {
    apply_raw_tuple(
        TupleOp::Write,
        &format!("user:{}", identity.id),
        "operator",
        &format!("tenant:{tenant_id}"),
    )
    .await
    .context("grant test tenant operator")
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
