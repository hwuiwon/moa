//! `[token_vault]` configuration for third-party token retrieval.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use url::Url;

use moa_core::error::{MoaError, Result};

/// Token vault provider configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenVaultConfig {
    /// Selected token vault provider.
    #[serde(default)]
    pub provider: TokenVaultKind,
    /// Outbound OAuth refresh settings for the self-hosted Postgres vault, keyed
    /// by connection name. When a connection has an entry here, the vault
    /// refreshes its expired access tokens via the `refresh_token` grant instead
    /// of surfacing them as unavailable. Empty (the default) preserves the
    /// no-refresh behavior. Only the self-hosted vault consults this; the Auth0
    /// vault refreshes externally.
    #[serde(default)]
    pub refresh: BTreeMap<String, OAuthRefreshConfig>,
}

/// Outbound OAuth token-endpoint settings used to refresh one connection's
/// expired access token via the `refresh_token` grant (RFC 6749 §6).
///
/// Deployments should supply this map through `MOA_TOKEN_VAULT_REFRESH_JSON` so
/// each replica receives the same complete refresh configuration at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthRefreshConfig {
    /// Provider token endpoint that accepts the `refresh_token` grant, for
    /// example `https://oauth2.googleapis.com/token`.
    pub token_endpoint: String,
    /// OAuth client id registered for this connection.
    pub client_id: String,
    /// OAuth client secret. Absent for public clients that refresh without a
    /// secret.
    #[serde(default)]
    pub client_secret: Option<String>,
}

impl TokenVaultConfig {
    /// Validates the configured OAuth refresh clients.
    pub fn validate(&self) -> Result<()> {
        for (connection, refresh) in &self.refresh {
            if connection.trim().is_empty() {
                return Err(MoaError::ConfigError(
                    "token_vault.refresh connection names must be non-empty".to_string(),
                ));
            }
            if refresh.client_id.trim().is_empty() {
                return Err(MoaError::ConfigError(format!(
                    "token_vault.refresh.{connection}.client_id must be non-empty"
                )));
            }
            if refresh
                .client_secret
                .as_ref()
                .is_some_and(|secret| secret.trim().is_empty())
            {
                return Err(MoaError::ConfigError(format!(
                    "token_vault.refresh.{connection}.client_secret must be non-empty when set"
                )));
            }

            let endpoint = Url::parse(refresh.token_endpoint.trim()).map_err(|error| {
                MoaError::ConfigError(format!(
                    "token_vault.refresh.{connection}.token_endpoint must be a valid URL: {error}"
                ))
            })?;
            if !matches!(endpoint.scheme(), "http" | "https") || endpoint.host_str().is_none() {
                return Err(MoaError::ConfigError(format!(
                    "token_vault.refresh.{connection}.token_endpoint must be an absolute http or https URL"
                )));
            }
        }
        Ok(())
    }
}

/// Supported token vault provider kinds.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum TokenVaultKind {
    /// No token vault configured.
    #[default]
    None,
    /// Self-hosted, Postgres-backed token vault.
    Postgres,
    /// Auth0 Token Vault.
    Auth0,
}

impl TokenVaultKind {
    /// Return the serialized configuration value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}
