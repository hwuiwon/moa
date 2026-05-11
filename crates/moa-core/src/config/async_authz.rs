//! `[async_authz]` configuration for human-in-the-loop approvals.

use serde::{Deserialize, Serialize};

/// Async authorization provider configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsyncAuthzConfig {
    /// Selected async authorization provider.
    #[serde(default)]
    pub provider: AsyncAuthzKind,
    /// Default approval timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub default_timeout_secs: u64,
}

impl Default for AsyncAuthzConfig {
    fn default() -> Self {
        Self {
            provider: AsyncAuthzKind::Builtin,
            default_timeout_secs: default_timeout_secs(),
        }
    }
}

/// Supported async authorization provider kinds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsyncAuthzKind {
    /// Builtin Postgres + Restate awakeable approvals.
    #[default]
    Builtin,
    /// Auth0 CIBA-backed approvals.
    Auth0,
}

impl AsyncAuthzKind {
    /// Return the serialized configuration value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Auth0 => "auth0",
        }
    }
}

fn default_timeout_secs() -> u64 {
    900
}
