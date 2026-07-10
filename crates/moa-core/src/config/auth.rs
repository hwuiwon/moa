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
    /// Shared secret used to verify Auth0 connection-linked webhooks.
    #[serde(default)]
    pub auth0_webhook_secret: Option<String>,
    /// Generic OIDC provider settings.
    #[serde(default)]
    pub oidc: Option<OidcAuthConfig>,
    /// MOA-issued contact token settings.
    #[serde(default)]
    pub contact_tokens: ContactTokenConfig,
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
    /// Auth0 client id loaded from runtime configuration.
    pub client_id: String,
    /// Auth0 client secret loaded from runtime configuration.
    pub client_secret: String,
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
    /// RSA private key PEM for issuance.
    pub private_key_pem: String,
    /// RSA public key PEM for verification.
    pub public_key_pem: String,
    /// 32-byte hex key used for contact point lookup hashes.
    pub contact_point_hash_key_hex: String,
    /// TTL for unverified contact tokens.
    pub unverified_ttl_seconds: i64,
    /// TTL for verified contact tokens, in seconds.
    ///
    /// Contact-token verification is stateless (no jti denylist), so this TTL is
    /// the effective revocation window: a leaked or revoked token remains usable
    /// until it expires. Keep it short. A jti denylist would decouple revocation
    /// from the TTL (follow-up, not implemented).
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
            private_key_pem: String::new(),
            public_key_pem: String::new(),
            contact_point_hash_key_hex: String::new(),
            unverified_ttl_seconds: 3600,
            // 2h: the TTL bounds the stateless revocation window (see field doc).
            verified_ttl_seconds: 7_200,
            verification_ttl_seconds: 600,
        }
    }
}
