//! Offline tests for Auth0 Token Vault exchange behavior.

use std::sync::Arc;

use httpmock::{Method::POST, MockServer};
use moa_auth_providers_auth0::Auth0TokenVaultProvider;
use moa_core::traits::{TokenVaultError, TokenVaultProvider};
use secrecy::{ExposeSecret, SecretString};
use serde_json::json;
use uuid::Uuid;

mod support;

#[tokio::test]
async fn vault_get_token_returns_token_on_happy_path() {
    // Pins: linked users are exchanged through Auth0 Token Vault and scopes are split exactly.
    let pool = support::migrated_auth0_pool().await;
    let server = MockServer::start();
    let m2m = server.mock(|when, then| {
        when.method(POST).path("/oauth/token").json_body(json!({
            "grant_type": "client_credentials",
            "client_id": "client-1",
            "client_secret": "secret-1",
            "audience": "https://example.test/api/v2/",
        }));
        then.status(200)
            .json_body(json!({ "access_token": "m2m-token", "expires_in": 3600 }));
    });
    let exchange = server.mock(|when, then| {
        when.method(POST).path("/oauth/token").json_body(json!({
            "grant_type": "urn:auth0:params:oauth:grant-type:token-vault",
            "subject_token": "auth0|user-1",
            "subject_token_type": "urn:auth0:params:oauth:token-type:auth0-user",
            "connection": "github",
        }));
        then.status(200).json_body(json!({
            "access_token": "github-access",
            "expires_in": 900,
            "scope": "repo read:user",
        }));
    });
    let user_id = Uuid::from_u128(0x101);
    let tenant_id = Uuid::from_u128(0x202);
    insert_linked_user(&pool, user_id, tenant_id, "auth0|user-1", "github").await;

    let provider = provider(&server, pool);
    let token = provider
        .get_token(user_id, "github")
        .await
        .expect("linked user token exchange should succeed");

    assert_eq!(token.access_token.expose_secret(), "github-access");
    assert_eq!(
        token.scopes,
        vec!["repo".to_string(), "read:user".to_string()]
    );
    assert!(
        token.expires_at.is_some(),
        "vault response with expires_in should set expires_at"
    );
    m2m.assert_hits(1);
    exchange.assert_hits(1);
}

#[tokio::test]
async fn vault_get_token_returns_not_linked_for_unlinked_user() {
    // Pins: a mapped Auth0 user without the requested linked connection fails before HTTP exchange.
    let pool = support::migrated_auth0_pool().await;
    let server = MockServer::start();
    let user_id = Uuid::from_u128(0x303);
    let tenant_id = Uuid::from_u128(0x404);
    insert_user_map(&pool, user_id, tenant_id, "auth0|user-2").await;

    let error = match provider(&server, pool).get_token(user_id, "github").await {
        Ok(_) => panic!("unlinked user should not receive a token"),
        Err(error) => error,
    };

    assert!(matches!(error, TokenVaultError::NotLinked));
}

#[tokio::test]
async fn vault_caches_m2m_token_between_exchanges() {
    // Pins: repeated token exchanges reuse a still-fresh M2M token in-process.
    let pool = support::migrated_auth0_pool().await;
    let server = MockServer::start();
    let m2m = server.mock(|when, then| {
        when.method(POST).path("/oauth/token").json_body(json!({
            "grant_type": "client_credentials",
            "client_id": "client-1",
            "client_secret": "secret-1",
            "audience": "https://example.test/api/v2/",
        }));
        then.status(200)
            .json_body(json!({ "access_token": "m2m-token", "expires_in": 3600 }));
    });
    let exchange = server.mock(|when, then| {
        when.method(POST).path("/oauth/token").json_body(json!({
            "grant_type": "urn:auth0:params:oauth:grant-type:token-vault",
            "subject_token": "auth0|user-3",
            "subject_token_type": "urn:auth0:params:oauth:token-type:auth0-user",
            "connection": "github",
        }));
        then.status(200).json_body(json!({
            "access_token": "github-access",
            "expires_in": 900,
            "scope": "repo",
        }));
    });
    let user_id = Uuid::from_u128(0x505);
    let tenant_id = Uuid::from_u128(0x606);
    insert_linked_user(&pool, user_id, tenant_id, "auth0|user-3", "github").await;

    let provider = provider(&server, pool);
    let first = provider
        .get_token(user_id, "github")
        .await
        .expect("first exchange should succeed");
    let second = provider
        .get_token(user_id, "github")
        .await
        .expect("second exchange should succeed");

    assert_eq!(first.access_token.expose_secret(), "github-access");
    assert_eq!(second.access_token.expose_secret(), "github-access");
    m2m.assert_hits(1);
    exchange.assert_hits(2);
}

fn provider(server: &MockServer, pool: sqlx::PgPool) -> Auth0TokenVaultProvider {
    Auth0TokenVaultProvider::new_with_base_url(
        server.base_url(),
        "client-1".to_string(),
        SecretString::new("secret-1".to_string().into_boxed_str()),
        "https://example.test/api/v2/".to_string(),
        Arc::new(pool),
    )
    .expect("test provider should build")
}

async fn insert_linked_user(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    tenant_id: Uuid,
    sub: &str,
    connection: &str,
) {
    insert_user_map(pool, user_id, tenant_id, sub).await;
    sqlx::query(
        r#"
        INSERT INTO linked_connections (user_id, connection_name, scopes_granted, external_sub)
        VALUES ($1, $2, $3, $4)
        "#,
    )
    .bind(user_id)
    .bind(connection)
    .bind(Vec::<String>::new())
    .bind("external-1")
    .execute(pool)
    .await
    .expect("insert linked connection");
}

async fn insert_user_map(pool: &sqlx::PgPool, user_id: Uuid, tenant_id: Uuid, sub: &str) {
    sqlx::query(
        r#"
        INSERT INTO auth0_user_map (sub, tenant_id, user_id)
        VALUES ($1, $2, $3)
        "#,
    )
    .bind(sub)
    .bind(tenant_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("insert auth0 user map");
}
