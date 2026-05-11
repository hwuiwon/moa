//! Shared CLI client construction helpers.

use anyhow::{Context, Result};
use moa_orchestrator_client::OrchestratorClient;

/// Build an orchestrator client that authenticates to `moa-edge` with the stored API key.
pub(crate) async fn client_from_credentials() -> Result<OrchestratorClient> {
    let path = credentials_path()?;
    let raw = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read {}; did you run `moa auth use-key`?", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    let key = value
        .get("api_key")
        .and_then(|value| value.as_str())
        .context("api_key missing from credentials.json")?;
    let base = std::env::var("MOA_EDGE_URL").unwrap_or_else(|_| "http://localhost:10000".into());
    Ok(OrchestratorClient::new(base)?.with_bearer(key.to_string()))
}

/// Return the default MOA CLI credentials path.
pub(crate) fn credentials_path() -> Result<std::path::PathBuf> {
    let home = dirs::home_dir().context("home directory unavailable")?;
    Ok(home.join(".moa").join("credentials.json"))
}
