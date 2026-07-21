//! DB-backed authentication for MOA-issued OAuth 2.1 access tokens.

use moa_auth_providers::OAuthAccessTokenProvider;
use moa_auth_providers::oauth_as::{
    AuthorizationDecision, AuthorizationRequest, AuthorizationSubject, CodeExchangeRequest,
    OAuthServer,
};
use moa_core::config::{OAuthClientConfig, OAuthClientType, OAuthServerConfig};
use moa_core::traits::{AuthError, IdentityType};
use moa_core::types::identifiers::TenantId;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::support::TestDatabase;

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const REDIRECT_URI: &str = "https://app.example/callback";
const RESOURCE: &str = "https://moa.test/mcp";
const CLIENT_ID: &str = "test-client";

fn server_config() -> OAuthServerConfig {
    OAuthServerConfig {
        issuer: "https://moa.test".to_string(),
        resource: RESOURCE.to_string(),
        authorization_request_ttl_seconds: 300,
        authorization_code_ttl_seconds: 60,
        access_token_ttl_seconds: 3600,
        refresh_token_ttl_seconds: 7200,
        clients: vec![OAuthClientConfig {
            client_id: CLIENT_ID.to_string(),
            client_type: OAuthClientType::Public,
            redirect_uris: vec![REDIRECT_URI.to_string()],
            scopes: vec!["mcp:read".to_string(), "mcp:write".to_string()],
            client_secret_sha256: None,
        }],
    }
}

fn authorization_request() -> AuthorizationRequest<'static> {
    AuthorizationRequest {
        response_type: "code",
        client_id: CLIENT_ID,
        redirect_uri: REDIRECT_URI,
        scopes: vec!["mcp:read".to_string()],
        resource: RESOURCE,
        state: None,
        code_challenge: CHALLENGE,
        code_challenge_method: "S256",
    }
}

async fn issue_access_token(server: &OAuthServer, subject: &AuthorizationSubject) -> SecretString {
    let pending = server
        .begin_authorization(&authorization_request(), subject)
        .await
        .expect("begin consent");
    let code = server
        .complete_authorization(
            pending.request_id,
            &pending.csrf_token,
            subject,
            AuthorizationDecision::Approve,
        )
        .await
        .expect("approve consent")
        .code
        .expect("approval issues a code")
        .code;
    let client = server
        .client(CLIENT_ID)
        .await
        .expect("client lookup")
        .expect("client exists");
    server
        .exchange_authorization_code(
            &client,
            None,
            &CodeExchangeRequest {
                code: &code,
                redirect_uri: REDIRECT_URI,
                resource: RESOURCE,
                code_verifier: VERIFIER,
            },
        )
        .await
        .expect("exchange authorization code")
        .access_token
}

#[tokio::test]
async fn oauth_access_token_resolves_identity_delegation_and_resource_once_db() {
    // Pins: one provider lookup returns the identity and the complete OAuth
    // delegation used by MCP scope and resource authorization.
    let database = TestDatabase::new("moa_oauth_at_auth_test").await;
    let server = OAuthServer::from_config(&server_config(), database.pool())
        .await
        .expect("build oauth server");
    let provider = OAuthAccessTokenProvider::new(database.independent_pool().await);
    let subject = AuthorizationSubject {
        subject_id: Uuid::new_v4(),
        subject_type: "operator".to_string(),
        tenant_id: TenantId::new(),
    };
    let access_token = issue_access_token(&server, &subject).await;

    let principal = provider
        .authenticate(access_token.expose_secret())
        .await
        .expect("active access token authenticates");

    assert_eq!(principal.identity.identity_type, IdentityType::Operator);
    assert_eq!(principal.identity.id, subject.subject_id);
    assert_eq!(principal.identity.tenant_id, subject.tenant_id);
    assert_eq!(principal.identity.api_key_id, None);
    assert_eq!(principal.identity.acting_on_behalf_of, None);
    let delegation = principal.oauth.expect("oauth delegation is present");
    assert_eq!(delegation.client_id, CLIENT_ID);
    assert_eq!(delegation.scopes, vec!["mcp:read".to_string()]);
    assert_eq!(delegation.resource, RESOURCE);
}

#[tokio::test]
async fn oauth_revoked_expired_and_unknown_access_tokens_fail_closed_db() {
    // Pins: token lifecycle state is authoritative in Postgres and every
    // inactive or absent access token fails closed through the same lookup.
    let database = TestDatabase::new("moa_oauth_at_auth_test").await;
    let server = OAuthServer::from_config(&server_config(), database.pool())
        .await
        .expect("build oauth server");
    let provider = OAuthAccessTokenProvider::new(database.independent_pool().await);
    let subject = AuthorizationSubject {
        subject_id: Uuid::new_v4(),
        subject_type: "agent".to_string(),
        tenant_id: TenantId::new(),
    };

    let revoked = issue_access_token(&server, &subject).await;
    let client = server
        .client(CLIENT_ID)
        .await
        .expect("client lookup")
        .expect("client exists");
    server
        .revoke(&client, None, &revoked)
        .await
        .expect("revoke access token");
    let error = provider
        .authenticate(revoked.expose_secret())
        .await
        .expect_err("revoked token is rejected");
    assert!(matches!(error, AuthError::Rejected));

    let expired = issue_access_token(&server, &subject).await;
    sqlx::query(
        "UPDATE oauth_tokens SET access_token_expires_at = NOW() - INTERVAL '1 second' \
         WHERE access_token_hash = $1",
    )
    .bind(hex::encode(Sha256::digest(
        expired.expose_secret().as_bytes(),
    )))
    .execute(database.raw_pool())
    .await
    .expect("expire access token");
    let error = provider
        .authenticate(expired.expose_secret())
        .await
        .expect_err("expired token is rejected");
    assert!(matches!(error, AuthError::Expired));

    let error = provider
        .authenticate("moa_oauth_at_never_issued_value")
        .await
        .expect_err("unknown token is rejected");
    assert!(matches!(error, AuthError::Rejected));
}
