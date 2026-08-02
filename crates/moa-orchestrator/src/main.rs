//! Restate-backed `moa-orchestrator` binary entrypoint.

use std::future::Future;
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
use moa_orchestrator::services::scim::{self, ScimState};
use moa_orchestrator::{
    config::{
        ProvidersOverride, load_moa_config_from_env, restate_admin_url, restate_ingress_url,
        skip_fga_from_env,
    },
    runtime::{
        channel_ingress::spawn_channel_ingress,
        database::{build_database_pool, database_search_path},
        deps::RuntimeDeps,
        endpoint::{
            DeploymentListResponse, RegisteredDeployment, build_endpoint, services_registered,
        },
        jobs::{
            restate_ingress_base_url, spawn_default_cron_bootstrap, start_action_review_reaper,
            start_authz_challenge_reaper_if_configured, start_hand_lease_reaper,
            start_mcp_catalog_refresh,
        },
        kms::KmsRuntime,
    },
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
const SHUTDOWN_TASK_TIMEOUT: Duration = Duration::from_secs(15);
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

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum Command {
    /// Apply database migrations and exit.
    Migrate,
    /// Activate the required root-key generation and rewrap live KEKs in batches.
    KmsRewrap {
        /// Maximum KEKs claimed and committed per transaction.
        #[arg(long, default_value_t = 100)]
        batch_size: u32,
        /// Explicit old generation to retire only after every rewrap batch drains.
        #[arg(long)]
        retire_generation: Option<String>,
    },
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
    let telemetry =
        init_observability(moa_config.as_ref(), &TelemetryConfig { json_stdout: true })?;
    let database_search_path = database_search_path(moa_config.as_ref());
    match args.command.as_ref() {
        Some(Command::Migrate) => {
            moa_migrations::run(moa_config.database.admin_url())
                .await
                .context("apply database migrations")?;
            return Ok(());
        }
        Some(Command::KmsRewrap {
            batch_size,
            retire_generation,
        }) => {
            let pool = build_database_pool(
                moa_config.database.admin_url(),
                &database_search_path,
                moa_config.database.max_connections.clamp(1, 5),
                Duration::from_secs(moa_config.database.connect_timeout_seconds),
            )
            .await
            .context("connect KMS rewrap database pool")?;
            let kms = KmsRuntime::build_maintenance(moa_config.as_ref(), pool)
                .await
                .context("build KMS maintenance provider")?;
            let report = kms
                .rewrap_to_required(*batch_size, retire_generation.as_deref())
                .await?;
            tracing::info!(
                active_generation = %report.active_generation,
                rewrapped = report.rewrapped,
                batches = report.batches,
                retired_generation = ?report.retired_generation,
                "KMS rewrap complete"
            );
            return Ok(());
        }
        None => {}
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
    moa_migrations::validate_complete_history(&pool)
        .await
        .context("validate complete database migration history")?;
    let background_pool = build_database_pool(
        moa_config.database.runtime_url(),
        &database_search_path,
        moa_config.database.background_max_connections,
        Duration::from_secs(moa_config.database.connect_timeout_seconds),
    )
    .await
    .context("connect background database pool")?;
    let mut runtime_deps = RuntimeDeps::build(
        moa_config.clone(),
        pool.clone(),
        background_pool,
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
        runtime_deps.background_pool.clone(),
        runtime_deps.kms.provider(),
        runtime_deps.fga_client.clone(),
        runtime_deps.providers.clone(),
        runtime_deps.tool_router.clone(),
        moa_config.session_limits.clone(),
        moa_config.clone(),
        runtime_deps.contact_token_issuer.clone(),
        runtime_deps.credential_vault.clone(),
        runtime_deps.lineage.handle.clone(),
        runtime_deps.embedding_provider.clone(),
        Arc::new(runtime_deps.channel_adapters.clone()),
        runtime_deps.runtime_cache.clone(),
        runtime_deps.lineage.score_handle(),
    );

    let readiness = Arc::new(AtomicBool::new(false));
    let probe_state = ProbeState::new(
        readiness.clone(),
        pool.clone(),
        runtime_deps.kms.clone(),
        restate_admin_url,
        runtime_deps.lineage.writer.clone(),
    )?;
    let shutdown = CancellationToken::new();
    let authz_challenge_reaper_handle = start_authz_challenge_reaper_if_configured(
        &runtime_deps.background_pool,
        moa_config.as_ref(),
        runtime_deps.awakeable_resolver.clone(),
    )?;
    let action_review_reaper_handle =
        start_action_review_reaper(&runtime_deps.background_pool, restate_ingress_url.clone());
    // The destruction owner for every bounded sandbox deadline. It is started
    // before the servers accept traffic, and startup fails outright if no hand
    // provider is registered, so no sandbox is ever provisioned under a policy
    // this process cannot enforce.
    let hand_lease_reaper_handle = start_hand_lease_reaper(
        &runtime_deps.background_pool,
        runtime_deps.tool_router.hand_providers(),
    )?;
    // Optional connectors that failed discovery at startup are retried here, and
    // schema changes republished, without restarting the process.
    let mcp_catalog_refresh_handle =
        start_mcp_catalog_refresh(moa_config.as_ref(), runtime_deps.tool_router.clone());

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
    let mut analytics_export = moa_analytics_export::spawn_analytics_export(
        runtime_deps.background_pool.clone(),
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
    let cron_bootstrap = {
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

            if probe_state.deregister_on_shutdown() {
                best_effort_deregister(&probe_state).await;
            }

            // Give the load balancer a bounded window to observe readiness=false
            // before closing ingress. Audit admission stays open throughout this
            // interval because in-flight requests may still emit records.
            tokio::time::sleep(SHUTDOWN_DRAIN_DELAY).await;
            shutdown.cancel();

            // Restate, SCIM, and channel ingress are the request-owned audit
            // producers. Join them before closing audit admission so every
            // accepted request has finished its final audit emission.
            let _ = join_task_bounded("Restate handler server", restate_server).await;
            if let Some(result) = join_task_bounded("health probe server", health_server).await
                && let Err(error) = result
            {
                tracing::warn!(%error, "health probe server failed during shutdown");
            }
            if let Some(result) = join_task_bounded("SCIM server", scim_server).await
                && let Err(error) = result
            {
                tracing::warn!(%error, "SCIM server failed during shutdown");
            }
            if let Some(handle) = channel_ingress.take() {
                let _ = join_task_bounded("channel ingress", handle).await;
            }
            if let Some(handle) = analytics_export.take() {
                let _ = join_task_bounded("analytics export", handle).await;
            }

            abort_and_join_task("hand lease reaper", hand_lease_reaper_handle).await;
            if let Some(handle) = mcp_catalog_refresh_handle {
                abort_and_join_task("MCP catalog refresh", handle).await;
            }
            abort_and_join_task("cron bootstrap", cron_bootstrap).await;

            if let Some(poller_handle) = runtime_deps.authz_outbox_poller.take() {
                let _ = shutdown_future_bounded(
                    "authz outbox poller",
                    poller_handle.shutdown(),
                )
                .await;
            }
            if let Some(reaper_handle) = authz_challenge_reaper_handle {
                let _ = shutdown_future_bounded(
                    "authz challenge reaper",
                    reaper_handle.shutdown(),
                )
                .await;
            }
            let _ = shutdown_future_bounded(
                "action review reaper",
                action_review_reaper_handle.shutdown(),
            )
            .await;

            let audit = runtime_deps.audit.clone();

            if let Some(writer) = runtime_deps.lineage.writer.clone() {
                tracing::info!("draining lineage writer");
                if let Some(result) =
                    shutdown_future_bounded("lineage writer", writer.shutdown()).await
                {
                    match result {
                        Ok(stats) => tracing::info!(
                            written = stats.written,
                            pending = stats.pending,
                            "lineage writer drained; any pending rows stay committed for \
                             another replica"
                        ),
                        Err(error) => tracing::warn!(?error, "lineage writer drain failed"),
                    }
                }
            }

            // Audit is last among background writers. Its shutdown closes
            // admission before draining, so any producer accidentally left
            // alive is counted as queue_closed rather than silently accepted.
            let dropped = shutdown_future_bounded("security audit writer", audit.shutdown())
                .await
                .unwrap_or_else(|| audit.emitter().dropped_count());
            if dropped > 0 {
                tracing::warn!(
                    dropped,
                    "security audit events were dropped during this process lifetime; the \
                     audit trail is incomplete"
                );
            }

        }
    }

    // No task that records metrics or spans remains. Flush both signals only
    // after their producers have stopped so final drain counters are included.
    telemetry.shutdown();
    Ok(())
}

async fn join_task_bounded<T>(name: &'static str, mut handle: JoinHandle<T>) -> Option<T> {
    match tokio::time::timeout(SHUTDOWN_TASK_TIMEOUT, &mut handle).await {
        Ok(Ok(output)) => Some(output),
        Ok(Err(error)) => {
            tracing::warn!(task = name, %error, "background task ended abnormally during shutdown");
            None
        }
        Err(_) => {
            tracing::warn!(
                task = name,
                timeout_secs = SHUTDOWN_TASK_TIMEOUT.as_secs(),
                "background task exceeded shutdown deadline; aborting"
            );
            handle.abort();
            let _ = handle.await;
            None
        }
    }
}

async fn abort_and_join_task(name: &'static str, handle: JoinHandle<()>) {
    handle.abort();
    if let Err(error) = handle.await
        && !error.is_cancelled()
    {
        tracing::warn!(task = name, %error, "background task ended abnormally during shutdown");
    }
}

async fn shutdown_future_bounded<T>(
    name: &'static str,
    shutdown: impl Future<Output = T>,
) -> Option<T> {
    match tokio::time::timeout(SHUTDOWN_TASK_TIMEOUT, shutdown).await {
        Ok(output) => Some(output),
        Err(_) => {
            tracing::warn!(
                task = name,
                timeout_secs = SHUTDOWN_TASK_TIMEOUT.as_secs(),
                "background shutdown exceeded deadline"
            );
            None
        }
    }
}

#[derive(Clone)]
struct ProbeState {
    readiness: Arc<AtomicBool>,
    pool: sqlx::PgPool,
    kms: KmsRuntime,
    admin_base_url: String,
    client: Client,
    require_registration: bool,
    deregister_on_shutdown: bool,
    /// Durable lineage writer, when this deployment owns one.
    ///
    /// Held so readiness can refuse traffic for a writer that is dead, draining,
    /// cut off from Postgres, or sitting on a backlog older than its limit. All
    /// four are readiness conditions and none is a liveness condition: the
    /// accepted rows are safe in Postgres, and restarting the process would only
    /// drop leases and slow the drain that is already behind.
    lineage_writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
}

impl ProbeState {
    fn new(
        readiness: Arc<AtomicBool>,
        pool: sqlx::PgPool,
        kms: KmsRuntime,
        admin_base_url: String,
        lineage_writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
    ) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(ADMIN_CHECK_TIMEOUT)
            .build()
            .context("build Restate admin HTTP client")?;

        Ok(Self {
            readiness,
            pool,
            kms,
            admin_base_url: admin_base_url.trim_end_matches('/').to_string(),
            client,
            require_registration: env_flag("MOA_REQUIRE_RESTATE_REGISTRATION_FOR_READINESS", false),
            deregister_on_shutdown: env_flag("MOA_DEREGISTER_ON_SHUTDOWN", false),
            lineage_writer,
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

        self.kms.check_readiness().await?;

        if let Some(writer) = &self.lineage_writer
            && let Some(reason) = writer.unready_reason()
        {
            bail!("lineage writer not ready: {reason}");
        }

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
