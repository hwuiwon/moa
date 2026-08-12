//! Fleet-owned weighted-fair task admission contracts.

use chrono::DateTime;
use moa_artifacts::execution_plan::{ExecutionNode, ExecutionOperation};
use moa_config::ExecutionConfig;
use moa_db::ScopedConn;
use moa_execution::repository::ready::ReadyMaterializationRequest;
use moa_execution::repository::{
    capacity::ExecutionAdmissionBatch, ready::ReadyMaterializationOutcome,
};

use super::support::*;

fn output_node() -> ExecutionNode {
    ExecutionNode {
        id: "work".to_string(),
        requirement_ids: vec!["req".to_string()],
        depends_on: Vec::new(),
        when: None,
        input: json!({}),
        output_schema: json!({ "type": "object" }),
        operation: ExecutionOperation::Output { value: json!({}) },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
        },
        budget: None,
    }
}

async fn ready_run(
    repository: &ExecutionRepository,
    tenant_id: TenantId,
    key: &str,
    task_count: u64,
) -> Result<Uuid, moa_execution::Error> {
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        key,
        ExecutionRunStatus::Queued,
        budget(task_count.saturating_mul(2)),
    );
    candidate.plan.definition.nodes = vec![output_node()];
    let run = create_run(repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    let tasks = (0..task_count)
        .map(|index| logical_task(run.run_uid, "work", &format!("{index:04}"), estimate(1)))
        .collect();
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &ExecutionConfig::default(),
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    tasks,
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    Ok(run.run_uid)
}

#[tokio::test]
async fn tenant_capacity_scope_shares_one_fleet_bucket_without_cross_tenant_access_db() -> TestResult
{
    // Pins: tenant-scoped admission can create, read, and update its own bucket plus the one
    // shared fleet sentinel; another tenant reuses that fleet row but cannot observe or mutate
    // the first tenant's bucket, and fleet owner coordinates remain immutable.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let fleet_uid = Uuid::now_v7();
    let tenant_a_uid = Uuid::now_v7();
    let tenant_b_uid = Uuid::now_v7();

    let mut tenant_a_conn = ScopedConn::begin_tenant(&pool, tenant_a).await?;
    tenant_a_conn.assume_app_role().await?;
    sqlx::query(
        "INSERT INTO moa.execution_capacity_bucket ( \
             capacity_bucket_uid,scope_kind,tenant_id,resource_dimension,limit_value \
         ) VALUES ($1,'fleet',NULL,'active_runs',100), \
                  ($2,'tenant',$3,'active_runs',10)",
    )
    .bind(fleet_uid)
    .bind(tenant_a_uid)
    .bind(tenant_a.0)
    .execute(tenant_a_conn.as_mut())
    .await?;
    sqlx::query(
        "UPDATE moa.execution_capacity_bucket SET reserved_quantity=reserved_quantity+1 \
         WHERE resource_dimension='active_runs'",
    )
    .execute(tenant_a_conn.as_mut())
    .await?;
    let tenant_a_rows: Vec<(String, Option<Uuid>, i64)> = sqlx::query_as(
        "SELECT scope_kind,tenant_id,reserved_quantity \
         FROM moa.execution_capacity_bucket ORDER BY scope_kind",
    )
    .fetch_all(tenant_a_conn.as_mut())
    .await?;
    assert_eq!(
        tenant_a_rows,
        vec![
            ("fleet".to_string(), None, 1),
            ("tenant".to_string(), Some(tenant_a.0), 1),
        ]
    );
    sqlx::query("SAVEPOINT cross_tenant_insert")
        .execute(tenant_a_conn.as_mut())
        .await?;
    let cross_tenant_insert = sqlx::query(
        "INSERT INTO moa.execution_capacity_bucket ( \
             capacity_bucket_uid,scope_kind,tenant_id,resource_dimension,limit_value \
         ) VALUES ($1,'tenant',$2,'active_tasks',10)",
    )
    .bind(Uuid::now_v7())
    .bind(tenant_b.0)
    .execute(tenant_a_conn.as_mut())
    .await;
    assert!(cross_tenant_insert.is_err());
    sqlx::query("ROLLBACK TO SAVEPOINT cross_tenant_insert")
        .execute(tenant_a_conn.as_mut())
        .await?;
    sqlx::query("SAVEPOINT fleet_owner_mutation")
        .execute(tenant_a_conn.as_mut())
        .await?;
    let fleet_owner_mutation = sqlx::query(
        "UPDATE moa.execution_capacity_bucket SET resource_dimension='active_tasks' \
         WHERE capacity_bucket_uid=$1",
    )
    .bind(fleet_uid)
    .execute(tenant_a_conn.as_mut())
    .await;
    assert!(fleet_owner_mutation.is_err());
    sqlx::query("ROLLBACK TO SAVEPOINT fleet_owner_mutation")
        .execute(tenant_a_conn.as_mut())
        .await?;
    tenant_a_conn.commit().await?;

    let mut tenant_b_conn = ScopedConn::begin_tenant(&pool, tenant_b).await?;
    tenant_b_conn.assume_app_role().await?;
    sqlx::query(
        "INSERT INTO moa.execution_capacity_bucket ( \
             capacity_bucket_uid,scope_kind,tenant_id,resource_dimension,limit_value \
         ) VALUES ($1,'fleet',NULL,'active_runs',100) ON CONFLICT DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .execute(tenant_b_conn.as_mut())
    .await?;
    sqlx::query(
        "INSERT INTO moa.execution_capacity_bucket ( \
             capacity_bucket_uid,scope_kind,tenant_id,resource_dimension,limit_value \
         ) VALUES ($1,'tenant',$2,'active_runs',10)",
    )
    .bind(tenant_b_uid)
    .bind(tenant_b.0)
    .execute(tenant_b_conn.as_mut())
    .await?;
    sqlx::query(
        "UPDATE moa.execution_capacity_bucket SET reserved_quantity=reserved_quantity+1 \
         WHERE scope_kind='fleet' AND resource_dimension='active_runs'",
    )
    .execute(tenant_b_conn.as_mut())
    .await?;
    let tenant_a_visible: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_capacity_bucket WHERE tenant_id=$1)",
    )
    .bind(tenant_a.0)
    .fetch_one(tenant_b_conn.as_mut())
    .await?;
    assert!(!tenant_a_visible);
    let cross_tenant_update =
        sqlx::query("UPDATE moa.execution_capacity_bucket SET limit_value=11 WHERE tenant_id=$1")
            .bind(tenant_a.0)
            .execute(tenant_b_conn.as_mut())
            .await?;
    assert_eq!(cross_tenant_update.rows_affected(), 0);
    let cross_tenant_delete =
        sqlx::query("DELETE FROM moa.execution_capacity_bucket WHERE tenant_id=$1")
            .bind(tenant_a.0)
            .execute(tenant_b_conn.as_mut())
            .await?;
    assert_eq!(cross_tenant_delete.rows_affected(), 0);
    tenant_b_conn.commit().await?;

    let mut control = ScopedConn::begin_control_plane(&pool).await?;
    control.assume_app_role().await?;
    let fleet: (i64, i64) = sqlx::query_as(
        "SELECT count(*),max(reserved_quantity) \
         FROM moa.execution_capacity_bucket \
         WHERE scope_kind='fleet' AND resource_dimension='active_runs'",
    )
    .fetch_one(control.as_mut())
    .await?;
    assert_eq!(fleet, (1, 2));
    let tenant_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT tenant_id FROM moa.execution_capacity_bucket \
         WHERE scope_kind='tenant' AND resource_dimension='active_runs' ORDER BY tenant_id",
    )
    .fetch_all(control.as_mut())
    .await?;
    let mut expected_tenants = vec![tenant_a.0, tenant_b.0];
    expected_tenants.sort_unstable();
    assert_eq!(tenant_ids, expected_tenants);
    control.commit().await?;
    Ok(())
}

#[tokio::test]
async fn weighted_admission_is_atomic_bounded_and_fleet_owned_db() -> TestResult {
    // Pins: one globally serialized admission transaction preserves the fleet ceiling,
    // persists 2:1 weighted fairness, and commits task/outbox/watchdog/counters together.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let heavy_tenant = TenantId::new();
    let normal_tenant = TenantId::new();
    let heavy_run = ready_run(&repository, heavy_tenant, "capacity-heavy", 6).await?;
    let normal_run = ready_run(&repository, normal_tenant, "capacity-normal", 6).await?;
    sqlx::query(
        "UPDATE moa.execution_tenant_dispatch_state SET weight = 2 \
         WHERE tenant_id = $1",
    )
    .bind(heavy_tenant.0)
    .execute(&pool)
    .await?;

    let config = ExecutionConfig {
        max_fleet_active_tasks: 6,
        max_tenant_active_tasks: 6,
        max_in_flight_tasks: 12,
        ..ExecutionConfig::default()
    };
    let ExecutionAdmissionBatch { admitted, .. } = repository
        .admit_ready_attempts(&config, 6, Utc::now())
        .await?;
    assert_eq!(admitted.len(), 6);
    let heavy_count = admitted
        .iter()
        .filter(|item| item.tenant_id == heavy_tenant)
        .count();
    let normal_count = admitted
        .iter()
        .filter(|item| item.tenant_id == normal_tenant)
        .count();
    assert_eq!((heavy_count, normal_count), (4, 2));
    assert!(admitted.iter().all(|item| {
        !item.dispatch_uid.is_nil()
            && !item.capacity_reservation_uid.is_nil()
            && !item.watchdog_trigger_uid.is_nil()
            && !item.watchdog_dispatch_uid.is_nil()
    }));

    let fleet_reserved: i64 = sqlx::query_scalar(
        "SELECT reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE scope_kind = 'fleet' AND resource_dimension = 'active_tasks'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(fleet_reserved, 6);
    let active_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_task WHERE run_uid = ANY($1::UUID[]) \
         AND status = 'dispatching'",
    )
    .bind(vec![heavy_run, normal_run])
    .fetch_one(&pool)
    .await?;
    assert_eq!(active_rows, 6);
    let outbox_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_dispatch_outbox \
         WHERE dispatch_kind = 'task_attempt' AND state = 'pending' \
           AND run_uid = ANY($1::UUID[])",
    )
    .bind(vec![heavy_run, normal_run])
    .fetch_one(&pool)
    .await?;
    assert_eq!(outbox_rows, 6);

    let second = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?;
    assert!(second.admitted.is_empty(), "fleet ceiling must be exact");
    Ok(())
}

#[tokio::test]
async fn requested_admission_limit_is_a_hard_bound_independent_of_vec_capacity_db() -> TestResult {
    // Pins: a dispatcher request for one attempt admits exactly one durable task/outbox/watchdog
    // tuple even when fleet, tenant, run, and allocator capacity could accept a larger batch.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let run_uid = ready_run(&repository, tenant_id, "capacity-request-bound", 3).await?;
    let config = ExecutionConfig {
        max_fleet_active_tasks: 10,
        max_tenant_active_tasks: 10,
        max_in_flight_tasks: 10,
        ..ExecutionConfig::default()
    };

    let batch = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?;
    assert_eq!(batch.admitted.len(), 1);
    let state_counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
           count(*) FILTER (WHERE status='dispatching'), \
           count(*) FILTER (WHERE status='ready'), \
           (SELECT count(*) FROM moa.execution_dispatch_outbox \
             WHERE run_uid=$1 AND dispatch_kind='task_attempt' AND state='pending') \
         FROM moa.execution_task WHERE run_uid=$1",
    )
    .bind(run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(state_counts, (1, 2, 1));
    Ok(())
}

#[tokio::test]
async fn future_ready_task_is_admitted_only_at_its_persisted_due_time_db() -> TestResult {
    // Pins: retry backoff remains storage-owned even when a dispatcher runs early; the exact
    // task stays Ready without an outbox or capacity receipt until the supplied clock reaches it.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let run_uid = ready_run(&repository, tenant_id, "capacity-future-ready", 1).await?;
    let observed_at = pg_deadline(Duration::zero());
    let ready_at = observed_at + Duration::seconds(10);
    sqlx::query(
        "UPDATE moa.execution_task SET ready_at=$2, last_progress_at=$2, updated_at=NOW() \
         WHERE run_uid=$1 AND status='ready'",
    )
    .bind(run_uid)
    .bind(ready_at)
    .execute(&pool)
    .await?;

    let config = ExecutionConfig::default();
    let early = repository
        .admit_ready_attempts(&config, 1, observed_at)
        .await?;
    assert!(
        early.admitted.is_empty(),
        "future-ready task must not consume attempt capacity"
    );
    let early_state: (String, DateTime<Utc>, i64, i64) = sqlx::query_as(
        "SELECT task.status, task.ready_at, \
             (SELECT count(*) FROM moa.execution_dispatch_outbox AS dispatch \
              WHERE dispatch.run_uid=task.run_uid AND dispatch.dispatch_kind='task_attempt'), \
             (SELECT count(*) FROM moa.execution_capacity_reservation AS capacity \
              WHERE capacity.run_uid=task.run_uid AND capacity.resource_dimension='active_tasks') \
         FROM moa.execution_task AS task WHERE task.run_uid=$1",
    )
    .bind(run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        early_state,
        ("ready".to_string(), ready_at, 0, 0),
        "early admission must leave the storage-owned backoff intact"
    );
    let due = repository
        .admit_ready_attempts(&config, 1, ready_at)
        .await?;
    assert_eq!(due.admitted.len(), 1);
    assert_eq!(due.admitted[0].run_uid, run_uid);
    let due_state: (String, Option<DateTime<Utc>>, i64, i64) = sqlx::query_as(
        "SELECT task.status, task.ready_at, \
             (SELECT count(*) FROM moa.execution_dispatch_outbox AS dispatch \
              WHERE dispatch.run_uid=task.run_uid AND dispatch.dispatch_kind='task_attempt'), \
             (SELECT count(*) FROM moa.execution_capacity_reservation AS capacity \
              WHERE capacity.run_uid=task.run_uid AND capacity.resource_dimension='active_tasks' \
                AND capacity.state='reserved') \
         FROM moa.execution_task AS task WHERE task.run_uid=$1",
    )
    .bind(run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        due_state,
        ("dispatching".to_string(), None, 1, 1),
        "due admission must create one exact task attempt"
    );
    Ok(())
}

#[tokio::test]
async fn no_work_admission_does_not_rewrite_unchanged_capacity_bucket_db() -> TestResult {
    // Pins: polling an empty ready queue keeps the fleet admission lock and configured limit,
    // but does not create a new bucket version when no durable capacity state changes.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = ExecutionConfig::default();

    let first = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?;
    assert!(first.admitted.is_empty());
    let version_after_first_poll: i64 = sqlx::query_scalar(
        "SELECT version FROM moa.execution_capacity_bucket \
         WHERE scope_kind='fleet' AND resource_dimension='active_tasks'",
    )
    .fetch_one(&pool)
    .await?;

    let second = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?;
    assert!(second.admitted.is_empty());
    let version_after_second_poll: i64 = sqlx::query_scalar(
        "SELECT version FROM moa.execution_capacity_bucket \
         WHERE scope_kind='fleet' AND resource_dimension='active_tasks'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        version_after_second_poll, version_after_first_poll,
        "a no-work prelock must not rewrite an unchanged capacity bucket"
    );
    let changed_limit = config.max_fleet_active_tasks.saturating_add(1);
    let changed_config = ExecutionConfig {
        max_fleet_active_tasks: changed_limit,
        ..config
    };
    let changed = repository
        .admit_ready_attempts(&changed_config, 1, Utc::now())
        .await?;
    assert!(changed.admitted.is_empty());
    let changed_bucket: (i64, i64) = sqlx::query_as(
        "SELECT limit_value, version FROM moa.execution_capacity_bucket \
         WHERE scope_kind='fleet' AND resource_dimension='active_tasks'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        changed_bucket,
        (
            i64::from(changed_limit),
            version_after_second_poll.saturating_add(1)
        ),
        "a real configured-limit change must remain durable and versioned"
    );
    Ok(())
}
