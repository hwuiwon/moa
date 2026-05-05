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
use clap::Parser;
use moa_core::{TelemetryConfig, init_observability, metrics_endpoint_url};
use moa_hands::ToolRouter;
use moa_orchestrator::{
    DeadMemoryStoreShim, OrchestratorCtx,
    config::OrchestratorConfig,
    objects::session::{Session, SessionImpl},
    objects::sub_agent::{SubAgent, SubAgentImpl},
    objects::workspace::{Workspace, WorkspaceImpl},
    restate_register::{IngestionVO, IngestionVOImpl},
    services::{
        health::{Health, HealthImpl},
        intent_manager::{IntentManager, IntentManagerImpl},
        llm_gateway::{LLMGateway, LLMGatewayImpl, ProviderRegistry},
        session_store::{SessionStore, SessionStoreImpl},
        tool_executor::{ToolExecutor, ToolExecutorImpl},
        workspace_store::{WorkspaceStore, WorkspaceStoreImpl},
    },
    workflows::{
        consolidate::{Consolidate, ConsolidateImpl},
        intent_discovery::{IntentDiscovery, IntentDiscoveryImpl},
    },
};
use moa_providers::build_embedding_provider_from_config;
use moa_session::PostgresSessionStore;
use reqwest::Client;
use restate_sdk::prelude::*;
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const DEFAULT_RESTATE_PORT: u16 = 9080;
const DEFAULT_HEALTH_PORT: u16 = 9081;
const ADMIN_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_DRAIN_DELAY: Duration = Duration::from_secs(5);
const EXPECTED_SERVICE_NAMES: &[&str] = &[
    "Consolidate",
    "Health",
    "IntentManager",
    "IntentDiscovery",
    "IngestionVO",
    "LLMGateway",
    "Session",
    "SessionStore",
    "SubAgent",
    "ToolExecutor",
    "Workspace",
    "WorkspaceStore",
];

/// Command line arguments for the orchestrator process.
#[derive(Debug, Parser)]
struct Args {
    /// HTTP port for the Restate handler endpoint.
    #[arg(long, default_value_t = DEFAULT_RESTATE_PORT)]
    port: u16,
    /// HTTP port for Kubernetes liveness/readiness probes.
    #[arg(long, default_value_t = DEFAULT_HEALTH_PORT)]
    health_port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = OrchestratorConfig::from_env()?;
    let moa_config = Arc::new(config.to_moa_config());
    let _telemetry = init_observability(
        moa_config.as_ref(),
        &TelemetryConfig {
            json_stdout: true,
            ..TelemetryConfig::default()
        },
    )?;
    let pool = PgPoolOptions::new()
        .max_connections(25)
        .connect(&config.postgres_url)
        .await?;
    moa_session::schema::migrate(&pool, None).await?;
    let session_store = Arc::new(
        PostgresSessionStore::from_existing_pool(&config.postgres_url, pool.clone()).await?,
    );

    let providers = Arc::new(ProviderRegistry::from_env());
    let embedding_provider = build_embedding_provider_from_config(moa_config.as_ref())?;
    let memory_store = Arc::new(DeadMemoryStoreShim);
    let tool_router = Arc::new(
        ToolRouter::from_config(moa_config.as_ref(), memory_store.clone())
            .await?
            .with_rule_store(session_store.clone())
            .with_session_store(session_store.clone()),
    );
    let ctx = Arc::new(OrchestratorCtx {
        config: moa_config.clone(),
        session_store: session_store.clone(),
        graph_pool: session_store.pool().clone(),
        memory_store: memory_store.clone(),
        providers: providers.clone(),
        embedding_provider: embedding_provider.clone(),
        tool_router: tool_router.clone(),
        tool_schemas: Arc::new(tool_router.tool_schemas()),
    });
    OrchestratorCtx::install(ctx).expect("install orchestrator ctx");
    let _ = memory_ingest::install_runtime_with_pool(pool.clone());

    let endpoint = Endpoint::builder()
        .bind(HealthImpl.serve())
        .bind(SessionStoreImpl::new(session_store.clone()).serve())
        .bind(
            IntentManagerImpl::new(
                session_store.clone(),
                embedding_provider.clone(),
                moa_config.clone(),
            )
            .serve(),
        )
        .bind(LLMGatewayImpl::new(providers).serve())
        .bind(IngestionVOImpl.serve())
        .bind(ToolExecutorImpl::new(tool_router.clone()).serve())
        .bind(WorkspaceStoreImpl::new(tool_router.clone()).serve())
        .bind(SessionImpl.serve())
        .bind(SubAgentImpl.serve())
        .bind(WorkspaceImpl.serve())
        .bind(ConsolidateImpl.serve())
        .bind(IntentDiscoveryImpl.serve())
        .build();

    let readiness = Arc::new(AtomicBool::new(false));
    let probe_state = ProbeState::new(readiness.clone(), pool.clone(), config.restate_admin_url)?;
    let shutdown = CancellationToken::new();

    let restate_listener = bind_listener(args.port).await?;
    let health_listener = bind_listener(args.health_port).await?;
    let mut restate_server = spawn_restate_server(endpoint, restate_listener, shutdown.clone());
    let mut health_server =
        spawn_health_server(health_listener, probe_state.clone(), shutdown.clone());

    tracing::info!(
        port = args.port,
        health_port = args.health_port,
        restate_admin_url = %probe_state.admin_base_url(),
        metrics_url = metrics_endpoint_url(&moa_config.metrics).unwrap_or_else(|| "disabled".to_string()),
        "starting moa-orchestrator"
    );
    readiness.store(true, Ordering::Release);

    tokio::select! {
        result = &mut restate_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            health_server.abort();
            result.context("join Restate handler server")?;
            bail!("Restate handler server exited unexpectedly");
        }
        result = &mut health_server => {
            readiness.store(false, Ordering::Release);
            shutdown.cancel();
            restate_server.abort();
            result.context("join health probe server")??;
            bail!("health probe server exited unexpectedly");
        }
        signal = shutdown_signal() => {
            signal?;
            tracing::info!("shutdown signal received, draining");
            readiness.store(false, Ordering::Release);

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

fn services_registered(deployments: &[RegisteredDeployment]) -> bool {
    deployments.iter().any(|deployment| {
        EXPECTED_SERVICE_NAMES.iter().all(|expected| {
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
        RegisteredDeployment, RegisteredService, env_flag_from_reader, services_registered,
    };

    fn deployment_with_services(services: &[&str]) -> RegisteredDeployment {
        RegisteredDeployment {
            id: "dp_test".to_string(),
            uri: Some("http://localhost:9080".to_string()),
            services: services
                .iter()
                .map(|name| RegisteredService {
                    name: (*name).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn registration_check_requires_all_expected_services() {
        let deployments = vec![deployment_with_services(&[
            "Consolidate",
            "Health",
            "IntentManager",
            "IntentDiscovery",
            "IngestionVO",
            "LLMGateway",
            "Session",
            "SessionStore",
            "SubAgent",
            "ToolExecutor",
            "Workspace",
            "WorkspaceStore",
        ])];

        assert!(services_registered(&deployments));
    }

    #[test]
    fn registration_check_rejects_partial_deployments() {
        let deployments = vec![deployment_with_services(&["Health", "SessionStore"])];

        assert!(!services_registered(&deployments));
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
