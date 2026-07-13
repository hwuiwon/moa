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
    /// Default tenant-admin action-review timeout in seconds.
    ///
    /// A pending `tenant_action_reviews` row that is not decided within this
    /// window is transitioned to `timeout` by the action-review reaper and
    /// fails closed: the gated tool never executes.
    #[serde(default = "default_action_review_timeout_secs")]
    pub action_review_timeout_secs: u64,
}

impl Default for AsyncAuthzConfig {
    fn default() -> Self {
        Self {
            provider: AsyncAuthzKind::Builtin,
            default_timeout_secs: default_timeout_secs(),
            action_review_timeout_secs: default_action_review_timeout_secs(),
        }
    }
}

/// Supported async authorization provider kinds.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
        self.into()
    }
}

fn default_timeout_secs() -> u64 {
    900
}

fn default_action_review_timeout_secs() -> u64 {
    // Tenant-admin reviews are human-scale and asynchronous, so the default
    // window is far longer than the interactive builtin approval timeout: a day
    // to decide before the gated action fails closed.
    86_400
}
