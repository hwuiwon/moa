//! Task-outcome, external-wait, review, and replanning persistence contracts.

use super::support::*;

#[tokio::test]
async fn retry_and_input_resume_terminalize_elapsed_or_exhausted_run_envelope_db() -> TestResult {
    // Pins: redispatch rejected by deadline or budget atomically records the
    // typed terminal failure, releases reservations, wakes finalization, and
    // remains idempotent without weakening the generation fence.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };

    for (kind, waiting_outcome) in [
        ("input", needs_input(0)),
        (
            "retry",
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: usage(0),
                result: ExecutionTaskResult::Failed {
                    class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
                    message: "retry later".to_string(),
                },
            },
        ),
    ] {
        let run = create_run(
            &repository,
            scope,
            new_run(
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
            ),
        )
        .await?;
        let task = logical_task(run.run_uid, kind, "deadline", estimate(1));
        repository
            .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
            .await?;
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
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
        let transition = if kind == "input" {
            repository
                .resume_task_with_input(scope, run.run_uid, task.task_id, 1, json!({"ok": true}))
                .await?
        } else {
            repository
                .retry_task(scope, run.run_uid, task.task_id, 1)
                .await?
        };
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
        assert_eq!(terminal_run.wake_epoch, before_terminal.wake_epoch + 1);
        let replay = if kind == "input" {
            repository
                .resume_task_with_input(scope, run.run_uid, task.task_id, 1, json!({"ok": true}))
                .await?
        } else {
            repository
                .retry_task(scope, run.run_uid, task.task_id, 1)
                .await?
        };
        assert_eq!(
            replay,
            TransitionOutcome::AlreadyApplied(terminal_task.clone())
        );
        let stale = if kind == "input" {
            repository
                .resume_task_with_input(scope, run.run_uid, task.task_id, 0, json!({"ok": true}))
                .await?
        } else {
            repository
                .retry_task(scope, run.run_uid, task.task_id, 0)
                .await?
        };
        assert_eq!(
            stale,
            TransitionOutcome::Rejected(
                moa_execution::repository::TransitionRejection::GenerationMismatch
            )
        );
    }

    for (kind, waiting_outcome) in [("input", needs_input(1)), ("retry", retryable(1))] {
        let run = create_run(
            &repository,
            scope,
            new_run(
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
            ),
        )
        .await?;
        let task = logical_task(run.run_uid, kind, "budget", estimate(1));
        repository
            .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
            .await?;
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
        assert!(matches!(
            repository
                .record_task_outcome(scope, run.run_uid, task.task_id, 1, waiting_outcome,)
                .await?,
            TaskOutcomeWrite::Applied { .. }
        ));
        let before_terminal = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("waiting run should remain queryable");
        let transition = if kind == "input" {
            repository
                .resume_task_with_input(scope, run.run_uid, task.task_id, 1, json!({"ok": true}))
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
        assert_eq!(terminal_run.wake_epoch, before_terminal.wake_epoch + 1);
        let replay = if kind == "input" {
            repository
                .resume_task_with_input(scope, run.run_uid, task.task_id, 1, json!({"ok": true}))
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
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "outcomes",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let task = logical_task(run.run_uid, "outcome", "", estimate(10));
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, task.task_id, 1, needs_input(1))
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    let TransitionOutcome::Applied(resumed) = repository
        .resume_task_with_input(
            scope,
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
            .record_task_outcome(scope, run.run_uid, task.task_id, 2, completed(2))
            .await?,
        TaskOutcomeWrite::Applied { .. }
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
            ExecutionRunStatus::WaitingInput,
            ExecutionTaskStatus::Failed,
            1,
        ),
    ];

    for (key, waiting_status, outcome, expected_run_status, expected_task_status, failed_tasks) in
        cases
    {
        let run = create_run(
            &repository,
            scope,
            new_run(tenant_id, None, key, ExecutionRunStatus::Queued, budget(1)),
        )
        .await?;
        assert!(matches!(
            repository
                .transition_run_wait(
                    scope,
                    run.run_uid,
                    ExecutionRunStatus::Queued,
                    ExecutionRunStatus::Running,
                )
                .await?,
            TransitionOutcome::RunApplied(_)
        ));
        let task = logical_task(run.run_uid, "outcome", key, estimate(1));
        repository
            .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
            .await?;
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
        assert!(matches!(
            repository
                .transition_run_wait(
                    scope,
                    run.run_uid,
                    ExecutionRunStatus::Running,
                    waiting_status,
                )
                .await?,
            TransitionOutcome::RunApplied(_)
        ));

        let TaskOutcomeWrite::Applied {
            run: persisted_run,
            task: persisted_task,
            ..
        } = repository
            .record_task_outcome(scope, run.run_uid, task.task_id, task.generation, outcome)
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

#[tokio::test]
async fn action_review_resolution_is_review_uid_idempotent_and_generation_fenced_db() -> TestResult
{
    // Pins: outbox replay applies one review UID once, while stale generations
    // remain auditable without resolving or mutating the current task projection;
    // a reused identity cannot smuggle in different typed resolution semantics.
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
            "review-resolution",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let task = logical_task(run.run_uid, "review", "", estimate(1));
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    let stale_review = Uuid::new_v4();
    let current_review = Uuid::new_v4();
    let resolution = ExecutionActionReviewResolution::Denied {
        reason: "operator denied".to_string(),
    };

    assert_eq!(
        repository
            .record_action_review_resolution(
                scope,
                run.run_uid,
                task.task_id,
                2,
                stale_review,
                &resolution,
            )
            .await?,
        ActionReviewResolutionWrite::AuditedStale
    );
    assert_eq!(
        repository
            .record_action_review_resolution(
                scope,
                run.run_uid,
                task.task_id,
                1,
                current_review,
                &resolution,
            )
            .await?,
        ActionReviewResolutionWrite::Applied
    );
    assert_eq!(
        repository
            .record_action_review_resolution(
                scope,
                run.run_uid,
                task.task_id,
                1,
                current_review,
                &resolution,
            )
            .await?,
        ActionReviewResolutionWrite::Replayed
    );
    let conflicting_resolution = ExecutionActionReviewResolution::Completed {
        tool_output: json!({"unexpected": true}),
    };
    let conflict = repository
        .record_action_review_resolution(
            scope,
            run.run_uid,
            task.task_id,
            1,
            current_review,
            &conflicting_resolution,
        )
        .await
        .expect_err("same task review identity with a different resolution must fail closed");
    assert!(
        matches!(conflict, moa_execution::Error::InvalidRepositoryData { .. }),
        "task review identity conflict returned the wrong error: {conflict:?}"
    );
    let persisted = repository
        .load_task(scope, run.run_uid, task.task_id)
        .await?
        .expect("task should remain visible");
    assert_eq!(persisted.status, ExecutionTaskStatus::Running);
    assert_eq!(persisted.generation, 1);
    assert_eq!(persisted.outcome_audit.len(), 2);
    assert_eq!(persisted.outcome_audit[0]["accepted"], false);
    assert_eq!(persisted.outcome_audit[1]["accepted"], true);
    Ok(())
}
