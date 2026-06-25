//! Tool routing core, registry, policy, dispatch, and recovery for MOA hands.

mod construction;
mod dispatch;
mod lifecycle;
mod normalization;
mod output_budget;
mod policy;
mod recovery;
mod registration;
mod telemetry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use moa_core::{
    HandHandle, HandProvider, McpServerConfig, MemoryToolExecutor, SandboxFile, SessionId,
    SessionStore, TenantId, ToolBudgetConfig, ToolOutputConfig,
};
use moa_security::{ActionPolicies, ActionPolicyRuleStore, MCPCredentialProxy};
use tokio::sync::RwLock;

use crate::adapters::local::LocalHandProvider;
use crate::adapters::mcp::MCPClient;

pub use policy::{ActionOrigin, PreparedActionInvocation};
pub use registration::{ToolExecution, ToolRegistry};

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// Routes tool invocations to built-ins, local hands, or MCP backends.
pub struct ToolRouter {
    registry: ToolRegistry,
    providers: HashMap<String, Arc<dyn HandProvider>>,
    local_provider: Option<Arc<LocalHandProvider>>,
    mcp_clients: RwLock<HashMap<String, Arc<MCPClient>>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    mcp_proxy: Option<Arc<MCPCredentialProxy>>,
    active_hands: RwLock<HashMap<String, HandHandle>>,
    trusted_sandbox_files: RwLock<HashMap<SessionId, Vec<SandboxFile>>>,
    installed_files: RwLock<HashMap<String, Vec<SandboxFile>>>,
    workspace_roots: RwLock<HashMap<TenantId, PathBuf>>,
    policies: ActionPolicies,
    rule_store: Option<Arc<dyn ActionPolicyRuleStore>>,
    session_store: Option<Arc<dyn SessionStore>>,
    memory_tool_executor: RwLock<Option<Arc<dyn MemoryToolExecutor>>>,
    lineage: Arc<dyn moa_core::LineageHandle>,
    sandbox_root: Option<PathBuf>,
    tool_output: ToolOutputConfig,
    tool_budgets: ToolBudgetConfig,
}
