//! Long-horizon execution state, identity, RLS, and generation-fence contracts.

use super::support::*;

#[tokio::test]
async fn admitted_identity_and_activation_checkpoint_round_trip_exactly_db() -> TestResult {
    // Pins: the authenticated admission principal and bounded-controller checkpoint are
    // canonical Postgres state, and exact activation replays do not mutate them twice.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "long-horizon-identity",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.admitted_identity = Identity {
        identity_type: IdentityType::Service,
        id: Uuid::now_v7(),
        tenant_id,
        api_key_id: Some(Uuid::now_v7()),
        acting_on_behalf_of: Some(Uuid::now_v7()),
    };
    let expected_identity = candidate.admitted_identity.clone();

    let created = create_run(&repository, scope, candidate).await?;
    assert_eq!(created.admitted_identity, expected_identity);
    let stored_identity: serde_json::Value =
        sqlx::query_scalar("SELECT admitted_identity FROM moa.execution_run WHERE run_uid = $1")
            .bind(created.run_uid)
            .fetch_one(test_db.store().pool())
            .await?;
    assert_eq!(stored_identity, serde_json::to_value(&expected_identity)?);
    assert_eq!(created.controller_generation, 1);
    assert_eq!(created.activation_state, ExecutionActivationState::Queued);
    assert_eq!(created.ready_task_count, 0);
    assert_eq!(created.active_task_count, 0);
    assert_eq!(created.last_progress_at, created.created_at);

    let RunActivationWriteOutcome::Applied(claimed) = repository
        .claim_run_activation(scope, created.run_uid, 1)
        .await?
    else {
        panic!("current queued generation must be claimed");
    };
    assert_eq!(
        claimed.activation_state,
        ExecutionActivationState::Advancing
    );
    assert!(claimed.last_progress_at >= created.last_progress_at);
    assert_eq!(
        repository
            .claim_run_activation(scope, created.run_uid, 1)
            .await?,
        RunActivationWriteOutcome::AlreadyApplied(claimed.clone())
    );
    assert_eq!(
        repository
            .claim_run_activation(scope, created.run_uid, 2)
            .await?,
        RunActivationWriteOutcome::GenerationMismatch
    );

    let next_wake_at = pg_deadline(Duration::minutes(5));
    let checkpoint = ExecutionRunActivationCheckpoint {
        status: ExecutionRunStatus::Running,
        activation_state: ExecutionActivationState::Idle,
        next_wake_at: Some(next_wake_at),
        waiting_since: None,
        ready_task_count: 2,
        active_task_count: 1,
    };
    let RunActivationWriteOutcome::Applied(parked) = repository
        .checkpoint_run_activation(scope, created.run_uid, 1, checkpoint.clone())
        .await?
    else {
        panic!("claimed generation must persist its checkpoint");
    };
    assert_eq!(parked.status, ExecutionRunStatus::Running);
    assert_eq!(parked.activation_state, ExecutionActivationState::Idle);
    assert_eq!(parked.next_wake_at, Some(next_wake_at));
    assert_eq!(parked.ready_task_count, 2);
    assert_eq!(parked.active_task_count, 1);
    assert_eq!(
        repository
            .checkpoint_run_activation(scope, created.run_uid, 1, checkpoint)
            .await?,
        RunActivationWriteOutcome::AlreadyApplied(parked)
    );
    Ok(())
}

#[tokio::test]
async fn long_horizon_children_are_tenant_isolated_and_cross_tenant_fenced_db() -> TestResult {
    // Pins: every V59 child row is visible only to its tenant, and composite foreign keys
    // prevent a child from attaching to a run owned by a different tenant.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let owner_tenant = TenantId::new();
    let other_tenant = TenantId::new();
    let run = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: owner_tenant,
        },
        new_run(
            owner_tenant,
            None,
            "long-horizon-rls",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let node_state_uid = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO moa.execution_node_state \
         (node_state_uid, tenant_id, run_uid, node_id, node_order) \
         VALUES ($1, $2, $3, 'collect', 0)",
    )
    .bind(node_state_uid)
    .bind(owner_tenant.0)
    .bind(run.run_uid)
    .execute(&pool)
    .await?;

    let mut owner = moa_db::ScopedConn::begin_tenant(&pool, owner_tenant).await?;
    owner.assume_app_role().await?;
    let owner_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_node_state WHERE node_state_uid = $1",
    )
    .bind(node_state_uid)
    .fetch_one(owner.as_mut())
    .await?;
    owner.commit().await?;
    assert_eq!(owner_count, 1);

    let mut other = moa_db::ScopedConn::begin_tenant(&pool, other_tenant).await?;
    other.assume_app_role().await?;
    let other_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_node_state WHERE node_state_uid = $1",
    )
    .bind(node_state_uid)
    .fetch_one(other.as_mut())
    .await?;
    other.commit().await?;
    assert_eq!(other_count, 0);
    assert_eq!(
        repository
            .claim_run_activation(
                ExecutionScope::Tenant {
                    tenant_id: other_tenant,
                },
                run.run_uid,
                1,
            )
            .await?,
        RunActivationWriteOutcome::NotFound
    );

    let cross_tenant = sqlx::query(
        "INSERT INTO moa.execution_node_state \
         (node_state_uid, tenant_id, run_uid, node_id, node_order) \
         VALUES ($1, $2, $3, 'cross-tenant', 1)",
    )
    .bind(Uuid::now_v7())
    .bind(other_tenant.0)
    .bind(run.run_uid)
    .execute(&pool)
    .await;
    assert_db_error_contains(cross_tenant, "execution_node_state_run_tenant_fk");
    Ok(())
}

#[tokio::test]
async fn attempt_generation_and_long_horizon_guards_reject_stale_or_invalid_state_db() -> TestResult
{
    // Pins: repository task mutations fence both logical and attempt generations, while
    // immutable identity, monotonic generations, and nonnegative counters fail closed.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "long-horizon-generation",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let tasks = repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![logical_task(run.run_uid, "collect", "one", estimate(1))],
        )
        .await?;
    let task = &tasks[0];
    assert_eq!(task.attempt_generation, task.generation);
    assert_eq!(task.active_dispatch_uid, None);
    assert_eq!(task.dispatch_sequence, 0);
    assert_eq!(task.attempt_state, ExecutionAttemptState::Idle);
    assert!(task.attempt_started_at.is_none());
    assert!(task.attempt_deadline_at.is_none());
    assert!(task.waiting_since.is_none());
    assert!(task.ready_at.is_none());
    assert!(task.external_job_uid.is_none());

    sqlx::query("UPDATE moa.execution_task SET attempt_generation = 2 WHERE task_id = $1")
        .bind(task.task_id.as_uuid())
        .execute(&pool)
        .await?;
    assert_eq!(
        repository
            .reserve_task(scope, run.run_uid, task.task_id, 1)
            .await?,
        ReservationOutcome::Rejected(ReservationRejection::GenerationMismatch)
    );

    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_task SET attempt_generation = 1 WHERE task_id = $1")
            .bind(task.task_id.as_uuid())
            .execute(&pool)
            .await,
        "attempt generation must be monotonic",
    );
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_run SET ready_task_count = -1 WHERE run_uid = $1")
            .bind(run.run_uid)
            .execute(&pool)
            .await,
        "execution_run_ready_task_count_check",
    );
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_run SET admitted_identity = \
             jsonb_set(admitted_identity, '{id}', to_jsonb($2::TEXT)) WHERE run_uid = $1",
        )
        .bind(run.run_uid)
        .bind(Uuid::now_v7().to_string())
        .execute(&pool)
        .await,
        "execution run admitted identity is immutable",
    );
    Ok(())
}
