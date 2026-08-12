//! Consolidated offline moa-hands integration tests (one harness binary per lane).

#[path = "hands_offline/call_origin_offline.rs"]
mod call_origin_offline;
#[path = "hands_offline/connector_router.rs"]
mod connector_router;
#[path = "hands_offline/local_tools_offline.rs"]
mod local_tools_offline;
#[path = "hands_offline/maintenance_provider_inventory_offline.rs"]
mod maintenance_provider_inventory_offline;
#[path = "hands_offline/mcp_router.rs"]
mod mcp_router;
#[path = "hands_offline/provider_credentials_offline.rs"]
mod provider_credentials_offline;
#[path = "hands_offline/sandbox_profile_offline.rs"]
mod sandbox_profile_offline;
#[path = "hands_offline/sandbox_workspace_recovery_offline.rs"]
mod sandbox_workspace_recovery_offline;
#[path = "hands_offline/security_defaults.rs"]
mod security_defaults;
#[path = "hands_offline/tool_output_security_offline.rs"]
mod tool_output_security_offline;
