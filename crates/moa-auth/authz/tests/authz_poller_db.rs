//! DB-backed authz outbox poller recovery tests.

use moa_authz::{FgaClient, FgaConfig, OutboxPoller, PollerConfig};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

#[tokio::test]
async fn poller_reclaims_stale_in_flight_row_db() {
    // Pins: a worker crash after marking a row in_flight does not strand the tuple write.
    let pool = test_pool().await;
    let row_id = Uuid::new_v4();
    insert_outbox_row(
        &pool,
        row_id,
        "in_flight",
        Some(Uuid::new_v4()),
        "NOW() - INTERVAL '5 minutes'",
    )
    .await;
    let poller = poller(pool.clone(), 1);

    let drained = poller.tick().await.expect("poller tick should complete");

    assert_eq!(drained, 0, "failed FGA writes are not counted as drained");
    let (status, attempts, lease_token, last_error): (String, i32, Option<Uuid>, Option<String>) =
        sqlx::query_as(
            "SELECT status, attempts, lease_token, last_error FROM authz_outbox WHERE id = $1",
        )
        .bind(row_id)
        .fetch_one(&pool)
        .await
        .expect("claimed row should remain readable");
    assert_eq!(status, "pending");
    assert_eq!(
        attempts, 1,
        "stale in_flight row must be retried exactly once"
    );
    assert_eq!(lease_token, None, "failed attempt should release the lease");
    assert!(
        last_error.is_some(),
        "failed FGA write should leave retry diagnostics"
    );
}

#[tokio::test]
async fn concurrent_pollers_claim_pending_row_once_db() {
    // Pins: multiple pods racing on the same pending row cannot both own and process the lease.
    let pool = test_pool().await;
    let row_id = Uuid::new_v4();
    insert_outbox_row(&pool, row_id, "pending", None, "NULL").await;
    let first = poller(pool.clone(), 1);
    let second = poller(pool.clone(), 1);

    let (first_result, second_result) = tokio::join!(first.tick(), second.tick());

    assert_eq!(
        first_result.expect("first tick should complete")
            + second_result.expect("second tick should complete"),
        0,
        "failing FGA writes are not counted as drained"
    );
    let (status, attempts, lease_token): (String, i32, Option<Uuid>) =
        sqlx::query_as("SELECT status, attempts, lease_token FROM authz_outbox WHERE id = $1")
            .bind(row_id)
            .fetch_one(&pool)
            .await
            .expect("outbox row should remain readable");
    assert_eq!(status, "pending");
    assert_eq!(
        attempts, 1,
        "competing pollers must process the pending row exactly once"
    );
    assert_eq!(lease_token, None, "failed attempt should release the lease");
}

async fn test_pool() -> PgPool {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let schema_name = format!("authz_poller_test_{}", Uuid::new_v4().simple());
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
        CREATE TABLE authz_outbox (
            id              UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
            idempotency_key TEXT        NOT NULL UNIQUE,
            op              TEXT        NOT NULL CHECK (op IN ('write', 'delete')),
            tuple_user      TEXT        NOT NULL,
            tuple_relation  TEXT        NOT NULL,
            tuple_object    TEXT        NOT NULL,
            model_version   INTEGER     NOT NULL,
            status          TEXT        NOT NULL DEFAULT 'pending'
                                          CHECK (status IN ('pending', 'in_flight', 'succeeded', 'dead_letter')),
            attempts        INTEGER     NOT NULL DEFAULT 0,
            last_error      TEXT,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            tenant_id       UUID,
            lease_token     UUID,
            lease_expires_at TIMESTAMPTZ
        );

        CREATE INDEX idx_authz_outbox_claimable
            ON authz_outbox(status, next_attempt_at, lease_expires_at)
            WHERE status IN ('pending', 'in_flight');
        "#,
    )
    .execute(&pool)
    .await
    .expect("authz outbox test schema should apply");
    pool
}

fn poller(pool: PgPool, batch_size: usize) -> OutboxPoller {
    let client = FgaClient::new(FgaConfig {
        url: "http://127.0.0.1:9".to_string(),
        preshared_key: "test".to_string(),
        store_id: "store".to_string(),
        model_id: "model".to_string(),
        timeout_ms: 100,
    })
    .expect("test FGA config should be valid");
    OutboxPoller::new(
        pool,
        client,
        PollerConfig {
            batch_size,
            poll_interval: Duration::from_millis(10),
            max_attempts: 4,
            backoff_base: Duration::from_secs(60),
            backoff_cap: Duration::from_secs(60),
            lease_duration: Duration::from_secs(60),
        },
    )
}

async fn insert_outbox_row(
    pool: &PgPool,
    row_id: Uuid,
    status: &str,
    lease_token: Option<Uuid>,
    lease_expires_at_sql: &str,
) {
    let sql = format!(
        r#"
        INSERT INTO authz_outbox
            (id, idempotency_key, op, tuple_user, tuple_relation, tuple_object,
             model_version, status, next_attempt_at, lease_token, lease_expires_at)
        VALUES
            ($1, $2, 'write', $3, 'operator', $4, 1, $5,
             NOW() - INTERVAL '1 minute', $6, {lease_expires_at_sql})
        "#
    );
    sqlx::query(&sql)
        .bind(row_id)
        .bind(format!("test-key-{row_id}"))
        .bind(format!("user:{}", Uuid::new_v4()))
        .bind(format!("tenant:{}", Uuid::new_v4()))
        .bind(status)
        .bind(lease_token)
        .execute(pool)
        .await
        .expect("insert outbox row should succeed");
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
