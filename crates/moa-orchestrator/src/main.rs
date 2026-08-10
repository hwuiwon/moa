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
use moa_orchestrator::objects::session_status_migrator::build_status_migration_endpoint;
use moa_orchestrator::services::scim::{self, ScimState};
use moa_orchestrator::{
    config::{ProvidersOverride, load_moa_config_from_env, restate_ingress_url, skip_fga_from_env},
    credential_ingress,
    runtime::{
        bootstrap::{BootstrapOptions, run as run_bootstrap, wait_for_session_status_cutover},
        channel_ingress::spawn_channel_ingress,
        database::{build_database_pool, database_search_path},
        deps::RuntimeDeps,
        endpoint::build_endpoint,
        jobs::{
            start_action_review_reaper, start_authz_challenge_reaper_if_configured,
            start_checkpoint_bucket_versioning_refresh, start_hand_lease_reaper,
            start_mcp_catalog_refresh, start_workspace_reaper,
        },
        kms::KmsRuntime,
        sandbox_workspace_rollout::validate_startup_state as validate_sandbox_workspace_rollout,
    },
};
use restate_sdk::prelude::*;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DEFAULT_RESTATE_PORT: u16 = 10020;
const DEFAULT_HEALTH_PORT: u16 = 10021;
const DEFAULT_SCIM_PORT: u16 = 10022;
const DEFAULT_CONNECTOR_CREDENTIAL_PORT: u16 = 10023;
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
    /// Private HTTP port for edge-forwarded connector credential writes.
    #[arg(long, default_value_t = DEFAULT_CONNECTOR_CREDENTIAL_PORT)]
    credential_port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
enum Command {
    /// Apply database migrations and exit.
    Migrate,
    /// Serve only the raw Session state cutover handlers.
    ServeStatusMigration,
    /// Block a normal runtime replica until the raw-state cutover is complete.
    WaitStatusCutover {
        /// Runtime Postgres URL used only to read and verify the cutover receipt.
        #[arg(long)]
        database_url: String,
    },
    /// Reconcile Restate control-plane state from a dedicated least-privilege process.
    Bootstrap {
        /// Restate Admin API URL used to observe Operator registration.
        #[arg(long)]
        admin_url: String,
        /// Restate ingress URL used for public bootstrap handlers.
        #[arg(long)]
        ingress_url: String,
        /// Privileged Postgres URL used for the one-way Session state cutover.
        #[arg(long)]
        database_url: String,
        /// Migration-only handler URI registered for the raw-state stage.
        #[arg(long)]
        migration_deployment_uri: String,
        /// Steady-state handler URI to register after cutover without an Operator.
        #[arg(long)]
        runtime_deployment_uri: Option<String>,
        /// Sandbox-workspace rollout mode whose service binding must register.
        #[arg(long, default_value = "disabled", value_parser = parse_sandbox_workspace_mode)]
        sandbox_workspace_mode: moa_config::SandboxWorkspaceMode,
    },
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

fn parse_sandbox_workspace_mode(value: &str) -> Result<moa_config::SandboxWorkspaceMode, String> {
    match value {
        "disabled" => Ok(moa_config::SandboxWorkspaceMode::Disabled),
        "maintenance" => Ok(moa_config::SandboxWorkspaceMode::Maintenance),
        "admit" => Ok(moa_config::SandboxWorkspaceMode::Admit),
        _ => Err("expected disabled, maintenance, or admit".to_string()),
    }
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
    if matches!(args.command.as_ref(), Some(Command::ServeStatusMigration)) {
        let listener = bind_listener(args.port).await?;
        let shutdown = CancellationToken::new();
        let server = spawn_restate_server(
            build_status_migration_endpoint(),
            listener,
            shutdown.clone(),
        );
        tracing::info!(port = args.port, "starting migration-only Restate endpoint");
        shutdown_signal().await?;
        shutdown.cancel();
        let _ = server.await;
        return Ok(());
    }
    if let Some(Command::WaitStatusCutover { database_url }) = args.command.as_ref() {
        wait_for_session_status_cutover(database_url).await?;
        return Ok(());
    }
    if let Some(Command::Bootstrap {
        admin_url,
        ingress_url,
        database_url,
        migration_deployment_uri,
        runtime_deployment_uri,
        sandbox_workspace_mode,
    }) = args.command.as_ref()
    {
        let report = run_bootstrap(BootstrapOptions {
            admin_url: admin_url.clone(),
            ingress_url: ingress_url.clone(),
            database_url: database_url.clone(),
            migration_deployment_uri: migration_deployment_uri.clone(),
            runtime_deployment_uri: runtime_deployment_uri.clone(),
            sandbox_workspace_mode: *sandbox_workspace_mode,
        })
        .await?;
        tracing::info!(
            sessions = report.sessions_migrated,
            status_keys_rewritten = report.status_keys_rewritten,
            meta_statuses_rewritten = report.meta_statuses_rewritten,
            "Restate bootstrap complete"
        );
        return Ok(());
    }
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
        Some(Command::Bootstrap { .. }) => unreachable!("bootstrap returned before runtime config"),
        Some(Command::ServeStatusMigration) | Some(Command::WaitStatusCutover { .. }) => {
            unreachable!("pre-runtime command returned before runtime config")
        }
        None => {}
    }

    moa_config
        .validate_sandbox_workspace_runtime(skip_fga)
        .context("validate sandbox workspace runtime rollout")?;

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
    validate_sandbox_workspace_rollout(moa_config.as_ref(), &pool).await?;
    let background_pool = build_database_pool(
        moa_config.database.runtime_url(),
        &database_search_path,
        moa_config.database.background_max_connections,
        Duration::from_secs(moa_config.database.connect_timeout_seconds),
    )
    .await
    .context("connect background database pool")?;
    let maintenance_pool = if moa_config.sandbox_workspaces.mode.maintenance_enabled() {
        let url = moa_config.database.maintenance_url().ok_or_else(|| {
            anyhow::anyhow!("sandbox workspace maintenance database URL is unavailable")
        })?;
        Some(
            build_database_pool(
                url,
                &database_search_path,
                moa_config.database.background_max_connections,
                Duration::from_secs(moa_config.database.connect_timeout_seconds),
            )
            .await
            .context("connect dedicated sandbox workspace maintenance database pool")?,
        )
    } else {
        None
    };
    let mut runtime_deps = RuntimeDeps::build(
        moa_config.clone(),
        pool.clone(),
        background_pool,
        maintenance_pool,
        &restate_ingress_url,
        providers_override,
        skip_fga,
    )
    .await?;
    let scim_base_url = std::env::var("MOA_SCIM_BASE_URL")
        .unwrap_or_else(|_| format!("http://localhost:{}/scim/v2", args.scim_port));
    let scim_state = runtime_deps.scim_state(scim_base_url);

    let endpoint = build_endpoint(&runtime_deps);

    let mut checkpoint_versioning_refresh_handle = runtime_deps
        .checkpoint_versioning_observer
        .clone()
        .map(start_checkpoint_bucket_versioning_refresh);

    let mut workspace_reaper_handle = runtime_deps
        .workspace_maintenance
        .clone()
        .map(|coordinator| start_workspace_reaper(coordinator, moa_config.as_ref()))
        .transpose()?;
    let workspace_reaper_readiness = workspace_reaper_handle
        .as_ref()
        .map(moa_hands::core::sandbox_workspace::reaper::WorkspaceReaperHandle::readiness);

    let readiness = Arc::new(AtomicBool::new(false));
    let probe_state = ProbeState::new(
        readiness.clone(),
        pool.clone(),
        runtime_deps.kms.clone(),
        runtime_deps.lineage.writer.clone(),
        runtime_deps.checkpoint_versioning_observer.clone(),
        workspace_reaper_readiness,
    );
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
    let hand_lease_reaper_handle = moa_config
        .sandbox_workspaces
        .mode
        .maintenance_enabled()
        .then(|| {
            start_hand_lease_reaper(
                &runtime_deps.background_pool,
                runtime_deps.tool_router.hand_providers(),
            )
        })
        .transpose()?;
    // Optional connectors that failed discovery at startup are retried here, and
    // schema changes republished, without restarting the process.
    let mcp_catalog_refresh_handle =
        start_mcp_catalog_refresh(moa_config.as_ref(), runtime_deps.tool_router.clone());

    let restate_listener = bind_listener(args.port).await?;
    let health_listener = bind_listener(args.health_port).await?;
    let scim_listener = bind_listener(args.scim_port).await?;
    let credential_listener = bind_listener(args.credential_port).await?;
    let mut restate_server = spawn_restate_server(endpoint, restate_listener, shutdown.clone());
    let mut health_server =
        spawn_health_server(health_listener, probe_state.clone(), shutdown.clone());
    let mut scim_server = spawn_scim_server(scim_listener, scim_state, shutdown.clone());
    let mut credential_server = spawn_credential_server(
        credential_listener,
        credential_ingress::router(runtime_deps.connector_credential_ingress()),
        shutdown.clone(),
    );
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
        credential_port = args.credential_port,
        metrics_url =
            metrics_endpoint_url(&moa_config.metrics).unwrap_or_else(|| "disabled".to_string()),
        "starting moa-orchestrator"
    );
    readiness.store(true, Ordering::Release);

    tokio::select! {
        result = &mut restate_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            health_server.abort();
            scim_server.abort();
            credential_server.abort();
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
            credential_server.abort();
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
            credential_server.abort();
            if let Some(handle) = channel_ingress.take() {
                handle.abort();
            }
            if let Some(handle) = analytics_export.take() {
                handle.abort();
            }
            result.context("join SCIM server")??;
            bail!("SCIM server exited unexpectedly");
        }
        result = &mut credential_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            restate_server.abort();
            health_server.abort();
            scim_server.abort();
            if let Some(handle) = channel_ingress.take() {
                handle.abort();
            }
            if let Some(handle) = analytics_export.take() {
                handle.abort();
            }
            result.context("join connector credential ingress server")??;
            bail!("connector credential ingress server exited unexpectedly");
        }
        result = await_workspace_reaper_exit(&mut workspace_reaper_handle) => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            restate_server.abort();
            health_server.abort();
            scim_server.abort();
            credential_server.abort();
            if let Some(handle) = channel_ingress.take() {
                handle.abort();
            }
            if let Some(handle) = analytics_export.take() {
                handle.abort();
            }
            result.context("durable workspace reaper exited")?;
            bail!("durable workspace reaper exited unexpectedly");
        }
        result = await_checkpoint_versioning_refresh_exit(&mut checkpoint_versioning_refresh_handle) => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            restate_server.abort();
            health_server.abort();
            scim_server.abort();
            credential_server.abort();
            if let Some(handle) = channel_ingress.take() {
                handle.abort();
            }
            if let Some(handle) = analytics_export.take() {
                handle.abort();
            }
            result.context("checkpoint bucket versioning refresh exited")?;
            bail!("checkpoint bucket versioning refresh exited unexpectedly");
        }
        signal = shutdown_signal() => {
            signal?;
            tracing::info!("shutdown signal received, draining");
            readiness.store(false, Ordering::Release);

            // Give the load balancer a bounded window to observe readiness=false
            // before closing ingress. Audit admission stays open throughout this
            // interval because in-flight requests may still emit records.
            tokio::time::sleep(SHUTDOWN_DRAIN_DELAY).await;
            shutdown.cancel();

            // Restate, SCIM, connector credential ingress, and channel ingress
            // are request-owned audit producers. Join them before closing audit
            // admission so every accepted request finishes its final emission.
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
            if let Some(result) =
                join_task_bounded("connector credential ingress server", credential_server).await
                && let Err(error) = result
            {
                tracing::warn!(%error, "connector credential ingress server failed during shutdown");
            }
            if let Some(handle) = channel_ingress.take() {
                let _ = join_task_bounded("channel ingress", handle).await;
            }
            if let Some(handle) = analytics_export.take() {
                let _ = join_task_bounded("analytics export", handle).await;
            }

            if let Some(handle) = workspace_reaper_handle.take() {
                let _ = shutdown_future_bounded(
                    "durable workspace reaper",
                    handle.shutdown(),
                )
                .await;
            }

            if let Some(handle) = checkpoint_versioning_refresh_handle.take() {
                let _ = shutdown_future_bounded(
                    "checkpoint bucket versioning refresh",
                    handle.shutdown(),
                )
                .await;
            }

            if let Some(handle) = hand_lease_reaper_handle {
                abort_and_join_task("hand lease reaper", handle).await;
            }
            if let Some(handle) = mcp_catalog_refresh_handle {
                abort_and_join_task("MCP catalog refresh", handle).await;
            }
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

    // No task that records logs, metrics, or spans remains. Flush all signals only
    // after their producers have stopped so final drain counters are included.
    telemetry.shutdown();
    Ok(())
}

async fn await_workspace_reaper_exit(
    handle: &mut Option<moa_hands::core::sandbox_workspace::reaper::WorkspaceReaperHandle>,
) -> moa_core::error::Result<()> {
    match handle {
        Some(handle) => handle.task_result().await,
        None => std::future::pending().await,
    }
}

async fn await_checkpoint_versioning_refresh_exit(
    handle: &mut Option<moa_orchestrator::runtime::jobs::CheckpointBucketVersioningRefreshHandle>,
) -> anyhow::Result<()> {
    match handle {
        Some(handle) => handle.task_result().await,
        None => std::future::pending().await,
    }
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
    /// Durable lineage writer, when this deployment owns one.
    ///
    /// Held so readiness can refuse traffic for a writer that is dead, draining,
    /// cut off from Postgres, or sitting on a backlog older than its limit. All
    /// four are readiness conditions and none is a liveness condition: the
    /// accepted rows are safe in Postgres, and restarting the process would only
    /// drop leases and slow the drain that is already behind.
    lineage_writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
    /// Authenticated checkpoint-bucket observation shared with the deletion gate.
    checkpoint_versioning: Option<
        moa_hands::core::sandbox_workspace::checkpoint::versioning::CheckpointBucketVersioningObserver,
    >,
    /// Supervised workspace-maintenance readiness, when maintenance is enabled.
    workspace_reaper:
        Option<moa_hands::core::sandbox_workspace::reaper::WorkspaceReaperReadiness>,
}

impl ProbeState {
    fn new(
        readiness: Arc<AtomicBool>,
        pool: sqlx::PgPool,
        kms: KmsRuntime,
        lineage_writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
        checkpoint_versioning: Option<
            moa_hands::core::sandbox_workspace::checkpoint::versioning::CheckpointBucketVersioningObserver,
        >,
        workspace_reaper: Option<
            moa_hands::core::sandbox_workspace::reaper::WorkspaceReaperReadiness,
        >,
    ) -> Self {
        Self {
            readiness,
            pool,
            kms,
            lineage_writer,
            checkpoint_versioning,
            workspace_reaper,
        }
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

        if let Some(observer) = &self.checkpoint_versioning
            && !observer.is_ready()
        {
            bail!("checkpoint bucket versioning observation is missing or stale");
        }

        if let Some(reaper) = &self.workspace_reaper
            && let Some(reason) = reaper.unready_reason()
        {
            bail!("durable workspace maintenance not ready: {reason}");
        }

        Ok(())
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

fn spawn_credential_server(
    listener: TcpListener,
    router: Router,
    shutdown: CancellationToken,
) -> JoinHandle<anyhow::Result<()>> {
    tokio::spawn(async move {
        serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .context("serve connector credential ingress HTTP server")
    })
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
