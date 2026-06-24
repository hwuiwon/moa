//! Shared tool-name routing for sandbox adapters.

#[cfg(any(feature = "daytona", feature = "e2b"))]
use moa_core::MoaError;

/// Tool routes supported by local filesystem-backed sandboxes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SandboxToolRoute {
    /// Run a shell command.
    Bash,
    /// Search file contents.
    Grep,
    /// Return a structured file outline.
    FileOutline,
    /// Read a file.
    FileRead,
    /// Replace text in a file.
    StrReplace,
    /// Write a file.
    FileWrite,
    /// Search file names.
    FileSearch,
}

impl SandboxToolRoute {
    /// Parses a registered sandbox tool name.
    pub(super) fn from_name(tool: &str) -> Option<Self> {
        match tool {
            "bash" => Some(Self::Bash),
            "grep" => Some(Self::Grep),
            "file_outline" => Some(Self::FileOutline),
            "file_read" => Some(Self::FileRead),
            "str_replace" => Some(Self::StrReplace),
            "file_write" => Some(Self::FileWrite),
            "file_search" => Some(Self::FileSearch),
            _ => None,
        }
    }
}

/// Tool routes supported by HTTP-backed cloud sandboxes.
#[cfg(any(feature = "daytona", feature = "e2b", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloudSandboxToolRoute {
    /// Run a shell command.
    Bash,
    /// Read a file.
    FileRead,
    /// Replace text in a file.
    StrReplace,
    /// Write a file.
    FileWrite,
    /// Search file names.
    FileSearch,
}

#[cfg(any(feature = "daytona", feature = "e2b", test))]
impl CloudSandboxToolRoute {
    /// Parses a cloud sandbox tool name.
    pub(super) fn from_name(tool: &str) -> Option<Self> {
        match SandboxToolRoute::from_name(tool)? {
            SandboxToolRoute::Bash => Some(Self::Bash),
            SandboxToolRoute::FileRead => Some(Self::FileRead),
            SandboxToolRoute::StrReplace => Some(Self::StrReplace),
            SandboxToolRoute::FileWrite => Some(Self::FileWrite),
            SandboxToolRoute::FileSearch => Some(Self::FileSearch),
            SandboxToolRoute::Grep | SandboxToolRoute::FileOutline => None,
        }
    }
}

/// Builds a provider-specific unsupported-tool error.
#[cfg(any(feature = "daytona", feature = "e2b"))]
pub(super) fn unsupported_tool(provider: &str, tool: &str) -> MoaError {
    MoaError::ToolError(format!("unsupported {provider} tool: {tool}"))
}

#[cfg(test)]
mod tests {
    use super::{CloudSandboxToolRoute, SandboxToolRoute};

    #[test]
    fn sandbox_route_parses_registered_tool_names() {
        // Pins: adapter routing recognizes every registered sandbox tool exactly once.
        assert_eq!(
            SandboxToolRoute::from_name("bash"),
            Some(SandboxToolRoute::Bash)
        );
        assert_eq!(
            SandboxToolRoute::from_name("file_search"),
            Some(SandboxToolRoute::FileSearch)
        );
        assert_eq!(SandboxToolRoute::from_name("unknown"), None);
    }

    #[test]
    fn cloud_route_excludes_host_only_tools() {
        // Pins: cloud HTTP adapters do not advertise local-only grep or file_outline routes.
        assert_eq!(
            CloudSandboxToolRoute::from_name("file_read"),
            Some(CloudSandboxToolRoute::FileRead)
        );
        assert_eq!(CloudSandboxToolRoute::from_name("grep"), None);
        assert_eq!(CloudSandboxToolRoute::from_name("file_outline"), None);
    }
}
