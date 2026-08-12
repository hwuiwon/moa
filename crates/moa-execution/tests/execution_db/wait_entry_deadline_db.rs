//! Wait entry that cannot finish before the run deadline projects a typed task failure.

use moa_artifacts::execution_plan::{ExecutionNode, ExecutionOperation};
use moa_execution::repository::ready::{ReadyMaterializationOutcome, ReadyMaterializationRequest};
use moa_execution::repository::task::{
    TaskAttemptFence, TaskAttemptSettlementOutcome, TaskAttemptStartOutcome,
};

use super::support::*;

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
    assert_eq!(failed_run.next_wake_at, None);
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
    Ok(())
}

#[tokio::test]
async fn input_wait_past_run_deadline_fails_its_task_instead_of_erroring_db() -> TestResult {
    // Pins: settling a NeedsInput attempt whose plan-level input-wait expiry resolves at or after
    // the run deadline terminates the task with a typed DeadlineExceeded failure naming its node
    // rather than aborting settlement; an expiry that still fits parks the ordinary input wait.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();

    for (key, expiry_seconds, expected_status) in [
        (
            "input-wait-past-deadline",
            86_400_u64,
            ExecutionTaskStatus::Failed,
        ),
        (
            "input-wait-inside-deadline",
            60,
            ExecutionTaskStatus::WaitingInput,
        ),
    ] {
        let mut candidate = new_run(tenant_id, None, key, ExecutionRunStatus::Queued, budget(10));
        candidate.plan.definition.nodes = vec![output_node("ask", &[])];
        candidate.plan.definition.input_wait_policy = ExecutionWaitPolicy {
            expiry: ExecutionTemporalTarget::After {
                delay_seconds: expiry_seconds,
            },
            on_expiry: ExecutionWaitExpiryAction::FailTask,
        };
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
            panic!("{key} settlement must apply");
        };
        assert_eq!(task.status, expected_status, "{key}");
        if expected_status == ExecutionTaskStatus::Failed {
            let (class, message) =
                failure_class(&task).expect("the failed input wait must carry a typed outcome");
            assert_eq!(class, ExecutionFailureClass::DeadlineExceeded);
            assert!(
                message.contains("`ask`"),
                "the failure must name its node, got `{message}`"
            );
            let settled_run = repository
                .load_run(scope, run.run_uid)
                .await?
                .expect("run must remain visible");
            assert_eq!(settled_run.waiting_input_task_count, 0);
            assert_eq!(settled_run.progress_failed_tasks, 1);
        }
    }
    Ok(())
}
