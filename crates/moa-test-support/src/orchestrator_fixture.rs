//! Shared Restate, Postgres, and `moa-orchestrator` stack for integration tests.
//!
//! The fixture is shared while at least one test holds its returned `Arc`;
//! later callers isolate themselves with unique workspace/user prefixes.
//! Set `MOA_RESTATE_INGRESS_URL` to reuse an already-running stack
//! instead of starting Docker containers.

use std::collections::HashMap;
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
use moa_core::wire::{
    AppendEventRequest, GetEventsRequest, InitSessionVoRequest, SessionSnapshot, StartTurnRequest,
    StartTurnResponse, TurnOutcome,
};
use moa_core::{
    Channel, Event, EventRange, EventRecord, ModelId, SessionActorRef, SessionId, SessionMeta,
    SessionStatus, TenantId, UserId, WorkspaceId,
};
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::json;
use sqlx::postgres::PgPoolOptions;
use tempfile::TempDir;
use testcontainers::core::{Host, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::sync::{Mutex, OnceCell};
use uuid::Uuid;

const POSTGRES_IMAGE: &str = "moa-postgres-age";
const POSTGRES_TAG: &str = "pg17-age1.7.0";
const POSTGRES_DB: &str = "moa_test";
const POSTGRES_USER: &str = "moa_owner";
const POSTGRES_PASSWORD: &str = "dev";
const RESTATE_IMAGE: &str = "docker.restate.dev/restatedev/restate";
const RESTATE_TAG: &str = "1.6.2";
const OPENFGA_IMAGE: &str = "openfga/openfga";
const OPENFGA_TAG: &str = "v1.8.16";
const OPENFGA_PRESHARED_KEY: &str = "localdev-preshared-key-do-not-use-in-prod";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// Shared workspace/user prefix for this fixture process.
    pub workspace_prefix: String,
    _script_dir: Option<TempDir>,
    _postgres: Option<ContainerAsync<GenericImage>>,
    _restate: Option<ContainerAsync<GenericImage>>,
    _openfga: Option<ContainerAsync<GenericImage>>,
    orchestrator: Mutex<Option<Child>>,
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

    async fn build() -> Result<Self> {
        if let Ok(ingress_url) = std::env::var("MOA_RESTATE_INGRESS_URL") {
            return Self::external(ingress_url);
        }
        Self::internal(None).await
    }

    /// Starts a dedicated fixture with a scripted provider fixture loaded at startup.
    pub async fn with_script(script: serde_json::Value) -> Result<Self> {
        if std::env::var("MOA_RESTATE_INGRESS_URL").is_ok() {
            bail!("dedicated scripted fixtures cannot use an external orchestrator");
        }
        Self::internal(Some(script)).await
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
            workspace_prefix: format!("external-{}", Uuid::now_v7().simple()),
            _script_dir: None,
            _postgres: None,
            _restate: None,
            _openfga: None,
            orchestrator: Mutex::new(None),
        })
    }

    async fn internal(script: Option<serde_json::Value>) -> Result<Self> {
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

        let openfga = start_openfga_container().await?;
        let openfga_port = openfga.get_host_port_ipv4(8080.tcp()).await?;
        let openfga_url = format!("http://127.0.0.1:{openfga_port}");
        wait_for_openfga(&openfga_url).await?;
        let fga_config = bootstrap_openfga(&openfga_url, OPENFGA_PRESHARED_KEY).await?;
        let fga_client =
            FgaClient::new(fga_config.clone()).context("build fixture OpenFGA client")?;

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

        let orchestrator_bin = locate_orchestrator_binary(&repo_root).await?;
        let orchestrator_port = pick_free_port()?;
        let health_port = pick_free_port()?;
        let scim_port = pick_free_port()?;
        let mut orchestrator = spawn_orchestrator(OrchestratorSpawnConfig {
            binary: &orchestrator_bin,
            port: orchestrator_port,
            health_port,
            scim_port,
            postgres_url: &postgres_url,
            admin_url: &admin_url,
            ingress_url: &ingress_url,
            script_path: &script_path,
            fga_config: &fga_config,
        })?;
        wait_for_orchestrator_health(health_port, &mut orchestrator).await?;
        let deployment_uri = format!("http://host.docker.internal:{orchestrator_port}");
        register_deployment(&admin_url, &deployment_uri).await?;
        wait_for_registered_services(&admin_url).await?;

        let client = TestApiClient::new(&ingress_url)
            .context("construct test client")?
            .with_identity(default_test_identity());
        Ok(Self {
            client,
            ingress_url,
            admin_url,
            postgres_url,
            fga_client: Some(fga_client),
            workspace_prefix: format!("fixture-{}", Uuid::now_v7().simple()),
            _script_dir: Some(script_dir),
            _postgres: Some(postgres),
            _restate: Some(restate),
            _openfga: Some(openfga),
            orchestrator: Mutex::new(Some(orchestrator)),
        })
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

fn default_test_identity() -> Identity {
    Identity {
        identity_type: IdentityType::User,
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
        IdentityType::User => format!("user:{}", identity.id),
        IdentityType::Agent => format!("agent:{}", identity.id),
        IdentityType::Service => format!("service:{}", identity.id),
        IdentityType::Contact => format!("contact:{}", identity.id),
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
    pub fn client(&self) -> &TestApiClient {
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
            agent_context: Some(moa_core::AgentContext::system_default()),
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

/// Small test-only HTTP helper for calling Restate ingress directly.
#[derive(Clone, Debug)]
pub struct TestApiClient {
    endpoint: String,
    http: reqwest::Client,
    identity: Option<Identity>,
}

impl TestApiClient {
    /// Creates a client for a Restate ingress endpoint.
    pub fn new(endpoint: impl Into<String>) -> Result<Self> {
        let endpoint = endpoint.into().trim_end_matches('/').to_string();
        url::Url::parse(&endpoint).with_context(|| format!("parse orchestrator URL {endpoint}"))?;
        Ok(Self {
            endpoint,
            http: reqwest::Client::new(),
            identity: None,
        })
    }

    /// Attaches trusted identity headers to all requests.
    #[must_use]
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Persists one session metadata row.
    pub async fn create_session(&self, meta: SessionMeta) -> Result<SessionId> {
        self.post_call("/SessionStore/create_session", &meta).await
    }

    /// Initializes one Session virtual object.
    pub async fn init_session_vo(&self, session_id: SessionId, meta: SessionMeta) -> Result<()> {
        self.post_void(
            "/SessionStore/init_session_vo",
            &InitSessionVoRequest { session_id, meta },
        )
        .await
    }

    /// Appends one event to the durable session log.
    pub async fn append_event(&self, session_id: SessionId, event: Event) -> Result<u64> {
        self.post_call(
            "/SessionStore/append_event",
            &AppendEventRequest { session_id, event },
        )
        .await
    }

    /// Loads one session metadata row.
    pub async fn get_session(&self, session_id: SessionId) -> Result<SessionMeta> {
        self.post_call("/SessionStore/get_session", &session_id)
            .await
    }

    /// Loads persisted events for one session.
    pub async fn get_events(
        &self,
        session_id: SessionId,
        range: EventRange,
    ) -> Result<Vec<EventRecord>> {
        self.post_call(
            "/SessionStore/get_events",
            &GetEventsRequest { session_id, range },
        )
        .await
    }

    /// Returns a handle scoped to one Session virtual object.
    pub fn session(&self, session_id: impl Into<String>) -> TestSessionHandle<'_> {
        TestSessionHandle {
            client: self,
            session_id: session_id.into(),
        }
    }

    /// Sends an authenticated JSON POST request and decodes a JSON response.
    pub async fn post_call<Req, Resp>(&self, path: &str, body: &Req) -> Result<Resp>
    where
        Req: serde::Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        let response = self.authed(
            self.http
                .post(format!("{}{path}", self.endpoint))
                .json(body),
        );
        decode_response(response.send().await.context("send orchestrator request")?).await
    }

    async fn post_call_with_idempotency<Req, Resp>(
        &self,
        path: &str,
        body: &Req,
        idempotency_key: Option<&str>,
    ) -> Result<Resp>
    where
        Req: serde::Serialize + ?Sized,
        Resp: serde::de::DeserializeOwned,
    {
        let mut request = self.authed(
            self.http
                .post(format!("{}{path}", self.endpoint))
                .json(body),
        );
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        decode_response(request.send().await.context("send orchestrator request")?).await
    }

    async fn post_empty_call<Resp>(&self, path: &str) -> Result<Resp>
    where
        Resp: serde::de::DeserializeOwned,
    {
        let response = self.authed(self.http.post(format!("{}{path}", self.endpoint)));
        decode_response(response.send().await.context("send orchestrator request")?).await
    }

    /// Sends an authenticated JSON POST request that must return a success status.
    pub async fn post_void<Req>(&self, path: &str, body: &Req) -> Result<()>
    where
        Req: serde::Serialize + ?Sized,
    {
        let response = self
            .authed(
                self.http
                    .post(format!("{}{path}", self.endpoint))
                    .json(body),
            )
            .send()
            .await
            .context("send orchestrator request")?;
        ensure_success(response).await
    }

    fn authed(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let Some(identity) = &self.identity else {
            return request;
        };
        let identity_type = match identity.identity_type {
            IdentityType::User => "user",
            IdentityType::Agent => "agent",
            IdentityType::Service => "service",
            IdentityType::Contact => "contact",
        };
        let mut request = request
            .header("x-moa-identity-type", identity_type)
            .header("x-moa-identity-id", identity.id.to_string())
            .header("x-moa-tenant-id", identity.tenant_id.to_string());
        if let Some(api_key_id) = identity.api_key_id {
            request = request.header("x-moa-api-key-id", api_key_id.to_string());
        }
        if let Some(user_id) = identity.acting_on_behalf_of {
            request = request.header("x-moa-acting-on-behalf-of", user_id.to_string());
        }
        request
    }
}

/// Test-only handle scoped to one Session virtual object.
pub struct TestSessionHandle<'a> {
    client: &'a TestApiClient,
    session_id: String,
}

impl TestSessionHandle<'_> {
    /// Starts a new turn for the session.
    pub async fn start_turn(
        &self,
        request: StartTurnRequest,
        idempotency_key: Option<&str>,
    ) -> Result<StartTurnResponse> {
        self.client
            .post_call_with_idempotency(
                &format!("/Session/{}/start_turn", self.session_id),
                &request,
                idempotency_key,
            )
            .await
    }

    /// Reads a non-blocking session snapshot.
    pub async fn snapshot(&self) -> Result<SessionSnapshot> {
        self.client
            .post_empty_call(&format!("/Session/{}/snapshot", self.session_id))
            .await
    }

    /// Polls snapshots until the requested turn's terminal outcome is visible.
    pub async fn await_turn_outcome(
        &self,
        turn_id: &str,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Result<TurnOutcome> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot().await?;
            if let Some(outcome) = snapshot.last_outcome
                && outcome.turn_id == turn_id
            {
                return Ok(outcome);
            }
            if Instant::now() >= deadline {
                bail!("turn {turn_id} did not complete within {timeout:?}");
            }
            tokio::time::sleep(poll_interval).await;
        }
    }
}

async fn decode_response<Resp>(response: reqwest::Response) -> Result<Resp>
where
    Resp: serde::de::DeserializeOwned,
{
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read orchestrator response")?;
    if !status.is_success() {
        bail!("orchestrator returned bad status {status}: {body}");
    }
    serde_json::from_str(&body).context("decode orchestrator response")
}

async fn ensure_success(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    let body = response
        .text()
        .await
        .context("read orchestrator response")?;
    if !status.is_success() {
        bail!("orchestrator returned bad status {status}: {body}");
    }
    Ok(())
}

async fn response_json_or_error(
    response: reqwest::Response,
    operation: &str,
) -> Result<serde_json::Value> {
    let status = response.status();
    let body = response
        .text()
        .await
        .with_context(|| format!("read OpenFGA {operation} response"))?;
    if !status.is_success() {
        bail!("OpenFGA {operation} returned {status}: {body}");
    }
    serde_json::from_str(&body).with_context(|| format!("decode OpenFGA {operation} response"))
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

async fn start_openfga_container() -> Result<ContainerAsync<GenericImage>> {
    GenericImage::new(OPENFGA_IMAGE, OPENFGA_TAG)
        .with_exposed_port(8080.tcp())
        .with_wait_for(WaitFor::seconds(1))
        .with_env_var("OPENFGA_DATASTORE_ENGINE", "memory")
        .with_env_var("OPENFGA_AUTHN_METHOD", "preshared")
        .with_env_var("OPENFGA_AUTHN_PRESHARED_KEYS", OPENFGA_PRESHARED_KEY)
        .with_env_var("OPENFGA_LOG_FORMAT", "json")
        .with_cmd(["run"])
        .start()
        .await
        .context("start OpenFGA testcontainer")
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

async fn wait_for_openfga(openfga_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match client.get(format!("{openfga_url}/healthz")).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            Ok(response) if Instant::now() < deadline => {
                tracing::debug!(status = %response.status(), "waiting for OpenFGA health");
            }
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, "waiting for OpenFGA health");
            }
            Ok(response) => bail!(
                "OpenFGA did not become healthy; last status {}",
                response.status()
            ),
            Err(error) => return Err(error).context("OpenFGA did not become healthy"),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn bootstrap_openfga(openfga_url: &str, preshared_key: &str) -> Result<FgaConfig> {
    let client = reqwest::Client::new();
    let store_name = format!("moa-test-{}", Uuid::now_v7().simple());
    let store = response_json_or_error(
        client
            .post(format!("{openfga_url}/stores"))
            .bearer_auth(preshared_key)
            .json(&json!({ "name": store_name }))
            .send()
            .await
            .context("create fixture OpenFGA store")?,
        "CreateStore",
    )
    .await?;
    let store_id = store
        .get("id")
        .and_then(|value| value.as_str())
        .context("CreateStore response missing id")?
        .to_string();

    let model = serde_json::from_str::<serde_json::Value>(SCHEMA_V1_JSON)
        .context("parse embedded OpenFGA model")?;
    let model_response = response_json_or_error(
        client
            .post(format!(
                "{openfga_url}/stores/{store_id}/authorization-models"
            ))
            .bearer_auth(preshared_key)
            .json(&model)
            .send()
            .await
            .context("write fixture OpenFGA authorization model")?,
        "WriteAuthorizationModel",
    )
    .await?;
    let model_id = model_response
        .get("authorization_model_id")
        .and_then(|value| value.as_str())
        .context("WriteAuthorizationModel response missing authorization_model_id")?
        .to_string();

    Ok(FgaConfig {
        url: openfga_url.to_string(),
        preshared_key: preshared_key.to_string(),
        store_id,
        model_id,
        timeout_ms: 5000,
    })
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
        "--features",
        "provider-overrides",
    ])
    .status()
    .await
    .context("build moa-orchestrator-bin for test fixture")?;
    if !status.success() {
        bail!(
            "cargo build -p moa-orchestrator --bin moa-orchestrator-bin --features provider-overrides failed"
        );
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

struct OrchestratorSpawnConfig<'a> {
    binary: &'a Path,
    port: u16,
    health_port: u16,
    scim_port: u16,
    postgres_url: &'a str,
    admin_url: &'a str,
    ingress_url: &'a str,
    script_path: &'a Path,
    fga_config: &'a FgaConfig,
}

fn spawn_orchestrator(config: OrchestratorSpawnConfig<'_>) -> Result<Child> {
    Command::new(config.binary)
        .arg("--port")
        .arg(config.port.to_string())
        .arg("--health-port")
        .arg(config.health_port.to_string())
        .arg("--scim-port")
        .arg(config.scim_port.to_string())
        .env("MOA_DATABASE_URL", config.postgres_url)
        .env("MOA_RESTATE_ADMIN_URL", config.admin_url)
        .env("MOA_RESTATE_INGRESS_URL", config.ingress_url)
        .env(
            "MOA_PROVIDERS_OVERRIDE",
            format!("scripted:{}", config.script_path.display()),
        )
        .env("MOA_AUTHZ_OPENFGA_URL", &config.fga_config.url)
        .env(
            "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
            &config.fga_config.preshared_key,
        )
        .env("MOA_AUTHZ_OPENFGA_STORE_ID", &config.fga_config.store_id)
        .env("MOA_AUTHZ_OPENFGA_MODEL_ID", &config.fga_config.model_id)
        .env("MOA_OBSERVABILITY_ENVIRONMENT", "test")
        .env("MOA_LINEAGE_SINK", "null")
        .env("RUST_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn orchestrator binary {}", config.binary.display()))
}

async fn wait_for_orchestrator_health(health_port: u16, child: &mut Child) -> Result<()> {
    let client = reqwest::Client::new();
    let health_url = format!("http://127.0.0.1:{health_port}/_health/live");
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait().context("poll orchestrator child status")? {
            let logs = read_child_logs(child);
            bail!("orchestrator exited before becoming healthy: {status}{logs}");
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

fn read_child_logs(child: &mut Child) -> String {
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        let mut stdout_text = String::new();
        let _ = stdout.read_to_string(&mut stdout_text);
        if !stdout_text.trim().is_empty() {
            output.push_str("\nstdout:\n");
            output.push_str(stdout_text.trim());
        }
    }
    if let Some(mut stderr) = child.stderr.take() {
        let mut stderr_text = String::new();
        let _ = stderr.read_to_string(&mut stderr_text);
        if !stderr_text.trim().is_empty() {
            output.push_str("\nstderr:\n");
            output.push_str(stderr_text.trim());
        }
    }
    output
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

fn external_fga_client(repo_root: &Path) -> Result<Option<FgaClient>> {
    let values = fga_env_values(repo_root);
    let Some(store_id) = fga_value(&values, "MOA_AUTHZ_OPENFGA_STORE_ID") else {
        return Ok(None);
    };
    let Some(model_id) = fga_value(&values, "MOA_AUTHZ_OPENFGA_MODEL_ID") else {
        return Ok(None);
    };
    let url = fga_value(&values, "MOA_AUTHZ_OPENFGA_URL")
        .unwrap_or_else(|| "http://127.0.0.1:10030".to_string());
    let preshared_key = fga_value(&values, "MOA_AUTHZ_OPENFGA_PRESHARED_KEY")
        .unwrap_or_else(|| OPENFGA_PRESHARED_KEY.to_string());
    FgaClient::new(FgaConfig {
        url,
        preshared_key,
        store_id,
        model_id,
        timeout_ms: 5000,
    })
    .map(Some)
    .context("build external OpenFGA client")
}

fn fga_env_values(repo_root: &Path) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for key in [
        "MOA_AUTHZ_OPENFGA_URL",
        "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
        "MOA_AUTHZ_OPENFGA_STORE_ID",
        "MOA_AUTHZ_OPENFGA_MODEL_ID",
    ] {
        if let Ok(value) = std::env::var(key) {
            values.insert(key.to_string(), value);
        }
    }

    if let Ok(contents) = std::fs::read_to_string(repo_root.join(".env.fga")) {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            values
                .entry(key.trim().to_string())
                .or_insert_with(|| value.trim().trim_matches('"').to_string());
        }
    }
    values
}

fn fga_value(values: &HashMap<String, String>, key: &str) -> Option<String> {
    values
        .get(key)
        .filter(|value| !value.trim().is_empty())
        .cloned()
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
