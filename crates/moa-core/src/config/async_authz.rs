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

impl super::MoaEnvOverlay {
    /// Applies async authorization environment overrides.
    pub(in crate::config) fn apply_async_authz_overlay(&self, config: &mut super::MoaConfig) {
        use super::env_overlay::set_copy_if_some;

        set_copy_if_some(&mut config.async_authz.provider, self.async_authz_provider);
        set_copy_if_some(
            &mut config.async_authz.default_timeout_secs,
            self.async_authz_default_timeout_secs,
        );
    }
}
