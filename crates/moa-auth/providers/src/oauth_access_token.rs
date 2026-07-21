//! One-pass authentication for MOA-issued OAuth access tokens.

use std::sync::Arc;

use chrono::Utc;
use moa_core::traits::{AuthError, Identity};
use moa_core::types::identifiers::TenantId;
use sqlx::PgPool;

use crate::oauth_as::{
    ACCESS_TOKEN_PREFIX, OAuthError, OAuthStore, ResolvedAccessToken, digest_hex,
};

/// OAuth delegation attached to an authenticated edge principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAuthorization {
    /// Client that received the grant.
    pub client_id: String,
    /// Exact granted scopes.
    pub scopes: Vec<String>,
    /// Exact RFC 8707 protected resource.
    pub resource: String,
}

/// One edge authentication result reused for authorization and dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPrincipal {
    /// Resolved MOA identity.
    pub identity: Identity,
    /// OAuth delegation, absent for API keys, sessions, and JWTs.
    pub oauth: Option<OAuthAuthorization>,
}

impl AuthenticatedPrincipal {
    /// Build a principal for a non-OAuth credential.
    #[must_use]
    pub fn from_identity(identity: Identity) -> Self {
        Self {
            identity,
            oauth: None,
        }
    }

    /// Whether this principal carries an OAuth delegation.
    #[must_use]
    pub fn is_oauth(&self) -> bool {
        self.oauth.is_some()
    }

    /// Whether the OAuth grant contains the exact scope.
    #[must_use]
    pub fn has_oauth_scope(&self, required: &str) -> bool {
        self.oauth
            .as_ref()
            .is_some_and(|oauth| oauth.scopes.iter().any(|scope| scope == required))
    }
}

/// Resolves opaque MOA-issued OAuth access tokens from Postgres.
pub struct OAuthAccessTokenProvider {
    store: OAuthStore,
}

impl OAuthAccessTokenProvider {
    /// Build the provider over an existing Postgres pool.
    #[must_use]
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self {
            store: OAuthStore::new(pool),
        }
    }

    /// Resolve identity, client, scopes, and resource with one database lookup.
    pub async fn authenticate(&self, token: &str) -> Result<AuthenticatedPrincipal, AuthError> {
        let resolved = self
            .store
            .resolve_active_access_token(&digest_hex(token))
            .await
            .map_err(map_store_error)?
            .ok_or(AuthError::Rejected)?;
        if resolved.access_token_expires_at <= Utc::now() {
            return Err(AuthError::Expired);
        }
        build_principal(resolved)
    }

    /// Stable provider name for auth telemetry.
    #[must_use]
    pub fn name(&self) -> &'static str {
        "oauth_access_token"
    }
}

fn build_principal(resolved: ResolvedAccessToken) -> Result<AuthenticatedPrincipal, AuthError> {
    let identity_type = resolved
        .subject_type
        .parse()
        .map_err(|unknown| AuthError::Internal(format!("unknown oauth subject_type: {unknown}")))?;
    Ok(AuthenticatedPrincipal {
        identity: Identity {
            identity_type,
            id: resolved.subject_id,
            tenant_id: TenantId::from(resolved.tenant_id),
            api_key_id: None,
            acting_on_behalf_of: None,
        },
        oauth: Some(OAuthAuthorization {
            client_id: resolved.client_id,
            scopes: resolved.scopes,
            resource: resolved.resource,
        }),
    })
}

fn map_store_error(error: OAuthError) -> AuthError {
    tracing::error!(error = %error, "oauth access-token store error during authentication");
    AuthError::Unavailable("oauth token store unavailable".to_string())
}

/// Whether a raw bearer value has the first-party OAuth access-token prefix.
#[must_use]
pub fn looks_like_oauth_access_token(token: &str) -> bool {
    token.starts_with(ACCESS_TOKEN_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_subject_type_fails_closed() {
        // Pins: corrupt persisted subject types cannot become an identity.
        let error = build_principal(ResolvedAccessToken {
            tenant_id: uuid::Uuid::new_v4(),
            client_id: "client".to_string(),
            subject_id: uuid::Uuid::new_v4(),
            subject_type: "workspace_admin".to_string(),
            scopes: Vec::new(),
            resource: "https://api.example.com".to_string(),
            access_token_expires_at: Utc::now(),
        })
        .expect_err("unknown subject_type must be rejected");
        assert!(matches!(
            error,
            AuthError::Internal(message)
                if message == "unknown oauth subject_type: workspace_admin"
        ));
    }

    #[test]
    fn prefix_recognizes_only_access_tokens() {
        // Pins: refresh tokens and API keys never enter the OAuth lookup.
        assert!(looks_like_oauth_access_token("moa_oauth_at_abc123"));
        assert!(!looks_like_oauth_access_token("moa_oauth_rt_abc123"));
        assert!(!looks_like_oauth_access_token("moa_dev_abc123"));
    }
}
