//! Local authentication provider backed by MOA API keys.

use std::sync::Arc;

use async_trait::async_trait;
use moa_core::traits::{AuthError, AuthProvider, Credential, Identity, IdentityType};
use sqlx::PgPool;

use crate::api_keys::{ApiKeyError, validate};

/// Local API-key authentication provider.
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
        let key = match credential {
            Credential::ApiKey(key) => key,
            Credential::BearerJwt(_) => return Err(AuthError::NotConfigured),
        };

        match validate(&self.pool, key).await {
            Ok(resolved) => {
                let (identity_type, id) = match (resolved.owner_user_id, resolved.owner_agent_id) {
                    (Some(user_id), None) => (IdentityType::User, user_id),
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

    fn name(&self) -> &'static str {
        "local"
    }
}
