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
use moa_authz::AwakeableResolver;
use moa_brain::build_default_graph_memory_retriever;
use moa_core::config::{AsyncAuthzKind, AuthHeaderTrustKind};
use moa_core::{MoaConfig, TelemetryConfig, init_observability, metrics_endpoint_url};
use moa_hands::ToolRouter;
use moa_memory_ingest::{IngestionVO, IngestionVOImpl};
#[cfg(feature = "internal-eval-runner")]
use moa_orchestrator::services::eval::{Eval, EvalImpl};
#[cfg(feature = "internal-eval-runner")]
use moa_orchestrator::workflows::eval_run::{EvalRun, EvalRunImpl};
use moa_orchestrator::{
    OrchestratorCtx,
    config::{
        ProvidersOverride, load_moa_config_from_env, restate_admin_url, restate_ingress_url,
        skip_fga_from_env,
    },
    ctx::{self, HeaderTrustMode},
    lineage::build_lineage_sink,
    objects::cron_job::{CronJob, CronJobImpl},
    objects::session::{Session, SessionImpl},
    objects::sub_agent::{SubAgent, SubAgentImpl},
    objects::workspace::{WorkspaceImpl, WorkspaceObject},
    services::{
        admin_maintenance::{AdminMaintenance, AdminMaintenanceImpl},
        agents::{Agents, AgentsImpl},
        analytics::{Analytics, AnalyticsImpl},
        api_keys::{ApiKeys, ApiKeysImpl},
        approvals::{Approvals, ApprovalsImpl},
        approvals_reaper::{ApprovalReaper, ApprovalReaperHandle, HttpAwakeableResolver},
        artifacts::{Artifacts, ArtifactsImpl},
        audit::{Audit, AuditImpl},
        authz_admin::{Authz, AuthzImpl},
        experiments::{Experiments, ExperimentsImpl},
        graph_memory_maint::{GraphMemoryMaint, GraphMemoryMaintImpl},
        health::{Health, HealthImpl},
        lineage_admin::{LineageAdmin, LineageAdminImpl},
        llm_gateway::{LLMGateway, LLMGatewayImpl, ProviderRegistry},
        memory::{Memory, MemoryImpl},
        neon_maint::{NeonMaint, NeonMaintImpl},
        privacy::{Privacy, PrivacyImpl},
        scim::{self, ScimState},
        session_store::{RestateSessionStore, SessionStoreImpl},
        skills::{Skills, SkillsImpl},
        tenants::{Tenants, TenantsImpl},
        tool_executor::{ToolExecutor, ToolExecutorImpl},
        whoami::{Whoami, WhoamiImpl},
        workflows::{Workflows, WorkflowsImpl},
        workspace_store::{WorkspaceStore, WorkspaceStoreImpl},
    },
    workflows::{
        consolidate::{Consolidate, ConsolidateImpl},
        experiment_run::{ExperimentRun, ExperimentRunImpl},
        experiment_trial_run::{ExperimentTrialRun, ExperimentTrialRunImpl},
        sub_agent_turn_execution::{SubAgentTurnExecution, SubAgentTurnExecutionImpl},
        turn_execution::{TurnExecution, TurnExecutionImpl},
    },
};
use moa_providers::build_embedding_provider_from_config;
use moa_session::PostgresSessionStore;
use reqwest::Client;
use restate_sdk::prelude::*;
use serde::Deserialize;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DEFAULT_RESTATE_PORT: u16 = 10020;
const DEFAULT_RESTATE_INGRESS_PORT: u16 = 8080;
const DEFAULT_HEALTH_PORT: u16 = 10021;
const DEFAULT_SCIM_PORT: u16 = 10022;
const ADMIN_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const CRON_BOOTSTRAP_ATTEMPTS: u32 = 60;
const CRON_BOOTSTRAP_INTERVAL: Duration = Duration::from_secs(2);
const SHUTDOWN_DRAIN_DELAY: Duration = Duration::from_secs(5);
const DEFAULT_EXPECTED_SERVICE_NAMES: &[&str] = &[
    "Agents",
    "AdminMaintenance",
    "Analytics",
    "Artifacts",
    "Approvals",
    "ApiKeys",
    "Audit",
    "Authz",
    "Consolidate",
    "CronJob",
    "Experiments",
    "ExperimentRun",
    "ExperimentTrialRun",
    "GraphMemoryMaint",
    "Health",
    "IngestionVO",
    "LineageAdmin",
    "LLMGateway",
    "Memory",
    "NeonMaint",
    "Privacy",
    "Session",
    "SessionStore",
    "Skills",
    "SubAgent",
    "SubAgentTurnExecution",
    "Tenants",
    "ToolExecutor",
    "TurnExecution",
    "Workspace",
    "WorkspaceStore",
    "Whoami",
    "Workflows",
];
const INTERNAL_EVAL_SERVICE_NAMES: &[&str] = &["Eval", "EvalRun"];

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let moa_config = load_moa_config_from_env()?;
    let header_trust_mode = header_trust_mode_from_config(&moa_config);
    let skip_fga = skip_fga_from_env();
    let moa_config = Arc::new(moa_config);
    let _ = ctx::HEADER_TRUST_MODE.set(header_trust_mode);
    let _telemetry = init_observability(
        moa_config.as_ref(),
        &TelemetryConfig {
            json_stdout: true,
            ..TelemetryConfig::default()
        },
    )?;
    let database_search_path = moa_config
        .database
        .schema
        .as_deref()
        .map(|schema_name| format!("{}, public", quote_identifier(schema_name)))
        .unwrap_or_else(|| "public".to_string());
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
    moa_authz::configure_security_audit(pool.clone(), moa_config.audit_security.emit_authz_allows);
    let fga_client = if skip_fga {
        tracing::warn!("MOA_SKIP_FGA set; authz outbox poller disabled");
        None
    } else {
        Some(build_fga_client(moa_config.as_ref())?)
    };
    let poller_handle = fga_client
        .clone()
        .map(|fga_client| start_authz_outbox_poller(&pool, fga_client));
    let session_store = Arc::new(
        PostgresSessionStore::from_existing_pool(&moa_config.database.url, pool.clone()).await?,
    );
    let awakeable_resolver: Arc<dyn AwakeableResolver> = Arc::new(HttpAwakeableResolver::new(
        cron_bootstrap_ingress_url(&restate_ingress_url),
    )?);
    let auth_providers = moa_auth_providers::build_providers_with_resolver(
        moa_config.as_ref(),
        Arc::new(pool.clone()),
        Some(awakeable_resolver.clone()),
    )
    .context("build providers bundle")?;

    let llm_providers = Arc::new(match providers_override {
        ProvidersOverride::None => ProviderRegistry::from_config(moa_config.as_ref()),
        ProvidersOverride::Scripted { path } => {
            tracing::warn!(
                path = %path.display(),
                "loading scripted provider override (test mode)"
            );
            ProviderRegistry::scripted(path)?
        }
        ProvidersOverride::Mock { seed } => {
            tracing::warn!(seed, "using mock provider override (test mode)");
            ProviderRegistry::mock(seed)?
        }
    });
    let embedding_provider = build_embedding_provider_from_config(moa_config.as_ref())?;
    let tool_router = Arc::new(
        ToolRouter::from_config(moa_config.as_ref())
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let lineage = build_lineage_sink(moa_config.as_ref(), pool.clone()).await?;
    let graph_memory_retriever = build_default_graph_memory_retriever(
        moa_config.as_ref(),
        session_store.pool().clone(),
        lineage.handle.clone(),
    );
    let ctx = Arc::new(OrchestratorCtx {
        config: moa_config.clone(),
        session_store: session_store.clone(),
        graph_pool: session_store.pool().clone(),
        fga_client,
        auth_providers: auth_providers.clone(),
        providers: llm_providers.clone(),
        embedding_provider: embedding_provider.clone(),
        tool_router: tool_router.clone(),
        tool_schemas: Arc::new(tool_router.tool_schemas()),
        graph_memory_retriever,
        lineage: lineage.handle.clone(),
        lineage_writer: lineage.writer.clone(),
    });
    OrchestratorCtx::install(ctx).expect("install orchestrator ctx");
    let _ = moa_memory_ingest::install_runtime_with_config(pool.clone(), moa_config.as_ref());
    let scim_base_url = std::env::var("MOA_SCIM_BASE_URL")
        .unwrap_or_else(|_| format!("http://localhost:{}/scim/v2", args.scim_port));
    let scim_state = ScimState::new(
        pool.clone(),
        Arc::new(moa_auth_providers::LocalAuthProvider::new(Arc::new(
            pool.clone(),
        ))),
        OrchestratorCtx::current().fga_client.clone(),
        scim_base_url,
    );

    let endpoint = Endpoint::builder()
        .bind(HealthImpl.serve())
        .bind(SessionStoreImpl::new(session_store.clone()).serve())
        .bind(LLMGatewayImpl::new(llm_providers).serve())
        .bind(AgentsImpl.serve())
        .bind(AdminMaintenanceImpl.serve())
        .bind(AnalyticsImpl.serve())
        .bind(ArtifactsImpl.serve())
        .bind(ApprovalsImpl.serve())
        .bind(ApiKeysImpl.serve())
        .bind(AuditImpl.serve())
        .bind(AuthzImpl.serve());
    #[cfg(feature = "internal-eval-runner")]
    let endpoint = endpoint.bind(EvalImpl.serve());
    let endpoint = endpoint
        .bind(ExperimentsImpl.serve())
        .bind(IngestionVOImpl.serve())
        .bind(ToolExecutorImpl::new(tool_router.clone()).serve())
        .bind(WorkspaceStoreImpl::new(tool_router.clone()).serve())
        .bind(GraphMemoryMaintImpl.serve())
        .bind(LineageAdminImpl.serve())
        .bind(MemoryImpl.serve())
        .bind(NeonMaintImpl.serve())
        .bind(PrivacyImpl.serve())
        .bind(SkillsImpl.serve())
        .bind(CronJobImpl.serve())
        .bind(SessionImpl.serve())
        .bind(SubAgentImpl.serve())
        .bind(TenantsImpl.serve())
        .bind(WorkspaceImpl.serve())
        .bind(WhoamiImpl.serve())
        .bind(WorkflowsImpl.serve())
        .bind(ConsolidateImpl.serve());
    #[cfg(feature = "internal-eval-runner")]
    let endpoint = endpoint.bind(EvalRunImpl.serve());
    let endpoint = endpoint
        .bind(ExperimentRunImpl.serve())
        .bind(ExperimentTrialRunImpl.serve())
        .bind(SubAgentTurnExecutionImpl.serve())
        .bind(TurnExecutionImpl.serve())
        .build();

    let readiness = Arc::new(AtomicBool::new(false));
    let probe_state = ProbeState::new(readiness.clone(), pool.clone(), restate_admin_url)?;
    let shutdown = CancellationToken::new();
    let approval_reaper_handle =
        start_approval_reaper_if_configured(&pool, moa_config.as_ref(), awakeable_resolver)?;

    let restate_listener = bind_listener(args.port).await?;
    let health_listener = bind_listener(args.health_port).await?;
    let scim_listener = bind_listener(args.scim_port).await?;
    let mut restate_server = spawn_restate_server(endpoint, restate_listener, shutdown.clone());
    let mut health_server =
        spawn_health_server(health_listener, probe_state.clone(), shutdown.clone());
    let mut scim_server = spawn_scim_server(scim_listener, scim_state, shutdown.clone());

    tracing::info!(
        port = args.port,
        health_port = args.health_port,
        scim_port = args.scim_port,
        header_trust_mode = ?header_trust_mode,
        restate_admin_url = %probe_state.admin_base_url(),
        metrics_url = metrics_endpoint_url(&moa_config.metrics).unwrap_or_else(|| "disabled".to_string()),
        "starting moa-orchestrator"
    );
    readiness.store(true, Ordering::Release);
    let _cron_bootstrap = spawn_default_cron_bootstrap(
        probe_state.clone(),
        cron_bootstrap_ingress_url(&restate_ingress_url),
    );

    tokio::select! {
        result = &mut restate_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            health_server.abort();
            scim_server.abort();
            result.context("join Restate handler server")?;
            bail!("Restate handler server exited unexpectedly");
        }
        result = &mut health_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            restate_server.abort();
            scim_server.abort();
            result.context("join health probe server")??;
            bail!("health probe server exited unexpectedly");
        }
        result = &mut scim_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            restate_server.abort();
            health_server.abort();
            result.context("join SCIM server")??;
            bail!("SCIM server exited unexpectedly");
        }
        signal = shutdown_signal() => {
            signal?;
            tracing::info!("shutdown signal received, draining");
            readiness.store(false, Ordering::Release);

            if let Some(poller_handle) = poller_handle {
                poller_handle.shutdown().await;
            }
            if let Some(reaper_handle) = approval_reaper_handle {
                reaper_handle.shutdown().await;
            }

            if let Some(writer) = OrchestratorCtx::current().lineage_writer.clone() {
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
        }
    }

    Ok(())
}

fn header_trust_mode_from_config(config: &MoaConfig) -> HeaderTrustMode {
    match config.auth.header_trust {
        AuthHeaderTrustKind::Strict => HeaderTrustMode::Strict,
        AuthHeaderTrustKind::Lenient => HeaderTrustMode::Lenient,
    }
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

async fn build_database_pool(
    database_url: &str,
    database_search_path: &str,
    max_connections: u32,
    connect_timeout: Duration,
) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(connect_timeout)
        .after_connect({
            let database_search_path = database_search_path.to_string();
            move |conn, _meta| {
                let database_search_path = database_search_path.clone();
                Box::pin(async move {
                    sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                        .bind(database_search_path)
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            }
        })
        .connect(database_url)
        .await
        .map_err(Into::into)
}

async fn apply_database_migrations(config: &MoaConfig, _pool: &PgPool) -> anyhow::Result<()> {
    moa_migrations::run(config.database.admin_url())
        .await
        .context("apply database migrations")?;
    Ok(())
}

fn build_fga_client(config: &MoaConfig) -> anyhow::Result<moa_authz::FgaClient> {
    let openfga = config
        .authz
        .openfga
        .as_ref()
        .context("authz.openfga config missing")?;
    moa_authz::FgaClient::new(moa_authz::FgaConfig {
        url: openfga.url.clone(),
        preshared_key: openfga.preshared_key.clone(),
        store_id: openfga.store_id.clone(),
        model_id: openfga.model_id.clone(),
        timeout_ms: openfga.timeout_ms,
    })
    .context("build OpenFGA client")
}

fn start_authz_outbox_poller(
    pool: &PgPool,
    fga_client: moa_authz::FgaClient,
) -> moa_authz::PollerHandle {
    let outbox_poller =
        moa_authz::OutboxPoller::new(pool.clone(), fga_client, moa_authz::PollerConfig::default());
    let poller_handle = outbox_poller.spawn();
    tracing::info!("authz outbox poller started");
    poller_handle
}

fn start_approval_reaper_if_configured(
    pool: &PgPool,
    config: &MoaConfig,
    resolver: Arc<dyn AwakeableResolver>,
) -> anyhow::Result<Option<ApprovalReaperHandle>> {
    if config.async_authz.provider != AsyncAuthzKind::Builtin {
        return Ok(None);
    }
    let handle = ApprovalReaper::new(pool.clone()).spawn(resolver);
    tracing::info!("approval reaper started");
    Ok(Some(handle))
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

#[derive(Debug, Deserialize)]
struct DeploymentListResponse {
    deployments: Vec<RegisteredDeployment>,
}

#[derive(Debug, Deserialize)]
struct RegisteredDeployment {
    id: String,
    services: Vec<RegisteredService>,
    uri: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RegisteredService {
    name: String,
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

fn spawn_default_cron_bootstrap(state: ProbeState, ingress_url: String) -> JoinHandle<()> {
    tokio::spawn(async move {
        for attempt in 1..=CRON_BOOTSTRAP_ATTEMPTS {
            match state.fetch_deployments().await {
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

fn cron_bootstrap_ingress_url(configured_ingress_url: &str) -> String {
    let trimmed = configured_ingress_url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return format!("http://localhost:{DEFAULT_RESTATE_INGRESS_PORT}");
    }
    trimmed.to_string()
}

async fn install_default_cron_jobs(ingress_url: &str) -> anyhow::Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("build cron-bootstrap HTTP client")?;
    let ingress_url = ingress_url.trim_end_matches('/');

    for job in default_cron_jobs() {
        let response = client
            .post(format!("{ingress_url}/CronJob/{}/configure", job.key))
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
            let text = response.text().await.unwrap_or_default();
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

fn expected_service_names() -> Vec<&'static str> {
    expected_service_names_for_internal_eval(cfg!(feature = "internal-eval-runner"))
}

fn expected_service_names_for_internal_eval(internal_eval_enabled: bool) -> Vec<&'static str> {
    let mut names = DEFAULT_EXPECTED_SERVICE_NAMES.to_vec();
    if internal_eval_enabled {
        names.extend_from_slice(INTERNAL_EVAL_SERVICE_NAMES);
    }
    names
}

fn services_registered(deployments: &[RegisteredDeployment]) -> bool {
    let expected_services = expected_service_names();
    services_registered_with_expected(deployments, &expected_services)
}

fn services_registered_with_expected(
    deployments: &[RegisteredDeployment],
    expected_services: &[&str],
) -> bool {
    deployments.iter().any(|deployment| {
        expected_services.iter().all(|expected| {
            deployment
                .services
                .iter()
                .any(|service| service.name == *expected)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{
        RegisteredDeployment, RegisteredService, env_flag_from_reader,
        expected_service_names_for_internal_eval, services_registered,
        services_registered_with_expected,
    };

    fn deployment_with_services(services: &[&str]) -> RegisteredDeployment {
        RegisteredDeployment {
            id: "dp_test".to_string(),
            uri: Some("http://localhost:10020".to_string()),
            services: services
                .iter()
                .map(|name| RegisteredService {
                    name: (*name).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn default_expected_services_hide_hosted_eval() {
        let names = expected_service_names_for_internal_eval(false);

        assert!(
            !names.contains(&"Eval"),
            "default product readiness must not expect hosted Eval service"
        );
        assert!(
            !names.contains(&"EvalRun"),
            "default product readiness must not expect hosted EvalRun workflow"
        );
        assert!(
            names.contains(&"Experiments"),
            "default product readiness should include Experiments"
        );
        assert!(
            names.contains(&"ExperimentRun"),
            "default product readiness should include ExperimentRun"
        );
        assert!(
            names.contains(&"ExperimentTrialRun"),
            "default product readiness should include ExperimentTrialRun"
        );
    }

    #[test]
    fn internal_eval_gate_adds_hosted_eval_services() {
        let names = expected_service_names_for_internal_eval(true);

        assert_eq!(
            names.iter().filter(|name| **name == "Eval").count(),
            1,
            "internal eval gate should add Eval exactly once"
        );
        assert_eq!(
            names.iter().filter(|name| **name == "EvalRun").count(),
            1,
            "internal eval gate should add EvalRun exactly once"
        );
        assert!(
            names.contains(&"Experiments"),
            "internal eval mode should keep Experiments registered"
        );
        assert!(
            names.contains(&"ExperimentRun"),
            "internal eval mode should keep ExperimentRun registered"
        );
        assert!(
            names.contains(&"ExperimentTrialRun"),
            "internal eval mode should keep ExperimentTrialRun registered"
        );
    }

    #[test]
    fn registration_check_requires_all_expected_services() {
        let names =
            expected_service_names_for_internal_eval(cfg!(feature = "internal-eval-runner"));
        let deployments = vec![deployment_with_services(&names)];

        assert!(services_registered(&deployments));
    }

    #[test]
    fn registration_check_rejects_partial_deployments() {
        let deployments = vec![deployment_with_services(&["Health", "SessionStore"])];

        assert!(!services_registered(&deployments));
    }

    #[test]
    fn internal_eval_registration_requires_eval_and_eval_run_when_enabled() {
        let default_names = expected_service_names_for_internal_eval(false);
        let internal_names = expected_service_names_for_internal_eval(true);
        let default_deployment = vec![deployment_with_services(&default_names)];
        let internal_deployment = vec![deployment_with_services(&internal_names)];

        assert!(
            !services_registered_with_expected(&default_deployment, &internal_names),
            "internal eval readiness must reject a deployment missing Eval and EvalRun"
        );
        assert!(
            services_registered_with_expected(&internal_deployment, &internal_names),
            "internal eval readiness should accept Eval and EvalRun when explicitly enabled"
        );
    }

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
