//! DB-backed authz challenge reaper recovery tests.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use moa_authz::{AwakeableResolveError, AwakeableResolver};
use moa_orchestrator::services::{
    authz_challenges_reaper::{AuthzChallengeReaper, AuthzChallengeTimeoutDelivery},
    durable_timeout::AuthzChallengeTimeout,
};
use moa_test_support::fixtures::quote_identifier;
use serde_json::json;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn stale_missing_awakeable_is_suppressed_after_first_sweep_db() {
    // Pins: a stale terminal challenge whose awakeable is gone is suppressed instead of retried forever.
    let pool = test_pool().await;
    let challenge_id =
        insert_terminal_challenge(&pool, "awakeable-missing", Some(Uuid::new_v4())).await;
    let resolver = MissingAwakeableResolver::default();
    let reaper = AuthzChallengeReaper::new(pool.clone());

    let resolved = reaper
        .sweep(&resolver)
        .await
        .expect("missing awakeable sweep should complete");
    // Sampling the builtin-approval gauges must decode against the real
    // schema (pins the EXTRACT(EPOCH ...) NUMERIC -> f64 cast).
    reaper
        .sample_gauges()
        .await
        .expect("gauge sampling should decode against the real schema");

    assert_eq!(
        resolved, 0,
        "suppressed missing awakeables are not counted as delivered"
    );
    assert_eq!(
        resolver.calls(),
        1,
        "first sweep should attempt the stale orphan exactly once"
    );
    let (resolved_at, resolve_claim_token): (Option<DateTime<Utc>>, Option<Uuid>) = sqlx::query_as(
        "SELECT resolved_at, resolve_claim_token FROM builtin_pending_approvals WHERE id = $1",
    )
    .bind(challenge_id)
    .fetch_one(&pool)
    .await
    .expect("challenge row should remain readable");
    assert!(
        resolved_at.is_some(),
        "missing awakeable suppression should mark the row terminal"
    );
    assert_eq!(
        resolve_claim_token, None,
        "missing awakeable suppression should clear the stale claim"
    );

    let second_resolved = reaper
        .sweep(&resolver)
        .await
        .expect("second missing awakeable sweep should complete");

    assert_eq!(second_resolved, 0);
    assert_eq!(
        resolver.calls(),
        1,
        "suppressed missing awakeable must not be retried on later sweeps"
    );
}

#[tokio::test]
async fn concurrent_reapers_resolve_terminal_challenge_once_db() {
    // Pins: two reapers racing on one terminal challenge produce one awakeable resolution.
    let pool = test_pool().await;
    let challenge_id = insert_terminal_challenge(&pool, "awakeable-once", None).await;
    let resolver = RecordingResolver::default();
    let first = AuthzChallengeReaper::new(pool.clone());
    let second = AuthzChallengeReaper::new(pool.clone());

    let (first_result, second_result) =
        tokio::join!(first.sweep(&resolver), second.sweep(&resolver));
    let total_resolved = first_result.expect("first reaper should complete")
        + second_result.expect("second reaper should complete");

    assert_eq!(
        total_resolved, 1,
        "competing reapers must report exactly one delivered challenge"
    );
    assert_eq!(
        resolver.calls(),
        1,
        "competing reapers must call the awakeable resolver exactly once"
    );
    assert_eq!(
        resolver.payloads(),
        vec![json!({ "outcome": "approved" })],
        "the single resolver call should deliver the stored terminal decision"
    );
    let (resolved_at, resolve_claim_token): (Option<DateTime<Utc>>, Option<Uuid>) = sqlx::query_as(
        "SELECT resolved_at, resolve_claim_token FROM builtin_pending_approvals WHERE id = $1",
    )
    .bind(challenge_id)
    .fetch_one(&pool)
    .await
    .expect("challenge row should remain readable");
    assert!(resolved_at.is_some(), "resolved challenge should be marked");
    assert_eq!(
        resolve_claim_token, None,
        "resolved claim should be cleared"
    );
}

#[tokio::test]
async fn durable_timeout_requires_the_exact_authz_awakeable_incarnation_db() {
    // Pins: a delayed timeout cannot resolve or terminalize a replacement
    // awakeable; the exact id/awakeable incarnation is claimed only once.
    let pool = test_pool().await;
    let challenge_id = insert_expired_pending_challenge(&pool, "awakeable-current").await;
    let reaper = AuthzChallengeReaper::new(pool.clone());

    let stale = reaper
        .apply_timeout(&AuthzChallengeTimeout {
            challenge_id,
            awakeable_id: "awakeable-older-incarnation".to_string(),
        })
        .await
        .expect("stale timeout should be a no-op");
    assert_eq!(stale, AuthzChallengeTimeoutDelivery::Stale);
    let status: String =
        sqlx::query_scalar("SELECT status FROM builtin_pending_approvals WHERE id = $1")
            .bind(challenge_id)
            .fetch_one(&pool)
            .await
            .expect("challenge status should load");
    assert_eq!(status, "pending", "stale awakeable must not fail closed");

    let exact = AuthzChallengeTimeout {
        challenge_id,
        awakeable_id: "awakeable-current".to_string(),
    };
    let delivery = reaper
        .apply_timeout(&exact)
        .await
        .expect("exact timeout should apply");
    let AuthzChallengeTimeoutDelivery::Resolve {
        challenge_id: delivered_challenge_id,
        awakeable_id,
        resolve_claim_token,
        newly_timed_out,
    } = delivery
    else {
        panic!("exact timeout should return its claimed awakeable delivery");
    };
    assert_eq!(delivered_challenge_id, challenge_id);
    assert_eq!(awakeable_id, "awakeable-current");
    assert!(newly_timed_out, "the first exact timeout changes the row");
    let (status, stored_claim): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT status, resolve_claim_token FROM builtin_pending_approvals WHERE id = $1",
    )
    .bind(challenge_id)
    .fetch_one(&pool)
    .await
    .expect("claimed challenge state should load");
    assert_eq!(status, "timeout");
    assert_eq!(
        stored_claim,
        Some(resolve_claim_token),
        "the returned delivery token must be the transaction's durable claim"
    );
    assert_eq!(
        reaper
            .apply_timeout(&exact)
            .await
            .expect("claimed timeout replay should be a no-op"),
        AuthzChallengeTimeoutDelivery::AlreadyDelivered,
        "the exact resolution claim prevents duplicate delivery"
    );
}

async fn test_pool() -> PgPool {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let schema_name = format!("authz_challenge_test_{}", Uuid::new_v4().simple());
    let search_path = format!("{}, public", quote_identifier(&schema_name));
    let pool = PgPoolOptions::new()
        .max_connections(4)
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
        CREATE TABLE builtin_pending_approvals (
            id                  UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
            session_id          UUID        NOT NULL,
            deciding_user_id    UUID        NOT NULL,
            tenant_id           UUID        NOT NULL,
            awakeable_id        TEXT        NOT NULL UNIQUE,
            action_summary      TEXT        NOT NULL,
            action_details      JSONB       NOT NULL,
            status              TEXT        NOT NULL DEFAULT 'pending'
                                              CHECK (status IN ('pending', 'approved', 'denied', 'timeout')),
            deny_reason         TEXT,
            created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            expires_at          TIMESTAMPTZ NOT NULL,
            decided_at          TIMESTAMPTZ,
            decided_by_user_id  UUID,
            resolved_at         TIMESTAMPTZ,
            resolve_claim_token UUID,
            resolve_claim_expires_at TIMESTAMPTZ
        );

        CREATE INDEX idx_builtin_approvals_resolution_claim
            ON builtin_pending_approvals(resolve_claim_expires_at, decided_at, expires_at)
            WHERE status IN ('approved', 'denied', 'timeout') AND resolved_at IS NULL;
        "#,
    )
    .execute(&pool)
    .await
    .expect("authz challenge test schema should apply");
    pool
}

async fn insert_terminal_challenge(
    pool: &PgPool,
    awakeable_id: &str,
    stale_claim_token: Option<Uuid>,
) -> Uuid {
    let challenge_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO builtin_pending_approvals
            (id, session_id, deciding_user_id, tenant_id, awakeable_id,
             action_summary, action_details, status, expires_at, decided_at,
             resolve_claim_token, resolve_claim_expires_at)
        VALUES
            ($1, $2, $3, $4, $5, 'approve deploy', '{}'::jsonb,
             'approved', NOW() - INTERVAL '1 minute', NOW() - INTERVAL '1 minute',
             $6,
             CASE WHEN $6::UUID IS NULL THEN NULL ELSE NOW() - INTERVAL '5 minutes' END)
        "#,
    )
    .bind(challenge_id)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(awakeable_id)
    .bind(stale_claim_token)
    .execute(pool)
    .await
    .expect("terminal challenge should insert");
    challenge_id
}

async fn insert_expired_pending_challenge(pool: &PgPool, awakeable_id: &str) -> Uuid {
    let challenge_id = Uuid::new_v4();
    sqlx::query(
        r#"
        INSERT INTO builtin_pending_approvals
            (id, session_id, deciding_user_id, tenant_id, awakeable_id,
             action_summary, action_details, status, expires_at)
        VALUES
            ($1, $2, $3, $4, $5, 'approve deploy', '{}'::jsonb,
             'pending', NOW() - INTERVAL '1 minute')
        "#,
    )
    .bind(challenge_id)
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(Uuid::new_v4())
    .bind(awakeable_id)
    .execute(pool)
    .await
    .expect("expired pending challenge should insert");
    challenge_id
}

#[derive(Default)]
struct MissingAwakeableResolver {
    calls: AtomicUsize,
}

impl MissingAwakeableResolver {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AwakeableResolver for MissingAwakeableResolver {
    async fn resolve(
        &self,
        _awakeable_id: &str,
        _payload: &serde_json::Value,
    ) -> Result<(), AwakeableResolveError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(AwakeableResolveError::message(
            "HTTP 404: awakeable not found",
        ))
    }
}

#[derive(Default)]
struct RecordingResolver {
    calls: AtomicUsize,
    payloads: Mutex<Vec<serde_json::Value>>,
}

impl RecordingResolver {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn payloads(&self) -> Vec<serde_json::Value> {
        self.payloads
            .lock()
            .expect("resolver payload lock should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl AwakeableResolver for RecordingResolver {
    async fn resolve(
        &self,
        _awakeable_id: &str,
        payload: &serde_json::Value,
    ) -> Result<(), AwakeableResolveError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.payloads
            .lock()
            .map_err(|_| AwakeableResolveError::message("resolver payload lock poisoned"))?
            .push(payload.clone());
        Ok(())
    }
}
