//! Issuance and verification for MOA contact JWTs.

use chrono::{DateTime, Duration, Utc};
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header,
};
use moa_core::config::ContactTokenConfig;
use moa_core::{
    error::MoaError, types::contact::ContactRef, types::contact::ContactTokenClaims,
    types::contact::ContactVerificationState,
};
use thiserror::Error;
use uuid::Uuid;

/// Contact-token issuance or verification failure.
#[derive(Debug, Error)]
pub enum ContactTokenError {
    /// Required key material was not present in the environment.
    #[error("missing required environment variable: {0}")]
    MissingEnv(String),
    /// Configured key material could not be parsed.
    #[error("invalid contact token key material: {0}")]
    InvalidKey(String),
    /// Token syntax was invalid.
    #[error("invalid contact token format")]
    InvalidFormat,
    /// Token was rejected by signature, issuer, audience, or scope validation.
    #[error("contact token rejected")]
    Rejected,
    /// Token was valid but expired.
    #[error("contact token expired")]
    Expired,
}

impl From<ContactTokenError> for MoaError {
    fn from(value: ContactTokenError) -> Self {
        MoaError::ConfigError(value.to_string())
    }
}

/// Verifies MOA-issued contact tokens.
#[derive(Clone)]
pub struct ContactTokenVerifier {
    issuer: String,
    audience: String,
    key_id: String,
    decoding_key: DecodingKey,
}

impl ContactTokenVerifier {
    /// Builds a verifier from configured key material.
    pub fn from_env(config: &ContactTokenConfig) -> Result<Self, ContactTokenError> {
        let public_key = config_secret(
            "MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM",
            &config.public_key_pem,
        )?;
        Self::from_public_key_pem(config, public_key.as_bytes())
    }

    /// Builds a verifier from an RSA public key PEM.
    pub fn from_public_key_pem(
        config: &ContactTokenConfig,
        public_key_pem: &[u8],
    ) -> Result<Self, ContactTokenError> {
        let decoding_key = DecodingKey::from_rsa_pem(public_key_pem)
            .map_err(|error| ContactTokenError::InvalidKey(error.to_string()))?;
        Ok(Self {
            issuer: config.issuer.clone(),
            audience: config.audience.clone(),
            key_id: config.key_id.clone(),
            decoding_key,
        })
    }

    /// Verifies a token and returns its decoded claims.
    pub fn verify(&self, token: &str) -> Result<ContactTokenClaims, ContactTokenError> {
        let header = decode_header(token).map_err(|_| ContactTokenError::InvalidFormat)?;
        if header.alg != Algorithm::RS256 {
            return Err(ContactTokenError::Rejected);
        }
        if header.kid.as_deref() != Some(self.key_id.as_str()) {
            return Err(ContactTokenError::Rejected);
        }

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "nbf", "iat", "sub", "iss", "aud", "jti"]);
        validation.leeway = 30;

        // Verification is stateless: `jti` is required for presence but is not
        // checked against a revocation denylist, so the token's TTL is the only
        // revocation window. A `jti` denylist (checked here) would let operators
        // revoke a token before it expires — follow-up, not implemented.

        decode::<ContactTokenClaims>(token, &self.decoding_key, &validation)
            .map(|data| data.claims)
            .map_err(|error| match error.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => ContactTokenError::Expired,
                _ => ContactTokenError::Rejected,
            })
    }
}

/// Issues and verifies MOA contact JWTs.
pub struct ContactTokenIssuer {
    config: ContactTokenConfig,
    encoding_key: EncodingKey,
    verifier: ContactTokenVerifier,
}

/// Signed contact token plus the exact claims persisted for audit/revocation.
pub struct IssuedContactToken {
    /// Compact signed JWT.
    pub token: String,
    /// Token expiration timestamp.
    pub expires_at: DateTime<Utc>,
    /// Claims embedded in the signed token.
    pub claims: ContactTokenClaims,
}

impl ContactTokenIssuer {
    /// Builds an issuer from configured key material.
    pub fn from_env(config: &ContactTokenConfig) -> Result<Self, ContactTokenError> {
        let private_key = config_secret(
            "MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM",
            &config.private_key_pem,
        )?;
        let public_key = config_secret(
            "MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM",
            &config.public_key_pem,
        )?;
        Self::from_key_pems(config, private_key.as_bytes(), public_key.as_bytes())
    }

    /// Builds an issuer from RSA private and public key PEMs.
    pub fn from_key_pems(
        config: &ContactTokenConfig,
        private_key_pem: &[u8],
        public_key_pem: &[u8],
    ) -> Result<Self, ContactTokenError> {
        let encoding_key = EncodingKey::from_rsa_pem(private_key_pem)
            .map_err(|error| ContactTokenError::InvalidKey(error.to_string()))?;
        let verifier = ContactTokenVerifier::from_public_key_pem(config, public_key_pem)?;
        Ok(Self {
            config: config.clone(),
            encoding_key,
            verifier,
        })
    }

    /// Issues a signed contact token for the provided contact projection.
    pub fn issue(
        &self,
        contact: &ContactRef,
    ) -> Result<(String, DateTime<Utc>), ContactTokenError> {
        let issued = self.issue_with_claims(contact)?;
        Ok((issued.token, issued.expires_at))
    }

    /// Issues a signed contact token and returns the claims used for grant persistence.
    pub fn issue_with_claims(
        &self,
        contact: &ContactRef,
    ) -> Result<IssuedContactToken, ContactTokenError> {
        let now = Utc::now();
        let ttl = token_ttl(&self.config, contact.state);
        let expires_at = now + Duration::seconds(ttl);
        let claims = ContactTokenClaims {
            iss: self.config.issuer.clone(),
            aud: self.config.audience.clone(),
            sub: contact.contact_id.to_string(),
            exp: expires_at.timestamp(),
            iat: now.timestamp(),
            nbf: now.timestamp(),
            jti: Uuid::now_v7().to_string(),
            tenant_id: contact.tenant_id,
            state: contact.state,
            scopes: contact.scopes.clone(),
            permissions: contact.permissions.clone(),
            agent_ids: contact.agent_ids.clone(),
            session_ids: contact.session_ids.clone(),
            verified_contact_point_ids: contact.verified_contact_point_ids.clone(),
            linked_contact_ids: contact.linked_contact_ids.clone(),
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(self.config.key_id.clone());
        let token = jsonwebtoken::encode(&header, &claims, &self.encoding_key)
            .map_err(|error| ContactTokenError::InvalidKey(error.to_string()))?;
        Ok(IssuedContactToken {
            token,
            expires_at,
            claims,
        })
    }

    /// Verifies a token using the matching public key configuration.
    pub fn verify(&self, token: &str) -> Result<ContactTokenClaims, ContactTokenError> {
        self.verifier.verify(token)
    }
}

fn token_ttl(config: &ContactTokenConfig, state: ContactVerificationState) -> i64 {
    if state.is_verified() {
        config.verified_ttl_seconds
    } else {
        config.unverified_ttl_seconds
    }
}

fn config_secret(env_name: &'static str, value: &str) -> Result<String, ContactTokenError> {
    moa_core::config::required_config_secret(env_name, value)
        .map_err(|_| ContactTokenError::MissingEnv(env_name.to_string()))
}
