//! Live Auth0 validation tests gated behind `MOA_RUN_LIVE_AUTH0_TESTS=1`.

use std::sync::Arc;

use moa_auth_providers_auth0::Auth0AuthProvider;
use moa_core::traits::{AuthError, AuthProvider, Credential, IdentityType};
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires MOA_RUN_LIVE_AUTH0_TESTS=1 and an Auth0 tenant"]
async fn auth0_authenticate_valid_token_returns_identity(pool: sqlx::PgPool) {
    // Pins: a real Auth0 access token with MOA namespaced claims resolves to a MOA user identity.
    let Some(env) = live_valid_env() else {
        return;
    };
    let provider = Auth0AuthProvider::new(&env.domain, &env.audience, Arc::new(pool));
    let identity = provider
        .authenticate(&Credential::BearerJwt(env.valid_token))
        .await
        .expect("valid live Auth0 token should authenticate");
    assert_eq!(identity.identity_type, IdentityType::User);
    assert_eq!(identity.tenant_id, env.tenant_id);
    assert_eq!(identity.api_key_id, None);
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires MOA_RUN_LIVE_AUTH0_TESTS=1 and an expired Auth0 token"]
async fn auth0_expired_token_returns_expired(pool: sqlx::PgPool) {
    // Pins: a real expired Auth0 token maps to AuthError::Expired rather than generic rejection.
    let Some(env) = live_config() else {
        return;
    };
    let expired_token = required_env("MOA_TEST_AUTH0_EXPIRED_TOKEN");
    let provider = Auth0AuthProvider::new(&env.domain, &env.audience, Arc::new(pool));
    let error = provider
        .authenticate(&Credential::BearerJwt(expired_token))
        .await
        .expect_err("expired token should not authenticate");
    assert!(matches!(error, AuthError::Expired), "got {error:?}");
}

#[sqlx::test(migrations = "./migrations")]
#[ignore = "requires MOA_RUN_LIVE_AUTH0_TESTS=1 and a wrong-audience Auth0 token"]
async fn auth0_wrong_audience_returns_rejected(pool: sqlx::PgPool) {
    // Pins: a real Auth0 token for a different audience maps to AuthError::Rejected.
    let Some(env) = live_config() else {
        return;
    };
    let wrong_audience_token = required_env("MOA_TEST_AUTH0_WRONG_AUDIENCE_TOKEN");
    let provider = Auth0AuthProvider::new(&env.domain, &env.audience, Arc::new(pool));
    let error = provider
        .authenticate(&Credential::BearerJwt(wrong_audience_token))
        .await
        .expect_err("wrong-audience token should not authenticate");
    assert!(matches!(error, AuthError::Rejected), "got {error:?}");
}

struct LiveEnv {
    domain: String,
    audience: String,
    tenant_id: Uuid,
    valid_token: String,
}

struct LiveConfig {
    domain: String,
    audience: String,
}

fn live_config() -> Option<LiveConfig> {
    if std::env::var("MOA_RUN_LIVE_AUTH0_TESTS").as_deref() != Ok("1") {
        return None;
    }
    Some(LiveConfig {
        domain: required_env("MOA_TEST_AUTH0_DOMAIN"),
        audience: required_env("MOA_TEST_AUTH0_AUDIENCE"),
    })
}

fn live_valid_env() -> Option<LiveEnv> {
    let config = live_config()?;
    Some(LiveEnv {
        domain: config.domain,
        audience: config.audience,
        tenant_id: Uuid::parse_str(&required_env("MOA_TEST_AUTH0_TENANT_ID"))
            .expect("MOA_TEST_AUTH0_TENANT_ID must be a UUID"),
        valid_token: required_env("MOA_TEST_AUTH0_TOKEN"),
    })
}

fn required_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is required when MOA_RUN_LIVE_AUTH0_TESTS=1"))
}
