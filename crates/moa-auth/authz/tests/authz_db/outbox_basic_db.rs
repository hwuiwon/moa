//! Outbox desired-state, convergence, and retry behavior against Postgres.

use httpmock::{Method::POST, MockServer};
use moa_authz::{FgaClient, FgaConfig, OutboxPoller, PollerConfig, enqueue};
use moa_authz_schema::{MODEL_VERSION, ObjectType, Relation, TupleKey, TupleOp, UserType};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

async fn test_pool() -> PgPool {
    let database_url = std::env::var("MOA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let schema_name = format!("authz_outbox_test_{}", Uuid::new_v4().simple());
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
    moa_migrations::run_auth_schema(&pool, &schema_name)
        .await
        .expect("auth schema should apply");
    pool
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn test_tuple() -> (TupleKey, Uuid) {
    let tenant_id = Uuid::new_v4();
    let tuple = TupleKey::new(
        UserType::Operator,
        Uuid::new_v4(),
        Relation::Operator,
        ObjectType::Tenant,
        tenant_id,
    );
    (tuple, tenant_id)
}

/// The single desired-state row for a tuple identity: `(op, generation, status)`.
async fn desired_state(pool: &PgPool, tuple: &TupleKey) -> Option<(String, i64, String)> {
    sqlx::query_as(
        "SELECT op, generation, status FROM authz_outbox
         WHERE tuple_user=$1 AND tuple_relation=$2 AND tuple_object=$3",
    )
    .bind(tuple.user_wire())
    .bind(tuple.relation.to_string())
    .bind(tuple.object_wire())
    .fetch_optional(pool)
    .await
    .expect("desired-state query should succeed")
}

async fn row_count(pool: &PgPool, tuple: &TupleKey) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM authz_outbox
         WHERE tuple_user=$1 AND tuple_relation=$2 AND tuple_object=$3",
    )
    .bind(tuple.user_wire())
    .bind(tuple.relation.to_string())
    .bind(tuple.object_wire())
    .fetch_one(pool)
    .await
    .expect("count query should succeed")
}

fn failing_poller(pool: PgPool, max_attempts: i32) -> OutboxPoller {
    // 127.0.0.1:9 (discard) makes every FGA apply fail, driving the retry path.
    poller_with_url(pool, "http://127.0.0.1:9".to_string(), max_attempts)
}

fn poller_with_url(pool: PgPool, url: String, max_attempts: i32) -> OutboxPoller {
    let client = FgaClient::new(FgaConfig {
        url,
        preshared_key: "test".to_string(),
        store_id: "store".to_string(),
        model_id: "model".to_string(),
        timeout_ms: 5_000,
    })
    .expect("client config should be valid");
    OutboxPoller::new(
        pool,
        client,
        PollerConfig {
            batch_size: 8,
            poll_interval: Duration::from_millis(10),
            max_attempts,
            backoff_base: Duration::from_millis(1),
            backoff_cap: Duration::from_millis(1),
            lease_duration: PollerConfig::default().lease_duration,
        },
    )
}

#[tokio::test]
async fn outbox_same_op_enqueue_is_idempotent_and_holds_generation_db() {
    // Pins: repeated enqueue of the same desired op keeps exactly one row at its
    // initial generation, so unchanged intent never busts the row or resets retry.
    let pool = test_pool().await;
    let (tuple, tenant_id) = test_tuple();

    for _ in 0..3 {
        enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
            .await
            .expect("same-op enqueue should be idempotent");
    }

    assert_eq!(row_count(&pool, &tuple).await, 1);
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("write".to_string(), 1, "pending".to_string()))
    );
}

#[tokio::test]
async fn outbox_alternating_ops_collapse_to_latest_desired_state_db() {
    // Pins: write then delete for one tuple identity yields a single row whose
    // desired op is the latest (delete) with a bumped generation — not two rows,
    // and not a write that permanently owns the identity.
    let pool = test_pool().await;
    let (tuple, tenant_id) = test_tuple();

    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("write enqueue should succeed");
    enqueue(&pool, TupleOp::Delete, &tuple, Some(tenant_id))
        .await
        .expect("delete enqueue should succeed");

    assert_eq!(row_count(&pool, &tuple).await, 1);
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("delete".to_string(), 2, "pending".to_string()))
    );
}

#[tokio::test]
async fn outbox_grant_revoke_grant_converges_to_write_db() {
    // Pins: the F03 regression — grant, revoke, then re-grant must leave the tuple
    // in the granted (write) desired state, not suppressed by the first write.
    let pool = test_pool().await;
    let (tuple, tenant_id) = test_tuple();

    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("grant should succeed");
    enqueue(&pool, TupleOp::Delete, &tuple, Some(tenant_id))
        .await
        .expect("revoke should succeed");
    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("re-grant should succeed");

    assert_eq!(row_count(&pool, &tuple).await, 1);
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("write".to_string(), 3, "pending".to_string()))
    );
}

#[tokio::test]
async fn outbox_revoke_grant_revoke_converges_to_delete_db() {
    // Pins: the inverse sequence — revoke, grant, then re-revoke must converge to
    // the revoked (delete) desired state.
    let pool = test_pool().await;
    let (tuple, tenant_id) = test_tuple();

    enqueue(&pool, TupleOp::Delete, &tuple, Some(tenant_id))
        .await
        .expect("revoke should succeed");
    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("grant should succeed");
    enqueue(&pool, TupleOp::Delete, &tuple, Some(tenant_id))
        .await
        .expect("re-revoke should succeed");

    assert_eq!(row_count(&pool, &tuple).await, 1);
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("delete".to_string(), 3, "pending".to_string()))
    );
}

#[tokio::test]
async fn outbox_poller_applies_latest_desired_state_after_change_db() {
    // Pins: when the desired op changes after the row was picked up (simulated by
    // an in_flight row plus an enqueue of the opposite op), the poller applies the
    // NEW op and converges the row to succeeded at the bumped generation.
    let pool = test_pool().await;
    let (tuple, tenant_id) = test_tuple();

    // Simulate a poller that claimed a write (in_flight, generation 1). The
    // model_version must match what `enqueue` uses, since it is part of the
    // tuple identity the reactivating upsert conflicts on.
    sqlx::query(
        "INSERT INTO authz_outbox
            (op, tuple_user, tuple_relation, tuple_object, model_version, tenant_id,
             generation, status, lease_token, lease_expires_at, next_attempt_at)
         VALUES ('write', $1, $2, $3, $4, $5, 1, 'in_flight', $6,
                 NOW() + INTERVAL '5 minutes', NOW())",
    )
    .bind(tuple.user_wire())
    .bind(tuple.relation.to_string())
    .bind(tuple.object_wire())
    .bind(MODEL_VERSION as i32)
    .bind(tenant_id)
    .bind(Uuid::new_v4())
    .execute(&pool)
    .await
    .expect("seed in_flight row should succeed");

    // A concurrent revoke changes the desired op: the row reactivates as a
    // pending delete at generation 2, releasing the stale lease.
    enqueue(&pool, TupleOp::Delete, &tuple, Some(tenant_id))
        .await
        .expect("revoke should reactivate the row");
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("delete".to_string(), 2, "pending".to_string()))
    );

    let server = MockServer::start();
    let delete = server.mock(|when, then| {
        when.method(POST)
            .path("/stores/store/write")
            .body_contains("deletes");
        then.status(200).json_body(serde_json::json!({}));
    });
    let poller = poller_with_url(pool.clone(), server.base_url(), 4);

    let drained = poller.tick().await.expect("poller tick should complete");

    assert_eq!(drained, 1, "the latest desired op should be applied once");
    delete.assert_hits(1);
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("delete".to_string(), 2, "succeeded".to_string())),
        "poller must converge the row to the latest desired op"
    );
}

#[tokio::test]
async fn outbox_dead_letter_reactivates_on_new_desired_state_db() {
    // Pins: a dead-lettered tuple still accepts a new desired op and returns to
    // pending, so a redrive after terminal failure is not permanently suppressed.
    let pool = test_pool().await;
    let (tuple, tenant_id) = test_tuple();

    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("initial grant should succeed");

    // Exhaust retries against an unreachable FGA to reach dead_letter.
    let poller = failing_poller(pool.clone(), 1);
    poller.tick().await.expect("poller tick should complete");
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("write".to_string(), 1, "dead_letter".to_string()))
    );

    // A new desired op (revoke) must reactivate the dead-lettered identity.
    enqueue(&pool, TupleOp::Delete, &tuple, Some(tenant_id))
        .await
        .expect("revoke should reactivate a dead-lettered tuple");

    assert_eq!(row_count(&pool, &tuple).await, 1);
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("delete".to_string(), 2, "pending".to_string()))
    );
}

#[tokio::test]
async fn outbox_dead_letter_reactivates_on_same_op_redrive_db() {
    // Pins: re-enqueuing the SAME op that dead-lettered still reactivates the row,
    // so a transient outage that exhausted retries can be redriven.
    let pool = test_pool().await;
    let (tuple, tenant_id) = test_tuple();

    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("initial grant should succeed");
    failing_poller(pool.clone(), 1)
        .tick()
        .await
        .expect("poller tick should complete");
    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("write".to_string(), 1, "dead_letter".to_string()))
    );

    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("same-op redrive should reactivate the row");

    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("write".to_string(), 2, "pending".to_string()))
    );
}

#[tokio::test]
async fn outbox_failed_row_moves_to_dead_letter_at_max_attempts_db() {
    // Pins: a non-retryable poller batch stops at dead_letter after max_attempts.
    let pool = test_pool().await;
    let (tuple, tenant_id) = test_tuple();
    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("enqueue should succeed");

    let drained = failing_poller(pool.clone(), 1)
        .tick()
        .await
        .expect("poller tick should complete");
    assert_eq!(drained, 0, "failed rows are not counted as drained");

    assert_eq!(
        desired_state(&pool, &tuple).await,
        Some(("write".to_string(), 1, "dead_letter".to_string()))
    );
    let attempts: i32 = sqlx::query_scalar(
        "SELECT attempts FROM authz_outbox WHERE tuple_object=$1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tuple.object_wire())
    .fetch_one(&pool)
    .await
    .expect("attempts query should succeed");
    assert_eq!(attempts, 1);
}
