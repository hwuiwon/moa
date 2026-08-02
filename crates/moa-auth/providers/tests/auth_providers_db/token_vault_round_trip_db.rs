//! DB-backed tests for the self-hosted Postgres token vault provider.
//!
//! These exercise the real store -> get -> list path through row-level security
//! under the `moa_app` role. They require a reachable Postgres (the compose
//! default or `MOA_DATABASE_URL`) whose database already provides the `moa_app`
//! role; the isolated auth schema installs the token table and its inline RLS.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use moa_auth_providers::{
    OAuthRefreshEndpoint, PostgresTokenVaultProvider, StoreTokenRequest, TokenRefresher,
};
use moa_core::traits::{TokenVaultError, TokenVaultProvider};
use moa_core::types::identifiers::TenantId;
use moa_crypto::LocalKmsProvider;
use moa_db::ScopedConn;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::Notify;
use uuid::Uuid;
use wiremock::{Request, Respond, ResponseTemplate};

use super::support::{migrated_pool, migrated_pool_pair};

#[tokio::test]
async fn store_get_list_round_trip_db() {
    // Pins: a stored token is retrievable with its exact secret and scopes, and
    // the connection appears in the user's connection listing.
    let pool = migrated_pool("moa_token_vault_test").await;
    let provider = encrypted_provider(Arc::new(pool));
    let tenant = TenantId::new();
    let user_id = Uuid::new_v4();

    provider
        .store_token(StoreTokenRequest {
            tenant_id: tenant,
            user_id,
            connection_name: "google",
            provider: "google",
            external_account_id: Some("acct-123"),
            access_token: SecretString::new("ya29.access-token".to_string().into_boxed_str()),
            refresh_token: Some(SecretString::new(
                "1//refresh-token".to_string().into_boxed_str(),
            )),
            token_type: Some("Bearer"),
            expires_at: Some(moa_test_support::fixtures::pg_now() + ChronoDuration::hours(1)),
            scopes: &["email".to_string(), "profile".to_string()],
        })
        .await
        .expect("store token");

    let token = provider
        .get_token(user_id, "google")
        .await
        .expect("get token");
    assert_eq!(token.access_token.expose_secret(), "ya29.access-token");
    assert_eq!(
        token.scopes,
        vec!["email".to_string(), "profile".to_string()]
    );
    assert!(token.expires_at.is_some());

    provider
        .store_token(StoreTokenRequest {
            tenant_id: tenant,
            user_id,
            connection_name: "github",
            provider: "github",
            external_account_id: None,
            access_token: SecretString::new("gho_access".to_string().into_boxed_str()),
            refresh_token: None,
            token_type: Some("Bearer"),
            expires_at: None,
            scopes: &["repo".to_string()],
        })
        .await
        .expect("store second token");

    let connections = provider
        .list_connections(user_id)
        .await
        .expect("list connections");
    assert_eq!(
        connections,
        vec!["github".to_string(), "google".to_string()]
    );
}

#[tokio::test]
async fn store_token_global_conflict_target_preserves_tenant_owner_db() {
    // Pins: the globally unique (user, connection) path is the sole upsert
    // arbiter, so a same-tenant relink updates in place while a different tenant
    // cannot take ownership or mutate the existing encrypted credential.
    let pool = Arc::new(migrated_pool("moa_token_vault_test").await);
    let provider = encrypted_provider(pool.clone());
    let tenant = TenantId::new();
    let user_id = Uuid::new_v4();

    provider
        .store_token(StoreTokenRequest {
            tenant_id: tenant,
            user_id,
            connection_name: "google",
            provider: "google",
            external_account_id: None,
            access_token: SecretString::new("first-token".to_string().into_boxed_str()),
            refresh_token: None,
            token_type: Some("Bearer"),
            expires_at: Some(moa_test_support::fixtures::pg_now() + ChronoDuration::hours(1)),
            scopes: &["email".to_string()],
        })
        .await
        .expect("first store must use the global unique index");

    let first: (Uuid, Uuid, Vec<u8>, i64) = sqlx::query_as(
        "SELECT id, tenant_id, access_token_sealed, generation \
         FROM token_vault_connections \
         WHERE user_id = $1 AND connection_name = 'google'",
    )
    .bind(user_id)
    .fetch_one(pool.as_ref())
    .await
    .expect("read first encrypted row");

    provider
        .store_token(StoreTokenRequest {
            tenant_id: tenant,
            user_id,
            connection_name: "google",
            provider: "google",
            external_account_id: None,
            access_token: SecretString::new("second-token".to_string().into_boxed_str()),
            refresh_token: None,
            token_type: Some("Bearer"),
            expires_at: Some(moa_test_support::fixtures::pg_now() + ChronoDuration::hours(1)),
            scopes: &["email".to_string()],
        })
        .await
        .expect("same-tenant relink must update in place");

    let relinked: (Uuid, Uuid, Vec<u8>, i64) = sqlx::query_as(
        "SELECT id, tenant_id, access_token_sealed, generation \
         FROM token_vault_connections \
         WHERE user_id = $1 AND connection_name = 'google'",
    )
    .bind(user_id)
    .fetch_one(pool.as_ref())
    .await
    .expect("read relinked encrypted row");
    assert_eq!(relinked.0, first.0, "relink must preserve row identity");
    assert_eq!(
        relinked.1, tenant.0,
        "relink must preserve tenant ownership"
    );
    assert_ne!(
        relinked.2, first.2,
        "relink must replace the encrypted token"
    );
    assert_eq!(relinked.3, 2, "relink must advance generation exactly once");

    let other_tenant = TenantId::new();
    let cross_tenant = provider
        .store_token(StoreTokenRequest {
            tenant_id: other_tenant,
            user_id,
            connection_name: "google",
            provider: "google",
            external_account_id: None,
            access_token: SecretString::new("attacker-token".to_string().into_boxed_str()),
            refresh_token: None,
            token_type: Some("Bearer"),
            expires_at: None,
            scopes: &["email".to_string()],
        })
        .await;
    assert!(
        matches!(cross_tenant, Err(TokenVaultError::Internal(_))),
        "cross-tenant replay must fail closed, got {cross_tenant:?}"
    );

    let after_rejected_replay: (Uuid, Uuid, Vec<u8>, i64) = sqlx::query_as(
        "SELECT id, tenant_id, access_token_sealed, generation \
         FROM token_vault_connections \
         WHERE user_id = $1 AND connection_name = 'google'",
    )
    .bind(user_id)
    .fetch_one(pool.as_ref())
    .await
    .expect("read row after rejected cross-tenant replay");
    assert_eq!(
        after_rejected_replay, relinked,
        "rejected replay must not change tenant, ciphertext, generation, or row identity"
    );

    let obsolete_target_error = sqlx::query(
        "INSERT INTO token_vault_connections (\
             tenant_id, user_id, connection_name, provider, access_token_sealed\
         ) VALUES ($1, $2, 'google', 'google', '\\x00'::BYTEA) \
         ON CONFLICT (tenant_id, user_id, connection_name) DO NOTHING",
    )
    .bind(tenant.0)
    .bind(user_id)
    .execute(pool.as_ref())
    .await
    .expect_err("the removed three-column conflict target must have no arbiter");
    assert_eq!(
        obsolete_target_error
            .as_database_error()
            .and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("42P10")),
        "obsolete conflict target must fail because its arbiter is absent"
    );

    let token = provider
        .get_token(user_id, "google")
        .await
        .expect("get token");
    assert_eq!(token.access_token.expose_secret(), "second-token");

    let connections = provider
        .list_connections(user_id)
        .await
        .expect("list connections");
    assert_eq!(connections, vec!["google".to_string()]);
}

#[tokio::test]
async fn get_token_unlinked_connection_returns_not_linked_db() {
    // Pins: retrieving a connection the user never linked fails explicitly.
    let pool = migrated_pool("moa_token_vault_test").await;
    let provider = encrypted_provider(Arc::new(pool));
    let user_id = Uuid::new_v4();

    // VaultToken holds a secret and is intentionally not Debug, so match on the
    // result rather than using expect_err.
    match provider.get_token(user_id, "google").await {
        Err(TokenVaultError::NotLinked) => {}
        Err(other) => panic!("expected NotLinked, got {other:?}"),
        Ok(_) => panic!("unlinked connection must fail"),
    }
}

#[tokio::test]
async fn get_token_expired_returns_unavailable_db() {
    // Pins: an expired access token is surfaced as unavailable rather than
    // handed back stale, wiring the expiry check before refresh exists.
    let pool = migrated_pool("moa_token_vault_test").await;
    let provider = encrypted_provider(Arc::new(pool));
    let tenant = TenantId::new();
    let user_id = Uuid::new_v4();

    provider
        .store_token(StoreTokenRequest {
            tenant_id: tenant,
            user_id,
            connection_name: "google",
            provider: "google",
            external_account_id: None,
            access_token: SecretString::new("stale-token".to_string().into_boxed_str()),
            refresh_token: Some(SecretString::new("refresh".to_string().into_boxed_str())),
            token_type: Some("Bearer"),
            expires_at: Some(moa_test_support::fixtures::pg_now() - ChronoDuration::hours(1)),
            scopes: &["email".to_string()],
        })
        .await
        .expect("store expired token");

    // VaultToken holds a secret and is intentionally not Debug, so match on the
    // result rather than using expect_err.
    match provider.get_token(user_id, "google").await {
        Err(TokenVaultError::Unavailable(_)) => {}
        Err(other) => panic!("expected Unavailable, got {other:?}"),
        Ok(_) => panic!("expired token must not be returned"),
    }
}

#[tokio::test]
async fn envelope_encrypts_token_at_rest_db() {
    // Pins: with the explicit KMS, a stored token round-trips through
    // decryption on read AND the raw access_token_sealed column is ciphertext,
    // not the plaintext token — encryption at rest confirmed end to end.
    let pool = Arc::new(migrated_pool("moa_token_vault_test").await);
    let provider = PostgresTokenVaultProvider::new(pool.clone(), test_kms());
    let tenant = TenantId::new();
    let user_id = Uuid::new_v4();
    let plaintext = "ya29.super-secret-access-token";

    provider
        .store_token(StoreTokenRequest {
            tenant_id: tenant,
            user_id,
            connection_name: "google",
            provider: "google",
            external_account_id: None,
            access_token: SecretString::new(plaintext.to_string().into_boxed_str()),
            refresh_token: Some(SecretString::new("1//refresh".to_string().into_boxed_str())),
            token_type: Some("Bearer"),
            expires_at: Some(moa_test_support::fixtures::pg_now() + ChronoDuration::hours(1)),
            scopes: &["email".to_string()],
        })
        .await
        .expect("store token");

    // Round-trips through decryption on read.
    let token = provider
        .get_token(user_id, "google")
        .await
        .expect("get token");
    assert_eq!(token.access_token.expose_secret(), plaintext);

    // The raw stored column is ciphertext, read under the same control-plane
    // moa_app path the provider uses so row-level security does not hide it.
    let mut conn = ScopedConn::begin_control_plane(&pool)
        .await
        .expect("control-plane conn");
    conn.assume_app_role().await.expect("assume moa_app");
    let (sealed,): (Vec<u8>,) = sqlx::query_as(
        "SELECT access_token_sealed FROM token_vault_connections \
         WHERE user_id = $1 AND connection_name = $2",
    )
    .bind(user_id)
    .bind("google")
    .fetch_one(conn.as_mut())
    .await
    .expect("read sealed column");
    conn.commit().await.expect("commit read");

    assert_ne!(
        sealed.as_slice(),
        plaintext.as_bytes(),
        "token must be encrypted at rest"
    );
    assert!(
        !sealed
            .windows(plaintext.len())
            .any(|window| window == plaintext.as_bytes()),
        "sealed column must not contain the plaintext token"
    );
}

#[tokio::test]
async fn get_token_expired_refreshes_and_persists_db() {
    // Pins: an expired access token with a stored refresh token and a configured
    // refresh endpoint is transparently refreshed via the refresh_token grant,
    // the rotated material is persisted (a second get returns it without a second
    // refresh), and the stale token is never returned.
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "fresh-access-token",
            "refresh_token": "rotated-refresh-token",
            "expires_in": 3600,
            "scope": "email profile"
        })))
        // Exactly one refresh: the second get must be served from the persisted
        // rotated token, proving persistence.
        .expect(1)
        .mount(&server)
        .await;

    let pool = Arc::new(migrated_pool("moa_token_vault_test").await);
    let mut endpoints = HashMap::new();
    endpoints.insert(
        "google".to_string(),
        OAuthRefreshEndpoint {
            token_endpoint: format!("{}/token", server.uri()),
            client_id: "client-123".to_string(),
            client_secret: Some(SecretString::new(
                "client-secret".to_string().into_boxed_str(),
            )),
        },
    );
    let refresher = Arc::new(TokenRefresher::new(endpoints).expect("refresher builds"));
    let provider = encrypted_provider(pool.clone()).with_refresher(refresher);

    let tenant = TenantId::new();
    let user_id = Uuid::new_v4();
    provider
        .store_token(StoreTokenRequest {
            tenant_id: tenant,
            user_id,
            connection_name: "google",
            provider: "google",
            external_account_id: Some("acct-1"),
            access_token: SecretString::new("stale-access".to_string().into_boxed_str()),
            refresh_token: Some(SecretString::new(
                "old-refresh".to_string().into_boxed_str(),
            )),
            token_type: Some("Bearer"),
            expires_at: Some(moa_test_support::fixtures::pg_now() - ChronoDuration::hours(1)),
            scopes: &["email".to_string()],
        })
        .await
        .expect("store expired token");

    let token = provider
        .get_token(user_id, "google")
        .await
        .expect("refresh on get");
    assert_eq!(token.access_token.expose_secret(), "fresh-access-token");
    assert_eq!(
        token.scopes,
        vec!["email".to_string(), "profile".to_string()]
    );
    assert!(
        token
            .expires_at
            .is_some_and(|expiry| expiry > moa_test_support::fixtures::pg_now()),
        "refreshed token must carry a future expiry"
    );

    // The rotation persisted: the second get is served from storage without a
    // second refresh (the mock expects exactly one call, verified on drop).
    let token2 = provider
        .get_token(user_id, "google")
        .await
        .expect("second get from persisted token");
    assert_eq!(token2.access_token.expose_secret(), "fresh-access-token");
}

#[tokio::test]
async fn concurrent_refresh_across_separate_pools_calls_upstream_once_db() {
    // Pins: two Kubernetes replicas represented by independent providers and
    // pools elect one durable refresh winner, return the same persisted token,
    // and store both rotated secrets encrypted at rest.
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .and(body_string_contains("grant_type=refresh_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(150))
                .set_body_json(serde_json::json!({
                    "access_token": "shared-fresh-access",
                    "refresh_token": "shared-rotated-refresh",
                    "expires_in": 3600,
                    "scope": "email profile"
                })),
        )
        .mount(&server)
        .await;

    let (pool_a, pool_b) = migrated_pool_pair("moa_token_vault_test").await;
    let pool_a = Arc::new(pool_a);
    let pool_b = Arc::new(pool_b);
    let kms = test_kms();
    let refresher = test_refresher(&server);
    let provider_a = PostgresTokenVaultProvider::new(pool_a.clone(), kms.clone())
        .with_refresher(refresher.clone());
    let provider_b = PostgresTokenVaultProvider::new(pool_b.clone(), kms).with_refresher(refresher);
    let tenant = TenantId::new();
    let user_id = Uuid::new_v4();

    provider_a
        .store_token(expired_google_token(
            tenant,
            user_id,
            "stale-access",
            "old-refresh",
        ))
        .await
        .expect("store expired token");

    let (token_a, token_b) = tokio::join!(
        provider_a.get_token(user_id, "google"),
        provider_b.get_token(user_id, "google")
    );
    let token_a = token_a.expect("first replica should resolve refreshed token");
    let token_b = token_b.expect("second replica should resolve persisted token");
    assert_eq!(token_a.access_token.expose_secret(), "shared-fresh-access");
    assert_eq!(token_b.access_token.expose_secret(), "shared-fresh-access");
    assert_eq!(token_a.scopes, vec!["email", "profile"]);
    assert_eq!(token_b.scopes, token_a.scopes);

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose refresh requests");
    assert_eq!(requests.len(), 1, "exactly one replica may call upstream");

    let (access_sealed, refresh_sealed) = read_sealed_tokens(&pool_b, user_id).await;
    assert_ciphertext_excludes(&access_sealed, b"shared-fresh-access", "access");
    let refresh_sealed = refresh_sealed.expect("rotated refresh token should be stored");
    assert_ciphertext_excludes(&refresh_sealed, b"shared-rotated-refresh", "refresh");
    let state = read_refresh_state(&pool_b, user_id).await;
    assert_eq!(state.generation, 1);
    assert_eq!(state.refresh_state, "ready");
    assert!(state.refresh_lease_id.is_none());
    assert!(state.refresh_lease_expires_at.is_none());
}

#[tokio::test]
async fn expired_lease_requires_explicit_relink_without_remote_retry_db() {
    // Pins: an uncertain refresh lease never reuses a possibly-consumed refresh
    // token. Recovery marks the row relink_required, and an explicit store/relink
    // supplies fresh credentials, increments generation, and clears the fence.
    use wiremock::MockServer;

    let server = MockServer::start().await;
    let pool = Arc::new(migrated_pool("moa_token_vault_test").await);
    let provider = encrypted_provider(pool.clone()).with_refresher(test_refresher(&server));
    let tenant = TenantId::new();
    let user_id = Uuid::new_v4();
    provider
        .store_token(expired_google_token(
            tenant,
            user_id,
            "stale-access",
            "possibly-consumed-refresh",
        ))
        .await
        .expect("store expired token");
    force_expired_refresh_lease(&pool, user_id).await;

    match provider.get_token(user_id, "google").await {
        Err(TokenVaultError::Unavailable(_)) => {}
        Err(other) => panic!("expected Unavailable, got {other:?}"),
        Ok(_) => panic!("expired uncertain lease must require relinking"),
    }
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose refresh requests");
    assert_eq!(requests.len(), 0, "expired lease must not retry upstream");
    let uncertain = read_refresh_state(&pool, user_id).await;
    assert_eq!(uncertain.generation, 1);
    assert_eq!(uncertain.refresh_state, "relink_required");
    assert!(uncertain.refresh_lease_id.is_none());
    assert!(uncertain.refresh_lease_expires_at.is_none());

    provider
        .store_token(StoreTokenRequest {
            tenant_id: tenant,
            user_id,
            connection_name: "google",
            provider: "google",
            external_account_id: Some("acct-relinked"),
            access_token: SecretString::new("relinked-access".to_string().into_boxed_str()),
            refresh_token: Some(SecretString::new(
                "relinked-refresh".to_string().into_boxed_str(),
            )),
            token_type: Some("Bearer"),
            expires_at: Some(moa_test_support::fixtures::pg_now() + ChronoDuration::hours(1)),
            scopes: &["email".to_string()],
        })
        .await
        .expect("explicit relink should replace uncertain credentials");

    let relinked = read_refresh_state(&pool, user_id).await;
    assert_eq!(relinked.generation, 2);
    assert_eq!(relinked.refresh_state, "ready");
    assert!(relinked.refresh_lease_id.is_none());
    assert!(relinked.refresh_lease_expires_at.is_none());
    let token = provider
        .get_token(user_id, "google")
        .await
        .expect("relinked token should be readable");
    assert_eq!(token.access_token.expose_secret(), "relinked-access");
}

#[tokio::test]
async fn stale_refresh_winner_cannot_cross_generation_fence_db() {
    // Pins: even if a stale winner still holds the same lease UUID, changing the
    // row generation prevents its delayed response from overwriting newer state.
    // This directly mutation-pins the generation predicate in the refresh CAS.
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    let server = MockServer::start().await;
    let arrived = Arc::new(Notify::new());
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(SignalingRefreshResponse {
            arrived: arrived.clone(),
        })
        .mount(&server)
        .await;

    let pool = Arc::new(migrated_pool("moa_token_vault_test").await);
    let provider =
        Arc::new(encrypted_provider(pool.clone()).with_refresher(test_refresher(&server)));
    let tenant = TenantId::new();
    let user_id = Uuid::new_v4();
    provider
        .store_token(expired_google_token(
            tenant,
            user_id,
            "stale-access",
            "old-refresh",
        ))
        .await
        .expect("store expired token");
    let (sealed_before, _) = read_sealed_tokens(&pool, user_id).await;

    let arrival = arrived.notified();
    let refresh_provider = provider.clone();
    let refresh = tokio::spawn(async move { refresh_provider.get_token(user_id, "google").await });
    arrival.await;
    bump_generation_while_refreshing(&pool, user_id).await;

    let result = refresh.await.expect("refresh task should join");
    match result {
        Err(TokenVaultError::Unavailable(_)) => {}
        Err(other) => panic!("expected Unavailable after stale CAS, got {other:?}"),
        Ok(_) => panic!("stale generation must not persist or return its token"),
    }
    let (sealed_after, _) = read_sealed_tokens(&pool, user_id).await;
    assert_eq!(
        sealed_after, sealed_before,
        "stale winner must not overwrite stored ciphertext"
    );
    let state = read_refresh_state(&pool, user_id).await;
    assert_eq!(state.generation, 2);
    assert_eq!(state.refresh_state, "refreshing");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should expose refresh requests");
    assert_eq!(requests.len(), 1, "the stale winner made one remote call");
}

/// Wiremock responder that exposes when remote I/O starts and delays the reply
/// long enough for the test to advance the durable generation fence.
struct SignalingRefreshResponse {
    arrived: Arc<Notify>,
}

impl Respond for SignalingRefreshResponse {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.arrived.notify_one();
        ResponseTemplate::new(200)
            .set_delay(Duration::from_millis(200))
            .set_body_json(serde_json::json!({
                "access_token": "stale-winner-access",
                "refresh_token": "stale-winner-refresh",
                "expires_in": 3600,
                "scope": "email"
            }))
    }
}

#[derive(sqlx::FromRow)]
struct RefreshStateRow {
    generation: i64,
    refresh_state: String,
    refresh_lease_id: Option<Uuid>,
    refresh_lease_expires_at: Option<chrono::DateTime<Utc>>,
}

fn test_kms() -> Arc<dyn moa_crypto::KeyManagementProvider> {
    Arc::new(LocalKmsProvider::new())
}

fn encrypted_provider(pool: Arc<sqlx::PgPool>) -> PostgresTokenVaultProvider {
    PostgresTokenVaultProvider::new(pool, test_kms())
}

fn test_refresher(server: &wiremock::MockServer) -> Arc<TokenRefresher> {
    let mut endpoints = HashMap::new();
    endpoints.insert(
        "google".to_string(),
        OAuthRefreshEndpoint {
            token_endpoint: format!("{}/token", server.uri()),
            client_id: "client-123".to_string(),
            client_secret: Some(SecretString::new(
                "client-secret".to_string().into_boxed_str(),
            )),
        },
    );
    Arc::new(TokenRefresher::new(endpoints).expect("refresher should build"))
}

fn expired_google_token(
    tenant_id: TenantId,
    user_id: Uuid,
    access_token: &str,
    refresh_token: &str,
) -> StoreTokenRequest<'static> {
    StoreTokenRequest {
        tenant_id,
        user_id,
        connection_name: "google",
        provider: "google",
        external_account_id: Some("acct-1"),
        access_token: SecretString::new(access_token.to_string().into_boxed_str()),
        refresh_token: Some(SecretString::new(
            refresh_token.to_string().into_boxed_str(),
        )),
        token_type: Some("Bearer"),
        expires_at: Some(moa_test_support::fixtures::pg_now() - ChronoDuration::hours(1)),
        scopes: &[],
    }
}

async fn read_refresh_state(pool: &sqlx::PgPool, user_id: Uuid) -> RefreshStateRow {
    let mut conn = ScopedConn::begin_control_plane(pool)
        .await
        .expect("control-plane connection should open");
    conn.assume_app_role()
        .await
        .expect("test should assume moa_app");
    let row = sqlx::query_as(
        r#"
        SELECT generation, refresh_state, refresh_lease_id, refresh_lease_expires_at
        FROM token_vault_connections
        WHERE user_id = $1 AND connection_name = 'google'
        "#,
    )
    .bind(user_id)
    .fetch_one(conn.as_mut())
    .await
    .expect("refresh state row should exist");
    conn.commit().await.expect("state read should commit");
    row
}

async fn read_sealed_tokens(pool: &sqlx::PgPool, user_id: Uuid) -> (Vec<u8>, Option<Vec<u8>>) {
    let mut conn = ScopedConn::begin_control_plane(pool)
        .await
        .expect("control-plane connection should open");
    conn.assume_app_role()
        .await
        .expect("test should assume moa_app");
    let row = sqlx::query_as(
        r#"
        SELECT access_token_sealed, refresh_token_sealed
        FROM token_vault_connections
        WHERE user_id = $1 AND connection_name = 'google'
        "#,
    )
    .bind(user_id)
    .fetch_one(conn.as_mut())
    .await
    .expect("sealed token row should exist");
    conn.commit()
        .await
        .expect("sealed token read should commit");
    row
}

async fn force_expired_refresh_lease(pool: &sqlx::PgPool, user_id: Uuid) {
    let mut conn = ScopedConn::begin_control_plane(pool)
        .await
        .expect("control-plane connection should open");
    conn.assume_app_role()
        .await
        .expect("test should assume moa_app");
    let updated = sqlx::query(
        r#"
        UPDATE token_vault_connections
        SET refresh_state = 'refreshing',
            refresh_lease_id = $2,
            refresh_lease_expires_at = NOW() - INTERVAL '1 second'
        WHERE user_id = $1 AND connection_name = 'google'
        "#,
    )
    .bind(user_id)
    .bind(Uuid::new_v4())
    .execute(conn.as_mut())
    .await
    .expect("expired lease should be installed");
    assert_eq!(updated.rows_affected(), 1);
    conn.commit()
        .await
        .expect("expired lease write should commit");
}

async fn bump_generation_while_refreshing(pool: &sqlx::PgPool, user_id: Uuid) {
    let mut conn = ScopedConn::begin_control_plane(pool)
        .await
        .expect("control-plane connection should open");
    conn.assume_app_role()
        .await
        .expect("test should assume moa_app");
    let updated = sqlx::query(
        r#"
        UPDATE token_vault_connections
        SET generation = generation + 1
        WHERE user_id = $1
          AND connection_name = 'google'
          AND refresh_state = 'refreshing'
        "#,
    )
    .bind(user_id)
    .execute(conn.as_mut())
    .await
    .expect("generation fence should advance");
    assert_eq!(updated.rows_affected(), 1);
    conn.commit()
        .await
        .expect("generation fence write should commit");
}

fn assert_ciphertext_excludes(sealed: &[u8], plaintext: &[u8], label: &str) {
    assert_ne!(sealed, plaintext, "{label} token must be ciphertext");
    assert!(
        !sealed
            .windows(plaintext.len())
            .any(|window| window == plaintext),
        "sealed {label} token must not contain plaintext"
    );
}
