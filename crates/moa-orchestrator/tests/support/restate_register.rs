//! Restate deployment registration helper.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use serde_json::json;

/// Register a spawned test deployment with Restate admin over HTTP.
pub async fn register_deployment(admin_url: &str, deployment_uri: &str) -> Result<()> {
    match register_deployment_once(admin_url, deployment_uri).await {
        Ok(()) => Ok(()),
        Err(error) if should_retry_with_host_gateway(deployment_uri) => {
            let fallback_uri = host_gateway_deployment_uri(deployment_uri);
            register_deployment_once(admin_url, &fallback_uri)
                .await
                .with_context(|| {
                    format!(
                        "register deployment {deployment_uri} failed before host-gateway fallback: {error}"
                    )
                })
        }
        Err(error) => Err(error),
    }
}

async fn register_deployment_once(admin_url: &str, deployment_uri: &str) -> Result<()> {
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

fn should_retry_with_host_gateway(deployment_uri: &str) -> bool {
    std::env::var("MOA_RESTATE_DEPLOYMENT_HOST").is_err()
        && (deployment_uri.starts_with("http://127.0.0.1:")
            || deployment_uri.starts_with("http://localhost:"))
}

fn host_gateway_deployment_uri(deployment_uri: &str) -> String {
    deployment_uri
        .replacen("http://127.0.0.1:", "http://host.docker.internal:", 1)
        .replacen("http://localhost:", "http://host.docker.internal:", 1)
}
