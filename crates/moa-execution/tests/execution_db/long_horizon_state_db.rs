//! Long-horizon execution state, identity, RLS, and generation-fence contracts.

use moa_artifacts::execution_plan::{ExecutionNode, ExecutionOperation};
use moa_execution::repository::ready::{ReadyMaterializationOutcome, ReadyMaterializationRequest};
use moa_execution::repository::task::{
    ActiveAttemptLiveness, NewTaskAttemptCheckpoint, TaskAttemptCheckpointKind,
    TaskAttemptContinuationYieldOutcome, TaskAttemptFence, TaskAttemptProgressOutcome,
    TaskAttemptReleaseClaimOutcome, TaskAttemptStartOutcome, classify_active_attempt_liveness,
};
use moa_execution::repository::trigger::{
    ExecutionTriggerNoOp, ExecutionWatchdogDeferOutcome, ExecutionWatchdogTriggerOutcome,
};

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

/// Admits and starts one active attempt per item key, returning the run and each started fence.
async fn start_admitted_attempts(
    repository: &ExecutionRepository,
    tenant_id: TenantId,
    key: &str,
    config: &ExecutionConfig,
    item_keys: &[&str],
) -> Result<
    (Uuid, Vec<(TaskAttemptFence, chrono::DateTime<Utc>)>),
    Box<dyn std::error::Error + Send + Sync>,
> {
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        key,
        ExecutionRunStatus::Queued,
        budget(item_keys.len() as u64 * 4),
    );
    candidate.plan.definition.nodes = vec![ExecutionNode {
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
    }];
    let run = create_run(repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
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
                    condition_skipped: false,
                    tasks: item_keys
                        .iter()
                        .map(|item| logical_task(run.run_uid, "work", item, estimate(1)))
                        .collect(),
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admitted = repository
        .admit_ready_attempts(config, item_keys.len() as u32, Utc::now())
        .await?
        .admitted;
    assert_eq!(
        admitted.len(),
        item_keys.len(),
        "every ready task must be admitted"
    );
    let mut started = Vec::new();
    for admission in admitted {
        let fence = TaskAttemptFence {
            tenant_id: admission.tenant_id,
            run_uid: admission.run_uid,
            task_id: admission.task_id,
            controller_generation: admission.controller_generation,
            attempt_generation: admission.attempt_generation,
            dispatch_uid: admission.dispatch_uid,
            capacity_reservation_uid: admission.capacity_reservation_uid,
            watchdog_trigger_uid: admission.watchdog_trigger_uid,
            attempt_deadline_at: admission.attempt_deadline_at,
        };
        let TaskAttemptStartOutcome::Started(record) = repository.start_task_attempt(fence).await?
        else {
            panic!("an exactly admitted dispatch must start");
        };
        started.push((fence, record.task.last_progress_at));
    }
    Ok((run.run_uid, started))
}

#[tokio::test]
async fn attempt_heartbeat_keeps_a_progressing_attempt_live_while_a_wedged_one_stalls_db()
-> TestResult {
    // Pins: the durable heartbeat an active slice writes at an in-slice step boundary is what
    // keeps that attempt classified live. Two attempts admitted together and started together
    // diverge only because one committed a step boundary; the attempt that committed nothing is
    // classified stalled while its admission deadline is still eight minutes away, which is the
    // detection latency the heartbeat exists to remove.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        attempt_heartbeat_staleness_seconds: 60,
        active_attempt_timeout_seconds: 600,
        ..ExecutionConfig::default()
    };
    let (run_uid, started) = start_admitted_attempts(
        &repository,
        tenant_id,
        "attempt-heartbeat-staleness",
        &config,
        &["progressing", "wedged"],
    )
    .await?;
    let ((progressing_fence, progressing_started_at), (wedged_fence, wedged_started_at)) =
        (started[0], started[1]);

    // The progressing attempt commits one in-slice step boundary ninety seconds in; the wedged
    // attempt commits nothing after its start.
    let heartbeat_at = progressing_started_at + Duration::seconds(90);
    assert_eq!(
        repository
            .record_task_attempt_progress(progressing_fence, heartbeat_at, None)
            .await?,
        TaskAttemptProgressOutcome::Applied
    );

    let observed_at = progressing_started_at.max(wedged_started_at) + Duration::seconds(120);
    let progressing = repository
        .load_task(scope, run_uid, progressing_fence.task_id)
        .await?
        .expect("the heartbeated attempt must remain visible");
    let wedged = repository
        .load_task(scope, run_uid, wedged_fence.task_id)
        .await?
        .expect("the wedged attempt must remain visible");
    assert_eq!(progressing.last_progress_at, heartbeat_at);
    assert_eq!(wedged.last_progress_at, wedged_started_at);

    assert_eq!(
        classify_active_attempt_liveness(
            &config,
            progressing_fence.attempt_deadline_at,
            progressing.last_progress_at,
            None,
            observed_at,
        ),
        ActiveAttemptLiveness::Live,
        "an attempt that committed a durable step boundary is not stalled"
    );
    assert_eq!(
        classify_active_attempt_liveness(
            &config,
            wedged_fence.attempt_deadline_at,
            wedged.last_progress_at,
            None,
            observed_at,
        ),
        ActiveAttemptLiveness::Stalled,
        "an attempt that committed nothing past the staleness window is stalled"
    );
    assert!(
        wedged_fence.attempt_deadline_at - observed_at >= Duration::minutes(7),
        "the stall must be observable long before the admission deadline exposes it"
    );
    Ok(())
}

#[tokio::test]
async fn declared_step_bound_defers_the_watchdog_it_kept_alive_db() -> TestResult {
    // Pins: when a step declares a bound wider than the staleness floor, the liveness
    // classifier and the watchdog deferral must derive their window from that same bound.
    // Deriving the deferral from the bare floor instead produces a state with no exit: the
    // attempt classifies Live because of the wider bound, but the floor-derived next
    // observation cannot advance past the due time that just fired, so the deferral refuses,
    // the caller treats a still-current receiver as an error, and the delivery retries
    // forever behind the fleet-serialized drain. Only a bound wider than the floor reaches
    // this; an unbounded step defers normally and hides it.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        attempt_heartbeat_staleness_seconds: 2,
        active_attempt_timeout_seconds: 600,
        ..ExecutionConfig::default()
    };
    let (_run_uid, started) = start_admitted_attempts(
        &repository,
        tenant_id,
        "watchdog-step-bound-deferral",
        &config,
        &["bounded"],
    )
    .await?;
    let [(fence, _)] = started[..] else {
        panic!("one attempt must start");
    };

    let armed_due_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT due_at FROM moa.execution_trigger WHERE trigger_uid=$1")
            .bind(fence.watchdog_trigger_uid)
            .fetch_one(&pool)
            .await?;

    // The step declares far more than the two-second floor, as a long sandbox command or an
    // external provider start does.
    let bound_seconds = 120_u32;
    let heartbeat_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT now()").fetch_one(&pool).await?;
    assert_eq!(
        repository
            .record_task_attempt_progress(fence, heartbeat_at, Some(bound_seconds))
            .await?,
        TaskAttemptProgressOutcome::Applied
    );

    // Past the floor, well inside the declared bound: still working, not stalled.
    let observed_at = heartbeat_at + Duration::seconds(30);
    assert_eq!(
        classify_active_attempt_liveness(
            &config,
            fence.attempt_deadline_at,
            heartbeat_at,
            Some(Duration::seconds(i64::from(bound_seconds))),
            observed_at,
        ),
        ActiveAttemptLiveness::Live,
        "a step inside its declared bound is live even past the staleness floor"
    );

    // The deferral must agree, and must move strictly forward. A floor-derived window would
    // land at heartbeat + 2s, which is not past the already-armed due time, and refuse.
    let ExecutionWatchdogDeferOutcome::Deferred { next_due_at } = repository
        .defer_task_attempt_watchdog(scope, &config, fence.watchdog_trigger_uid)
        .await?
    else {
        panic!(
            "an attempt kept alive by its declared step bound must be rearmed on that same bound"
        );
    };
    assert!(
        next_due_at > armed_due_at,
        "the rearmed observation must advance past the due time that just fired"
    );
    assert!(
        next_due_at >= heartbeat_at + Duration::seconds(i64::from(bound_seconds)),
        "the rearmed observation must outlast the bound the step declared"
    );
    assert_eq!(
        repository
            .prepare_watchdog_trigger(scope, fence.watchdog_trigger_uid)
            .await?,
        ExecutionWatchdogTriggerOutcome::NoOp(ExecutionTriggerNoOp::NotDue),
        "a rearmed watchdog must stop firing until its next observation"
    );

    // A recovered delivery can journal Live just before the boundary while the deferral
    // transaction observes the database clock just after it. The same delivery identity must
    // still advance once; otherwise Restate memoizes RetryDelivery and revalidation loops forever.
    let raced_progress_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT now()").fetch_one(&pool).await?;
    assert_eq!(
        repository
            .record_task_attempt_progress(fence, raced_progress_at, None)
            .await?,
        TaskAttemptProgressOutcome::Applied
    );
    let raced_at = raced_progress_at + Duration::seconds(2);
    sqlx::query("UPDATE moa.execution_trigger SET state='pending',due_at=$2 WHERE trigger_uid=$1")
        .bind(fence.watchdog_trigger_uid)
        .bind(raced_at)
        .execute(&pool)
        .await?;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox SET state='pending',not_before_at=$2 \
         WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery'",
    )
    .bind(fence.watchdog_trigger_uid)
    .bind(raced_at)
    .execute(&pool)
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(2_050)).await;
    let ExecutionWatchdogDeferOutcome::Deferred { next_due_at } = repository
        .defer_task_attempt_watchdog(scope, &config, fence.watchdog_trigger_uid)
        .await?
    else {
        panic!("a boundary-crossing RetryDelivery must receive a fresh delivery identity");
    };
    assert!(next_due_at > raced_at);
    Ok(())
}

#[tokio::test]
async fn three_agent_slices_keep_one_logical_task_reservation_and_reset_step_bound_db() -> TestResult
{
    // Pins: bounded continuations release only active capacity. Redispatching the same logical
    // task must keep its original task-budget reservation, while each successor slice starts
    // without inheriting the predecessor step's wider heartbeat bound.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool);
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let (run_uid, started) = start_admitted_attempts(
        &repository,
        tenant_id,
        "three-slice-logical-reservation",
        &config,
        &["agent"],
    )
    .await?;
    let [(mut fence, _)] = started[..] else {
        panic!("one first slice must start");
    };
    let initially_reserved = repository
        .load_run(scope, run_uid)
        .await?
        .expect("run remains visible")
        .reserved;
    assert_eq!(initially_reserved.tasks, 1);

    for completed_slice in 1..=2 {
        let heartbeat_at = moa_test_support::fixtures::pg_now();
        assert_eq!(
            repository
                .record_task_attempt_progress(fence, heartbeat_at, Some(120))
                .await?,
            TaskAttemptProgressOutcome::Applied
        );
        let active = listed_task(&repository, scope, run_uid, fence.task_id).await?;
        assert_eq!(active.progress_step_bound_seconds, Some(120));
        let TaskAttemptReleaseClaimOutcome::Applied(releasing) = repository
            .begin_task_attempt_release(
                fence,
                active.generation,
                "bounded_agent_continuation",
                heartbeat_at,
            )
            .await?
        else {
            panic!("the active slice must enter its release boundary");
        };
        assert_eq!(releasing.task.progress_step_bound_seconds, None);
        let TaskAttemptContinuationYieldOutcome::Applied { task: ready, .. } = repository
            .yield_task_attempt_continuation(NewTaskAttemptCheckpoint {
                fence,
                task_generation: active.generation,
                kind: TaskAttemptCheckpointKind::AgentContinuation,
                schema_version: 1,
                payload: json!({"state":{"kind":"agent","slice":completed_slice}}),
                workspace_release_receipt: None,
                created_at: heartbeat_at,
            })
            .await?
        else {
            panic!("each bounded slice must checkpoint and return the task to ready storage");
        };
        assert_eq!(ready.progress_step_bound_seconds, None);
        assert_eq!(
            repository
                .load_run(scope, run_uid)
                .await?
                .expect("run remains visible")
                .reserved,
            initially_reserved,
            "yield must retain exactly one logical-task reservation"
        );

        let admission = repository
            .admit_ready_attempts(&config, 1, Utc::now())
            .await?
            .admitted
            .into_iter()
            .next()
            .expect("successor bounded slice must be admitted");
        assert_eq!(admission.attempt_generation, fence.attempt_generation + 1);
        fence = TaskAttemptFence {
            tenant_id: admission.tenant_id,
            run_uid: admission.run_uid,
            task_id: admission.task_id,
            controller_generation: admission.controller_generation,
            attempt_generation: admission.attempt_generation,
            dispatch_uid: admission.dispatch_uid,
            capacity_reservation_uid: admission.capacity_reservation_uid,
            watchdog_trigger_uid: admission.watchdog_trigger_uid,
            attempt_deadline_at: admission.attempt_deadline_at,
        };
        let TaskAttemptStartOutcome::Started(started) =
            repository.start_task_attempt(fence).await?
        else {
            panic!("successor admission must start");
        };
        assert_eq!(started.task.progress_step_bound_seconds, None);
        assert_eq!(
            repository
                .load_run(scope, run_uid)
                .await?
                .expect("run remains visible")
                .reserved,
            initially_reserved,
            "redispatch must reuse rather than duplicate the logical-task reservation"
        );
    }
    assert_eq!(fence.attempt_generation, 3, "the third slice is active");
    Ok(())
}

#[tokio::test]
async fn stalled_attempt_watchdog_becomes_deliverable_before_its_deadline_db() -> TestResult {
    // Pins: the watchdog is armed one staleness window ahead of the attempt deadline, so a wedged
    // attempt becomes deliverable minutes before the deadline would have exposed it, while an
    // attempt that keeps committing durable steps is rearmed instead of terminated. This is the
    // difference between detecting a stall and merely classifying one after the fact.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    // A two-second window with a ten-minute deadline keeps the test fast while leaving the two
    // apart by two orders of magnitude, which is exactly the separation the arming change buys.
    let config = ExecutionConfig {
        attempt_heartbeat_staleness_seconds: 2,
        active_attempt_timeout_seconds: 600,
        ..ExecutionConfig::default()
    };
    let (run_uid, started) = start_admitted_attempts(
        &repository,
        tenant_id,
        "watchdog-staleness-delivery",
        &config,
        &["progressing", "wedged", "capped"],
    )
    .await?;
    let [(progressing_fence, _), (wedged_fence, _), (capped_fence, _)] = started[..] else {
        panic!("three attempts must start");
    };

    // The watchdog is armed at the staleness window, not the deadline.
    let armed_due_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT due_at FROM moa.execution_trigger WHERE trigger_uid=$1")
            .bind(wedged_fence.watchdog_trigger_uid)
            .fetch_one(&pool)
            .await?;
    assert!(
        armed_due_at < wedged_fence.attempt_deadline_at,
        "the watchdog must be armed strictly before the attempt deadline"
    );
    assert!(
        wedged_fence.attempt_deadline_at - armed_due_at >= Duration::minutes(9),
        "arming at the staleness window must leave almost the whole deadline unspent"
    );

    // Let the staleness window actually elapse. `last_progress_at` is monotonic in Postgres by
    // trigger, so an aged attempt cannot be simulated by rewinding it.
    tokio::time::sleep(std::time::Duration::from_millis(4_000)).await;

    // The progressing attempt commits a durable step boundary; the wedged one commits nothing.
    let heartbeat_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT now()").fetch_one(&pool).await?;
    for fence in [progressing_fence, capped_fence] {
        assert_eq!(
            repository
                .record_task_attempt_progress(fence, heartbeat_at, None)
                .await?,
            TaskAttemptProgressOutcome::Applied
        );
    }

    // The wedged attempt's watchdog is now genuinely deliverable, and it is a stall rather than a
    // consumed deadline: the deadline is still more than nine minutes away.
    let prepared = repository
        .prepare_watchdog_trigger(scope, wedged_fence.watchdog_trigger_uid)
        .await?;
    let ExecutionWatchdogTriggerOutcome::Task(request) = prepared else {
        panic!("a stalled attempt's watchdog must be deliverable before its deadline");
    };
    assert_eq!(request.task_id, wedged_fence.task_id);
    let wedged = repository
        .load_task(scope, run_uid, wedged_fence.task_id)
        .await?
        .expect("the wedged attempt must remain visible");
    let observed_at: chrono::DateTime<Utc> =
        sqlx::query_scalar("SELECT now()").fetch_one(&pool).await?;
    assert_eq!(
        classify_active_attempt_liveness(
            &config,
            wedged_fence.attempt_deadline_at,
            wedged.last_progress_at,
            None,
            observed_at,
        ),
        ActiveAttemptLiveness::Stalled
    );
    assert!(
        wedged_fence.attempt_deadline_at - observed_at >= Duration::minutes(9),
        "the stall is detected with the whole deadline still unspent"
    );
    // A stalled attempt is never rearmed; its watchdog stays due and terminates it.
    assert_eq!(
        repository
            .defer_task_attempt_watchdog(scope, &config, wedged_fence.watchdog_trigger_uid)
            .await?,
        ExecutionWatchdogDeferOutcome::NotDeferred
    );
    assert!(matches!(
        repository
            .prepare_watchdog_trigger(scope, wedged_fence.watchdog_trigger_uid)
            .await?,
        ExecutionWatchdogTriggerOutcome::Task(_)
    ));

    // The progressing attempt is rearmed for its next observation instead of terminated.
    let ExecutionWatchdogDeferOutcome::Deferred { next_due_at } = repository
        .defer_task_attempt_watchdog(scope, &config, progressing_fence.watchdog_trigger_uid)
        .await?
    else {
        panic!("an attempt that proved progress must be rearmed, not terminated");
    };
    assert_eq!(next_due_at, heartbeat_at + Duration::seconds(2));
    assert!(next_due_at < progressing_fence.attempt_deadline_at);
    assert_eq!(
        repository
            .prepare_watchdog_trigger(scope, progressing_fence.watchdog_trigger_uid)
            .await?,
        ExecutionWatchdogTriggerOutcome::NoOp(ExecutionTriggerNoOp::NotDue),
        "a rearmed watchdog must stop firing until its next observation"
    );
    let (trigger_state, trigger_due_at, dispatch_state, not_before_at): (
        String,
        chrono::DateTime<Utc>,
        String,
        chrono::DateTime<Utc>,
    ) = sqlx::query_as(
        "SELECT trigger.state, trigger.due_at, dispatch.state, dispatch.not_before_at \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.trigger_uid = trigger.trigger_uid \
          AND dispatch.dispatch_kind = 'trigger_delivery' \
         WHERE trigger.trigger_uid = $1",
    )
    .bind(progressing_fence.watchdog_trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (trigger_state.as_str(), dispatch_state.as_str()),
        ("pending", "pending"),
        "a rearm keeps the trigger pending and reuses its one delivery row"
    );
    assert_eq!((trigger_due_at, not_before_at), (next_due_at, next_due_at));
    // A rearm is not a supersede, so the trigger keeps its scheduled_triggers receipt.
    let receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_dispatch_outbox \
         WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery'",
    )
    .bind(progressing_fence.watchdog_trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(receipts, 1, "a rearm must not orphan a second delivery row");

    // The deadline is the hard backstop: an attempt whose next observation would land past the
    // deadline is rearmed only as far as the deadline itself.
    sqlx::query(
        "UPDATE moa.execution_task SET attempt_deadline_at=$2 WHERE run_uid=$1 AND task_id=$3",
    )
    .bind(run_uid)
    .bind(heartbeat_at + Duration::milliseconds(1_500))
    .bind(capped_fence.task_id.as_uuid())
    .execute(&pool)
    .await?;
    assert_eq!(
        repository
            .defer_task_attempt_watchdog(scope, &config, capped_fence.watchdog_trigger_uid)
            .await?,
        ExecutionWatchdogDeferOutcome::Deferred {
            next_due_at: heartbeat_at + Duration::milliseconds(1_500)
        },
        "deferral must never push an observation past the attempt deadline"
    );

    // A superseded controller generation cannot be rearmed.
    sqlx::query("UPDATE moa.execution_run SET controller_generation=controller_generation+1 WHERE run_uid=$1")
        .bind(run_uid)
        .execute(&pool)
        .await?;
    assert_eq!(
        repository
            .defer_task_attempt_watchdog(scope, &config, progressing_fence.watchdog_trigger_uid)
            .await?,
        ExecutionWatchdogDeferOutcome::NotDeferred,
        "a generation-stale watchdog must never be rearmed"
    );
    Ok(())
}
