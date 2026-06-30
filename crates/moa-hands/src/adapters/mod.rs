//! Sandbox adapter implementations for local, Daytona, E2B, and MCP backends.

#[cfg(feature = "daytona")]
pub mod daytona;
#[cfg(feature = "e2b")]
pub mod e2b;
#[cfg(any(feature = "daytona", feature = "e2b"))]
pub(crate) mod http_util;
pub mod local;
pub mod mcp;
