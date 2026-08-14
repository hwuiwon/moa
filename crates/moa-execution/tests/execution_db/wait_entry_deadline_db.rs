//! Wait entry that cannot finish before the run deadline projects a typed task failure.

use std::time::Duration as StdDuration;

use moa_artifacts::execution_plan::{ExecutionNode, ExecutionOperation};
use moa_execution::repository::ready::{ReadyMaterializationOutcome, ReadyMaterializationRequest};
use moa_execution::repository::task::{
    TaskAttemptFence, TaskAttemptSettlementOutcome, TaskAttemptStartOutcome,
};
use moa_execution::repository::terminal::{
    PendingTerminalAdvanceOutcome, PendingTerminalAdvanceStage,
};
use moa_execution::repository::trigger::{
    ExecutionRunDeadlineTriggerOutcome, ExecutionTriggerNoOp,
};
use moa_execution::state::ExecutionTerminalEvidence;

use super::support::*;

#[tokio::test]
async fn storage_wait_materialization_locks_trigger_capacity_before_run_db() -> TestResult {
    // Pins: a storage-only task waits for ScheduledTriggers capacity before taking its run row,
    // so concurrent trigger settlement cannot deadlock with wait materialization.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let mut candidate = new_run(
        tenant_id,
        None,
        "storage-wait-lock-order",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![output_node("wait", &[])];
    let run = create_run(&repository, scope, candidate).await?;
    let run_uid = run.run_uid;
    repository
        .initialize_scheduler_state(scope, run_uid)
        .await?;
    let mut task = logical_task(run_uid, "wait", "", estimate(1));
    task.kind = LogicalTaskKind::WaitUntil {
        wake: ExecutionTemporalTarget::After { delay_seconds: 60 },
        result: json!({"elapsed": true}),
    };

    let mut capacity_holder = pool.begin().await?;
    let locked: Vec<Uuid> = sqlx::query_scalar(
        "SELECT capacity_bucket_uid FROM moa.execution_capacity_bucket \
         WHERE resource_dimension='scheduled_triggers' \
           AND ((scope_kind='fleet' AND tenant_id IS NULL) \
             OR (scope_kind='tenant' AND tenant_id=$1)) \
         ORDER BY CASE scope_kind WHEN 'fleet' THEN 0 ELSE 1 END FOR UPDATE",
    )
    .bind(tenant_id.0)
    .fetch_all(&mut *capacity_holder)
    .await?;
    assert_eq!(locked.len(), 2);

    let materialization_repository = repository.clone();
    let materialization_config = config.clone();
    let mut materialization = tokio::spawn(async move {
        materialization_repository
            .materialize_ready_page(
                scope,
                &materialization_config,
                ReadyMaterializationRequest {
                    run_uid,
                    plan_revision: 1,
                    node_id: "wait".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![task],
                },
            )
            .await
    });
    assert!(
        tokio::time::timeout(StdDuration::from_millis(100), &mut materialization)
            .await
            .is_err(),
        "wait materialization must block on ScheduledTriggers capacity"
    );
    sqlx::query("SELECT run_uid FROM moa.execution_run WHERE run_uid=$1 FOR UPDATE NOWAIT")
        .bind(run_uid)
        .fetch_one(&mut *capacity_holder)
        .await?;
    capacity_holder.commit().await?;

    let outcome = tokio::time::timeout(StdDuration::from_secs(5), materialization).await???;
    assert!(matches!(
        outcome,
        ReadyMaterializationOutcome::Applied { ref triggers, .. } if triggers.len() == 1
    ));
    Ok(())
}

fn output_node(id: &str, depends_on: &[&str]) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["req".to_string()],
        depends_on: depends_on.iter().map(|id| (*id).to_string()).collect(),
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

fn failure_class(task: &ExecutionTaskRecord) -> Option<(ExecutionFailureClass, String)> {
    match task.current_outcome.as_ref().map(|outcome| &outcome.result) {
        Some(ExecutionTaskResult::Failed { class, message }) => {
            Some((class.clone(), message.clone()))
        }
        _ => None,
    }
}

#[tokio::test]
async fn storage_wait_past_run_deadline_fails_its_node_instead_of_erroring_db() -> TestResult {
    // Pins: a relative storage wait that was legal at compile time but resolves at or after the
    // run deadline once wait entry is reached materializes as a terminal DeadlineExceeded task
    // failure naming its node, cascades to its unmaterialized dependent, and parks no trigger —
    // instead of aborting materialization with an infra-shaped error. A wait that still fits
    // inside the deadline keeps parking normally.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let mut candidate = new_run(
        tenant_id,
        None,
        "storage-wait-past-deadline",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![
        output_node("late", &[]),
        output_node("after", &["late"]),
        output_node("early", &[]),
    ];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;

    // The approved budget expires in one hour, so a two-hour wait can never settle in time.
    let mut late = logical_task(run.run_uid, "late", "", estimate(1));
    late.kind = LogicalTaskKind::WaitUntil {
        wake: ExecutionTemporalTarget::After {
            delay_seconds: 7_200,
        },
        result: json!({ "elapsed": true }),
    };
    let ReadyMaterializationOutcome::Applied {
        tasks,
        triggers,
        next_cursor,
    } = repository
        .materialize_ready_page(
            scope,
            &config,
            ReadyMaterializationRequest {
                run_uid: run.run_uid,
                plan_revision: 1,
                node_id: "late".to_string(),
                expected_cursor: 0,
                reduce_cursor: None,
                source_exhausted: true,
                terminal_output: None,
                condition_skipped: false,
                tasks: vec![late],
            },
        )
        .await?
    else {
        panic!("a wait past the run deadline must still apply as a typed failure");
    };
    assert_eq!(next_cursor, 1);
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].status, ExecutionTaskStatus::Failed);
    assert!(
        triggers.is_empty(),
        "an unenterable wait must not park a durable trigger"
    );
    let (class, message) =
        failure_class(&tasks[0]).expect("the failed wait must carry a typed failure outcome");
    assert_eq!(class, ExecutionFailureClass::DeadlineExceeded);
    assert!(
        message.contains("`late`"),
        "the failure must name its node, got `{message}`"
    );
    assert!(tasks[0].completed_at.is_some());

    let node_projection = sqlx::query_as::<_, (String, i64, i64, i64)>(
        "SELECT node_status, total_task_count, terminal_task_count, failed_task_count \
         FROM moa.execution_node_state WHERE run_uid=$1 AND node_id='late'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(node_projection, ("failed".to_string(), 1, 1, 1));
    let dependent_status = sqlx::query_scalar::<_, String>(
        "SELECT node_status FROM moa.execution_node_state WHERE run_uid=$1 AND node_id='after'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        dependent_status, "cancelled",
        "the failed wait must cascade through the normal terminal projection"
    );

    let failed_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run must remain visible");
    assert_eq!(failed_run.progress_failed_tasks, 1);
    assert_eq!(failed_run.waiting_task_count, 0);
    assert_eq!(failed_run.ready_task_count, 0);
    assert_eq!(
        failed_run.next_wake_at, failed_run.approved_budget.deadline_at,
        "the run deadline remains the next durable wake when no task-local wait is parked"
    );
    assert!(failed_run.waiting_reasons.is_empty());

    // A wait that still fits inside the same deadline keeps its ordinary parked projection.
    let mut early = logical_task(run.run_uid, "early", "", estimate(1));
    early.kind = LogicalTaskKind::WaitUntil {
        wake: ExecutionTemporalTarget::After { delay_seconds: 60 },
        result: json!({ "elapsed": true }),
    };
    let ReadyMaterializationOutcome::Applied {
        tasks, triggers, ..
    } = repository
        .materialize_ready_page(
            scope,
            &config,
            ReadyMaterializationRequest {
                run_uid: run.run_uid,
                plan_revision: 1,
                node_id: "early".to_string(),
                expected_cursor: 0,
                reduce_cursor: None,
                source_exhausted: true,
                terminal_output: None,
                condition_skipped: false,
                tasks: vec![early],
            },
        )
        .await?
    else {
        panic!("a wait inside the run deadline must park normally");
    };
    assert_eq!(tasks[0].status, ExecutionTaskStatus::WaitingTimer);
    assert_eq!(triggers.len(), 1);
    let trigger_uid = triggers[0].trigger_uid;
    let bucket_before: Vec<(String, i64)> = sqlx::query_as(
        "SELECT scope_kind,reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE resource_dimension='scheduled_triggers' \
           AND (scope_kind='fleet' OR tenant_id=$1) ORDER BY scope_kind",
    )
    .bind(tenant_id.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(bucket_before.len(), 2);
    let run_scheduled_before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_capacity_reservation \
         WHERE run_uid=$1 AND resource_dimension='scheduled_triggers' \
           AND state IN ('reserved','reconciling')",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert!(run_scheduled_before >= 1);
    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("wait-owning run remains visible");

    let PendingTerminalAdvanceOutcome::Applied(commit) = repository
        .fence_completion_terminal_and_enqueue_settlement(
            &config,
            scope,
            run.run_uid,
            current.controller_generation,
            current.wake_epoch,
            PendingExecutionTerminal {
                status: ExecutionRunStatus::Failed,
                reason: ExecutionTerminalReason::InternalFailure,
                terminal_evidence: ExecutionTerminalEvidence {
                    cause: ExecutionTerminalCause::InternalFailure,
                    satisfied_requirement_count: 0,
                    requirement_count: 1,
                },
                completion_check_results: Vec::new(),
                terminal_gaps: vec!["fixture terminal fence".to_string()],
                output: None,
                cancellation_reason: None,
            },
            moa_test_support::fixtures::pg_now(),
            100,
        )
        .await?
    else {
        panic!("terminal fencing must settle the storage-only wait");
    };
    assert_eq!(commit.stage, PendingTerminalAdvanceStage::Finalized);
    let trigger_state: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_trigger WHERE trigger_uid=$1")
            .bind(trigger_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(trigger_state, "superseded");
    let dispatch_state: String =
        sqlx::query_scalar("SELECT state FROM moa.execution_dispatch_outbox WHERE trigger_uid=$1")
            .bind(trigger_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        dispatch_state, "cancelled",
        "superseding a storage wait must terminally settle its pending outbox delivery"
    );
    let capacity_state: (String, bool) = sqlx::query_as(
        "SELECT state,released_at IS NOT NULL FROM moa.execution_capacity_reservation \
         WHERE trigger_uid=$1 AND resource_dimension='scheduled_triggers'",
    )
    .bind(trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(capacity_state, ("released".to_string(), true));
    let bucket_after: Vec<(String, i64)> = sqlx::query_as(
        "SELECT scope_kind,reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE resource_dimension='scheduled_triggers' \
           AND (scope_kind='fleet' OR tenant_id=$1) ORDER BY scope_kind",
    )
    .bind(tenant_id.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(bucket_after.len(), bucket_before.len());
    for ((before_scope, before), (after_scope, after)) in
        bucket_before.into_iter().zip(bucket_after)
    {
        assert_eq!(after_scope, before_scope);
        assert_eq!(
            after,
            before - run_scheduled_before,
            "the wait plus any run-owned terminalized triggers must release every exact receipt"
        );
    }
    Ok(())
}

#[tokio::test]
async fn input_wait_suspends_run_deadline_without_trigger_or_active_capacity_and_resumes_exactly_db()
-> TestResult {
    // Pins: a fully human-input-parked run has no input-expiry trigger or active-task capacity,
    // an already-dispatched deadline cannot terminalize it, and exact input shifts then rearms
    // the run deadline without weakening per-attempt watchdogs.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();

    let mut candidate = new_run(
        tenant_id,
        None,
        "input-wait-indefinite",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![output_node("ask", &[])];
    let run = create_run(&repository, scope, candidate).await?;
    let original_deadline = run.approved_budget.deadline_at.expect("fixture deadline");
    let deadline_trigger_uid: Uuid = sqlx::query_scalar(
        "SELECT trigger_uid FROM moa.execution_trigger \
         WHERE run_uid=$1 AND trigger_kind='run_deadline' AND state='pending'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
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
                    node_id: "ask".to_string(),
                    expected_cursor: 0,
                    reduce_cursor: None,
                    source_exhausted: true,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: vec![logical_task(run.run_uid, "ask", "", estimate(1))],
                },
            )
            .await?,
        ReadyMaterializationOutcome::Applied { .. }
    ));
    let admission = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?;
    let admitted = admission
        .admitted
        .into_iter()
        .find(|item| item.run_uid == run.run_uid)
        .expect("the only ready task must be admitted");
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
    assert!(matches!(
        repository.start_task_attempt(fence).await?,
        TaskAttemptStartOutcome::Started(_)
    ));
    let TaskAttemptSettlementOutcome::Applied { task, .. } = repository
        .settle_task_attempt(&config, fence, needs_input(1), None, Utc::now())
        .await?
    else {
        panic!("NeedsInput settlement must apply");
    };
    assert_eq!(task.status, ExecutionTaskStatus::WaitingInput);
    let parked = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("parked run remains visible");
    assert_eq!(parked.approved_budget.deadline_at, Some(original_deadline));
    let suspended_at = parked
        .budget_deadline_suspended_at
        .expect("human input wait suspends wall-clock deadline accounting");
    assert_eq!(parked.active_task_count, 0);
    assert_eq!(parked.waiting_task_count, 1);
    assert_eq!(parked.waiting_input_task_count, 1);
    let active_task_receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_capacity_reservation \
         WHERE run_uid=$1 AND resource_dimension='active_tasks' \
           AND state IN ('reserved','reconciling')",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(active_task_receipts, 0);
    let input_expiry_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_trigger \
         WHERE run_uid=$1 AND task_id=$2 AND trigger_kind='wait_expiry'",
    )
    .bind(run.run_uid)
    .bind(task.task_id.as_uuid())
    .fetch_one(&pool)
    .await?;
    assert_eq!(input_expiry_rows, 0);

    assert_eq!(
        repository
            .prepare_run_deadline_trigger(scope, deadline_trigger_uid)
            .await?,
        ExecutionRunDeadlineTriggerOutcome::NoOp(ExecutionTriggerNoOp::Inactive)
    );
    assert!(matches!(
        repository
            .fence_deadline_and_enqueue_settlement(
                &config,
                scope,
                run.run_uid,
                parked.controller_generation,
                parked.wake_epoch,
                original_deadline + Duration::hours(1),
                1,
            )
            .await?,
        PendingTerminalAdvanceOutcome::Conflict
    ));

    let TransitionOutcome::Applied(resumed) = repository
        .resume_task_with_input(
            scope,
            &config,
            run.run_uid,
            task.task_id,
            task.generation,
            json!({"answer": "continue"}),
        )
        .await?
    else {
        panic!("exact input must resume the parked generation");
    };
    assert_eq!(resumed.status, ExecutionTaskStatus::Ready);
    let resumed_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("resumed run remains visible");
    assert!(resumed_run.budget_deadline_suspended_at.is_none());
    let shifted_deadline = resumed_run
        .approved_budget
        .deadline_at
        .expect("resume restores the deadline");
    assert!(shifted_deadline > original_deadline);
    assert_eq!(
        shifted_deadline - original_deadline,
        resumed
            .ready_at
            .expect("resumed task has an exact ready time")
            - suspended_at
    );
    let current_deadlines: Vec<chrono::DateTime<Utc>> = sqlx::query_scalar(
        "SELECT due_at FROM moa.execution_trigger \
         WHERE run_uid=$1 AND trigger_kind='run_deadline' AND state='pending'",
    )
    .bind(run.run_uid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(current_deadlines, vec![shifted_deadline]);

    let readmission = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .find(|item| item.run_uid == run.run_uid)
        .expect("resumed task is admitted under its new attempt generation");
    let second_fence = TaskAttemptFence {
        tenant_id: readmission.tenant_id,
        run_uid: readmission.run_uid,
        task_id: readmission.task_id,
        controller_generation: readmission.controller_generation,
        attempt_generation: readmission.attempt_generation,
        dispatch_uid: readmission.dispatch_uid,
        capacity_reservation_uid: readmission.capacity_reservation_uid,
        watchdog_trigger_uid: readmission.watchdog_trigger_uid,
        attempt_deadline_at: readmission.attempt_deadline_at,
    };
    assert!(matches!(
        repository.start_task_attempt(second_fence).await?,
        TaskAttemptStartOutcome::Started(_)
    ));
    let second_settlement = repository
        .settle_task_attempt(&config, second_fence, needs_input(2), None, Utc::now())
        .await?;
    assert!(
        matches!(
            second_settlement,
            TaskAttemptSettlementOutcome::Applied { .. }
        ),
        "second NeedsInput settlement must apply, got {second_settlement:?}"
    );
    let suspended_again = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("second input wait remains visible");
    assert!(suspended_again.budget_deadline_suspended_at.is_some());
    let terminal_evidence =
        moa_execution::completion::cancellation_terminal_evidence_from_completed_nodes(
            &suspended_again.goal,
            &suspended_again.active_plan,
            &std::collections::BTreeSet::<String>::new(),
        )?;
    let PendingTerminalAdvanceOutcome::Applied(cancelled) = repository
        .fence_completion_terminal_and_enqueue_settlement(
            &config,
            scope,
            run.run_uid,
            suspended_again.controller_generation,
            suspended_again.wake_epoch,
            PendingExecutionTerminal {
                status: ExecutionRunStatus::Cancelled,
                reason: ExecutionTerminalReason::Cancelled,
                terminal_evidence,
                completion_check_results: Vec::new(),
                terminal_gaps: Vec::new(),
                output: None,
                cancellation_reason: Some("operator cancelled during input wait".to_string()),
            },
            Utc::now(),
            8,
        )
        .await?
    else {
        panic!("explicit cancellation must drain a deadline-suspended input wait");
    };
    assert_eq!(cancelled.run.status, ExecutionRunStatus::Cancelled);
    assert!(cancelled.run.budget_deadline_suspended_at.is_none());
    Ok(())
}

#[tokio::test]
async fn last_active_completion_suspends_deadline_when_sibling_waits_for_input_db() -> TestResult {
    // Pins: deadline suspension is fenced by the locked run counters, not by the task whose
    // settlement happens to make the run fully input-parked. A sibling may already be waiting.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let mut candidate = new_run(
        tenant_id,
        None,
        "input-wait-last-active",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![
        output_node("ask-a", &[]),
        output_node("ask-b", &[]),
        output_node("finish", &[]),
    ];
    let run = create_run(&repository, scope, candidate).await?;
    repository
        .initialize_scheduler_state(scope, run.run_uid)
        .await?;

    let tasks = [
        logical_task(run.run_uid, "ask-a", "", estimate(1)),
        logical_task(run.run_uid, "ask-b", "", estimate(1)),
        logical_task(run.run_uid, "finish", "", estimate(1)),
    ];
    for task in &tasks {
        assert!(matches!(
            repository
                .materialize_ready_page(
                    scope,
                    &config,
                    ReadyMaterializationRequest {
                        run_uid: run.run_uid,
                        plan_revision: 1,
                        node_id: task.node_id.clone(),
                        expected_cursor: 0,
                        reduce_cursor: None,
                        source_exhausted: true,
                        terminal_output: None,
                        condition_skipped: false,
                        tasks: vec![task.clone()],
                    },
                )
                .await?,
            ReadyMaterializationOutcome::Applied { next_cursor: 1, .. }
        ));
    }
    let admitted = repository
        .admit_ready_attempts(&config, 3, Utc::now())
        .await?
        .admitted;
    assert_eq!(admitted.len(), 3);
    let fences = admitted
        .iter()
        .map(|item| TaskAttemptFence {
            tenant_id: item.tenant_id,
            run_uid: item.run_uid,
            task_id: item.task_id,
            controller_generation: item.controller_generation,
            attempt_generation: item.attempt_generation,
            dispatch_uid: item.dispatch_uid,
            capacity_reservation_uid: item.capacity_reservation_uid,
            watchdog_trigger_uid: item.watchdog_trigger_uid,
            attempt_deadline_at: item.attempt_deadline_at,
        })
        .collect::<Vec<_>>();
    for fence in &fences {
        assert!(matches!(
            repository.start_task_attempt(*fence).await?,
            TaskAttemptStartOutcome::Started(_)
        ));
    }

    assert!(matches!(
        repository
            .settle_task_attempt(&config, fences[0], needs_input(1), None, Utc::now())
            .await?,
        TaskAttemptSettlementOutcome::Applied { .. }
    ));
    let partly_active = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("partly active run remains visible");
    assert_eq!(partly_active.active_task_count, 2);
    assert_eq!(partly_active.waiting_input_task_count, 1);
    assert!(partly_active.budget_deadline_suspended_at.is_none());

    assert!(matches!(
        repository
            .settle_task_attempt(&config, fences[1], needs_input(1), None, Utc::now())
            .await?,
        TaskAttemptSettlementOutcome::Applied { .. }
    ));
    assert!(matches!(
        repository
            .settle_task_attempt(&config, fences[2], completed(1), None, Utc::now())
            .await?,
        TaskAttemptSettlementOutcome::Applied { .. }
    ));
    let fully_parked = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("fully parked run remains visible");
    assert_eq!(fully_parked.active_task_count, 0);
    assert_eq!(fully_parked.waiting_task_count, 2);
    assert_eq!(fully_parked.waiting_input_task_count, 2);
    assert!(fully_parked.budget_deadline_suspended_at.is_some());
    let active_task_receipts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_capacity_reservation \
         WHERE run_uid=$1 AND resource_dimension='active_tasks' \
           AND state IN ('reserved','reconciling')",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(active_task_receipts, 0);
    let input_expiry_rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_trigger \
         WHERE run_uid=$1 AND trigger_kind='wait_expiry'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(input_expiry_rows, 0);

    sqlx::query("UPDATE moa.execution_run SET activation_state='idle' WHERE run_uid=$1")
        .bind(run.run_uid)
        .execute(&pool)
        .await?;
    let idle_parked = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("fully parked run remains visible after idle checkpoint fixture");
    assert_eq!(idle_parked.activation_state, ExecutionActivationState::Idle);

    let terminal_evidence =
        moa_execution::completion::cancellation_terminal_evidence_from_completed_nodes(
            &idle_parked.goal,
            &idle_parked.active_plan,
            &std::collections::BTreeSet::from(["finish".to_string()]),
        )?;
    let PendingTerminalAdvanceOutcome::Applied(cancelled_page) = repository
        .fence_completion_terminal_and_enqueue_settlement(
            &config,
            scope,
            run.run_uid,
            idle_parked.controller_generation,
            idle_parked.wake_epoch,
            PendingExecutionTerminal {
                status: ExecutionRunStatus::Cancelled,
                reason: ExecutionTerminalReason::Cancelled,
                terminal_evidence,
                completion_check_results: Vec::new(),
                terminal_gaps: Vec::new(),
                output: None,
                cancellation_reason: Some("operator cancelled fully parked run".to_string()),
            },
            Utc::now(),
            1,
        )
        .await?
    else {
        panic!("idle external cancellation must checkpoint its bounded terminal page");
    };
    assert!(cancelled_page.work_remaining);
    assert_eq!(
        cancelled_page.run.activation_state,
        ExecutionActivationState::Queued
    );
    assert!(cancelled_page.continuation.is_some());
    assert!(cancelled_page.run.budget_deadline_suspended_at.is_some());
    Ok(())
}
