//! Outbox enqueue and retry behavior against Postgres.

use moa_authz::{FgaClient, FgaConfig, OutboxPoller, PollerConfig, enqueue};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
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

#[tokio::test]
async fn outbox_basic_enqueue_is_idempotent_on_same_key() {
    // Pins: repeated enqueue of the same tuple operation creates exactly one outbox row.
    let pool = test_pool().await;
    let user_id = Uuid::new_v4();
    let tenant_id = Uuid::new_v4();
    let tuple = TupleKey::new(
        UserType::Operator,
        user_id,
        Relation::Operator,
        ObjectType::Tenant,
        tenant_id,
    );

    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("first enqueue should succeed");
    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("second enqueue should be idempotent");
    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
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
    let tenant_id = Uuid::new_v4();
    let tuple = TupleKey::new(
        UserType::Operator,
        user_id,
        Relation::Operator,
        ObjectType::Tenant,
        tenant_id,
    );

    enqueue(&pool, TupleOp::Write, &tuple, Some(tenant_id))
        .await
        .expect("write enqueue should succeed");
    enqueue(&pool, TupleOp::Delete, &tuple, Some(tenant_id))
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
        UserType::Operator,
        user_id,
        Relation::Operator,
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
            lease_duration: PollerConfig::default().lease_duration,
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
