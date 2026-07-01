//! Shared Restate, Postgres, and `moa-orchestrator` stack for integration tests.
//!
//! The fixture is shared while at least one test holds its returned `Arc`;
//! later callers isolate themselves with unique tenant/user prefixes.
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
use moa_core::wire::session_store::{AppendEventRequest, GetEventsRequest, InitSessionVoRequest};
use moa_core::wire::turn::{SessionSnapshot, StartTurnRequest, StartTurnResponse, TurnOutcome};
use moa_core::{
    AgentContext, AgentKnowledgePolicy, AgentKnowledgeScopeMode, AgentPolicySnapshot, Channel,
    Event, EventRange, EventRecord, ModelId, SessionActorRef, SessionId, SessionMeta,
    SessionStatus, StoragePartitionId, TenantId, UserId,
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

const POSTGRES_IMAGE: &str = "moa-postgres";
const POSTGRES_TAG: &str = "pg17-pgvector0.8.2-pgaudit";
const POSTGRES_DB: &str = "moa_test";
const POSTGRES_USER: &str = "moa_owner";
const POSTGRES_PASSWORD: &str = "dev";
const RESTATE_IMAGE: &str = "docker.restate.dev/restatedev/restate";
const RESTATE_TAG: &str = "1.6.2";
const OPENFGA_IMAGE: &str = "openfga/openfga";
const OPENFGA_TAG: &str = "v1.8.16";
const OPENFGA_PRESHARED_KEY: &str = "localdev-preshared-key-do-not-use-in-prod";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);

mod client;
mod conversation;
mod openfga;
mod postgres;
mod process;
mod restate;
mod scripted_provider;

pub use client::{TestApiClient, TestSessionHandle};
pub use conversation::{
    ConversationOptions, drive_conversation, drive_conversation_cost, fetch_all_events,
};

use openfga::{bootstrap_openfga, external_fga_client, start_openfga_container, wait_for_openfga};
use postgres::{ensure_postgres_image, start_postgres_container, wait_for_postgres};
use process::{
    OrchestratorSpawnConfig, locate_orchestrator_binary, pick_free_port, repo_root,
    spawn_orchestrator, terminate_child, wait_for_orchestrator_health,
};
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
            prefix: format!("{}-{}", self.test_prefix, Uuid::now_v7().simple()),
        }
    }

    async fn build() -> Result<Self> {
        if let Ok(ingress_url) = std::env::var("MOA_RESTATE_INGRESS_URL") {
            return Self::external(ingress_url);
        }
        Self::internal(None, Vec::new()).await
    }

    /// Starts a dedicated fixture with a scripted provider fixture loaded at startup.
    pub async fn with_script(script: serde_json::Value) -> Result<Self> {
        if std::env::var("MOA_RESTATE_INGRESS_URL").is_ok() {
            bail!("dedicated scripted fixtures cannot use an external orchestrator");
        }
        Self::internal(Some(script), Vec::new()).await
    }

    /// Starts a dedicated scripted fixture with extra orchestrator process environment.
    pub async fn with_script_and_env(
        script: serde_json::Value,
        extra_env: Vec<(String, String)>,
    ) -> Result<Self> {
        if std::env::var("MOA_RESTATE_INGRESS_URL").is_ok() {
            bail!("dedicated scripted fixtures cannot use an external orchestrator");
        }
        Self::internal(Some(script), extra_env).await
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
            orchestrator: Mutex::new(None),
        })
    }

    async fn internal(
        script: Option<serde_json::Value>,
        extra_env: Vec<(String, String)>,
    ) -> Result<Self> {
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
            extra_env: &extra_env,
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
            test_prefix: format!("fixture-{}", Uuid::now_v7().simple()),
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
