//! Independent builders for authentication, token vault, and approvals.

use std::sync::Arc;

#[cfg(feature = "auth0")]
use async_trait::async_trait;
use moa_authz::AwakeableResolver;
use moa_core::config::MoaConfig;
use moa_core::config::{AsyncAuthzKind, AuthProviderKind, TokenVaultKind};
use moa_core::traits::{AsyncAuthzProvider, AuthProvider, TokenVaultProvider};
#[cfg(feature = "auth0")]
use moa_core::traits::{AuthError, Credential, Identity};
use thiserror::Error;

/// Authentication component construction failures.
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

/// Construct only the configured credential authentication provider.
pub fn build_auth_provider(
    cfg: &MoaConfig,
    pool: Arc<sqlx::PgPool>,
) -> Result<Arc<dyn AuthProvider>, BuildError> {
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

    tracing::info!(auth = auth.name(), "authentication provider constructed");
    Ok(auth)
}

/// Construct only the configured third-party token-vault provider.
pub fn build_token_vault_provider(
    cfg: &MoaConfig,
    pool: Arc<sqlx::PgPool>,
    kms: Arc<dyn moa_crypto::KeyManagementProvider>,
) -> Result<Arc<dyn TokenVaultProvider>, BuildError> {
    cfg.token_vault
        .validate()
        .map_err(|error| BuildError::Provider(error.to_string()))?;
    let token_vault: Arc<dyn TokenVaultProvider> = match cfg.token_vault.provider {
        TokenVaultKind::None => Arc::new(crate::NullTokenVaultProvider),
        TokenVaultKind::Postgres => {
            let mut vault = crate::PostgresTokenVaultProvider::new(pool.clone(), kms);
            if let Some(refresher) = build_token_refresher(&cfg.token_vault)? {
                vault = vault.with_refresher(refresher);
            }
            Arc::new(vault)
        }
        TokenVaultKind::Auth0 => {
            #[cfg(feature = "auth0")]
            {
                let auth0 = cfg.auth.auth0.as_ref().ok_or(BuildError::MissingConfig(
                    "auth.auth0 (required for token vault)",
                ))?;
                let client_id =
                    required_config_secret("MOA_AUTH_AUTH0_CLIENT_ID", &auth0.client_id)?;
                let client_secret =
                    required_config_secret("MOA_AUTH_AUTH0_CLIENT_SECRET", &auth0.client_secret)?;
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

    tracing::info!(
        vault = token_vault.name(),
        "token-vault provider constructed"
    );
    Ok(token_vault)
}

/// Construct only the configured asynchronous authorization provider.
pub fn build_async_authz_provider(
    cfg: &MoaConfig,
    pool: Arc<sqlx::PgPool>,
    #[cfg_attr(not(feature = "auth0"), allow(unused_variables))] awakeable_resolver: Option<
        Arc<dyn AwakeableResolver>,
    >,
) -> Result<Arc<dyn AsyncAuthzProvider>, BuildError> {
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
                let client_id =
                    required_config_secret("MOA_AUTH_AUTH0_CLIENT_ID", &auth0.client_id)?;
                let client_secret =
                    required_config_secret("MOA_AUTH_AUTH0_CLIENT_SECRET", &auth0.client_secret)?;
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
        async_authz = async_authz.name(),
        "async authz provider constructed"
    );
    Ok(async_authz)
}

/// Construct the optional contact-token issuer from direct runtime config.
pub fn build_contact_token_issuer(
    cfg: &MoaConfig,
) -> Result<Option<Arc<crate::ContactTokenIssuer>>, BuildError> {
    let private_key =
        moa_core::config::optional_config_secret(&cfg.auth.contact_tokens.private_key_pem);
    let public_key =
        moa_core::config::optional_config_secret(&cfg.auth.contact_tokens.public_key_pem);
    match (private_key, public_key) {
        (None, None) => Ok(None),
        (Some(private_key), Some(public_key)) => crate::ContactTokenIssuer::from_key_pems(
            &cfg.auth.contact_tokens,
            private_key.as_bytes(),
            public_key.as_bytes(),
        )
        .map(Arc::new)
        .map(Some)
        .map_err(|error| BuildError::Provider(error.to_string())),
        (None, Some(_)) => Err(BuildError::MissingEnv(
            "MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM".to_string(),
        )),
        (Some(_), None) => Err(BuildError::MissingEnv(
            "MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM".to_string(),
        )),
    }
}

#[cfg(feature = "auth0")]
fn required_config_secret(env_name: &'static str, value: &str) -> Result<String, BuildError> {
    moa_core::config::required_config_secret(env_name, value)
        .map_err(|_| BuildError::MissingEnv(env_name.to_string()))
}

/// Build the optional OAuth refresher for the self-hosted token vault.
///
/// Returns `Ok(None)` when no connection has refresh settings, preserving the
/// expired-token-fails-closed behavior. Otherwise resolves each connection's
/// client secret directly from typed config and constructs the refresher.
fn build_token_refresher(
    cfg: &moa_core::config::TokenVaultConfig,
) -> Result<Option<Arc<crate::TokenRefresher>>, BuildError> {
    if cfg.refresh.is_empty() {
        return Ok(None);
    }
    let mut endpoints = std::collections::HashMap::with_capacity(cfg.refresh.len());
    for (connection, refresh) in &cfg.refresh {
        let client_secret = refresh
            .client_secret
            .as_ref()
            .map(|secret| secrecy::SecretString::new(secret.clone().into_boxed_str()));
        endpoints.insert(
            connection.clone(),
            crate::OAuthRefreshEndpoint {
                token_endpoint: refresh.token_endpoint.trim().to_string(),
                client_id: refresh.client_id.trim().to_string(),
                client_secret,
            },
        );
    }
    let refresher = crate::TokenRefresher::new(endpoints)
        .map_err(|error| BuildError::Provider(error.to_string()))?;
    Ok(Some(Arc::new(refresher)))
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
            Credential::ApiKey(_)
            | Credential::UserSessionToken(_)
            | Credential::OAuthAccessToken(_) => self.local.authenticate(credential).await,
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
        let error = match build_auth_provider(&config, Arc::new(pool)) {
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
        let provider =
            build_auth_provider(&config, Arc::new(pool)).expect("disabled auth should build");

        assert_eq!(provider.name(), "disabled");
        assert!(!provider.requires_credentials());
        let identity = provider
            .authenticate(&Credential::ApiKey("ignored".to_string()))
            .await
            .expect("disabled auth accepts any credential");
        assert_eq!(identity.identity_type, IdentityType::Service);
        assert_eq!(identity.id, uuid::Uuid::nil());
        assert_eq!(
            identity.tenant_id,
            moa_core::types::identifiers::TenantId::from(uuid::Uuid::nil())
        );
        assert_eq!(identity.api_key_id, None);
    }

    #[test]
    fn token_refresher_accepts_direct_client_secret() {
        // Pins: refresh credentials are consumed directly from typed config;
        // construction does not depend on a second environment-variable name.
        let mut config = moa_core::config::TokenVaultConfig::default();
        config.refresh.insert(
            "github".to_string(),
            moa_core::config::OAuthRefreshConfig {
                token_endpoint: "https://github.com/login/oauth/access_token".to_string(),
                client_id: "client-id".to_string(),
                client_secret: Some("client-secret".to_string()),
            },
        );

        assert!(
            build_token_refresher(&config)
                .expect("direct refresh config builds")
                .is_some()
        );
    }
}
