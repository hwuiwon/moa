//! Shared runtime context for the Restate-backed orchestrator binary.

use std::sync::{Arc, OnceLock};

use moa_authz::FgaClient;
use moa_brain::pipeline::memory::GraphMemoryRetriever;
use moa_core::{
    LineageHandle, MoaConfig,
    traits::{EmbeddingProvider, Identity, IdentityType},
};
use moa_hands::ToolRouter;
use moa_session::PostgresSessionStore;
use serde_json::Value;
use uuid::Uuid;

use crate::services::llm_gateway::ProviderRegistry;

static CTX: OnceLock<Arc<OrchestratorCtx>> = OnceLock::new();

/// Runtime dependencies shared by every Restate handler in this binary.
///
/// Constructed once at startup from `main.rs` and installed via
/// [`OrchestratorCtx::install`]. Handlers read the current instance via
/// [`OrchestratorCtx::current`].
pub struct OrchestratorCtx {
    /// Shared orchestrator configuration.
    pub config: Arc<MoaConfig>,
    /// Session store used by Restate handlers.
    pub session_store: Arc<PostgresSessionStore>,
    /// Postgres pool used by graph-memory retrieval and ingestion paths.
    pub graph_pool: sqlx::PgPool,
    /// OpenFGA client used by handler authorization checks.
    pub fga_client: Option<FgaClient>,
    /// Authentication, token-vault, and async-approval providers.
    pub auth_providers: moa_auth_providers::Providers,
    /// Registry of configured LLM providers.
    pub providers: Arc<ProviderRegistry>,
    /// Optional embedding provider shared by embedding-dependent runtime helpers.
    pub embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// Tool router used by Restate services.
    pub tool_router: Arc<ToolRouter>,
    /// Precompiled tool schemas exposed to the model.
    pub tool_schemas: Arc<Vec<Value>>,
    /// Process-wide graph-memory retriever reused by Restate turn pipelines.
    pub graph_memory_retriever: Arc<GraphMemoryRetriever>,
    /// Hot-path lineage capture bridge selected at startup.
    pub lineage: Arc<dyn LineageHandle>,
    /// Optional durable lineage writer used for graceful shutdown.
    pub lineage_writer: Option<Arc<moa_lineage_sink::WriterHandle>>,
}

impl OrchestratorCtx {
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
    let tenant_id = parse_uuid(raw_tenant, "x-moa-tenant-id")?;
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
        "agent" => Ok(IdentityType::Agent),
        "service" => Ok(IdentityType::Service),
        other => Err(IdentityHeaderError::UnknownType(other.to_string())),
    }
}

fn parse_uuid(value: &str, header: &'static str) -> Result<Uuid, IdentityHeaderError> {
    Uuid::parse_str(value).map_err(|_| IdentityHeaderError::Malformed(header))
}
