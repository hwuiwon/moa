//! Tool routing, local hand provisioning, and built-in tools for MOA.

#[cfg(feature = "daytona")]
pub mod daytona;
#[cfg(feature = "e2b")]
pub mod e2b;
pub mod local;
pub mod mcp;
pub mod router;
pub mod tools;

#[cfg(feature = "daytona")]
pub use daytona::DaytonaHandProvider;
#[cfg(feature = "e2b")]
pub use e2b::E2BHandProvider;
pub use local::LocalHandProvider;
pub use mcp::{MCPClient, McpDiscoveredTool};
pub use router::{
    BuiltInTool, ToolContext, ToolDefinition, ToolExecution, ToolRegistry, ToolRouter,
};
