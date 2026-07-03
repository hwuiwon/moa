//! Minimal Toxiproxy HTTP API client for network fault injection.
//!
//! The chaos overlay (`docker-compose.chaos.yml`) routes the orchestrator's
//! Postgres and OpenFGA connections through toxiproxy; this client adds and
//! removes toxics on those routes during experiments.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Toxiproxy control-API client.
#[derive(Debug, Clone)]
pub struct Toxiproxy {
    base_url: String,
    http: reqwest::Client,
}

impl Toxiproxy {
    /// Creates a client for the control API (host port 10060 in the overlay).
    pub fn new(base_url: impl Into<String>) -> Result<Self> {
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .context("toxiproxy client")?,
        })
    }

    /// True when the control API answers; used to fail fast with guidance
    /// when the chaos overlay is not running.
    pub async fn available(&self) -> bool {
        let url = format!("{}/version", self.base_url);
        matches!(self.http.get(url).send().await, Ok(response) if response.status().is_success())
    }

    /// Adds a downstream latency toxic to a proxy.
    pub async fn add_latency(
        &self,
        proxy: &str,
        toxic_name: &str,
        latency: Duration,
        jitter: Duration,
    ) -> Result<()> {
        let url = format!("{}/proxies/{proxy}/toxics", self.base_url);
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({
                "name": toxic_name,
                "type": "latency",
                "stream": "downstream",
                "toxicity": 1.0,
                "attributes": {
                    "latency": latency.as_millis() as u64,
                    "jitter": jitter.as_millis() as u64,
                },
            }))
            .send()
            .await
            .context("toxiproxy add latency")?;
        if !response.status().is_success() {
            bail!(
                "toxiproxy add latency on {proxy} returned {}",
                response.status()
            );
        }
        Ok(())
    }

    /// Removes a named toxic from a proxy.
    pub async fn remove_toxic(&self, proxy: &str, toxic_name: &str) -> Result<()> {
        let url = format!("{}/proxies/{proxy}/toxics/{toxic_name}", self.base_url);
        let response = self
            .http
            .delete(url)
            .send()
            .await
            .context("toxiproxy remove toxic")?;
        if !response.status().is_success() && response.status() != reqwest::StatusCode::NOT_FOUND {
            bail!(
                "toxiproxy remove toxic {toxic_name} on {proxy} returned {}",
                response.status()
            );
        }
        Ok(())
    }

    /// Enables or disables a proxy; disabled means a full partition.
    pub async fn set_enabled(&self, proxy: &str, enabled: bool) -> Result<()> {
        let url = format!("{}/proxies/{proxy}", self.base_url);
        let response = self
            .http
            .post(url)
            .json(&serde_json::json!({ "enabled": enabled }))
            .send()
            .await
            .context("toxiproxy set enabled")?;
        if !response.status().is_success() {
            bail!(
                "toxiproxy set enabled={enabled} on {proxy} returned {}",
                response.status()
            );
        }
        Ok(())
    }
}
