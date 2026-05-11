//! Live Auth0 Token Vault tests.
//!
//! Requires `MOA_RUN_LIVE_AUTH0_TESTS=1` plus:
//! `MOA_TEST_AUTH0_DOMAIN`, `MOA_TEST_AUTH0_CLIENT_ID`, `MOA_TEST_AUTH0_CLIENT_SECRET`,
//! `MOA_TEST_AUTH0_USER_ID`, `MOA_TEST_AUTH0_TENANT_ID`,
//! `MOA_TEST_AUTH0_SUB`, and `MOA_TEST_AUTH0_CONNECTION`.

use std::sync::Arc;

use moa_auth_providers_auth0::Auth0TokenVaultProvider;
use moa_core::traits::TokenVaultProvider;
use secrecy::{ExposeSecret, SecretString};
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires MOA_RUN_LIVE_AUTH0_TESTS=1 and a real Auth0 linked user"]
async fn live_auth0_token_vault_returns_third_party_token(pool: sqlx::PgPool) {
    // Pins: a real Auth0-linked user can exchange linked connection metadata for a provider token.
    if std::env::var("MOA_RUN_LIVE_AUTH0_TESTS").as_deref() != Ok("1") {
        return;
    }
    let domain = required_env("MOA_TEST_AUTH0_DOMAIN");
    let client_id = required_env("MOA_TEST_AUTH0_CLIENT_ID");
    let client_secret = required_env("MOA_TEST_AUTH0_CLIENT_SECRET");
    let user_id = required_uuid("MOA_TEST_AUTH0_USER_ID");
    let tenant_id = required_uuid("MOA_TEST_AUTH0_TENANT_ID");
    let sub = required_env("MOA_TEST_AUTH0_SUB");
    let connection = required_env("MOA_TEST_AUTH0_CONNECTION");
    let management_audience = std::env::var("MOA_TEST_AUTH0_MANAGEMENT_AUDIENCE")
        .unwrap_or_else(|_| format!("https://{}/api/v2/", domain.trim_end_matches('/')));

    upsert_linked_user(&pool, user_id, tenant_id, &sub, &connection).await;

    let provider = Auth0TokenVaultProvider::new(
        domain,
        client_id,
        SecretString::new(client_secret.into_boxed_str()),
        management_audience,
        Arc::new(pool),
    )
    .expect("build Auth0 Token Vault provider");
    let token = provider
        .get_token(user_id, &connection)
        .await
        .expect("live Auth0 Token Vault exchange should succeed");

    assert!(
        !token.access_token.expose_secret().trim().is_empty(),
        "live token should not be empty"
    );
    assert!(
        token
            .expires_at
            .is_some_and(|expires_at| expires_at > chrono::Utc::now()),
        "live token should expire in the future"
    );
    assert!(
        !token.scopes.is_empty(),
        "live token should report at least one granted scope"
    );
}

async fn upsert_linked_user(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    tenant_id: Uuid,
    sub: &str,
    connection: &str,
) {
    sqlx::query(
        r#"
        INSERT INTO auth0_user_map (sub, tenant_id, user_id)
        VALUES ($1, $2, $3)
        ON CONFLICT (sub, tenant_id)
        DO UPDATE SET user_id = EXCLUDED.user_id
        "#,
    )
    .bind(sub)
    .bind(tenant_id)
    .bind(user_id)
    .execute(pool)
    .await
    .expect("upsert auth0_user_map");
    sqlx::query(
        r#"
        INSERT INTO linked_connections (user_id, connection_name)
        VALUES ($1, $2)
        ON CONFLICT (user_id, connection_name) DO NOTHING
        "#,
    )
    .bind(user_id)
    .bind(connection)
    .execute(pool)
    .await
    .expect("upsert linked_connections");
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is required when MOA_RUN_LIVE_AUTH0_TESTS=1"))
}

fn required_uuid(name: &str) -> Uuid {
    Uuid::parse_str(&required_env(name))
        .unwrap_or_else(|error| panic!("{name} must be a UUID: {error}"))
}
