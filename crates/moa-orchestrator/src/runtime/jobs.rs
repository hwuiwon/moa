//! Background job wiring for orchestrator startup.

use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{Context as AnyhowContext, Result, bail};
use moa_authz::{AwakeableResolver, FgaClient};
use moa_core::{MoaConfig, config::AsyncAuthzKind};
use reqwest::Client;
use sqlx::PgPool;
use tokio::task::JoinHandle;

use crate::{
    runtime::endpoint::{RegisteredDeployment, services_registered},
    services::authz_challenges_reaper::{AuthzChallengeReaper, AuthzChallengeReaperHandle},
};

const DEFAULT_RESTATE_INGRESS_PORT: u16 = 8080;
const CRON_BOOTSTRAP_ATTEMPTS: u32 = 60;
const CRON_BOOTSTRAP_INTERVAL: Duration = Duration::from_secs(2);

/// Starts the OpenFGA outbox poller that drains queued authorization tuple changes.
pub fn start_authz_outbox_poller(pool: &PgPool, fga_client: FgaClient) -> moa_authz::PollerHandle {
    let outbox_poller =
        moa_authz::OutboxPoller::new(pool.clone(), fga_client, moa_authz::PollerConfig::default());
    let poller_handle = outbox_poller.spawn();
    tracing::info!("authz outbox poller started");
    poller_handle
}

/// Starts the builtin async-authorization challenge reaper when configured.
pub fn start_authz_challenge_reaper_if_configured(
    pool: &PgPool,
    config: &MoaConfig,
    resolver: Arc<dyn AwakeableResolver>,
) -> Result<Option<AuthzChallengeReaperHandle>> {
    if config.async_authz.provider != AsyncAuthzKind::Builtin {
        return Ok(None);
    }
    let handle = AuthzChallengeReaper::new(pool.clone()).spawn(resolver);
    tracing::info!("authz challenge reaper started");
    Ok(Some(handle))
}

/// Spawns default cron-job bootstrap after Restate service registration appears.
pub fn spawn_default_cron_bootstrap<F, Fut>(
    mut fetch_deployments: F,
    ingress_url: String,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = Result<Vec<RegisteredDeployment>>> + Send + 'static,
{
    tokio::spawn(async move {
        for attempt in 1..=CRON_BOOTSTRAP_ATTEMPTS {
            match fetch_deployments().await {
                Ok(deployments) if services_registered(&deployments) => {
                    if let Err(error) = install_default_cron_jobs(&ingress_url).await {
                        tracing::warn!(
                            error = %error,
                            "failed to install default cron jobs; will not retry"
                        );
                    }
                    return;
                }
                Ok(_) => tracing::debug!(
                    attempt,
                    "waiting for Restate service registration before cron bootstrap"
                ),
                Err(error) => tracing::debug!(
                    attempt,
                    error = %error,
                    "failed to check Restate registration before cron bootstrap"
                ),
            }
            tokio::time::sleep(CRON_BOOTSTRAP_INTERVAL).await;
        }

        tracing::warn!(
            attempts = CRON_BOOTSTRAP_ATTEMPTS,
            "default cron job bootstrap timed out waiting for Restate registration"
        );
    })
}

/// Normalizes the Restate ingress URL used by cron bootstrap and awakeables.
#[must_use]
pub fn restate_ingress_base_url(configured_ingress_url: &str) -> String {
    let trimmed = configured_ingress_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return format!("http://localhost:{DEFAULT_RESTATE_INGRESS_PORT}");
    }
    trimmed.to_string()
}

async fn install_default_cron_jobs(ingress_url: &str) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build cron-bootstrap HTTP client")?;
    let ingress_url = ingress_url.trim_end_matches('/');

    for job in default_cron_jobs() {
        let response = client
            .post(format!(
                "{ingress_url}/restate/call/CronJob/{}/configure",
                job.key
            ))
            .header(
                "idempotency-key",
                format!("cron-config-{}-{}", job.key, job.version),
            )
            .header("content-type", "application/json")
            .json(&job.body)
            .send()
            .await
            .with_context(|| format!("configure cron job {}", job.key))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => format!("<failed to read response body: {error}>"),
            };
            bail!("cron configure {} returned {status}: {text}", job.key);
        }

        tracing::info!(key = job.key, "cron job configured");
    }

    Ok(())
}

struct DefaultCronJob {
    key: &'static str,
    body: serde_json::Value,
    version: &'static str,
}

fn default_cron_jobs() -> Vec<DefaultCronJob> {
    vec![
        DefaultCronJob {
            key: "graph_memory_compact",
            body: serde_json::json!({
                "schedule": "0 0 * * * *",
                "timezone": "UTC",
                "target_service": "GraphMemoryMaint",
                "target_handler": "compact",
                "payload": {}
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "segment_materialized_views_refresh",
            body: serde_json::json!({
                "schedule": "0 */15 * * * *",
                "timezone": "UTC",
                "target_service": "SessionStore",
                "target_handler": "refresh_segment_materialized_views",
                "payload": {}
            }),
            version: "v1",
        },
        DefaultCronJob {
            key: "neon_prune_branches",
            body: serde_json::json!({
                "schedule": "0 0 0,6,12,18 * * *",
                "timezone": "UTC",
                "target_service": "NeonMaint",
                "target_handler": "prune_branches",
                "payload": null
            }),
            version: "v1",
        },
    ]
}
