//! Task-outcome, external-wait, review, and replanning persistence contracts.

use super::support::*;
use moa_execution::{
    repository::{
        ready::{ReadyMaterializationOutcome, ReadyMaterializationRequest},
        task::{TaskAttemptFence, TaskAttemptSettlementOutcome, TaskAttemptStartOutcome},
    },
    state::LogicalTask,
};

#[tokio::test]
async fn input_resume_starts_a_schedulable_generation_without_a_prior_outcome_db() -> TestResult {
    // Pins: input redispatch starts one clean running generation while the prior NeedsInput
    // remains audit history, so the persisted scheduler projection accepts the resumed task.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut new_run = new_run(
        tenant_id,
        None,
        "input-resume-projection",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    new_run.plan.definition.nodes = vec![moa_artifacts::execution_plan::ExecutionNode {
        id: "input-resume".to_string(),
        requirement_ids: Vec::new(),
        depends_on: Vec::new(),
        when: None,
        input: json!({}),
        output_schema: json!({"type": "object"}),
        operation: moa_artifacts::execution_plan::ExecutionOperation::Output { value: json!({}) },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 3,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
        },
        budget: None,
    }];
    let run = create_run(&repository, scope, new_run).await?;
    let task = logical_task(run.run_uid, "input-resume", "", estimate(10));
    let fence = materialize_admit_and_start(&repository, scope, run.run_uid, task.clone()).await?;
    assert!(matches!(
        repository
            .settle_task_attempt(
                &ExecutionConfig::default(),
                fence,
                needs_input(1),
                None,
                Utc::now(),
            )
            .await?,
        TaskAttemptSettlementOutcome::Applied { .. }
    ));

    let TransitionOutcome::Applied(resumed) = repository
        .resume_task_with_input(
            scope,
            &ExecutionConfig::default(),
            run.run_uid,
            task.task_id,
            1,
            json!({"answer": "approved"}),
        )
        .await?
    else {
        panic!("input resume must apply");
    };
    assert_eq!(resumed.status, ExecutionTaskStatus::Ready);
    assert_eq!(resumed.generation, 2);
    assert!(
        resumed.current_outcome.is_none(),
        "the resumed generation must not project the prior generation's NeedsInput outcome"
    );
    assert_eq!(resumed.outcome_audit.len(), 1);

    let persisted_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("resumed run should remain visible");
    let persisted_task = listed_task(&repository, scope, run.run_uid, task.task_id).await?;
    assert_eq!(persisted_run.status, ExecutionRunStatus::Running);
    assert_eq!(persisted_task.status, ExecutionTaskStatus::Ready);
    assert_eq!(persisted_task.generation, 2);
    assert!(persisted_task.current_outcome.is_none());
    Ok(())
}

#[tokio::test]
async fn retry_and_input_resume_terminalize_elapsed_or_exhausted_run_envelope_db() -> TestResult {
    // Pins: redispatch rejected by deadline or budget atomically records the
    // typed terminal failure, releases reservations, wakes finalization, and
    // remains idempotent without weakening the generation fence.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };

    for (kind, waiting_outcome) in [(
        "retry",
        ExecutionTaskOutcome {
            schema_version: 1,
            usage: usage(0),
            result: ExecutionTaskResult::Failed {
                class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
                message: "retry later".to_string(),
            },
        },
    )] {
        let mut candidate = new_run(
            tenant_id,
            None,
            &format!("elapsed-{kind}"),
            ExecutionRunStatus::Queued,
            ExecutionBudgetLimit {
                deadline_at: Some(
                    moa_test_support::fixtures::pg_now() + Duration::milliseconds(150),
                ),
                ..budget(2)
            },
        );
        candidate.plan.definition.nodes = vec![outcome_node(kind)];
        let run = create_run(&repository, scope, candidate).await?;
        let task = logical_task(run.run_uid, kind, "deadline", estimate(1));
        let _fence =
            materialize_admit_and_start(&repository, scope, run.run_uid, task.clone()).await?;
        assert!(matches!(
            repository
                .record_task_outcome(scope, run.run_uid, task.task_id, 1, waiting_outcome,)
                .await?,
            TaskOutcomeWrite::Applied { .. }
        ));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let before_terminal = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("waiting run should remain queryable");
        let transition = repository
            .retry_task(scope, run.run_uid, task.task_id, 1)
            .await?;
        let TransitionOutcome::Applied(terminal_task) = transition else {
            panic!("{kind} elapsed redispatch must terminalize atomically");
        };
        assert_terminal_redispatch_failure(
            &terminal_task,
            moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
        );
        let terminal_run = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("terminalized run should remain queryable");
        assert_eq!(terminal_run.status, ExecutionRunStatus::Running);
        assert_eq!(terminal_run.reserved, ExecutionEstimate::default());
        assert_eq!(terminal_run.consumed.tasks, 1);
        assert!(
            terminal_run.wake_epoch > before_terminal.wake_epoch,
            "deadline rejection must durably wake terminal evaluation"
        );
        let replay = repository
            .retry_task(scope, run.run_uid, task.task_id, 1)
            .await?;
        assert_eq!(
            replay,
            TransitionOutcome::AlreadyApplied(terminal_task.clone())
        );
        let stale = repository
            .retry_task(scope, run.run_uid, task.task_id, 0)
            .await?;
        assert_eq!(
            stale,
            TransitionOutcome::Rejected(
                moa_execution::repository::TransitionRejection::GenerationMismatch
            )
        );
    }

    for (kind, waiting_outcome) in [("input", needs_input(1)), ("retry", retryable(1))] {
        let mut candidate = new_run(
            tenant_id,
            None,
            &format!("exhausted-{kind}"),
            ExecutionRunStatus::Queued,
            ExecutionBudgetLimit {
                max_cost_microusd: Some(1),
                max_tokens: Some(1),
                max_tasks: Some(1),
                max_tool_calls: Some(1),
                max_retrieved_bytes: Some(1),
                deadline_at: Some(pg_deadline(Duration::hours(1))),
            },
        );
        candidate.plan.definition.nodes = vec![outcome_node(kind)];
        let run = create_run(&repository, scope, candidate).await?;
        let task = logical_task(run.run_uid, kind, "budget", estimate(1));
        let fence =
            materialize_admit_and_start(&repository, scope, run.run_uid, task.clone()).await?;
        if kind == "input" {
            assert!(matches!(
                repository
                    .settle_task_attempt(
                        &ExecutionConfig::default(),
                        fence,
                        waiting_outcome,
                        None,
                        Utc::now(),
                    )
                    .await?,
                TaskAttemptSettlementOutcome::Applied { .. }
            ));
        } else {
            assert!(matches!(
                repository
                    .record_task_outcome(scope, run.run_uid, task.task_id, 1, waiting_outcome,)
                    .await?,
                TaskOutcomeWrite::Applied { .. }
            ));
        }
        let before_terminal = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("waiting run should remain queryable");
        let transition = if kind == "input" {
            repository
                .resume_task_with_input(
                    scope,
                    &ExecutionConfig::default(),
                    run.run_uid,
                    task.task_id,
                    1,
                    json!({"ok": true}),
                )
                .await?
        } else {
            repository
                .retry_task(scope, run.run_uid, task.task_id, 1)
                .await?
        };
        let TransitionOutcome::Applied(terminal_task) = transition else {
            panic!("{kind} exhausted redispatch must terminalize atomically");
        };
        assert_terminal_redispatch_failure(
            &terminal_task,
            moa_artifacts::execution_plan::ExecutionFailureClass::BudgetExceeded,
        );
        let terminal_run = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("terminalized run should remain queryable");
        assert_eq!(terminal_run.status, ExecutionRunStatus::Running);
        assert_eq!(terminal_run.reserved, ExecutionEstimate::default());
        assert_eq!(terminal_run.consumed.tasks, 1);
        assert!(
            terminal_run.wake_epoch > before_terminal.wake_epoch,
            "budget rejection must durably wake terminal evaluation"
        );
        let replay = if kind == "input" {
            repository
                .resume_task_with_input(
                    scope,
                    &ExecutionConfig::default(),
                    run.run_uid,
                    task.task_id,
                    1,
                    json!({"ok": true}),
                )
                .await?
        } else {
            repository
                .retry_task(scope, run.run_uid, task.task_id, 1)
                .await?
        };
        assert_eq!(replay, TransitionOutcome::AlreadyApplied(terminal_task));
    }
    Ok(())
}

#[tokio::test]
async fn exact_external_wait_outcome_replay_recovers_committed_handoff_db() -> TestResult {
    // Pins: if the service loses its handler result after the outcome transaction
    // commits, replaying the same generation and payload must recover the accepted
    // outcome instead of reporting a terminal-task conflict that suppresses handoff.
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
            "external-wait-post-commit-replay",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;
    let task = logical_task(run.run_uid, "review", "", estimate(1));
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    let outcome = completed(1);
    assert!(matches!(
        repository
            .complete_external_wait(
                scope,
                &ExecutionConfig::default(),
                run.run_uid,
                task.task_id,
                task.generation,
                outcome.clone(),
            )
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));

    let replay = repository
        .complete_external_wait(
            scope,
            &ExecutionConfig::default(),
            run.run_uid,
            task.task_id,
            task.generation,
            outcome.clone(),
        )
        .await?;
    assert!(
        matches!(replay, TaskOutcomeWrite::Replayed { .. }),
        "exact post-commit replay must recover the accepted handoff, got {replay:?}"
    );
    let wrong_generation = repository
        .record_task_outcome(
            scope,
            run.run_uid,
            task.task_id,
            task.generation + 1,
            outcome,
        )
        .await?;
    assert!(matches!(
        wrong_generation,
        TaskOutcomeWrite::Rejected {
            reason: TaskOutcomeRejection::TerminalTask,
            ..
        }
    ));
    Ok(())
}

#[tokio::test]
async fn oversized_citation_id_is_rejected_without_projection_or_usage_mutation_db() -> TestResult {
    // Pins: production outcome persistence applies artifact validation before any run/task write.
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
            "oversized-citation-id",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let task = logical_task(run.run_uid, "citation", "oversized", estimate(3));
    let task = repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task])
        .await?
        .into_iter()
        .next()
        .expect("materialized citation task");
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    let before_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run exists before rejected outcome");
    let before_task = repository
        .load_task(scope, run.run_uid, task.task_id)
        .await?
        .expect("task exists before rejected outcome");

    let error = repository
        .record_task_outcome(
            scope,
            run.run_uid,
            task.task_id,
            1,
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: usage(2),
                result: ExecutionTaskResult::Completed {
                    output: json!({ "ok": true }),
                    citations: vec![ExecutionCitation {
                        source_id: "é".repeat(513),
                        uri: None,
                        locator: None,
                    }],
                },
            },
        )
        .await
        .expect_err("oversized citation source id must reject before persistence");
    assert!(matches!(
        error,
        moa_execution::Error::InvalidRepositoryInput { message }
            if message.contains("citation source_id must be at most 512 characters")
    ));

    assert_eq!(
        repository.load_run(scope, run.run_uid).await?,
        Some(before_run),
        "rejected outcome must not mutate run projection or usage"
    );
    assert_eq!(
        repository
            .load_task(scope, run.run_uid, task.task_id)
            .await?,
        Some(before_task),
        "rejected outcome must not mutate task projection, usage, or audit"
    );
    Ok(())
}

#[tokio::test]
async fn stale_and_terminal_outcomes_are_audited_without_projection_mutation_db() -> TestResult {
    // Pins: generation is the sole result CAS fence and every rejected delivery remains auditable.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "outcomes",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![outcome_node("outcome")];
    let run = create_run(&repository, scope, candidate).await?;
    let task = logical_task(run.run_uid, "outcome", "", estimate(10));
    let fence = materialize_admit_and_start(&repository, scope, run.run_uid, task.clone()).await?;
    assert!(matches!(
        repository
            .settle_task_attempt(
                &ExecutionConfig::default(),
                fence,
                needs_input(1),
                None,
                Utc::now(),
            )
            .await?,
        TaskAttemptSettlementOutcome::Applied { .. }
    ));
    let TransitionOutcome::Applied(resumed) = repository
        .resume_task_with_input(
            scope,
            &ExecutionConfig::default(),
            run.run_uid,
            task.task_id,
            1,
            json!({"answer": "approved"}),
        )
        .await?
    else {
        panic!("input resume must apply");
    };
    assert_eq!((resumed.attempt, resumed.generation), (1, 2));
    assert_eq!(
        resumed.resume_input_history,
        vec![json!({"answer": "approved"})],
        "resume payload history is exact and append-only"
    );
    assert_eq!(
        repository
            .resume_task_with_input(
                scope,
                &ExecutionConfig::default(),
                run.run_uid,
                task.task_id,
                1,
                json!({"answer": "approved"}),
            )
            .await?,
        TransitionOutcome::AlreadyApplied(resumed.clone()),
        "an exact accepted input redispatch must replay without advancing generation"
    );
    assert_eq!(
        repository
            .resume_task_with_input(
                scope,
                &ExecutionConfig::default(),
                run.run_uid,
                task.task_id,
                1,
                json!({"answer": "different"}),
            )
            .await?,
        TransitionOutcome::Rejected(
            moa_execution::repository::TransitionRejection::GenerationMismatch
        ),
        "a changed payload must retain the generation fence"
    );

    let second_fence = admit_and_start(&repository, run.run_uid, task.task_id).await?;

    let stale = repository
        .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(2))
        .await?;
    let TaskOutcomeWrite::Rejected {
        task: stale_task,
        reason: TaskOutcomeRejection::StaleGeneration,
    } = stale
    else {
        panic!("stale generation must be audit-only, got {stale:?}");
    };
    assert_eq!(stale_task.status, ExecutionTaskStatus::Running);
    assert!(stale_task.output.is_none());
    assert_eq!(stale_task.actual.tokens, 1);

    assert!(matches!(
        repository
            .settle_task_attempt(
                &ExecutionConfig::default(),
                second_fence,
                completed(2),
                None,
                Utc::now(),
            )
            .await?,
        TaskAttemptSettlementOutcome::Applied { .. }
    ));
    let duplicate = repository
        .record_task_outcome(scope, run.run_uid, task.task_id, 2, completed(9))
        .await?;
    let TaskOutcomeWrite::Rejected {
        task: terminal_task,
        reason: TaskOutcomeRejection::TerminalTask,
    } = duplicate
    else {
        panic!("terminal redelivery must be audit-only, got {duplicate:?}");
    };
    assert_eq!(terminal_task.status, ExecutionTaskStatus::Completed);
    assert_eq!(terminal_task.output, Some(json!({ "tokens": 2 })));
    assert_eq!(terminal_task.actual.tokens, 2);
    assert_eq!(terminal_task.outcome_audit.len(), 4);
    assert_eq!(terminal_task.outcome_audit[1]["accepted"], false);
    assert_eq!(terminal_task.outcome_audit[3]["accepted"], false);
    let run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run visible");
    assert_eq!(run.consumed.tokens, 2);
    assert_eq!(run.consumed.tasks, 1);
    assert_eq!(run.progress_completed_tasks, 1);
    Ok(())
}

#[tokio::test]
async fn task_outcomes_update_review_state_and_failure_accounting_exactly_db() -> TestResult {
    // Pins: review completion resumes the run, other waits remain parked, and only failures increment the failure counter.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let cases = [
        (
            "review-completed",
            ExecutionRunStatus::WaitingReview,
            completed(1),
            ExecutionRunStatus::Running,
            ExecutionTaskStatus::Completed,
            0,
        ),
        (
            "input-failed",
            ExecutionRunStatus::WaitingInput,
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: usage(1),
                result: ExecutionTaskResult::Failed {
                    class: moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
                    message: "terminal failure".to_string(),
                },
            },
            ExecutionRunStatus::Running,
            ExecutionTaskStatus::Failed,
            1,
        ),
    ];

    for (key, waiting_status, outcome, expected_run_status, expected_task_status, failed_tasks) in
        cases
    {
        let mut candidate = new_run(tenant_id, None, key, ExecutionRunStatus::Queued, budget(1));
        candidate.plan.definition.nodes = vec![outcome_node("outcome")];
        let run = create_run(&repository, scope, candidate).await?;
        let _running =
            claim_running_controller(&repository, scope, &ExecutionConfig::default(), &run).await?;
        let task = logical_task(run.run_uid, "outcome", key, estimate(1));
        let fence =
            materialize_admit_and_start(&repository, scope, run.run_uid, task.clone()).await?;
        let current = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("outcome fixture run remains visible");
        let claimed = match repository
            .claim_controller_wake(
                scope,
                current.run_uid,
                current.controller_generation,
                current.wake_epoch,
            )
            .await?
        {
            RunControllerClaimOutcome::Claimed(claimed)
            | RunControllerClaimOutcome::Resumed(claimed) => claimed,
            outcome => panic!("task wake must be claimable: {outcome:?}"),
        };
        assert!(matches!(
            repository
                .complete_controller_wake(
                    scope,
                    &ExecutionConfig::default(),
                    claimed.run_uid,
                    RunControllerCompletionRequest {
                        controller_generation: claimed.controller_generation,
                        wake_epoch: claimed.wake_epoch,
                        checkpoint: ExecutionRunActivationCheckpoint {
                            status: waiting_status,
                            activation_state: ExecutionActivationState::Idle,
                            next_wake_at: claimed.next_wake_at,
                            waiting_since: Some(Utc::now()),
                            ready_task_count: claimed.ready_task_count,
                            active_task_count: claimed.active_task_count,
                        },
                        continuation_payload: None,
                        continuation_not_before_at: Utc::now(),
                    },
                )
                .await?,
            RunControllerCompletionOutcome::Applied { .. }
        ));

        let TaskAttemptSettlementOutcome::Applied {
            run: persisted_run,
            task: persisted_task,
        } = repository
            .settle_task_attempt(
                &ExecutionConfig::default(),
                fence,
                outcome,
                None,
                Utc::now(),
            )
            .await?
        else {
            panic!("{key} outcome must apply");
        };
        assert_eq!(persisted_run.status, expected_run_status, "{key}");
        assert_eq!(persisted_task.status, expected_task_status, "{key}");
        assert_eq!(persisted_run.progress_failed_tasks, failed_tasks, "{key}");
    }
    Ok(())
}

fn outcome_node(id: &str) -> moa_artifacts::execution_plan::ExecutionNode {
    moa_artifacts::execution_plan::ExecutionNode {
        id: id.to_string(),
        requirement_ids: Vec::new(),
        depends_on: Vec::new(),
        when: None,
        input: json!({}),
        output_schema: json!({"type": "object"}),
        operation: moa_artifacts::execution_plan::ExecutionOperation::Output { value: json!({}) },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 3,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
        },
        budget: None,
    }
}

async fn materialize_admit_and_start(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: Uuid,
    task: LogicalTask,
) -> Result<TaskAttemptFence, Box<dyn std::error::Error + Send + Sync>> {
    let config = ExecutionConfig::default();
    assert!(matches!(
        repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid,
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
        ReadyMaterializationOutcome::Applied { .. }
    ));
    admit_and_start(repository, run_uid, task.task_id).await
}

async fn admit_and_start(
    repository: &ExecutionRepository,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
) -> Result<TaskAttemptFence, Box<dyn std::error::Error + Send + Sync>> {
    let config = ExecutionConfig::default();
    let admitted = repository
        .admit_ready_attempts(&config, 1, Utc::now())
        .await?
        .admitted
        .into_iter()
        .next()
        .expect("one canonical task attempt must be admitted");
    assert_eq!(admitted.run_uid, run_uid);
    assert_eq!(admitted.task_id, task_id);
    assert!(matches!(
        repository
            .start_task_attempt(TaskAttemptFence {
                tenant_id: admitted.tenant_id,
                run_uid: admitted.run_uid,
                task_id: admitted.task_id,
                controller_generation: admitted.controller_generation,
                attempt_generation: admitted.attempt_generation,
                dispatch_uid: admitted.dispatch_uid,
                capacity_reservation_uid: admitted.capacity_reservation_uid,
                watchdog_trigger_uid: admitted.watchdog_trigger_uid,
                attempt_deadline_at: admitted.attempt_deadline_at,
            })
            .await?,
        TaskAttemptStartOutcome::Started(_) | TaskAttemptStartOutcome::AlreadyStarted(_)
    ));
    Ok(TaskAttemptFence {
        tenant_id: admitted.tenant_id,
        run_uid: admitted.run_uid,
        task_id: admitted.task_id,
        controller_generation: admitted.controller_generation,
        attempt_generation: admitted.attempt_generation,
        dispatch_uid: admitted.dispatch_uid,
        capacity_reservation_uid: admitted.capacity_reservation_uid,
        watchdog_trigger_uid: admitted.watchdog_trigger_uid,
        attempt_deadline_at: admitted.attempt_deadline_at,
    })
}
