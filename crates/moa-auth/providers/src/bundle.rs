//! Independent builders for authentication and approvals.

use std::sync::Arc;

#[cfg(feature = "auth0")]
use async_trait::async_trait;
use moa_authz::AwakeableResolver;
use moa_config::MoaConfig;
use moa_config::{AsyncAuthzKind, AuthProviderKind};
use moa_core::traits::{AsyncAuthzProvider, AuthProvider};
#[cfg(feature = "auth0")]
use moa_core::traits::{AuthError, Credential, Identity};
use thiserror::Error;

/// Authentication component construction failures.
#[derive(Debug, Error)]
pub enum BuildError {
    /// Auth provider requires an unavailable feature.
    #[error("auth provider '{0:?}' selected but feature not enabled")]
    AuthFeatureMissing(AuthProviderKind),
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
                    Arc::new(crate::auth0::Auth0AuthProvider::new(
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
                    Arc::new(crate::auth0::OidcAuthProvider::new(
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
                    crate::auth0::Auth0AsyncAuthzProvider::new(
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
    let private_key = moa_config::optional_config_secret(&cfg.auth.contact_tokens.private_key_pem);
    let public_key = moa_config::optional_config_secret(&cfg.auth.contact_tokens.public_key_pem);
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
    moa_config::required_config_secret(env_name, value)
        .map_err(|_| BuildError::MissingEnv(env_name.to_string()))
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
    use moa_config::AuthProviderKind;
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
}
