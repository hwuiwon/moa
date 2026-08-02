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
async fn concurrent_pollers_claim_disjoint_pending_batches_db() {
    // Pins: concurrent pods claim disjoint batches rather than applying any tuple twice.
    let pool = test_pool().await;
    let server = MockServer::start();
    let write = server.mock(|when, then| {
        when.method(POST).path("/stores/store/write");
        then.status(200).json_body(json!({}));
    });
    let row_ids = [
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    ];
    for row_id in row_ids {
        insert_outbox_row(&pool, row_id, "pending", None, "NULL").await;
    }
    let first = poller_with_url(pool.clone(), 2, server.base_url());
    let second = poller_with_url(pool.clone(), 2, server.base_url());

    let (first_result, second_result) = tokio::join!(first.tick(), second.tick());

    assert_eq!(
        first_result.expect("first tick should complete")
            + second_result.expect("second tick should complete"),
        row_ids.len(),
        "concurrent pollers should drain each pending row exactly once"
    );
    write.assert_hits(row_ids.len());
    let statuses: Vec<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, status, lease_token FROM authz_outbox WHERE id = ANY($1) ORDER BY id",
    )
    .bind(row_ids.as_slice())
    .fetch_all(&pool)
    .await
    .expect("claimed rows should remain readable");
    assert_eq!(statuses.len(), row_ids.len());
    assert!(
        statuses
            .iter()
            .all(|(_, status, lease_token)| status == "succeeded" && lease_token.is_none()),
        "every claimed row should succeed and release its lease: {statuses:?}"
    );
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

#[tokio::test]
async fn targeted_flush_takes_over_live_replica_lease_db() {
    // Pins: a session-creation visibility barrier can deliver its exact object
    // even when another Kubernetes replica already holds a non-expired poller
    // lease; correctness cannot wait for that pod's polling interval.
    let pool = test_pool().await;
    let server = MockServer::start();
    let write = server.mock(|when, then| {
        when.method(POST).path("/stores/store/write");
        then.status(200).json_body(json!({}));
    });
    let row_id = Uuid::new_v4();
    let object = format!("session:{}", Uuid::new_v4());
    insert_outbox_row_for_object(
        &pool,
        row_id,
        "in_flight",
        Some(Uuid::new_v4()),
        "NOW() + INTERVAL '5 minutes'",
        &object,
    )
    .await;
    let poller = poller_with_url(pool.clone(), 1, server.base_url());

    let delivered = poller
        .flush_object(&object)
        .await
        .expect("targeted flush should complete");

    assert_eq!(delivered, 1, "the exact session tuple should be delivered");
    write.assert_hits(1);
    let (status, lease_token): (String, Option<Uuid>) =
        sqlx::query_as("SELECT status, lease_token FROM authz_outbox WHERE id = $1")
            .bind(row_id)
            .fetch_one(&pool)
            .await
            .expect("flushed row should remain readable");
    assert_eq!(status, "succeeded");
    assert_eq!(
        lease_token, None,
        "the visibility barrier releases its lease"
    );
}

#[tokio::test]
async fn targeted_flush_accepts_same_generation_success_without_reapplying_db() {
    // Pins: a replay observes its exact completed receipt and does not issue a
    // duplicate network write merely because the session workflow was retried.
    let pool = test_pool().await;
    let server = MockServer::start();
    let write = server.mock(|when, then| {
        when.method(POST).path("/stores/store/write");
        then.status(200).json_body(json!({}));
    });
    let row_id = Uuid::new_v4();
    let object = format!("session:{}", Uuid::new_v4());
    insert_outbox_row_for_object(&pool, row_id, "succeeded", None, "NULL", &object).await;
    let poller = poller_with_url(pool.clone(), 1, server.base_url());

    let satisfied = poller
        .flush_object(&object)
        .await
        .expect("completed receipt should satisfy the barrier");

    assert_eq!(satisfied, 1);
    write.assert_hits(0);
    let status: String = sqlx::query_scalar("SELECT status FROM authz_outbox WHERE id = $1")
        .bind(row_id)
        .fetch_one(&pool)
        .await
        .expect("completed receipt should remain readable");
    assert_eq!(status, "succeeded");
}

#[tokio::test]
async fn targeted_flush_never_revives_dead_letter_receipt_db() {
    // Pins: explicit synchronous delivery does not erase an exhausted tuple's
    // diagnostics or silently turn a dead letter back into live work.
    let pool = test_pool().await;
    let server = MockServer::start();
    let write = server.mock(|when, then| {
        when.method(POST).path("/stores/store/write");
        then.status(200).json_body(json!({}));
    });
    let row_id = Uuid::new_v4();
    let object = format!("session:{}", Uuid::new_v4());
    insert_outbox_row_for_object(&pool, row_id, "dead_letter", None, "NULL", &object).await;
    let poller = poller_with_url(pool.clone(), 1, server.base_url());

    let error = poller
        .flush_object(&object)
        .await
        .expect_err("dead-lettered receipt must fail closed");

    assert!(
        error.to_string().contains("dead-lettered"),
        "error should name the exhausted receipt: {error}"
    );
    write.assert_hits(0);
    let status: String = sqlx::query_scalar("SELECT status FROM authz_outbox WHERE id = $1")
        .bind(row_id)
        .fetch_one(&pool)
        .await
        .expect("dead-lettered receipt should remain readable");
    assert_eq!(status, "dead_letter");
}

#[tokio::test]
async fn targeted_flush_lookup_uses_object_ordered_index_db() {
    // Pins: the synchronous visibility barrier constrains on one exact object
    // and returns its receipts in id order without scanning or sorting the
    // full authorization outbox.
    let pool = test_pool().await;
    let object = format!("session:{}", Uuid::new_v4());

    sqlx::query(
        r#"
        INSERT INTO authz_outbox
            (id, op, tuple_user, tuple_relation, tuple_object,
             model_version, status)
        SELECT gen_random_uuid(),
               'write',
               'operator:' || series::text,
               'operator',
               CASE WHEN series = 1 THEN $1 ELSE 'session:' || gen_random_uuid()::text END,
               1,
               'succeeded'
        FROM generate_series(1, 20000) AS series
        "#,
    )
    .bind(&object)
    .execute(&pool)
    .await
    .expect("seed a production-sized authz outbox");
    sqlx::query("ANALYZE authz_outbox")
        .execute(&pool)
        .await
        .expect("refresh planner statistics for the outbox corpus");

    let (first_key, second_key, predicate, valid, ready): (
        String,
        String,
        Option<String>,
        bool,
        bool,
    ) = sqlx::query_as(
        r#"
        SELECT pg_get_indexdef(indexrelid, 1, true),
               pg_get_indexdef(indexrelid, 2, true),
               pg_get_expr(indpred, indrelid),
               indisvalid,
               indisready
        FROM pg_index
        WHERE indexrelid = to_regclass('idx_authz_outbox_object_id')
        "#,
    )
    .fetch_one(&pool)
    .await
    .expect("object receipt index should exist");
    assert_eq!(
        (first_key.as_str(), second_key.as_str()),
        ("tuple_object", "id")
    );
    assert_eq!(predicate, None, "every receipt status must be indexed");
    assert!(valid && ready, "object receipt index must be usable");

    let poller = poller(pool.clone(), 1);
    let delivered = poller
        .flush_object(&object)
        .await
        .expect("succeeded receipts should satisfy the visibility barrier");
    assert_eq!(delivered, 1, "the exact-object receipt should be observed");

    let plan = sqlx::query_scalar::<_, String>(
        r#"
        EXPLAIN (COSTS OFF)
        SELECT id, generation, status
        FROM authz_outbox
        WHERE tuple_object = $1
        ORDER BY id
        "#,
    )
    .bind(&object)
    .fetch_all(&pool)
    .await
    .expect("explain the targeted flush receipt lookup")
    .join("\n");
    assert!(
        plan.contains("Index Scan using idx_authz_outbox_object_id"),
        "targeted flush must use the object-leading index:\n{plan}"
    );
    assert!(
        plan.contains("Index Cond: (tuple_object ="),
        "targeted flush must constrain the index by object:\n{plan}"
    );
    assert!(
        !plan.contains("Sort"),
        "targeted flush must read receipts in index order:\n{plan}"
    );
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
    insert_outbox_row_for_object(
        pool,
        row_id,
        status,
        lease_token,
        lease_expires_at_sql,
        &format!("tenant:{}", Uuid::new_v4()),
    )
    .await;
}

async fn insert_outbox_row_for_object(
    pool: &PgPool,
    row_id: Uuid,
    status: &str,
    lease_token: Option<Uuid>,
    lease_expires_at_sql: &str,
    object: &str,
) {
    let sql = format!(
        r#"
        INSERT INTO authz_outbox
            (id, op, tuple_user, tuple_relation, tuple_object,
             model_version, status, next_attempt_at, lease_token, lease_expires_at)
        VALUES
            ($1, 'write', $2, 'operator', $3, 1, $4,
             NOW() - INTERVAL '1 minute', $5, {lease_expires_at_sql})
        "#
    );
    sqlx::query(&sql)
        .bind(row_id)
        .bind(format!("operator:{}", Uuid::new_v4()))
        .bind(object)
        .bind(status)
        .bind(lease_token)
        .execute(pool)
        .await
        .expect("insert outbox row should succeed");
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
