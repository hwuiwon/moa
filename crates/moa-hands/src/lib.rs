//! Tool routing, local hand provisioning, and built-in tools for MOA.

pub mod adapters;
pub mod core;
pub mod tools;

pub use adapters::daytona::DaytonaHandProvider;
pub use adapters::e2b::E2BHandProvider;
pub use adapters::local::LocalHandProvider;
pub use adapters::mcp::{MCPClient, McpDiscoveredTool};
pub use core::{
    ActionOrigin, HandRoute, PreparedActionInvocation, ToolExecution, ToolRegistry, ToolRouter,
};
