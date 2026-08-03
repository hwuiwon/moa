//! Tool routing, local hand provisioning, and built-in tools for MOA.

pub mod adapters;
pub mod core;
pub mod tools;

pub use adapters::daytona::{DAYTONA_CAPABILITIES, DaytonaHandProvider};
pub use adapters::e2b::{E2B_CAPABILITIES, E2BHandProvider};
pub use adapters::local::{LOCAL_HAND_CAPABILITIES, LocalHandProvider};
pub use adapters::mcp::{MCPClient, McpDiscoveredTool};
pub use core::{
    ActionOrigin, CandidateConnector, CatalogDefect, HandLeaseReaper, HandLeaseReaperConfig,
    HandRoute, MCP_TOOL_REFERENCE_PREFIX, McpCatalogActivation, McpCatalogRefresh,
    McpConnectorHealth, PendingConnectorToolOutput, PinnedToolContract, PinnedToolOwner,
    PostgresExpiredHandLeaseClaims, PostgresTenantSandboxPolicyStore, PreparedActionInvocation,
    TenantSandboxPolicyStore, ToolCallScope, ToolCatalogDrift, ToolCatalogPin, ToolCatalogSnapshot,
    ToolExecution, ToolRegistry, ToolRouter, deployment_sandbox_policy,
    governed_tool_contract_revision, local_development_sandbox_policy, mcp_tool_reference,
    route_sandbox_policy, spawn_mcp_catalog_refresh,
};
