//! Offline tests for Auth0 CIBA approval resolution.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use httpmock::{Method::POST, MockServer};
use moa_auth_providers_auth0::Auth0AsyncAuthzProvider;
use moa_authz::{AwakeableResolveError, AwakeableResolver};
use moa_core::traits::{ApprovalRequest, AsyncAuthzProvider};
use secrecy::SecretString;
use serde_json::json;
use tokio::sync::oneshot;
use uuid::Uuid;

#[sqlx::test(migrations = "./migrations")]
async fn ciba_approved_poll_resolves_awakeable(pool: sqlx::PgPool) {
    // Pins: a successful CIBA token poll resolves the waiting awakeable as approved.
    let server = MockServer::start();
    let authorize = server.mock(|when, then| {
        when.method(POST).path("/bc-authorize");
        then.status(200).json_body(json!({
            "auth_req_id": "auth-req-1",
            "expires_in": 10,
            "interval": 1,
        }));
    });
    let poll = server.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(200)
            .json_body(json!({ "access_token": "approved-token" }));
    });
    let user_id = Uuid::from_u128(0x701);
    insert_user_map(&pool, user_id, Uuid::from_u128(0x702), "auth0|ciba-user").await;
    let (resolver, rx) = RecordingResolver::new();
    let provider = provider(&server, pool, resolver);

    let handle = provider
        .request_approval(request(user_id, "awakeable-approved"))
        .await
        .expect("CIBA request should start");
    let payload = tokio::time::timeout(Duration::from_secs(3), rx)
        .await
        .expect("approval should resolve before timeout")
        .expect("resolver sender should deliver payload");

    assert_eq!(handle.awakeable_id, "awakeable-approved");
    assert_eq!(handle.provider_specific["kind"], "auth0_ciba");
    assert_eq!(handle.provider_specific["auth_req_id"], "auth-req-1");
    assert_eq!(payload, json!({ "outcome": "approved" }));
    authorize.assert_hits(1);
    poll.assert_hits(1);
}

#[sqlx::test(migrations = "./migrations")]
async fn ciba_access_denied_resolves_denied_reason(pool: sqlx::PgPool) {
    // Pins: Auth0 access_denied resolves a denied payload with the provider reason.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/bc-authorize");
        then.status(200).json_body(json!({
            "auth_req_id": "auth-req-2",
            "expires_in": 10,
            "interval": 1,
        }));
    });
    server.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(400).json_body(json!({
            "error": "access_denied",
            "error_description": "user denied in Guardian",
        }));
    });
    let user_id = Uuid::from_u128(0x703);
    insert_user_map(&pool, user_id, Uuid::from_u128(0x704), "auth0|denied-user").await;
    let (resolver, rx) = RecordingResolver::new();

    provider(&server, pool, resolver)
        .request_approval(request(user_id, "awakeable-denied"))
        .await
        .expect("CIBA request should start");
    let payload = tokio::time::timeout(Duration::from_secs(3), rx)
        .await
        .expect("denial should resolve before timeout")
        .expect("resolver sender should deliver payload");

    assert_eq!(
        payload,
        json!({ "outcome": "denied", "reason": "user denied in Guardian" })
    );
}

#[sqlx::test(migrations = "./migrations")]
async fn ciba_expired_token_resolves_timeout(pool: sqlx::PgPool) {
    // Pins: Auth0 expired_token maps to MOA's timeout approval outcome.
    let server = MockServer::start();
    server.mock(|when, then| {
        when.method(POST).path("/bc-authorize");
        then.status(200).json_body(json!({
            "auth_req_id": "auth-req-3",
            "expires_in": 10,
            "interval": 1,
        }));
    });
    server.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(400)
            .json_body(json!({ "error": "expired_token" }));
    });
    let user_id = Uuid::from_u128(0x705);
    insert_user_map(&pool, user_id, Uuid::from_u128(0x706), "auth0|timeout-user").await;
    let (resolver, rx) = RecordingResolver::new();

    provider(&server, pool, resolver)
        .request_approval(request(user_id, "awakeable-timeout"))
        .await
        .expect("CIBA request should start");
    let payload = tokio::time::timeout(Duration::from_secs(3), rx)
        .await
        .expect("timeout should resolve before test timeout")
        .expect("resolver sender should deliver payload");

    assert_eq!(payload, json!({ "outcome": "timeout" }));
}

fn provider(
    server: &MockServer,
    pool: sqlx::PgPool,
    resolver: Arc<RecordingResolver>,
) -> Auth0AsyncAuthzProvider {
    Auth0AsyncAuthzProvider::new_with_base_url(
        server.base_url(),
        "https://issuer.example.test/".to_string(),
        "client-1".to_string(),
        SecretString::new("secret-1".to_string().into_boxed_str()),
        Arc::new(pool),
        resolver,
    )
    .expect("test provider should build")
}

fn request(user_id: Uuid, awakeable_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        session_id: Uuid::from_u128(0x900),
        deciding_user_id: user_id,
        action_summary: "approve deploy".to_string(),
        action_details: json!({ "_tenant_id": Uuid::from_u128(0x901).to_string() }),
        awakeable_id: awakeable_id.to_string(),
        timeout: Duration::from_secs(10),
    }
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

struct RecordingResolver {
    tx: Mutex<Option<oneshot::Sender<serde_json::Value>>>,
}

impl RecordingResolver {
    fn new() -> (Arc<Self>, oneshot::Receiver<serde_json::Value>) {
        let (tx, rx) = oneshot::channel();
        (
            Arc::new(Self {
                tx: Mutex::new(Some(tx)),
            }),
            rx,
        )
    }
}

#[async_trait]
impl AwakeableResolver for RecordingResolver {
    async fn resolve(
        &self,
        _awakeable_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), AwakeableResolveError> {
        let sender = self
            .tx
            .lock()
            .map_err(|_| AwakeableResolveError::message("resolver lock poisoned"))?
            .take();
        if let Some(sender) = sender {
            let _ = sender.send(payload.clone());
        }
        Ok(())
    }
}
