//! Live outbox poller test against a running OpenFGA.

use moa_authz::{FgaClient, FgaConfig, OutboxPoller, PollerConfig, enqueue};
use moa_authz_schema::{ObjectType, Relation, TupleKey, TupleOp, UserType};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::time::Duration;
use uuid::Uuid;

fn fga_from_env() -> FgaConfig {
    FgaConfig {
        url: std::env::var("MOA_OPENFGA_URL").expect("MOA_OPENFGA_URL"),
        preshared_key: std::env::var("MOA_OPENFGA_PRESHARED_KEY")
            .expect("MOA_OPENFGA_PRESHARED_KEY"),
        store_id: std::env::var("MOA_OPENFGA_STORE_ID").expect("MOA_OPENFGA_STORE_ID"),
        model_id: std::env::var("MOA_OPENFGA_MODEL_ID").expect("MOA_OPENFGA_MODEL_ID"),
        timeout_ms: 5000,
    }
}

async fn test_pool() -> PgPool {
    let database_url = std::env::var("DATABASE_URL")
        .or_else(|_| std::env::var("POSTGRES_URL"))
        .unwrap_or_else(|_| "postgres://moa_owner:dev@localhost:25432/moa".to_string());
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&database_url)
        .await
        .expect("test Postgres should be reachable");
    moa_authz::schema::migrate(&pool)
        .await
        .expect("authz migrations should apply");
    pool
}

#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_OPENFGA_TESTS=1 and live OpenFGA"]
async fn poller_drains_write_to_fga() {
    // Pins: the poller applies a queued tenant-member tuple to live OpenFGA.
    if std::env::var("MOA_RUN_LIVE_OPENFGA_TESTS").as_deref() != Ok("1") {
        return;
    }

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

    let client = FgaClient::new(fga_from_env()).expect("FGA config should be valid");
    let poller = OutboxPoller::new(
        pool.clone(),
        client.clone(),
        PollerConfig {
            poll_interval: Duration::from_millis(10),
            ..PollerConfig::default()
        },
    );

    let drained = poller.tick().await.expect("poller tick should succeed");
    assert_eq!(drained, 1, "exactly one row drained");

    let allowed = client
        .check(
            &format!("user:{user_id}"),
            "member",
            &format!("tenant:{tenant_id}"),
        )
        .await
        .expect("live FGA check should succeed");
    assert!(allowed, "tuple write should make user a tenant member");

    let status: String =
        sqlx::query_scalar("SELECT status FROM authz_outbox WHERE tuple_object=$1 LIMIT 1")
            .bind(tuple.object_wire())
            .fetch_one(&pool)
            .await
            .expect("status query should succeed");
    assert_eq!(status, "succeeded");

    enqueue(&pool, TupleOp::Delete, &tuple, Some(tenant_id))
        .await
        .expect("cleanup delete enqueue should succeed");
    let cleanup_drained = poller
        .tick()
        .await
        .expect("cleanup poller tick should succeed");
    assert_eq!(cleanup_drained, 1, "cleanup delete should drain one row");
}
