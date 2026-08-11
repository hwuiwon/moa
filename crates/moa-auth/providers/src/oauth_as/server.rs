//! Database-backed OAuth 2.1 authorization-server protocol.

use std::sync::Arc;

use chrono::{Duration, Utc};
use moa_config::OAuthServerConfig;
use moa_core::types::identifiers::TenantId;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use super::client::{OAuthClient, OAuthClientRegistry};
use super::pkce::{self, CodeChallengeMethod};
use super::store::{
    AuthorizationDecisionSubject, ExchangedTokens, IntrospectionRow, NewAuthorizationTransaction,
    OAuthStore, RotatedTokens,
};
use super::{
    ACCESS_TOKEN_PREFIX, AUTHORIZATION_CODE_PREFIX, CONSENT_CSRF_PREFIX, OAuthError,
    REFRESH_TOKEN_PREFIX, digest_hex, generate_opaque_token,
};

/// A validated authorization request.
pub struct AuthorizationRequest<'a> {
    /// Must be `code`.
    pub response_type: &'a str,
    /// Requesting client id.
    pub client_id: &'a str,
    /// Exact registered callback URI.
    pub redirect_uri: &'a str,
    /// Requested MCP scopes.
    pub scopes: Vec<String>,
    /// Exact RFC 8707 protected resource.
    pub resource: &'a str,
    /// Opaque client callback state.
    pub state: Option<&'a str>,
    /// PKCE challenge.
    pub code_challenge: &'a str,
    /// Must be `S256`.
    pub code_challenge_method: &'a str,
}

/// Authenticated resource owner for authorization consent.
pub struct AuthorizationSubject {
    /// Resource-owner subject id.
    pub subject_id: Uuid,
    /// Resource-owner subject type.
    pub subject_type: String,
    /// Resource-owner tenant.
    pub tenant_id: TenantId,
}

/// Durable consent transaction rendered by GET `/oauth/authorize`.
pub struct PendingAuthorization {
    /// Stable transaction identifier submitted by the consent form.
    pub request_id: Uuid,
    /// CSRF value submitted by the consent form.
    pub csrf_token: SecretString,
    /// Requesting client id.
    pub client_id: String,
    /// Exact requested scopes.
    pub scopes: Vec<String>,
    /// Exact requested resource.
    pub resource: String,
}

/// Explicit resource-owner consent decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationDecision {
    /// Approve and issue one authorization code.
    Approve,
    /// Deny without issuing a code.
    Deny,
}

/// Result of a completed consent transaction.
pub struct AuthorizationOutcome {
    /// Validated callback URI stored by GET.
    pub redirect_uri: String,
    /// Opaque callback state stored by GET.
    pub state: Option<String>,
    /// Code present only for an approved decision.
    pub code: Option<IssuedAuthorizationCode>,
}

/// A freshly issued authorization code.
pub struct IssuedAuthorizationCode {
    /// Plaintext code returned exactly once.
    pub code: SecretString,
}

/// Authorization-code token request.
pub struct CodeExchangeRequest<'a> {
    /// Presented authorization code.
    pub code: &'a SecretString,
    /// Exact callback URI.
    pub redirect_uri: &'a str,
    /// Exact RFC 8707 resource.
    pub resource: &'a str,
    /// PKCE verifier.
    pub code_verifier: &'a str,
}

/// Issued token material returned exactly once.
pub struct TokenGrant {
    /// Opaque access token.
    pub access_token: SecretString,
    /// Opaque refresh token.
    pub refresh_token: SecretString,
    /// Always `Bearer`.
    pub token_type: &'static str,
    /// Access-token lifetime in seconds.
    pub expires_in: i64,
    /// Granted scopes.
    pub scopes: Vec<String>,
    /// Exact protected resource.
    pub resource: String,
}

/// RFC 7662 token introspection response.
#[derive(Debug, Serialize)]
pub struct IntrospectionResponse {
    /// Whether the token is active for the authenticated issuing client.
    pub active: bool,
    /// Space-delimited scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Issuing client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Bearer for access tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    /// Expiry epoch seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
    /// Resource-owner subject.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    /// Authorization-server issuer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    /// Exact protected-resource audience.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    /// MOA subject type.
    #[serde(rename = "moa_subject_type", skip_serializing_if = "Option::is_none")]
    pub subject_type: Option<String>,
    /// MOA tenant id.
    #[serde(rename = "moa_tenant_id", skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
}

impl IntrospectionResponse {
    /// Return the metadata-free inactive response.
    #[must_use]
    pub fn inactive() -> Self {
        Self {
            active: false,
            scope: None,
            client_id: None,
            token_type: None,
            exp: None,
            sub: None,
            iss: None,
            aud: None,
            subject_type: None,
            tenant_id: None,
        }
    }
}

/// First-party OAuth server backed by one shared Postgres protocol store.
pub struct OAuthServer {
    store: OAuthStore,
    issuer: String,
    resource: String,
    request_ttl: Duration,
    code_ttl: Duration,
    access_ttl: Duration,
    refresh_ttl: Duration,
}

impl OAuthServer {
    /// Validate config, converge startup clients, and build a server.
    pub async fn from_config(
        config: &OAuthServerConfig,
        pool: Arc<PgPool>,
    ) -> Result<Self, OAuthError> {
        config
            .validate()
            .map_err(OAuthError::InvalidClientConfiguration)?;
        let registry = OAuthClientRegistry::from_configs(&config.clients)?;
        let store = OAuthStore::new(pool);
        store.bootstrap_clients(&registry).await?;
        Ok(Self {
            store,
            issuer: config.issuer.trim_end_matches('/').to_string(),
            resource: config.resource.clone(),
            request_ttl: Duration::seconds(config.authorization_request_ttl_seconds),
            code_ttl: Duration::seconds(config.authorization_code_ttl_seconds),
            access_ttl: Duration::seconds(config.access_token_ttl_seconds),
            refresh_ttl: Duration::seconds(config.refresh_token_ttl_seconds),
        })
    }

    /// Resolve one authoritative client row.
    pub async fn client(&self, client_id: &str) -> Result<Option<OAuthClient>, OAuthError> {
        self.store.client(client_id).await
    }

    /// Resolve a client and validate its callback URI.
    pub async fn resolve_for_authorization(
        &self,
        client_id: &str,
        redirect_uri: &str,
    ) -> Result<OAuthClient, OAuthError> {
        let client = self
            .client(client_id)
            .await?
            .ok_or(OAuthError::InvalidClient)?;
        if !client.allows_redirect(redirect_uri) {
            return Err(OAuthError::InvalidRedirectUri);
        }
        Ok(client)
    }

    /// Persist a consent transaction without issuing an authorization code.
    pub async fn begin_authorization(
        &self,
        request: &AuthorizationRequest<'_>,
        subject: &AuthorizationSubject,
    ) -> Result<PendingAuthorization, OAuthError> {
        let client = self
            .resolve_for_authorization(request.client_id, request.redirect_uri)
            .await?;
        if request.response_type != "code" {
            return Err(OAuthError::UnsupportedResponseType);
        }
        if request.resource != self.resource {
            return Err(OAuthError::InvalidTarget);
        }
        let method = CodeChallengeMethod::parse(request.code_challenge_method).ok_or(
            OAuthError::InvalidRequest("code_challenge_method must be S256"),
        )?;
        if method != CodeChallengeMethod::S256
            || !pkce::is_valid_code_challenge(request.code_challenge)
        {
            return Err(OAuthError::InvalidRequest("malformed code_challenge"));
        }
        let mut scopes = request.scopes.clone();
        scopes.sort();
        scopes.dedup();
        if !client.allows_scopes(&scopes) {
            return Err(OAuthError::InvalidScope);
        }

        let request_id = Uuid::now_v7();
        let csrf_token = generate_opaque_token(CONSENT_CSRF_PREFIX);
        let csrf_hash = digest_hex(csrf_token.expose_secret());
        self.store
            .insert_authorization_transaction(NewAuthorizationTransaction {
                id: request_id,
                tenant_id: subject.tenant_id,
                client_id: &client.client_id,
                subject_id: subject.subject_id,
                subject_type: &subject.subject_type,
                redirect_uri: request.redirect_uri,
                scopes: &scopes,
                resource: request.resource,
                state: request.state,
                code_challenge: request.code_challenge,
                code_challenge_method: method.as_str(),
                csrf_hash: &csrf_hash,
                expires_at: Utc::now() + self.request_ttl,
            })
            .await?;
        Ok(PendingAuthorization {
            request_id,
            csrf_token,
            client_id: client.client_id,
            scopes,
            resource: request.resource.to_string(),
        })
    }

    /// Approve or deny one durable authorization transaction exactly once.
    pub async fn complete_authorization(
        &self,
        request_id: Uuid,
        csrf_token: &SecretString,
        subject: &AuthorizationSubject,
        decision: AuthorizationDecision,
    ) -> Result<AuthorizationOutcome, OAuthError> {
        let code = matches!(decision, AuthorizationDecision::Approve)
            .then(|| generate_opaque_token(AUTHORIZATION_CODE_PREFIX));
        let code_hash = code.as_ref().map(|code| digest_hex(code.expose_secret()));
        let record = self
            .store
            .decide_authorization_transaction(
                request_id,
                AuthorizationDecisionSubject {
                    tenant_id: subject.tenant_id,
                    subject_id: subject.subject_id,
                    subject_type: &subject.subject_type,
                },
                &digest_hex(csrf_token.expose_secret()),
                code.is_some(),
                code_hash.as_deref(),
                Utc::now() + self.code_ttl,
            )
            .await?;
        Ok(AuthorizationOutcome {
            redirect_uri: record.redirect_uri,
            state: record.state,
            code: code.map(|code| IssuedAuthorizationCode { code }),
        })
    }

    /// Atomically validate a code and insert its resource-bound token grant.
    pub async fn exchange_authorization_code(
        &self,
        client: &OAuthClient,
        client_secret: Option<&SecretString>,
        request: &CodeExchangeRequest<'_>,
    ) -> Result<TokenGrant, OAuthError> {
        if !client.authenticate(client_secret) {
            return Err(OAuthError::InvalidClientCredentials);
        }
        if request.resource != self.resource {
            return Err(OAuthError::InvalidTarget);
        }
        let now = Utc::now();
        let access = generate_opaque_token(ACCESS_TOKEN_PREFIX);
        let refresh = generate_opaque_token(REFRESH_TOKEN_PREFIX);
        let access_hash = digest_hex(access.expose_secret());
        let refresh_hash = digest_hex(refresh.expose_secret());
        let grant = self
            .store
            .exchange_authorization_code(
                &digest_hex(request.code.expose_secret()),
                &client.client_id,
                request.redirect_uri,
                request.resource,
                request.code_verifier,
                ExchangedTokens {
                    access_token_hash: &access_hash,
                    access_token_expires_at: now + self.access_ttl,
                    refresh_token_hash: &refresh_hash,
                    refresh_token_expires_at: now + self.refresh_ttl,
                },
            )
            .await?
            .ok_or(OAuthError::InvalidGrant)?;
        Ok(TokenGrant {
            access_token: access,
            refresh_token: refresh,
            token_type: "Bearer",
            expires_in: self.access_ttl.num_seconds(),
            scopes: grant.scopes,
            resource: grant.resource,
        })
    }

    /// Rotate a refresh token while preserving scopes and exact resource.
    pub async fn refresh_token_grant(
        &self,
        client: &OAuthClient,
        client_secret: Option<&SecretString>,
        refresh_token: &SecretString,
        resource: &str,
    ) -> Result<TokenGrant, OAuthError> {
        if !client.authenticate(client_secret) {
            return Err(OAuthError::InvalidClientCredentials);
        }
        if resource != self.resource {
            return Err(OAuthError::InvalidTarget);
        }
        let now = Utc::now();
        let access = generate_opaque_token(ACCESS_TOKEN_PREFIX);
        let refresh = generate_opaque_token(REFRESH_TOKEN_PREFIX);
        let access_hash = digest_hex(access.expose_secret());
        let refresh_hash = digest_hex(refresh.expose_secret());
        let rotated = self
            .store
            .rotate_refresh_token(
                &digest_hex(refresh_token.expose_secret()),
                &client.client_id,
                RotatedTokens {
                    access_token_hash: &access_hash,
                    access_token_expires_at: now + self.access_ttl,
                    refresh_token_hash: &refresh_hash,
                    refresh_token_expires_at: now + self.refresh_ttl,
                },
            )
            .await?
            .ok_or(OAuthError::InvalidGrant)?;
        Ok(TokenGrant {
            access_token: access,
            refresh_token: refresh,
            token_type: "Bearer",
            expires_in: self.access_ttl.num_seconds(),
            scopes: rotated.scopes,
            resource: rotated.resource,
        })
    }

    /// Introspect only a token issued to the authenticated confidential client.
    pub async fn introspect(
        &self,
        client: &OAuthClient,
        client_secret: Option<&SecretString>,
        token: &SecretString,
    ) -> Result<IntrospectionResponse, OAuthError> {
        if !client.is_confidential() || !client.authenticate(client_secret) {
            return Err(OAuthError::InvalidClientCredentials);
        }
        let token_hash = digest_hex(token.expose_secret());
        let Some(row) = self
            .store
            .find_active_for_introspection(&token_hash, &client.client_id)
            .await?
        else {
            return Ok(IntrospectionResponse::inactive());
        };
        Ok(self.introspection_from_row(&token_hash, row))
    }

    /// Revoke only a token issued to the authenticated client.
    pub async fn revoke(
        &self,
        client: &OAuthClient,
        client_secret: Option<&SecretString>,
        token: &SecretString,
    ) -> Result<(), OAuthError> {
        if !client.authenticate(client_secret) {
            return Err(OAuthError::InvalidClientCredentials);
        }
        self.store
            .revoke_token(&digest_hex(token.expose_secret()), &client.client_id)
            .await
    }

    /// Canonical authorization-server issuer.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Exact protected resource accepted by authorization and token endpoints.
    #[must_use]
    pub fn resource(&self) -> &str {
        &self.resource
    }

    fn introspection_from_row(
        &self,
        token_hash: &str,
        row: IntrospectionRow,
    ) -> IntrospectionResponse {
        let (token_type, expiry) = if row.access_token_hash == token_hash {
            (Some("Bearer".to_string()), row.access_token_expires_at)
        } else {
            (None, row.refresh_token_expires_at)
        };
        if expiry <= Utc::now() {
            return IntrospectionResponse::inactive();
        }
        IntrospectionResponse {
            active: true,
            scope: Some(row.scopes.join(" ")),
            client_id: Some(row.client_id),
            token_type,
            exp: Some(expiry.timestamp()),
            sub: Some(row.subject_id.to_string()),
            iss: Some(self.issuer.clone()),
            aud: Some(row.resource),
            subject_type: Some(row.subject_type),
            tenant_id: Some(row.tenant_id.to_string()),
        }
    }
}
