//! Temporal-trigger, dispatch-outbox, and asynchronous-job PostgreSQL contracts.

use std::{collections::HashSet, time::Duration as StdDuration};

use super::support::*;
use chrono::DateTime;
use moa_artifacts::execution_plan::{
    ExecutionFailureClass, ExecutionNode, ExecutionOperation, ExecutionTaskOutcome,
    ExecutionTaskResult,
};
use moa_config::ExecutionConfig;
use moa_core::{
    types::completion::ToolInvocation,
    types::identifiers::{ExecutionRunScopeId, ExecutionTaskScopeId},
    types::sandbox_workspace::{ExecutionHandReleaseOwner, ExecutionHandReleaseReceipt},
    types::tools::IdempotencyClass,
};
use moa_execution::repository::run::enqueue_run_activation_in_conn;
use moa_execution::repository::{
    external_job::{
        ExecutionExternalJobBinding, ExecutionExternalJobCallback,
        ExecutionExternalJobCallbackOutcome, ExecutionExternalJobCallbackUpdate,
        ExecutionExternalJobCancellation, ExecutionExternalJobCancellationOutcome,
        ExecutionExternalJobOwner, ExecutionExternalJobStartRecoveryAdoptionOutcome,
        ExecutionExternalJobState, NewExecutionExternalJobIntent,
    },
    outbox::{
        ExecutionDeliveryState, ExecutionDispatchFailureOutcome, ExecutionDispatchKind,
        ExecutionDispatchRetryPolicy, ExecutionMaintenanceJobKind,
        ExecutionMaintenanceSettlementOutcome, NewExecutionDispatch,
    },
    trigger::{
        ExecutionExternalStartRecoveryRearmOutcome, ExecutionExternalStartRecoveryTriggerOutcome,
        ExecutionRunDeadlineTriggerOutcome, ExecutionTriggerFireOutcome, ExecutionTriggerKind,
        ExecutionTriggerNoOp, ExecutionTriggerSupersedeOutcome, ExecutionWatchdogTriggerOutcome,
        NewExecutionTrigger, create_trigger_with_dispatch_in_conn, supersede_trigger_in_conn,
    },
};
use moa_execution::repository::{
    ready::{ReadyMaterializationOutcome, ReadyMaterializationRequest},
    task::{
        NewTaskAttemptCheckpoint, ReleasedTaskAttemptCapacityOutcome,
        ResolveTaskAttemptReviewRequest, TaskAttemptCheckpointKind,
        TaskAttemptCheckpointWriteOutcome, TaskAttemptExternalOutcome, TaskAttemptFence,
        TaskAttemptReleaseClaimOutcome, TaskAttemptReviewParkOutcome,
        TaskAttemptReviewResolutionOutcome, TaskAttemptSettlementOutcome, TaskAttemptStartOutcome,
    },
    terminal::PendingTerminalAdvanceOutcome,
};
use moa_execution::wire::{
    ExecutionActionReviewResolution, ExecutionExternalJobStartRecoveryOwner,
    ExecutionExternalJobStartRecoveryRequest,
};

fn watchdog_output_node() -> ExecutionNode {
    let mut node = output_node("watchdog-work");
    node.requirement_ids = vec!["req".to_string()];
    node
}

fn output_node(id: &str) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: Vec::new(),
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

#[tokio::test]
async fn task_start_uses_post_lock_progress_time_db() -> TestResult {
    // Pins: a task-start transaction whose PostgreSQL NOW() predates a contended run lock still
    // advances both task and run progress monotonically after the lock owner commits newer times.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "task-start-post-lock-time",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![watchdog_output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "watchdog-work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(
                        run.run_uid,
                        "watchdog-work",
                        "one",
                        estimate(1),
                    )],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admitted = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one task must be admitted");
    let fence = TaskAttemptFence {
        tenant_id: admitted.tenant_id,
        run_uid: admitted.run_uid,
        task_id: admitted.task_id,
        controller_generation: admitted.controller_generation,
        attempt_generation: admitted.attempt_generation,
        dispatch_uid: admitted.dispatch_uid,
        capacity_reservation_uid: admitted.capacity_reservation_uid,
        watchdog_trigger_uid: admitted.watchdog_trigger_uid,
        attempt_deadline_at: admitted.attempt_deadline_at,
    };

    let mut lock_owner = pool.begin().await?;
    sqlx::query("SELECT run_uid FROM moa.execution_run WHERE run_uid=$1 FOR UPDATE")
        .bind(run.run_uid)
        .fetch_one(&mut *lock_owner)
        .await?;
    let start_repository = repository.clone();
    let start = tokio::spawn(async move { start_repository.start_task_attempt(fence).await });
    tokio::time::sleep(StdDuration::from_millis(100)).await;
    let newer_progress: DateTime<Utc> = sqlx::query_scalar(
        "UPDATE moa.execution_task SET last_progress_at=clock_timestamp() \
         WHERE run_uid=$1 AND task_id=$2 RETURNING last_progress_at",
    )
    .bind(run.run_uid)
    .bind(admitted.task_id.as_uuid())
    .fetch_one(&mut *lock_owner)
    .await?;
    sqlx::query("UPDATE moa.execution_run SET last_progress_at=$2 WHERE run_uid=$1")
        .bind(run.run_uid)
        .bind(newer_progress)
        .execute(&mut *lock_owner)
        .await?;
    lock_owner.commit().await?;

    assert!(matches!(
        tokio::time::timeout(StdDuration::from_secs(5), start).await???,
        TaskAttemptStartOutcome::Started(_)
    ));
    let persisted: (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT run.last_progress_at, task.last_progress_at \
         FROM moa.execution_run AS run JOIN moa.execution_task AS task USING (run_uid) \
         WHERE run.run_uid=$1 AND task.task_id=$2",
    )
    .bind(run.run_uid)
    .bind(admitted.task_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert!(persisted.0 >= newer_progress);
    assert!(persisted.1 >= newer_progress);
    Ok(())
}

#[tokio::test]
async fn trigger_creation_is_atomic_and_firing_is_due_generation_fenced_db() -> TestResult {
    // Pins: a trigger and its fallback delivery commit together; early delivery does not
    // advance state; one current due generation wakes once; stale and duplicate delivery no-op.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let execution_config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "trigger-atomic-generation",
            ExecutionRunStatus::Queued,
            budget_without_deadline(10),
        ),
    )
    .await?;

    let rolled_back_uid = Uuid::now_v7();
    let rolled_back = run_deadline(
        rolled_back_uid,
        tenant_id,
        run.run_uid,
        1,
        pg_deadline(Duration::minutes(1)),
    );
    let mut transaction = pool.begin().await?;
    create_trigger_with_dispatch_in_conn(&mut transaction, &execution_config, &rolled_back).await?;
    transaction.rollback().await?;
    let (trigger_count, dispatch_count): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM moa.execution_trigger WHERE trigger_uid = $1), \
                (SELECT count(*) FROM moa.execution_dispatch_outbox WHERE trigger_uid = $1)",
    )
    .bind(rolled_back_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!((trigger_count, dispatch_count), (0, 0));

    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox SET state='delivered',delivered_at=NOW(),updated_at=NOW() \
         WHERE run_uid=$1 AND state='pending'",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;

    let future_due_at = pg_deadline(Duration::minutes(5));
    let mut future_request = new_run(
        tenant_id,
        None,
        "future-trigger",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    future_request.approved_budget.deadline_at = Some(future_due_at);
    let future_run = create_run(&repository, scope, future_request).await?;
    let (future_uid, future_dispatch_uid): (Uuid, Uuid) = sqlx::query_as(
        "SELECT trigger.trigger_uid, dispatch.dispatch_uid \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         WHERE trigger.run_uid=$1 AND trigger.trigger_kind='run_deadline'",
    )
    .bind(future_run.run_uid)
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox SET state='delivered', delivered_at=NOW(), \
         updated_at=NOW() WHERE run_uid=$1 AND dispatch_kind='run_activation'",
    )
    .bind(future_run.run_uid)
    .execute(&pool)
    .await?;
    // Pins: creating a future trigger persists no process-local timer; the indexed outbox head is
    // the sole normal timing authority, remains unclaimable early, and becomes the due delivery.
    let wake = repository.next_pending_dispatch_wake(scope).await?;
    assert_eq!(wake.dispatch_uid, Some(future_dispatch_uid));
    assert_eq!(wake.next_due_at, Some(future_due_at));
    assert!(
        repository
            .claim_due_dispatches(scope, "future-trigger-owner", 1, StdDuration::from_secs(30))
            .await?
            .is_empty()
    );
    assert_eq!(
        repository.fire_trigger(scope, future_uid).await?,
        ExecutionTriggerFireOutcome::NoOp(ExecutionTriggerNoOp::NotDue)
    );
    let future_dispatch_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_dispatch_outbox WHERE dispatch_uid = $1",
    )
    .bind(future_dispatch_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(future_dispatch_state, "pending");
    let mut transaction = pool.begin().await?;
    assert_eq!(
        supersede_trigger_in_conn(
            &mut transaction,
            future_uid,
            ExecutionTriggerKind::RunDeadline,
            Some(1),
            None,
            None,
            None,
        )
        .await?,
        ExecutionTriggerSupersedeOutcome::Superseded
    );
    transaction.commit().await?;

    let due_at = pg_deadline(Duration::minutes(-1));
    let mut due_request = new_run(
        tenant_id,
        None,
        "due-trigger",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    due_request.approved_budget.deadline_at = Some(due_at);
    let due_run = create_run(&repository, scope, due_request).await?;
    let (due_uid, due_dispatch_uid): (Uuid, Uuid) = sqlx::query_as(
        "SELECT trigger.trigger_uid, dispatch.dispatch_uid \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         WHERE trigger.run_uid=$1 AND trigger.trigger_kind='run_deadline'",
    )
    .bind(due_run.run_uid)
    .fetch_one(&pool)
    .await?;
    sqlx::query("DELETE FROM moa.execution_dispatch_outbox WHERE dispatch_uid = $1")
        .bind(due_dispatch_uid)
        .execute(&pool)
        .await?;
    let repaired = repository
        .reconcile_due_trigger_dispatches(scope, 10)
        .await?;
    assert_eq!(repaired.len(), 1);
    let repaired_dispatch_uid = repaired[0].dispatch_uid;
    assert_ne!(
        repaired_dispatch_uid, due_dispatch_uid,
        "reconstructing a lost delivery must escape the missing Restate identity"
    );
    let ExecutionTriggerFireOutcome::Delivered {
        activation: Some(activation),
    } = repository.fire_trigger(scope, due_uid).await?
    else {
        panic!("current due run trigger must enqueue one activation");
    };
    assert_eq!(activation.kind, ExecutionDispatchKind::RunActivation);
    assert_eq!(activation.controller_generation, Some(1));
    assert_eq!(
        repository.fire_trigger(scope, due_uid).await?,
        ExecutionTriggerFireOutcome::NoOp(ExecutionTriggerNoOp::Duplicate)
    );
    let due_dispatch_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_dispatch_outbox WHERE dispatch_uid = $1",
    )
    .bind(repaired_dispatch_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(due_dispatch_state, "delivered");

    let stale_due_at = pg_deadline(Duration::seconds(-1));
    let mut stale_request = new_run(
        tenant_id,
        None,
        "stale-trigger",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    stale_request.approved_budget.deadline_at = Some(stale_due_at);
    let stale_run = create_run(&repository, scope, stale_request).await?;
    let (stale_uid, stale_dispatch_uid): (Uuid, Uuid) = sqlx::query_as(
        "SELECT trigger.trigger_uid, dispatch.dispatch_uid \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         WHERE trigger.run_uid=$1 AND trigger.trigger_kind='run_deadline'",
    )
    .bind(stale_run.run_uid)
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET budget_deadline_suspended_at=clock_timestamp() \
         WHERE run_uid=$1",
    )
    .bind(stale_run.run_uid)
    .execute(&pool)
    .await?;
    assert_eq!(
        repository.fire_trigger(scope, stale_uid).await?,
        ExecutionTriggerFireOutcome::NoOp(ExecutionTriggerNoOp::StaleGeneration)
    );
    let (trigger_state, dispatch_state): (String, String) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         WHERE trigger.trigger_uid = $1",
    )
    .bind(stale_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        (trigger_state.as_str(), dispatch_state.as_str()),
        ("superseded", "cancelled")
    );

    let activation_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND dispatch_kind = 'run_activation' \
           AND payload->>'trigger_uid'=$2",
    )
    .bind(due_run.run_uid)
    .bind(due_uid.to_string())
    .fetch_one(&pool)
    .await?;
    assert_eq!(activation_count, 1);
    let stale_dispatch_state: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_dispatch_outbox WHERE dispatch_uid=$1")
            .bind(stale_dispatch_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(stale_dispatch_state, "cancelled");
    Ok(())
}

#[tokio::test]
async fn trigger_fire_missing_existing_capacity_bucket_rolls_back_db() -> TestResult {
    // Pins: receipt-backed trigger fire fails closed when one exact canonical bucket is missing;
    // neither trigger nor outbox state advances in the aborted transaction.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut request = new_run(
        tenant_id,
        None,
        "trigger-missing-capacity-bucket",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    request.approved_budget.deadline_at = Some(pg_deadline(Duration::seconds(-1)));
    let run = create_run(&repository, scope, request).await?;
    let trigger_uid: Uuid = sqlx::query_scalar(
        "SELECT trigger_uid FROM moa.execution_trigger \
         WHERE run_uid=$1 AND trigger_kind='run_deadline' AND state='pending'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "DELETE FROM moa.execution_capacity_bucket \
         WHERE scope_kind='tenant' AND tenant_id=$1 AND resource_dimension='parked_runs'",
    )
    .bind(tenant_id.0)
    .execute(&pool)
    .await?;

    let error = repository
        .fire_trigger(scope, trigger_uid)
        .await
        .expect_err("missing canonical capacity must fail trigger fire");
    assert!(
        error
            .to_string()
            .contains("missing canonical capacity buckets during existing-row prelock")
    );
    let states: (String, String) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         WHERE trigger.trigger_uid=$1",
    )
    .bind(trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(states, ("pending".to_string(), "pending".to_string()));
    Ok(())
}

#[tokio::test]
async fn trigger_fire_prelocks_existing_capacity_without_reconciling_limits_db() -> TestResult {
    // Pins: firing a committed run trigger locks all six receipt-backed capacity keys in canonical
    // order without rewriting persisted limits from mutable runtime configuration.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut request = new_run(
        tenant_id,
        None,
        "trigger-existing-capacity-prelock",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    request.approved_budget.deadline_at = Some(pg_deadline(Duration::seconds(-1)));
    let run = create_run(&repository, scope, request).await?;
    let trigger_uid: Uuid = sqlx::query_scalar(
        "SELECT trigger_uid FROM moa.execution_trigger \
         WHERE run_uid=$1 AND trigger_kind='run_deadline' AND state='pending'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_capacity_bucket SET limit_value=limit_value+100 \
         WHERE resource_dimension IN ('active_runs','parked_runs','scheduled_triggers') \
           AND (scope_kind='fleet' OR (scope_kind='tenant' AND tenant_id=$1))",
    )
    .bind(tenant_id.0)
    .execute(&pool)
    .await?;
    let limits = || async {
        sqlx::query_as::<_, (String, Option<Uuid>, String, i64)>(
            "SELECT scope_kind,tenant_id,resource_dimension,limit_value \
             FROM moa.execution_capacity_bucket \
             WHERE resource_dimension IN ('active_runs','parked_runs','scheduled_triggers') \
               AND (scope_kind='fleet' OR (scope_kind='tenant' AND tenant_id=$1)) \
             ORDER BY resource_dimension,scope_kind",
        )
        .bind(tenant_id.0)
        .fetch_all(&pool)
        .await
    };
    let before = limits().await?;
    assert_eq!(before.len(), 6);

    assert!(matches!(
        repository.fire_trigger(scope, trigger_uid).await?,
        ExecutionTriggerFireOutcome::Delivered { .. }
    ));
    assert_eq!(limits().await?, before);
    Ok(())
}

#[tokio::test]
async fn paused_run_deadline_race_reprepares_current_fences_before_trigger_settlement_db()
-> TestResult {
    // Pins: a pause that wins after deadline preparation cannot consume the only absolute
    // deadline trigger. The stale fence conflicts with the trigger/capacity still active;
    // re-preparation adopts the paused run's current generation and enforces the elapsed deadline.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = execution_capacity_config();
    let mut request = new_run(
        tenant_id,
        None,
        "paused-deadline-race",
        ExecutionRunStatus::Queued,
        budget(1),
    );
    request.approved_budget.deadline_at = Some(pg_deadline(Duration::minutes(-1)));
    let RunAdmissionOutcome::Admitted(run) =
        create_run_with_config(&repository, scope, &config, request).await?
    else {
        panic!("deadline race fixture must be admitted");
    };
    let trigger_uid: Uuid = sqlx::query_scalar(
        "SELECT trigger_uid FROM moa.execution_trigger \
         WHERE run_uid=$1 AND trigger_kind='run_deadline' AND state='pending'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    let ExecutionRunDeadlineTriggerOutcome::Ready {
        run_uid,
        controller_generation: stale_generation,
        wake_epoch: stale_wake_epoch,
        observed_at: stale_observed_at,
    } = repository
        .prepare_run_deadline_trigger(scope, trigger_uid)
        .await?
    else {
        panic!("elapsed deadline must prepare against the admitted run fence");
    };

    let TransitionOutcome::RunApplied(paused) = repository
        .pause_run(scope, &config, run_uid, stale_generation)
        .await?
    else {
        panic!("zero-active-task run must pause immediately");
    };
    assert_eq!(paused.status, ExecutionRunStatus::Paused);
    assert!(paused.controller_generation > stale_generation);
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET state='delivered', delivered_at=NOW()-INTERVAL '2 minutes', updated_at=NOW() \
         WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery'",
    )
    .bind(trigger_uid)
    .execute(&pool)
    .await?;
    let repaired = repository
        .reconcile_due_trigger_dispatches(scope, 3)
        .await?;
    assert!(
        repaired
            .iter()
            .any(|dispatch| dispatch.trigger_uid == Some(trigger_uid)),
        "a paused run's immutable absolute deadline must survive Restate delivery-state loss"
    );
    let repaired_boundary: (String, String, String) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state, capacity.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         JOIN moa.execution_capacity_reservation AS capacity USING (trigger_uid) \
         WHERE trigger.trigger_uid=$1",
    )
    .bind(trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        repaired_boundary,
        (
            "pending".to_string(),
            "pending".to_string(),
            "reserved".to_string(),
        ),
        "reconciliation must redrive, not supersede, a paused run deadline"
    );
    assert_eq!(
        repository
            .fence_deadline_and_enqueue_settlement(
                &config,
                scope,
                run_uid,
                stale_generation,
                stale_wake_epoch,
                stale_observed_at,
                1,
            )
            .await?,
        PendingTerminalAdvanceOutcome::Conflict
    );
    let active_boundary: (String, String, String) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state, capacity.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         JOIN moa.execution_capacity_reservation AS capacity USING (trigger_uid) \
         WHERE trigger.trigger_uid=$1",
    )
    .bind(trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        active_boundary,
        (
            "pending".to_string(),
            "pending".to_string(),
            "reserved".to_string(),
        ),
        "a stale deadline fence must not consume its sole recovery trigger"
    );

    let ExecutionRunDeadlineTriggerOutcome::Ready {
        controller_generation,
        wake_epoch,
        observed_at,
        ..
    } = repository
        .prepare_run_deadline_trigger(scope, trigger_uid)
        .await?
    else {
        panic!("paused elapsed deadline must reprepare against the current run fence");
    };
    assert_eq!(controller_generation, paused.controller_generation);
    assert_eq!(wake_epoch, paused.wake_epoch);
    assert!(matches!(
        repository
            .fence_deadline_and_enqueue_settlement(
                &config,
                scope,
                run_uid,
                controller_generation,
                wake_epoch,
                observed_at,
                1,
            )
            .await?,
        PendingTerminalAdvanceOutcome::Applied(_) | PendingTerminalAdvanceOutcome::Replayed(_)
    ));
    assert!(matches!(
        repository
            .settle_run_deadline_trigger(scope, trigger_uid)
            .await?,
        ExecutionTriggerSupersedeOutcome::Superseded
            | ExecutionTriggerSupersedeOutcome::AlreadySuperseded
            | ExecutionTriggerSupersedeOutcome::AlreadyInactive
    ));
    let settled_boundary: (String, String, String) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state, capacity.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         JOIN moa.execution_capacity_reservation AS capacity USING (trigger_uid) \
         WHERE trigger.trigger_uid=$1",
    )
    .bind(trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        settled_boundary,
        (
            "superseded".to_string(),
            "cancelled".to_string(),
            "released".to_string(),
        )
    );
    Ok(())
}

#[tokio::test]
async fn run_deadline_settlement_locks_scheduled_capacity_before_trigger_db() -> TestResult {
    // Pins: terminal draining locks capacity before trigger rows. A concurrent deadline
    // settlement must wait without holding the trigger, preventing a capacity-trigger cycle.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = execution_capacity_config();
    let mut request = new_run(
        tenant_id,
        None,
        "deadline-capacity-lock-order",
        ExecutionRunStatus::Queued,
        budget(1),
    );
    request.approved_budget.deadline_at = Some(pg_deadline(Duration::minutes(5)));
    let RunAdmissionOutcome::Admitted(run) =
        create_run_with_config(&repository, scope, &config, request).await?
    else {
        panic!("deadline lock-order fixture must be admitted");
    };
    let trigger_uid: Uuid = sqlx::query_scalar(
        "SELECT trigger_uid FROM moa.execution_trigger \
         WHERE tenant_id=$1 AND run_uid=$2 AND trigger_kind='run_deadline' AND state='pending'",
    )
    .bind(tenant_id.0)
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;

    let mut capacity_holder = pool.begin().await?;
    for (scope_kind, owner) in [("fleet", None), ("tenant", Some(tenant_id.0))] {
        sqlx::query(
            "SELECT capacity_bucket_uid FROM moa.execution_capacity_bucket \
             WHERE scope_kind=$1 AND tenant_id IS NOT DISTINCT FROM $2 \
               AND resource_dimension='scheduled_triggers' FOR UPDATE",
        )
        .bind(scope_kind)
        .bind(owner)
        .fetch_one(&mut *capacity_holder)
        .await?;
    }

    let settlement_repository = repository.clone();
    let mut settlement = tokio::spawn(async move {
        settlement_repository
            .settle_run_deadline_trigger(scope, trigger_uid)
            .await
    });
    assert!(
        tokio::time::timeout(StdDuration::from_millis(100), &mut settlement)
            .await
            .is_err(),
        "settlement must wait on the prelocked ScheduledTriggers bucket"
    );
    sqlx::query(
        "SELECT trigger_uid FROM moa.execution_trigger \
         WHERE trigger_uid=$1 FOR UPDATE NOWAIT",
    )
    .bind(trigger_uid)
    .fetch_one(&mut *capacity_holder)
    .await?;
    capacity_holder.commit().await?;

    assert_eq!(
        tokio::time::timeout(StdDuration::from_secs(5), settlement).await???,
        ExecutionTriggerSupersedeOutcome::Superseded
    );
    Ok(())
}

#[tokio::test]
async fn reconciliation_redrives_accepted_trigger_and_run_dispatches_after_restate_loss_db()
-> TestResult {
    // Pins: after total Restate state loss, a sufficiently old accepted trigger delivery and
    // run activation are requeued with the same immutable dispatch IDs only while their exact
    // database generations remain current.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    assert!(
        repository
            .reconcile_due_trigger_dispatches(scope, 2)
            .await
            .is_err(),
        "a reconciliation batch must fund trigger, accepted-dispatch, and run lanes"
    );
    let due_at = pg_deadline(Duration::minutes(-2));
    let mut request = new_run(
        tenant_id,
        None,
        "restate-loss-redrive",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    request.approved_budget.deadline_at = Some(due_at);
    let run = create_run(&repository, scope, request).await?;
    let (trigger_uid, trigger_dispatch_uid): (Uuid, Uuid) = sqlx::query_as(
        "SELECT trigger.trigger_uid, dispatch.dispatch_uid \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch \
           ON dispatch.trigger_uid=trigger.trigger_uid \
          AND dispatch.dispatch_kind='trigger_delivery' \
         WHERE trigger.run_uid=$1 AND trigger.trigger_kind='run_deadline'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    let mut transaction = pool.begin().await?;
    let activation = enqueue_run_activation_in_conn(
        &mut transaction,
        tenant_id,
        run.run_uid,
        run.controller_generation,
        pg_deadline(Duration::minutes(-2)),
        json!({"source": "restate-loss-test"}),
    )
    .await?;
    transaction.commit().await?;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET state = 'delivered', delivered_at = now() - interval '2 minutes', \
             delivery_attempts = 4, updated_at = now() - interval '2 minutes' \
         WHERE dispatch_uid = ANY($1)",
    )
    .bind(vec![trigger_dispatch_uid, activation.dispatch_uid])
    .execute(&pool)
    .await?;

    let repaired = repository
        .reconcile_due_trigger_dispatches(scope, 10)
        .await?;
    let repaired_ids = repaired
        .iter()
        .map(|dispatch| dispatch.dispatch_uid)
        .collect::<HashSet<_>>();
    assert_eq!(
        repaired_ids,
        HashSet::from([trigger_dispatch_uid, activation.dispatch_uid])
    );
    let rows: Vec<(Uuid, String, Option<DateTime<Utc>>, i32)> = sqlx::query_as(
        "SELECT dispatch_uid, state, delivered_at, delivery_attempts \
         FROM moa.execution_dispatch_outbox WHERE dispatch_uid = ANY($1) \
         ORDER BY dispatch_uid",
    )
    .bind(vec![trigger_dispatch_uid, activation.dispatch_uid])
    .fetch_all(&pool)
    .await?;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|(_, state, delivered_at, attempts)| {
        state == "pending" && delivered_at.is_none() && *attempts == 0
    }));

    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET state = 'delivered', delivered_at = now() - interval '2 minutes', \
             updated_at = now() - interval '2 minutes' \
         WHERE dispatch_uid = ANY($1)",
    )
    .bind(vec![trigger_dispatch_uid, activation.dispatch_uid])
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run \
         SET controller_generation = controller_generation + 1, \
             budget_deadline_suspended_at = clock_timestamp(), updated_at = now() \
         WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    assert!(
        repository
            .reconcile_due_trigger_dispatches(scope, 10)
            .await?
            .is_empty()
    );
    let stale_states: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT dispatch_uid, state FROM moa.execution_dispatch_outbox \
         WHERE dispatch_uid = ANY($1) ORDER BY dispatch_uid",
    )
    .bind(vec![trigger_dispatch_uid, activation.dispatch_uid])
    .fetch_all(&pool)
    .await?;
    assert!(stale_states.iter().all(|(_, state)| state == "delivered"));
    let trigger_state: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_trigger WHERE trigger_uid = $1")
            .bind(trigger_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(trigger_state, "superseded");
    Ok(())
}

#[tokio::test]
async fn trigger_capacity_saturates_atomically_and_releases_once_db() -> TestResult {
    // Pins: a pending trigger owns one fleet and tenant receipt; saturation rolls
    // the new trigger back, and replayed supersession cannot decrement twice.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut execution_config = execution_capacity_config();
    execution_config.max_tenant_scheduled_triggers = 1;
    execution_config.max_fleet_scheduled_triggers = 1;
    execution_config.validate()?;
    let first_run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "trigger-capacity-first",
            ExecutionRunStatus::Queued,
            budget_without_deadline(10),
        ),
    )
    .await?;
    let second_run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "trigger-capacity-second",
            ExecutionRunStatus::Queued,
            budget_without_deadline(10),
        ),
    )
    .await?;
    let first_uid = Uuid::now_v7();
    repository
        .create_trigger(
            scope,
            &execution_config,
            run_deadline(
                first_uid,
                tenant_id,
                first_run.run_uid,
                first_run.controller_generation,
                pg_deadline(Duration::minutes(5)),
            ),
        )
        .await?;
    let rejected_uid = Uuid::now_v7();
    assert!(matches!(
        repository
            .create_trigger(
                scope,
                &execution_config,
                run_deadline(
                    rejected_uid,
                    tenant_id,
                    second_run.run_uid,
                    second_run.controller_generation,
                    pg_deadline(Duration::minutes(5)),
                ),
            )
            .await,
        Err(moa_execution::Error::CapacitySaturated {
            dimension: "scheduled_triggers"
        })
    ));
    let rejected_rows: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM moa.execution_trigger WHERE trigger_uid = $1), \
                (SELECT count(*) FROM moa.execution_dispatch_outbox WHERE trigger_uid = $1)",
    )
    .bind(rejected_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(rejected_rows, (0, 0));

    let mut transaction = pool.begin().await?;
    assert_eq!(
        supersede_trigger_in_conn(
            &mut transaction,
            first_uid,
            ExecutionTriggerKind::RunDeadline,
            Some(first_run.controller_generation),
            None,
            None,
            None,
        )
        .await?,
        ExecutionTriggerSupersedeOutcome::Superseded
    );
    assert_eq!(
        supersede_trigger_in_conn(
            &mut transaction,
            first_uid,
            ExecutionTriggerKind::RunDeadline,
            Some(first_run.controller_generation),
            None,
            None,
            None,
        )
        .await?,
        ExecutionTriggerSupersedeOutcome::AlreadySuperseded
    );
    transaction.commit().await?;
    let counters: Vec<(String, i64)> = sqlx::query_as(
        "SELECT scope_kind, reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE resource_dimension = 'scheduled_triggers' ORDER BY scope_kind",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        counters,
        vec![("fleet".to_string(), 0), ("tenant".to_string(), 0)]
    );
    Ok(())
}

#[tokio::test]
async fn correctness_outbox_claims_are_disjoint_expiry_recoverable_and_durably_retried_db()
-> TestResult {
    // Pins: bounded SKIP LOCKED claimers never overlap; expired ownership can be stolen;
    // stale owners cannot ack; correctness dispatches remain behind durable sparse retry.
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
            "outbox-claim-retry",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    for wake_epoch in 10..14 {
        repository
            .enqueue_dispatch(
                scope,
                run_activation(
                    tenant_id,
                    run.run_uid,
                    wake_epoch,
                    pg_deadline(Duration::seconds(-1)),
                ),
            )
            .await?;
    }
    let health = repository.sample_execution_queue_health(scope, 3).await?;
    assert_eq!(health.claimable_dispatches.observed_count, 3);
    assert!(health.claimable_dispatches.saturated);
    assert!(health.claimable_dispatches.oldest_at.is_some());
    let other_health = repository
        .sample_execution_queue_health(
            ExecutionScope::Tenant {
                tenant_id: TenantId::new(),
            },
            3,
        )
        .await?;
    assert_eq!(other_health.claimable_dispatches.observed_count, 0);
    assert!(!other_health.claimable_dispatches.saturated);

    let (owner_a, owner_b) = tokio::join!(
        repository.claim_due_dispatches(scope, "owner-a", 2, StdDuration::from_secs(30)),
        repository.claim_due_dispatches(scope, "owner-b", 2, StdDuration::from_secs(30)),
    );
    let owner_a = owner_a?;
    let owner_b = owner_b?;
    assert_eq!((owner_a.len(), owner_b.len()), (2, 2));
    let a_ids = owner_a
        .iter()
        .map(|dispatch| dispatch.dispatch_uid)
        .collect::<HashSet<_>>();
    let b_ids = owner_b
        .iter()
        .map(|dispatch| dispatch.dispatch_uid)
        .collect::<HashSet<_>>();
    assert!(a_ids.is_disjoint(&b_ids));

    let abandoned = owner_a[0].dispatch_uid;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET claimed_at = now() - interval '2 seconds', \
             claim_expires_at = now() - interval '1 second' WHERE dispatch_uid = $1",
    )
    .bind(abandoned)
    .execute(&pool)
    .await?;
    // Pins: a totally lost drain leaves no pending row, but its expired dispatching claim is the
    // indexed head at claim expiry and can be reclaimed without an unrelated producer kick.
    let expired_head = repository.next_pending_dispatch_wake(scope).await?;
    assert_eq!(expired_head.dispatch_uid, Some(abandoned));
    assert!(expired_head.next_due_at <= Some(expired_head.observed_at));
    assert!(expired_head.head_updated_at.is_some());
    let recovered = repository
        .claim_due_dispatches(scope, "owner-c", 1, StdDuration::from_secs(30))
        .await?;
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].dispatch_uid, abandoned);
    assert_eq!(recovered[0].delivery_attempts, 2);
    assert_eq!(
        repository
            .mark_dispatches_delivered(scope, &[abandoned], "owner-a")
            .await?,
        Vec::<Uuid>::new()
    );

    let retry_uid = owner_b[0].dispatch_uid;
    let retry = ExecutionDispatchRetryPolicy {
        max_attempts: 2,
        base_delay: StdDuration::from_secs(1),
        maximum_delay: StdDuration::from_secs(4),
    };
    assert!(matches!(
        repository
            .record_dispatch_failure(scope, retry_uid, "owner-b", "transient", retry)
            .await?,
        ExecutionDispatchFailureOutcome::RetryScheduled { .. }
    ));
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET not_before_at = now() - interval '1 second' WHERE dispatch_uid = $1",
    )
    .bind(retry_uid)
    .execute(&pool)
    .await?;
    let reclaimed = repository
        .claim_due_dispatches(scope, "owner-d", 1, StdDuration::from_secs(30))
        .await?;
    assert_eq!(reclaimed[0].dispatch_uid, retry_uid);
    assert_eq!(reclaimed[0].delivery_attempts, 2);
    assert!(matches!(
        repository
            .record_dispatch_failure(scope, retry_uid, "owner-d", "permanent", retry)
            .await?,
        ExecutionDispatchFailureOutcome::RetryScheduled { .. }
    ));
    let state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_dispatch_outbox WHERE dispatch_uid = $1",
    )
    .bind(retry_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(state, "pending");
    let health = repository.sample_execution_queue_health(scope, 10).await?;
    assert_eq!(health.dead_letter_dispatches.observed_count, 0);
    assert!(!health.dead_letter_dispatches.saturated);
    Ok(())
}

#[tokio::test]
async fn dispatch_batch_acknowledges_only_exact_owned_unique_claims_db() -> TestResult {
    // Pins: one bounded success acknowledgement preserves request identity order, transitions
    // every exact current claim in one repository call, and leaves already-delivered or differently
    // owned rows untouched. Duplicate identities fail closed before any row can transition.
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
            "dispatch-batch-ack",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox SET state='delivered', delivered_at=NOW(), \
         updated_at=NOW() WHERE run_uid=$1 AND state='pending'",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    for wake_epoch in 10..14 {
        repository
            .enqueue_dispatch(
                scope,
                run_activation(
                    tenant_id,
                    run.run_uid,
                    wake_epoch,
                    pg_deadline(Duration::seconds(-1)),
                ),
            )
            .await?;
    }
    let claimed = repository
        .claim_due_dispatches(scope, "batch-owner", 4, StdDuration::from_secs(30))
        .await?;
    assert_eq!(claimed.len(), 4);
    let differently_owned = claimed[1].dispatch_uid;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox SET claim_owner='other-owner' \
         WHERE dispatch_uid=$1 AND state='dispatching'",
    )
    .bind(differently_owned)
    .execute(&pool)
    .await?;
    let already_delivered = claimed[2].dispatch_uid;
    assert_eq!(
        repository
            .mark_dispatches_delivered(scope, &[already_delivered], "batch-owner")
            .await?,
        vec![already_delivered]
    );

    let requested = vec![
        claimed[3].dispatch_uid,
        claimed[0].dispatch_uid,
        already_delivered,
        differently_owned,
    ];
    assert_eq!(
        repository
            .mark_dispatches_delivered(scope, &requested, "batch-owner")
            .await?,
        vec![claimed[3].dispatch_uid, claimed[0].dispatch_uid]
    );
    let differently_owned_row: (String, Option<String>) = sqlx::query_as(
        "SELECT state, claim_owner FROM moa.execution_dispatch_outbox WHERE dispatch_uid=$1",
    )
    .bind(differently_owned)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        differently_owned_row,
        ("dispatching".to_string(), Some("other-owner".to_string()))
    );

    assert!(matches!(
        repository
            .mark_dispatches_delivered(
                scope,
                &[differently_owned, differently_owned],
                "other-owner",
            )
            .await,
        Err(moa_execution::Error::InvalidRepositoryInput { message })
            if message == "execution dispatch acknowledgement identities must be unique"
    ));
    let state_after_rejection: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_dispatch_outbox WHERE dispatch_uid=$1")
            .bind(differently_owned)
            .fetch_one(&pool)
            .await?;
    assert_eq!(state_after_rejection, "dispatching");
    Ok(())
}

#[tokio::test]
async fn pending_dispatch_wake_reports_earliest_indexed_deadline_db() -> TestResult {
    // Pins: an empty due claim pass still discovers the exact earliest future dispatch through
    // the pending queue index, while tenant scope cannot observe another tenant's timer.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "pending-dispatch-wake",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox SET state='delivered', delivered_at=NOW(), \
         updated_at=NOW() WHERE run_uid=$1 AND state='pending'",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await?;
    let first_due_at = pg_deadline(Duration::seconds(10));
    let second_due_at = pg_deadline(Duration::seconds(20));
    let second = repository
        .enqueue_dispatch(
            scope,
            run_activation(tenant_id, run.run_uid, 21, second_due_at),
        )
        .await?;
    let first = repository
        .enqueue_dispatch(
            scope,
            run_activation(tenant_id, run.run_uid, 20, first_due_at),
        )
        .await?;

    assert!(
        repository
            .claim_due_dispatches(scope, "future-owner", 10, StdDuration::from_secs(30))
            .await?
            .is_empty()
    );
    let wake = repository.next_pending_dispatch_wake(scope).await?;
    assert_eq!(wake.dispatch_uid, Some(first.dispatch_uid));
    assert_eq!(wake.next_due_at, Some(first_due_at));
    assert!(wake.observed_at < first_due_at);
    assert_ne!(wake.dispatch_uid, Some(second.dispatch_uid));
    assert_eq!(
        repository
            .next_pending_dispatch_wake(ExecutionScope::Tenant {
                tenant_id: TenantId::new(),
            })
            .await?
            .dispatch_uid,
        None
    );
    Ok(())
}

#[tokio::test]
async fn task_watchdog_preparation_is_due_fenced_and_exact_owner_replay_safe_db() -> TestResult {
    // Pins: watchdog delivery does not invoke an attempt before DB time is due, resolves the
    // exact active dispatch/capacity owner when due, and terminal trigger settlement is replay-safe.
    // Total Restate loss redrives the accepted start only while DB state remains Dispatching;
    // once Running, watchdog recovery owns ambiguity and the start is never replayed.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "watchdog-exact-owner",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![watchdog_output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox SET state='delivered', delivered_at=NOW(), \
         updated_at=NOW() WHERE run_uid=$1 AND dispatch_kind='run_activation' AND state='pending'",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "watchdog-work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(
                        run.run_uid,
                        "watchdog-work",
                        "one",
                        estimate(1),
                    )],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admitted = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one task must be admitted");
    // Pins: the dispatch drain's post-delivery admission pass turns a task materialized by a
    // synchronous RunActivation into the immediate indexed TaskAttempt head. The drain can thus
    // self-chain without relying on a second producer kick.
    let admitted_head = repository.next_pending_dispatch_wake(scope).await?;
    assert_eq!(admitted_head.dispatch_uid, Some(admitted.dispatch_uid));
    assert!(admitted_head.next_due_at <= Some(admitted_head.observed_at));
    assert_eq!(
        repository
            .prepare_watchdog_trigger(scope, admitted.watchdog_trigger_uid)
            .await?,
        ExecutionWatchdogTriggerOutcome::NoOp(ExecutionTriggerNoOp::NotDue)
    );
    let fence = TaskAttemptFence {
        tenant_id: admitted.tenant_id,
        run_uid: admitted.run_uid,
        task_id: admitted.task_id,
        controller_generation: admitted.controller_generation,
        attempt_generation: admitted.attempt_generation,
        dispatch_uid: admitted.dispatch_uid,
        capacity_reservation_uid: admitted.capacity_reservation_uid,
        watchdog_trigger_uid: admitted.watchdog_trigger_uid,
        attempt_deadline_at: admitted.attempt_deadline_at,
    };
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET state='delivered', delivered_at=NOW()-INTERVAL '2 minutes', updated_at=NOW() \
         WHERE dispatch_uid=$1",
    )
    .bind(admitted.dispatch_uid)
    .execute(&pool)
    .await?;
    let accepted_before_start = repository
        .reconcile_due_trigger_dispatches(scope, 10)
        .await?;
    assert_eq!(accepted_before_start.len(), 1);
    assert_eq!(accepted_before_start[0].dispatch_uid, admitted.dispatch_uid);
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET state='delivered', delivered_at=NOW()-INTERVAL '2 minutes', updated_at=NOW() \
         WHERE dispatch_uid=$1",
    )
    .bind(admitted.dispatch_uid)
    .execute(&pool)
    .await?;
    assert!(matches!(
        repository.start_task_attempt(fence).await?,
        TaskAttemptStartOutcome::Started(_)
    ));
    assert!(
        repository
            .reconcile_due_trigger_dispatches(scope, 10)
            .await?
            .is_empty()
    );
    let running_dispatch_state: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_dispatch_outbox WHERE dispatch_uid=$1")
            .bind(admitted.dispatch_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(running_dispatch_state, "delivered");
    sqlx::query(
        "UPDATE moa.execution_trigger SET due_at=NOW()-INTERVAL '1 second' \
         WHERE trigger_uid=$1",
    )
    .bind(admitted.watchdog_trigger_uid)
    .execute(&pool)
    .await?;
    let ExecutionWatchdogTriggerOutcome::Task(request) = repository
        .prepare_watchdog_trigger(scope, admitted.watchdog_trigger_uid)
        .await?
    else {
        panic!("the exact due active task watchdog must resolve its receiver");
    };
    assert_eq!(request.dispatch_uid, admitted.dispatch_uid);
    assert_eq!(
        request.capacity_reservation_uid,
        admitted.capacity_reservation_uid
    );
    assert_eq!(request.watchdog_trigger_uid, admitted.watchdog_trigger_uid);
    assert_eq!(request.attempt_generation, admitted.attempt_generation);
    assert_eq!(
        repository
            .settle_watchdog_trigger(scope, admitted.watchdog_trigger_uid)
            .await?,
        ExecutionTriggerSupersedeOutcome::Superseded
    );
    assert_eq!(
        repository
            .prepare_watchdog_trigger(scope, admitted.watchdog_trigger_uid)
            .await?,
        ExecutionWatchdogTriggerOutcome::NoOp(ExecutionTriggerNoOp::Inactive)
    );
    Ok(())
}

#[tokio::test]
async fn one_attempt_generation_admits_exactly_one_armed_watchdog_db() -> TestResult {
    // Pins: `execution_trigger_current_run_generation_uidx` still keys the armed-trigger
    // uniqueness on the single `pending` state after the trigger claim apparatus was
    // deleted. A second watchdog for the same attempt generation must be rejected by the
    // index, and rearming after the first one settles must be admitted again — the second
    // half fails if the partial predicate is widened past `pending`.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "watchdog-generation-uniqueness",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![watchdog_output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "watchdog-work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(
                        run.run_uid,
                        "watchdog-work",
                        "one",
                        estimate(1),
                    )],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admitted = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one task must be admitted");
    let armed_state: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_trigger WHERE trigger_uid=$1")
            .bind(admitted.watchdog_trigger_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        armed_state, "pending",
        "an armed watchdog is never claimed; claiming lives on the dispatch outbox"
    );

    let duplicate = task_watchdog(
        Uuid::now_v7(),
        tenant_id,
        admitted.run_uid,
        admitted.task_id.as_uuid(),
        admitted.controller_generation,
        admitted.attempt_generation,
        pg_deadline(Duration::minutes(5)),
    );
    let mut transaction = pool.begin().await?;
    let conflict = create_trigger_with_dispatch_in_conn(&mut transaction, &config, &duplicate)
        .await
        .expect_err("one attempt generation must never arm two watchdogs");
    transaction.rollback().await?;
    let moa_execution::Error::Database { source } = conflict else {
        panic!("a duplicate armed watchdog must surface the unique-index violation");
    };
    let violation = source
        .as_database_error()
        .expect("the unique violation must carry its PostgreSQL provenance");
    assert_eq!(violation.code().as_deref(), Some("23505"));
    assert_eq!(
        violation.constraint(),
        Some("execution_trigger_current_run_generation_uidx")
    );

    assert_eq!(
        repository
            .settle_watchdog_trigger(scope, admitted.watchdog_trigger_uid)
            .await?,
        ExecutionTriggerSupersedeOutcome::Superseded
    );
    let mut transaction = pool.begin().await?;
    let rearmed =
        create_trigger_with_dispatch_in_conn(&mut transaction, &config, &duplicate).await?;
    transaction.commit().await?;
    assert_eq!(rearmed.trigger.trigger_uid, duplicate.trigger_uid);
    assert_eq!(rearmed.trigger.state, ExecutionDeliveryState::Pending);
    let armed_uids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT trigger_uid FROM moa.execution_trigger \
         WHERE run_uid=$1 AND trigger_kind='task_watchdog' AND state='pending'",
    )
    .bind(run.run_uid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(armed_uids, vec![duplicate.trigger_uid]);
    Ok(())
}

#[tokio::test]
async fn task_external_start_recovery_adopts_started_not_started_and_replay_atomically_db()
-> TestResult {
    // Pins: task recovery consumes a provisional checkpoint under exact active-resource fences.
    // NotStarted returns one fresh Ready attempt without losing the checkpoint; Started binds the
    // provider job and parks WaitingExternal; exact replay is stable and a missing checkpoint
    // rolls back intent/capacity release.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "task-external-start-recovery",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    candidate.plan.definition.nodes = vec![watchdog_output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "watchdog-work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: ["not-started", "started", "missing-checkpoint"]
                        .into_iter()
                        .map(|item| {
                            logical_task(run.run_uid, "watchdog-work", item, estimate(1))
                        })
                        .collect(),
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admissions = repository
        .admit_ready_attempts(&config, 3, Utc::now())
        .await?
        .admitted;
    assert_eq!(admissions.len(), 3);

    let mut started = Vec::new();
    for admission in admissions {
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
            panic!("each admitted recovery fixture task must start exactly once");
        };
        started.push((fence, record));
    }
    for (fence, record) in &started[..2] {
        assert!(matches!(
            repository
                .persist_running_task_external_start_checkpoint(NewTaskAttemptCheckpoint {
                    fence: *fence,
                    task_generation: record.task.generation,
                    kind: TaskAttemptCheckpointKind::CapabilityExternalStart,
                    schema_version: 1,
                    payload: json!({
                        "state": {
                            "kind": "capability_external_start",
                            "tool_id": "fixture-async-tool",
                            "usage": {}
                        }
                    }),
                    workspace_release_receipt: None,
                    created_at: Utc::now(),
                })
                .await?,
            TaskAttemptCheckpointWriteOutcome::Applied(_)
        ));
    }

    let make_recovery = |fence: TaskAttemptFence, suffix: &str| {
        let external_job_uid = Uuid::now_v7();
        let provider = "task-recovery-provider".to_string();
        let idempotency_key = format!("task-recovery-{suffix}-{external_job_uid}");
        let owner = ExecutionExternalJobOwner::Task {
            task_id: fence.task_id.as_uuid(),
            attempt_generation: fence.attempt_generation,
        };
        (
            NewExecutionExternalJobIntent {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner,
                job_generation: 1,
                provider: provider.clone(),
                idempotency_key: idempotency_key.clone(),
                expires_at: pg_deadline(Duration::minutes(1)),
            },
            ExecutionExternalJobStartRecoveryRequest {
                tenant_id,
                run_uid: run.run_uid,
                owner: ExecutionExternalJobStartRecoveryOwner::Task {
                    task_id: fence.task_id.as_uuid(),
                    attempt_generation: fence.attempt_generation,
                },
                external_job_uid,
                job_generation: 1,
                provider,
                idempotency_key,
                trigger_uid: Uuid::now_v7(),
            },
        )
    };

    let (not_started_intent, not_started_recovery) = make_recovery(started[0].0, "none");
    repository
        .reserve_external_job_intent(scope, &config, not_started_intent)
        .await?;
    let Some(not_started_authority) = repository
        .load_current_task_external_start_recovery(started[0].0)
        .await?
    else {
        panic!("the exact current unbound external start must outrank watchdog teardown");
    };
    assert_eq!(
        not_started_authority.external_job_uid,
        not_started_recovery.external_job_uid
    );
    assert_eq!(
        not_started_authority.idempotency_key,
        not_started_recovery.idempotency_key
    );
    assert!(matches!(
        repository
            .recover_external_job_start_not_started(&not_started_authority, Utc::now())
            .await?,
        ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied {
            compensation_release: None
        }
    ));
    assert_eq!(
        repository
            .load_current_task_external_start_recovery(started[0].0)
            .await?,
        None,
        "a settled external-start recovery must no longer defer the watchdog"
    );
    assert!(matches!(
        repository
            .recover_external_job_start_not_started(&not_started_authority, Utc::now())
            .await?,
        ExecutionExternalJobStartRecoveryAdoptionOutcome::Replayed {
            compensation_release: None
        }
    ));
    let not_started_state: (String, String, i64, Option<Uuid>) = sqlx::query_as(
        "SELECT status,attempt_state,attempt_generation,active_dispatch_uid \
         FROM moa.execution_task WHERE run_uid=$1 AND task_id=$2",
    )
    .bind(run.run_uid)
    .bind(started[0].0.task_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        not_started_state,
        ("ready".to_string(), "idle".to_string(), 2, None)
    );
    assert!(
        repository
            .load_task_attempt_checkpoint(scope, run.run_uid, started[0].0.task_id)
            .await?
            .is_some()
    );

    let (started_intent, started_recovery) = make_recovery(started[1].0, "started");
    let started_owner = started_intent.owner;
    repository
        .reserve_external_job_intent(scope, &config, started_intent.clone())
        .await?;
    let Some(started_authority) = repository
        .load_current_task_external_start_recovery(started[1].0)
        .await?
    else {
        panic!("started fixture must expose its exact durable recovery authority");
    };
    assert_eq!(
        started_authority.external_job_uid,
        started_recovery.external_job_uid
    );
    sqlx::query(
        "UPDATE moa.execution_capacity_reservation \
         SET expires_at=NOW() - INTERVAL '1 second' \
         WHERE external_job_uid=$1 AND resource_dimension='external_jobs'",
    )
    .bind(started_intent.external_job_uid)
    .execute(&pool)
    .await?;
    let binding = ExecutionExternalJobBinding {
        external_job_uid: started_intent.external_job_uid,
        tenant_id,
        run_uid: run.run_uid,
        owner: started_owner,
        job_generation: 1,
        idempotency_key: started_intent.idempotency_key.clone(),
        provider: started_intent.provider.clone(),
        provider_job_id: format!("provider-job-{}", Uuid::now_v7()),
        callback_auth_reference: "vault://task-recovery".to_string(),
        state: ExecutionExternalJobState::Running,
        progress_phase: Some("running".to_string()),
        cancel_supported: true,
        next_reconcile_at: Some(pg_deadline(Duration::minutes(2))),
        provider_contract_violation: None,
    };
    assert!(matches!(
        repository
            .recover_external_job_start_started(
                &config,
                &started_authority,
                binding.clone(),
                Utc::now(),
            )
            .await?,
        ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied {
            compensation_release: None
        }
    ));
    assert_eq!(
        repository
            .load_current_task_external_start_recovery(started[1].0)
            .await?,
        None
    );
    assert!(matches!(
        repository
            .recover_external_job_start_started(&config, &started_authority, binding, Utc::now(),)
            .await?,
        ExecutionExternalJobStartRecoveryAdoptionOutcome::Replayed {
            compensation_release: None
        }
    ));
    let started_state: (String, String, Option<Uuid>) = sqlx::query_as(
        "SELECT status,attempt_state,external_job_uid FROM moa.execution_task \
         WHERE run_uid=$1 AND task_id=$2",
    )
    .bind(run.run_uid)
    .bind(started[1].0.task_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        started_state,
        (
            "waiting_external".to_string(),
            "waiting".to_string(),
            Some(started_intent.external_job_uid)
        )
    );
    let recovered_job: (String, bool) = sqlx::query_as(
        "SELECT job.state, capacity.expires_at IS NULL \
         FROM moa.execution_external_job AS job \
         JOIN moa.execution_capacity_reservation AS capacity \
           ON capacity.external_job_uid=job.external_job_uid \
          AND capacity.resource_dimension='external_jobs' \
         WHERE job.external_job_uid=$1",
    )
    .bind(started_intent.external_job_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(recovered_job, ("running".to_string(), true));

    let (missing_intent, missing_recovery) = make_recovery(started[2].0, "missing");
    repository
        .reserve_external_job_intent(scope, &config, missing_intent.clone())
        .await?;
    assert!(!matches!(
        repository
            .recover_external_job_start_not_started(&missing_recovery, Utc::now())
            .await?,
        ExecutionExternalJobStartRecoveryAdoptionOutcome::Applied { .. }
            | ExecutionExternalJobStartRecoveryAdoptionOutcome::Replayed { .. }
    ));
    let preserved = repository
        .load_external_job(scope, missing_intent.external_job_uid)
        .await?
        .expect("failed adoption must roll back unbound intent release");
    assert_eq!(preserved.state, ExecutionExternalJobState::Unbound);
    Ok(())
}

#[tokio::test]
async fn external_start_rearm_replaces_completed_restate_delivery_identity_db() -> TestResult {
    // Pins: Unknown provider-start recovery keeps one durable timer row but changes the Restate
    // delivery identity, so the next due claim cannot replay the completed NotDue invocation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "external-start-rearm-identity",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let task = repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![logical_task(
                run.run_uid,
                "provider-job",
                "rearm",
                estimate(1),
            )],
        )
        .await?
        .into_iter()
        .next()
        .expect("one external-start recovery task");
    sqlx::query(
        "UPDATE moa.execution_task SET status='reserved', updated_at=NOW() WHERE task_id=$1",
    )
    .bind(task.task_id.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_task SET status='running', attempt_state='running', \
         last_progress_at=NOW(), updated_at=NOW() WHERE task_id=$1",
    )
    .bind(task.task_id.as_uuid())
    .execute(&pool)
    .await?;
    let external_job_uid = Uuid::now_v7();
    repository
        .reserve_external_job_intent(
            scope,
            &config,
            NewExecutionExternalJobIntent {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner: ExecutionExternalJobOwner::Task {
                    task_id: task.task_id.as_uuid(),
                    attempt_generation: 1,
                },
                job_generation: 1,
                provider: "batch-provider".to_string(),
                idempotency_key: "external-start-rearm-identity".to_string(),
                expires_at: pg_deadline(Duration::minutes(15)),
            },
        )
        .await?;
    let (trigger_uid, original_dispatch_uid): (Uuid, Uuid) = sqlx::query_as(
        "SELECT trigger.trigger_uid, dispatch.dispatch_uid \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (tenant_id, trigger_uid) \
         WHERE trigger.payload->>'external_job_uid'=$1 \
           AND trigger.trigger_kind='external_start_recovery'",
    )
    .bind(external_job_uid.to_string())
    .fetch_one(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_trigger SET due_at=NOW()-interval '1 second' \
         WHERE trigger_uid=$1",
    )
    .bind(trigger_uid)
    .execute(&pool)
    .await?;
    let ExecutionExternalStartRecoveryTriggerOutcome::Ready(recovery) = repository
        .prepare_external_start_recovery_trigger(scope, trigger_uid)
        .await?
    else {
        panic!("the expired unbound intent must be recoverable");
    };
    let retry_at = pg_deadline(Duration::minutes(5));
    let ExecutionExternalStartRecoveryRearmOutcome::Rearmed(rearmed) = repository
        .rearm_external_start_recovery(
            ExecutionScope::ControlPlane,
            &recovery,
            retry_at,
            "provider start remains ambiguous",
        )
        .await?
    else {
        panic!("the current recovery must rearm a fresh delivery identity");
    };
    assert_ne!(rearmed.dispatch_uid, original_dispatch_uid);
    assert_eq!(rearmed.delivery_attempts, 0);
    let persisted: (i64, Uuid, String, DateTime<Utc>) = sqlx::query_as(
        "SELECT COUNT(*) OVER (), dispatch_uid, state, not_before_at \
         FROM moa.execution_dispatch_outbox \
         WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery'",
    )
    .bind(trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        persisted,
        (1, rearmed.dispatch_uid, "pending".to_string(), retry_at)
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM moa.execution_dispatch_outbox WHERE dispatch_uid=$1)",
        )
        .bind(original_dispatch_uid)
        .fetch_one(&pool)
        .await?
    );

    let elapsed_due_at = pg_deadline(Duration::minutes(-2));
    sqlx::query(
        "UPDATE moa.execution_trigger SET due_at=$2, updated_at=NOW() WHERE trigger_uid=$1",
    )
    .bind(trigger_uid)
    .bind(elapsed_due_at)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_dispatch_outbox \
         SET state='delivered', not_before_at=$2, \
             delivered_at=NOW()-INTERVAL '2 minutes', updated_at=NOW() \
         WHERE dispatch_uid=$1",
    )
    .bind(rearmed.dispatch_uid)
    .bind(elapsed_due_at)
    .execute(&pool)
    .await?;
    let redriven = repository
        .reconcile_due_trigger_dispatches(ExecutionScope::ControlPlane, 3)
        .await?;
    assert_eq!(redriven.len(), 1);
    assert_eq!(redriven[0].dispatch_uid, rearmed.dispatch_uid);
    assert_eq!(redriven[0].state, ExecutionDeliveryState::Pending);
    let persisted_redrive: (i64, Uuid, String) = sqlx::query_as(
        "SELECT COUNT(*) OVER (), dispatch_uid, state \
         FROM moa.execution_dispatch_outbox \
         WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery'",
    )
    .bind(trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        persisted_redrive,
        (1, rearmed.dispatch_uid, "pending".to_string())
    );

    sqlx::query("DELETE FROM moa.execution_dispatch_outbox WHERE dispatch_uid=$1")
        .bind(rearmed.dispatch_uid)
        .execute(&pool)
        .await?;
    let repaired = repository
        .reconcile_due_trigger_dispatches(ExecutionScope::ControlPlane, 3)
        .await?;
    assert_eq!(repaired.len(), 1);
    assert_ne!(repaired[0].dispatch_uid, original_dispatch_uid);
    assert_ne!(repaired[0].dispatch_uid, rearmed.dispatch_uid);
    let persisted_repair: (i64, Uuid, String) = sqlx::query_as(
        "SELECT COUNT(*) OVER (), dispatch_uid, state \
         FROM moa.execution_dispatch_outbox \
         WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery'",
    )
    .bind(trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        persisted_repair,
        (1, repaired[0].dispatch_uid, "pending".to_string())
    );
    Ok(())
}

#[tokio::test]
async fn external_callbacks_are_tenant_generation_deduped_and_reconciled_db() -> TestResult {
    // Pins: async callbacks admit progress and terminal outcomes once for the exact provider
    // generation, while reconciliation and callback lookup remain tenant-scoped under RLS.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let mut execution_config = execution_capacity_config();
    execution_config.max_tenant_external_jobs = 1;
    execution_config.max_fleet_external_jobs = 1;
    execution_config.validate()?;
    let tenant_id = TenantId::new();
    let other_tenant = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "external-callback-dedupe",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![output_node("provider-job")];
    let run = create_run(&repository, scope, candidate).await?;
    let task_spec = logical_task(run.run_uid, "provider-job", "one", estimate(1));
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &execution_config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "provider-job".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![task_spec],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admission = repository
        .admit_ready_attempts(&execution_config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one external callback task must be admitted");
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
    let TaskAttemptStartOutcome::Started(started) = repository.start_task_attempt(fence).await?
    else {
        panic!("external callback fixture must start its admitted attempt");
    };
    let external_job_uid = Uuid::now_v7();
    let first_intent = NewExecutionExternalJobIntent {
        external_job_uid,
        tenant_id,
        run_uid: run.run_uid,
        owner: ExecutionExternalJobOwner::Task {
            task_id: fence.task_id.as_uuid(),
            attempt_generation: fence.attempt_generation,
        },
        job_generation: 1,
        provider: "batch-provider".to_string(),
        idempotency_key: "external-job-start-1".to_string(),
        expires_at: pg_deadline(Duration::minutes(15)),
    };
    repository
        .reserve_external_job_intent(scope, &execution_config, first_intent.clone())
        .await?;
    let (start_recovery_trigger_uid, start_recovery_dispatch_uid): (Uuid, Uuid) = sqlx::query_as(
        "SELECT trigger.trigger_uid, dispatch.dispatch_uid \
             FROM moa.execution_trigger AS trigger \
             JOIN moa.execution_dispatch_outbox AS dispatch USING (tenant_id, trigger_uid) \
             WHERE trigger.payload->>'external_job_uid'=$1 \
               AND trigger.trigger_kind='external_start_recovery'",
    )
    .bind(external_job_uid.to_string())
    .fetch_one(test_db.store().pool())
    .await?;
    sqlx::query(
        "UPDATE moa.execution_trigger SET due_at=NOW()-interval '1 second' \
         WHERE trigger_uid=$1",
    )
    .bind(start_recovery_trigger_uid)
    .execute(test_db.store().pool())
    .await?;
    let ExecutionExternalStartRecoveryTriggerOutcome::Ready(start_recovery) = repository
        .prepare_external_start_recovery_trigger(scope, start_recovery_trigger_uid)
        .await?
    else {
        panic!("the exact expired unbound intent must be recoverable");
    };
    let retry_at = pg_deadline(Duration::minutes(5));
    let ExecutionExternalStartRecoveryRearmOutcome::Rearmed(rearmed) = repository
        .rearm_external_start_recovery(
            ExecutionScope::ControlPlane,
            &start_recovery,
            retry_at,
            "provider start remains ambiguous",
        )
        .await?
    else {
        panic!("an ambiguous current provider start must rearm a fresh delivery");
    };
    assert_ne!(rearmed.dispatch_uid, start_recovery_dispatch_uid);
    assert_eq!(rearmed.delivery_attempts, 0);
    let rearmed_state: (String, String, DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state, trigger.due_at, dispatch.not_before_at \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (tenant_id, trigger_uid) \
         WHERE trigger.trigger_uid=$1",
    )
    .bind(start_recovery_trigger_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(
        (rearmed_state.0.as_str(), rearmed_state.1.as_str()),
        ("pending", "pending")
    );
    assert_eq!(rearmed_state.2, retry_at);
    assert_eq!(rearmed_state.3, retry_at);
    // Pins: an ambiguous provider result retains one outbox-backed timing authority but replaces
    // its Restate idempotency identity, so the next due delivery cannot replay the completed
    // NotDue invocation from the preceding provider lookup.
    let delivery_rows: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_dispatch_outbox \
         WHERE trigger_uid=$1 AND dispatch_kind='trigger_delivery'",
    )
    .bind(start_recovery_trigger_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(delivery_rows, 1);
    let old_delivery_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM moa.execution_dispatch_outbox WHERE dispatch_uid=$1)",
    )
    .bind(start_recovery_dispatch_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert!(!old_delivery_exists);
    repository
        .bind_external_job(
            scope,
            &execution_config,
            ExecutionExternalJobBinding {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner: first_intent.owner,
                job_generation: 1,
                idempotency_key: first_intent.idempotency_key.clone(),
                provider: first_intent.provider.clone(),
                provider_job_id: "provider-job-1".to_string(),
                callback_auth_reference: "vault://callback/provider-1".to_string(),
                state: ExecutionExternalJobState::Starting,
                progress_phase: None,
                cancel_supported: true,
                next_reconcile_at: Some(pg_deadline(Duration::seconds(-1))),
                provider_contract_violation: None,
            },
        )
        .await?;
    assert!(matches!(
        repository
            .begin_task_attempt_release(fence, started.task.generation, "external_job", Utc::now(),)
            .await?,
        TaskAttemptReleaseClaimOutcome::Applied(_)
    ));
    let TaskAttemptExternalOutcome::Applied { task, .. } = repository
        .yield_task_attempt_to_external_job(fence, external_job_uid, None, None, Utc::now())
        .await?
    else {
        panic!("external callback fixture must park on its bound provider job");
    };
    let mut second_candidate = new_run(
        tenant_id,
        None,
        "external-callback-capacity-second",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    second_candidate.plan.definition.nodes = vec![output_node("provider-job-second")];
    let second_run = create_run(&repository, scope, second_candidate).await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &execution_config,
                ReadyMaterializationRequest {
                    run_uid: second_run.run_uid,
                    plan_revision: 1,
                    node_id: "provider-job-second".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(
                        second_run.run_uid,
                        "provider-job-second",
                        "one",
                        estimate(1),
                    )],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let second_admission = repository
        .admit_ready_attempts(&execution_config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one second external callback task must be admitted");
    let second_fence = TaskAttemptFence {
        tenant_id: second_admission.tenant_id,
        run_uid: second_admission.run_uid,
        task_id: second_admission.task_id,
        controller_generation: second_admission.controller_generation,
        attempt_generation: second_admission.attempt_generation,
        dispatch_uid: second_admission.dispatch_uid,
        capacity_reservation_uid: second_admission.capacity_reservation_uid,
        watchdog_trigger_uid: second_admission.watchdog_trigger_uid,
        attempt_deadline_at: second_admission.attempt_deadline_at,
    };
    let TaskAttemptStartOutcome::Started(second_started) =
        repository.start_task_attempt(second_fence).await?
    else {
        panic!("second external callback fixture must start its admitted attempt");
    };
    let second_external_job_uid = Uuid::now_v7();
    let second_intent = NewExecutionExternalJobIntent {
        external_job_uid: second_external_job_uid,
        tenant_id,
        run_uid: second_run.run_uid,
        owner: ExecutionExternalJobOwner::Task {
            task_id: second_fence.task_id.as_uuid(),
            attempt_generation: second_fence.attempt_generation,
        },
        job_generation: 1,
        provider: "batch-provider".to_string(),
        idempotency_key: "external-job-start-2".to_string(),
        expires_at: pg_deadline(Duration::minutes(15)),
    };
    assert!(matches!(
        repository
            .reserve_external_job_intent(scope, &execution_config, second_intent.clone())
            .await,
        Err(moa_execution::Error::CapacitySaturated {
            dimension: "external_jobs"
        })
    ));
    let rejected_job_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_external_job WHERE external_job_uid = $1",
    )
    .bind(second_external_job_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(rejected_job_count, 0);
    assert_eq!(repository.list_due_external_jobs(scope, 10).await?.len(), 1);
    assert!(
        repository
            .list_due_external_jobs(
                ExecutionScope::Tenant {
                    tenant_id: other_tenant,
                },
                10,
            )
            .await?
            .is_empty()
    );
    assert!(
        repository
            .load_external_job(scope, external_job_uid)
            .await?
            .is_some()
    );
    assert!(
        repository
            .load_external_job(
                ExecutionScope::Tenant {
                    tenant_id: other_tenant,
                },
                external_job_uid,
            )
            .await?
            .is_none()
    );
    let ExecutionExternalJobCancellationOutcome::Applied(cancel_requested) = repository
        .settle_external_job_cancellation(
            scope,
            &execution_config,
            ExecutionExternalJobCancellation {
                external_job_uid,
                job_generation: 1,
                provider: "batch-provider".to_string(),
                provider_job_id: "provider-job-1".to_string(),
                state: ExecutionExternalJobState::CancelRequested,
                next_reconcile_at: Some(pg_deadline(Duration::minutes(5))),
                error: None,
            },
        )
        .await?
    else {
        panic!("exact cancellation request must apply");
    };
    assert_eq!(
        cancel_requested.state,
        ExecutionExternalJobState::CancelRequested
    );
    assert_eq!(
        repository
            .settle_external_job_cancellation(
                scope,
                &execution_config,
                ExecutionExternalJobCancellation {
                    external_job_uid,
                    job_generation: 2,
                    provider: "batch-provider".to_string(),
                    provider_job_id: "provider-job-1".to_string(),
                    state: ExecutionExternalJobState::Cancelled,
                    next_reconcile_at: None,
                    error: None,
                },
            )
            .await?,
        ExecutionExternalJobCancellationOutcome::StaleGeneration
    );

    let progress = callback(
        external_job_uid,
        1,
        "progress-1",
        ExecutionExternalJobCallbackUpdate::Progress {
            state: ExecutionExternalJobState::Running,
            progress_phase: Some("map".to_string()),
            next_reconcile_at: Some(pg_deadline(Duration::minutes(10))),
        },
    );
    let progress_write = repository
        .apply_external_job_callback_and_activate(
            ExecutionScope::ControlPlane,
            &execution_config,
            progress.clone(),
        )
        .await?;
    let ExecutionExternalJobCallbackOutcome::Applied(progressed) = progress_write.outcome else {
        panic!("exact progress callback must apply");
    };
    assert_eq!(
        progressed.state,
        ExecutionExternalJobState::CancelRequested,
        "provider progress must not clear an accepted cancellation intent"
    );
    assert_eq!(progressed.progress_phase.as_deref(), Some("map"));
    assert_eq!(progress_write.activation, None);
    let duplicate = repository
        .apply_external_job_callback_and_activate(
            ExecutionScope::ControlPlane,
            &execution_config,
            progress,
        )
        .await?;
    assert_eq!(
        duplicate.outcome,
        ExecutionExternalJobCallbackOutcome::Duplicate
    );
    assert_eq!(duplicate.activation, None);
    assert_eq!(
        repository
            .load_external_job(scope, external_job_uid)
            .await?,
        Some((*progressed).clone())
    );
    assert_eq!(
        repository
            .apply_external_job_callback_and_activate(
                ExecutionScope::ControlPlane,
                &execution_config,
                callback(
                    external_job_uid,
                    2,
                    "stale-generation",
                    ExecutionExternalJobCallbackUpdate::Progress {
                        state: ExecutionExternalJobState::Running,
                        progress_phase: Some("reduce".to_string()),
                        next_reconcile_at: None,
                    },
                ),
            )
            .await?
            .outcome,
        ExecutionExternalJobCallbackOutcome::StaleGeneration
    );
    assert!(matches!(
        repository
            .apply_external_job_callback_and_activate(
                ExecutionScope::ControlPlane,
                &execution_config,
                callback(
                    external_job_uid,
                    1,
                    "progress-2",
                    ExecutionExternalJobCallbackUpdate::Progress {
                        state: ExecutionExternalJobState::WaitingReconcile,
                        progress_phase: Some("reduce".to_string()),
                        next_reconcile_at: Some(pg_deadline(Duration::minutes(10))),
                    },
                ),
            )
            .await?,
        moa_execution::repository::external_job::ExecutionExternalJobCallbackWrite {
            outcome: ExecutionExternalJobCallbackOutcome::Applied(_),
            activation: None,
        }
    ));
    assert_eq!(
        repository
            .apply_external_job_callback_and_activate(
                ExecutionScope::ControlPlane,
                &execution_config,
                callback(
                    external_job_uid,
                    1,
                    "progress-1",
                    ExecutionExternalJobCallbackUpdate::Progress {
                        state: ExecutionExternalJobState::Running,
                        progress_phase: Some("map".to_string()),
                        next_reconcile_at: None,
                    },
                ),
            )
            .await?
            .outcome,
        ExecutionExternalJobCallbackOutcome::Duplicate
    );
    let progress_activation_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_dispatch_outbox \
         WHERE run_uid=$1 AND dispatch_kind='run_activation' \
           AND payload->>'source'='external_job_callback'",
    )
    .bind(run.run_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(progress_activation_count, 0);

    repository
        .pause_run(scope, &execution_config, run.run_uid, 1)
        .await?;
    let paused_wake_epoch: i64 = sqlx::query_scalar(
        "SELECT wake_epoch FROM moa.execution_run WHERE run_uid=$1 AND status='paused'",
    )
    .bind(run.run_uid)
    .fetch_one(test_db.store().pool())
    .await?;

    let terminal = callback(
        external_job_uid,
        1,
        "terminal-1",
        ExecutionExternalJobCallbackUpdate::Terminal {
            state: ExecutionExternalJobState::Completed,
            progress_phase: Some("complete".to_string()),
            output: Some(json!({"artifact": "result-1"})),
            error: None,
        },
    );
    let terminal_write = repository
        .apply_external_job_callback_and_activate(
            ExecutionScope::ControlPlane,
            &execution_config,
            terminal.clone(),
        )
        .await?;
    let ExecutionExternalJobCallbackOutcome::Applied(completed) = terminal_write.outcome else {
        panic!("exact terminal callback must apply");
    };
    assert_eq!(terminal_write.activation, None);
    assert_eq!(completed.state, ExecutionExternalJobState::Completed);
    assert!(completed.completed_at.is_some());
    let paused_after_callback: (String, i64, String) = sqlx::query_as(
        "SELECT run.status, run.wake_epoch, task.status \
         FROM moa.execution_run AS run JOIN moa.execution_task AS task USING (run_uid) \
         WHERE run.run_uid=$1 AND task.task_id=$2",
    )
    .bind(run.run_uid)
    .bind(task.task_id.as_uuid())
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(paused_after_callback.0, "paused");
    assert!(
        paused_after_callback.1 > paused_wake_epoch,
        "terminal outcome persistence must advance the durable wake fence"
    );
    assert_eq!(paused_after_callback.2, "completed");
    assert_eq!(
        repository
            .apply_external_job_callback_and_activate(
                ExecutionScope::ControlPlane,
                &execution_config,
                terminal,
            )
            .await?,
        moa_execution::repository::external_job::ExecutionExternalJobCallbackWrite {
            outcome: ExecutionExternalJobCallbackOutcome::Duplicate,
            activation: None,
        }
    );
    let capacity_after_terminal: Vec<(String, i64)> = sqlx::query_as(
        "SELECT scope_kind, reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE resource_dimension = 'external_jobs' ORDER BY scope_kind",
    )
    .fetch_all(test_db.store().pool())
    .await?;
    assert_eq!(
        capacity_after_terminal,
        vec![("fleet".to_string(), 0), ("tenant".to_string(), 0)]
    );
    repository
        .reserve_external_job_intent(scope, &execution_config, second_intent.clone())
        .await?;
    repository
        .bind_external_job(
            scope,
            &execution_config,
            ExecutionExternalJobBinding {
                external_job_uid: second_external_job_uid,
                tenant_id,
                run_uid: second_run.run_uid,
                owner: second_intent.owner,
                job_generation: 1,
                idempotency_key: second_intent.idempotency_key,
                provider: second_intent.provider,
                provider_job_id: "provider-job-2".to_string(),
                callback_auth_reference: "vault://callback/provider-2".to_string(),
                state: ExecutionExternalJobState::Starting,
                progress_phase: None,
                cancel_supported: true,
                next_reconcile_at: Some(pg_deadline(Duration::seconds(-1))),
                provider_contract_violation: None,
            },
        )
        .await?;
    assert!(matches!(
        repository
            .begin_task_attempt_release(
                second_fence,
                second_started.task.generation,
                "external_job",
                Utc::now(),
            )
            .await?,
        TaskAttemptReleaseClaimOutcome::Applied(_)
    ));
    assert!(matches!(
        repository
            .yield_task_attempt_to_external_job(
                second_fence,
                second_external_job_uid,
                None,
                None,
                Utc::now(),
            )
            .await?,
        TaskAttemptExternalOutcome::Applied { .. }
    ));
    sqlx::query("UPDATE moa.execution_run SET wake_epoch = $2 WHERE run_uid = $1")
        .bind(second_run.run_uid)
        .bind(i64::MAX)
        .execute(test_db.store().pool())
        .await?;
    let mut rollback_event = callback(
        second_external_job_uid,
        1,
        "rollback-event",
        ExecutionExternalJobCallbackUpdate::Terminal {
            state: ExecutionExternalJobState::Completed,
            progress_phase: Some("must-rollback".to_string()),
            output: Some(json!({"must": "rollback"})),
            error: None,
        },
    );
    rollback_event.provider_job_id = "provider-job-2".to_string();
    let rollback_result = repository
        .apply_external_job_callback_and_activate(
            ExecutionScope::ControlPlane,
            &execution_config,
            rollback_event,
        )
        .await;
    assert!(
        rollback_result.is_err(),
        "wake-epoch overflow must fail after callback mutation, got {rollback_result:?}"
    );
    let rolled_back: (String, Option<String>, i64) = sqlx::query_as(
        "SELECT job.state, job.last_provider_event_id, \
         (SELECT count(*) FROM moa.execution_external_job_callback_receipt receipt \
          WHERE receipt.external_job_uid = job.external_job_uid \
            AND receipt.provider_event_id = 'rollback-event') \
         FROM moa.execution_external_job job WHERE job.external_job_uid = $1",
    )
    .bind(second_external_job_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(rolled_back, ("starting".to_string(), None, 0));
    assert_eq!(
        repository
            .apply_external_job_callback_and_activate(
                ExecutionScope::ControlPlane,
                &execution_config,
                callback(
                    external_job_uid,
                    1,
                    "late-progress",
                    ExecutionExternalJobCallbackUpdate::Progress {
                        state: ExecutionExternalJobState::Running,
                        progress_phase: Some("late".to_string()),
                        next_reconcile_at: None,
                    },
                ),
            )
            .await?
            .outcome,
        ExecutionExternalJobCallbackOutcome::AlreadyTerminal
    );
    Ok(())
}

#[tokio::test]
async fn terminal_callback_before_task_release_commits_then_settles_once_db() -> TestResult {
    // Pins: a terminal callback for an exactly bound active task commits its provider receipt
    // without mutating the task; the subsequent attempt release consumes that terminal job once.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "terminal-callback-before-task-release",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    candidate.plan.definition.nodes = vec![watchdog_output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "watchdog-work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(
                        run.run_uid,
                        "watchdog-work",
                        "callback-race",
                        estimate(1),
                    )],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admission = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one callback-race task must be admitted");
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
    let TaskAttemptStartOutcome::Started(started) = repository.start_task_attempt(fence).await?
    else {
        panic!("callback-race fixture must start its admitted attempt");
    };
    let external_job_uid = Uuid::now_v7();
    let intent = NewExecutionExternalJobIntent {
        external_job_uid,
        tenant_id,
        run_uid: run.run_uid,
        owner: ExecutionExternalJobOwner::Task {
            task_id: fence.task_id.as_uuid(),
            attempt_generation: fence.attempt_generation,
        },
        job_generation: 1,
        provider: "batch-provider".to_string(),
        idempotency_key: format!("terminal-before-release-{external_job_uid}"),
        expires_at: pg_deadline(Duration::minutes(15)),
    };
    repository
        .reserve_external_job_intent(scope, &config, intent.clone())
        .await?;
    repository
        .bind_external_job(
            scope,
            &config,
            ExecutionExternalJobBinding {
                external_job_uid,
                tenant_id,
                run_uid: run.run_uid,
                owner: intent.owner,
                job_generation: 1,
                idempotency_key: intent.idempotency_key,
                provider: intent.provider,
                provider_job_id: "provider-job-1".to_string(),
                callback_auth_reference: "vault://callback/race".to_string(),
                state: ExecutionExternalJobState::Running,
                progress_phase: Some("running".to_string()),
                cancel_supported: true,
                next_reconcile_at: None,
                provider_contract_violation: None,
            },
        )
        .await?;

    let callback_write = repository
        .apply_external_job_callback_and_activate(
            ExecutionScope::ControlPlane,
            &config,
            callback(
                external_job_uid,
                1,
                "terminal-before-release",
                ExecutionExternalJobCallbackUpdate::Terminal {
                    state: ExecutionExternalJobState::Completed,
                    progress_phase: Some("completed".to_string()),
                    output: Some(json!({"artifact": "callback-race"})),
                    error: None,
                },
            ),
        )
        .await?;
    assert!(matches!(
        callback_write.outcome,
        ExecutionExternalJobCallbackOutcome::Applied(_)
    ));
    assert_eq!(callback_write.activation, None);
    let before_release: (String, String, Option<Uuid>, Uuid, i64) = sqlx::query_as(
        "SELECT task.status, task.attempt_state, task.external_job_uid, \
                task.active_dispatch_uid, \
                (SELECT COUNT(*) FROM moa.execution_external_job_callback_receipt receipt \
                 WHERE receipt.external_job_uid=$3 \
                   AND receipt.provider_event_id='terminal-before-release') \
         FROM moa.execution_task AS task WHERE task.run_uid=$1 AND task.task_id=$2",
    )
    .bind(run.run_uid)
    .bind(fence.task_id.as_uuid())
    .bind(external_job_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        before_release,
        (
            "running".to_string(),
            "running".to_string(),
            None,
            fence.dispatch_uid,
            1,
        ),
        "the callback must commit without stealing the active attempt's release boundary"
    );

    assert!(matches!(
        repository
            .begin_task_attempt_release(fence, started.task.generation, "external_job", Utc::now(),)
            .await?,
        TaskAttemptReleaseClaimOutcome::Applied(_)
    ));
    let TaskAttemptExternalOutcome::Applied { task, .. } = repository
        .yield_task_attempt_to_external_job(fence, external_job_uid, None, None, Utc::now())
        .await?
    else {
        panic!("the exact task release must consume the committed terminal callback");
    };
    assert_eq!(task.status, ExecutionTaskStatus::Completed);
    assert_eq!(task.output, Some(json!({"artifact": "callback-race"})));
    assert!(matches!(
        repository
            .yield_task_attempt_to_external_job(fence, external_job_uid, None, None, Utc::now(),)
            .await?,
        TaskAttemptExternalOutcome::Replayed { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn retry_settlement_preserves_cancelling_until_ready_transition_db() -> TestResult {
    // Pins: recording a retryable watchdog outcome must not transiently reopen the attempt from
    // Cancelling to Running before the exact settlement advances it to the next Ready generation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "watchdog-retry-settlement",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    candidate.plan.definition.nodes = vec![watchdog_output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "watchdog-work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(
                        run.run_uid,
                        "watchdog-work",
                        "retry",
                        estimate(1),
                    )],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admission = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one retry task must be admitted");
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
    let TaskAttemptStartOutcome::Started(started) = repository.start_task_attempt(fence).await?
    else {
        panic!("retry fixture must start its admitted attempt");
    };
    let settled_at = Utc::now();
    let TaskAttemptReleaseClaimOutcome::Applied(releasing) = repository
        .begin_task_attempt_release(fence, started.task.generation, "watchdog", settled_at)
        .await?
    else {
        panic!("the exact watchdog attempt must enter its release boundary");
    };
    assert_eq!(releasing.task.status, ExecutionTaskStatus::Running);
    assert_eq!(
        releasing.task.attempt_state,
        ExecutionAttemptState::Cancelling
    );
    assert_eq!(releasing.task.attempt_generation, fence.attempt_generation);
    let retry_at = settled_at + Duration::milliseconds(50);
    let TaskAttemptSettlementOutcome::Applied { task, .. } = repository
        .settle_released_task_attempt(
            &config,
            fence,
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: started.task.actual,
                result: ExecutionTaskResult::Failed {
                    class: ExecutionFailureClass::Retryable,
                    message: "watchdog expired".to_string(),
                },
            },
            Some(retry_at),
            settled_at,
            None,
        )
        .await?
    else {
        panic!("the claimed watchdog retry must settle into the next ready generation");
    };
    assert_eq!(task.status, ExecutionTaskStatus::Ready);
    assert_eq!(task.attempt_state, ExecutionAttemptState::Idle);
    assert_eq!(task.attempt, 2);
    assert_eq!(task.generation, 2);
    assert_eq!(task.attempt_generation, 2);
    assert_eq!(task.ready_at, Some(retry_at));
    let boundary: (String, String, i64) = sqlx::query_as(
        "SELECT trigger.state, capacity.state, \
                (SELECT COUNT(*) FROM moa.execution_dispatch_outbox \
                 WHERE run_uid=$1 AND dispatch_kind='run_activation' \
                   AND payload->>'source'='task_attempt_settlement') \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_capacity_reservation AS capacity USING (trigger_uid) \
         WHERE trigger.trigger_uid=$2",
    )
    .bind(run.run_uid)
    .bind(fence.watchdog_trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        boundary,
        ("superseded".to_string(), "released".to_string(), 1)
    );
    Ok(())
}

#[tokio::test]
async fn durable_task_release_receipt_splits_capacity_from_outcome_settlement_db() -> TestResult {
    // Pins: entering Cancelling alone and a missing/forged durable hand receipt leave the exact
    // ActiveTasks reservation held. The exact persisted receipt releases it once and supersedes
    // its watchdog without reconciling the four committed bucket limits; replay is harmless. A
    // final outcome can then settle while the fleet bucket is locked, proving the crash-gap
    // recovery path does not reacquire either capacity bucket.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "proof-aware-task-release",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    candidate.plan.definition.nodes = vec![watchdog_output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "watchdog-work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(
                        run.run_uid,
                        "watchdog-work",
                        "proof-aware",
                        estimate(1),
                    )],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admission = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one proof-aware task must be admitted");
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
    let TaskAttemptStartOutcome::Started(started) = repository.start_task_attempt(fence).await?
    else {
        panic!("proof-aware task must start");
    };
    let now = Utc::now();
    let TaskAttemptReleaseClaimOutcome::Applied(releasing) = repository
        .begin_task_attempt_release(fence, started.task.generation, "task_outcome", now)
        .await?
    else {
        panic!("proof-aware task must enter Cancelling");
    };
    let reserved_quantity = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT reserved_quantity FROM moa.execution_capacity_bucket \
             WHERE scope_kind='fleet' AND resource_dimension='active_tasks'",
        )
        .fetch_one(&pool)
        .await
    };
    assert_eq!(
        reserved_quantity().await?,
        1,
        "begin-release must retain capacity"
    );

    let receipt = task_release_receipt(&releasing.task, fence, now);
    assert_eq!(
        repository
            .release_released_task_attempt_capacity(
                fence,
                releasing.task.generation,
                receipt.clone(),
            )
            .await?,
        ReleasedTaskAttemptCapacityOutcome::Stale
    );
    assert_eq!(
        reserved_quantity().await?,
        1,
        "an unpersisted receipt is not proof"
    );
    persist_task_release_receipt(&pool, &receipt).await?;
    let mut forged = receipt.clone();
    forged.receipt_id = Uuid::now_v7();
    assert_eq!(
        repository
            .release_released_task_attempt_capacity(fence, releasing.task.generation, forged)
            .await?,
        ReleasedTaskAttemptCapacityOutcome::Stale
    );
    assert_eq!(
        reserved_quantity().await?,
        1,
        "a forged receipt must roll back release"
    );
    let release_limits = [
        ("fleet", None, "active_tasks", 17_i64),
        ("tenant", Some(tenant_id.0), "active_tasks", 13_i64),
        ("fleet", None, "scheduled_triggers", 19_i64),
        ("tenant", Some(tenant_id.0), "scheduled_triggers", 11_i64),
    ];
    for (scope_kind, owner, dimension, limit) in release_limits {
        sqlx::query(
            "UPDATE moa.execution_capacity_bucket SET limit_value=$4 \
             WHERE scope_kind=$1 AND tenant_id IS NOT DISTINCT FROM $2 \
               AND resource_dimension=$3",
        )
        .bind(scope_kind)
        .bind(owner)
        .bind(dimension)
        .bind(limit)
        .execute(&pool)
        .await?;
    }
    let buckets_before_release = sqlx::query_as::<_, (String, Option<Uuid>, String, i64, i64)>(
        "SELECT scope_kind,tenant_id,resource_dimension,limit_value,version \
         FROM moa.execution_capacity_bucket \
         WHERE resource_dimension IN ('active_tasks','scheduled_triggers') \
           AND (scope_kind='fleet' OR tenant_id=$1) \
         ORDER BY resource_dimension,scope_kind",
    )
    .bind(tenant_id.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(buckets_before_release.len(), 4);
    assert_eq!(
        repository
            .release_released_task_attempt_capacity(
                fence,
                releasing.task.generation,
                receipt.clone(),
            )
            .await?,
        ReleasedTaskAttemptCapacityOutcome::Applied
    );
    let buckets_after_release = sqlx::query_as::<_, (String, Option<Uuid>, String, i64, i64)>(
        "SELECT scope_kind,tenant_id,resource_dimension,limit_value,version \
         FROM moa.execution_capacity_bucket \
         WHERE resource_dimension IN ('active_tasks','scheduled_triggers') \
           AND (scope_kind='fleet' OR tenant_id=$1) \
         ORDER BY resource_dimension,scope_kind",
    )
    .bind(tenant_id.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(buckets_after_release.len(), 4);
    for (before, after) in buckets_before_release.iter().zip(&buckets_after_release) {
        assert_eq!(&after.0, &before.0);
        assert_eq!(after.1, before.1);
        assert_eq!(&after.2, &before.2);
        assert_eq!(
            after.3, before.3,
            "release must not rewrite capacity limits"
        );
        assert_eq!(
            after.4,
            before.4 + 1,
            "each exact receipt release must advance its bucket once"
        );
    }
    assert_eq!(reserved_quantity().await?, 0);
    assert_eq!(
        repository
            .release_released_task_attempt_capacity(
                fence,
                releasing.task.generation,
                receipt.clone(),
            )
            .await?,
        ReleasedTaskAttemptCapacityOutcome::Replayed
    );
    assert_eq!(
        reserved_quantity().await?,
        0,
        "release replay must not decrement twice"
    );

    let mut capacity_lock = pool.begin().await?;
    sqlx::query(
        "SELECT capacity_bucket_uid FROM moa.execution_capacity_bucket \
         WHERE scope_kind='fleet' AND resource_dimension='active_tasks' FOR UPDATE",
    )
    .fetch_one(&mut *capacity_lock)
    .await?;
    let settlement = tokio::time::timeout(
        StdDuration::from_secs(2),
        repository.settle_released_task_attempt(
            &config,
            fence,
            completed(1),
            None,
            now + Duration::milliseconds(1),
            Some(receipt.clone()),
        ),
    )
    .await
    .expect("pre-released settlement must not wait for the fleet capacity lock")?;
    capacity_lock.rollback().await?;
    assert!(matches!(
        settlement,
        TaskAttemptSettlementOutcome::Applied { .. }
    ));
    assert!(matches!(
        repository
            .settle_released_task_attempt(
                &config,
                fence,
                completed(1),
                None,
                now + Duration::milliseconds(1),
                Some(receipt),
            )
            .await?,
        TaskAttemptSettlementOutcome::Replayed { .. }
    ));
    Ok(())
}

fn task_release_receipt(
    task: &ExecutionTaskRecord,
    fence: TaskAttemptFence,
    released_at: DateTime<Utc>,
) -> ExecutionHandReleaseReceipt {
    ExecutionHandReleaseReceipt {
        receipt_id: Uuid::now_v7(),
        tenant_id: fence.tenant_id,
        run_id: ExecutionRunScopeId(fence.run_uid),
        owner: ExecutionHandReleaseOwner::Task {
            task_id: ExecutionTaskScopeId(fence.task_id.as_uuid()),
            logical_generation: task.generation,
        },
        attempt_generation: fence.attempt_generation,
        workspace_id: None,
        writer_epoch: None,
        instance_generation: None,
        hand_provisioning_operation_id: None,
        hand_lease_generation: None,
        checkpoint_id: None,
        checkpoint_generation: None,
        checkpoint_manifest_digest: None,
        checkpoint_logical_bytes: None,
        requested_at: released_at,
        released_at,
    }
}

async fn persist_task_release_receipt(
    pool: &sqlx::PgPool,
    receipt: &ExecutionHandReleaseReceipt,
) -> Result<(), sqlx::Error> {
    let ExecutionHandReleaseOwner::Task {
        task_id,
        logical_generation,
    } = receipt.owner
    else {
        unreachable!("task receipt fixture must have a task owner");
    };
    sqlx::query(
        "INSERT INTO moa.sandbox_execution_hand_release_receipts \
         (receipt_id,tenant_id,run_uid,owner_kind,task_id,compensation_id, \
          logical_generation,attempt_generation,workspace_id,writer_epoch,instance_generation, \
          hand_provisioning_operation_id,hand_lease_generation,checkpoint_id, \
          checkpoint_generation,checkpoint_manifest_digest,checkpoint_logical_bytes, \
          receipt_state,destroy_outcome,claim_token,claim_expires_at,requested_at,deadline_at, \
          released_at) VALUES ($1,$2,$3,'task',$4,NULL,$5,$6,NULL,NULL,NULL,NULL,NULL,NULL,NULL, \
          NULL,NULL,'released','verified_absent',NULL,NULL,$7,$7,$7)",
    )
    .bind(receipt.receipt_id)
    .bind(receipt.tenant_id.0)
    .bind(receipt.run_id.0)
    .bind(task_id.0)
    .bind(i64::try_from(logical_generation).expect("fixture generation fits i64"))
    .bind(i64::try_from(receipt.attempt_generation).expect("fixture attempt fits i64"))
    .bind(receipt.released_at)
    .execute(pool)
    .await?;
    Ok(())
}

fn execution_capacity_config() -> ExecutionConfig {
    ExecutionConfig {
        planner_repair_attempts: 1,
        repeated_failure_limit: 3,
        max_in_flight_tasks: 64,
        maximum_horizon_seconds: 30 * 24 * 60 * 60,
        maximum_activation_steps: 128,
        dispatch_batch_size: 32,
        active_attempt_timeout_seconds: 10 * 60,
        attempt_heartbeat_staleness_seconds: 2 * 60,
        max_tenant_active_runs: 100,
        max_fleet_active_runs: 1_000,
        max_tenant_active_tasks: 256,
        max_fleet_active_tasks: 4_096,
        max_tenant_parked_runs: 10_000,
        max_fleet_parked_runs: 100_000,
        max_tenant_scheduled_triggers: 50_000,
        max_fleet_scheduled_triggers: 500_000,
        max_tenant_external_jobs: 1_000,
        max_fleet_external_jobs: 10_000,
        trigger_reconciliation_cadence_seconds: 60,
        terminal_detail_retention_days: 30,
        max_tasks: 10_000,
        max_tokens: 10_000_000,
        max_tool_calls: 100_000,
        max_retrieved_bytes: 10_000_000_000,
        max_cost_microusd: 100_000_000,
        unattended_max_cost_microusd: 5_000_000,
        agent_turn_cost_microusd: 100_000,
        agent_turn_tokens: 8_000,
        agent_turn_tool_calls: 8,
        agent_turn_retrieved_bytes: 10_000_000,
        verifier_turn_cost_microusd: 200_000,
        verifier_turn_tokens: 16_000,
        verifier_turn_tool_calls: 4,
        verifier_turn_retrieved_bytes: 1_000_000,
    }
}

#[tokio::test]
async fn maintenance_checkpoint_is_control_plane_bounded_and_generation_fenced_db() -> TestResult {
    // Pins: reconciler health means a completed repair+dispatch pass, not merely a Cron fire;
    // stale invocations cannot overwrite a newer generation and errors fit the schema byte bound.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let kind = ExecutionMaintenanceJobKind::DispatchReconciliation;
    assert!(
        repository
            .begin_execution_maintenance(
                ExecutionScope::Tenant {
                    tenant_id: TenantId::new(),
                },
                kind,
            )
            .await
            .is_err()
    );
    assert!(
        repository
            .load_execution_maintenance_checkpoint(ExecutionScope::ControlPlane, kind)
            .await?
            .is_none()
    );

    let first = repository
        .begin_execution_maintenance(ExecutionScope::ControlPlane, kind)
        .await?;
    assert_eq!(first.generation, 1);
    assert!(first.last_started_at.is_some());
    let oversized_error = "💥".repeat(2_000);
    let ExecutionMaintenanceSettlementOutcome::Applied(failed) = repository
        .fail_execution_maintenance(
            ExecutionScope::ControlPlane,
            kind,
            first.generation,
            &oversized_error,
        )
        .await?
    else {
        panic!("current maintenance generation must record failure");
    };
    assert!(failed.last_failure_at.is_some());
    assert!(
        failed
            .last_error
            .as_ref()
            .is_some_and(|error| error.len() <= 4_096)
    );

    let second = repository
        .begin_execution_maintenance(ExecutionScope::ControlPlane, kind)
        .await?;
    assert_eq!(second.generation, 2);
    assert_eq!(
        repository
            .complete_execution_maintenance(ExecutionScope::ControlPlane, kind, first.generation,)
            .await?,
        ExecutionMaintenanceSettlementOutcome::StaleOrMissing
    );
    let ExecutionMaintenanceSettlementOutcome::Applied(succeeded) = repository
        .complete_execution_maintenance(ExecutionScope::ControlPlane, kind, second.generation)
        .await?
    else {
        panic!("current maintenance generation must record success");
    };
    assert!(succeeded.last_succeeded_at.is_some());
    let loaded = repository
        .load_execution_maintenance_checkpoint(ExecutionScope::ControlPlane, kind)
        .await?
        .expect("maintenance health receipt must persist");
    assert_eq!(loaded, succeeded);
    Ok(())
}

#[tokio::test]
async fn paused_task_review_decision_persists_until_single_resume_activation_db() -> TestResult {
    // Pins: a decision for the exact pre-pause review owner is storage-only while paused, is
    // replayable, and becomes runnable through the single activation created by resume.
    assert_paused_task_review_resolution(ExecutionActionReviewResolution::Completed {
        tool_output: json!({"approved": true}),
    })
    .await
}

#[tokio::test]
async fn paused_task_review_timeout_persists_until_single_resume_activation_db() -> TestResult {
    // Pins: an exact review timeout cannot become stale merely because pause advanced the run
    // controller generation; it remains storage-only and resumes through one activation.
    assert_paused_task_review_resolution(ExecutionActionReviewResolution::TimedOut {
        reason: "review expired".to_string(),
    })
    .await
}

async fn assert_paused_task_review_resolution(
    resolution: ExecutionActionReviewResolution,
) -> TestResult {
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = execution_capacity_config();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "paused-task-review-resolution",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    candidate.plan.definition.nodes = vec![watchdog_output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: 1,
                    node_id: "watchdog-work".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(
                        run.run_uid,
                        "watchdog-work",
                        "reviewed",
                        estimate(1),
                    )],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admission = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one reviewed task must be admitted");
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
    let TaskAttemptStartOutcome::Started(started) = repository.start_task_attempt(fence).await?
    else {
        panic!("review fixture must start its exact admitted attempt");
    };
    let review_uid = Uuid::now_v7();
    let now = Utc::now();
    assert!(matches!(
        repository
            .begin_task_attempt_release(fence, started.task.generation, "action_review", now)
            .await?,
        TaskAttemptReleaseClaimOutcome::Applied(_)
    ));
    let invocation = ToolInvocation {
        id: Some("paused-review-call".to_string()),
        name: "fixture_reviewed_tool".to_string(),
        input: json!({"value": 1}),
    };
    assert!(matches!(
        repository
            .park_task_attempt_on_review(
                NewTaskAttemptCheckpoint {
                    fence,
                    task_generation: started.task.generation,
                    kind: TaskAttemptCheckpointKind::CapabilityReview,
                    schema_version: 1,
                    payload: json!({
                        "state": {
                            "kind": "capability_review",
                            "pending_review": {
                                "review_uid": review_uid,
                                "expires_at": now + Duration::minutes(5),
                                "invocation": invocation,
                                "effect_idempotency": IdempotencyClass::NonIdempotent,
                            },
                            "usage": {},
                        },
                        "review_resolution": null,
                        "external_job_resolution": null,
                        "workspace_release_receipt_id": null,
                    }),
                    workspace_release_receipt: None,
                    created_at: now,
                },
                review_uid,
            )
            .await?,
        TaskAttemptReviewParkOutcome::Applied { .. }
    ));
    let review_park_activation_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_dispatch_outbox \
         WHERE run_uid=$1 AND dispatch_kind='run_activation' \
           AND payload->>'source'='task_attempt_review_park' \
           AND payload->>'task_id'=$2 AND payload->>'dispatch_uid'=$3 \
           AND (payload->>'attempt_generation')::BIGINT=$4",
    )
    .bind(run.run_uid)
    .bind(fence.task_id.as_uuid().to_string())
    .bind(fence.dispatch_uid.to_string())
    .bind(i64::try_from(fence.attempt_generation)?)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        review_park_activation_count, 1,
        "review parking must wake the controller to release ActiveRuns ownership"
    );
    let TransitionOutcome::RunApplied(paused) = repository
        .pause_run(scope, &config, run.run_uid, run.controller_generation)
        .await?
    else {
        panic!("a run with only a storage-owned review must pause exactly");
    };
    assert_eq!(paused.status, ExecutionRunStatus::Paused);
    assert_eq!(
        paused.controller_generation,
        fence.controller_generation + 1
    );

    let request = |resolved_at| ResolveTaskAttemptReviewRequest {
        scope: ExecutionScope::ControlPlane,
        run_uid: paused.run_uid,
        task_id: fence.task_id,
        expected_task_generation: started.task.generation,
        review_uid,
        resolution: resolution.clone(),
        resolved_at,
    };
    let TaskAttemptReviewResolutionOutcome::Applied { task, checkpoint } = repository
        .resolve_task_attempt_review(&config, request(now + Duration::seconds(1)))
        .await?
    else {
        panic!("the exact pre-pause task review owner must remain resolvable");
    };
    assert_eq!(task.status, ExecutionTaskStatus::Ready);
    assert_eq!(
        checkpoint.controller_generation,
        paused.controller_generation
    );
    assert_eq!(
        run_activation_count_for_generation(&pool, paused.run_uid, paused.controller_generation,)
            .await?,
        0,
        "review resolution must not wake a paused run"
    );
    assert!(matches!(
        repository
            .resolve_task_attempt_review(&config, request(now + Duration::seconds(2)))
            .await?,
        TaskAttemptReviewResolutionOutcome::Replayed { .. }
    ));

    let TransitionOutcome::RunApplied(resumed) = repository
        .resume_run(scope, &config, paused.run_uid, paused.controller_generation)
        .await?
    else {
        panic!("paused task review must resume after its decision is persisted");
    };
    assert_eq!(
        run_activation_count_for_generation(&pool, resumed.run_uid, resumed.controller_generation)
            .await?,
        1,
        "resume must enqueue exactly one controller activation"
    );
    Ok(())
}

async fn run_activation_count_for_generation(
    pool: &sqlx::PgPool,
    run_uid: Uuid,
    controller_generation: u64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_dispatch_outbox WHERE run_uid=$1 \
         AND controller_generation=$2 AND dispatch_kind='run_activation' \
         AND state <> 'cancelled'",
    )
    .bind(run_uid)
    .bind(i64::try_from(controller_generation).expect("fixture generation fits i64"))
    .fetch_one(pool)
    .await
}

fn run_deadline(
    trigger_uid: Uuid,
    tenant_id: TenantId,
    run_uid: Uuid,
    controller_generation: u64,
    due_at: chrono::DateTime<Utc>,
) -> NewExecutionTrigger {
    NewExecutionTrigger {
        trigger_uid,
        tenant_id,
        run_uid: Some(run_uid),
        task_id: None,
        compensation_id: None,
        schedule_uid: None,
        schedule_incarnation: None,
        kind: ExecutionTriggerKind::RunDeadline,
        controller_generation: Some(controller_generation),
        attempt_generation: None,
        compensation_generation: None,
        compensation_attempt_generation: None,
        occurrence_sequence: None,
        due_at,
        payload: json!({"run_uid": run_uid, "deadline_at": due_at}),
    }
}

fn task_watchdog(
    trigger_uid: Uuid,
    tenant_id: TenantId,
    run_uid: Uuid,
    task_id: Uuid,
    controller_generation: u64,
    attempt_generation: u64,
    due_at: chrono::DateTime<Utc>,
) -> NewExecutionTrigger {
    NewExecutionTrigger {
        trigger_uid,
        tenant_id,
        run_uid: Some(run_uid),
        task_id: Some(task_id),
        compensation_id: None,
        schedule_uid: None,
        schedule_incarnation: None,
        kind: ExecutionTriggerKind::TaskWatchdog,
        controller_generation: Some(controller_generation),
        attempt_generation: Some(attempt_generation),
        compensation_generation: None,
        compensation_attempt_generation: None,
        occurrence_sequence: None,
        due_at,
        payload: json!({}),
    }
}

fn run_activation(
    tenant_id: TenantId,
    run_uid: Uuid,
    wake_epoch: u64,
    not_before_at: chrono::DateTime<Utc>,
) -> NewExecutionDispatch {
    NewExecutionDispatch {
        dispatch_uid: Uuid::now_v7(),
        tenant_id,
        run_uid: Some(run_uid),
        task_id: None,
        compensation_id: None,
        trigger_uid: None,
        external_job_uid: None,
        kind: ExecutionDispatchKind::RunActivation,
        controller_generation: Some(1),
        wake_epoch: Some(wake_epoch),
        attempt_generation: None,
        compensation_generation: None,
        compensation_attempt_generation: None,
        not_before_at,
        payload: json!({"wake_epoch": wake_epoch}),
    }
}

fn callback(
    external_job_uid: Uuid,
    job_generation: u64,
    provider_event_id: &str,
    update: ExecutionExternalJobCallbackUpdate,
) -> ExecutionExternalJobCallback {
    ExecutionExternalJobCallback {
        external_job_uid,
        job_generation,
        provider: "batch-provider".to_string(),
        provider_job_id: "provider-job-1".to_string(),
        provider_event_id: provider_event_id.to_string(),
        update,
    }
}
