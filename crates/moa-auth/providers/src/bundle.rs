//! Provider bundle construction for authentication, token vault, and approvals.

use std::sync::Arc;

#[cfg(feature = "auth0")]
use async_trait::async_trait;
use moa_authz::AwakeableResolver;
use moa_core::MoaConfig;
use moa_core::config::{AsyncAuthzKind, AuthProviderKind, TokenVaultKind};
use moa_core::traits::{AsyncAuthzProvider, AuthProvider, TokenVaultProvider};
#[cfg(feature = "auth0")]
use moa_core::traits::{AuthError, Credential, Identity};
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
    /// A selected provider is missing its required configuration section.
    #[error("missing required config section: {0}")]
    MissingConfig(&'static str),
    /// A selected provider is missing a required environment variable.
    #[error("missing required environment variable: {0}")]
    MissingEnv(String),
    /// A selected provider failed during construction.
    #[error("provider construction failed: {0}")]
    Provider(String),
}

/// Construct the configured provider bundle.
pub fn build_providers(cfg: &MoaConfig, pool: Arc<sqlx::PgPool>) -> Result<Providers, BuildError> {
    build_providers_with_resolver(cfg, pool, None)
}

/// Construct the configured provider bundle with an optional awakeable resolver.
pub fn build_providers_with_resolver(
    cfg: &MoaConfig,
    pool: Arc<sqlx::PgPool>,
    #[cfg_attr(not(feature = "auth0"), allow(unused_variables))] awakeable_resolver: Option<
        Arc<dyn AwakeableResolver>,
    >,
) -> Result<Providers, BuildError> {
    let auth: Arc<dyn AuthProvider> = match cfg.auth.provider {
        AuthProviderKind::Local => Arc::new(crate::LocalAuthProvider::new(pool.clone())),
        AuthProviderKind::Disabled => Arc::new(crate::DisabledAuthProvider),
        AuthProviderKind::Auth0 => {
            #[cfg(feature = "auth0")]
            {
                let auth0 = cfg
                    .auth
                    .auth0
                    .as_ref()
                    .ok_or(BuildError::MissingConfig("auth.auth0"))?;
                Arc::new(HybridAuthProvider::new(
                    Arc::new(crate::LocalAuthProvider::new(pool.clone())),
                    Arc::new(moa_auth_providers_auth0::Auth0AuthProvider::new(
                        &auth0.domain,
                        &auth0.audience,
                        pool.clone(),
                    )),
                ))
            }
            #[cfg(not(feature = "auth0"))]
            {
                return Err(BuildError::AuthFeatureMissing(AuthProviderKind::Auth0));
            }
        }
        AuthProviderKind::Oidc => {
            #[cfg(feature = "auth0")]
            {
                let oidc = cfg
                    .auth
                    .oidc
                    .as_ref()
                    .ok_or(BuildError::MissingConfig("auth.oidc"))?;
                Arc::new(HybridAuthProvider::new(
                    Arc::new(crate::LocalAuthProvider::new(pool.clone())),
                    Arc::new(moa_auth_providers_auth0::OidcAuthProvider::new(
                        oidc.issuer.clone(),
                        oidc.audience.clone(),
                        oidc.jwks_url.clone(),
                        None,
                        None,
                        pool.clone(),
                    )),
                ))
            }
            #[cfg(not(feature = "auth0"))]
            {
                return Err(BuildError::AuthFeatureMissing(AuthProviderKind::Oidc));
            }
        }
    };

    let token_vault: Arc<dyn TokenVaultProvider> = match cfg.token_vault.provider {
        TokenVaultKind::None => Arc::new(crate::NullTokenVaultProvider),
        TokenVaultKind::Auth0 => {
            #[cfg(feature = "auth0")]
            {
                let auth0 = cfg.auth.auth0.as_ref().ok_or(BuildError::MissingConfig(
                    "auth.auth0 (required for token vault)",
                ))?;
                let client_id = env_value(&auth0.client_id_env)?;
                let client_secret = env_value(&auth0.client_secret_env)?;
                Arc::new(
                    moa_auth_providers_auth0::Auth0TokenVaultProvider::new(
                        auth0.domain.clone(),
                        client_id,
                        secrecy::SecretString::new(client_secret.into_boxed_str()),
                        format!("https://{}/api/v2/", auth0.domain.trim_end_matches('/')),
                        pool.clone(),
                    )
                    .map_err(|error| BuildError::Provider(error.to_string()))?,
                )
            }
            #[cfg(not(feature = "auth0"))]
            {
                return Err(BuildError::VaultFeatureMissing(TokenVaultKind::Auth0));
            }
        }
    };

    let async_authz: Arc<dyn AsyncAuthzProvider> = match cfg.async_authz.provider {
        AsyncAuthzKind::Builtin => Arc::new(crate::BuiltinAsyncAuthzProvider::new(pool)),
        AsyncAuthzKind::Auth0 => {
            #[cfg(feature = "auth0")]
            {
                let auth0 = cfg.auth.auth0.as_ref().ok_or(BuildError::MissingConfig(
                    "auth.auth0 (required for async_authz)",
                ))?;
                let resolver =
                    awakeable_resolver.ok_or(BuildError::MissingConfig("awakeable resolver"))?;
                let client_id = env_value(&auth0.client_id_env)?;
                let client_secret = env_value(&auth0.client_secret_env)?;
                Arc::new(
                    moa_auth_providers_auth0::Auth0AsyncAuthzProvider::new(
                        auth0.domain.clone(),
                        client_id,
                        secrecy::SecretString::new(client_secret.into_boxed_str()),
                        pool.clone(),
                        resolver,
                    )
                    .map_err(|error| BuildError::Provider(error.to_string()))?,
                )
            }
            #[cfg(not(feature = "auth0"))]
            {
                return Err(BuildError::AsyncAuthzFeatureMissing(AsyncAuthzKind::Auth0));
            }
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

#[cfg(feature = "auth0")]
fn env_value(name: &str) -> Result<String, BuildError> {
    std::env::var(name).map_err(|_| BuildError::MissingEnv(name.to_string()))
}

#[cfg(feature = "auth0")]
struct HybridAuthProvider {
    local: Arc<dyn AuthProvider>,
    bearer: Arc<dyn AuthProvider>,
}

#[cfg(feature = "auth0")]
impl HybridAuthProvider {
    fn new(local: Arc<dyn AuthProvider>, bearer: Arc<dyn AuthProvider>) -> Self {
        Self { local, bearer }
    }
}

#[cfg(feature = "auth0")]
#[async_trait]
impl AuthProvider for HybridAuthProvider {
    async fn authenticate(&self, credential: &Credential) -> Result<Identity, AuthError> {
        match credential {
            Credential::ApiKey(_) => self.local.authenticate(credential).await,
            Credential::BearerJwt(_) => self.bearer.authenticate(credential).await,
        }
    }

    fn name(&self) -> &'static str {
        self.bearer.name()
    }
}

#[cfg(all(test, not(feature = "auth0")))]
mod tests {
    use super::*;
    use moa_core::config::AuthProviderKind;
    use moa_core::traits::{Credential, IdentityType};

    #[tokio::test]
    async fn auth0_without_feature_returns_feature_missing() {
        // Pins: selecting Auth0 without compiling the auth0 feature fails before startup can serve.
        let mut config = MoaConfig::default();
        config.auth.provider = AuthProviderKind::Auth0;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://moa_owner:dev@127.0.0.1:1/moa")
            .expect("lazy pool should not connect");
        let error = match build_providers(&config, Arc::new(pool)) {
            Ok(_) => panic!("auth0 should be missing"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            BuildError::AuthFeatureMissing(AuthProviderKind::Auth0)
        ));
    }

    #[tokio::test]
    async fn disabled_auth_provider_accepts_any_credential_as_service_identity() {
        // Pins: auth.provider=disabled constructs a provider that bypasses credential checks.
        let mut config = MoaConfig::default();
        config.auth.provider = AuthProviderKind::Disabled;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://moa_owner:dev@127.0.0.1:1/moa")
            .expect("lazy pool should not connect");
        let providers =
            build_providers(&config, Arc::new(pool)).expect("disabled auth should build");

        assert_eq!(providers.auth.name(), "disabled");
        assert!(!providers.auth.requires_credentials());
        let identity = providers
            .auth
            .authenticate(&Credential::ApiKey("ignored".to_string()))
            .await
            .expect("disabled auth accepts any credential");
        assert_eq!(identity.identity_type, IdentityType::Service);
        assert_eq!(identity.id, uuid::Uuid::nil());
        assert_eq!(identity.tenant_id, uuid::Uuid::nil());
        assert_eq!(identity.api_key_id, None);
    }
}
