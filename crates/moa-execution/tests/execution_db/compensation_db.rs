//! Compensation registration, reverse-order settlement, and replay persistence contracts.

use moa_artifacts::execution_plan::{
    CapabilityReference, CompensationInputBinding, CompensationInputMapping,
    CompensationValueSource, ExecutionCancelPolicy, ExecutionCompensation,
};
use moa_core::types::{
    action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
    tools::IdempotencyClass,
};
use moa_execution::{
    capability::{
        CapabilityPolicyContext, CapabilityRollbackContract, CapabilitySource, ExecutionCapability,
        ExecutionClass,
    },
    repository::{
        BeginCompensationOutcome, CompensationClaimOutcome, CompensationFinalizationOutcome,
        CompensationOutcomeWrite, ExecutionEffectAdmissionOutcome, ExecutionEffectOwner,
        FencedTerminalFinalizationOutcome, TerminalFenceOutcome,
    },
    state::{
        CompensationId, CompensationStatus, ExecutionCompensationOutcome,
        ExecutionTerminalEvidence, PendingExecutionTerminal,
    },
    wire::ExecutionToolDispatchRejection,
};

use super::support::*;

#[tokio::test]
async fn concurrent_forward_commits_register_one_unique_monotonic_sequence_each_db() -> TestResult {
    // Pins: forward effects may finish concurrently, but the run-row lock makes
    // their atomic compensation registrations unique and gap-free.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        "concurrent-compensation-registration",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let run = create_run(&repository, scope, new).await?;
    let tasks = ["concurrent_a", "concurrent_b", "concurrent_c"].map(|node_id| {
        compensated_task(
            run.run_uid,
            node_id,
            forward_reference.clone(),
            compensation.clone(),
        )
    });
    repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.to_vec())
        .await?;
    for task in &tasks {
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    }
    let (first, second, third) = tokio::join!(
        repository.record_task_outcome(scope, run.run_uid, tasks[0].task_id, 1, completed(1)),
        repository.record_task_outcome(scope, run.run_uid, tasks[1].task_id, 1, completed(1)),
        repository.record_task_outcome(scope, run.run_uid, tasks[2].task_id, 1, completed(1)),
    );
    for outcome in [first?, second?, third?] {
        assert!(matches!(outcome, TaskOutcomeWrite::Applied { .. }));
    }
    let snapshot = repository
        .load_compensation_snapshot(scope, run.run_uid)
        .await?
        .expect("concurrent forward run must remain visible");
    let mut sequences = snapshot
        .registrations
        .iter()
        .map(|registration| registration.registered_sequence)
        .collect::<Vec<_>>();
    sequences.sort_unstable();
    assert_eq!(sequences, vec![1, 2, 3]);
    let mut owners = snapshot
        .registrations
        .iter()
        .map(|registration| registration.forward_task_id)
        .collect::<Vec<_>>();
    owners.sort_unstable();
    owners.dedup();
    assert_eq!(
        owners.len(),
        3,
        "each committed effect must own one registration"
    );
    Ok(())
}

#[tokio::test]
async fn third_forward_failure_compensates_only_the_first_two_committed_effects_db() -> TestResult {
    // Pins: the acceptance failure is a real third task outcome, not an
    // artificial fence after all three effects have already committed.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        "third-forward-real-failure",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let run = create_run(&repository, scope, new).await?;
    let tasks = ["first_effect", "second_effect", "third_effect"].map(|node_id| {
        compensated_task(
            run.run_uid,
            node_id,
            forward_reference.clone(),
            compensation.clone(),
        )
    });
    repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.to_vec())
        .await?;
    for task in &tasks {
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    }
    for task in &tasks[..2] {
        assert!(matches!(
            repository
                .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
                .await?,
            TaskOutcomeWrite::Applied { .. }
        ));
    }
    let failed = ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(1),
        result: ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Terminal,
            message: "third forward effect failed before commit".to_string(),
        },
    };
    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, tasks[2].task_id, 1, failed)
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    let TerminalFenceOutcome::Applied(fence) =
        fence_failed_run(&repository, scope, run.run_uid).await?
    else {
        panic!("real third-task failure must install a terminal fence");
    };
    let BeginCompensationOutcome::Applied(begin) = repository
        .begin_compensation(
            scope,
            run.run_uid,
            fence.run.plan_revision,
            fence.run.wake_epoch,
        )
        .await?
    else {
        panic!("two committed effects must enter compensation");
    };
    assert_eq!(
        begin
            .registrations
            .iter()
            .map(|registration| registration.forward_task_id)
            .collect::<Vec<_>>(),
        vec![tasks[1].task_id, tasks[0].task_id]
    );
    assert!(
        begin
            .registrations
            .iter()
            .all(|registration| registration.forward_task_id != tasks[2].task_id),
        "the failed third effect must not invent a rollback registration"
    );
    Ok(())
}

#[tokio::test]
async fn reverse_order_failure_finalizes_without_settling_lower_compensations_db() -> TestResult {
    // Pins: after the highest compensation completes and the next one fails,
    // finalization records manual repair without trying to claim or settle the
    // lower pending registration.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        "reverse-order-compensation-failure",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let run = create_run(&repository, scope, new).await?;
    let tasks = ["effect_lowest", "effect_middle", "effect_highest"].map(|node_id| {
        compensated_task(
            run.run_uid,
            node_id,
            forward_reference.clone(),
            compensation.clone(),
        )
    });
    repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.to_vec())
        .await?;
    for task in &tasks {
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
        assert!(matches!(
            repository
                .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
                .await?,
            TaskOutcomeWrite::Applied { .. }
        ));
    }

    let fenced = fence_failed_run(&repository, scope, run.run_uid).await?;
    let TerminalFenceOutcome::Applied(fence) = fenced else {
        panic!("first terminal fence must be applied");
    };
    assert!(fence.tasks_to_settle.is_empty());
    let BeginCompensationOutcome::Applied(begin) = repository
        .begin_compensation(
            scope,
            run.run_uid,
            fence.run.plan_revision,
            fence.run.wake_epoch,
        )
        .await?
    else {
        panic!("fenced run with three registrations must begin compensation");
    };
    assert_eq!(
        begin
            .registrations
            .iter()
            .map(|registration| registration.forward_task_id)
            .collect::<Vec<_>>(),
        vec![tasks[2].task_id, tasks[1].task_id, tasks[0].task_id]
    );

    let highest = &begin.registrations[0];
    let CompensationClaimOutcome::Claimed(claimed_highest) = repository
        .claim_next_compensation(
            scope,
            run.run_uid,
            highest.compensation_id,
            highest.generation,
        )
        .await?
    else {
        panic!("highest reverse sequence must be claimable first");
    };
    assert_eq!(claimed_highest.status, CompensationStatus::Running);
    let completed_outcome = ExecutionCompensationOutcome::Completed {
        output: json!({"tokens": 0}),
        usage: usage(1),
    };
    let CompensationOutcomeWrite::Completed(completed_highest) = repository
        .record_compensation_outcome(
            scope,
            run.run_uid,
            highest.compensation_id,
            highest.generation,
            completed_outcome.clone(),
        )
        .await?
    else {
        panic!("highest compensation must settle completed");
    };
    assert_eq!(completed_highest.outcome, Some(completed_outcome));

    let middle = &begin.registrations[1];
    let CompensationClaimOutcome::Claimed(claimed_middle) = repository
        .claim_next_compensation(
            scope,
            run.run_uid,
            middle.compensation_id,
            middle.generation,
        )
        .await?
    else {
        panic!("middle reverse sequence must be claimable after the highest settles");
    };
    assert_eq!(claimed_middle.status, CompensationStatus::Running);
    let failed_outcome = ExecutionCompensationOutcome::Failed {
        message: "undo was rejected permanently".to_string(),
        retryable: false,
        usage: usage(1),
    };
    let CompensationOutcomeWrite::Failed(failed_middle) = repository
        .record_compensation_outcome(
            scope,
            run.run_uid,
            middle.compensation_id,
            middle.generation,
            failed_outcome.clone(),
        )
        .await?
    else {
        panic!("terminal undo failure must persist as failed");
    };
    assert_eq!(failed_middle.outcome, Some(failed_outcome.clone()));

    let snapshot = repository
        .load_compensation_snapshot(scope, run.run_uid)
        .await?
        .expect("compensating run must remain visible");
    assert_eq!(
        snapshot
            .registrations
            .iter()
            .map(|registration| registration.status)
            .collect::<Vec<_>>(),
        vec![
            CompensationStatus::Completed,
            CompensationStatus::Failed,
            CompensationStatus::Pending,
        ]
    );
    assert!(snapshot.manual_repair_required);
    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("manual-repair run must remain visible");
    let CompensationFinalizationOutcome::ManualRepairRequired(finalized) = repository
        .finalize_compensation(scope, run.run_uid, current.wake_epoch)
        .await?
    else {
        panic!("failed compensation must finalize as manual repair without waiting on lower work");
    };
    assert_eq!(finalized.status, ExecutionRunStatus::Failed);
    assert_eq!(
        finalized.terminal_reason,
        Some(ExecutionTerminalReason::CompensationFailed)
    );
    assert!(finalized.manual_repair_required);
    assert!(finalized.pending_terminal.is_none());
    assert!(matches!(
        finalized.terminal_evidence.as_ref().map(|evidence| &evidence.cause),
        Some(ExecutionTerminalCause::CompensationFailure {
            compensation_id,
            outcome,
            ..
        }) if *compensation_id == middle.compensation_id && outcome == &failed_outcome
    ));
    let finalized_snapshot = repository
        .load_compensation_snapshot(scope, run.run_uid)
        .await?
        .expect("finalized run must retain compensation audit rows");
    assert_eq!(
        finalized_snapshot
            .registrations
            .iter()
            .map(|registration| registration.status)
            .collect::<Vec<_>>(),
        vec![
            CompensationStatus::Completed,
            CompensationStatus::Failed,
            CompensationStatus::Pending,
        ]
    );
    Ok(())
}

#[tokio::test]
async fn external_effect_admission_is_generation_and_terminal_fence_linearized_db() -> TestResult {
    // Pins: ToolExecutor can start an effect only for the exact current running
    // owner before its terminal fence; pending, stale, manual-repair, and
    // terminal lifecycle states fail closed.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };

    let (task_catalog, task_forward_reference, task_compensation) = compensated_catalog();
    let mut task_new = new_run(
        tenant_id,
        None,
        "forward-effect-admission",
        ExecutionRunStatus::Queued,
        budget(5),
    );
    task_new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    task_new.plan.catalog_hash = task_catalog.catalog_hash;
    task_new.authorization.capability_refs = task_catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    task_new.catalog = task_catalog;
    let task_run = create_run(&repository, scope, task_new).await?;
    let task = compensated_task(
        task_run.run_uid,
        "forward_effect",
        task_forward_reference,
        task_compensation,
    );
    repository
        .materialize_tasks(scope, task_run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, task_run.run_uid, task.task_id).await?;
    let task_owner = ExecutionEffectOwner::Task {
        task_id: task.task_id,
        generation: 1,
    };
    assert_eq!(
        repository
            .admit_execution_effect(scope, task_run.run_uid, task_run.session_id, task_owner)
            .await?,
        ExecutionEffectAdmissionOutcome::Admitted
    );
    assert_eq!(
        repository
            .admit_execution_effect(scope, task_run.run_uid, SessionId::new(), task_owner,)
            .await?,
        ExecutionEffectAdmissionOutcome::Rejected(ExecutionToolDispatchRejection::OriginNotFound)
    );
    assert_eq!(
        repository
            .admit_execution_effect(
                scope,
                task_run.run_uid,
                task_run.session_id,
                ExecutionEffectOwner::Task {
                    task_id: task.task_id,
                    generation: 2,
                },
            )
            .await?,
        ExecutionEffectAdmissionOutcome::Rejected(ExecutionToolDispatchRejection::StaleGeneration)
    );
    let TerminalFenceOutcome::Applied(task_fence) =
        fence_failed_run(&repository, scope, task_run.run_uid).await?
    else {
        panic!("running forward task must accept an admission fence");
    };
    assert_eq!(
        task_fence
            .tasks_to_settle
            .iter()
            .map(|task| task.task_id)
            .collect::<Vec<_>>(),
        vec![task.task_id]
    );
    assert_eq!(
        repository
            .admit_execution_effect(scope, task_run.run_uid, task_run.session_id, task_owner)
            .await?,
        ExecutionEffectAdmissionOutcome::Rejected(
            ExecutionToolDispatchRejection::RunNotDispatchable
        )
    );

    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        "compensation-effect-admission",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let compensation_run = create_run(&repository, scope, new).await?;
    let forward_task = compensated_task(
        compensation_run.run_uid,
        "compensated_effect",
        forward_reference,
        compensation,
    );
    repository
        .materialize_tasks(
            scope,
            compensation_run.run_uid,
            1,
            vec![forward_task.clone()],
        )
        .await?;
    reserve_and_start(
        &repository,
        scope,
        compensation_run.run_uid,
        forward_task.task_id,
    )
    .await?;
    assert!(matches!(
        repository
            .record_task_outcome(
                scope,
                compensation_run.run_uid,
                forward_task.task_id,
                1,
                completed(1),
            )
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    let TerminalFenceOutcome::Applied(compensation_fence) =
        fence_failed_run(&repository, scope, compensation_run.run_uid).await?
    else {
        panic!("completed forward effect must accept a compensation fence");
    };
    let BeginCompensationOutcome::Applied(begin) = repository
        .begin_compensation(
            scope,
            compensation_run.run_uid,
            compensation_fence.run.plan_revision,
            compensation_fence.run.wake_epoch,
        )
        .await?
    else {
        panic!("registered effect must begin compensation");
    };
    let registration = &begin.registrations[0];
    let compensation_owner = ExecutionEffectOwner::Compensation {
        compensation_id: registration.compensation_id,
        generation: registration.generation,
    };
    assert_eq!(registration.status, CompensationStatus::Pending);
    assert_eq!(
        repository
            .admit_execution_effect(
                scope,
                compensation_run.run_uid,
                compensation_run.session_id,
                compensation_owner,
            )
            .await?,
        ExecutionEffectAdmissionOutcome::Rejected(
            ExecutionToolDispatchRejection::OperationNotRunning
        )
    );
    assert!(matches!(
        repository
            .claim_next_compensation(
                scope,
                compensation_run.run_uid,
                registration.compensation_id,
                registration.generation,
            )
            .await?,
        CompensationClaimOutcome::Claimed(_)
    ));
    assert_eq!(
        repository
            .admit_execution_effect(
                scope,
                compensation_run.run_uid,
                compensation_run.session_id,
                compensation_owner,
            )
            .await?,
        ExecutionEffectAdmissionOutcome::Admitted
    );
    assert_eq!(
        repository
            .admit_execution_effect(
                scope,
                compensation_run.run_uid,
                compensation_run.session_id,
                ExecutionEffectOwner::Compensation {
                    compensation_id: registration.compensation_id,
                    generation: registration.generation + 1,
                },
            )
            .await?,
        ExecutionEffectAdmissionOutcome::Rejected(ExecutionToolDispatchRejection::StaleGeneration)
    );
    assert!(matches!(
        repository
            .record_compensation_outcome(
                scope,
                compensation_run.run_uid,
                registration.compensation_id,
                registration.generation,
                ExecutionCompensationOutcome::Failed {
                    message: "manual repair required".to_string(),
                    retryable: false,
                    usage: usage(1),
                },
            )
            .await?,
        CompensationOutcomeWrite::Failed(_)
    ));
    assert_eq!(
        repository
            .admit_execution_effect(
                scope,
                compensation_run.run_uid,
                compensation_run.session_id,
                compensation_owner,
            )
            .await?,
        ExecutionEffectAdmissionOutcome::Rejected(
            ExecutionToolDispatchRejection::RunNotDispatchable
        )
    );
    let repair_run = repository
        .load_run(scope, compensation_run.run_uid)
        .await?
        .expect("manual-repair run must remain visible");
    assert!(repair_run.manual_repair_required);
    assert!(matches!(
        repository
            .finalize_compensation(scope, compensation_run.run_uid, repair_run.wake_epoch)
            .await?,
        CompensationFinalizationOutcome::ManualRepairRequired(_)
    ));
    assert_eq!(
        repository
            .admit_execution_effect(
                scope,
                compensation_run.run_uid,
                compensation_run.session_id,
                compensation_owner,
            )
            .await?,
        ExecutionEffectAdmissionOutcome::Rejected(
            ExecutionToolDispatchRejection::RunNotDispatchable
        )
    );
    Ok(())
}

#[tokio::test]
async fn clean_compensations_claim_strict_reverse_order_and_restore_terminal_intent_db()
-> TestResult {
    // Pins: clean rollback dispatches every registration in descending commit
    // order, rejects an early lower claim, replays a settled generation, and
    // installs the exact held terminal intent only after the full drain.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        "clean-reverse-compensation-drain",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let run = create_run(&repository, scope, new).await?;
    let tasks = ["clean_lowest", "clean_middle", "clean_highest"].map(|node_id| {
        compensated_task(
            run.run_uid,
            node_id,
            forward_reference.clone(),
            compensation.clone(),
        )
    });
    repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.to_vec())
        .await?;
    for task in &tasks {
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
        assert!(matches!(
            repository
                .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
                .await?,
            TaskOutcomeWrite::Applied { .. }
        ));
    }
    let TerminalFenceOutcome::Applied(fence) =
        fence_failed_run(&repository, scope, run.run_uid).await?
    else {
        panic!("clean rollback fixture must accept its terminal fence");
    };
    let BeginCompensationOutcome::Applied(begin) = repository
        .begin_compensation(
            scope,
            run.run_uid,
            fence.run.plan_revision,
            fence.run.wake_epoch,
        )
        .await?
    else {
        panic!("clean rollback fixture must begin compensation");
    };
    assert_eq!(
        begin
            .registrations
            .iter()
            .map(|registration| registration.forward_task_id)
            .collect::<Vec<_>>(),
        vec![tasks[2].task_id, tasks[1].task_id, tasks[0].task_id]
    );
    let highest = &begin.registrations[0];
    let middle = &begin.registrations[1];
    let lowest = &begin.registrations[2];
    assert_eq!(
        repository
            .claim_next_compensation(
                scope,
                run.run_uid,
                lowest.compensation_id,
                lowest.generation,
            )
            .await?,
        CompensationClaimOutcome::Conflict
    );

    let highest_outcome = ExecutionCompensationOutcome::Completed {
        output: json!({"tokens": 0}),
        usage: usage(1),
    };
    assert!(matches!(
        repository
            .claim_next_compensation(
                scope,
                run.run_uid,
                highest.compensation_id,
                highest.generation,
            )
            .await?,
        CompensationClaimOutcome::Claimed(_)
    ));
    let CompensationOutcomeWrite::Completed(completed_highest) = repository
        .record_compensation_outcome(
            scope,
            run.run_uid,
            highest.compensation_id,
            highest.generation,
            highest_outcome.clone(),
        )
        .await?
    else {
        panic!("highest clean compensation must complete");
    };
    let CompensationOutcomeWrite::Replayed(replayed_highest) = repository
        .record_compensation_outcome(
            scope,
            run.run_uid,
            highest.compensation_id,
            highest.generation,
            highest_outcome,
        )
        .await?
    else {
        panic!("settled compensation outcome must replay exactly");
    };
    assert_eq!(replayed_highest, completed_highest);

    for registration in [middle, lowest] {
        assert!(matches!(
            repository
                .claim_next_compensation(
                    scope,
                    run.run_uid,
                    registration.compensation_id,
                    registration.generation,
                )
                .await?,
            CompensationClaimOutcome::Claimed(_)
        ));
        assert!(matches!(
            repository
                .record_compensation_outcome(
                    scope,
                    run.run_uid,
                    registration.compensation_id,
                    registration.generation,
                    ExecutionCompensationOutcome::Completed {
                        output: json!({"tokens": 0}),
                        usage: usage(1),
                    },
                )
                .await?,
            CompensationOutcomeWrite::Completed(_)
        ));
    }

    let before_finalization = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("drained compensation run must remain visible");
    let CompensationFinalizationOutcome::Finalized(finalized) = repository
        .finalize_compensation(scope, run.run_uid, before_finalization.wake_epoch)
        .await?
    else {
        panic!("clean reverse drain must restore its held terminal intent");
    };
    assert_eq!(finalized.status, ExecutionRunStatus::Failed);
    assert_eq!(
        finalized.terminal_reason,
        Some(ExecutionTerminalReason::InternalFailure)
    );
    assert_eq!(
        finalized.terminal_evidence,
        Some(ExecutionTerminalEvidence {
            cause: ExecutionTerminalCause::InternalFailure,
            satisfied_requirement_count: 0,
            requirement_count: 0,
        })
    );
    assert_eq!(
        finalized.completion_check_results,
        vec![json!({
            "check_id": "pre-compensation",
            "passed": false,
            "evidence": {"reason": "forward execution failed"},
        })]
    );
    assert_eq!(
        finalized.terminal_gaps,
        vec!["forward execution failed".to_string()]
    );
    assert_eq!(finalized.output, Some(json!({"forward": "evidence"})));
    assert!(!finalized.manual_repair_required);
    assert!(finalized.pending_terminal.is_none());
    Ok(())
}

#[tokio::test]
async fn ambiguous_forward_effect_fences_automatic_compensation_and_finalizes_manual_repair_db()
-> TestResult {
    // Pins: an ambiguous forward effect never invents an undo registration or
    // dispatch; once fenced and settled, the general terminal path preserves
    // the ambiguity as compensation_failed manual-repair evidence.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        "ambiguous-forward-effect",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let run = create_run(&repository, scope, new).await?;
    let task = compensated_task(
        run.run_uid,
        "ambiguous_effect",
        forward_reference,
        compensation,
    );
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    let ambiguous_outcome = ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(1),
        result: ExecutionTaskResult::UnknownOutcome {
            message: "forward effect may have committed".to_string(),
        },
    };
    let TaskOutcomeWrite::Applied {
        run: ambiguous_run,
        task: ambiguous_task,
        ..
    } = repository
        .record_task_outcome(
            scope,
            run.run_uid,
            task.task_id,
            1,
            ambiguous_outcome.clone(),
        )
        .await?
    else {
        panic!("forward UnknownOutcome must commit as a durable terminal task outcome");
    };
    assert_eq!(ambiguous_task.status, ExecutionTaskStatus::Failed);
    assert_eq!(ambiguous_task.current_outcome, Some(ambiguous_outcome));
    assert!(ambiguous_run.manual_repair_required);
    let snapshot = repository
        .load_compensation_snapshot(scope, run.run_uid)
        .await?
        .expect("ambiguous run must remain visible");
    assert!(snapshot.registrations.is_empty());
    assert!(snapshot.manual_repair_required);
    let completion_evaluation = CompletionEvaluation {
        status: CompletionStatus::Completed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: Vec::new(),
    };
    let completion_cause = ExecutionTerminalCause::Completion { limit_stop: None };
    let completion_projection = TerminalProjection::Completed {
        output: json!({"status": "complete"}),
    };
    let completion_evidence =
        terminal_evidence_from_evaluation(completion_cause.clone(), &completion_evaluation)?;
    let completion_reason = execution_terminal_reason(
        &completion_cause,
        &completion_projection,
        &completion_evaluation,
    )?;
    let mut ordinary_finalization = RunFinalizationRequest {
        run_uid: run.run_uid,
        expected_revision: ambiguous_run.plan_revision,
        expected_wake_epoch: ambiguous_run.wake_epoch,
        terminal_projection: completion_projection,
        completion_evaluation,
        terminal_evidence: completion_evidence,
        terminal_reason: completion_reason,
    };
    assert_eq!(
        repository
            .finalize_run(scope, ordinary_finalization.clone())
            .await?,
        FinalizationOutcome::Conflict,
        "ordinary finalization must reject manual-repair state"
    );

    let TerminalFenceOutcome::Applied(fence) =
        fence_failed_run(&repository, scope, run.run_uid).await?
    else {
        panic!("ambiguous settled forward task must accept the terminal fence");
    };
    assert!(fence.tasks_to_settle.is_empty());
    ordinary_finalization.expected_wake_epoch = fence.run.wake_epoch;
    assert_eq!(
        repository
            .finalize_run(scope, ordinary_finalization)
            .await?,
        FinalizationOutcome::Conflict,
        "ordinary finalization must reject a pending terminal intent"
    );
    assert_eq!(
        repository
            .claim_next_compensation(scope, run.run_uid, CompensationId::derive(task.task_id), 1,)
            .await?,
        CompensationClaimOutcome::Conflict
    );
    let FencedTerminalFinalizationOutcome::ManualRepairRequired(finalized) = repository
        .finalize_fenced_terminal(
            scope,
            run.run_uid,
            fence.run.plan_revision,
            fence.run.wake_epoch,
        )
        .await?
    else {
        panic!("ambiguous forward effect must finalize through the manual-repair path");
    };
    let expected_compensation_outcome = ExecutionCompensationOutcome::UnknownOutcome {
        message: "forward effect may have committed".to_string(),
        usage: usage(1),
    };
    assert_eq!(finalized.status, ExecutionRunStatus::Failed);
    assert_eq!(
        finalized.terminal_reason,
        Some(ExecutionTerminalReason::CompensationFailed)
    );
    assert!(finalized.manual_repair_required);
    assert!(finalized.pending_terminal.is_none());
    assert!(matches!(
        finalized.terminal_evidence.as_ref().map(|evidence| &evidence.cause),
        Some(ExecutionTerminalCause::CompensationFailure {
            original_status: ExecutionRunStatus::Failed,
            original_reason: ExecutionTerminalReason::InternalFailure,
            compensation_id,
            outcome,
            ..
        }) if *compensation_id == CompensationId::derive(task.task_id)
            && outcome == &expected_compensation_outcome
    ));
    Ok(())
}

#[tokio::test]
async fn retryable_compensation_replay_recovers_requeued_generation_db() -> TestResult {
    // Pins: replaying the accepted generation-one retry outcome returns the
    // already-requeued generation-two projection instead of conflicting, and
    // the next generation remains claimable with review idempotency intact.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let (catalog, forward_reference, compensation) = compensated_catalog();
    let mut new = new_run(
        tenant_id,
        None,
        "compensation-retry-replay",
        ExecutionRunStatus::Queued,
        budget(20),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    new.plan.catalog_hash = catalog.catalog_hash;
    new.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    new.catalog = catalog;
    let run = create_run(&repository, scope, new).await?;
    let task = compensated_task(run.run_uid, "retry", forward_reference, compensation);
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    let TerminalFenceOutcome::Applied(fence) =
        fence_failed_run(&repository, scope, run.run_uid).await?
    else {
        panic!("first terminal fence must apply");
    };
    let BeginCompensationOutcome::Applied(begin) = repository
        .begin_compensation(
            scope,
            run.run_uid,
            fence.run.plan_revision,
            fence.run.wake_epoch,
        )
        .await?
    else {
        panic!("single registration must begin compensation");
    };
    let registration = &begin.registrations[0];
    assert!(matches!(
        repository
            .claim_next_compensation(scope, run.run_uid, registration.compensation_id, 1,)
            .await?,
        CompensationClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
            .await?,
        TaskOutcomeWrite::Replayed { .. }
    ));
    let claimed_snapshot = repository
        .load_compensation_snapshot(scope, run.run_uid)
        .await?
        .expect("claimed compensation must remain visible");
    assert_eq!(
        claimed_snapshot.registrations[0].status,
        CompensationStatus::Running
    );
    let retryable_failure = ExecutionCompensationOutcome::Failed {
        message: "retry undo".to_string(),
        retryable: true,
        usage: usage(1),
    };
    let CompensationOutcomeWrite::Requeued(requeued) = repository
        .record_compensation_outcome(
            scope,
            run.run_uid,
            registration.compensation_id,
            1,
            retryable_failure.clone(),
        )
        .await?
    else {
        panic!("first retryable failure must requeue atomically");
    };
    assert_eq!(requeued.status, CompensationStatus::Pending);
    assert_eq!(requeued.attempt, 2);
    assert_eq!(requeued.generation, 2);
    assert_eq!(requeued.outcome, Some(retryable_failure.clone()));

    let CompensationOutcomeWrite::Replayed(replayed) = repository
        .record_compensation_outcome(
            scope,
            run.run_uid,
            registration.compensation_id,
            1,
            retryable_failure,
        )
        .await?
    else {
        panic!("generation-one retry replay must recover the current generation-two row");
    };
    assert_eq!(replayed, requeued);

    let CompensationClaimOutcome::Claimed(claimed) = repository
        .claim_next_compensation(scope, run.run_uid, registration.compensation_id, 2)
        .await?
    else {
        panic!("replayed retry outcome must leave generation two claimable");
    };
    assert_eq!(claimed.status, CompensationStatus::Running);
    assert_eq!(claimed.generation, 2);

    let review_uid = Uuid::new_v4();
    let resolution = ExecutionActionReviewResolution::Completed {
        tool_output: json!({"reviewed": true}),
    };
    assert_eq!(
        repository
            .record_compensation_action_review_resolution(
                scope,
                run.run_uid,
                registration.compensation_id,
                2,
                review_uid,
                &resolution,
            )
            .await?,
        ActionReviewResolutionWrite::Applied
    );
    assert_eq!(
        repository
            .record_compensation_action_review_resolution(
                scope,
                run.run_uid,
                registration.compensation_id,
                2,
                review_uid,
                &resolution,
            )
            .await?,
        ActionReviewResolutionWrite::Replayed
    );
    let conflicting_resolution = ExecutionActionReviewResolution::Denied {
        reason: "different resolution".to_string(),
    };
    let conflict = repository
        .record_compensation_action_review_resolution(
            scope,
            run.run_uid,
            registration.compensation_id,
            2,
            review_uid,
            &conflicting_resolution,
        )
        .await
        .expect_err("same review identity with a different resolution must fail closed");
    assert!(
        matches!(conflict, moa_execution::Error::InvalidRepositoryData { .. }),
        "review identity conflict returned the wrong error: {conflict:?}"
    );
    Ok(())
}

#[tokio::test]
async fn invalid_compensation_mapping_registers_failed_without_rolling_back_forward_commit_db()
-> TestResult {
    // Pins: a committed forward effect is never erased when its durable
    // compensation mapping cannot resolve; the same transaction installs a
    // deterministic failed registration and a manual-repair fence.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut new = new_run(
        tenant_id,
        None,
        "invalid-compensation-registration",
        ExecutionRunStatus::Queued,
        budget(5),
    );
    new.plan.definition.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    let run = create_run(&repository, scope, new).await?;
    let forward_reference = capability_reference("effects.commit");
    let missing_compensator = capability_reference("effects.missing_undo");
    let compensation = ExecutionCompensation {
        compensator: missing_compensator,
        input_mapping: token_mapping(),
    };
    let task = compensated_task(
        run.run_uid,
        "invalid-mapping",
        forward_reference,
        compensation.clone(),
    );
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    let TaskOutcomeWrite::Applied {
        run: committed_run,
        task: committed_task,
        ..
    } = repository
        .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
        .await?
    else {
        panic!("forward completion must commit despite invalid compensation mapping");
    };
    assert_eq!(committed_task.status, ExecutionTaskStatus::Completed);
    assert_eq!(committed_task.output, Some(json!({"tokens": 1})));
    assert!(committed_run.manual_repair_required);
    assert_eq!(committed_run.next_compensation_sequence, 2);

    let snapshot = repository
        .load_compensation_snapshot(scope, run.run_uid)
        .await?
        .expect("forward commit must retain its failed compensation registration");
    assert!(snapshot.manual_repair_required);
    assert_eq!(snapshot.registrations.len(), 1);
    let registration = &snapshot.registrations[0];
    let expected_message = "invalid execution repository data: persisted compensation contract has no pinned compensator";
    assert_eq!(
        registration.compensation_id,
        CompensationId::derive(task.task_id)
    );
    assert_eq!(registration.forward_task_id, task.task_id);
    assert_eq!(registration.registered_sequence, 1);
    assert_eq!(registration.forward_generation, 1);
    assert_eq!(registration.compensator, compensation);
    assert_eq!(registration.mapped_input, serde_json::Value::Null);
    assert_eq!(registration.status, CompensationStatus::Failed);
    assert_eq!(
        registration.outcome,
        Some(ExecutionCompensationOutcome::Failed {
            message: expected_message.to_string(),
            retryable: false,
            usage: usage(0),
        })
    );
    assert_eq!(
        registration.error,
        Some(json!({
            "class": "mapping_input_invalid",
            "message": expected_message,
        }))
    );
    assert!(registration.started_at.is_some());
    assert!(registration.completed_at.is_some());

    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, task.task_id, 1, completed(1))
            .await?,
        TaskOutcomeWrite::Replayed { .. }
    ));
    let replayed_snapshot = repository
        .load_compensation_snapshot(scope, run.run_uid)
        .await?
        .expect("replayed forward commit must retain compensation audit");
    assert_eq!(replayed_snapshot.registrations, snapshot.registrations);
    Ok(())
}

fn compensated_catalog() -> (
    ExecutionCapabilityCatalog,
    CapabilityReference,
    ExecutionCompensation,
) {
    let mut forward = capability("effects.commit");
    let compensator = capability("effects.undo");
    let compensation = ExecutionCompensation {
        compensator: compensator.reference.clone(),
        input_mapping: token_mapping(),
    };
    forward.rollback = Some(CapabilityRollbackContract {
        compensator: compensation.compensator.clone(),
        input_mapping: compensation.input_mapping.clone(),
    });
    let forward_reference = forward.reference.clone();
    let catalog = ExecutionCapabilityCatalog::build(vec![forward, compensator])
        .expect("compensated test catalog must be valid");
    (catalog, forward_reference, compensation)
}

fn capability(name: &str) -> ExecutionCapability {
    let source = CapabilitySource::BuiltInTool {
        name: name.to_string(),
    };
    ExecutionCapability {
        reference: capability_reference(name),
        contract_revision: "contract-v1".to_string(),
        description: format!("test capability {name}"),
        input_schema: json!({
            "type": "object",
            "required": ["tokens"],
            "properties": {"tokens": {"type": "integer", "minimum": 0}},
            "additionalProperties": false,
        }),
        output_schema: json!({
            "type": "object",
            "required": ["tokens"],
            "properties": {"tokens": {"type": "integer", "minimum": 0}},
            "additionalProperties": false,
        }),
        action_class: ActionClass::ExternalWrite,
        risk_level: RiskLevel::Medium,
        default_effect: ActionPolicyEffect::Allow,
        idempotency_class: IdempotencyClass::Idempotent,
        execution_class: ExecutionClass::External,
        policy_context: CapabilityPolicyContext::registered(source.clone()),
        source,
        estimate: estimate(1),
        rollback: None,
    }
}

fn capability_reference(name: &str) -> CapabilityReference {
    CapabilityReference {
        name: name.to_string(),
        version: "v1".to_string(),
    }
}

fn token_mapping() -> CompensationInputMapping {
    CompensationInputMapping {
        bindings: vec![CompensationInputBinding {
            target_pointer: "/tokens".to_string(),
            source: CompensationValueSource::OriginalOutput {
                pointer: "/tokens".to_string(),
            },
        }],
    }
}

fn compensated_task(
    run_uid: Uuid,
    node_id: &str,
    forward_reference: CapabilityReference,
    compensation: ExecutionCompensation,
) -> LogicalTask {
    let mut task = logical_task(run_uid, node_id, "", estimate(1));
    task.input = json!({"tokens": 1});
    task.kind = LogicalTaskKind::Capability {
        reference: forward_reference,
    };
    task.compensation = Some(compensation);
    task
}

async fn fence_failed_run(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: Uuid,
) -> Result<TerminalFenceOutcome, moa_execution::Error> {
    let run = repository.load_run(scope, run_uid).await?.ok_or_else(|| {
        moa_execution::Error::InvalidRepositoryInput {
            message: "compensation fixture run is missing".to_string(),
        }
    })?;
    repository
        .fence_run_for_terminal(
            scope,
            run_uid,
            run.plan_revision,
            run.wake_epoch,
            PendingExecutionTerminal {
                status: ExecutionRunStatus::Failed,
                reason: ExecutionTerminalReason::InternalFailure,
                terminal_evidence: ExecutionTerminalEvidence {
                    cause: ExecutionTerminalCause::InternalFailure,
                    satisfied_requirement_count: 0,
                    requirement_count: 0,
                },
                completion_check_results: vec![json!({
                    "check_id": "pre-compensation",
                    "passed": false,
                    "evidence": {"reason": "forward execution failed"},
                })],
                terminal_gaps: vec!["forward execution failed".to_string()],
                output: Some(json!({"forward": "evidence"})),
                cancellation_reason: None,
            },
        )
        .await
}
