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
    /// Environment variable holding the Auth0 client id.
    pub client_id_env: String,
    /// Environment variable holding the Auth0 client secret.
    pub client_secret_env: String,
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

/// MOA-issued contact JWT settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactTokenConfig {
    /// Expected issuer for contact JWTs.
    pub issuer: String,
    /// Expected audience for contact JWTs.
    pub audience: String,
    /// JWT key id placed in the token header.
    pub key_id: String,
    /// Environment variable containing the RSA private key PEM for issuance.
    pub private_key_pem_env: String,
    /// Environment variable containing the RSA public key PEM for verification.
    pub public_key_pem_env: String,
    /// Environment variable containing the 32-byte hex key used for contact point lookup hashes.
    pub contact_point_hash_key_env: String,
    /// TTL for unverified contact tokens.
    pub unverified_ttl_seconds: i64,
    /// TTL for verified contact tokens.
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
            private_key_pem_env: "MOA_CONTACT_JWT_PRIVATE_KEY_PEM".to_string(),
            public_key_pem_env: "MOA_CONTACT_JWT_PUBLIC_KEY_PEM".to_string(),
            contact_point_hash_key_env: "MOA_CONTACT_POINT_HASH_KEY_HEX".to_string(),
            unverified_ttl_seconds: 3600,
            verified_ttl_seconds: 86_400,
            verification_ttl_seconds: 600,
        }
    }
}

impl super::MoaEnvOverlay {
    /// Applies authentication environment overrides.
    pub(in crate::config) fn apply_auth_overlay(
        &self,
        config: &mut super::MoaConfig,
    ) -> crate::Result<()> {
        use super::env_overlay::{set_copy_if_some, set_option_if_some};

        set_copy_if_some(&mut config.auth.provider, self.auth_provider);
        self.apply_auth0(config)?;
        set_option_if_some(
            &mut config.auth.auth0_webhook_secret,
            &self.auth_auth0_webhook_secret,
        );
        self.apply_oidc(config)?;
        self.apply_contact_tokens(config)?;

        Ok(())
    }

    fn apply_auth0(&self, config: &mut super::MoaConfig) -> crate::Result<()> {
        use super::env_overlay::{any_present, require_non_empty, set_if_some};

        if !any_present(&[
            self.auth_auth0_domain.is_some(),
            self.auth_auth0_audience.is_some(),
            self.auth_auth0_client_id_env.is_some(),
            self.auth_auth0_client_secret_env.is_some(),
        ]) {
            return Ok(());
        }

        let mut auth0 = config
            .auth
            .auth0
            .clone()
            .unwrap_or_else(|| Auth0AuthConfig {
                domain: String::new(),
                audience: String::new(),
                client_id_env: String::new(),
                client_secret_env: String::new(),
            });
        set_if_some(&mut auth0.domain, &self.auth_auth0_domain);
        set_if_some(&mut auth0.audience, &self.auth_auth0_audience);
        set_if_some(&mut auth0.client_id_env, &self.auth_auth0_client_id_env);
        set_if_some(
            &mut auth0.client_secret_env,
            &self.auth_auth0_client_secret_env,
        );
        require_non_empty("MOA_AUTH_AUTH0_DOMAIN", &auth0.domain)?;
        require_non_empty("MOA_AUTH_AUTH0_AUDIENCE", &auth0.audience)?;
        require_non_empty("MOA_AUTH_AUTH0_CLIENT_ID_ENV", &auth0.client_id_env)?;
        require_non_empty("MOA_AUTH_AUTH0_CLIENT_SECRET_ENV", &auth0.client_secret_env)?;
        config.auth.auth0 = Some(auth0);
        Ok(())
    }

    fn apply_oidc(&self, config: &mut super::MoaConfig) -> crate::Result<()> {
        use super::env_overlay::{any_present, require_non_empty, set_if_some};

        if !any_present(&[
            self.auth_oidc_issuer.is_some(),
            self.auth_oidc_audience.is_some(),
            self.auth_oidc_jwks_url.is_some(),
        ]) {
            return Ok(());
        }

        let mut oidc = config.auth.oidc.clone().unwrap_or_else(|| OidcAuthConfig {
            issuer: String::new(),
            audience: String::new(),
            jwks_url: String::new(),
        });
        set_if_some(&mut oidc.issuer, &self.auth_oidc_issuer);
        set_if_some(&mut oidc.audience, &self.auth_oidc_audience);
        set_if_some(&mut oidc.jwks_url, &self.auth_oidc_jwks_url);
        require_non_empty("MOA_AUTH_OIDC_ISSUER", &oidc.issuer)?;
        require_non_empty("MOA_AUTH_OIDC_AUDIENCE", &oidc.audience)?;
        require_non_empty("MOA_AUTH_OIDC_JWKS_URL", &oidc.jwks_url)?;
        config.auth.oidc = Some(oidc);
        Ok(())
    }

    fn apply_contact_tokens(&self, config: &mut super::MoaConfig) -> crate::Result<()> {
        use super::env_overlay::{any_present, require_non_empty, set_copy_if_some, set_if_some};

        if !any_present(&[
            self.auth_contact_tokens_issuer.is_some(),
            self.auth_contact_tokens_audience.is_some(),
            self.auth_contact_tokens_key_id.is_some(),
            self.auth_contact_tokens_private_key_pem_env.is_some(),
            self.auth_contact_tokens_public_key_pem_env.is_some(),
            self.auth_contact_tokens_contact_point_hash_key_env
                .is_some(),
            self.auth_contact_tokens_unverified_ttl_seconds.is_some(),
            self.auth_contact_tokens_verified_ttl_seconds.is_some(),
            self.auth_contact_tokens_verification_ttl_seconds.is_some(),
        ]) {
            return Ok(());
        }

        let mut contact_tokens: ContactTokenConfig = config.auth.contact_tokens.clone();
        set_if_some(&mut contact_tokens.issuer, &self.auth_contact_tokens_issuer);
        set_if_some(
            &mut contact_tokens.audience,
            &self.auth_contact_tokens_audience,
        );
        set_if_some(&mut contact_tokens.key_id, &self.auth_contact_tokens_key_id);
        set_if_some(
            &mut contact_tokens.private_key_pem_env,
            &self.auth_contact_tokens_private_key_pem_env,
        );
        set_if_some(
            &mut contact_tokens.public_key_pem_env,
            &self.auth_contact_tokens_public_key_pem_env,
        );
        set_if_some(
            &mut contact_tokens.contact_point_hash_key_env,
            &self.auth_contact_tokens_contact_point_hash_key_env,
        );
        set_copy_if_some(
            &mut contact_tokens.unverified_ttl_seconds,
            self.auth_contact_tokens_unverified_ttl_seconds,
        );
        set_copy_if_some(
            &mut contact_tokens.verified_ttl_seconds,
            self.auth_contact_tokens_verified_ttl_seconds,
        );
        set_copy_if_some(
            &mut contact_tokens.verification_ttl_seconds,
            self.auth_contact_tokens_verification_ttl_seconds,
        );
        require_non_empty("MOA_AUTH_CONTACT_TOKENS_ISSUER", &contact_tokens.issuer)?;
        require_non_empty("MOA_AUTH_CONTACT_TOKENS_AUDIENCE", &contact_tokens.audience)?;
        require_non_empty("MOA_AUTH_CONTACT_TOKENS_KEY_ID", &contact_tokens.key_id)?;
        require_non_empty(
            "MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM_ENV",
            &contact_tokens.private_key_pem_env,
        )?;
        require_non_empty(
            "MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM_ENV",
            &contact_tokens.public_key_pem_env,
        )?;
        require_non_empty(
            "MOA_AUTH_CONTACT_TOKENS_CONTACT_POINT_HASH_KEY_ENV",
            &contact_tokens.contact_point_hash_key_env,
        )?;
        config.auth.contact_tokens = contact_tokens;
        Ok(())
    }
}
