//! Shared tool-name routing for sandbox adapters.

#[cfg(any(feature = "daytona", feature = "e2b"))]
use moa_core::MoaError;

/// Tool routes supported by registered hand-backed sandboxes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SandboxToolRoute {
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
    /// All registered hand-tool routes in stable route order.
    pub(crate) const ALL: [Self; 7] = [
        Self::Bash,
        Self::Grep,
        Self::FileOutline,
        Self::FileRead,
        Self::StrReplace,
        Self::FileWrite,
        Self::FileSearch,
    ];

    /// Prompt-facing default loadout order for registered hand tools.
    pub(crate) const DEFAULT_LOADOUT: [Self; 7] = [
        Self::FileSearch,
        Self::Grep,
        Self::FileOutline,
        Self::FileRead,
        Self::StrReplace,
        Self::FileWrite,
        Self::Bash,
    ];

    /// Returns the stable registered tool name for this route.
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Grep => "grep",
            Self::FileOutline => "file_outline",
            Self::FileRead => "file_read",
            Self::StrReplace => "str_replace",
            Self::FileWrite => "file_write",
            Self::FileSearch => "file_search",
        }
    }

    /// Parses a registered sandbox tool name.
    pub(crate) fn from_name(tool: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|route| route.name() == tool)
    }
}

/// Builds a provider-specific unsupported-tool error.
#[cfg(any(feature = "daytona", feature = "e2b"))]
pub(super) fn unsupported_tool(provider: &str, tool: &str) -> MoaError {
    MoaError::ToolError(format!("unsupported {provider} tool: {tool}"))
}

#[cfg(test)]
mod tests {
    use super::SandboxToolRoute;

    #[test]
    fn sandbox_route_parses_all_registered_tool_names() {
        // Pins: adapter routing recognizes every registered sandbox tool exactly once.
        let parsed = SandboxToolRoute::ALL
            .into_iter()
            .map(|route| SandboxToolRoute::from_name(route.name()))
            .collect::<Vec<_>>();
        assert_eq!(
            parsed,
            SandboxToolRoute::ALL
                .into_iter()
                .map(Some)
                .collect::<Vec<_>>()
        );
        assert_eq!(SandboxToolRoute::from_name("unknown"), None);
    }

    #[test]
    fn default_loadout_keeps_prompt_order() {
        // Pins: hand-tool prompt order stays stable while routes remain a single source of truth.
        assert_eq!(
            SandboxToolRoute::DEFAULT_LOADOUT.map(SandboxToolRoute::name),
            [
                "file_search",
                "grep",
                "file_outline",
                "file_read",
                "str_replace",
                "file_write",
                "bash",
            ]
        );
    }
}
