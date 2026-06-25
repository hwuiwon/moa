//! Shared runtime context for the Restate-backed orchestrator binary.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use moa_authz::FgaClient;
use moa_brain::pipeline::{memory::GraphMemoryRetriever, skills::SkillInjector};
use moa_core::{
    Channel, LineageHandle, MoaConfig,
    traits::{ChannelAdapter, EmbeddingProvider, Identity, IdentityType},
};
use moa_hands::ToolRouter;
use moa_providers::ProviderRegistry;
use moa_session::PostgresSessionStore;
use serde_json::Value;
use uuid::Uuid;

static CTX: OnceLock<Arc<OrchestratorCtx>> = OnceLock::new();

/// Persistence dependencies shared by handlers that read or write product data.
#[derive(Clone)]
pub struct PersistenceDeps {
    session_store: Arc<PostgresSessionStore>,
    graph_pool: sqlx::PgPool,
}

impl PersistenceDeps {
    /// Creates a persistence dependency group.
    #[must_use]
    pub fn new(session_store: Arc<PostgresSessionStore>, graph_pool: sqlx::PgPool) -> Self {
        Self {
            session_store,
            graph_pool,
        }
    }

    /// Returns the Postgres-backed session store.
    #[must_use]
    pub fn session_store(&self) -> Arc<PostgresSessionStore> {
        self.session_store.clone()
    }

    /// Returns the Postgres pool used by graph-memory and application repositories.
    #[must_use]
    pub fn graph_pool(&self) -> sqlx::PgPool {
        self.graph_pool.clone()
    }
}

/// AuthN/AuthZ dependencies shared by handler boundaries.
#[derive(Clone)]
pub struct AuthDeps {
    fga_client: Option<FgaClient>,
    auth_providers: moa_auth_providers::Providers,
}

impl AuthDeps {
    /// Creates an authentication and authorization dependency group.
    #[must_use]
    pub fn new(
        fga_client: Option<FgaClient>,
        auth_providers: moa_auth_providers::Providers,
    ) -> Self {
        Self {
            fga_client,
            auth_providers,
        }
    }

    /// Returns the configured OpenFGA client, when authorization is enabled.
    #[must_use]
    pub fn fga_client(&self) -> Option<FgaClient> {
        self.fga_client.clone()
    }

    /// Returns the authentication provider bundle.
    #[must_use]
    pub fn auth_providers(&self) -> moa_auth_providers::Providers {
        self.auth_providers.clone()
    }
}

/// LLM and embedding provider dependencies.
#[derive(Clone)]
pub struct ProviderDeps {
    registry: Arc<ProviderRegistry>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl ProviderDeps {
    /// Creates a provider dependency group.
    #[must_use]
    pub fn new(
        registry: Arc<ProviderRegistry>,
        embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self {
            registry,
            embedding_provider,
        }
    }

    /// Returns the configured LLM provider registry.
    #[must_use]
    pub fn registry(&self) -> Arc<ProviderRegistry> {
        self.registry.clone()
    }

    /// Returns the configured embedding provider, when available.
    #[must_use]
    pub fn embedding_provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embedding_provider.clone()
    }
}

/// Tool-routing dependencies exposed to turn and tool handlers.
#[derive(Clone)]
pub struct ToolDeps {
    router: Arc<ToolRouter>,
    schemas: Arc<Vec<Value>>,
}

impl ToolDeps {
    /// Creates a tool dependency group.
    #[must_use]
    pub fn new(router: Arc<ToolRouter>, schemas: Arc<Vec<Value>>) -> Self {
        Self { router, schemas }
    }

    /// Returns the configured tool router.
    #[must_use]
    pub fn router(&self) -> Arc<ToolRouter> {
        self.router.clone()
    }

    /// Returns the precompiled tool schemas exposed to model requests.
    #[must_use]
    pub fn schemas(&self) -> Arc<Vec<Value>> {
        self.schemas.clone()
    }
}

/// Memory retrieval dependencies for context compilation and memory APIs.
#[derive(Clone)]
pub struct MemoryDeps {
    graph_memory_retriever: Arc<GraphMemoryRetriever>,
    skill_injector: Arc<SkillInjector>,
}

impl MemoryDeps {
    /// Creates a memory dependency group.
    #[must_use]
    pub fn new(
        graph_memory_retriever: Arc<GraphMemoryRetriever>,
        skill_injector: Arc<SkillInjector>,
    ) -> Self {
        Self {
            graph_memory_retriever,
            skill_injector,
        }
    }

    /// Returns the shared graph-memory retriever.
    #[must_use]
    pub fn graph_memory_retriever(&self) -> Arc<GraphMemoryRetriever> {
        self.graph_memory_retriever.clone()
    }

    /// Returns the shared skill injector used by context compilation.
    #[must_use]
    pub fn skill_injector(&self) -> Arc<SkillInjector> {
        self.skill_injector.clone()
    }
}

/// Lineage capture and durable sink dependencies.
#[derive(Clone)]
pub struct LineageDeps {
    handle: Arc<dyn LineageHandle>,
    writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
}

impl LineageDeps {
    /// Creates a lineage dependency group.
    #[must_use]
    pub fn new(
        handle: Arc<dyn LineageHandle>,
        writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
    ) -> Self {
        Self { handle, writer }
    }

    /// Returns the hot-path lineage capture handle.
    #[must_use]
    pub fn handle(&self) -> Arc<dyn LineageHandle> {
        self.handle.clone()
    }

    /// Returns the durable lineage writer, when configured.
    #[must_use]
    pub fn writer(&self) -> Option<Arc<moa_lineage_sink::WriterHandle>> {
        self.writer.clone()
    }
}

/// Messaging adapter dependencies used for live channel updates.
#[derive(Clone, Default)]
pub struct MessagingDeps {
    adapters: Arc<HashMap<Channel, Arc<dyn ChannelAdapter>>>,
}

impl MessagingDeps {
    /// Creates a messaging dependency group from channel adapters.
    #[must_use]
    pub fn new(adapters: HashMap<Channel, Arc<dyn ChannelAdapter>>) -> Self {
        Self {
            adapters: Arc::new(adapters),
        }
    }

    /// Returns the adapter for a channel, when live outbound delivery is configured.
    #[must_use]
    pub fn adapter(&self, channel: Channel) -> Option<Arc<dyn ChannelAdapter>> {
        self.adapters.get(&channel).cloned()
    }
}

/// Runtime dependencies shared by every Restate handler in this binary.
pub struct OrchestratorDeps {
    /// Persistence dependencies shared by handlers that read or write product data.
    pub persistence: PersistenceDeps,
    /// AuthN/AuthZ dependencies shared by handler boundaries.
    pub auth: AuthDeps,
    /// LLM and embedding provider dependencies.
    pub providers: ProviderDeps,
    /// Tool-routing dependencies exposed to turn and tool handlers.
    pub tools: ToolDeps,
    /// Memory retrieval dependencies for context compilation and memory APIs.
    pub memory: MemoryDeps,
    /// Lineage capture and durable sink dependencies.
    pub lineage: LineageDeps,
    /// Messaging adapter dependencies used for live channel updates.
    pub messaging: MessagingDeps,
}

/// Runtime dependencies shared by every Restate handler in this binary.
///
/// Constructed once at startup from `main.rs` and installed via
/// [`OrchestratorCtx::install`]. Handlers read the current instance via
/// [`OrchestratorCtx::current`].
pub struct OrchestratorCtx {
    config: Arc<MoaConfig>,
    persistence: PersistenceDeps,
    auth: AuthDeps,
    providers: ProviderDeps,
    tools: ToolDeps,
    memory: MemoryDeps,
    lineage: LineageDeps,
    messaging: MessagingDeps,
}

impl OrchestratorCtx {
    /// Creates the process-wide orchestrator context from typed dependency groups.
    #[must_use]
    pub fn new(config: Arc<MoaConfig>, deps: OrchestratorDeps) -> Self {
        Self {
            config,
            persistence: deps.persistence,
            auth: deps.auth,
            providers: deps.providers,
            tools: deps.tools,
            memory: deps.memory,
            lineage: deps.lineage,
            messaging: deps.messaging,
        }
    }

    /// Installs the singleton runtime context during binary startup.
    pub fn install(ctx: Arc<Self>) -> Result<(), &'static str> {
        CTX.set(ctx)
            .map_err(|_| "OrchestratorCtx already installed")
    }

    /// Returns the installed context.
    ///
    /// Panics if startup forgot to install it before registering handlers.
    #[must_use]
    pub fn current() -> Arc<Self> {
        CTX.get().cloned().expect(
            "OrchestratorCtx not installed; call install() in main before registering handlers",
        )
    }

    /// Returns the current runtime configuration.
    #[must_use]
    pub fn current_config() -> Arc<MoaConfig> {
        Self::current().config()
    }

    /// Returns the current session store.
    #[must_use]
    pub fn current_session_store() -> Arc<PostgresSessionStore> {
        Self::current().session_store()
    }

    /// Returns the current graph/application Postgres pool.
    #[must_use]
    pub fn current_graph_pool() -> sqlx::PgPool {
        Self::current().graph_pool()
    }

    /// Returns the current provider registry.
    #[must_use]
    pub fn current_provider_registry() -> Arc<ProviderRegistry> {
        Self::current().provider_registry()
    }

    /// Returns the current tool router.
    #[must_use]
    pub fn current_tool_router() -> Arc<ToolRouter> {
        Self::current().tool_router()
    }

    /// Returns the current precompiled tool schemas.
    #[must_use]
    pub fn current_tool_schemas() -> Arc<Vec<Value>> {
        Self::current().tool_schemas()
    }

    /// Returns the current lineage handle.
    #[must_use]
    pub fn current_lineage() -> Arc<dyn LineageHandle> {
        Self::current().lineage()
    }

    /// Returns the configured live messaging adapter for a channel, when available.
    #[must_use]
    pub fn current_channel_adapter(channel: Channel) -> Option<Arc<dyn ChannelAdapter>> {
        Self::current().channel_adapter(channel)
    }

    /// Returns the current runtime configuration.
    #[must_use]
    pub fn config(&self) -> Arc<MoaConfig> {
        self.config.clone()
    }

    /// Returns the persistence dependency group.
    #[must_use]
    pub fn persistence_deps(&self) -> PersistenceDeps {
        self.persistence.clone()
    }

    /// Returns the authentication dependency group.
    #[must_use]
    pub fn auth_deps(&self) -> AuthDeps {
        self.auth.clone()
    }

    /// Returns the provider dependency group.
    #[must_use]
    pub fn provider_deps(&self) -> ProviderDeps {
        self.providers.clone()
    }

    /// Returns the tool dependency group.
    #[must_use]
    pub fn tool_deps(&self) -> ToolDeps {
        self.tools.clone()
    }

    /// Returns the memory dependency group.
    #[must_use]
    pub fn memory_deps(&self) -> MemoryDeps {
        self.memory.clone()
    }

    /// Returns the lineage dependency group.
    #[must_use]
    pub fn lineage_deps(&self) -> LineageDeps {
        self.lineage.clone()
    }

    /// Returns the messaging dependency group.
    #[must_use]
    pub fn messaging_deps(&self) -> MessagingDeps {
        self.messaging.clone()
    }

    /// Returns the session store from persistence dependencies.
    #[must_use]
    pub fn session_store(&self) -> Arc<PostgresSessionStore> {
        self.persistence.session_store()
    }

    /// Returns the graph/application Postgres pool from persistence dependencies.
    #[must_use]
    pub fn graph_pool(&self) -> sqlx::PgPool {
        self.persistence.graph_pool()
    }

    /// Returns the OpenFGA client from auth dependencies, when configured.
    #[must_use]
    pub fn fga_client(&self) -> Option<FgaClient> {
        self.auth.fga_client()
    }

    /// Returns the authentication provider bundle.
    #[must_use]
    pub fn auth_providers(&self) -> moa_auth_providers::Providers {
        self.auth.auth_providers()
    }

    /// Returns the provider registry from provider dependencies.
    #[must_use]
    pub fn provider_registry(&self) -> Arc<ProviderRegistry> {
        self.providers.registry()
    }

    /// Returns the embedding provider from provider dependencies, when available.
    #[must_use]
    pub fn embedding_provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.providers.embedding_provider()
    }

    /// Returns the tool router from tool dependencies.
    #[must_use]
    pub fn tool_router(&self) -> Arc<ToolRouter> {
        self.tools.router()
    }

    /// Returns the tool schemas from tool dependencies.
    #[must_use]
    pub fn tool_schemas(&self) -> Arc<Vec<Value>> {
        self.tools.schemas()
    }

    /// Returns the graph-memory retriever from memory dependencies.
    #[must_use]
    pub fn graph_memory_retriever(&self) -> Arc<GraphMemoryRetriever> {
        self.memory.graph_memory_retriever()
    }

    /// Returns the shared skill injector from memory dependencies.
    #[must_use]
    pub fn skill_injector(&self) -> Arc<SkillInjector> {
        self.memory.skill_injector()
    }

    /// Returns the lineage handle from lineage dependencies.
    #[must_use]
    pub fn lineage(&self) -> Arc<dyn LineageHandle> {
        self.lineage.handle()
    }

    /// Returns the configured live messaging adapter for a channel, when available.
    #[must_use]
    pub fn channel_adapter(&self, channel: Channel) -> Option<Arc<dyn ChannelAdapter>> {
        self.messaging.adapter(channel)
    }

    /// Returns the durable lineage writer from lineage dependencies, when configured.
    #[must_use]
    pub fn lineage_writer(&self) -> Option<Arc<moa_lineage_sink::WriterHandle>> {
        self.lineage.writer()
    }
}

/// Identity-header extraction failure.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdentityHeaderError {
    /// A required identity header was absent.
    #[error("missing identity header: {0}")]
    Missing(&'static str),
    /// One or more identity headers were malformed.
    #[error("malformed identity header: {0}")]
    Malformed(&'static str),
    /// Identity type was present but not recognized.
    #[error("unknown identity type: {0}")]
    UnknownType(String),
}

/// Controls how the orchestrator treats missing edge identity headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HeaderTrustMode {
    /// Reject requests that do not include the required identity header set.
    #[default]
    Strict,
    /// Accept requests without identity headers while Phase 1 handlers are wired.
    Lenient,
}

/// Process-wide identity header trust mode.
pub static HEADER_TRUST_MODE: OnceLock<HeaderTrustMode> = OnceLock::new();

/// Common header access for all Restate handler context variants.
pub trait RequestHeaders {
    /// Returns the request headers attached to the current Restate invocation.
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap;
}

impl RequestHeaders for restate_sdk::context::Context<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

impl RequestHeaders for restate_sdk::context::ObjectContext<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

impl RequestHeaders for restate_sdk::context::SharedObjectContext<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

impl RequestHeaders for restate_sdk::context::WorkflowContext<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

impl RequestHeaders for restate_sdk::context::SharedWorkflowContext<'_> {
    fn request_headers(&self) -> &restate_sdk::context::HeaderMap {
        self.headers()
    }
}

/// Extract a trusted identity from Restate request headers.
///
/// The core identity header set is all-or-nothing: either
/// `x-moa-identity-type`, `x-moa-identity-id`, and `x-moa-tenant-id` are all
/// present, or none are present. Optional API-key and delegation headers are
/// parsed only after the core set is valid.
pub fn extract_identity(
    headers: &restate_sdk::context::HeaderMap,
    mode: HeaderTrustMode,
) -> Result<Option<Identity>, IdentityHeaderError> {
    let get = |name: &'static str| headers.get(name).map(String::as_str);
    let raw_type = get("x-moa-identity-type");
    let raw_id = get("x-moa-identity-id");
    let raw_tenant = get("x-moa-tenant-id");

    let (raw_type, raw_id, raw_tenant) = match (raw_type, raw_id, raw_tenant) {
        (None, None, None) if mode == HeaderTrustMode::Strict => {
            return Err(IdentityHeaderError::Missing("x-moa-identity-type"));
        }
        (None, None, None) => return Ok(None),
        (Some(raw_type), Some(raw_id), Some(raw_tenant)) => (raw_type, raw_id, raw_tenant),
        _ => {
            return Err(IdentityHeaderError::Malformed(
                "partial identity headers; require all of type/id/tenant",
            ));
        }
    };

    let identity_type = parse_identity_type(raw_type)?;
    let id = parse_uuid(raw_id, "x-moa-identity-id")?;
    let tenant_id = moa_core::TenantId::from(parse_uuid(raw_tenant, "x-moa-tenant-id")?);
    let api_key_id = get("x-moa-api-key-id")
        .map(|value| parse_uuid(value, "x-moa-api-key-id"))
        .transpose()?;
    let acting_on_behalf_of = get("x-moa-acting-on-behalf-of")
        .map(|value| parse_uuid(value, "x-moa-acting-on-behalf-of"))
        .transpose()?;

    Ok(Some(Identity {
        identity_type,
        id,
        tenant_id,
        api_key_id,
        acting_on_behalf_of,
    }))
}

/// Extract identity from a Restate handler context using the configured trust mode.
pub fn current_identity(
    ctx: &impl RequestHeaders,
) -> Result<Option<Identity>, IdentityHeaderError> {
    let mode = HEADER_TRUST_MODE.get().copied().unwrap_or_default();
    let identity = extract_identity(ctx.request_headers(), mode)?;
    tracing::debug!(mode = ?mode, has_identity = identity.is_some(), "extracted request identity");
    Ok(identity)
}

fn parse_identity_type(value: &str) -> Result<IdentityType, IdentityHeaderError> {
    match value {
        "user" => Ok(IdentityType::User),
        "contact" => Ok(IdentityType::Contact),
        "agent" => Ok(IdentityType::Agent),
        "service" => Ok(IdentityType::Service),
        other => Err(IdentityHeaderError::UnknownType(other.to_string())),
    }
}

fn parse_uuid(value: &str, header: &'static str) -> Result<Uuid, IdentityHeaderError> {
    Uuid::parse_str(value).map_err(|_| IdentityHeaderError::Malformed(header))
}
