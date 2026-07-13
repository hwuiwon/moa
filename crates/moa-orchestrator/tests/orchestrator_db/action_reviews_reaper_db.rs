//! DB-backed tenant action-review timeout reaper tests.

use moa_orchestrator::services::action_reviews_reaper::ActionReviewReaper;
use moa_test_support::fixtures::quote_identifier;
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn expired_pending_review_is_failed_closed_db() {
    // Pins: a pending tenant action review past its expiry is transitioned to a
    // terminal `timeout` by the reaper. The terminal status is what makes the
    // gated tool fail closed: decide_review rejects any later clear once the row
    // leaves `pending`.
    let pool = test_pool().await;
    let review_id = insert_review(&pool, "command_execution", "high", ReviewClock::Expired).await;
    let reaper = ActionReviewReaper::new(pool.clone());

    let timed_out = reaper.sweep().await.expect("sweep should complete");

    assert_eq!(timed_out, 1, "the expired review should be failed closed");
    let (status, decided_at, deny_reason): (
        String,
        Option<chrono::DateTime<chrono::Utc>>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, decided_at, deny_reason FROM tenant_action_reviews WHERE id = $1",
    )
    .bind(review_id)
    .fetch_one(&pool)
    .await
    .expect("review row should remain readable");
    assert_eq!(status, "timeout", "review should be terminal timeout");
    assert!(decided_at.is_some(), "timeout must record a decision time");
    assert!(
        deny_reason.is_some(),
        "timeout must record a deny reason so the audit trail explains the closure"
    );
}

#[tokio::test]
async fn unexpired_pending_review_survives_sweep_db() {
    // Pins: a pending review that has not reached its expiry is left pending and
    // still counts toward the pending-queue depth the reaper publishes.
    let pool = test_pool().await;
    let review_id = insert_review(&pool, "local_write", "low", ReviewClock::Fresh).await;
    let reaper = ActionReviewReaper::new(pool.clone());

    let timed_out = reaper.sweep().await.expect("sweep should complete");
    // Sampling the gauges must decode against the real schema (pins the
    // EXTRACT(EPOCH ...) NUMERIC -> f64 cast).
    reaper
        .sample_gauges()
        .await
        .expect("gauge sampling should decode against the real schema");

    assert_eq!(timed_out, 0, "an unexpired review must not be timed out");
    let status: String =
        sqlx::query_scalar("SELECT status FROM tenant_action_reviews WHERE id = $1")
            .bind(review_id)
            .fetch_one(&pool)
            .await
            .expect("review row should remain readable");
    assert_eq!(status, "pending", "unexpired review stays pending");
}

/// Whether the inserted review is already past its expiry.
enum ReviewClock {
    Expired,
    Fresh,
}

async fn test_pool() -> PgPool {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let schema_name = format!("action_review_reaper_test_{}", Uuid::new_v4().simple());
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
        CREATE TABLE tenant_action_reviews (
            id UUID PRIMARY KEY,
            status TEXT NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'cleared', 'denied', 'timeout')),
            action_class TEXT NOT NULL,
            risk_level TEXT NOT NULL,
            deny_reason TEXT,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            expires_at TIMESTAMPTZ NOT NULL,
            decided_at TIMESTAMPTZ
        );
        "#,
    )
    .execute(&pool)
    .await
    .expect("action review test schema should apply");
    pool
}

async fn insert_review(
    pool: &PgPool,
    action_class: &str,
    risk_level: &str,
    clock: ReviewClock,
) -> Uuid {
    let review_id = Uuid::new_v4();
    let expires_at_sql = match clock {
        ReviewClock::Expired => "NOW() - INTERVAL '1 minute'",
        ReviewClock::Fresh => "NOW() + INTERVAL '1 day'",
    };
    sqlx::query(&format!(
        r#"
        INSERT INTO tenant_action_reviews
            (id, status, action_class, risk_level, created_at, expires_at)
        VALUES ($1, 'pending', $2, $3, NOW() - INTERVAL '2 minutes', {expires_at_sql})
        "#
    ))
    .bind(review_id)
    .bind(action_class)
    .bind(risk_level)
    .execute(pool)
    .await
    .expect("pending review should insert");
    review_id
}
