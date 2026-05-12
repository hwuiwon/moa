//! Outbox enqueue and retry behavior against Postgres.

use moa_authz::{FgaClient, FgaConfig, OutboxPoller, PollerConfig, enqueue};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

async fn test_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("POSTGRES_URL"))
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:10040/moa".to_string());
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .expect("test Postgres should be reachable");
    moa_authz::schema::migrate(&pool)
        .await
        .expect("authz migrations should apply");
    pool
}

#[tokio::test]
async fn outbox_basic_enqueue_is_idempotent_on_same_key() {
    // Pins: repeated enqueue of the same tuple operation creates exactly one outbox row.
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let tuple = TupleKey::new(
        UserType::User,
        user_id,
        Relation::Editor,
        ObjectType::Workspace,
        workspace_id,
    );

    enqueue(&pool, TupleOp::Write, &tuple, None)
        .await
        .expect("first enqueue should succeed");
    enqueue(&pool, TupleOp::Write, &tuple, None)
        .await
        .expect("second enqueue should be idempotent");
    enqueue(&pool, TupleOp::Write, &tuple, None)
        .await
        .expect("third enqueue should be idempotent");

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM authz_outbox WHERE tuple_user=$1 AND tuple_relation=$2 AND tuple_object=$3",
    )
    .bind(tuple.user_wire())
    .bind(tuple.relation.to_string())
    .bind(tuple.object_wire())
    .fetch_one(&pool)
    .await
    .expect("count query should succeed");
    assert_eq!(
        count, 1,
        "three enqueues of the same key must produce exactly one row"
    );
}

#[tokio::test]
async fn outbox_basic_enqueue_separates_write_and_delete() {
    // Pins: write and delete use distinct idempotency keys for the same tuple.
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    let tuple = TupleKey::new(
        UserType::User,
        user_id,
        Relation::Editor,
        ObjectType::Workspace,
        workspace_id,
    );

    enqueue(&pool, TupleOp::Write, &tuple, None)
        .await
        .expect("write enqueue should succeed");
    enqueue(&pool, TupleOp::Delete, &tuple, None)
        .await
        .expect("delete enqueue should succeed");

    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM authz_outbox WHERE tuple_user=$1 AND tuple_relation=$2 AND tuple_object=$3",
    )
    .bind(tuple.user_wire())
    .bind(tuple.relation.to_string())
    .bind(tuple.object_wire())
    .fetch_one(&pool)
    .await
    .expect("count query should succeed");
    assert_eq!(
        count, 2,
        "write and delete must produce separate outbox rows"
    );
}

#[tokio::test]
async fn outbox_basic_failed_row_moves_to_dead_letter_at_max_attempts() {
    // Pins: a non-retryable poller batch stops at dead_letter after max_attempts.
    let pool = test_pool().await;
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let tuple = TupleKey::new(
        UserType::User,
        user_id,
        Relation::Member,
        ObjectType::Tenant,
        tenant_id,
    );
    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("enqueue should succeed");

    let client = FgaClient::new(FgaConfig {
        url: "http://127.0.0.1:9".to_string(),
        preshared_key: "test".to_string(),
        store_id: "store".to_string(),
        model_id: "model".to_string(),
        timeout_ms: 200,
    })
    .expect("client config should be valid");
    let poller = OutboxPoller::new(
        pool.clone(),
        client,
        PollerConfig {
            batch_size: 8,
            poll_interval: Duration::from_millis(10),
            max_attempts: 1,
            backoff_base: Duration::from_millis(1),
            backoff_cap: Duration::from_millis(1),
        },
    );

    let drained = poller.tick().await.expect("poller tick should complete");
    assert_eq!(drained, 0, "failed rows are not counted as drained");

    let (status, attempts): (String, i32) = sqlx::query_as(
        "SELECT status, attempts FROM authz_outbox WHERE tuple_object=$1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tuple.object_wire())
    .fetch_one(&pool)
    .await
    .expect("status query should succeed");
    assert_eq!(status, "dead_letter");
    assert_eq!(attempts, 1);
}
