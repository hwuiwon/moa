//! Generic OIDC JWT authentication provider.
//!
//! The provider validates RS256 bearer JWTs against a configured issuer,
//! audience, and JWKS URL. Claim names for tenant and identity type are
//! intentionally simple for P1.7 and can be extended by config later.

use crate::auth0_provider::{parse_identity_type, resolve_or_provision_static};
use crate::jwks_cache::JwksCache;
use async_trait::async_trait;
use jsonwebtoken::{Algorithm, Validation, decode, decode_header};
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity};
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Generic OIDC-backed JWT authentication provider.
pub struct OidcAuthProvider {
    jwks: JwksCache,
    issuer: String,
    audience: String,
    tenant_id_claim: String,
    identity_type_claim: String,
    pool: Arc<PgPool>,
}

impl OidcAuthProvider {
    /// Construct a generic OIDC provider with explicit issuer and JWKS URL.
    #[must_use]
    pub fn new(
        issuer: String,
        audience: String,
        jwks_url: String,
        tenant_id_claim: Option<String>,
        identity_type_claim: Option<String>,
        pool: Arc<PgPool>,
    ) -> Self {
        Self {
            jwks: JwksCache::new(jwks_url, Duration::from_secs(3600)),
            issuer,
            audience,
            tenant_id_claim: tenant_id_claim.unwrap_or_else(|| "tenant_id".to_string()),
            identity_type_claim: identity_type_claim.unwrap_or_else(|| "identity_type".to_string()),
            pool,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct GenericClaims {
    sub: String,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[async_trait]
impl AuthProvider for OidcAuthProvider {
    async fn authenticate(&self, credential: &Credential) -> Result<Identity, AuthError> {
        let token = match credential {
            Credential::BearerJwt(token) => token,
            Credential::ApiKey(_) => return Err(AuthError::NotConfigured),
        };

        let header = decode_header(token).map_err(|_| AuthError::InvalidFormat)?;
        if header.alg != Algorithm::RS256 {
            return Err(AuthError::Rejected);
        }
        let kid = header.kid.ok_or(AuthError::InvalidFormat)?;
        let key = self
            .jwks
            .key_for(&kid)
            .await
            .map_err(|error| AuthError::Unavailable(format!("jwks: {error}")))?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "sub", "iss", "aud"]);
        validation.leeway = 30;

        let claims = decode::<GenericClaims>(token, &key, &validation)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::Expired,
                _ => AuthError::Rejected,
            })?
            .claims;

        let tenant_str = claims
            .extra
            .get(&self.tenant_id_claim)
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                AuthError::Internal(format!("missing required claim: {}", self.tenant_id_claim))
            })?;
        let tenant_id = Uuid::parse_str(tenant_str)
            .map_err(|_| AuthError::Internal("tenant claim is not a UUID".into()))?;
        let identity_type = parse_identity_type(
            claims
                .extra
                .get(&self.identity_type_claim)
                .and_then(|value| value.as_str()),
        )?;
        let id =
            resolve_or_provision_static(&self.pool, &claims.sub, tenant_id, identity_type, "oidc")
                .await
                .map_err(|error| AuthError::Internal(format!("resolve sub: {error}")))?;

        Ok(Identity {
            identity_type,
            id,
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        })
    }

    fn name(&self) -> &'static str {
        "oidc"
    }
}
