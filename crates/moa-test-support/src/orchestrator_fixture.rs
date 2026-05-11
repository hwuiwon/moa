//! Shared Restate, Postgres, and `moa-orchestrator` stack for integration tests.
//!
//! The fixture is shared while at least one test holds its returned `Arc`;
//! later callers isolate themselves with unique workspace/user prefixes.
//! Set `MOA_TEST_EXTERNAL_INGRESS_URL` to reuse an already-running stack
//! instead of starting Docker containers.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use moa_core::{
    Event, ModelId, Platform, SessionId, SessionMeta, SessionStatus, UserId, WorkspaceId,
};
use moa_orchestrator_client::OrchestratorClient;
use reqwest::StatusCode;
use serde::Deserialize;
use sqlx::postgres::PgPoolOptions;
use tempfile::TempDir;
use testcontainers::core::{Host, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::sync::{Mutex, MutexGuard, OnceCell};
use uuid::Uuid;

const POSTGRES_IMAGE: &str = "moa-postgres-age";
const POSTGRES_TAG: &str = "pg17-age1.7.0";
const POSTGRES_DB: &str = "moa_test";
const POSTGRES_USER: &str = "moa_owner";
const POSTGRES_PASSWORD: &str = "dev";
const RESTATE_IMAGE: &str = "docker.restate.dev/restatedev/restate";
const RESTATE_TAG: &str = "1.6.2";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

/// Shared Restate-backed orchestrator fixture for integration tests.
pub struct OrchestratorTestFixture {
    /// Thin client pointed at the fixture's Restate ingress endpoint.
    pub client: OrchestratorClient,
    /// Restate ingress URL used by tests.
    pub ingress_url: String,
    /// Restate admin URL used for service registration and diagnostics.
    pub admin_url: String,
    /// Postgres URL used by the orchestrator.
    pub postgres_url: String,
    /// Shared workspace/user prefix for this fixture process.
    pub workspace_prefix: String,
    /// Scripted provider fixture path passed to the orchestrator.
    pub script_path: PathBuf,
    _script_dir: Option<TempDir>,
    _postgres: Option<ContainerAsync<GenericImage>>,
    _restate: Option<ContainerAsync<GenericImage>>,
    orchestrator: Mutex<Option<Child>>,
    serial_lock: Mutex<()>,
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
            prefix: format!("{}-{}", self.workspace_prefix, Uuid::now_v7().simple()),
        }
    }

    /// Acquires an exclusive fixture namespace for tests that mutate shared orchestrator state.
    pub async fn serialized(&self) -> SerializedTest<'_> {
        let guard = self.serial_lock.lock().await;
        let isolated = self.isolated().await;
        SerializedTest {
            isolated,
            _guard: guard,
        }
    }

    async fn build() -> Result<Self> {
        if let Ok(ingress_url) = std::env::var("MOA_TEST_EXTERNAL_INGRESS_URL") {
            return Self::external(ingress_url);
        }
        Self::internal().await
    }

    fn external(raw_ingress_url: String) -> Result<Self> {
        let ingress_url = trim_url(&raw_ingress_url)?;
        let admin_url = std::env::var("MOA_TEST_EXTERNAL_ADMIN_URL")
            .or_else(|_| std::env::var("RESTATE_ADMIN_URL"))
            .ok()
            .map(|url| trim_url(&url))
            .transpose()?
            .unwrap_or_else(|| derive_admin_url(&ingress_url));
        let postgres_url = std::env::var("MOA_TEST_EXTERNAL_POSTGRES_URL")
            .or_else(|_| std::env::var("POSTGRES_URL"))
            .unwrap_or_default();
        let client = OrchestratorClient::new(&ingress_url).context("construct test client")?;
        Ok(Self {
            client,
            ingress_url,
            admin_url,
            postgres_url,
            workspace_prefix: format!("external-{}", Uuid::now_v7().simple()),
            script_path: PathBuf::new(),
            _script_dir: None,
            _postgres: None,
            _restate: None,
            orchestrator: Mutex::new(None),
            serial_lock: Mutex::new(()),
        })
    }

    async fn internal() -> Result<Self> {
        let repo_root = repo_root();
        ensure_postgres_image(&repo_root).await?;
        let postgres = start_postgres_container().await?;
        let postgres_port = postgres.get_host_port_ipv4(5432.tcp()).await?;
        let postgres_url = format!(
            "postgres://{POSTGRES_USER}:{POSTGRES_PASSWORD}@127.0.0.1:{postgres_port}/{POSTGRES_DB}"
        );
        wait_for_postgres(&postgres_url).await?;

        let restate = start_restate_container().await?;
        let ingress_port = restate.get_host_port_ipv4(8080.tcp()).await?;
        let admin_port = restate.get_host_port_ipv4(9070.tcp()).await?;
        let ingress_url = format!("http://127.0.0.1:{ingress_port}");
        let admin_url = format!("http://127.0.0.1:{admin_port}");
        wait_for_restate_admin(&admin_url).await?;

        let script_dir = tempfile::Builder::new()
            .prefix("moa-scripted-provider-")
            .tempdir()
            .context("create scripted-provider tempdir")?;
        let script_path = script_dir.path().join("default-script.json");
        std::fs::write(&script_path, default_script()).with_context(|| {
            format!("write scripted provider fixture {}", script_path.display())
        })?;

        let orchestrator_bin = locate_orchestrator_binary(&repo_root).await?;
        let orchestrator_port = pick_free_port()?;
        let health_port = pick_free_port()?;
        let mut orchestrator = spawn_orchestrator(
            &orchestrator_bin,
            orchestrator_port,
            health_port,
            &postgres_url,
            &admin_url,
            &ingress_url,
            &script_path,
        )?;
        wait_for_orchestrator_health(health_port, &mut orchestrator).await?;
        let deployment_uri = format!("http://host.docker.internal:{orchestrator_port}");
        register_deployment(&admin_url, &deployment_uri).await?;
        wait_for_registered_services(&admin_url).await?;

        let client = OrchestratorClient::new(&ingress_url).context("construct test client")?;
        Ok(Self {
            client,
            ingress_url,
            admin_url,
            postgres_url,
            workspace_prefix: format!("fixture-{}", Uuid::now_v7().simple()),
            script_path,
            _script_dir: Some(script_dir),
            _postgres: Some(postgres),
            _restate: Some(restate),
            orchestrator: Mutex::new(Some(orchestrator)),
            serial_lock: Mutex::new(()),
        })
    }
}

impl Drop for OrchestratorTestFixture {
    fn drop(&mut self) {
        let Ok(mut guard) = self.orchestrator.try_lock() else {
            return;
        };
        if let Some(child) = guard.take() {
            terminate_child(child);
        }
    }
}

/// Isolated namespace within a shared orchestrator fixture.
pub struct IsolatedTest<'a> {
    /// Parent fixture.
    pub fixture: &'a OrchestratorTestFixture,
    /// Unique test prefix for workspace/user identifiers.
    pub prefix: String,
}

impl IsolatedTest<'_> {
    /// Returns the fixture client.
    #[must_use]
    pub fn client(&self) -> &OrchestratorClient {
        &self.fixture.client
    }

    /// Creates a unique workspace identifier for this isolated test.
    #[must_use]
    pub fn workspace_id(&self, suffix: &str) -> WorkspaceId {
        WorkspaceId::new(format!("{}-{suffix}", self.prefix))
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
        let workspace_id = self.workspace_id("workspace");
        let user_id = self.user_id("user");
        let meta = SessionMeta {
            id: session_id,
            workspace_id: workspace_id.clone(),
            user_id: user_id.clone(),
            title: Some(format!("{}-{suffix}", self.prefix)),
            status: SessionStatus::Created,
            platform: Platform::Cli,
            platform_channel: None,
            model: ModelId::new("scripted-loadtest"),
            created_at: now,
            updated_at: now,
            completed_at: None,
            parent_session_id: None,
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
        self.client()
            .append_event(
                session_id,
                Event::SessionCreated {
                    workspace_id,
                    user_id,
                    model: ModelId::new("scripted-loadtest"),
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

    /// Writes a new scripted provider fixture.
    ///
    /// This mutates the shared orchestrator script path. Tests needing custom
    /// scripts should call [`OrchestratorTestFixture::serialized`] or route
    /// within one shared script by session/workspace identifiers.
    pub async fn install_script(&self, script: serde_json::Value) -> Result<()> {
        if self.fixture.script_path.as_os_str().is_empty() {
            bail!("external orchestrator fixtures cannot install local provider scripts");
        }
        let bytes = serde_json::to_vec(&script).context("serialize scripted provider fixture")?;
        std::fs::write(&self.fixture.script_path, bytes).with_context(|| {
            format!(
                "write scripted provider fixture {}",
                self.fixture.script_path.display()
            )
        })
    }
}

/// Serialized isolated namespace for tests that need exclusive fixture access.
pub struct SerializedTest<'a> {
    isolated: IsolatedTest<'a>,
    _guard: MutexGuard<'a, ()>,
}

impl<'a> std::ops::Deref for SerializedTest<'a> {
    type Target = IsolatedTest<'a>;

    fn deref(&self) -> &Self::Target {
        &self.isolated
    }
}

async fn start_postgres_container() -> Result<ContainerAsync<GenericImage>> {
    GenericImage::new(POSTGRES_IMAGE, POSTGRES_TAG)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("POSTGRES_DB", POSTGRES_DB)
        .with_env_var("POSTGRES_USER", POSTGRES_USER)
        .with_env_var("POSTGRES_PASSWORD", POSTGRES_PASSWORD)
        .with_cmd([
            "postgres",
            "-c",
            "shared_preload_libraries=age,pgaudit",
            "-c",
            "session_preload_libraries=age",
            "-c",
            "wal_level=logical",
            "-c",
            "max_replication_slots=10",
            "-c",
            "max_wal_senders=10",
        ])
        .start()
        .await
        .context("start Postgres testcontainer")
}

async fn start_restate_container() -> Result<ContainerAsync<GenericImage>> {
    GenericImage::new(RESTATE_IMAGE, RESTATE_TAG)
        .with_exposed_port(8080.tcp())
        .with_exposed_port(9070.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("DO_NOT_TRACK", "1")
        .with_host("host.docker.internal", Host::HostGateway)
        .with_cmd(["--node-name=restate-test"])
        .start()
        .await
        .context("start Restate testcontainer")
}

async fn ensure_postgres_image(repo_root: &Path) -> Result<()> {
    let image = format!("{POSTGRES_IMAGE}:{POSTGRES_TAG}");
    let inspect_status = tokio::process::Command::new("docker")
        .args(["image", "inspect", &image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("inspect Postgres test image with Docker")?;
    if inspect_status.success() {
        return Ok(());
    }

    let build_status = tokio::process::Command::new("docker")
        .current_dir(repo_root)
        .args([
            "build",
            "-f",
            "docker/postgres/Dockerfile",
            "--build-arg",
            "AGE_REF=release/PG17/1.7.0",
            "--build-arg",
            "PGVECTOR_REF=v0.8.2",
            "-t",
            &image,
            ".",
        ])
        .status()
        .await
        .context("build Postgres test image with Docker")?;
    if !build_status.success() {
        bail!("docker build for {image} failed with status {build_status}");
    }
    Ok(())
}

async fn wait_for_postgres(postgres_url: &str) -> Result<()> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match PgPoolOptions::new()
            .max_connections(1)
            .connect(postgres_url)
            .await
        {
            Ok(pool) => {
                pool.close().await;
                return Ok(());
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for Postgres testcontainer");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => return Err(error).context("Postgres testcontainer did not become ready"),
        }
    }
}

async fn wait_for_restate_admin(admin_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match client.get(format!("{admin_url}/health")).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for Restate admin health");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for Restate admin health");
            }
            Ok(response) => bail!(
                "Restate admin did not become healthy; last status {}",
                response.status()
            ),
            Err(error) => return Err(error).context("Restate admin did not become healthy"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn locate_orchestrator_binary(repo_root: &Path) -> Result<PathBuf> {
    if let Ok(path) = std::env::var("MOA_ORCHESTRATOR_BIN") {
        let path = PathBuf::from(path);
        if path.exists() {
            return Ok(path);
        }
        bail!(
            "MOA_ORCHESTRATOR_BIN points to missing file {}",
            path.display()
        );
    }

    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| repo_root.join("target"));
    let candidate = target_dir.join("debug").join(format!(
        "moa-orchestrator-bin{}",
        std::env::consts::EXE_SUFFIX
    ));
    if candidate.exists() {
        return Ok(candidate);
    }

    let status = tokio::process::Command::new(
        std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string()),
    )
    .current_dir(repo_root)
    .args([
        "build",
        "-p",
        "moa-orchestrator",
        "--bin",
        "moa-orchestrator-bin",
    ])
    .status()
    .await
    .context("build moa-orchestrator-bin for test fixture")?;
    if !status.success() {
        bail!("cargo build -p moa-orchestrator --bin moa-orchestrator-bin failed");
    }
    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(anyhow!(
            "built orchestrator binary but did not find {}",
            candidate.display()
        ))
    }
}

fn spawn_orchestrator(
    binary: &Path,
    port: u16,
    health_port: u16,
    postgres_url: &str,
    admin_url: &str,
    ingress_url: &str,
    script_path: &Path,
) -> Result<Child> {
    Command::new(binary)
        .arg("--port")
        .arg(port.to_string())
        .arg("--health-port")
        .arg(health_port.to_string())
        .env("POSTGRES_URL", postgres_url)
        .env("RESTATE_ADMIN_URL", admin_url)
        .env("MOA_LOCAL_INGRESS_URL", ingress_url)
        .env(
            "MOA_PROVIDERS_OVERRIDE",
            format!("scripted:{}", script_path.display()),
        )
        .env("MOA__ENVIRONMENT", "test")
        .env("MOA_LINEAGE_SINK", "null")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn orchestrator binary {}", binary.display()))
}

async fn wait_for_orchestrator_health(health_port: u16, child: &mut Child) -> Result<()> {
    let client = reqwest::Client::new();
    let health_url = format!("http://127.0.0.1:{health_port}/_health/live");
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("poll orchestrator child status")? {
            bail!("orchestrator exited before becoming healthy: {status}");
        }
        match client.get(&health_url).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for orchestrator health");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for orchestrator health");
            }
            Ok(response) => bail!(
                "orchestrator did not become healthy; last status {}",
                response.status()
            ),
            Err(error) => return Err(error).context("orchestrator did not become healthy"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn register_deployment(admin_url: &str, deployment_uri: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let body = serde_json::json!({ "uri": deployment_uri });
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match client
            .post(format!("{admin_url}/deployments"))
            .json(&body)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if response.status() == StatusCode::CONFLICT => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                tracing::debug!(%status, body = %text, "waiting to register Restate deployment");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting to register Restate deployment");
            }
            Ok(response) => {
                let status = response.status();
                let text = response.text().await.unwrap_or_default();
                bail!("register deployment returned {status}: {text}");
            }
            Err(error) => return Err(error).context("register deployment with Restate"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn wait_for_registered_services(admin_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match client.get(format!("{admin_url}/deployments")).send().await {
            Ok(response) if response.status().is_success() => {
                let payload = response
                    .json::<DeploymentsResponse>()
                    .await
                    .context("decode Restate deployment list")?;
                if payload.deployments.iter().any(|deployment| {
                    deployment
                        .services
                        .iter()
                        .any(|service| service.name == "Session")
                }) {
                    return Ok(());
                }
            }
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for registered services");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for registered services");
            }
            Ok(response) => bail!(
                "registered services did not appear; last status {}",
                response.status()
            ),
            Err(error) => return Err(error).context("registered services did not appear"),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[derive(Deserialize)]
struct DeploymentsResponse {
    deployments: Vec<Deployment>,
}

#[derive(Deserialize)]
struct Deployment {
    services: Vec<RegisteredService>,
}

#[derive(Deserialize)]
struct RegisteredService {
    name: String,
}

fn pick_free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn trim_url(raw: &str) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("URL must be non-empty");
    }
    url::Url::parse(trimmed).with_context(|| format!("invalid URL {trimmed}"))?;
    Ok(trimmed.to_string())
}

fn derive_admin_url(ingress_url: &str) -> String {
    url::Url::parse(ingress_url)
        .ok()
        .and_then(|mut url| {
            url.set_port(Some(10011)).ok()?;
            Some(url.to_string().trim_end_matches('/').to_string())
        })
        .unwrap_or_else(|| "http://127.0.0.1:10011".to_string())
}

fn default_script() -> Vec<u8> {
    br#"{"default":{"completion":{"content":"ok","duration_ms":1,"input_tokens":64,"cached_input_tokens":0,"cache_write_input_tokens":0,"tool_calls":[]}}}"#.to_vec()
}

fn terminate_child(mut child: Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(child.id() as i32), Signal::SIGTERM);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(_) => return,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}
