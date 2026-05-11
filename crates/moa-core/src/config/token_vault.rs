//! `[token_vault]` configuration for third-party token retrieval.

use serde::{Deserialize, Serialize};

/// Token vault provider configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenVaultConfig {
    /// Selected token vault provider.
    #[serde(default)]
    pub provider: TokenVaultKind,
}

/// Supported token vault provider kinds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenVaultKind {
    /// No token vault configured.
    #[default]
    None,
    /// Auth0 Token Vault.
    Auth0,
}

impl TokenVaultKind {
    /// Return the serialized configuration value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Auth0 => "auth0",
        }
    }
}
