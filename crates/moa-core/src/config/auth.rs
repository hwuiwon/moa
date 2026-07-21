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
    /// First-party OAuth 2.1 Authorization Server settings.
    #[serde(default)]
    pub oauth: OAuthServerConfig,
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

/// First-party OAuth 2.1 Authorization Server settings.
///
/// When [`OAuthServerConfig::clients`] is empty the Authorization Server has no
/// registered client, so every `/oauth/*` request fails closed on client lookup.
/// Configured clients are validated and converged into Postgres at startup;
/// request-time lookup always uses that authoritative table. Dynamic client
/// registration (RFC 7591) is a follow-up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthServerConfig {
    /// Canonical authorization-server issuer URL.
    pub issuer: String,
    /// Exact RFC 8707 protected resource accepted by this server.
    pub resource: String,
    /// Lifetime of an unapproved authorization transaction, in seconds.
    pub authorization_request_ttl_seconds: i64,
    /// Lifetime of a single-use authorization code, in seconds. Keep short.
    pub authorization_code_ttl_seconds: i64,
    /// Lifetime of an issued access token, in seconds.
    pub access_token_ttl_seconds: i64,
    /// Lifetime of an issued refresh token, in seconds.
    pub refresh_token_ttl_seconds: i64,
    /// Statically configured OAuth clients.
    #[serde(default)]
    pub clients: Vec<OAuthClientConfig>,
}

impl Default for OAuthServerConfig {
    fn default() -> Self {
        Self {
            issuer: "https://moa.local".to_string(),
            resource: "https://moa.local/mcp".to_string(),
            authorization_request_ttl_seconds: 300,
            // 60s: an authorization code is redeemed immediately after the
            // browser redirect, so the exposure window stays minimal.
            authorization_code_ttl_seconds: 60,
            access_token_ttl_seconds: 3600,
            // 14 days: refresh tokens rotate on every use, so a leaked token is
            // detectable and bounded by the rotation window.
            refresh_token_ttl_seconds: 1_209_600,
            clients: Vec::new(),
        }
    }
}

impl OAuthServerConfig {
    /// Validate the authorization-server deployment contract.
    pub fn validate(&self) -> Result<(), String> {
        validate_oauth_url("issuer", &self.issuer, "/")?;
        validate_oauth_url("resource", &self.resource, "/mcp")?;
        if self.authorization_request_ttl_seconds <= 0
            || self.authorization_code_ttl_seconds <= 0
            || self.access_token_ttl_seconds <= 0
            || self.refresh_token_ttl_seconds <= 0
        {
            return Err("OAuth lifetimes must be positive".to_string());
        }
        Ok(())
    }
}

fn validate_oauth_url(field: &str, value: &str, expected_path: &str) -> Result<(), String> {
    let parsed =
        url::Url::parse(value).map_err(|error| format!("invalid OAuth {field}: {error}"))?;
    if parsed.cannot_be_a_base()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != expected_path
    {
        return Err(format!("invalid OAuth {field} URL"));
    }
    Ok(())
}

/// One statically registered OAuth client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthClientConfig {
    /// Public client identifier presented on `/oauth/*` requests.
    pub client_id: String,
    /// Whether the client authenticates with a secret (`confidential`) or relies
    /// solely on PKCE (`public`).
    #[serde(default)]
    pub client_type: OAuthClientType,
    /// Exact redirect URIs the client may use; matched byte-for-byte.
    pub redirect_uris: Vec<String>,
    /// Scopes the client may request. An empty list allows no scopes.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Lowercase SHA-256 hex digest of the client secret, required for
    /// `confidential` clients and ignored for `public` clients. The plaintext
    /// secret is never stored.
    #[serde(default)]
    pub client_secret_sha256: Option<String>,
}

/// OAuth client authentication class.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum OAuthClientType {
    /// PKCE-only client with no secret (e.g. native or single-page apps).
    #[default]
    Public,
    /// Client that authenticates with a shared secret.
    Confidential,
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
