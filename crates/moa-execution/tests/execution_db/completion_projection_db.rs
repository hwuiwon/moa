//! Bounded persisted completion projection and terminal-evidence contracts.

use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionNode, ExecutionOperation,
};
use moa_execution::{
    capability::node_output_hash,
    repository::{
        completion::{CompletionAdvanceOutcome, CompletionAdvanceRequest},
        ready::{ReadyMaterializationOutcome, ReadyMaterializationRequest},
        replan_stop::{NewExecutionReplanStopIntent, ReplanStopIntentWriteOutcome},
        task::{
            TaskAttemptFence, TaskAttemptReleaseClaimOutcome, TaskAttemptSettlementOutcome,
            TaskAttemptStartOutcome,
        },
        terminal::{PendingTerminalAdvanceOutcome, PendingTerminalAdvanceStage},
    },
    state::{ExecutionLimitStop, ExecutionTerminalEvidence},
};

use super::support::*;

fn output_node() -> ExecutionNode {
    output_node_with_dependencies("output", &[])
}

fn output_node_with_dependencies(id: &str, depends_on: &[&str]) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["req".to_string()],
        depends_on: depends_on
            .iter()
            .map(|dependency| (*dependency).to_string())
            .collect(),
        when: None,
        input: json!({}),
        output_schema: json!({ "type": "object" }),
        operation: ExecutionOperation::Output { value: json!({}) },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        budget: None,
    }
}

#[tokio::test]
async fn failed_and_unknown_outcome_tasks_cancel_transitive_unmaterialized_dependents_db()
-> TestResult {
    // Pins: both Failed and UnknownOutcome source tasks terminalize every transitive dependent
    // that never materialized, allowing the bounded completion projection to reach terminal intent.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();

    for (case, outcome, expected_task_status) in [
        (
            "failed",
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: usage(1),
                result: ExecutionTaskResult::Failed {
                    class: ExecutionFailureClass::Terminal,
                    message: "source failed".to_string(),
                },
            },
            ExecutionTaskStatus::Failed,
        ),
        (
            "unknown-outcome",
            ExecutionTaskOutcome {
                schema_version: 1,
                usage: usage(1),
                result: ExecutionTaskResult::UnknownOutcome {
                    message: "source outcome is unknowable".to_string(),
                },
            },
            ExecutionTaskStatus::UnknownOutcome,
        ),
    ] {
        let mut candidate = new_run(
            tenant_id,
            None,
            &format!("transitive-terminal-{case}"),
            ExecutionRunStatus::Queued,
            budget(3),
        );
        candidate.plan.definition.nodes = vec![
            output_node_with_dependencies("source", &[]),
            output_node_with_dependencies("middle", &["source"]),
            output_node_with_dependencies("leaf", &["middle"]),
        ];
        candidate.plan.estimate.tasks = 3;
        let run = create_run(&repository, scope, candidate).await?;
        let source = logical_task(run.run_uid, "source", case, estimate(1));
        assert!(
            repository
                .initialize_scheduler_state(scope, run.run_uid)
                .await?
        );
        assert!(matches!(
            repository
                .materialize_ready_page(
                    scope,
                    &config,
                    ReadyMaterializationRequest {
                        run_uid: run.run_uid,
                        plan_revision: 1,
                        node_id: "source".to_string(),
                        expected_cursor: 0,
                        reduce_cursor: None,
                        source_exhausted: true,
                        terminal_output: None,
                        condition_skipped: false,
                        tasks: vec![source],
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
            .expect("one terminal-source task must be admitted");
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
        let TaskAttemptStartOutcome::Started(started) =
            repository.start_task_attempt(fence).await?
        else {
            panic!("{case} source attempt must start");
        };
        let settled_at = Utc::now();
        assert!(matches!(
            repository
                .begin_task_attempt_release(
                    fence,
                    started.task.generation,
                    "terminal_source",
                    settled_at,
                )
                .await?,
            TaskAttemptReleaseClaimOutcome::Applied(_)
        ));
        let TaskAttemptSettlementOutcome::Applied { task, .. } = repository
            .settle_released_task_attempt(&config, fence, outcome, None, settled_at, None)
            .await?
        else {
            panic!("{case} source attempt must settle");
        };
        assert_eq!(task.status, expected_task_status, "{case}");
        let nodes: Vec<(String, String, i64, i64, bool, bool)> = sqlx::query_as(
            "SELECT node_id,node_status,total_task_count,remaining_dependency_count, \
                    materialization_complete,aggregate_complete \
             FROM moa.execution_node_state WHERE run_uid=$1 ORDER BY node_order",
        )
        .bind(run.run_uid)
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            nodes,
            vec![
                (
                    "source".to_string(),
                    "failed".to_string(),
                    1,
                    0,
                    true,
                    false
                ),
                (
                    "middle".to_string(),
                    "cancelled".to_string(),
                    0,
                    0,
                    true,
                    true
                ),
                (
                    "leaf".to_string(),
                    "cancelled".to_string(),
                    0,
                    0,
                    true,
                    true
                ),
            ],
            "{case} must close every never-materialized transitive dependent"
        );

        let current = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("terminal-source run remains visible");
        let request = CompletionAdvanceRequest {
            run_uid: run.run_uid,
            controller_generation: current.controller_generation,
            wake_epoch: current.wake_epoch,
            page_size: 10,
            now: Utc::now(),
        };
        let mut pages = Vec::new();
        loop {
            match repository
                .advance_completion_projection(scope, &config, request)
                .await?
            {
                CompletionAdvanceOutcome::Continue {
                    scanned_tasks,
                    scanned_nodes,
                } => pages.push((scanned_tasks, scanned_nodes)),
                CompletionAdvanceOutcome::NonSuccessTerminal { pending_terminal } => {
                    assert_eq!(
                        pending_terminal.status,
                        ExecutionRunStatus::Failed,
                        "{case}"
                    );
                    break;
                }
                other => panic!("{case} completion projection did not advance: {other:?}"),
            }
            assert!(
                pages.len() <= 2,
                "{case} completion scan must remain bounded"
            );
        }
        assert_eq!(pages, vec![(1, 0), (0, 3)], "{case}");
    }
    Ok(())
}

#[tokio::test]
async fn completion_projection_pages_twenty_five_hundred_tasks_and_nodes_db() -> TestResult {
    // Pins: terminal evaluation advances an exact task cursor and a separate node cursor; the
    // finalization activation never reloads the full task or node projection.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "bounded-completion-2501",
        ExecutionRunStatus::Queued,
        budget(5_000),
    );
    candidate.plan.definition.nodes = vec![output_node()];
    let run = create_run(&repository, scope, candidate).await?;
    let config = ExecutionConfig::default();

    let all_tasks = (0_u64..2_501)
        .map(|index| logical_task(run.run_uid, "output", &format!("{index:04}"), estimate(1)))
        .collect::<Vec<_>>();
    let mut cursor = 0_u64;
    for (page_index, page) in all_tasks.chunks(1_000).enumerate() {
        let source_exhausted = page_index == 2;
        let ReadyMaterializationOutcome::Applied { next_cursor, .. } = repository
            .materialize_ready_page(
                scope,
                &config,
                ReadyMaterializationRequest {
                    run_uid: run.run_uid,
                    plan_revision: run.plan_revision,
                    node_id: "output".to_string(),
                    expected_cursor: cursor,
                    reduce_cursor: None,
                    source_exhausted,
                    terminal_output: None,
                    condition_skipped: false,
                    tasks: page.to_vec(),
                },
            )
            .await?
        else {
            panic!("fresh completion setup page must apply");
        };
        cursor = next_cursor;
    }
    for (status, attempt_state) in [
        ("dispatching", "dispatching"),
        ("running", "running"),
        ("completed", "terminal"),
    ] {
        sqlx::query(
            "UPDATE moa.execution_task SET status=$2,attempt_state=$3, \
                 attempt_started_at=CASE WHEN $2='running' THEN NOW() ELSE attempt_started_at END, \
                 output=CASE WHEN $2='completed' THEN '{}'::JSONB ELSE output END, \
                 completed_at=CASE WHEN $2='completed' THEN NOW() ELSE completed_at END, \
                 updated_at=NOW() WHERE run_uid=$1",
        )
        .bind(run.run_uid)
        .bind(status)
        .bind(attempt_state)
        .execute(&pool)
        .await?;
    }
    let output = json!({});
    sqlx::query(
        "UPDATE moa.execution_node_state SET node_status='completed', \
             ready_task_count=0,terminal_task_count=total_task_count, \
             succeeded_task_count=total_task_count,aggregate_output=$2,aggregate_output_hash=$3, \
             updated_at=NOW() WHERE run_uid=$1 AND node_id='output'",
    )
    .bind(run.run_uid)
    .bind(&output)
    .bind(node_output_hash(&output)?.to_string())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET status='running',ready_task_count=0,active_task_count=0, \
             updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("completion run remains visible");
    let RunControllerClaimOutcome::Claimed(mut current) = repository
        .claim_controller_wake(
            scope,
            current.run_uid,
            current.controller_generation,
            current.wake_epoch,
        )
        .await?
    else {
        panic!("initial completion wake must be claimable");
    };

    let mut pages = Vec::new();
    let mut source_progress_at = None;
    loop {
        match repository
            .advance_completion_projection(
                scope,
                &config,
                CompletionAdvanceRequest {
                    run_uid: run.run_uid,
                    controller_generation: current.controller_generation,
                    wake_epoch: current.wake_epoch,
                    page_size: 1_000,
                    now: Utc::now(),
                },
            )
            .await?
        {
            CompletionAdvanceOutcome::Continue {
                scanned_tasks,
                scanned_nodes,
            } => {
                pages.push((scanned_tasks, scanned_nodes));
                let persisted_source = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
                    "SELECT source_progress_at FROM moa.execution_completion_scan WHERE run_uid=$1",
                )
                .bind(run.run_uid)
                .fetch_one(&pool)
                .await?;
                if let Some(expected) = source_progress_at {
                    assert_eq!(persisted_source, expected);
                } else {
                    source_progress_at = Some(persisted_source);
                }
                let RunControllerCompletionOutcome::Applied {
                    run: continued,
                    continuation: Some(continuation),
                } = repository
                    .complete_controller_wake(
                        scope,
                        &config,
                        run.run_uid,
                        RunControllerCompletionRequest {
                            controller_generation: current.controller_generation,
                            wake_epoch: current.wake_epoch,
                            checkpoint: ExecutionRunActivationCheckpoint {
                                status: current.status,
                                activation_state: ExecutionActivationState::Idle,
                                next_wake_at: current.next_wake_at,
                                waiting_since: current.waiting_since,
                                ready_task_count: current.ready_task_count,
                                active_task_count: current.active_task_count,
                            },
                            continuation_payload: Some(json!({
                                "reason": "completion_projection_test_continue"
                            })),
                            continuation_not_before_at: Utc::now(),
                        },
                    )
                    .await?
                else {
                    panic!("completion page must atomically enqueue one continuation");
                };
                assert_eq!(continuation.wake_epoch, Some(continued.wake_epoch));
                let RunControllerClaimOutcome::Claimed(claimed) = repository
                    .claim_controller_wake(
                        scope,
                        continued.run_uid,
                        continued.controller_generation,
                        continued.wake_epoch,
                    )
                    .await?
                else {
                    panic!("completion continuation must be claimable");
                };
                assert_eq!(claimed.last_progress_at, source_progress_at.unwrap());
                current = claimed;
            }
            CompletionAdvanceOutcome::FinalizationReady(request) => {
                assert_eq!(request.run_uid, run.run_uid);
                break;
            }
            other => panic!("unexpected completion outcome: {other:?}"),
        }
        assert!(
            pages.len() <= 4,
            "completion cursor failed to make progress"
        );
    }
    assert_eq!(pages, vec![(1_000, 0), (1_000, 0), (501, 0), (0, 1)]);
    let scan = sqlx::query_as::<_, (i64, bool, bool, chrono::DateTime<Utc>)>(
        "SELECT scanned_task_count,scan_complete,node_scan_complete,source_progress_at \
         FROM moa.execution_completion_scan WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(scan, (2_501, true, true, source_progress_at.unwrap()));
    Ok(())
}

#[tokio::test]
async fn replan_stop_completion_pages_rebind_exact_wake_without_duplicate_verifiers_db()
-> TestResult {
    // Pins: every bounded ReplanStop page, old-wake ACK, single continuation, and intent-wake
    // rebind commit atomically; the excluded WaitingReplan origin becomes blocked evidence and
    // never causes an unbounded scan or verifier materialization.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut candidate = new_run(
        tenant_id,
        None,
        "bounded-replan-stop",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    candidate.plan.definition.nodes = vec![output_node()];
    candidate.goal.completion_checks = vec![CompletionCheck {
        id: "semantic".to_string(),
        description: "Verifier must not be materialized after ReplanStop".to_string(),
        requirement_ids: Vec::new(),
        constraint_ids: Vec::new(),
        kind: CompletionCheckKind::AgentVerifier {
            instructions: "verify terminal output".to_string(),
            max_turns: 1,
        },
    }];
    let run = create_run(&repository, scope, candidate).await?;
    let config = ExecutionConfig::default();
    let tasks = (0_u64..3)
        .map(|index| logical_task(run.run_uid, "output", &format!("{index:04}"), estimate(1)))
        .collect::<Vec<_>>();
    let origin = tasks[2].task_id;
    let ReadyMaterializationOutcome::Applied { .. } = repository
        .materialize_ready_page(
            scope,
            &config,
            ReadyMaterializationRequest {
                run_uid: run.run_uid,
                plan_revision: run.plan_revision,
                node_id: "output".to_string(),
                expected_cursor: 0,
                reduce_cursor: None,
                source_exhausted: true,
                terminal_output: None,
                condition_skipped: false,
                tasks,
            },
        )
        .await?
    else {
        panic!("fresh ReplanStop setup page must apply");
    };
    let failure = ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(0),
        result: ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Terminal,
            message: "source unavailable".to_string(),
        },
    };
    sqlx::query(
        "UPDATE moa.execution_task SET status='failed',attempt_state='terminal', \
             current_outcome=$2,completed_at=NOW(),updated_at=NOW() \
         WHERE run_uid=$1 AND task_id<>$3",
    )
    .bind(run.run_uid)
    .bind(serde_json::to_value(failure)?)
    .bind(origin.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_task SET status='waiting_replan',attempt_state='waiting', \
             current_outcome=$2,waiting_since=NOW(),updated_at=NOW() \
         WHERE run_uid=$1 AND task_id=$3",
    )
    .bind(run.run_uid)
    .bind(serde_json::to_value(needs_replan(1))?)
    .bind(origin.as_uuid())
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_node_state SET node_status='waiting',ready_task_count=0, \
             waiting_task_count=1,terminal_task_count=2,failed_task_count=2,updated_at=NOW() \
         WHERE run_uid=$1 AND node_id='output'",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    sqlx::query(
        "UPDATE moa.execution_run SET status='waiting_replan',ready_task_count=0, \
             active_task_count=0,waiting_task_count=1,waiting_replan_task_count=1, \
             waiting_reasons_truncated=TRUE,waiting_since=NOW(),last_progress_at=NOW(), \
             updated_at=NOW() WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    let amendment_hash: ExecutionHash = "a".repeat(64).parse()?;
    let ReplanStopIntentWriteOutcome::Applied(queued) = repository
        .request_replan_stop(
            scope,
            &ExecutionConfig::default(),
            NewExecutionReplanStopIntent {
                run_uid: run.run_uid,
                session_id: run.session_id,
                base_plan_revision: run.plan_revision,
                origin_task_id: origin,
                task_generation: 1,
                amendment_hash,
                stop_reason: ReplanStopReason::RepeatedFailure,
                detail: Some("same failure exhausted replan policy".to_string()),
            },
        )
        .await?
    else {
        panic!("fresh ReplanStop intent must persist with one activation");
    };
    let mut wake_epoch = queued.wake_epoch;
    let mut source_progress_at = None;
    let mut page_count = 0_u32;
    loop {
        let RunControllerClaimOutcome::Claimed(claimed) = repository
            .claim_controller_wake(scope, run.run_uid, run.controller_generation, wake_epoch)
            .await?
        else {
            panic!("exact rebound ReplanStop wake must be claimable");
        };
        let intent = repository
            .load_replan_stop_intent(scope, run.run_uid, run.controller_generation, wake_epoch)
            .await?
            .expect("intent must follow its exact current wake");
        sqlx::query("UPDATE moa.execution_run SET activation_failure_count=5 WHERE run_uid=$1")
            .bind(run.run_uid)
            .execute(&pool)
            .await?;
        match repository
            .advance_replan_stop_completion_projection(
                scope,
                &config,
                CompletionAdvanceRequest {
                    run_uid: run.run_uid,
                    controller_generation: run.controller_generation,
                    wake_epoch,
                    page_size: 1,
                    now: Utc::now(),
                },
                &intent,
            )
            .await?
        {
            CompletionAdvanceOutcome::ReplanStopContinue {
                scanned_tasks,
                scanned_nodes,
                continuation,
            } => {
                assert_eq!(u64::from(scanned_tasks) + u64::from(scanned_nodes), 1);
                let next_wake = continuation
                    .wake_epoch
                    .expect("ReplanStop continuation has an exact wake");
                assert!(next_wake > wake_epoch);
                let failure_count: i64 = sqlx::query_scalar(
                    "SELECT activation_failure_count FROM moa.execution_run WHERE run_uid=$1",
                )
                .bind(run.run_uid)
                .fetch_one(&pool)
                .await?;
                assert_eq!(
                    failure_count, 0,
                    "each successful ReplanStop page acknowledgement resets crash recovery"
                );
                let persisted_source = sqlx::query_scalar::<_, chrono::DateTime<Utc>>(
                    "SELECT source_progress_at FROM moa.execution_completion_scan WHERE run_uid=$1",
                )
                .bind(run.run_uid)
                .fetch_one(&pool)
                .await?;
                if let Some(expected) = source_progress_at {
                    assert_eq!(persisted_source, expected);
                } else {
                    source_progress_at = Some(persisted_source);
                }
                assert_eq!(claimed.last_progress_at, persisted_source);
                assert!(
                    repository
                        .load_replan_stop_intent(
                            scope,
                            run.run_uid,
                            run.controller_generation,
                            wake_epoch,
                        )
                        .await?
                        .is_none(),
                    "old wake must stop owning the intent after commit"
                );
                wake_epoch = next_wake;
                page_count += 1;
            }
            CompletionAdvanceOutcome::ReplanStopReady {
                pending_terminal,
                receipt,
            } => {
                assert_eq!(pending_terminal.status, ExecutionRunStatus::Blocked);
                assert_eq!(
                    pending_terminal.reason,
                    ExecutionTerminalReason::RepeatedFailure
                );
                assert!(pending_terminal.output.is_none());
                assert!(
                    pending_terminal
                        .terminal_gaps
                        .iter()
                        .any(|gap| { gap == "replan stop reason: repeated_failure" })
                );
                assert_eq!(receipt.task_id, origin);
                assert_eq!(receipt.task_generation, 1);
                assert_eq!(receipt.base_plan_revision, run.plan_revision);
                assert_eq!(receipt.amendment_hash, amendment_hash);
                break;
            }
            other => panic!("unexpected ReplanStop completion outcome: {other:?}"),
        }
        assert!(page_count <= 4, "ReplanStop cursor failed to make progress");
    }
    assert_eq!(page_count, 4, "three task pages plus one node page");
    let verifier_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM moa.execution_task WHERE run_uid=$1 AND node_id LIKE '@check/%'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(verifier_count, 0);
    Ok(())
}

#[tokio::test]
async fn every_terminal_cause_cohort_survives_finalization_and_reload_db() -> TestResult {
    // Pins: the limit-stopped-completion, budget limit-stop, deadline limit-stop, and
    // internal-failure terminal causes each finalize a run through the bounded terminal drain and
    // decode back into the identical closed cause and normalized reason on reload. Only four of
    // the eight causes reach a persisted run anywhere else, so a column, encoding, or
    // strict-decode regression on the other cohorts is otherwise silent.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();

    let cohorts = [
        (
            "limit-stopped-completion",
            ExecutionRunStatus::Failed,
            ExecutionTerminalReason::BudgetExceeded,
            ExecutionTerminalCause::Completion {
                limit_stop: Some(ExecutionLimitStop::BudgetExceeded),
            },
        ),
        (
            "budget-limit-stop",
            ExecutionRunStatus::Failed,
            ExecutionTerminalReason::BudgetExceeded,
            ExecutionTerminalCause::LimitStop {
                reason: ExecutionLimitStop::BudgetExceeded,
            },
        ),
        (
            "internal-failure",
            ExecutionRunStatus::Failed,
            ExecutionTerminalReason::InternalFailure,
            ExecutionTerminalCause::InternalFailure,
        ),
    ];
    for (key, status, reason, cause) in cohorts {
        let run = create_run(
            &repository,
            scope,
            new_run(
                tenant_id,
                None,
                &format!("terminal-cause-{key}-{}", Uuid::now_v7()),
                ExecutionRunStatus::Queued,
                budget(4),
            ),
        )
        .await?;
        let PendingTerminalAdvanceOutcome::Applied(commit) = repository
            .fence_completion_terminal_and_enqueue_settlement(
                &config,
                scope,
                run.run_uid,
                run.controller_generation,
                run.wake_epoch,
                PendingExecutionTerminal {
                    status,
                    reason,
                    terminal_evidence: ExecutionTerminalEvidence {
                        cause: cause.clone(),
                        satisfied_requirement_count: 0,
                        requirement_count: 0,
                    },
                    completion_check_results: Vec::new(),
                    terminal_gaps: vec![format!("{key} gap")],
                    output: None,
                    cancellation_reason: None,
                },
                moa_test_support::fixtures::pg_now(),
                1,
            )
            .await?
        else {
            panic!("a task-free {key} terminal must finalize on its first bounded page");
        };
        assert_eq!(
            commit.stage,
            PendingTerminalAdvanceStage::Finalized,
            "{key} must finalize rather than defer"
        );
        let reloaded = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("finalized run stays visible");
        assert_eq!(reloaded.status, status, "{key} status");
        assert_eq!(reloaded.terminal_reason, Some(reason), "{key} reason");
        assert_eq!(
            reloaded
                .terminal_evidence
                .as_ref()
                .map(|evidence| &evidence.cause),
            Some(&cause),
            "{key} cause must decode back verbatim"
        );
        assert!(!reloaded.manual_repair_required, "{key} needs no repair");
        assert!(reloaded.pending_terminal.is_none(), "{key} intent consumed");
    }

    // The deadline cohort has a real producer, so drive that instead of asserting a hand-built
    // intent: an already-elapsed approved deadline must fence LimitStop { DeadlineExceeded }.
    let mut elapsed = new_run(
        tenant_id,
        None,
        &format!("terminal-cause-deadline-{}", Uuid::now_v7()),
        ExecutionRunStatus::Queued,
        budget(4),
    );
    elapsed.approved_budget.deadline_at = Some(pg_deadline(Duration::seconds(-30)));
    let run = create_run(&repository, scope, elapsed).await?;
    let PendingTerminalAdvanceOutcome::Applied(commit) = repository
        .fence_deadline_and_enqueue_settlement(
            &config,
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
            moa_test_support::fixtures::pg_now(),
            1,
        )
        .await?
    else {
        panic!("an elapsed approved deadline must fence and finalize its own terminal");
    };
    assert_eq!(commit.stage, PendingTerminalAdvanceStage::Finalized);
    let reloaded = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("deadline-terminal run stays visible");
    assert_eq!(reloaded.status, ExecutionRunStatus::Failed);
    assert_eq!(
        reloaded.terminal_reason,
        Some(ExecutionTerminalReason::DeadlineExceeded)
    );
    assert_eq!(
        reloaded
            .terminal_evidence
            .as_ref()
            .map(|evidence| &evidence.cause),
        Some(&ExecutionTerminalCause::LimitStop {
            reason: ExecutionLimitStop::DeadlineExceeded,
        }),
        "the deadline fence must record the limit stop itself, not only the normalized reason"
    );
    Ok(())
}
