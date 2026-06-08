//! `[auth]` configuration for credential authentication providers.

use serde::{Deserialize, Serialize};

/// Authentication provider configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Selected authentication provider.
    #[serde(default)]
    pub provider: AuthProviderKind,
    /// Local API-key provider settings.
    #[serde(default)]
    pub local: Option<LocalAuthConfig>,
    /// Auth0 provider settings.
    #[serde(default)]
    pub auth0: Option<Auth0AuthConfig>,
    /// Generic OIDC provider settings.
    #[serde(default)]
    pub oidc: Option<OidcAuthConfig>,
}

/// Supported authentication provider kinds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthProviderKind {
    /// Local API-key authentication.
    #[default]
    Local,
    /// Disable credential checks and assign a fixed service identity.
    #[serde(alias = "none")]
    Disabled,
    /// Auth0-backed authentication.
    Auth0,
    /// Generic OIDC authentication.
    Oidc,
}

impl AuthProviderKind {
    /// Return the serialized configuration value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Disabled => "disabled",
            Self::Auth0 => "auth0",
            Self::Oidc => "oidc",
        }
    }
}

/// Local API-key authentication settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalAuthConfig {}

/// Auth0 authentication settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Auth0AuthConfig {
    /// Auth0 tenant domain.
    pub domain: String,
    /// Expected API audience.
    pub audience: String,
    /// Environment variable holding the Auth0 client id.
    pub client_id_env: String,
    /// Environment variable holding the Auth0 client secret.
    pub client_secret_env: String,
}

/// Generic OIDC authentication settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcAuthConfig {
    /// OIDC issuer URL.
    pub issuer: String,
    /// Expected token audience.
    pub audience: String,
    /// JWKS endpoint URL.
    pub jwks_url: String,
}
