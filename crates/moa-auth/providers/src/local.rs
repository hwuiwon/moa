//! Local authentication provider backed by MOA API keys and user sessions.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity, IdentityType};
use sqlx::PgPool;

use crate::api_keys::{ApiKeyError, validate};
use crate::user_sessions::{self, UserSessionTokenError};

/// Local API-key and user-session authentication provider.
pub struct LocalAuthProvider {
    pool: Arc<PgPool>,
}

impl LocalAuthProvider {
    /// Build a local provider over an existing Postgres pool.
    #[must_use]
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl AuthProvider for LocalAuthProvider {
    async fn authenticate(&self, credential: &Credential) -> Result<Identity, AuthError> {
        match credential {
            Credential::ApiKey(key) => authenticate_api_key(&self.pool, key).await,
            Credential::UserSessionToken(token) => {
                authenticate_user_session(&self.pool, token).await
            }
            Credential::BearerJwt(_) => Err(AuthError::NotConfigured),
        }
    }

    fn name(&self) -> &'static str {
        "local"
    }
}

async fn authenticate_api_key(pool: &PgPool, key: &str) -> Result<Identity, AuthError> {
    match validate(pool, key).await {
        Ok(resolved) => {
            let (identity_type, id) = match (resolved.owner_user_id, resolved.owner_agent_id) {
                (Some(user_id), None) => (IdentityType::Operator, user_id),
                (None, Some(agent_id)) => (IdentityType::Agent, agent_id),
                _ => {
                    return Err(AuthError::Internal(
                        "api_keys owner invariant violated".to_string(),
                    ));
                }
            };
            Ok(Identity {
                identity_type,
                id,
                tenant_id: moa_core::TenantId::from(resolved.tenant_id),
                api_key_id: Some(resolved.id),
                acting_on_behalf_of: None,
            })
        }
        Err(ApiKeyError::Malformed(_) | ApiKeyError::CrcMismatch | ApiKeyError::UnknownEnv) => {
            Err(AuthError::InvalidFormat)
        }
        Err(ApiKeyError::NotFoundOrRevoked) => Err(AuthError::Rejected),
        Err(ApiKeyError::Database(error)) => {
            tracing::error!(error = %error, "api key database error during authentication");
            Err(AuthError::Unavailable(
                "api key store unavailable".to_string(),
            ))
        }
        Err(ApiKeyError::Hash(error)) => Err(AuthError::Internal(error)),
    }
}

async fn authenticate_user_session(pool: &PgPool, token: &str) -> Result<Identity, AuthError> {
    match user_sessions::validate(pool, token).await {
        Ok(resolved) => Ok(Identity {
            identity_type: IdentityType::Operator,
            id: resolved.user_id,
            tenant_id: moa_core::TenantId::from(resolved.tenant_id),
            api_key_id: None,
            acting_on_behalf_of: None,
        }),
        Err(UserSessionTokenError::Malformed(_)) => Err(AuthError::InvalidFormat),
        Err(UserSessionTokenError::NotFoundExpiredOrRevoked) => Err(AuthError::Rejected),
        Err(UserSessionTokenError::Database(error)) => {
            tracing::error!(error = %error, "user session database error during authentication");
            Err(AuthError::Unavailable(
                "user session store unavailable".to_string(),
            ))
        }
        Err(UserSessionTokenError::Hash(error)) => Err(AuthError::Internal(error)),
    }
}
