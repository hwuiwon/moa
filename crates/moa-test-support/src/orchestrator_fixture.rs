//! Shared Restate, Postgres, and `moa-orchestrator` stack for integration tests.
//!
//! The fixture is shared while at least one test holds its returned `Arc`;
//! later callers isolate themselves with unique tenant/user prefixes.
//! Set `MOA_RESTATE_INGRESS_URL` to reuse an already-running stack
//! instead of starting Docker containers.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use moa_authz::{FgaClient, FgaConfig};
use moa_authz_schema::{SCHEMA_V1_JSON, TupleOp};
use moa_core::traits::{Identity, IdentityType};
use moa_core::wire::session_store::{AppendEventRequest, GetEventsRequest, InitSessionVoRequest};
use moa_core::wire::turn::{SessionSnapshot, StartTurnRequest, StartTurnResponse, TurnOutcome};
use moa_core::{
    events::Event, types::agent::AgentContext, types::agent::AgentKnowledgePolicy,
    types::agent::AgentKnowledgeScopeMode, types::agent::AgentPolicySnapshot,
    types::channel::Channel, types::contact::SessionActorRef, types::events_stream::EventRange,
    types::events_stream::EventRecord, types::identifiers::ModelId, types::identifiers::SessionId,
    types::identifiers::StoragePartitionId, types::identifiers::TenantId,
    types::identifiers::UserId, types::session::SessionMeta, types::session::SessionStatus,
};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tempfile::TempDir;
use testcontainers::core::{ContainerPort, Host, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

const POSTGRES_IMAGE: &str = "moa-postgres";
const POSTGRES_TAG: &str = "pg17-pgvector0.8.2-pgaudit";
const POSTGRES_DB: &str = "moa_test";
const POSTGRES_USER: &str = "moa_owner";
const POSTGRES_PASSWORD: &str = "dev";
const RESTATE_IMAGE: &str = "docker.restate.dev/restatedev/restate";
const RESTATE_TAG: &str = "1.7.0";
const OPENFGA_IMAGE: &str = "openfga/openfga";
const OPENFGA_TAG: &str = "v1.8.16";
const OPENFGA_PRESHARED_KEY: &str = "localdev-preshared-key-do-not-use-in-prod";
const REDIS_IMAGE: &str = "valkey/valkey";
const REDIS_TAG: &str = "8-alpine";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

mod client;
mod conversation;
mod fixture_capability;
mod openfga;
mod otlp_capture;
mod postgres;
mod process;
mod redis;
mod restate;
mod scripted_provider;

pub use client::{TestApiClient, TestSessionHandle};
pub use conversation::{ConversationOptions, drive_conversation};
pub use fixture_capability::{
    FixtureCapabilityAttempt, FixtureCapabilityCall, FixtureCapabilityController,
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool,
};
pub use otlp_capture::OtlpCapture;

use openfga::{
    bootstrap_openfga, external_fga_client, fixture_fga_endpoint_from_env, start_openfga_container,
    wait_for_openfga,
};
use postgres::{ensure_postgres_image, start_postgres_container, wait_for_postgres};
use process::{
    OrchestratorRestartConfig, OrchestratorSpawnConfig, locate_orchestrator_binary, pick_free_port,
    read_child_logs, repo_root, spawn_orchestrator, terminate_child, wait_for_orchestrator_health,
};
use redis::{start_redis_container, wait_for_redis};
use restate::{
    derive_admin_url, register_deployment, start_restate_container, trim_url,
    wait_for_registered_services, wait_for_restate_admin,
};
use scripted_provider::default_script;

/// Shared Restate-backed orchestrator fixture for integration tests.
pub struct OrchestratorTestFixture {
    /// Test-only HTTP helper pointed at the fixture's Restate ingress endpoint.
    pub client: TestApiClient,
    /// Restate ingress URL used by tests.
    pub ingress_url: String,
    /// Restate admin URL used for service registration and diagnostics.
    pub admin_url: String,
    /// Postgres URL used by the orchestrator.
    pub postgres_url: String,
    /// OpenFGA client used to seed authorization tuples for test identities.
    pub fga_client: Option<FgaClient>,
    /// Shared tenant/user prefix for this fixture process.
    pub test_prefix: String,
    _script_dir: Option<TempDir>,
    _postgres: Option<ContainerAsync<GenericImage>>,
    _restate: Option<ContainerAsync<GenericImage>>,
    _openfga: Option<ContainerAsync<GenericImage>>,
    _redis: Option<ContainerAsync<GenericImage>>,
    orchestrator: Mutex<Option<Child>>,
    restart_config: Option<OrchestratorRestartConfig>,
    fixture_capability: Option<fixture_capability::FixtureCapabilityRuntime>,
    otlp_capture: Option<OtlpCapture>,
}

impl OrchestratorTestFixture {
    /// Returns the shared fixture, starting it when no live fixture exists.
    pub async fn shared() -> Result<Arc<OrchestratorTestFixture>> {
        static FIXTURE: OnceCell<Mutex<Weak<OrchestratorTestFixture>>> = OnceCell::const_new();
        let slot = FIXTURE
            .get_or_init(|| async { Mutex::new(Weak::new()) })
            .await;
        let mut guard = slot.lock().await;
        if let Some(fixture) = guard.upgrade() {
            return Ok(fixture);
        }

        let fixture = Arc::new(Self::build().await?);
        *guard = Arc::downgrade(&fixture);
        Ok(fixture)
    }

    /// Returns an isolated namespace inside the shared fixture.
    pub async fn isolated(&self) -> IsolatedTest<'_> {
        IsolatedTest {
            fixture: self,
            prefix: format!("{}-{}", self.test_prefix, Uuid::now_v7().simple()),
        }
    }

    async fn build() -> Result<Self> {
        if let Ok(ingress_url) = std::env::var("MOA_RESTATE_INGRESS_URL") {
            return Self::external(ingress_url);
        }
        Self::internal(None, Vec::new(), None).await
    }

    /// Starts a dedicated fixture with a scripted provider fixture loaded at startup.
    pub async fn with_script(script: serde_json::Value) -> Result<Self> {
        if std::env::var("MOA_RESTATE_INGRESS_URL").is_ok() {
            bail!("dedicated scripted fixtures cannot use an external orchestrator");
        }
        Self::internal(Some(script), Vec::new(), None).await
    }

    /// Starts a dedicated scripted fixture with extra orchestrator process environment.
    pub async fn with_script_and_env(
        script: serde_json::Value,
        extra_env: Vec<(String, String)>,
    ) -> Result<Self> {
        if std::env::var("MOA_RESTATE_INGRESS_URL").is_ok() {
            bail!("dedicated scripted fixtures cannot use an external orchestrator");
        }
        Self::internal(Some(script), extra_env, None).await
    }

    /// Starts a restartable dedicated fixture with a scripted provider and fake MCP capabilities.
    pub async fn with_execution_fixture(
        script: serde_json::Value,
        options: FixtureCapabilityOptions,
    ) -> Result<Self> {
        if std::env::var("MOA_RESTATE_INGRESS_URL").is_ok() {
            bail!("dedicated execution fixtures cannot use an external orchestrator");
        }
        validate_execution_fixture_env(&options.orchestrator_env)?;
        let extra_env = options.orchestrator_env.clone();
        Self::internal(Some(script), extra_env, Some(options)).await
    }

    fn external(raw_ingress_url: String) -> Result<Self> {
        let repo_root = repo_root();
        let ingress_url = trim_url(&raw_ingress_url)?;
        let admin_url = std::env::var("MOA_RESTATE_ADMIN_URL")
            .ok()
            .map(|url| trim_url(&url))
            .transpose()?
            .unwrap_or_else(|| derive_admin_url(&ingress_url));
        let postgres_url = std::env::var("MOA_DATABASE_URL").unwrap_or_default();
        let fga_client = external_fga_client(&repo_root)?;
        let client = TestApiClient::new(&ingress_url)
            .context("construct test client")?
            .with_identity(default_test_identity());
        Ok(Self {
            client,
            ingress_url,
            admin_url,
            postgres_url,
            fga_client,
            test_prefix: format!("external-{}", Uuid::now_v7().simple()),
            _script_dir: None,
            _postgres: None,
            _restate: None,
            _openfga: None,
            _redis: None,
            orchestrator: Mutex::new(None),
            restart_config: None,
            fixture_capability: None,
            otlp_capture: None,
        })
    }

    async fn internal(
        script: Option<serde_json::Value>,
        mut extra_env: Vec<(String, String)>,
        capability_options: Option<FixtureCapabilityOptions>,
    ) -> Result<Self> {
        let repo_root = repo_root();
        ensure_postgres_image(&repo_root).await?;
        let postgres = start_postgres_container().await?;
        let postgres_port =
            fixture_host_port_ipv4(&postgres, "postgres database", 5432.tcp()).await?;
        let postgres_url = format!(
            "postgres://{POSTGRES_USER}:{POSTGRES_PASSWORD}@127.0.0.1:{postgres_port}/{POSTGRES_DB}"
        );
        wait_for_postgres(&postgres_url).await?;

        let restate = start_restate_container().await?;
        let ingress_port = fixture_host_port_ipv4(&restate, "restate ingress", 8080.tcp()).await?;
        let admin_port = fixture_host_port_ipv4(&restate, "restate admin", 9070.tcp()).await?;
        let ingress_url = format!("http://127.0.0.1:{ingress_port}");
        let admin_url = format!("http://127.0.0.1:{admin_port}");
        wait_for_restate_admin(&admin_url).await?;

        let (openfga_url, openfga_container, openfga_preshared_key) =
            match fixture_fga_endpoint_from_env() {
                Some(endpoint) => (endpoint.url, None, endpoint.preshared_key),
                None => {
                    let openfga = start_openfga_container().await?;
                    let openfga_port =
                        fixture_host_port_ipv4(&openfga, "openfga api", 8080.tcp()).await?;
                    (
                        format!("http://127.0.0.1:{openfga_port}"),
                        Some(openfga),
                        OPENFGA_PRESHARED_KEY.to_string(),
                    )
                }
            };
        wait_for_openfga(&openfga_url).await?;
        let fga_config = bootstrap_openfga(&openfga_url, &openfga_preshared_key).await?;
        let fga_client =
            FgaClient::new(fga_config.clone()).context("build fixture OpenFGA client")?;

        // The orchestrator binary is built with the `redis` runtime-cache backend, so the internal
        // path must supply a Redis endpoint. Honor an ambient MOA_RUNTIME_CACHE_REDIS_URL (e.g. the
        // compose stack) when set; otherwise boot a throwaway Valkey container so the lane is
        // hermetic and does not depend on exported cache env.
        let (redis_url, redis_container) = match std::env::var("MOA_RUNTIME_CACHE_REDIS_URL") {
            Ok(url) if !url.trim().is_empty() => {
                let url = url.trim().to_string();
                wait_for_redis(&url).await?;
                (url, None)
            }
            _ => {
                let redis = start_redis_container().await?;
                let redis_port = fixture_host_port_ipv4(&redis, "valkey redis", 6379.tcp()).await?;
                let redis_url = format!("redis://127.0.0.1:{redis_port}/0");
                wait_for_redis(&redis_url).await?;
                (redis_url, Some(redis))
            }
        };

        let script_dir = tempfile::Builder::new()
            .prefix("moa-scripted-provider-")
            .tempdir()
            .context("create scripted-provider tempdir")?;
        let script_path = script_dir.path().join("default-script.json");
        let script_body = match script {
            Some(script) => {
                serde_json::to_vec(&script).context("serialize scripted provider fixture")?
            }
            None => default_script(),
        };
        std::fs::write(&script_path, script_body).with_context(|| {
            format!("write scripted provider fixture {}", script_path.display())
        })?;

        let journal_path = capability_options
            .as_ref()
            .map(|_| script_dir.path().join("scripted-requests.jsonl"));
        if let Some(path) = &journal_path {
            std::fs::write(path, []).with_context(|| {
                format!(
                    "create scripted-provider request journal {}",
                    path.display()
                )
            })?;
        }
        let fixture_capability = match capability_options {
            Some(options) => {
                let runtime = fixture_capability::FixtureCapabilityRuntime::start(options).await?;
                let mcp_servers = serde_json::to_string(&json!([{
                    "name": "fixture-capability",
                    "transport": "http",
                    "url": runtime.endpoint(),
                    "trust_tool_annotations": true
                }]))
                .context("serialize fixture MCP server configuration")?;
                extra_env.push(("MOA_MCP_SERVERS_JSON".to_string(), mcp_servers));
                Some(runtime)
            }
            None => None,
        };
        let client = TestApiClient::new(&ingress_url)
            .context("construct test client")?
            .with_identity(default_test_identity());
        let otlp_capture = OtlpCapture::start(format!(
            "moa-orchestrator-fixture-{}",
            Uuid::now_v7().simple()
        ))
        .await
        .context("start fixture OTLP collector")?;

        let orchestrator_bin = locate_orchestrator_binary(&repo_root).await?;
        let orchestrator_port = pick_free_port()?;
        let health_port = pick_free_port()?;
        let scim_port = pick_free_port()?;
        let restart_config = journal_path
            .as_ref()
            .map(|journal_path| OrchestratorRestartConfig {
                binary: orchestrator_bin.clone(),
                port: orchestrator_port,
                health_port,
                scim_port,
                postgres_url: postgres_url.clone(),
                admin_url: admin_url.clone(),
                ingress_url: ingress_url.clone(),
                redis_url: redis_url.clone(),
                script_path: script_path.clone(),
                journal_path: journal_path.clone(),
                fga_config: fga_config.clone(),
                extra_env: extra_env.clone(),
                otlp_endpoint: otlp_capture.endpoint().to_string(),
                observability_service_name: otlp_capture.resource_name().to_string(),
            });
        let mut orchestrator_guard = match &restart_config {
            Some(config) => config.spawn()?,
            None => spawn_orchestrator(OrchestratorSpawnConfig {
                binary: &orchestrator_bin,
                port: orchestrator_port,
                health_port,
                scim_port,
                postgres_url: &postgres_url,
                admin_url: &admin_url,
                ingress_url: &ingress_url,
                redis_url: &redis_url,
                script_path: &script_path,
                journal_path: None,
                fga_config: &fga_config,
                extra_env: &extra_env,
                otlp_endpoint: otlp_capture.endpoint(),
                observability_service_name: otlp_capture.resource_name(),
            })?,
        };
        let deployment_uri = restart_config.as_ref().map_or_else(
            || format!("http://host.docker.internal:{orchestrator_port}"),
            OrchestratorRestartConfig::deployment_uri,
        );
        wait_for_orchestrator_health(
            health_port,
            orchestrator_guard
                .child_mut()
                .context("new orchestrator child guard is unexpectedly disarmed")?,
        )
        .await?;
        register_deployment(&admin_url, &deployment_uri).await?;
        wait_for_registered_services(&admin_url).await?;
        let orchestrator = orchestrator_guard
            .disarm()
            .context("healthy orchestrator child guard is unexpectedly disarmed")?;

        Ok(Self {
            client,
            ingress_url,
            admin_url,
            postgres_url,
            fga_client: Some(fga_client),
            test_prefix: format!("fixture-{}", Uuid::now_v7().simple()),
            _script_dir: Some(script_dir),
            _postgres: Some(postgres),
            _restate: Some(restate),
            _openfga: openfga_container,
            _redis: redis_container,
            orchestrator: Mutex::new(Some(orchestrator)),
            restart_config,
            fixture_capability,
            otlp_capture: Some(otlp_capture),
        })
    }

    /// Returns the fixture-owned OTLP capture surface.
    ///
    /// External orchestrator mode cannot guarantee process exporter settings and
    /// therefore fails instead of returning a misleading empty collector.
    pub fn otlp_capture(&self) -> Result<&OtlpCapture> {
        self.otlp_capture.as_ref().context(
            "fixture OTLP capture is unavailable when MOA_RESTATE_INGRESS_URL selects external orchestrator mode",
        )
    }

    /// Restarts only the dedicated orchestrator child while retaining all dependency services.
    pub async fn restart_orchestrator(&self) -> Result<()> {
        self.restart_orchestrator_with_extra_env(Vec::new()).await
    }

    /// Restarts the dedicated orchestrator once with additional child-process environment.
    ///
    /// The additional environment is not retained by later calls to
    /// [`Self::restart_orchestrator`], allowing a test to arm one crash window and
    /// then recover with the fixture's original configuration.
    pub async fn restart_orchestrator_with_env(
        &self,
        extra_env: Vec<(String, String)>,
    ) -> Result<()> {
        self.restart_orchestrator_with_extra_env(extra_env).await
    }

    async fn restart_orchestrator_with_extra_env(
        &self,
        extra_env: Vec<(String, String)>,
    ) -> Result<()> {
        let config = self.restart_config.as_ref().context(
            "orchestrator fixture is external or was not created with with_execution_fixture",
        )?;
        validate_execution_fixture_env(&extra_env)?;
        let mut child_env = config.extra_env.clone();
        let mut keys = child_env
            .iter()
            .map(|(key, _)| key.clone())
            .collect::<std::collections::BTreeSet<_>>();
        for (key, value) in &extra_env {
            if !keys.insert(key.clone()) {
                bail!("duplicate execution fixture environment key `{key}`");
            }
            child_env.push((key.clone(), value.clone()));
        }
        let mut orchestrator = self.orchestrator.lock().await;
        if let Some(child) = orchestrator.take() {
            terminate_child(child);
        }

        wait_for_postgres(&config.postgres_url)
            .await
            .context("restart fixture retained Postgres health check")?;
        wait_for_restate_admin(&config.admin_url)
            .await
            .context("restart fixture retained Restate health check")?;
        wait_for_openfga(&config.fga_config.url)
            .await
            .context("restart fixture retained OpenFGA health check")?;
        wait_for_redis(&config.redis_url)
            .await
            .context("restart fixture retained Redis health check")?;

        let mut child_guard = spawn_orchestrator(OrchestratorSpawnConfig {
            binary: &config.binary,
            port: config.port,
            health_port: config.health_port,
            scim_port: config.scim_port,
            postgres_url: &config.postgres_url,
            admin_url: &config.admin_url,
            ingress_url: &config.ingress_url,
            redis_url: &config.redis_url,
            script_path: &config.script_path,
            journal_path: Some(&config.journal_path),
            fga_config: &config.fga_config,
            extra_env: &child_env,
            otlp_endpoint: &config.otlp_endpoint,
            observability_service_name: &config.observability_service_name,
        })?;
        wait_for_orchestrator_health(
            config.health_port,
            child_guard
                .child_mut()
                .context("restarted orchestrator child guard is unexpectedly disarmed")?,
        )
        .await
        .context("restart dedicated orchestrator fixture child")?;
        register_deployment(&config.admin_url, &config.deployment_uri())
            .await
            .context("register restarted orchestrator fixture deployment")?;
        wait_for_registered_services(&config.admin_url)
            .await
            .context("wait for restarted orchestrator fixture services")?;
        *orchestrator = Some(
            child_guard
                .disarm()
                .context("healthy restarted orchestrator child guard is unexpectedly disarmed")?,
        );
        Ok(())
    }

    /// Reads the dedicated scripted-provider JSONL journal in append order.
    pub fn scripted_requests(&self) -> Result<Vec<serde_json::Value>> {
        let path = self
            .restart_config
            .as_ref()
            .map(|config| config.journal_path.as_path())
            .context("scripted request inspection requires with_execution_fixture")?;
        parse_scripted_requests(path)
    }

    /// Explicitly truncates the dedicated scripted-provider request journal.
    pub fn reset_scripted_requests(&self) -> Result<()> {
        let path = self
            .restart_config
            .as_ref()
            .map(|config| config.journal_path.as_path())
            .context("scripted request reset requires with_execution_fixture")?;
        OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .with_context(|| format!("truncate scripted-provider journal {}", path.display()))?;
        Ok(())
    }

    /// Returns exit status when the dedicated orchestrator died unexpectedly.
    pub async fn unexpected_orchestrator_exit(&self) -> Result<Option<String>> {
        let mut orchestrator = self.orchestrator.lock().await;
        let Some(child) = orchestrator.as_mut() else {
            return Ok(Some(
                "dedicated orchestrator child is not present in the fixture".to_string(),
            ));
        };
        let Some(status) = child
            .try_wait()
            .context("poll dedicated orchestrator child status")?
        else {
            return Ok(None);
        };
        Ok(Some(format!("{status}{}", read_child_logs(child))))
    }

    /// Returns the fake MCP capability controller for opt-in execution fixtures.
    #[must_use]
    pub fn fixture_capability(&self) -> Option<&FixtureCapabilityController> {
        self.fixture_capability
            .as_ref()
            .map(fixture_capability::FixtureCapabilityRuntime::controller)
    }

    /// Grants the provided identity tenant-operator access.
    pub async fn grant_tenant_operator_identity(
        &self,
        identity: &Identity,
        tenant_id: TenantId,
    ) -> Result<()> {
        self.apply_raw_tuple(
            TupleOp::Write,
            &identity_subject(identity),
            "operator",
            &format!("tenant:{tenant_id}"),
        )
        .await
        .context("grant fixture tenant operator")
    }

    async fn grant_session_participant(
        &self,
        identity: &Identity,
        session_id: SessionId,
    ) -> Result<()> {
        self.apply_raw_tuple(
            TupleOp::Write,
            &identity_subject(identity),
            "participant",
            &format!("session:{session_id}"),
        )
        .await
        .context("grant fixture session participation")
    }

    /// Grants the fixture client's default identity tenant-admin access.
    pub async fn grant_default_tenant_admin(&self, tenant_id: TenantId) -> Result<()> {
        let identity = self
            .client
            .identity
            .as_ref()
            .context("fixture test client must carry identity headers")?;
        self.apply_raw_tuple(
            TupleOp::Write,
            &identity_subject(identity),
            "admin",
            &format!("tenant:{tenant_id}"),
        )
        .await
        .context("grant fixture tenant admin")
    }

    async fn apply_raw_tuple(
        &self,
        op: TupleOp,
        user: &str,
        relation: &str,
        object: &str,
    ) -> Result<()> {
        let fga = self
            .fga_client
            .as_ref()
            .context("fixture OpenFGA client is unavailable")?;
        let tuple = json!({
            "user": user,
            "relation": relation,
            "object": object,
        });
        let body = match op {
            TupleOp::Write => json!({
                "authorization_model_id": fga.model_id(),
                "writes": { "tuple_keys": [tuple] },
            }),
            TupleOp::Delete => json!({
                "authorization_model_id": fga.model_id(),
                "deletes": { "tuple_keys": [tuple] },
            }),
        };
        fga.apply_raw(body).await.context("apply OpenFGA tuple")
    }
}

async fn fixture_host_port_ipv4(
    container: &ContainerAsync<GenericImage>,
    label: &'static str,
    port: ContainerPort,
) -> Result<u16> {
    let mut last_error = None;
    for attempt in 1..=10 {
        match container.get_host_port_ipv4(port).await {
            Ok(host_port) => return Ok(host_port),
            Err(error) => {
                let error = error.to_string();
                tracing::debug!(
                    mapping = label,
                    container_id = %container.id(),
                    requested_port = %port,
                    attempt,
                    error = %error,
                    "fixture container host-port mapping unavailable"
                );
                last_error = Some(error);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let available_ports = match container.ports().await {
        Ok(ports) => format!("{ports:?}"),
        Err(error) => format!("unavailable: {error}"),
    };
    let last_error = last_error
        .as_deref()
        .unwrap_or("no get_host_port_ipv4 error recorded");
    bail!(
        "fixture container host-port mapping `{label}` failed after retries; container_id={}; requested_port={port}; available_ports={available_ports}; last_error={last_error}",
        container.id()
    )
}

fn validate_execution_fixture_env(extra_env: &[(String, String)]) -> Result<()> {
    let mut keys = std::collections::BTreeSet::new();
    for (key, _) in extra_env {
        if !keys.insert(key) {
            bail!("duplicate execution fixture environment key `{key}`");
        }
        if matches!(
            key.as_str(),
            "MOA_MCP_SERVERS_JSON" | "MOA_SCRIPTED_PROVIDER_REQUEST_LOG" | "MOA_PROVIDERS_OVERRIDE"
        ) {
            bail!("execution fixture owns reserved environment key `{key}`");
        }
    }
    Ok(())
}

fn parse_scripted_requests(path: &Path) -> Result<Vec<serde_json::Value>> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read scripted-provider journal {}", path.display()))?;
    contents
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line).with_context(|| {
                format!(
                    "decode scripted-provider journal {} line {}",
                    path.display(),
                    index + 1
                )
            })
        })
        .collect()
}

impl Drop for OrchestratorTestFixture {
    fn drop(&mut self) {
        if let Some(child) = self.orchestrator.get_mut().take() {
            terminate_child(child);
        }
        if let Some(runtime) = self.fixture_capability.as_mut() {
            runtime.stop();
        }
        if let Some(capture) = self.otlp_capture.as_mut() {
            capture.stop();
        }
    }
}

fn default_test_identity() -> Identity {
    Identity {
        identity_type: IdentityType::Operator,
        id: Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0001),
        tenant_id: TenantId::from(Uuid::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0001)),
        api_key_id: None,
        acting_on_behalf_of: None,
    }
}

fn identity_subject(identity: &Identity) -> String {
    if let Some(api_key_id) = identity.api_key_id {
        return format!("api_key:{api_key_id}");
    }
    match identity.identity_type {
        IdentityType::Operator => format!("operator:{}", identity.id),
        IdentityType::Agent => format!("agent:{}", identity.id),
        IdentityType::Service => format!("service:{}", identity.id),
        IdentityType::Contact => format!("contact:{}", identity.id),
    }
}

fn fixture_agent_context() -> AgentContext {
    let snapshot = AgentPolicySnapshot {
        knowledge_policy: AgentKnowledgePolicy {
            mode: AgentKnowledgeScopeMode::Disabled,
            ..AgentKnowledgePolicy::default()
        },
        ..AgentPolicySnapshot::default()
    };
    let mut context = AgentContext::system_default();
    context.policy_snapshot = json!(snapshot);
    context
}

/// Isolated namespace within a shared orchestrator fixture.
pub struct IsolatedTest<'a> {
    /// Parent fixture.
    pub fixture: &'a OrchestratorTestFixture,
    /// Unique test prefix for tenant/user identifiers.
    pub prefix: String,
}

impl IsolatedTest<'_> {
    /// Returns the fixture client.
    #[must_use]
    pub fn client(&self) -> &TestApiClient {
        &self.fixture.client
    }

    /// Creates a unique storage partition identifier for this isolated test.
    #[must_use]
    pub fn storage_partition_id(&self, suffix: &str) -> StoragePartitionId {
        StoragePartitionId::new(format!("{}-{suffix}", self.prefix))
    }

    /// Creates a unique user identifier for this isolated test.
    #[must_use]
    pub fn user_id(&self, suffix: &str) -> UserId {
        UserId::new(format!("{}-{suffix}", self.prefix))
    }

    /// Creates, persists, and initializes a real session for Session VO tests.
    pub async fn create_session(&self, suffix: &str) -> Result<SessionId> {
        let session_id = SessionId::new();
        let now = Utc::now();
        let identity = self
            .client()
            .identity
            .as_ref()
            .context("fixture test client must carry identity headers")?
            .clone();
        self.fixture
            .grant_tenant_operator_identity(&identity, identity.tenant_id)
            .await?;
        let meta = SessionMeta {
            id: session_id,
            tenant_id: identity.tenant_id,
            title: Some(format!("{}-{suffix}", self.prefix)),
            status: SessionStatus::Created,
            channel: Channel::Chat,
            active_channel_binding_id: None,
            model: ModelId::new("scripted-loadtest"),
            created_at: now,
            updated_at: now,
            completed_at: None,
            parent_session_id: None,
            contact: None,
            created_by: Some(SessionActorRef::Identity { id: identity.id }),
            contact_promoted_from_id: None,
            agent_context: Some(fixture_agent_context()),
            total_input_tokens: 0,
            total_input_tokens_uncached: 0,
            total_input_tokens_cache_write: 0,
            total_input_tokens_cache_read: 0,
            total_output_tokens: 0,
            total_cost_cents: 0,
            event_count: 0,
            last_checkpoint_seq: None,
        };
        self.client()
            .create_session(meta.clone())
            .await
            .context("create session through orchestrator client")?;
        self.fixture
            .grant_session_participant(&identity, session_id)
            .await?;
        self.client()
            .append_event(
                session_id,
                Event::SessionCreated {
                    tenant_id: identity.tenant_id,
                    contact_id: None,
                    created_by: Some(SessionActorRef::Identity { id: identity.id }),
                    model: ModelId::new("scripted-loadtest"),
                    channel: Channel::Chat,
                },
            )
            .await
            .context("append session-created event through orchestrator client")?;
        self.client()
            .init_session_vo(session_id, meta)
            .await
            .context("initialize Session VO through orchestrator client")?;
        Ok(session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nonrestartable_fixture() -> OrchestratorTestFixture {
        OrchestratorTestFixture {
            client: TestApiClient::new("http://127.0.0.1:1")
                .expect("construct inert fixture client"),
            ingress_url: "http://127.0.0.1:1".to_string(),
            admin_url: "http://127.0.0.1:2".to_string(),
            postgres_url: String::new(),
            fga_client: None,
            test_prefix: "inert".to_string(),
            _script_dir: None,
            _postgres: None,
            _restate: None,
            _openfga: None,
            _redis: None,
            orchestrator: Mutex::new(None),
            restart_config: None,
            fixture_capability: None,
            otlp_capture: None,
        }
    }

    fn journal_fixture() -> (OrchestratorTestFixture, PathBuf) {
        let directory = tempfile::tempdir().expect("create request-journal tempdir");
        let journal_path = directory.path().join("requests.jsonl");
        std::fs::write(&journal_path, []).expect("create request journal");
        let mut fixture = nonrestartable_fixture();
        fixture._script_dir = Some(directory);
        fixture.restart_config = Some(OrchestratorRestartConfig {
            binary: PathBuf::from("unused-orchestrator"),
            port: 1,
            health_port: 2,
            scim_port: 3,
            postgres_url: String::new(),
            admin_url: String::new(),
            ingress_url: String::new(),
            redis_url: String::new(),
            script_path: PathBuf::from("unused-script"),
            journal_path: journal_path.clone(),
            fga_config: FgaConfig {
                url: String::new(),
                preshared_key: String::new(),
                store_id: String::new(),
                model_id: String::new(),
                timeout_ms: 1,
            },
            extra_env: Vec::new(),
            otlp_endpoint: "http://127.0.0.1:1/v1/traces".to_string(),
            observability_service_name: "unused-fixture".to_string(),
        });
        (fixture, journal_path)
    }

    #[test]
    fn scripted_request_journal_preserves_nonempty_jsonl_order() {
        // Pins: request inspection is deterministic across blank lines and reports malformed rows.
        let (fixture, path) = journal_fixture();
        std::fs::write(&path, "{\"request\":1}\n\n {\"request\":2}\n")
            .expect("write request journal");

        let requests = fixture.scripted_requests().expect("parse request journal");

        assert_eq!(
            requests,
            vec![json!({ "request": 1 }), json!({ "request": 2 })]
        );
        fixture
            .reset_scripted_requests()
            .expect("truncate request journal");
        assert!(
            fixture
                .scripted_requests()
                .expect("read reset journal")
                .is_empty()
        );

        std::fs::write(&path, "{\"request\":1}\nnot-json\n")
            .expect("write malformed request journal");
        let error = fixture
            .scripted_requests()
            .expect_err("malformed JSONL should fail");
        assert!(
            error.to_string().contains("line 2"),
            "error should identify the malformed line: {error:#}"
        );
    }

    #[tokio::test]
    async fn nonexecution_fixture_exposes_no_capability_and_rejects_restart() {
        // Pins: existing fixture constructors remain opt-in-free and cannot silently restart.
        let fixture = nonrestartable_fixture();

        assert!(fixture.fixture_capability().is_none());
        let capture_error = match fixture.otlp_capture() {
            Ok(_) => panic!("external-shaped fixture must reject OTLP capture"),
            Err(error) => error,
        };
        assert!(
            capture_error
                .to_string()
                .contains("external orchestrator mode"),
            "unexpected OTLP capture error: {capture_error:#}"
        );
        assert!(fixture.scripted_requests().is_err());
        assert!(fixture.reset_scripted_requests().is_err());
        let error = fixture
            .restart_orchestrator()
            .await
            .expect_err("non-execution fixture restart must be rejected");
        assert!(error.to_string().contains("with_execution_fixture"));
    }
}
