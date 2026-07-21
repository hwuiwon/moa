//! Shared runtime context for the Restate-backed orchestrator binary.

use std::sync::{Arc, OnceLock};

use moa_authz::FgaClient;
use moa_brain::pipeline::{memory::GraphMemoryRetriever, skills::SkillInjector};
use moa_core::{
    config::MoaConfig,
    traits::LineageHandle,
    traits::{EmbeddingProvider, Identity, RuntimeCacheStore, SessionRepository},
};
use moa_crypto::KeyManagementProvider;
use moa_hands::ToolRouter;
use moa_providers::ProviderRegistry;
use moa_session::PostgresSessionStore;
use serde_json::Value;
use uuid::Uuid;

static CTX: OnceLock<Arc<OrchestratorCtx>> = OnceLock::new();

/// Persistence dependencies shared by handlers that read or write product data.
#[derive(Clone)]
pub struct PersistenceDeps {
    session_repository: Arc<dyn SessionRepository>,
    session_store_backend: Arc<PostgresSessionStore>,
    graph_pool: sqlx::PgPool,
}

impl PersistenceDeps {
    /// Creates a persistence dependency group.
    #[must_use]
    pub fn new(session_store: Arc<PostgresSessionStore>, graph_pool: sqlx::PgPool) -> Self {
        Self {
            session_repository: session_store.clone(),
            session_store_backend: session_store,
            graph_pool,
        }
    }

    /// Returns the session repository contract.
    #[must_use]
    pub fn session_store(&self) -> Arc<dyn SessionRepository> {
        self.session_repository.clone()
    }

    /// Returns the concrete Postgres session-store backend for composition-only surfaces.
    #[must_use]
    pub fn session_store_backend(&self) -> Arc<PostgresSessionStore> {
        self.session_store_backend.clone()
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
}

impl AuthDeps {
    /// Creates an authentication and authorization dependency group.
    #[must_use]
    pub fn new(fga_client: Option<FgaClient>) -> Self {
        Self { fga_client }
    }

    /// Returns the configured OpenFGA client, when authorization is enabled.
    #[must_use]
    pub fn fga_client(&self) -> Option<FgaClient> {
        self.fga_client.clone()
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
    kms: Arc<dyn KeyManagementProvider>,
    graph_memory_retriever: Arc<GraphMemoryRetriever>,
    skill_injector: Arc<SkillInjector>,
}

impl MemoryDeps {
    /// Creates a memory dependency group.
    #[must_use]
    pub fn new(
        kms: Arc<dyn KeyManagementProvider>,
        graph_memory_retriever: Arc<GraphMemoryRetriever>,
        skill_injector: Arc<SkillInjector>,
    ) -> Self {
        Self {
            kms,
            graph_memory_retriever,
            skill_injector,
        }
    }

    /// Returns the key-management provider used by graph-memory owners.
    #[must_use]
    pub fn kms(&self) -> Arc<dyn KeyManagementProvider> {
        self.kms.clone()
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

/// Lineage capture dependencies.
#[derive(Clone)]
pub struct LineageDeps {
    handle: Arc<dyn LineageHandle>,
}

impl LineageDeps {
    /// Creates a lineage dependency group.
    #[must_use]
    pub fn new(handle: Arc<dyn LineageHandle>) -> Self {
        Self { handle }
    }

    /// Returns the hot-path lineage capture handle.
    #[must_use]
    pub fn handle(&self) -> Arc<dyn LineageHandle> {
        self.handle.clone()
    }
}

/// Runtime dependencies shared by every Restate handler in this binary.
pub struct OrchestratorDeps {
    /// Persistence dependencies shared by handlers that read or write product data.
    pub persistence: PersistenceDeps,
    /// AuthN/AuthZ dependencies shared by handler boundaries.
    pub auth: AuthDeps,
    /// Runtime cache used for ephemeral coordination state.
    pub runtime_cache: Arc<dyn RuntimeCacheStore>,
    /// LLM and embedding provider dependencies.
    pub providers: ProviderDeps,
    /// Tool-routing dependencies exposed to turn and tool handlers.
    pub tools: ToolDeps,
    /// Memory retrieval dependencies for context compilation and memory APIs.
    pub memory: MemoryDeps,
    /// Lineage capture dependencies.
    pub lineage: LineageDeps,
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
    runtime_cache: Arc<dyn RuntimeCacheStore>,
    providers: ProviderDeps,
    tools: ToolDeps,
    memory: MemoryDeps,
    lineage: LineageDeps,
}

impl OrchestratorCtx {
    /// Creates the process-wide orchestrator context from typed dependency groups.
    #[must_use]
    pub fn new(config: Arc<MoaConfig>, deps: OrchestratorDeps) -> Self {
        Self {
            config,
            persistence: deps.persistence,
            auth: deps.auth,
            runtime_cache: deps.runtime_cache,
            providers: deps.providers,
            tools: deps.tools,
            memory: deps.memory,
            lineage: deps.lineage,
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

    /// Returns the current graph/application Postgres pool.
    #[must_use]
    pub fn current_graph_pool() -> sqlx::PgPool {
        Self::current().graph_pool()
    }

    /// Returns the current graph-memory key-management provider.
    #[must_use]
    pub fn current_kms() -> Arc<dyn KeyManagementProvider> {
        Self::current().kms()
    }

    /// Returns the current provider registry.
    #[must_use]
    pub fn current_provider_registry() -> Arc<ProviderRegistry> {
        Self::current().provider_registry()
    }

    /// Returns the current lineage handle.
    #[must_use]
    pub fn current_lineage() -> Arc<dyn LineageHandle> {
        Self::current().lineage()
    }

    /// Returns the current runtime configuration.
    #[must_use]
    pub fn config(&self) -> Arc<MoaConfig> {
        self.config.clone()
    }

    /// Returns the runtime cache used for ephemeral coordination state.
    #[must_use]
    pub fn runtime_cache(&self) -> Arc<dyn RuntimeCacheStore> {
        self.runtime_cache.clone()
    }

    /// Returns the session store from persistence dependencies.
    #[must_use]
    pub fn session_store(&self) -> Arc<dyn SessionRepository> {
        self.persistence.session_store()
    }

    /// Returns the concrete session-store backend for composition-only surfaces.
    #[must_use]
    pub fn session_store_backend(&self) -> Arc<PostgresSessionStore> {
        self.persistence.session_store_backend()
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

    /// Returns the key-management provider from memory dependencies.
    #[must_use]
    pub fn kms(&self) -> Arc<dyn KeyManagementProvider> {
        self.memory.kms()
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
) -> Result<Option<Identity>, IdentityHeaderError> {
    let get = |name: &'static str| headers.get(name).map(String::as_str);
    let raw_type = get("x-moa-identity-type");
    let raw_id = get("x-moa-identity-id");
    let raw_tenant = get("x-moa-tenant-id");

    let (raw_type, raw_id, raw_tenant) = match (raw_type, raw_id, raw_tenant) {
        (None, None, None) => {
            return Err(IdentityHeaderError::Missing("x-moa-identity-type"));
        }
        (Some(raw_type), Some(raw_id), Some(raw_tenant)) => (raw_type, raw_id, raw_tenant),
        _ => {
            return Err(IdentityHeaderError::Malformed(
                "partial identity headers; require all of type/id/tenant",
            ));
        }
    };

    let identity_type = raw_type.parse().map_err(IdentityHeaderError::UnknownType)?;
    let id = parse_uuid(raw_id, "x-moa-identity-id")?;
    let tenant_id =
        moa_core::types::identifiers::TenantId::from(parse_uuid(raw_tenant, "x-moa-tenant-id")?);
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

/// Adopts and links an inbound W3C trace context on the current span.
///
/// Restate forwards custom request headers verbatim across invocations, so an
/// upstream hop that injected `traceparent` through the identity-header helpers
/// remains causally connected. Restate's endpoint span may already own the local
/// parent edge, so the explicit link preserves the remote hop even when parent
/// adoption cannot replace that edge. A missing or malformed `traceparent` is a
/// no-op.
pub(crate) fn adopt_incoming_trace_parent(ctx: &impl RequestHeaders) {
    let headers = ctx.request_headers();
    let span = tracing::Span::current();
    let _ = moa_observability::adopt_remote_parent(&span, |name| headers.get(name).cloned());
    let _ = moa_observability::propagation::link_remote_context(&span, |name| {
        headers.get(name).cloned()
    });
}

/// Extract identity from a Restate handler context.
pub fn current_identity(
    ctx: &impl RequestHeaders,
) -> Result<Option<Identity>, IdentityHeaderError> {
    let identity = extract_identity(ctx.request_headers())?;
    tracing::debug!(
        has_identity = identity.is_some(),
        "extracted request identity"
    );
    Ok(identity)
}

fn parse_uuid(value: &str, header: &'static str) -> Result<Uuid, IdentityHeaderError> {
    Uuid::parse_str(value).map_err(|_| IdentityHeaderError::Malformed(header))
}
