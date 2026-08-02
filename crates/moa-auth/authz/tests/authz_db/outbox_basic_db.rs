//! Outbox desired-state, convergence, and retry behavior against Postgres.

use httpmock::{Method::POST, MockServer};
use moa_authz::{
    AuthzError, FgaClient, FgaConfig, OutboxPoller, PollerConfig, enqueue, enqueue_batch,
};
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

fn tenant_operator_intent(op: TupleOp, user_id: Uuid, tenant_id: Uuid) -> (TupleOp, TupleKey) {
    (
        op,
        TupleKey::new(
            UserType::Operator,
            user_id,
            Relation::Operator,
            ObjectType::Tenant,
            tenant_id,
        ),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
struct RawDesiredState {
    tuple_user: String,
    tuple_relation: String,
    tuple_object: String,
    model_version: i32,
    tenant_id: Option<Uuid>,
    op: String,
    generation: i64,
    status: String,
    attempts: i32,
    last_error: Option<String>,
    lease_token: Option<Uuid>,
    lease_expires_at: Option<String>,
    next_attempt_at: String,
    updated_at: String,
}

async fn raw_desired_states(pool: &PgPool) -> Vec<RawDesiredState> {
    sqlx::query_as(
        "SELECT tuple_user,
                tuple_relation,
                tuple_object,
                model_version,
                tenant_id,
                op,
                generation,
                status,
                attempts,
                last_error,
                lease_token,
                lease_expires_at::TEXT AS lease_expires_at,
                next_attempt_at::TEXT AS next_attempt_at,
                updated_at::TEXT AS updated_at
         FROM authz_outbox
         ORDER BY tuple_user, tuple_relation, tuple_object",
    )
    .fetch_all(pool)
    .await
    .expect("raw desired-state query should succeed")
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

async fn install_tenant_attribution_guard(pool: &PgPool) {
    // The migration lane pins the complete tenant-attribution function. This isolated-schema
    // fixture installs its immutable-attribution branch so the application SQL
    // test does not depend on the developer's shared database migration state.
    sqlx::query(
        r#"
        CREATE FUNCTION guard_authz_outbox_attribution()
        RETURNS TRIGGER
        LANGUAGE plpgsql
        AS $$
        BEGIN
            IF NEW.tenant_id IS DISTINCT FROM OLD.tenant_id THEN
                RAISE EXCEPTION 'authz outbox tuple identity and tenant attribution are immutable'
                    USING ERRCODE = '55000';
            END IF;
            RETURN NEW;
        END;
        $$
        "#,
    )
    .execute(pool)
    .await
    .expect("isolated tenant attribution guard function should install");
    sqlx::query(
        "CREATE TRIGGER authz_outbox_tenant_attribution_guard
         BEFORE UPDATE ON authz_outbox
         FOR EACH ROW EXECUTE FUNCTION guard_authz_outbox_attribution()",
    )
    .execute(pool)
    .await
    .expect("isolated tenant attribution guard trigger should attach");
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
async fn outbox_typed_batch_normalizes_and_converges_each_identity_once_db() {
    // Pins: one typed batch reduces duplicate identities to their final requested
    // state, leaves same-op active delivery state untouched, and resets only an
    // operation change or dead-letter redrive.
    let pool = test_pool().await;
    let tenant_id = Uuid::new_v4();
    let users: [Uuid; 4] = std::array::from_fn(|_| Uuid::new_v4());
    let user_wires = users.map(|user_id| format!("operator:{user_id}"));

    let mut transaction = pool.begin().await.expect("batch transaction should begin");
    enqueue_batch(&mut transaction, tenant_id, &[])
        .await
        .expect("empty batch should be a no-op");
    enqueue_batch(
        &mut transaction,
        tenant_id,
        &[
            tenant_operator_intent(TupleOp::Write, users[0], tenant_id),
            tenant_operator_intent(TupleOp::Delete, users[1], tenant_id),
            tenant_operator_intent(TupleOp::Write, users[2], tenant_id),
        ],
    )
    .await
    .expect("initial raw batch should succeed");
    transaction
        .commit()
        .await
        .expect("initial raw batch should commit");
    assert_eq!(raw_desired_states(&pool).await.len(), 3);

    let lease_token = Uuid::new_v4();
    sqlx::query(
        "UPDATE authz_outbox
         SET status = CASE tuple_user
                 WHEN $1 THEN 'in_flight'
                 WHEN $2 THEN 'dead_letter'
                 ELSE status
             END,
             attempts = CASE tuple_user WHEN $1 THEN 3 WHEN $2 THEN 7 ELSE attempts END,
             last_error = CASE tuple_user WHEN $1 THEN 'retrying' WHEN $2 THEN 'terminal' END,
             lease_token = CASE tuple_user WHEN $1 THEN $3 ELSE lease_token END,
             lease_expires_at = CASE tuple_user
                 WHEN $1 THEN NOW() + INTERVAL '5 minutes'
                 ELSE lease_expires_at
             END,
             next_attempt_at = CASE tuple_user
                 WHEN $1 THEN NOW() + INTERVAL '3 minutes'
                 ELSE next_attempt_at
             END
         WHERE tuple_user IN ($1, $2)",
    )
    .bind(&user_wires[0])
    .bind(&user_wires[1])
    .bind(lease_token)
    .execute(&pool)
    .await
    .expect("delivery states should be seeded");

    let before = raw_desired_states(&pool).await;
    let unchanged_before = before
        .iter()
        .find(|row| row.tuple_user == user_wires[0])
        .expect("same-op row should exist before batch")
        .clone();

    let mut transaction = pool.begin().await.expect("batch transaction should begin");
    enqueue_batch(
        &mut transaction,
        tenant_id,
        &[
            tenant_operator_intent(TupleOp::Delete, users[0], tenant_id),
            tenant_operator_intent(TupleOp::Write, users[0], tenant_id),
            tenant_operator_intent(TupleOp::Delete, users[1], tenant_id),
            tenant_operator_intent(TupleOp::Write, users[2], tenant_id),
            tenant_operator_intent(TupleOp::Delete, users[2], tenant_id),
            tenant_operator_intent(TupleOp::Delete, users[3], tenant_id),
            tenant_operator_intent(TupleOp::Write, users[3], tenant_id),
        ],
    )
    .await
    .expect("converging raw batch should succeed");
    transaction
        .commit()
        .await
        .expect("converging raw batch should commit");

    let after = raw_desired_states(&pool).await;
    assert_eq!(after.len(), 4, "one row must remain per tuple identity");
    assert_eq!(
        after
            .iter()
            .find(|row| row.tuple_user == user_wires[0])
            .expect("same-op row should remain"),
        &unchanged_before,
        "same-op active state, including its lease and timestamps, must not reset"
    );

    let dead_letter = after
        .iter()
        .find(|row| row.tuple_user == user_wires[1])
        .expect("dead-letter row should remain");
    assert_eq!(dead_letter.op, "delete");
    assert_eq!(dead_letter.generation, 2);
    assert_eq!(dead_letter.status, "pending");
    assert_eq!(dead_letter.attempts, 0);
    assert_eq!(dead_letter.last_error, None);
    assert_eq!(dead_letter.lease_token, None);

    let changed = after
        .iter()
        .find(|row| row.tuple_user == user_wires[2])
        .expect("changed-op row should remain");
    assert_eq!(changed.op, "delete");
    assert_eq!(changed.generation, 2);
    assert_eq!(changed.status, "pending");
    assert_eq!(changed.attempts, 0);

    let inserted = after
        .iter()
        .find(|row| row.tuple_user == user_wires[3])
        .expect("new normalized row should exist");
    assert_eq!(inserted.op, "write", "last duplicate intent must win");
    assert_eq!(inserted.generation, 1);
    assert_eq!(inserted.status, "pending");
    assert_eq!(inserted.tenant_id, Some(tenant_id));
    assert_eq!(inserted.model_version, MODEL_VERSION as i32);
}

#[tokio::test]
async fn outbox_typed_batch_cross_tenant_identity_aborts_caller_transaction_db() {
    // Pins: tuple identity and tenant attribution are immutable; attempting to
    // reuse another tenant's identity fails the statement and rolls back earlier
    // outbox work in the caller's transaction.
    let pool = test_pool().await;
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();
    install_tenant_attribution_guard(&pool).await;
    let shared_user_id = Uuid::new_v4();
    let shared_tuple = tenant_operator_intent(TupleOp::Write, shared_user_id, tenant_a).1;

    let mut seed = pool.begin().await.expect("seed transaction should begin");
    enqueue_batch(&mut seed, tenant_a, &[(TupleOp::Write, shared_tuple)])
        .await
        .expect("tenant A seed should succeed");
    seed.commit().await.expect("tenant A seed should commit");

    let marker_tuple = tenant_operator_intent(TupleOp::Write, Uuid::new_v4(), tenant_b).1;
    let mut transaction = pool.begin().await.expect("caller transaction should begin");
    enqueue_batch(
        &mut transaction,
        tenant_b,
        &[(TupleOp::Write, marker_tuple)],
    )
    .await
    .expect("earlier caller work should initially succeed");

    let error = enqueue_batch(
        &mut transaction,
        tenant_b,
        &[(TupleOp::Delete, shared_tuple)],
    )
    .await
    .expect_err("cross-tenant tuple identity must be rejected");
    let AuthzError::Database(error) = error else {
        panic!("cross-tenant collision should return a database error");
    };
    let database_error = error
        .as_database_error()
        .expect("cross-tenant collision should expose structured database details");
    assert_eq!(database_error.code().as_deref(), Some("55000"));
    assert_eq!(
        database_error.message(),
        "authz outbox tuple identity and tenant attribution are immutable"
    );
    assert_eq!(
        database_error.constraint(),
        None,
        "immutable attribution is a trigger contract, not a fabricated constraint failure"
    );
    transaction
        .rollback()
        .await
        .expect("aborted caller transaction should roll back");

    let rows = raw_desired_states(&pool).await;
    assert_eq!(rows.len(), 1, "earlier tenant B work must be rolled back");
    assert_eq!(rows[0].tuple_user, shared_tuple.user_wire());
    assert_eq!(rows[0].tuple_relation, shared_tuple.relation.to_string());
    assert_eq!(rows[0].tuple_object, shared_tuple.object_wire());
    assert_eq!(rows[0].tenant_id, Some(tenant_a));
    assert_eq!(rows[0].op, "write");
    assert_eq!(rows[0].generation, 1);
}

#[tokio::test]
#[ignore = "requires a full database with the bounded tenant-purge migration applied"]
async fn outbox_typed_batch_tenant_purge_fence_aborts_caller_transaction_db() {
    // Pins: the production outbox guard rejects ordinary desired writes for
    // a fenced tenant, and that failure rolls back earlier allowed deletes in the
    // same caller transaction.
    let pool = test_pool().await;
    let tenant_id = Uuid::new_v4();
    let operation_id = format!("authz-batch-fence-{}", Uuid::new_v4());

    sqlx::query(
        "CREATE TRIGGER authz_outbox_tenant_purge_guard
         BEFORE INSERT OR UPDATE ON authz_outbox
         FOR EACH ROW EXECUTE FUNCTION moa.guard_authz_outbox_during_tenant_purge()",
    )
    .execute(&pool)
    .await
    .expect("the production tenant-purge guard should attach to the isolated outbox");

    let mut transaction = pool.begin().await.expect("caller transaction should begin");
    sqlx::query("SELECT moa.start_tenant_purge($1, $2)")
        .bind(tenant_id)
        .bind(&operation_id)
        .execute(&mut *transaction)
        .await
        .expect("tenant purge fence should start");
    enqueue_batch(
        &mut transaction,
        tenant_id,
        &[tenant_operator_intent(
            TupleOp::Delete,
            Uuid::new_v4(),
            tenant_id,
        )],
    )
    .await
    .expect("delete delivery should remain allowed after fencing");

    let error = enqueue_batch(
        &mut transaction,
        tenant_id,
        &[tenant_operator_intent(
            TupleOp::Write,
            Uuid::new_v4(),
            tenant_id,
        )],
    )
    .await
    .expect_err("ordinary desired write must fail behind the tenant-purge fence");
    let AuthzError::Database(error) = error else {
        panic!("tenant fence should return a database error");
    };
    assert_eq!(
        error
            .as_database_error()
            .and_then(|database| database.code())
            .as_deref(),
        Some("55000")
    );
    transaction
        .rollback()
        .await
        .expect("fenced caller transaction should roll back");
    assert_eq!(
        raw_desired_states(&pool).await.len(),
        0,
        "the earlier delete must roll back with the rejected write"
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
