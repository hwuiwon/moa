//! `[auth]` configuration for credential authentication providers.

use serde::{Deserialize, Serialize};

/// Authentication provider configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Selected authentication provider.
    #[serde(default)]
    pub provider: AuthProviderKind,
    /// How strictly internal handlers require trusted identity headers.
    #[serde(default)]
    pub header_trust: AuthHeaderTrustKind,
    /// Local API-key provider settings.
    #[serde(default)]
    pub local: Option<LocalAuthConfig>,
    /// Auth0 provider settings.
    #[serde(default)]
    pub auth0: Option<Auth0AuthConfig>,
    /// Generic OIDC provider settings.
    #[serde(default)]
    pub oidc: Option<OidcAuthConfig>,
    /// MOA-issued contact token settings.
    #[serde(default)]
    pub contact_tokens: ContactTokenConfig,
}

/// Trusted identity header handling mode.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum AuthHeaderTrustKind {
    /// Reject requests that do not include the required identity header set.
    #[default]
    Strict,
    /// Accept requests without identity headers for transitional local wiring.
    Lenient,
}

impl AuthHeaderTrustKind {
    /// Return the serialized configuration value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Supported authentication provider kinds.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
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
        self.into()
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

/// MOA-issued contact JWT settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactTokenConfig {
    /// Expected issuer for contact JWTs.
    pub issuer: String,
    /// Expected audience for contact JWTs.
    pub audience: String,
    /// JWT key id placed in the token header.
    pub key_id: String,
    /// Environment variable containing the RSA private key PEM for issuance.
    pub private_key_pem_env: String,
    /// Environment variable containing the RSA public key PEM for verification.
    pub public_key_pem_env: String,
    /// Environment variable containing the 32-byte hex key used for contact point lookup hashes.
    pub contact_point_hash_key_env: String,
    /// TTL for unverified contact tokens.
    pub unverified_ttl_seconds: i64,
    /// TTL for verified contact tokens.
    pub verified_ttl_seconds: i64,
    /// TTL for one-time verification challenges.
    pub verification_ttl_seconds: i64,
}

impl Default for ContactTokenConfig {
    fn default() -> Self {
        Self {
            issuer: "https://moa.local/contacts".to_string(),
            audience: "moa-agent-contact".to_string(),
            key_id: "moa-contact-rs256".to_string(),
            private_key_pem_env: "MOA_CONTACT_JWT_PRIVATE_KEY_PEM".to_string(),
            public_key_pem_env: "MOA_CONTACT_JWT_PUBLIC_KEY_PEM".to_string(),
            contact_point_hash_key_env: "MOA_CONTACT_POINT_HASH_KEY_HEX".to_string(),
            unverified_ttl_seconds: 3600,
            verified_ttl_seconds: 86_400,
            verification_ttl_seconds: 600,
        }
    }
}
