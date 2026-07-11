//! Restate-backed `moa-orchestrator` binary entrypoint.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context as AnyhowContext, bail};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Router, serve};
use clap::{Parser, Subcommand};
use moa_observability::{TelemetryConfig, init_observability, metrics_endpoint_url};
use moa_orchestrator::{
    config::{
        ProvidersOverride, load_moa_config_from_env, restate_admin_url, restate_ingress_url,
        skip_fga_from_env,
    },
    runtime::{
        channel_ingress::spawn_channel_ingress,
        database::{apply_database_migrations, build_database_pool, database_search_path},
        deps::RuntimeDeps,
        endpoint::{
            DeploymentListResponse, RegisteredDeployment, build_endpoint, services_registered,
        },
        jobs::{
            restate_ingress_base_url, spawn_default_cron_bootstrap,
            start_authz_challenge_reaper_if_configured,
        },
    },
    services::scim::{self, ScimState},
};
use reqwest::Client;
use restate_sdk::prelude::*;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DEFAULT_RESTATE_PORT: u16 = 10020;
const DEFAULT_HEALTH_PORT: u16 = 10021;
const DEFAULT_SCIM_PORT: u16 = 10022;
const ADMIN_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_DRAIN_DELAY: Duration = Duration::from_secs(5);
const ORCHESTRATOR_WORKER_STACK_SIZE: usize = 16 * 1024 * 1024;

/// Process arguments for the orchestrator process.
#[derive(Debug, Parser)]
struct Args {
    /// Run database migrations and exit without starting Restate services.
    #[command(subcommand)]
    command: Option<Command>,
    /// HTTP port for the Restate handler endpoint.
    #[arg(long, default_value_t = DEFAULT_RESTATE_PORT)]
    port: u16,
    /// HTTP port for Kubernetes liveness/readiness probes.
    #[arg(long, default_value_t = DEFAULT_HEALTH_PORT)]
    health_port: u16,
    /// HTTP port for SCIM v2 provisioning endpoints.
    #[arg(long, default_value_t = DEFAULT_SCIM_PORT)]
    scim_port: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
enum Command {
    /// Apply database migrations and exit.
    Migrate,
}

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        // Restate workflow futures carry large request, context, and tracing
        // state across awaits. Use an explicit worker stack so live workflow
        // execution does not depend on ambient RUST_MIN_STACK.
        .thread_stack_size(ORCHESTRATOR_WORKER_STACK_SIZE)
        .build()
        .context("build orchestrator Tokio runtime")?
        .block_on(async_main())
}

async fn async_main() -> anyhow::Result<()> {
    let args = Args::parse();
    let moa_config = load_moa_config_from_env()?;
    let skip_fga = skip_fga_from_env();
    let moa_config = Arc::new(moa_config);
    let _telemetry = init_observability(
        moa_config.as_ref(),
        &TelemetryConfig {
            json_stdout: true,
            ..TelemetryConfig::default()
        },
    )?;
    let database_search_path = database_search_path(moa_config.as_ref());
    let migration_pool = build_database_pool(
        moa_config.database.admin_url(),
        &database_search_path,
        moa_config.database.max_connections.clamp(1, 5),
        Duration::from_secs(moa_config.database.connect_timeout_seconds),
    )
    .await
    .context("connect migration database pool")?;
    apply_database_migrations(moa_config.as_ref(), &migration_pool).await?;
    drop(migration_pool);
    if args.command == Some(Command::Migrate) {
        return Ok(());
    }

    let restate_admin_url = restate_admin_url(moa_config.as_ref())?;
    let restate_ingress_url = restate_ingress_url(moa_config.as_ref())?;
    let providers_override = ProvidersOverride::from_env();
    providers_override.ensure_allowed(moa_config.as_ref())?;
    let pool = build_database_pool(
        moa_config.database.runtime_url(),
        &database_search_path,
        moa_config.database.max_connections,
        Duration::from_secs(moa_config.database.connect_timeout_seconds),
    )
    .await
    .context("connect runtime database pool")?;
    let mut runtime_deps = RuntimeDeps::build(
        moa_config.clone(),
        pool.clone(),
        &restate_ingress_url,
        providers_override,
        skip_fga,
    )
    .await?;
    runtime_deps
        .install_orchestrator_ctx()
        .map_err(anyhow::Error::msg)?;
    let scim_base_url = std::env::var("MOA_SCIM_BASE_URL")
        .unwrap_or_else(|_| format!("http://localhost:{}/scim/v2", args.scim_port));
    let scim_state = runtime_deps.scim_state(scim_base_url);

    let endpoint = build_endpoint(
        runtime_deps.session_store.clone(),
        runtime_deps.pool.clone(),
        runtime_deps.fga_client.clone(),
        runtime_deps.providers.clone(),
        runtime_deps.tool_router.clone(),
        runtime_deps.tool_schemas.clone(),
        moa_config.session_limits.clone(),
        moa_config.clone(),
        runtime_deps.auth_providers.contact_tokens.clone(),
        runtime_deps.lineage.handle.clone(),
        runtime_deps.embedding_provider.clone(),
        Arc::new(runtime_deps.channel_adapters.clone()),
    );

    let readiness = Arc::new(AtomicBool::new(false));
    let probe_state = ProbeState::new(readiness.clone(), pool.clone(), restate_admin_url)?;
    let shutdown = CancellationToken::new();
    let authz_challenge_reaper_handle = start_authz_challenge_reaper_if_configured(
        &pool,
        moa_config.as_ref(),
        runtime_deps.awakeable_resolver.clone(),
    )?;

    let restate_listener = bind_listener(args.port).await?;
    let health_listener = bind_listener(args.health_port).await?;
    let scim_listener = bind_listener(args.scim_port).await?;
    let mut restate_server = spawn_restate_server(endpoint, restate_listener, shutdown.clone());
    let mut health_server =
        spawn_health_server(health_listener, probe_state.clone(), shutdown.clone());
    let mut scim_server = spawn_scim_server(scim_listener, scim_state, shutdown.clone());
    let mut channel_ingress = spawn_channel_ingress(
        runtime_deps.channel_adapters.clone(),
        runtime_deps.session_store.clone(),
        restate_ingress_url.clone(),
        shutdown.clone(),
    );
    let mut analytics_export = moa_orchestrator::analytics_export::spawn_analytics_export(
        pool.clone(),
        moa_config.clickhouse.as_ref(),
        shutdown.clone(),
    );

    tracing::info!(
        port = args.port,
        health_port = args.health_port,
        scim_port = args.scim_port,
        restate_admin_url = %probe_state.admin_base_url(),
        metrics_url = metrics_endpoint_url(&moa_config.metrics).unwrap_or_else(|| "disabled".to_string()),
        "starting moa-orchestrator"
    );
    readiness.store(true, Ordering::Release);
    let _cron_bootstrap = {
        let probe_state = probe_state.clone();
        spawn_default_cron_bootstrap(
            move || {
                let probe_state = probe_state.clone();
                async move { probe_state.fetch_deployments().await }
            },
            restate_ingress_base_url(&restate_ingress_url),
        )
    };

    tokio::select! {
        result = &mut restate_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            health_server.abort();
            scim_server.abort();
            if let Some(handle) = channel_ingress.take() {
                handle.abort();
            }
            if let Some(handle) = analytics_export.take() {
                handle.abort();
            }
            result.context("join Restate handler server")?;
            bail!("Restate handler server exited unexpectedly");
        }
        result = &mut health_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            restate_server.abort();
            scim_server.abort();
            if let Some(handle) = channel_ingress.take() {
                handle.abort();
            }
            if let Some(handle) = analytics_export.take() {
                handle.abort();
            }
            result.context("join health probe server")??;
            bail!("health probe server exited unexpectedly");
        }
        result = &mut scim_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            restate_server.abort();
            health_server.abort();
            if let Some(handle) = channel_ingress.take() {
                handle.abort();
            }
            if let Some(handle) = analytics_export.take() {
                handle.abort();
            }
            result.context("join SCIM server")??;
            bail!("SCIM server exited unexpectedly");
        }
        signal = shutdown_signal() => {
            signal?;
            tracing::info!("shutdown signal received, draining");
            readiness.store(false, Ordering::Release);

            if let Some(poller_handle) = runtime_deps.authz_outbox_poller.take() {
                poller_handle.shutdown().await;
            }
            if let Some(reaper_handle) = authz_challenge_reaper_handle {
                reaper_handle.shutdown().await;
            }

            if let Some(writer) = runtime_deps.lineage.writer.clone() {
                tracing::info!("draining lineage writer");
                match writer.shutdown().await {
                    Ok(stats) => tracing::info!(
                        written = stats.written,
                        journal_depth = stats.journal_depth,
                        "lineage writer drained"
                    ),
                    Err(error) => tracing::warn!(?error, "lineage writer drain failed"),
                }
            }

            if probe_state.deregister_on_shutdown() {
                best_effort_deregister(&probe_state).await;
            }

            tokio::time::sleep(SHUTDOWN_DRAIN_DELAY).await;
            shutdown.cancel();

            restate_server
                .await
                .context("join Restate handler server during shutdown")?;
            health_server
                .await
                .context("join health probe server during shutdown")??;
            scim_server
                .await
                .context("join SCIM server during shutdown")??;
            if let Some(handle) = channel_ingress.take() {
                handle
                    .await
                    .context("join channel ingress during shutdown")?;
            }
            if let Some(handle) = analytics_export.take() {
                handle
                    .await
                    .context("join analytics export during shutdown")?;
            }
        }
    }

    Ok(())
}

#[derive(Clone)]
struct ProbeState {
    readiness: Arc<AtomicBool>,
    pool: sqlx::PgPool,
    admin_base_url: String,
    client: Client,
    require_registration: bool,
    deregister_on_shutdown: bool,
}

impl ProbeState {
    fn new(
        readiness: Arc<AtomicBool>,
        pool: sqlx::PgPool,
        admin_base_url: String,
    ) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(ADMIN_CHECK_TIMEOUT)
            .build()
            .context("build Restate admin HTTP client")?;

        Ok(Self {
            readiness,
            pool,
            admin_base_url: admin_base_url.trim_end_matches('/').to_string(),
            client,
            require_registration: env_flag("MOA_REQUIRE_RESTATE_REGISTRATION_FOR_READINESS", false),
            deregister_on_shutdown: env_flag("MOA_DEREGISTER_ON_SHUTDOWN", false),
        })
    }

    fn admin_base_url(&self) -> &str {
        &self.admin_base_url
    }

    fn deregister_on_shutdown(&self) -> bool {
        self.deregister_on_shutdown
    }

    async fn check_ready(&self) -> anyhow::Result<()> {
        if !self.readiness.load(Ordering::Acquire) {
            bail!("readiness disabled");
        }

        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&self.pool)
            .await
            .context("Postgres readiness check failed")?;

        let deployments = self.fetch_deployments().await?;
        if self.require_registration && !services_registered(&deployments) {
            bail!("expected Restate services are not registered yet");
        }

        Ok(())
    }

    async fn fetch_deployments(&self) -> anyhow::Result<Vec<RegisteredDeployment>> {
        let response = self
            .client
            .get(format!("{}/deployments", self.admin_base_url))
            .send()
            .await
            .context("reach Restate admin API")?
            .error_for_status()
            .context("Restate admin API returned an error")?;

        let payload = response
            .json::<DeploymentListResponse>()
            .await
            .context("decode Restate deployment list response")?;
        Ok(payload.deployments)
    }
}

async fn live_handler() -> impl IntoResponse {
    StatusCode::OK
}

async fn ready_handler(State(state): State<ProbeState>) -> impl IntoResponse {
    match state.check_ready().await {
        Ok(()) => (StatusCode::OK, "ready".to_string()),
        Err(error) => {
            tracing::debug!(error = %error, "readiness check failed");
            (StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
    }
}

async fn serve_health_server(
    listener: TcpListener,
    state: ProbeState,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let router = Router::new()
        .route("/_health/live", get(live_handler))
        .route("/_health/ready", get(ready_handler))
        .with_state(state);

    serve(listener, router)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("serve health probe HTTP server")
}

async fn serve_scim_server(
    listener: TcpListener,
    state: ScimState,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let router = Router::new()
        .nest("/scim/v2", scim::router(state))
        .route("/scim/v2", get(meta_scim_root));

    serve(listener, router)
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .context("serve SCIM HTTP server")
}

async fn meta_scim_root() -> impl IntoResponse {
    (
        StatusCode::OK,
        "MOA SCIM v2 endpoint. Use /scim/v2/ServiceProviderConfig.",
    )
}

fn spawn_restate_server(
    endpoint: Endpoint,
    listener: TcpListener,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        HttpServer::new(endpoint)
            .serve_with_cancel(listener, shutdown.cancelled_owned())
            .await;
    })
}

fn spawn_health_server(
    listener: TcpListener,
    state: ProbeState,
    shutdown: CancellationToken,
) -> JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move { serve_health_server(listener, state, shutdown).await })
}

fn spawn_scim_server(
    listener: TcpListener,
    state: ScimState,
    shutdown: CancellationToken,
) -> JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move { serve_scim_server(listener, state, shutdown).await })
}

async fn bind_listener(port: u16) -> anyhow::Result<TcpListener> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind TCP listener on {addr}"))
}

async fn shutdown_signal() -> anyhow::Result<()> {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .context("listen for Ctrl-C shutdown signal")
    };

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .context("install SIGTERM handler")?;
        tokio::select! {
            result = ctrl_c => result,
            _ = sigterm.recv() => Ok(()),
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await
    }
}

async fn best_effort_deregister(state: &ProbeState) {
    let Some(uri) = std::env::var("MOA_RESTATE_DEPLOYMENT_URI").ok() else {
        tracing::info!(
            "skipping Restate deregistration because MOA_RESTATE_DEPLOYMENT_URI is unset"
        );
        return;
    };

    let deployments = match state.fetch_deployments().await {
        Ok(deployments) => deployments,
        Err(error) => {
            tracing::warn!(error = %error, "failed to list Restate deployments during shutdown");
            return;
        }
    };

    let Some(deployment_id) = deployments
        .into_iter()
        .find(|deployment| deployment.uri.as_deref() == Some(uri.as_str()))
        .map(|deployment| deployment.id)
    else {
        tracing::info!(
            uri,
            "no Restate deployment matched MOA_RESTATE_DEPLOYMENT_URI"
        );
        return;
    };

    match state
        .client
        .delete(format!(
            "{}/deployments/{deployment_id}",
            state.admin_base_url
        ))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => {
            tracing::info!(deployment_id, "requested Restate deployment deregistration")
        }
        Ok(response) => tracing::warn!(
            deployment_id,
            status = %response.status(),
            "Restate deployment deregistration returned a non-success status"
        ),
        Err(error) => tracing::warn!(
            deployment_id,
            error = %error,
            "failed to deregister Restate deployment during shutdown"
        ),
    }
}

fn env_flag(key: &str, default: bool) -> bool {
    env_flag_from_reader(key, default, |name| std::env::var(name).ok())
}

fn env_flag_from_reader(
    key: &str,
    default: bool,
    mut read_var: impl FnMut(&str) -> Option<String>,
) -> bool {
    read_var(key)
        .and_then(|value: String| match value.to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::env_flag_from_reader;

    #[test]
    fn env_flag_understands_common_truthy_and_falsey_values() {
        assert!(env_flag_from_reader(
            "MOA_TEST_ENV_FLAG",
            false,
            |key| match key {
                "MOA_TEST_ENV_FLAG" => Some("true".to_string()),
                _ => None,
            }
        ));

        assert!(!env_flag_from_reader(
            "MOA_TEST_ENV_FLAG",
            true,
            |key| match key {
                "MOA_TEST_ENV_FLAG" => Some("off".to_string()),
                _ => None,
            }
        ));

        assert!(env_flag_from_reader("MOA_TEST_ENV_FLAG", true, |_| None));
    }
}
