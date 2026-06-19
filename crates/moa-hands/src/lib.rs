//! Tool routing, local hand provisioning, and built-in tools for MOA.

pub mod adapters;
pub mod core;
pub mod tools;

#[cfg(feature = "daytona")]
pub use adapters::daytona::DaytonaHandProvider;
#[cfg(feature = "e2b")]
pub use adapters::e2b::E2BHandProvider;
pub use adapters::local::LocalHandProvider;
pub use adapters::mcp::{MCPClient, McpDiscoveredTool};
pub use core::{ActionOrigin, PreparedActionInvocation, ToolExecution, ToolRegistry, ToolRouter};
