//! Shared runtime context for the Restate-backed orchestrator binary.

use std::sync::{Arc, OnceLock};

use moa_core::{MemoryStore, MoaConfig};
use moa_hands::ToolRouter;
use moa_session::PostgresSessionStore;
use serde_json::Value;

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
    /// Memory store used during prompt compilation.
    pub memory_store: Arc<dyn MemoryStore>,
    /// Registry of configured LLM providers.
    pub providers: Arc<ProviderRegistry>,
    /// Tool router used by Restate services.
    pub tool_router: Arc<ToolRouter>,
    /// Precompiled tool schemas exposed to the model.
    pub tool_schemas: Arc<Vec<Value>>,
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
