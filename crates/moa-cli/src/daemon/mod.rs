//! Diagnostic helpers for the configured orchestrator endpoint.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_core::MoaConfig;
use moa_orchestrator_client::OrchestratorClient;

const DEFAULT_ORCHESTRATOR_ENDPOINT: &str = "http://localhost:18080";

/// Returns the configured Restate ingress endpoint for CLI traffic.
pub(crate) fn orchestrator_endpoint(config: &MoaConfig) -> &str {
    config
        .orchestrator
        .endpoint
        .as_deref()
        .unwrap_or(DEFAULT_ORCHESTRATOR_ENDPOINT)
}

/// Returns the configured or derived orchestrator health URL.
pub(crate) fn orchestrator_health_url(config: &MoaConfig) -> String {
    config
        .orchestrator
        .health_url
        .clone()
        .unwrap_or_else(|| derive_health_url(orchestrator_endpoint(config)))
}

/// Derives the direct orchestrator health URL from a Restate ingress endpoint.
pub(crate) fn derive_health_url(endpoint: &str) -> String {
    if let Ok(url) = reqwest::Url::parse(endpoint)
        && let (Some(host), Some(port)) = (url.host_str(), url.port())
        && port == 18080
    {
        return format!("http://{host}:9081/_health/live");
    }
    format!("{}/_health/live", endpoint.trim_end_matches('/'))
}

/// Checks the configured orchestrator process health endpoint.
pub(crate) async fn health_check(config: &MoaConfig) -> Result<()> {
    let health_url = orchestrator_health_url(config);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .context("building health-check HTTP client")?;
    let response = client
        .get(&health_url)
        .send()
        .await
        .with_context(|| unreachable_hint(&health_url))?;
    if !response.status().is_success() {
        bail!(
            "orchestrator returned status {} from {health_url}",
            response.status()
        );
    }
    Ok(())
}

/// Builds a thin orchestrator client from the loaded config.
pub(crate) fn build_client(config: &MoaConfig) -> Result<OrchestratorClient> {
    OrchestratorClient::new(orchestrator_endpoint(config)).with_context(|| {
        format!(
            "orchestrator endpoint not usable: {}.\n\
             Set MOA__ORCHESTRATOR__ENDPOINT or [orchestrator].endpoint.\n\
             For local dev, run `make dev` and use http://localhost:18080.",
            orchestrator_endpoint(config)
        )
    })
}

/// Returns the diagnostic hint used when the orchestrator cannot be reached.
pub(crate) fn unreachable_hint(health_url: &str) -> String {
    format!(
        "could not reach orchestrator at {health_url}.\n\
         hint: run `make dev` to start the local stack, or set \
         MOA__ORCHESTRATOR__ENDPOINT to a reachable orchestrator."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_ingress_derives_health_port() {
        // Pins: default compose Restate ingress maps to the orchestrator health port.
        assert_eq!(
            derive_health_url("http://localhost:18080"),
            "http://localhost:9081/_health/live"
        );
    }

    #[test]
    fn non_compose_endpoint_uses_endpoint_health_path() {
        // Pins: custom endpoints get a same-origin health path unless health_url is explicit.
        assert_eq!(
            derive_health_url("https://moa.example.com/"),
            "https://moa.example.com/_health/live"
        );
    }
}
