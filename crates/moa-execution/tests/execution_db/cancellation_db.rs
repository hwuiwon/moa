//! Cancellation, replay, and cancellation-race persistence contracts.

use super::support::*;

#[tokio::test]
async fn cancellation_preserves_preconfirmation_null_and_postqueue_timestamp_db() -> TestResult {
    // Pins: cancelling before confirmation never invents queue/start evidence,
    // while cancelling after queueing retains the one immutable queue timestamp.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };

    let awaiting = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "cancel-before-confirmation",
            ExecutionRunStatus::AwaitingConfirmation,
            budget(1),
        ),
    )
    .await?;
    let direct_insert = sqlx::query(
        r#"
        INSERT INTO moa.execution_run (
            run_uid, tenant_id, contact_id, session_id, owner_user_id,
            goal_contract, initial_plan, active_plan, initial_plan_hash, active_plan_hash,
            capability_catalog, authorization_envelope, pinned_instruction_skills,
            source_provenance, input, status,
            budget_max_cost_microusd, budget_max_tokens, budget_max_tasks,
            budget_max_tool_calls, budget_max_retrieved_bytes, budget_deadline_at,
            progress_total_tasks, idempotency_key, cancellation_reason,
            terminal_cause, terminal_satisfied_requirement_count,
            terminal_requirement_count, completed_at
        )
        SELECT
            $2, tenant_id, contact_id, session_id, owner_user_id,
            goal_contract, initial_plan, active_plan, initial_plan_hash, active_plan_hash,
            capability_catalog, authorization_envelope, pinned_instruction_skills,
            source_provenance, input, 'cancelled',
            budget_max_cost_microusd, budget_max_tokens, budget_max_tasks,
            budget_max_tool_calls, budget_max_retrieved_bytes, budget_deadline_at,
            progress_total_tasks, $3, 'direct terminal insert',
            '{"kind":"cancellation"}'::JSONB, 0,
            1, NOW()
        FROM moa.execution_run
        WHERE run_uid = $1
        "#,
    )
    .bind(awaiting.run_uid)
    .bind(Uuid::new_v4())
    .bind(format!("illegal-terminal-insert-{}", awaiting.run_uid))
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(
        direct_insert,
        "execution runs must start awaiting confirmation or queued",
    );
    let preconfirm_request = cancellation_request(
        &repository,
        scope,
        awaiting.run_uid,
        "cancel before confirmation".to_string(),
    )
    .await?;
    let CancellationOutcome::Cancelled(preconfirm) = repository
        .cancel_run(scope, awaiting.run_uid, preconfirm_request)
        .await?
    else {
        panic!("pre-confirm cancellation must commit");
    };
    assert_eq!(preconfirm.run.status, ExecutionRunStatus::Cancelled);
    assert!(preconfirm.run.queued_at.is_none());
    assert!(preconfirm.run.confirmed_at.is_none());
    assert!(preconfirm.run.confirmed_plan_hash.is_none());
    assert!(preconfirm.run.started_at.is_none());

    let replay_request = cancellation_request(
        &repository,
        scope,
        awaiting.run_uid,
        "cancel before confirmation".to_string(),
    )
    .await?;
    let CancellationOutcome::Replayed(replayed) = repository
        .cancel_run(scope, awaiting.run_uid, replay_request)
        .await?
    else {
        panic!("pre-confirm cancellation replay must recover the committed row");
    };
    assert!(replayed.run.queued_at.is_none());
    assert!(replayed.run.confirmed_at.is_none());
    assert!(replayed.run.confirmed_plan_hash.is_none());
    assert!(replayed.run.started_at.is_none());

    let invalid = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "invalid-preconfirm-start",
            ExecutionRunStatus::AwaitingConfirmation,
            budget(1),
        ),
    )
    .await?;
    let invalid_preconfirm_cancel = sqlx::query(
        "UPDATE moa.execution_run SET status = 'cancelled', started_at = NOW(), terminal_cause = '{\"kind\":\"cancellation\"}'::JSONB, terminal_satisfied_requirement_count = 0, terminal_requirement_count = 1 WHERE run_uid = $1",
    )
    .bind(invalid.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(invalid_preconfirm_cancel, "execution_run_queued_at");

    let queued = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "cancel-after-queue",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;
    let queued_at = queued
        .queued_at
        .expect("direct queued run must have a queue timestamp");
    let postqueue_request = cancellation_request(
        &repository,
        scope,
        queued.run_uid,
        "cancel after queue".to_string(),
    )
    .await?;
    let CancellationOutcome::Cancelled(postqueue) = repository
        .cancel_run(scope, queued.run_uid, postqueue_request)
        .await?
    else {
        panic!("post-queue cancellation must commit");
    };
    assert_eq!(postqueue.run.queued_at, Some(queued_at));
    Ok(())
}

#[tokio::test]
async fn cancellation_counts_only_completed_task_requirement_evidence_db() -> TestResult {
    // Pins: cancellation coverage ignores pending, reserved, running, waiting,
    // failed, and merely declared requirement mappings.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut new = new_run(
        tenant_id,
        None,
        "cancellation-completed-coverage",
        ExecutionRunStatus::Queued,
        budget(2),
    );
    new.goal.requirements = vec![
        ExecutionRequirement {
            id: "req-completed".to_string(),
            description: "completed evidence".to_string(),
        },
        ExecutionRequirement {
            id: "req-pending".to_string(),
            description: "pending declaration".to_string(),
        },
    ];
    new.plan.definition.nodes = vec![
        output_node("completed", "req-completed"),
        output_node("pending", "req-pending"),
    ];
    let run = create_run(&repository, scope, new).await?;
    let mut completed_task = logical_task(run.run_uid, "completed", "", estimate(1));
    completed_task.requirement_ids = vec!["req-completed".to_string()];
    let mut pending_task = logical_task(run.run_uid, "pending", "", estimate(1));
    pending_task.requirement_ids = vec!["req-pending".to_string()];
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![completed_task.clone(), pending_task],
        )
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, completed_task.task_id).await?;
    repository
        .record_task_outcome(scope, run.run_uid, completed_task.task_id, 1, completed(1))
        .await?;

    let request = cancellation_request(
        &repository,
        scope,
        run.run_uid,
        "stop remaining".to_string(),
    )
    .await?;
    assert_eq!(request.terminal_evidence.satisfied_requirement_count, 1);
    assert_eq!(request.terminal_evidence.requirement_count, 2);
    let CancellationOutcome::Cancelled(cancelled) =
        repository.cancel_run(scope, run.run_uid, request).await?
    else {
        panic!("cancellation must commit");
    };
    assert_eq!(
        cancelled
            .run
            .terminal_evidence
            .expect("cancelled run has evidence")
            .satisfied_requirement_count,
        1
    );
    Ok(())
}

#[tokio::test]
async fn run_cancellation_replaces_every_active_outcome_with_typed_evidence_and_replays_db()
-> TestResult {
    // Pins: cancellation cannot leave a nonterminal status paired with stale
    // NeedsInput/NeedsReplan evidence; every active task is atomically replaced
    // by one typed Cancelled outcome while prior audit history stays append-only.
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
            "cancel-all-active-outcomes",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let pending = logical_task(run.run_uid, "pending", "", estimate(1));
    let running = logical_task(run.run_uid, "running", "", estimate(2));
    let waiting_input = logical_task(run.run_uid, "input", "", estimate(3));
    let waiting_replan = logical_task(run.run_uid, "replan", "", estimate(4));
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![
                pending.clone(),
                running.clone(),
                waiting_input.clone(),
                waiting_replan.clone(),
            ],
        )
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, running.task_id).await?;
    reserve_and_start(&repository, scope, run.run_uid, waiting_input.task_id).await?;
    reserve_and_start(&repository, scope, run.run_uid, waiting_replan.task_id).await?;
    repository
        .record_task_outcome(scope, run.run_uid, waiting_input.task_id, 1, needs_input(1))
        .await?;
    assert!(matches!(
        repository
            .transition_run_wait(
                scope,
                run.run_uid,
                ExecutionRunStatus::WaitingInput,
                ExecutionRunStatus::Running,
            )
            .await?,
        TransitionOutcome::RunApplied(_)
    ));
    repository
        .record_task_outcome(
            scope,
            run.run_uid,
            waiting_replan.task_id,
            1,
            needs_replan(2),
        )
        .await?;

    let reason = "operator cancelled run".to_string();
    let request = cancellation_request(&repository, scope, run.run_uid, reason.clone()).await?;
    let CancellationOutcome::Cancelled(cancelled) =
        repository.cancel_run(scope, run.run_uid, request).await?
    else {
        panic!("first cancellation must commit its durable handoff");
    };
    let mut expected_task_ids = vec![
        pending.task_id,
        running.task_id,
        waiting_input.task_id,
        waiting_replan.task_id,
    ];
    expected_task_ids.sort();
    assert_eq!(cancelled.task_ids_to_release, expected_task_ids);
    let cancelled_wake_epoch = cancelled.run.wake_epoch;
    let first_page = repository
        .list_tasks(scope, run.run_uid, ExecutionTaskPageRequest::default())
        .await?;
    assert_eq!(first_page.tasks.len(), 4);
    for task in &first_page.tasks {
        assert_eq!(
            task.status,
            ExecutionTaskStatus::Cancelled,
            "{}",
            task.node_id
        );
        assert_eq!(
            task.reserved,
            ExecutionEstimate::default(),
            "{}",
            task.node_id
        );
        assert_eq!(task.actual_tasks, 1, "{}", task.node_id);
        assert!(
            matches!(
                task.current_outcome.as_ref().map(|outcome| &outcome.result),
                Some(ExecutionTaskResult::Cancelled { reason: actual }) if actual == &reason
            ),
            "{} retained a stale current outcome",
            task.node_id
        );
        assert_eq!(
            task.outcome_audit.last().map(|entry| &entry["kind"]),
            Some(&json!("run_cancelled"))
        );
    }
    let cancelled_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("cancelled run remains visible");
    assert_eq!(cancelled_run.status, ExecutionRunStatus::Cancelled);
    assert_eq!(cancelled_run.reserved, ExecutionEstimate::default());
    assert_eq!(cancelled_run.consumed.tasks, 4);
    assert_eq!(cancelled_run.progress_cancelled_tasks, 4);

    let replay_request = cancellation_request(&repository, scope, run.run_uid, reason).await?;
    let CancellationOutcome::Replayed(replayed) = repository
        .cancel_run(scope, run.run_uid, replay_request)
        .await?
    else {
        panic!("exact cancellation replay must recover its durable handoff");
    };
    assert_eq!(replayed.run.wake_epoch, cancelled_wake_epoch);
    assert_eq!(replayed.task_ids_to_release, expected_task_ids);
    let conflicting_request = cancellation_request(
        &repository,
        scope,
        run.run_uid,
        "different reason".to_string(),
    )
    .await?;
    assert_eq!(
        repository
            .cancel_run(scope, run.run_uid, conflicting_request)
            .await?,
        CancellationOutcome::Conflict
    );
    assert_eq!(
        repository
            .list_tasks(scope, run.run_uid, ExecutionTaskPageRequest::default())
            .await?
            .tasks,
        first_page.tasks
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_race_releases_reservations_and_preserves_completed_results_db() -> TestResult
{
    // Pins: reserve/cancel races serialize on the run, block later work, and retain prior outputs.
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
            "cancel-race",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let completed_task = logical_task(run.run_uid, "done", "", estimate(1));
    let racing_task = logical_task(run.run_uid, "racing", "", estimate(1));
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![completed_task.clone(), racing_task.clone()],
        )
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, completed_task.task_id).await?;
    repository
        .record_task_outcome(scope, run.run_uid, completed_task.task_id, 1, completed(1))
        .await?;

    let barrier = Arc::new(Barrier::new(3));
    let reserve_repo = repository.clone();
    let reserve_barrier = Arc::clone(&barrier);
    let reserve = tokio::spawn(async move {
        reserve_barrier.wait().await;
        reserve_repo
            .reserve_task(scope, run.run_uid, racing_task.task_id, 1)
            .await
    });
    let cancel_repo = repository.clone();
    let cancel_barrier = Arc::clone(&barrier);
    let cancel = tokio::spawn(async move {
        cancel_barrier.wait().await;
        let request = cancellation_request(
            &cancel_repo,
            scope,
            run.run_uid,
            "user requested".to_string(),
        )
        .await?;
        cancel_repo.cancel_run(scope, run.run_uid, request).await
    });
    barrier.wait().await;
    let reserve_outcome = reserve.await??;
    assert!(matches!(
        reserve_outcome,
        ReservationOutcome::Reserved(_)
            | ReservationOutcome::Rejected(ReservationRejection::InvalidTaskStatus)
    ));
    assert!(matches!(cancel.await??, CancellationOutcome::Cancelled(_)));

    let run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("cancelled run visible");
    assert_eq!(run.status, ExecutionRunStatus::Cancelled);
    assert_eq!(run.reserved, ExecutionEstimate::default());
    assert_eq!(run.cancellation_reason.as_deref(), Some("user requested"));
    let page = repository
        .list_tasks(scope, run.run_uid, ExecutionTaskPageRequest::default())
        .await?;
    let done = page
        .tasks
        .iter()
        .find(|task| task.task_id == completed_task.task_id)
        .expect("completed task retained");
    assert_eq!(done.status, ExecutionTaskStatus::Completed);
    assert_eq!(done.output, Some(json!({ "tokens": 1 })));
    let racing = page
        .tasks
        .iter()
        .find(|task| task.task_id == racing_task.task_id)
        .expect("racing task retained");
    assert_eq!(racing.status, ExecutionTaskStatus::Cancelled);
    assert!(matches!(
        repository
            .reserve_task(scope, run.run_uid, racing_task.task_id, 1)
            .await?,
        ReservationOutcome::Rejected(ReservationRejection::InvalidTaskStatus)
    ));
    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, racing_task.task_id, 1, completed(1))
            .await?,
        TaskOutcomeWrite::Rejected {
            reason: TaskOutcomeRejection::TerminalRun,
            ..
        }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_racing_outcome_write_has_one_consistent_winner_db() -> TestResult {
    // Pins: cancellation and a current-generation outcome serialize into one terminal task projection plus one audit entry.
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
            "cancel-outcome-race",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let task = logical_task(run.run_uid, "race", "", estimate(1));
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;

    let barrier = Arc::new(Barrier::new(3));
    let outcome_repository = repository.clone();
    let outcome_barrier = Arc::clone(&barrier);
    let outcome_write = tokio::spawn(async move {
        outcome_barrier.wait().await;
        outcome_repository
            .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
            .await
    });
    let cancel_repository = repository.clone();
    let cancel_barrier = Arc::clone(&barrier);
    let cancellation = tokio::spawn(async move {
        cancel_barrier.wait().await;
        let request = cancellation_request(
            &cancel_repository,
            scope,
            run.run_uid,
            "raced outcome".to_string(),
        )
        .await?;
        cancel_repository
            .cancel_run(scope, run.run_uid, request)
            .await
    });
    barrier.wait().await;

    let outcome_write = outcome_write.await??;
    assert!(matches!(
        cancellation.await??,
        CancellationOutcome::Cancelled(_)
    ));
    let cancelled_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("cancelled run remains visible");
    assert_eq!(cancelled_run.status, ExecutionRunStatus::Cancelled);
    assert_eq!(cancelled_run.reserved, ExecutionEstimate::default());
    assert_eq!(cancelled_run.consumed.tasks, 1);

    let page = repository
        .list_tasks(scope, run.run_uid, ExecutionTaskPageRequest::default())
        .await?;
    assert_eq!(page.tasks.len(), 1);
    let persisted_task = &page.tasks[0];
    match outcome_write {
        TaskOutcomeWrite::Applied { .. } => {
            assert_eq!(persisted_task.status, ExecutionTaskStatus::Completed);
            assert_eq!(persisted_task.output, Some(json!({ "tokens": 1 })));
            assert_eq!(persisted_task.outcome_audit.len(), 1);
            assert_eq!(persisted_task.outcome_audit[0]["accepted"], true);
            assert_eq!(cancelled_run.progress_completed_tasks, 1);
            assert_eq!(cancelled_run.progress_cancelled_tasks, 0);
        }
        TaskOutcomeWrite::Rejected {
            reason: TaskOutcomeRejection::TerminalRun,
            ..
        } => {
            assert_eq!(persisted_task.status, ExecutionTaskStatus::Cancelled);
            assert!(persisted_task.output.is_none());
            assert_eq!(persisted_task.outcome_audit.len(), 2);
            assert_eq!(persisted_task.outcome_audit[0]["kind"], "run_cancelled");
            assert_eq!(persisted_task.outcome_audit[0]["accepted"], true);
            assert_eq!(persisted_task.outcome_audit[1]["accepted"], false);
            assert_eq!(persisted_task.outcome_audit[1]["rejection"], "terminal_run");
            assert_eq!(cancelled_run.progress_completed_tasks, 0);
            assert_eq!(cancelled_run.progress_cancelled_tasks, 1);
        }
        other => panic!("outcome/cancellation race produced an invalid result: {other:?}"),
    }
    Ok(())
}
