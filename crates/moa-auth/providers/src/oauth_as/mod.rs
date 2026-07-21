//! First-party OAuth 2.1 Authorization Server core.
//!
//! MOA issues its own access and refresh tokens for the canonical MCP protected
//! resource via authorization code + PKCE, without depending on an external
//! identity provider. This module owns the provider-agnostic core:
//!
//! * [`pkce`] — S256 `code_challenge` verification (RFC 7636).
//! * [`client`] — startup client validation and client authentication.
//! * [`store`] — the Postgres-backed authorization-code and token store, under
//!   the same row-level-security model as the token vault.
//! * [`server`] — [`OAuthServer`], which ties the registry and store together
//!   into the authorize / token / introspect / revoke operations.
//!
//! The HTTP surface lives in `moa-edge`; this crate owns the Postgres-backed
//! cross-replica protocol.
//!
//! # Token format
//!
//! Access, refresh, and authorization codes are opaque, high-entropy random
//! strings with a stable prefix (see [`ACCESS_TOKEN_PREFIX`],
//! [`REFRESH_TOKEN_PREFIX`], [`AUTHORIZATION_CODE_PREFIX`]). Only the SHA-256
//! digest is persisted; the plaintext is returned to the caller exactly once at
//! issuance. The edge recognizes the access-token prefix, resolves one enriched
//! principal from Postgres, and accepts that delegation only on `/mcp`.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use secrecy::SecretString;
use sha2::{Digest, Sha256};

pub mod client;
pub mod pkce;
pub mod server;
pub mod store;

pub use client::{OAuthClient, OAuthClientRegistry};
pub use pkce::{CodeChallengeMethod, is_valid_code_challenge, verify_code_challenge};
pub use server::{
    AuthorizationDecision, AuthorizationOutcome, AuthorizationRequest, AuthorizationSubject,
    CodeExchangeRequest, IntrospectionResponse, IssuedAuthorizationCode, OAuthServer,
    PendingAuthorization, TokenGrant,
};
pub use store::{OAuthStore, ResolvedAccessToken};

/// Opaque access-token prefix. A MOA-issued OAuth access token starts with this.
pub const ACCESS_TOKEN_PREFIX: &str = "moa_oauth_at_";
/// Opaque refresh-token prefix.
pub const REFRESH_TOKEN_PREFIX: &str = "moa_oauth_rt_";
/// Opaque authorization-code prefix.
pub const AUTHORIZATION_CODE_PREFIX: &str = "moa_oauth_ac_";
/// Opaque consent CSRF-token prefix.
pub const CONSENT_CSRF_PREFIX: &str = "moa_oauth_csrf_";

/// Number of random bytes behind each opaque code/token (256 bits of entropy).
const TOKEN_RANDOM_BYTES: usize = 32;

/// Errors returned by the OAuth Authorization Server core.
///
/// Each variant maps to a registered OAuth 2.0 error code via
/// [`OAuthError::error_code`] so the HTTP surface can render RFC-compliant error
/// responses. Every failure path is fail-closed: no partial grant is ever
/// issued.
#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    /// The `client_id` is unknown. The caller MUST NOT redirect for this.
    #[error("unknown client")]
    InvalidClient,
    /// Client authentication failed (missing or wrong secret).
    #[error("client authentication failed")]
    InvalidClientCredentials,
    /// The `redirect_uri` is not registered for the client. The caller MUST NOT
    /// redirect for this.
    #[error("redirect uri is not registered for the client")]
    InvalidRedirectUri,
    /// A requested scope is not allowed for the client.
    #[error("requested scope is not permitted")]
    InvalidScope,
    /// The request was malformed.
    #[error("invalid request: {0}")]
    InvalidRequest(&'static str),
    /// The `response_type` is not `code`.
    #[error("unsupported response type")]
    UnsupportedResponseType,
    /// The `grant_type` is not supported.
    #[error("unsupported grant type")]
    UnsupportedGrantType,
    /// The authorization code / refresh token is invalid, expired, revoked, or
    /// already used, or a binding (client, redirect URI, PKCE) did not match.
    #[error("invalid grant")]
    InvalidGrant,
    /// Startup client declarations conflict with the authoritative database row.
    #[error("oauth client bootstrap conflict for {0}")]
    ClientBootstrapConflict(String),
    /// A startup client declaration is invalid.
    #[error("invalid oauth client configuration: {0}")]
    InvalidClientConfiguration(String),
    /// An authorization transaction was already approved or denied.
    #[error("authorization transaction already decided")]
    AuthorizationAlreadyDecided,
    /// A storage operation failed.
    #[error("oauth storage error: {0}")]
    Storage(String),
}

impl OAuthError {
    /// The registered OAuth 2.0 error code for this failure.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::InvalidClient | Self::InvalidClientCredentials => "invalid_client",
            Self::InvalidRedirectUri | Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidScope => "invalid_scope",
            Self::UnsupportedResponseType => "unsupported_response_type",
            Self::UnsupportedGrantType => "unsupported_grant_type",
            Self::InvalidGrant => "invalid_grant",
            Self::ClientBootstrapConflict(_)
            | Self::InvalidClientConfiguration(_)
            | Self::Storage(_) => "server_error",
            Self::AuthorizationAlreadyDecided => "invalid_request",
        }
    }

    /// Whether the failure occurred before the `redirect_uri` was validated, so
    /// the authorize endpoint MUST return a direct error instead of redirecting
    /// an error back to a caller-supplied URI (OAuth 2.1 §4.1.2.1).
    #[must_use]
    pub fn must_not_redirect(&self) -> bool {
        matches!(self, Self::InvalidClient | Self::InvalidRedirectUri)
    }
}

/// SHA-256 digest of an opaque code/token, lowercase hex. Used as the stored,
/// non-reversible lookup key so raw bearer values never touch the database.
#[must_use]
pub(crate) fn digest_hex(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

/// Generate a fresh opaque token with `prefix`, backed by 256 bits of OS entropy
/// encoded as URL-safe base64 without padding.
#[must_use]
pub(crate) fn generate_opaque_token(prefix: &str) -> SecretString {
    let mut bytes = [0u8; TOKEN_RANDOM_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let random = URL_SAFE_NO_PAD.encode(bytes);
    SecretString::new(format!("{prefix}{random}").into_boxed_str())
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    #[test]
    fn digest_is_stable_and_not_the_input() {
        // Pins: the stored digest is a deterministic SHA-256 hex, never the raw token.
        let token = "moa_oauth_at_example";
        let digest = digest_hex(token);
        assert_ne!(digest, token);
        assert_eq!(digest.len(), 64);
        assert_eq!(digest, digest_hex(token));
    }

    #[test]
    fn generated_tokens_are_prefixed_and_unique() {
        // Pins: opaque tokens carry the recognizable prefix and do not collide.
        let a = generate_opaque_token(ACCESS_TOKEN_PREFIX);
        let b = generate_opaque_token(ACCESS_TOKEN_PREFIX);
        assert!(a.expose_secret().starts_with(ACCESS_TOKEN_PREFIX));
        assert_ne!(a.expose_secret(), b.expose_secret());
    }

    #[test]
    fn error_codes_map_to_registered_oauth_values() {
        // Pins: the OAuth error rendering stays spec-compliant and fail-closed.
        assert_eq!(OAuthError::InvalidClient.error_code(), "invalid_client");
        assert_eq!(OAuthError::InvalidGrant.error_code(), "invalid_grant");
        assert_eq!(OAuthError::InvalidScope.error_code(), "invalid_scope");
        assert!(OAuthError::InvalidRedirectUri.must_not_redirect());
        assert!(OAuthError::InvalidClient.must_not_redirect());
        assert!(!OAuthError::InvalidScope.must_not_redirect());
    }
}
