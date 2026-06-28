//! DB-backed tests for Auth0 CIBA approval resolution.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use httpmock::{Method::POST, MockServer};
use moa_auth_providers_auth0::Auth0AsyncAuthzProvider;
use moa_authz::{AwakeableResolveError, AwakeableResolver};
use moa_core::traits::{ApprovalDecision, ApprovalHandle, ApprovalRequest, AsyncAuthzProvider};
use secrecy::SecretString;
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use tokio::sync::{Notify, oneshot};
use uuid::Uuid;

#[tokio::test]
async fn ciba_approved_poll_resolves_awakeable() {
    // Pins: a successful CIBA token poll resolves the waiting awakeable as approved.
    let pool = test_pool().await;
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

#[tokio::test]
async fn ciba_access_denied_resolves_denied_reason() {
    // Pins: Auth0 access_denied resolves a denied payload with the provider reason.
    let pool = test_pool().await;
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

#[tokio::test]
async fn ciba_expired_token_resolves_timeout() {
    // Pins: Auth0 expired_token maps to MOA's timeout approval outcome.
    let pool = test_pool().await;
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

#[tokio::test]
async fn ciba_poll_decision_resumes_persisted_auth_req_id() {
    // Pins: a restarted provider can poll Auth0 from the persisted auth_req_id row.
    let pool = test_pool().await;
    let server = MockServer::start();
    let poll = server.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(200)
            .json_body(json!({ "access_token": "approved-token" }));
    });
    let (resolver, _rx) = RecordingResolver::new();
    let provider = provider(&server, pool.clone(), resolver);
    let approval_id = Uuid::new_v4();
    insert_ciba_approval(
        &pool,
        approval_id,
        "awakeable-resume",
        "persisted-auth-req",
        false,
    )
    .await;
    let handle = ApprovalHandle {
        id: approval_id,
        awakeable_id: "awakeable-resume".to_string(),
        provider_specific: json!({
            "kind": "auth0_ciba",
            "auth_req_id": "persisted-auth-req"
        }),
    };

    let decision = provider
        .poll_decision(&handle)
        .await
        .expect("poll_decision should query Auth0");

    assert_eq!(decision, Some(ApprovalDecision::Approved));
    poll.assert_hits(1);
    let (status, resolved_at): (String, Option<DateTime<Utc>>) =
        sqlx::query_as("SELECT status, resolved_at FROM auth0_ciba_approvals WHERE id = $1")
            .bind(approval_id)
            .fetch_one(&pool)
            .await
            .expect("CIBA row should remain readable");
    assert_eq!(status, "approved");
    assert_eq!(
        resolved_at, None,
        "poll_decision reports the decision but does not resolve the awakeable itself"
    );
}

#[tokio::test]
async fn ciba_recovery_loop_resumes_due_row_after_provider_restart() {
    // Pins: the background recovery sweep resumes due CIBA work from the shared DB after a worker restart.
    let pool = test_pool().await;
    let server = MockServer::start();
    let poll = server.mock(|when, then| {
        when.method(POST).path("/oauth/token");
        then.status(200)
            .json_body(json!({ "access_token": "approved-token" }));
    });

    let old_approval_id = Uuid::new_v4();
    insert_ciba_approval(
        &pool,
        old_approval_id,
        "awakeable-old-worker",
        "old-worker-auth-req",
        true,
    )
    .await;
    let (old_resolver, old_rx) = BlockingResolver::new();
    let _old_provider = provider(&server, pool.clone(), old_resolver);
    let old_payload = tokio::time::timeout(Duration::from_secs(3), old_rx)
        .await
        .expect("old provider should finish its first recovery sweep")
        .expect("old resolver sender should deliver payload");
    assert_eq!(old_payload, json!({ "outcome": "approved" }));

    let restarted_approval_id = Uuid::new_v4();
    insert_ciba_approval(
        &pool,
        restarted_approval_id,
        "awakeable-after-restart",
        "restart-auth-req",
        true,
    )
    .await;
    let (restarted_resolver, restarted_rx) = RecordingResolver::new();
    let _restarted_provider = provider(&server, pool.clone(), restarted_resolver);
    let restarted_payload = tokio::time::timeout(Duration::from_secs(3), restarted_rx)
        .await
        .expect("restarted provider should recover the due row")
        .expect("restarted resolver sender should deliver payload");

    assert_eq!(restarted_payload, json!({ "outcome": "approved" }));
    poll.assert_hits(2);
    let (status, resolved_at) = wait_for_ciba_resolved(&pool, restarted_approval_id).await;
    assert_eq!(status, "approved");
    assert!(
        resolved_at.is_some(),
        "recovery must resolve the awakeable and mark the row delivered"
    );
}

fn provider(
    server: &MockServer,
    pool: sqlx::PgPool,
    resolver: Arc<dyn AwakeableResolver>,
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

async fn test_pool() -> PgPool {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let schema_name = format!("moa_auth0_ciba_test_{}", Uuid::new_v4().simple());
    let search_path = format!("{}, public", quote_identifier(&schema_name));
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |conn, _meta| {
            let search_path = search_path.clone();
            Box::pin(async move {
                sqlx::query("SELECT pg_catalog.set_config('search_path', $1, false)")
                    .bind(search_path)
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .expect("test Postgres should be reachable");
    sqlx::query(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_identifier(&schema_name)
    ))
    .execute(&pool)
    .await
    .expect("test schema should be created");
    sqlx::raw_sql(
        r#"
        CREATE TABLE auth0_user_map (
            sub        TEXT NOT NULL,
            tenant_id  UUID NOT NULL,
            user_id    UUID NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            PRIMARY KEY (sub, tenant_id)
        );

        CREATE INDEX idx_auth0_user_map_user ON auth0_user_map(user_id);

        CREATE TABLE auth0_ciba_approvals (
            id                  UUID        PRIMARY KEY,
            session_id          UUID        NOT NULL,
            deciding_user_id    UUID        NOT NULL,
            awakeable_id        TEXT        NOT NULL UNIQUE,
            auth_req_id         TEXT        NOT NULL UNIQUE,
            status              TEXT        NOT NULL DEFAULT 'pending'
                                                  CHECK (status IN ('pending', 'approved', 'denied', 'timeout')),
            deny_reason         TEXT,
            poll_interval_ms    INTEGER     NOT NULL,
            next_poll_at        TIMESTAMPTZ NOT NULL,
            expires_at          TIMESTAMPTZ NOT NULL,
            resolved_at         TIMESTAMPTZ,
            lease_token         UUID,
            lease_expires_at    TIMESTAMPTZ,
            created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW()
        );

        CREATE INDEX idx_auth0_ciba_claimable
            ON auth0_ciba_approvals(status, next_poll_at, lease_expires_at)
            WHERE status = 'pending';

        CREATE INDEX idx_auth0_ciba_unresolved_terminal
            ON auth0_ciba_approvals(lease_expires_at, updated_at)
            WHERE status IN ('approved', 'denied', 'timeout') AND resolved_at IS NULL;
        "#,
    )
    .execute(&pool)
    .await
    .expect("auth0 CIBA test schema should apply");
    pool
}

async fn insert_ciba_approval(
    pool: &sqlx::PgPool,
    approval_id: Uuid,
    awakeable_id: &str,
    auth_req_id: &str,
    due: bool,
) {
    sqlx::query(
        r#"
        INSERT INTO auth0_ciba_approvals
            (id, session_id, deciding_user_id, awakeable_id, auth_req_id,
             poll_interval_ms, next_poll_at, expires_at)
        VALUES
            ($1, $2, $3, $4, $5, 1000,
             CASE WHEN $6 THEN NOW() - INTERVAL '1 second'
                  ELSE NOW() + INTERVAL '5 minutes'
             END,
             NOW() + INTERVAL '10 minutes')
        "#,
    )
    .bind(approval_id)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(awakeable_id)
    .bind(auth_req_id)
    .bind(due)
    .execute(pool)
    .await
    .expect("persisted CIBA row should insert");
}

async fn wait_for_ciba_resolved(
    pool: &sqlx::PgPool,
    approval_id: Uuid,
) -> (String, Option<DateTime<Utc>>) {
    let mut last = None;
    for _ in 0..50 {
        let row: (String, Option<DateTime<Utc>>) =
            sqlx::query_as("SELECT status, resolved_at FROM auth0_ciba_approvals WHERE id = $1")
                .bind(approval_id)
                .fetch_one(pool)
                .await
                .expect("CIBA row should remain readable");
        if row.1.is_some() {
            return row;
        }
        last = Some(row);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    last.expect("CIBA row should have been observed")
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
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

struct BlockingResolver {
    tx: Mutex<Option<oneshot::Sender<serde_json::Value>>>,
    release: Notify,
}

impl BlockingResolver {
    fn new() -> (Arc<Self>, oneshot::Receiver<serde_json::Value>) {
        let (tx, rx) = oneshot::channel();
        (
            Arc::new(Self {
                tx: Mutex::new(Some(tx)),
                release: Notify::new(),
            }),
            rx,
        )
    }
}

#[async_trait]
impl AwakeableResolver for BlockingResolver {
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
        self.release.notified().await;
        Ok(())
    }
}
