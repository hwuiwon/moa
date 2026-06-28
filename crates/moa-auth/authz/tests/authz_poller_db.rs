//! DB-backed authz outbox poller recovery tests.

use httpmock::{Method::POST, MockServer};
use moa_authz::{FgaClient, FgaConfig, OutboxPoller, PollerConfig};
use serde_json::json;
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

#[tokio::test]
async fn poller_drains_pending_row_to_succeeded_db() {
    // Pins: when OpenFGA accepts the write (200), the poller counts the row as
    // drained and transitions it to 'succeeded' with the lease released — the
    // happy path that previously only existed against live OpenFGA.
    let pool = test_pool().await;
    let server = MockServer::start();
    let write = server.mock(|when, then| {
        when.method(POST).path("/stores/store/write");
        then.status(200).json_body(json!({}));
    });
    let row_id = Uuid::new_v4();
    insert_outbox_row(&pool, row_id, "pending", None, "NULL").await;
    let poller = poller_with_url(pool.clone(), 1, server.base_url());

    let drained = poller.tick().await.expect("poller tick should complete");

    assert_eq!(
        drained, 1,
        "an accepted FGA write counts as one drained row"
    );
    write.assert_hits(1);
    let (status, lease_token, last_error): (String, Option<Uuid>, Option<String>) =
        sqlx::query_as("SELECT status, lease_token, last_error FROM authz_outbox WHERE id = $1")
            .bind(row_id)
            .fetch_one(&pool)
            .await
            .expect("drained row should remain readable");
    assert_eq!(status, "succeeded");
    assert_eq!(lease_token, None, "a succeeded row releases its lease");
    assert_eq!(last_error, None, "a succeeded row has no retry diagnostics");
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
    moa_migrations::run_auth_schema(&pool, &schema_name)
        .await
        .expect("auth baseline should apply");
    pool
}

fn poller(pool: PgPool, batch_size: usize) -> OutboxPoller {
    // 127.0.0.1:9 (discard) makes every FGA write fail; failure-path tests rely on it.
    poller_with_url(pool, batch_size, "http://127.0.0.1:9".to_string())
}

fn poller_with_url(pool: PgPool, batch_size: usize, url: String) -> OutboxPoller {
    let client = FgaClient::new(FgaConfig {
        url,
        preshared_key: "test".to_string(),
        store_id: "store".to_string(),
        model_id: "model".to_string(),
        timeout_ms: 5_000,
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
