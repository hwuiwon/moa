//! Tool routing core, registry, policy, dispatch, and recovery for MOA hands.

mod construction;
mod dispatch;
pub mod leases;
mod lifecycle;
pub mod mcp_catalog;
pub mod mcp_connections;
mod normalization;
mod output_budget;
mod policy;
pub mod profile;
pub mod reaper;
mod recovery;
mod registration;
mod telemetry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use moa_config::McpServerConfig;
use moa_config::ToolBudgetConfig;
use moa_config::ToolOutputConfig;
use moa_core::{
    traits::HandProvider, traits::MemoryRetrievalExecutor, traits::MemoryToolExecutor,
    traits::SessionStore, types::hands::HandHandle, types::hands::SandboxFile,
    types::hands::SandboxPolicySnapshot, types::identifiers::TenantId,
};
use moa_security::{
    ActionPolicies, ActionPolicyRuleStore, MCPCredentialProxy, McpEgressGuard,
    UnmatchedPermissionPattern,
};
use tokio::sync::RwLock;

use crate::adapters::local::LocalHandProvider;
use crate::adapters::mcp::MCPClient;

use leases::HandLeaseStore;
pub use mcp_catalog::{McpCatalogRefresh, McpConnectorHealth, spawn_mcp_catalog_refresh};
pub use mcp_connections::{
    PostgresTenantMcpConnectionBindings, TenantMcpAuthorizer, TenantMcpBindingStatus,
    TenantMcpConnectionBinding, TenantMcpConnectionBindingStore, TenantMcpCredentialOwners,
    ToolCredentialScope,
};
pub use policy::{ActionOrigin, PreparedActionInvocation};
pub use profile::TenantSandboxPolicyStore;
pub use profile::{
    PostgresTenantSandboxPolicyStore, deployment_sandbox_policy, local_development_sandbox_policy,
    route_sandbox_policy,
};
pub use reaper::{HandLeaseReaper, HandLeaseReaperConfig, PostgresExpiredHandLeaseClaims};
pub use registration::{
    HandRoute, MCP_TOOL_REFERENCE_PREFIX, ToolExecution, ToolRegistry, mcp_tool_reference,
};

const DEFAULT_PROVIDER_NAME: &str = "local";
const DEFAULT_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// One immutable publication of the executable registry and its prompt schemas.
struct ToolCatalogSnapshot {
    registry: Arc<ToolRegistry>,
    tool_schemas: Arc<Vec<serde_json::Value>>,
}

impl ToolCatalogSnapshot {
    fn new(registry: ToolRegistry) -> Self {
        let tool_schemas = Arc::new(registry.default_tool_schemas());
        Self {
            registry: Arc::new(registry),
            tool_schemas,
        }
    }
}

/// Routes tool invocations to built-ins, local hands, or MCP backends.
pub struct ToolRouter {
    /// Live executable registry and prompt schemas, published as one snapshot.
    ///
    /// Held as an immutable `Arc` behind a lock rather than as a mutable
    /// registry so a background catalog refresh publishes atomically: every
    /// reader takes one snapshot and works from it, and no prompt compilation,
    /// capability listing, or dispatch can observe a half-refreshed connector.
    catalog: std::sync::RwLock<Arc<ToolCatalogSnapshot>>,
    providers: HashMap<String, Arc<dyn HandProvider>>,
    local_provider: Option<Arc<LocalHandProvider>>,
    mcp_clients: RwLock<HashMap<String, Arc<MCPClient>>>,
    mcp_servers: HashMap<String, McpServerConfig>,
    /// Last observed discovery outcome for every configured connector.
    ///
    /// This is the typed health the acceptance criteria require: an optional
    /// connector that is down is `Unavailable` (or `Degraded` while its
    /// last-known-good tools stay served) and every other connector's tools are
    /// unaffected, while a required connector that is down never reaches this
    /// map because startup fails with the same typed value.
    mcp_health: RwLock<std::collections::BTreeMap<String, McpConnectorHealth>>,
    mcp_proxy: Option<Arc<MCPCredentialProxy>>,
    /// Owners required to serve tenant-owned MCP servers: the durable credential
    /// vault behind [`MCPCredentialProxy`], the connection binding owner, and the
    /// delegated tenant-operator authorizer. `None` is valid only for a
    /// deployment that configures no tenant-owned MCP server — construction
    /// rejects the combination, and dispatch fails closed rather than falling
    /// back to the deployment credential.
    tenant_mcp: Option<Arc<TenantMcpCredentialOwners>>,
    /// Optional data-class egress guard for outbound MCP tool calls. When
    /// present, each call's serialized arguments are classified against the
    /// destination server's `allowed_data_classes` allowlist and blocked (fail
    /// closed) before dispatch when the payload carries a class the server is not
    /// permitted to receive. Absence is valid only when no MCP servers are
    /// configured: configured construction rejects it, and manually assembled
    /// routers fail closed at dispatch. The guard is held here rather than on
    /// [`MCPCredentialProxy`] so it governs every external MCP server, including
    /// credential-less ones for which no proxy is built.
    mcp_egress_guard: Option<Arc<McpEgressGuard>>,
    active_hands: RwLock<HashMap<String, HandHandle>>,
    preferred_hand_routes: RwLock<HashMap<String, String>>,
    hand_leases: Option<Arc<dyn HandLeaseStore>>,
    /// Deployment-level sandbox policy layer, injected at construction so
    /// provisioning can never substitute a default for the outermost layer.
    deployment_sandbox_policy: SandboxPolicySnapshot,
    /// Durable owner of each tenant's authored sandbox policy layer, read on
    /// every provisioning decision. `None` means no tenant has authored one,
    /// which contributes the named identity layer rather than an absent one.
    tenant_sandbox_policy: Option<Arc<dyn TenantSandboxPolicyStore>>,
    /// Whether the durable hand-lease reaper is running for this deployment.
    ///
    /// A provider that relies on the reaper to enforce a deadline may only
    /// serve a bounded deadline when this is true; otherwise admission refuses
    /// rather than provisioning a sandbox nothing will ever destroy.
    hand_lease_reaper_installed: bool,
    /// Trusted sandbox file manifests keyed by hand scope (`scope_key`):
    /// `"{session_id}:{worker_id}"`, where an empty worker segment is the
    /// session-level (coordinator) scope.
    trusted_sandbox_files: RwLock<HashMap<String, Vec<SandboxFile>>>,
    installed_files: RwLock<HashMap<String, Vec<SandboxFile>>>,
    workspace_roots: RwLock<HashMap<TenantId, PathBuf>>,
    policies: ActionPolicies,
    /// Configured permission patterns that govern no registered tool.
    ///
    /// Recomputed whenever the tool catalog changes rather than once at startup,
    /// so a lazily discovered connector clears a warning that was legitimately
    /// true before its tools existed.
    unmatched_permission_patterns: std::sync::RwLock<Vec<UnmatchedPermissionPattern>>,
    rule_store: Option<Arc<dyn ActionPolicyRuleStore>>,
    session_store: Option<Arc<dyn SessionStore>>,
    memory_tool_executor: RwLock<Option<Arc<dyn MemoryToolExecutor>>>,
    memory_retrieval_executor: RwLock<Option<Arc<dyn MemoryRetrievalExecutor>>>,
    lineage: Arc<dyn moa_core::traits::LineageHandle>,
    sandbox_root: Option<PathBuf>,
    tool_output: ToolOutputConfig,
    tool_budgets: ToolBudgetConfig,
}
