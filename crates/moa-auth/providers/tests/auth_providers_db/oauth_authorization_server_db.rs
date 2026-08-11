//! DB-backed cross-replica OAuth authorization-server protocol tests.

use moa_auth_providers::oauth_as::{
    AuthorizationDecision, AuthorizationRequest, AuthorizationSubject, CodeExchangeRequest,
    OAuthError, OAuthServer,
};
use moa_config::{OAuthClientConfig, OAuthClientType, OAuthServerConfig};
use moa_core::types::identifiers::TenantId;
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::support::TestDatabase;

const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
const WRONG_VERIFIER: &str = "aBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const REDIRECT_URI: &str = "https://app.example/callback";
const RESOURCE: &str = "https://moa.test/mcp";
const CLIENT_ID: &str = "test-client";
const CLIENT_SECRET: &str = "test-client-secret-with-high-entropy";

fn server_config() -> OAuthServerConfig {
    OAuthServerConfig {
        issuer: "https://moa.test".to_string(),
        resource: RESOURCE.to_string(),
        authorization_request_ttl_seconds: 300,
        authorization_code_ttl_seconds: 60,
        access_token_ttl_seconds: 3600,
        refresh_token_ttl_seconds: 7200,
        clients: vec![
            client_config(CLIENT_ID, CLIENT_SECRET),
            client_config("other-client", "other-secret-with-high-entropy"),
        ],
    }
}

fn client_config(client_id: &str, secret: &str) -> OAuthClientConfig {
    OAuthClientConfig {
        client_id: client_id.to_string(),
        client_type: OAuthClientType::Confidential,
        redirect_uris: vec![REDIRECT_URI.to_string()],
        scopes: vec!["mcp:read".to_string(), "mcp:write".to_string()],
        client_secret_sha256: Some(hex::encode(Sha256::digest(secret.as_bytes()))),
    }
}

fn authorization_request() -> AuthorizationRequest<'static> {
    AuthorizationRequest {
        response_type: "code",
        client_id: CLIENT_ID,
        redirect_uri: REDIRECT_URI,
        scopes: vec!["mcp:read".to_string()],
        resource: RESOURCE,
        state: Some("client-state"),
        code_challenge: CHALLENGE,
        code_challenge_method: "S256",
    }
}

fn subject() -> AuthorizationSubject {
    AuthorizationSubject {
        subject_id: Uuid::new_v4(),
        subject_type: "operator".to_string(),
        tenant_id: TenantId::new(),
    }
}

async fn approved_code(
    begin_server: &OAuthServer,
    decision_server: &OAuthServer,
    subject: &AuthorizationSubject,
) -> SecretString {
    let pending = begin_server
        .begin_authorization(&authorization_request(), subject)
        .await
        .expect("begin durable consent");
    let outcome = decision_server
        .complete_authorization(
            pending.request_id,
            &pending.csrf_token,
            subject,
            AuthorizationDecision::Approve,
        )
        .await
        .expect("approve durable consent");
    assert_eq!(outcome.redirect_uri, REDIRECT_URI);
    assert_eq!(outcome.state.as_deref(), Some("client-state"));
    outcome.code.expect("approval issues code").code
}

async fn exchange(
    server: &OAuthServer,
    code: &SecretString,
    verifier: &str,
) -> Result<moa_auth_providers::TokenGrant, OAuthError> {
    let client = server
        .client(CLIENT_ID)
        .await?
        .ok_or(OAuthError::InvalidClient)?;
    server
        .exchange_authorization_code(
            &client,
            Some(&SecretString::from(CLIENT_SECRET)),
            &CodeExchangeRequest {
                code,
                redirect_uri: REDIRECT_URI,
                resource: RESOURCE,
                code_verifier: verifier,
            },
        )
        .await
}

#[tokio::test]
async fn oauth_client_bootstrap_converges_and_conflicts_across_pools_db() {
    // Pins: identical replicas converge while a conflicting declaration fails
    // startup and cannot become last-pod-wins state.
    let database = TestDatabase::new("moa_oauth_as_test").await;
    let first_pool = database.pool();
    let second_pool = database.independent_pool().await;
    let config = server_config();
    let (first, second) = tokio::join!(
        OAuthServer::from_config(&config, first_pool),
        OAuthServer::from_config(&config, second_pool),
    );
    first.expect("first identical bootstrap succeeds");
    second.expect("second identical bootstrap succeeds");

    let mut conflicting = config;
    conflicting.clients[0].scopes = vec!["mcp:write".to_string()];
    let error =
        match OAuthServer::from_config(&conflicting, database.independent_pool().await).await {
            Ok(_) => panic!("conflicting client declaration must fail startup"),
            Err(error) => error,
        };
    assert!(matches!(error, OAuthError::ClientBootstrapConflict(client) if client == CLIENT_ID));
}

#[tokio::test]
async fn oauth_consent_exchange_refresh_and_introspection_cross_replicas_db() {
    // Pins: GET-state on one replica and POST decision on another issues one
    // resource-bound grant whose refresh and introspection stay client-scoped.
    let database = TestDatabase::new("moa_oauth_as_test").await;
    let first = OAuthServer::from_config(&server_config(), database.pool())
        .await
        .expect("first server");
    let second = OAuthServer::from_config(&server_config(), database.independent_pool().await)
        .await
        .expect("second server");
    let owner = subject();
    let code = approved_code(&first, &second, &owner).await;
    let grant = exchange(&first, &code, VERIFIER)
        .await
        .expect("exchange approved code");
    assert_eq!(grant.scopes, vec!["mcp:read".to_string()]);
    assert_eq!(grant.resource, RESOURCE);

    let client = second
        .client(CLIENT_ID)
        .await
        .expect("client lookup")
        .expect("client exists");
    let secret = SecretString::from(CLIENT_SECRET);
    let active = second
        .introspect(&client, Some(&secret), &grant.access_token)
        .await
        .expect("issuing client introspects");
    assert!(active.active);
    assert_eq!(active.aud.as_deref(), Some(RESOURCE));

    let other = second
        .client("other-client")
        .await
        .expect("other lookup")
        .expect("other exists");
    let inactive = second
        .introspect(
            &other,
            Some(&SecretString::from("other-secret-with-high-entropy")),
            &grant.access_token,
        )
        .await
        .expect("other client receives inactive response");
    assert!(!inactive.active);
    second
        .revoke(
            &other,
            Some(&SecretString::from("other-secret-with-high-entropy")),
            &grant.access_token,
        )
        .await
        .expect("other client revocation is an idempotent no-op");
    assert!(
        second
            .introspect(&client, Some(&secret), &grant.access_token)
            .await
            .expect("issuing client introspects after foreign revoke")
            .active,
        "a different client cannot revoke the grant"
    );

    let wrong_target = first
        .refresh_token_grant(
            &client,
            Some(&secret),
            &grant.refresh_token,
            "https://other.example/mcp",
        )
        .await;
    assert!(matches!(wrong_target, Err(OAuthError::InvalidTarget)));
    let rotated = first
        .refresh_token_grant(&client, Some(&secret), &grant.refresh_token, RESOURCE)
        .await
        .expect("rotate refresh token");
    assert_eq!(rotated.resource, RESOURCE);
    assert_eq!(rotated.scopes, vec!["mcp:read".to_string()]);
    first
        .revoke(&client, Some(&secret), &rotated.access_token)
        .await
        .expect("issuing client revokes rotated grant");
    assert!(
        !second
            .introspect(&client, Some(&secret), &rotated.access_token)
            .await
            .expect("introspect revoked rotated grant")
            .active
    );
}

#[tokio::test]
async fn oauth_concurrent_exchange_across_independent_pools_issues_one_grant_db() {
    // Pins: two pods racing one code produce exactly one durable token grant.
    let database = TestDatabase::new("moa_oauth_as_test").await;
    let first = OAuthServer::from_config(&server_config(), database.pool())
        .await
        .expect("first server");
    let second = OAuthServer::from_config(&server_config(), database.independent_pool().await)
        .await
        .expect("second server");
    let owner = subject();
    let code = approved_code(&first, &second, &owner).await;
    let (left, right) = tokio::join!(
        exchange(&first, &code, VERIFIER),
        exchange(&second, &code, VERIFIER)
    );
    assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
    assert_eq!(
        usize::from(matches!(left, Err(OAuthError::InvalidGrant)))
            + usize::from(matches!(right, Err(OAuthError::InvalidGrant))),
        1
    );
}

#[tokio::test]
async fn oauth_failed_validation_and_token_insert_do_not_burn_code_db() {
    // Pins: validation and DB failures roll back before code consumption.
    let database = TestDatabase::new("moa_oauth_as_test").await;
    let server = OAuthServer::from_config(&server_config(), database.pool())
        .await
        .expect("server");
    let owner = subject();
    let code = approved_code(&server, &server, &owner).await;
    assert!(matches!(
        exchange(&server, &code, WRONG_VERIFIER).await,
        Err(OAuthError::InvalidGrant)
    ));
    exchange(&server, &code, VERIFIER)
        .await
        .expect("correct verifier still exchanges after failed validation");

    let second_code = approved_code(&server, &server, &owner).await;
    sqlx::query(
        r#"
        CREATE FUNCTION fail_oauth_token_insert() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN RAISE EXCEPTION 'injected token insert failure'; END
        $$
        "#,
    )
    .execute(database.raw_pool())
    .await
    .expect("install failure function");
    sqlx::query(
        r#"
        CREATE TRIGGER fail_oauth_token_insert
        BEFORE INSERT ON oauth_tokens
        FOR EACH ROW EXECUTE FUNCTION fail_oauth_token_insert()
        "#,
    )
    .execute(database.raw_pool())
    .await
    .expect("install failure trigger");
    assert!(matches!(
        exchange(&server, &second_code, VERIFIER).await,
        Err(OAuthError::Storage(_))
    ));
    sqlx::query("DROP TRIGGER fail_oauth_token_insert ON oauth_tokens")
        .execute(database.raw_pool())
        .await
        .expect("remove failure trigger");
    sqlx::query("DROP FUNCTION fail_oauth_token_insert()")
        .execute(database.raw_pool())
        .await
        .expect("remove failure function");
    exchange(&server, &second_code, VERIFIER)
        .await
        .expect("code remains valid after insert rollback");
}

#[tokio::test]
async fn oauth_consent_csrf_and_decision_are_owner_bound_and_one_time_db() {
    // Pins: a bad CSRF value does not decide the request and two replicas cannot
    // approve or deny the same transaction twice.
    let database = TestDatabase::new("moa_oauth_as_test").await;
    let first = OAuthServer::from_config(&server_config(), database.pool())
        .await
        .expect("first server");
    let second = OAuthServer::from_config(&server_config(), database.independent_pool().await)
        .await
        .expect("second server");
    let owner = subject();
    let pending = first
        .begin_authorization(&authorization_request(), &owner)
        .await
        .expect("begin consent");
    let impostor = AuthorizationSubject {
        subject_id: Uuid::new_v4(),
        subject_type: owner.subject_type.clone(),
        tenant_id: owner.tenant_id,
    };
    let wrong_owner = second
        .complete_authorization(
            pending.request_id,
            &pending.csrf_token,
            &impostor,
            AuthorizationDecision::Approve,
        )
        .await;
    assert!(matches!(wrong_owner, Err(OAuthError::InvalidGrant)));
    let bad = second
        .complete_authorization(
            pending.request_id,
            &SecretString::from("wrong-csrf"),
            &owner,
            AuthorizationDecision::Approve,
        )
        .await;
    assert!(matches!(bad, Err(OAuthError::InvalidGrant)));
    first
        .complete_authorization(
            pending.request_id,
            &pending.csrf_token,
            &owner,
            AuthorizationDecision::Deny,
        )
        .await
        .expect("valid denial succeeds");
    let replay = second
        .complete_authorization(
            pending.request_id,
            &pending.csrf_token,
            &owner,
            AuthorizationDecision::Approve,
        )
        .await;
    assert!(matches!(
        replay,
        Err(OAuthError::AuthorizationAlreadyDecided)
    ));
}
