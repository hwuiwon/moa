//! Provider bundle construction for authentication, token vault, and approvals.

use std::sync::Arc;

use moa_core::MoaConfig;
use moa_core::config::{AsyncAuthzKind, AuthProviderKind, TokenVaultKind};
use moa_core::traits::{AsyncAuthzProvider, AuthProvider, TokenVaultProvider};
use thiserror::Error;

/// Authentication-related provider trait objects constructed at startup.
#[derive(Clone)]
pub struct Providers {
    /// Credential authentication provider.
    pub auth: Arc<dyn AuthProvider>,
    /// Third-party token vault provider.
    pub token_vault: Arc<dyn TokenVaultProvider>,
    /// Async human approval provider.
    pub async_authz: Arc<dyn AsyncAuthzProvider>,
}

/// Provider bundle construction failures.
#[derive(Debug, Error)]
pub enum BuildError {
    /// Auth provider requires an unavailable feature.
    #[error("auth provider '{0:?}' selected but feature not enabled")]
    AuthFeatureMissing(AuthProviderKind),
    /// Token vault provider requires an unavailable feature.
    #[error("token vault '{0:?}' selected but feature not enabled")]
    VaultFeatureMissing(TokenVaultKind),
    /// Async authorization provider requires an unavailable feature.
    #[error("async authz '{0:?}' selected but feature not enabled")]
    AsyncAuthzFeatureMissing(AsyncAuthzKind),
}

/// Construct the configured provider bundle.
pub fn build_providers(cfg: &MoaConfig, pool: Arc<sqlx::PgPool>) -> Result<Providers, BuildError> {
    let auth: Arc<dyn AuthProvider> = match cfg.auth.provider {
        AuthProviderKind::Local => Arc::new(crate::LocalAuthProvider::new(pool.clone())),
        AuthProviderKind::Auth0 => {
            return Err(BuildError::AuthFeatureMissing(AuthProviderKind::Auth0));
        }
        AuthProviderKind::Oidc => {
            return Err(BuildError::AuthFeatureMissing(AuthProviderKind::Oidc));
        }
    };

    let token_vault: Arc<dyn TokenVaultProvider> = match cfg.token_vault.provider {
        TokenVaultKind::None => Arc::new(crate::NullTokenVaultProvider),
        TokenVaultKind::Auth0 => {
            return Err(BuildError::VaultFeatureMissing(TokenVaultKind::Auth0));
        }
    };

    let async_authz: Arc<dyn AsyncAuthzProvider> = match cfg.async_authz.provider {
        AsyncAuthzKind::Builtin => Arc::new(crate::BuiltinAsyncAuthzProvider::new(pool)),
        AsyncAuthzKind::Auth0 => {
            return Err(BuildError::AsyncAuthzFeatureMissing(AsyncAuthzKind::Auth0));
        }
    };

    tracing::info!(
        auth = auth.name(),
        vault = token_vault.name(),
        async_authz = async_authz.name(),
        "providers bundle constructed"
    );

    Ok(Providers {
        auth,
        token_vault,
        async_authz,
    })
}
