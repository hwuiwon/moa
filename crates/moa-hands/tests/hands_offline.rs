//! Consolidated offline moa-hands integration tests (one harness binary per lane).

#[path = "hands_offline/local_tools_offline.rs"]
mod local_tools_offline;
#[path = "hands_offline/mcp_router.rs"]
mod mcp_router;
#[path = "hands_offline/security_defaults.rs"]
mod security_defaults;
