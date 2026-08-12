//! Lifetime active-run admission and release contracts.

use moa_artifacts::execution_plan::{ExecutionNode, ExecutionOperation};
use moa_core::events::{ExecutionBlockerAudience, ExecutionProgressPhase};
use moa_execution::repository::capacity::{
    ExecutionCapacityDimension, execution_capacity_reservation_uid,
};
use moa_execution::repository::{
    RunDeadlineArmOutcome,
    terminal::{RunTriggerDrainOutcome, RunTriggerDrainRequest},
    trigger::{ExecutionTriggerKind, NewExecutionTrigger},
};
use sqlx::Row;

use super::support::*;

fn output_node(id: &str, depends_on: &[&str]) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: Vec::new(),
        depends_on: depends_on
            .iter()
            .map(|value| (*value).to_string())
            .collect(),
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

fn two_node_run(tenant_id: TenantId, key: &str) -> NewExecutionRun {
    let mut candidate = new_run(tenant_id, None, key, ExecutionRunStatus::Queued, budget(2));
    candidate.plan.definition.nodes =
        vec![output_node("first", &[]), output_node("second", &["first"])];
    candidate.plan.estimate.tasks = 2;
    candidate
}

fn successful_request(
    run: &moa_execution::repository::ExecutionRunRecord,
) -> Result<RunFinalizationRequest, moa_execution::Error> {
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Completed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: Vec::new(),
    };
    let cause = ExecutionTerminalCause::Completion { limit_stop: None };
    let terminal = TerminalProjection::Completed { output: json!({}) };
    let evidence = terminal_evidence_from_evaluation(cause.clone(), &evaluation)?;
    let reason = execution_terminal_reason(&cause, &terminal, &evaluation)?;
    Ok(RunFinalizationRequest {
        run_uid: run.run_uid,
        expected_revision: run.plan_revision,
        expected_wake_epoch: run.wake_epoch,
        terminal_projection: terminal,
        completion_evaluation: evaluation,
        terminal_evidence: evidence,
        terminal_reason: reason,
    })
}

#[tokio::test]
async fn run_admission_reserves_exact_capacity_seeds_nodes_and_rolls_back_saturation_db()
-> TestResult {
    // Pins: one tenant-scoped transaction inserts the run, set-seeds every canonical node,
    // reserves the deterministic fleet+tenant ActiveRuns receipt, and can see only its own
    // tenant bucket plus the shared fleet bucket; saturation leaves no partial run.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        max_fleet_active_runs: 1,
        max_tenant_active_runs: 1,
        ..ExecutionConfig::default()
    };
    let candidate = two_node_run(tenant_id, "active-run-first");
    let replay_candidate = candidate.clone();

    let RunAdmissionOutcome::Admitted(run) =
        create_run_with_config(&repository, scope, &config, candidate).await?
    else {
        panic!("first run must own the only active-run slot");
    };
    let expected_receipt = execution_capacity_reservation_uid(
        ExecutionCapacityDimension::ActiveRuns,
        run.run_uid,
        None,
    );
    let receipt = sqlx::query(
        "SELECT reservation_uid, controller_generation, state \
         FROM moa.execution_capacity_reservation \
         WHERE run_uid = $1 AND resource_dimension = 'active_runs'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        receipt.try_get::<Uuid, _>("reservation_uid")?,
        expected_receipt
    );
    assert_eq!(receipt.try_get::<i64, _>("controller_generation")?, 1);
    assert_eq!(receipt.try_get::<String, _>("state")?, "reserved");

    let deadline_boundary: (String, String, String, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state, capacity.state, trigger.due_at \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         JOIN moa.execution_capacity_reservation AS capacity USING (trigger_uid) \
         WHERE trigger.run_uid = $1 AND trigger.trigger_kind = 'run_deadline'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        deadline_boundary,
        (
            "pending".to_string(),
            "pending".to_string(),
            "reserved".to_string(),
            run.approved_budget.deadline_at.expect("fixture deadline"),
        ),
        "admission must durably own its deadline before a controller can activate"
    );
    let initial_activation: (String, i64, i64) = sqlx::query_as(
        "SELECT state, controller_generation, wake_epoch \
         FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND dispatch_kind = 'run_activation'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(initial_activation, ("pending".to_string(), 1, 1));
    assert_eq!(run.wake_epoch, 1);
    assert_eq!(run.next_wake_at, run.approved_budget.deadline_at);

    let nodes = sqlx::query(
        "SELECT node_state_uid, node_id, node_order, dependency_count, \
                remaining_dependency_count \
         FROM moa.execution_node_state WHERE run_uid = $1 ORDER BY node_order",
    )
    .bind(run.run_uid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(nodes.len(), 2);
    assert_eq!(nodes[0].try_get::<String, _>("node_id")?, "first");
    assert_eq!(nodes[0].try_get::<i64, _>("node_order")?, 0);
    assert_eq!(nodes[0].try_get::<i64, _>("dependency_count")?, 0);
    assert_eq!(nodes[1].try_get::<String, _>("node_id")?, "second");
    assert_eq!(nodes[1].try_get::<i64, _>("node_order")?, 1);
    assert_eq!(nodes[1].try_get::<i64, _>("dependency_count")?, 1);
    assert_eq!(nodes[1].try_get::<i64, _>("remaining_dependency_count")?, 1);
    assert_eq!(
        nodes[0].try_get::<Uuid, _>("node_state_uid")?,
        Uuid::new_v5(&run.run_uid, b"first")
    );

    let RunAdmissionOutcome::Replayed(replayed) =
        create_run_with_config(&repository, scope, &config, replay_candidate).await?
    else {
        panic!("the exact idempotency replay must reuse its admitted run");
    };
    assert_eq!(replayed.run_uid, run.run_uid);

    let saturated_key = "active-run-saturated";
    assert!(matches!(
        create_run_with_config(
            &repository,
            scope,
            &config,
            two_node_run(tenant_id, saturated_key),
        )
        .await?,
        RunAdmissionOutcome::CapacitySaturated {
            dimension: ExecutionCapacityDimension::ActiveRuns
        }
    ));
    assert!(
        repository
            .load_run_by_idempotency_key(scope, tenant_id, None, saturated_key)
            .await?
            .is_none(),
        "capacity saturation must roll back the inserted run"
    );
    let fleet_reserved: i64 = sqlx::query_scalar(
        "SELECT reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE scope_kind = 'fleet' AND resource_dimension = 'active_runs'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(fleet_reserved, 1);
    let tenant_reserved: i64 = sqlx::query_scalar(
        "SELECT reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE scope_kind = 'tenant' AND tenant_id = $1 \
           AND resource_dimension = 'active_runs'",
    )
    .bind(tenant_id.0)
    .fetch_one(&pool)
    .await?;
    assert_eq!(tenant_reserved, 1);

    let mut owner = moa_db::ScopedConn::begin_tenant(&pool, tenant_id).await?;
    owner.assume_app_role().await?;
    let owner_buckets: Vec<(String, Option<Uuid>, i64)> = sqlx::query_as(
        "SELECT scope_kind, tenant_id, reserved_quantity \
         FROM moa.execution_capacity_bucket \
         WHERE resource_dimension = 'active_runs' \
         ORDER BY scope_kind",
    )
    .fetch_all(owner.as_mut())
    .await?;
    owner.commit().await?;
    assert_eq!(
        owner_buckets,
        vec![
            ("fleet".to_string(), None, 1),
            ("tenant".to_string(), Some(tenant_id.0), 1),
        ]
    );

    let other_tenant_id = TenantId::new();
    let mut other = moa_db::ScopedConn::begin_tenant(&pool, other_tenant_id).await?;
    other.assume_app_role().await?;
    let other_buckets: Vec<(String, Option<Uuid>, i64)> = sqlx::query_as(
        "SELECT scope_kind, tenant_id, reserved_quantity \
         FROM moa.execution_capacity_bucket \
         WHERE resource_dimension = 'active_runs' \
         ORDER BY scope_kind",
    )
    .fetch_all(other.as_mut())
    .await?;
    other.commit().await?;
    assert_eq!(
        other_buckets,
        vec![("fleet".to_string(), None, 1)],
        "another tenant may observe the shared fleet ceiling but not the owner's bucket"
    );
    Ok(())
}

#[tokio::test]
async fn resident_run_entitlement_caps_active_plus_parked_at_one_db() -> TestResult {
    // Pins: ParkedRuns is the resident-run entitlement ceiling, so a cap of one rejects a
    // second admission with the typed ParkedRuns dimension whether the resident is active or
    // storage-only parked, and neither rejection leaves a partial run.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        max_fleet_active_runs: 1,
        max_tenant_active_runs: 1,
        max_fleet_parked_runs: 1,
        max_tenant_parked_runs: 1,
        ..ExecutionConfig::default()
    };
    let RunAdmissionOutcome::Admitted(first) = create_run_with_config(
        &repository,
        scope,
        &config,
        new_run(
            tenant_id,
            None,
            "resident-entitlement-first",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?
    else {
        panic!("first resident run must be admitted");
    };
    let second = || {
        new_run(
            tenant_id,
            None,
            "resident-entitlement-second",
            ExecutionRunStatus::Queued,
            budget(1),
        )
    };
    assert!(matches!(
        create_run_with_config(&repository, scope, &config, second()).await?,
        RunAdmissionOutcome::CapacitySaturated {
            dimension: ExecutionCapacityDimension::ParkedRuns
        }
    ));
    assert!(matches!(
        repository
            .claim_controller_wake(
                scope,
                first.run_uid,
                first.controller_generation,
                first.wake_epoch,
            )
            .await?,
        RunControllerClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        repository
            .complete_controller_wake(
                scope,
                &config,
                first.run_uid,
                RunControllerCompletionRequest {
                    controller_generation: first.controller_generation,
                    wake_epoch: first.wake_epoch,
                    checkpoint: ExecutionRunActivationCheckpoint {
                        status: ExecutionRunStatus::WaitingInput,
                        activation_state: ExecutionActivationState::Idle,
                        next_wake_at: first.approved_budget.deadline_at,
                        waiting_since: Some(Utc::now()),
                        ready_task_count: 0,
                        active_task_count: 0,
                    },
                    continuation_payload: None,
                    continuation_not_before_at: Utc::now(),
                },
            )
            .await?,
        RunControllerCompletionOutcome::Applied { .. }
    ));
    assert!(matches!(
        create_run_with_config(&repository, scope, &config, second()).await?,
        RunAdmissionOutcome::CapacitySaturated {
            dimension: ExecutionCapacityDimension::ParkedRuns
        }
    ));
    assert!(
        repository
            .load_run_by_idempotency_key(scope, tenant_id, None, "resident-entitlement-second",)
            .await?
            .is_none(),
        "joint entitlement saturation must roll back the run row"
    );
    Ok(())
}

#[tokio::test]
async fn scheduled_trigger_saturation_rolls_back_the_whole_run_admission_db() -> TestResult {
    // Pins: ActiveRuns, node rows, the immutable deadline, and initial activation are one
    // admission transaction; ScheduledTriggers saturation cannot leave any partial run evidence.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        max_fleet_active_runs: 2,
        max_tenant_active_runs: 2,
        max_fleet_scheduled_triggers: 1,
        max_tenant_scheduled_triggers: 1,
        ..ExecutionConfig::default()
    };

    assert!(matches!(
        create_run_with_config(
            &repository,
            scope,
            &config,
            two_node_run(tenant_id, "scheduled-trigger-first"),
        )
        .await?,
        RunAdmissionOutcome::Admitted(_)
    ));
    let saturated_key = "scheduled-trigger-saturated";
    assert!(matches!(
        create_run_with_config(
            &repository,
            scope,
            &config,
            two_node_run(tenant_id, saturated_key),
        )
        .await?,
        RunAdmissionOutcome::CapacitySaturated {
            dimension: ExecutionCapacityDimension::ScheduledTriggers
        }
    ));
    assert!(
        repository
            .load_run_by_idempotency_key(scope, tenant_id, None, saturated_key)
            .await?
            .is_none()
    );
    let counts: (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM moa.execution_run WHERE tenant_id = $1), \
           (SELECT count(*) FROM moa.execution_node_state WHERE tenant_id = $1), \
           (SELECT count(*) FROM moa.execution_trigger WHERE tenant_id = $1), \
           (SELECT count(*) FROM moa.execution_dispatch_outbox WHERE tenant_id = $1)",
    )
    .bind(tenant_id.0)
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (1, 2, 1, 2));
    let reserved: Vec<(String, i64)> = sqlx::query_as(
        "SELECT resource_dimension, sum(reserved_quantity)::BIGINT \
         FROM moa.execution_capacity_bucket \
         WHERE resource_dimension IN ('active_runs', 'scheduled_triggers') \
         GROUP BY resource_dimension ORDER BY resource_dimension",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        reserved,
        vec![
            ("active_runs".to_string(), 2),
            ("scheduled_triggers".to_string(), 2),
        ],
        "fleet and tenant buckets each retain only the first run's receipt"
    );
    Ok(())
}

#[tokio::test]
async fn terminal_finalization_releases_active_run_once_and_capacity_is_reusable_db() -> TestResult
{
    // Pins: successful terminal settlement releases the lifetime receipt and clears any stale
    // run-budget reservation before the terminal constraint fires; replay cannot underflow it.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        max_fleet_active_runs: 1,
        max_tenant_active_runs: 1,
        ..ExecutionConfig::default()
    };
    let RunAdmissionOutcome::Admitted(run) = create_run_with_config(
        &repository,
        scope,
        &config,
        new_run(
            tenant_id,
            None,
            "active-run-terminal",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?
    else {
        panic!("terminal fixture must be admitted");
    };
    let running = claim_running_controller(&repository, scope, &config, &run).await?;
    let RunDeadlineArmOutcome::Armed(deadline) = repository
        .arm_run_deadline(scope, run.run_uid, running.controller_generation, &config)
        .await?
    else {
        panic!("terminal fixture must arm a deadline trigger");
    };
    assert!(matches!(
        repository
            .claim_controller_wake(
                scope,
                running.run_uid,
                running.controller_generation,
                running.wake_epoch,
            )
            .await?,
        RunControllerClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        repository
            .drain_run_triggers_page(
                scope,
                &config,
                RunTriggerDrainRequest {
                    run_uid: running.run_uid,
                    controller_generation: running.controller_generation,
                    wake_epoch: running.wake_epoch,
                    page_limit: 1,
                    now: Utc::now(),
                },
            )
            .await?,
        RunTriggerDrainOutcome::ReadyToFinalize {
            drained_trigger_count: 1,
            ..
        }
    ));
    let stale_reservation = estimate(7);
    sqlx::query(
        "UPDATE moa.execution_run SET waiting_task_count = 1, waiting_timer_task_count = 1, \
         waiting_reasons_truncated = TRUE, waiting_since = NOW(), next_wake_at = $2, \
         reserved_cost_microusd = $3, reserved_tokens = $4, reserved_tasks = $5, \
         reserved_tool_calls = $6, reserved_retrieved_bytes = $7 \
         WHERE run_uid = $1",
    )
    .bind(running.run_uid)
    .bind(running.approved_budget.deadline_at)
    .bind(i64::try_from(stale_reservation.cost_microusd)?)
    .bind(i64::try_from(stale_reservation.tokens)?)
    .bind(i64::try_from(stale_reservation.tasks)?)
    .bind(i64::try_from(stale_reservation.tool_calls)?)
    .bind(i64::try_from(stale_reservation.retrieved_bytes)?)
    .execute(&pool)
    .await?;
    let prefinal = repository
        .load_run(scope, running.run_uid)
        .await?
        .expect("prefinal run");
    let prefinal_progress = execution_progress_from_run(&prefinal)?;
    assert_eq!(prefinal.reserved, stale_reservation);
    assert_eq!(prefinal_progress.parked_tasks, 1);
    assert_eq!(
        prefinal_progress.phase,
        ExecutionProgressPhase::WaitingTimer
    );
    assert_eq!(
        prefinal_progress.blocker_audience,
        Some(ExecutionBlockerAudience::System)
    );
    let request = successful_request(&prefinal)?;
    let FinalizationOutcome::Finalized(finalized) =
        repository.finalize_run(scope, request.clone()).await?
    else {
        panic!("drained run must finalize");
    };
    let terminal_progress = execution_progress_from_run(&finalized)?;
    assert_eq!(terminal_progress.parked_tasks, 0);
    assert_eq!(terminal_progress.blocker_audience, None);
    assert_eq!(
        finalized.reserved,
        ExecutionEstimate::default(),
        "terminal finalization must clear the stale ledger before its terminal constraint fires"
    );
    assert_eq!(finalized.waiting_task_count, 0);
    assert_eq!(finalized.waiting_timer_task_count, 0);
    assert!(!finalized.waiting_reasons_truncated);
    assert_eq!(finalized.waiting_since, None);
    assert_eq!(finalized.next_wake_at, None);
    assert_eq!(
        finalized.activation_state,
        ExecutionActivationState::Terminal
    );
    assert_eq!(finalized.processed_wake_epoch, prefinal.wake_epoch);
    assert_eq!(finalized.wake_epoch, prefinal.wake_epoch + 1);
    assert!(matches!(
        repository.finalize_run(scope, request).await?,
        FinalizationOutcome::Replayed(_)
    ));
    let bucket: (i64, i64) = sqlx::query_as(
        "SELECT reserved_quantity, limit_value FROM moa.execution_capacity_bucket \
         WHERE scope_kind = 'fleet' AND resource_dimension = 'active_runs'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        bucket,
        (0, 1),
        "terminal replay must not underflow capacity"
    );
    let tenant_bucket: (i64, i64) = sqlx::query_as(
        "SELECT reserved_quantity, limit_value FROM moa.execution_capacity_bucket \
         WHERE scope_kind = 'tenant' AND tenant_id = $1 \
           AND resource_dimension = 'active_runs'",
    )
    .bind(tenant_id.0)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        tenant_bucket,
        (0, 1),
        "terminal replay must not underflow tenant capacity"
    );
    let receipt_state: String = sqlx::query_scalar(
        "SELECT state FROM moa.execution_capacity_reservation \
         WHERE run_uid = $1 AND resource_dimension = 'active_runs'",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(receipt_state, "released");
    let trigger_boundary: (String, String, String) = sqlx::query_as(
        "SELECT trigger.state, dispatch.state, capacity.state \
         FROM moa.execution_trigger AS trigger \
         JOIN moa.execution_dispatch_outbox AS dispatch USING (trigger_uid) \
         JOIN moa.execution_capacity_reservation AS capacity USING (trigger_uid) \
         WHERE trigger.trigger_uid = $1",
    )
    .bind(deadline.trigger.trigger_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        trigger_boundary,
        (
            "superseded".to_string(),
            "cancelled".to_string(),
            "released".to_string(),
        ),
        "terminal settlement must retire its delayed deadline and capacity receipt"
    );
    let scheduled_buckets: Vec<(String, i64)> = sqlx::query_as(
        "SELECT scope_kind, reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE resource_dimension = 'scheduled_triggers' ORDER BY scope_kind",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        scheduled_buckets,
        vec![("fleet".to_string(), 0), ("tenant".to_string(), 0)]
    );

    assert!(matches!(
        create_run_with_config(
            &repository,
            scope,
            &config,
            new_run(
                tenant_id,
                None,
                "active-run-after-terminal",
                ExecutionRunStatus::Queued,
                budget(1),
            ),
        )
        .await?,
        RunAdmissionOutcome::Admitted(_)
    ));
    Ok(())
}

#[tokio::test]
async fn concurrent_deadline_arm_and_terminal_finalization_use_scheduled_before_run_lock_db()
-> TestResult {
    // Pins: public deadline reconciliation and terminal settlement both acquire
    // ScheduledTriggers before the run row. Their race completes without deadlock and commits
    // exactly one coherent winner: either terminal state with no trigger or a live rearm that
    // keeps finalization not-ready.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig::default();
    let RunAdmissionOutcome::Admitted(run) = create_run_with_config(
        &repository,
        scope,
        &config,
        new_run(
            tenant_id,
            None,
            "deadline-arm-terminal-lock-order",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?
    else {
        panic!("deadline race fixture must be admitted");
    };
    let running = claim_running_controller(&repository, scope, &config, &run).await?;
    assert!(matches!(
        repository
            .arm_run_deadline(
                scope,
                running.run_uid,
                running.controller_generation,
                &config,
            )
            .await?,
        RunDeadlineArmOutcome::Armed(_)
    ));
    assert!(matches!(
        repository
            .claim_controller_wake(
                scope,
                running.run_uid,
                running.controller_generation,
                running.wake_epoch,
            )
            .await?,
        RunControllerClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        repository
            .drain_run_triggers_page(
                scope,
                &config,
                RunTriggerDrainRequest {
                    run_uid: running.run_uid,
                    controller_generation: running.controller_generation,
                    wake_epoch: running.wake_epoch,
                    page_limit: 1,
                    now: Utc::now(),
                },
            )
            .await?,
        RunTriggerDrainOutcome::ReadyToFinalize { .. }
    ));
    let prefinal = repository
        .load_run(scope, running.run_uid)
        .await?
        .expect("drained race fixture remains visible");
    let finalization = successful_request(&prefinal)?;
    let arm_repository = repository.clone();
    let terminal_repository = repository.clone();
    let arm_config = config.clone();
    let (arm, terminal) = tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        tokio::join!(
            arm_repository.arm_run_deadline(
                scope,
                prefinal.run_uid,
                prefinal.controller_generation,
                &arm_config,
            ),
            terminal_repository.finalize_run(scope, finalization),
        )
    })
    .await
    .expect("ScheduledTriggers-before-run ordering must not deadlock");
    match (arm?, terminal?) {
        (RunDeadlineArmOutcome::Terminal, FinalizationOutcome::Finalized(_))
        | (RunDeadlineArmOutcome::Armed(_), FinalizationOutcome::Conflict) => {}
        outcomes => {
            panic!("deadline arm/terminal race committed incoherent outcomes: {outcomes:?}")
        }
    }
    let persisted = repository
        .load_run(scope, running.run_uid)
        .await?
        .expect("race fixture remains queryable");
    let active_trigger_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_trigger \
         WHERE run_uid=$1 AND trigger_kind='run_deadline' \
           AND state IN ('pending','dispatching')",
    )
    .bind(running.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(
        active_trigger_count,
        if persisted.status.is_terminal() { 0 } else { 1 }
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_resume_and_terminal_release_preserve_capacity_lock_order_db() -> TestResult {
    // Pins: canonical parked-run resume transfers ParkedRuns to ActiveRuns before concurrent
    // terminal settlement releases the current owner and ScheduledTriggers, without deadlock,
    // leaked receipts, or counter underflow.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = ExecutionConfig {
        max_fleet_active_runs: 2,
        max_tenant_active_runs: 1,
        max_fleet_parked_runs: 2,
        max_tenant_parked_runs: 1,
        max_fleet_scheduled_triggers: 2,
        max_tenant_scheduled_triggers: 1,
        ..ExecutionConfig::default()
    };
    let mut settlements = Vec::new();
    for (tenant_id, key) in [
        (TenantId::new(), "terminal-lock-order-left"),
        (TenantId::new(), "terminal-lock-order-right"),
    ] {
        let scope = ExecutionScope::Tenant { tenant_id };
        let RunAdmissionOutcome::Admitted(run) = create_run_with_config(
            &repository,
            scope,
            &config,
            new_run(tenant_id, None, key, ExecutionRunStatus::Queued, budget(1)),
        )
        .await?
        else {
            panic!("concurrency fixture must be admitted");
        };
        assert!(matches!(
            repository
                .claim_controller_wake(
                    scope,
                    run.run_uid,
                    run.controller_generation,
                    run.wake_epoch,
                )
                .await?,
            RunControllerClaimOutcome::Claimed(_)
        ));
        let next_wake_at = run
            .approved_budget
            .deadline_at
            .expect("test budget has a deadline");
        let RunControllerCompletionOutcome::Applied { run: parked, .. } = repository
            .complete_controller_wake(
                scope,
                &config,
                run.run_uid,
                RunControllerCompletionRequest {
                    controller_generation: run.controller_generation,
                    wake_epoch: run.wake_epoch,
                    checkpoint: ExecutionRunActivationCheckpoint {
                        status: ExecutionRunStatus::WaitingInput,
                        activation_state: ExecutionActivationState::Idle,
                        next_wake_at: Some(next_wake_at),
                        waiting_since: Some(Utc::now()),
                        ready_task_count: 0,
                        active_task_count: 0,
                    },
                    continuation_payload: None,
                    continuation_not_before_at: Utc::now(),
                },
            )
            .await?
        else {
            panic!("storage-only wait must reserve ParkedRuns");
        };
        let TransitionOutcome::RunApplied(paused) = repository
            .pause_run(scope, &config, parked.run_uid, parked.controller_generation)
            .await?
        else {
            panic!("storage-only fixture must enter a canonical paused state");
        };
        let TransitionOutcome::RunApplied(resumed) = repository
            .resume_run(scope, &config, paused.run_uid, paused.controller_generation)
            .await?
        else {
            panic!("paused fixture must enqueue one canonical resume activation");
        };
        let running = match repository
            .claim_controller_wake(
                scope,
                resumed.run_uid,
                resumed.controller_generation,
                resumed.wake_epoch,
            )
            .await?
        {
            RunControllerClaimOutcome::Claimed(running) => running,
            outcome => panic!("resumed fixture wake must be claimable: {outcome:?}"),
        };
        assert!(matches!(
            repository
                .arm_run_deadline(
                    scope,
                    running.run_uid,
                    running.controller_generation,
                    &config,
                )
                .await?,
            RunDeadlineArmOutcome::Armed(_)
        ));
        assert!(matches!(
            repository
                .drain_run_triggers_page(
                    scope,
                    &config,
                    RunTriggerDrainRequest {
                        run_uid: running.run_uid,
                        controller_generation: running.controller_generation,
                        wake_epoch: running.wake_epoch,
                        page_limit: 1,
                        now: Utc::now(),
                    },
                )
                .await?,
            RunTriggerDrainOutcome::ReadyToFinalize { .. }
        ));
        settlements.push((scope, successful_request(&running)?));
    }
    let left_repository = repository.clone();
    let right_repository = repository.clone();
    let (left_scope, left_request) = settlements.remove(0);
    let (right_scope, right_request) = settlements.remove(0);
    let (left, right) = tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        tokio::join!(
            left_repository.finalize_run(left_scope, left_request),
            right_repository.finalize_run(right_scope, right_request),
        )
    })
    .await
    .expect("canonical capacity locking must not deadlock");
    assert!(matches!(left?, FinalizationOutcome::Finalized(_)));
    assert!(matches!(right?, FinalizationOutcome::Finalized(_)));
    let leaked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_capacity_reservation \
         WHERE state IN ('reserved', 'reconciling') AND resource_dimension IN \
           ('active_runs', 'parked_runs', 'scheduled_triggers')",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(leaked, 0);
    let nonzero_buckets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM moa.execution_capacity_bucket \
         WHERE resource_dimension IN ('active_runs', 'parked_runs', 'scheduled_triggers') \
           AND reserved_quantity <> 0",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(nonzero_buckets, 0);
    Ok(())
}

#[tokio::test]
async fn parked_to_active_transfer_saturates_atomically_and_replays_without_dual_ownership_db()
-> TestResult {
    // Pins: storage-only waits and pause do not retain ActiveRuns; a saturated resume rolls back
    // its generation/wake/capacity changes, and the eventual exact replay cannot reserve both
    // ActiveRuns and ParkedRuns or underflow either bucket.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        max_fleet_active_runs: 1,
        max_tenant_active_runs: 1,
        max_fleet_parked_runs: 2,
        max_tenant_parked_runs: 2,
        ..ExecutionConfig::default()
    };

    let RunAdmissionOutcome::Admitted(first) = create_run_with_config(
        &repository,
        scope,
        &config,
        new_run(
            tenant_id,
            None,
            "parked-active-transfer-first",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?
    else {
        panic!("first run must consume the only ActiveRuns slot");
    };
    assert!(matches!(
        repository
            .claim_controller_wake(
                scope,
                first.run_uid,
                first.controller_generation,
                first.wake_epoch,
            )
            .await?,
        RunControllerClaimOutcome::Claimed(_)
    ));
    let RunControllerCompletionOutcome::Applied {
        run: storage_parked,
        ..
    } = repository
        .complete_controller_wake(
            scope,
            &config,
            first.run_uid,
            RunControllerCompletionRequest {
                controller_generation: first.controller_generation,
                wake_epoch: first.wake_epoch,
                checkpoint: ExecutionRunActivationCheckpoint {
                    status: ExecutionRunStatus::WaitingInput,
                    activation_state: ExecutionActivationState::Idle,
                    next_wake_at: first.approved_budget.deadline_at,
                    waiting_since: Some(Utc::now()),
                    ready_task_count: 0,
                    active_task_count: 0,
                },
                continuation_payload: None,
                continuation_not_before_at: Utc::now(),
            },
        )
        .await?
    else {
        panic!("storage-only checkpoint must park the first run");
    };
    let TransitionOutcome::RunApplied(paused_first) = repository
        .pause_run(
            scope,
            &config,
            storage_parked.run_uid,
            storage_parked.controller_generation,
        )
        .await?
    else {
        panic!("parked run must enter paused state");
    };

    let RunAdmissionOutcome::Admitted(second) = create_run_with_config(
        &repository,
        scope,
        &config,
        new_run(
            tenant_id,
            None,
            "parked-active-transfer-second",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?
    else {
        panic!("parking the first run must free the only ActiveRuns slot");
    };
    let saturation = repository
        .resume_run(
            scope,
            &config,
            paused_first.run_uid,
            paused_first.controller_generation,
        )
        .await
        .expect_err("resume must defer while the only ActiveRuns slot is occupied");
    assert!(matches!(
        saturation,
        moa_execution::Error::CapacitySaturated {
            dimension: "active_runs"
        }
    ));
    let unchanged = repository
        .load_run(scope, paused_first.run_uid)
        .await?
        .expect("saturated run remains visible");
    assert_eq!(unchanged.status, ExecutionRunStatus::Paused);
    assert_eq!(
        (unchanged.controller_generation, unchanged.wake_epoch),
        (paused_first.controller_generation, paused_first.wake_epoch),
        "capacity saturation must roll back the exact resume fence"
    );
    let before_retry: Vec<(String, i64)> = sqlx::query_as(
        "SELECT resource_dimension,reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE scope_kind='tenant' AND tenant_id=$1 \
           AND resource_dimension IN ('active_runs','parked_runs') \
         ORDER BY resource_dimension",
    )
    .bind(tenant_id.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        before_retry,
        vec![
            ("active_runs".to_string(), 1),
            ("parked_runs".to_string(), 1)
        ]
    );

    let TransitionOutcome::RunApplied(paused_second) = repository
        .pause_run(scope, &config, second.run_uid, second.controller_generation)
        .await?
    else {
        panic!("second run must transfer its ActiveRuns slot to ParkedRuns");
    };
    let TransitionOutcome::RunApplied(resumed_first) = repository
        .resume_run(
            scope,
            &config,
            paused_first.run_uid,
            paused_first.controller_generation,
        )
        .await?
    else {
        panic!("first run must resume after the ActiveRuns slot is released");
    };
    assert!(matches!(
        repository
            .resume_run(
                scope,
                &config,
                paused_first.run_uid,
                paused_first.controller_generation,
            )
            .await?,
        TransitionOutcome::RunAlreadyApplied(ref replayed)
            if replayed.controller_generation == resumed_first.controller_generation
    ));
    let active_receipts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT run_uid,resource_dimension FROM moa.execution_capacity_reservation \
         WHERE tenant_id=$1 AND run_uid IN ($2,$3) AND state IN ('reserved','reconciling') \
           AND resource_dimension IN ('active_runs','parked_runs') ORDER BY run_uid",
    )
    .bind(tenant_id.0)
    .bind(resumed_first.run_uid)
    .bind(paused_second.run_uid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(active_receipts.len(), 2);
    assert!(active_receipts.contains(&(resumed_first.run_uid, "active_runs".to_string())));
    assert!(active_receipts.contains(&(paused_second.run_uid, "parked_runs".to_string())));
    let after_retry: Vec<(String, i64)> = sqlx::query_as(
        "SELECT resource_dimension,reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE scope_kind='tenant' AND tenant_id=$1 \
           AND resource_dimension IN ('active_runs','parked_runs') \
         ORDER BY resource_dimension",
    )
    .bind(tenant_id.0)
    .fetch_all(&pool)
    .await?;
    assert_eq!(after_retry, before_retry);
    Ok(())
}

#[tokio::test]
async fn deadline_rearm_releases_superseded_trigger_capacity_before_reserving_replacement_db()
-> TestResult {
    // Pins: rearming a run deadline for a new controller generation settles the old trigger
    // through its owning path, so a one-slot ScheduledTriggers budget admits the replacement.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        max_fleet_scheduled_triggers: 1,
        max_tenant_scheduled_triggers: 1,
        ..ExecutionConfig::default()
    };
    let RunAdmissionOutcome::Admitted(run) = create_run_with_config(
        &repository,
        scope,
        &config,
        new_run(
            tenant_id,
            None,
            "deadline-rearm-capacity",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?
    else {
        panic!("deadline fixture must be admitted");
    };
    let RunDeadlineArmOutcome::Armed(first) = repository
        .arm_run_deadline(scope, run.run_uid, 1, &config)
        .await?
    else {
        panic!("first generation must arm its deadline");
    };
    sqlx::query(
        "UPDATE moa.execution_run \
         SET controller_generation = 2, updated_at = NOW() WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(&pool)
    .await?;
    let RunDeadlineArmOutcome::Armed(second) = repository
        .arm_run_deadline(scope, run.run_uid, 2, &config)
        .await?
    else {
        panic!("replacement generation must reuse the released trigger slot");
    };
    assert_ne!(first.trigger.trigger_uid, second.trigger.trigger_uid);
    let states: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT trigger_uid, state FROM moa.execution_trigger \
         WHERE run_uid = $1 AND trigger_kind = 'run_deadline' ORDER BY controller_generation",
    )
    .bind(run.run_uid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        states,
        vec![
            (first.trigger.trigger_uid, "superseded".to_string()),
            (second.trigger.trigger_uid, "pending".to_string()),
        ]
    );
    let dispatch_states: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT trigger_uid, state FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND trigger_uid IS NOT NULL ORDER BY controller_generation",
    )
    .bind(run.run_uid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        dispatch_states,
        vec![
            (first.trigger.trigger_uid, "cancelled".to_string()),
            (second.trigger.trigger_uid, "pending".to_string()),
        ]
    );
    let receipts: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT trigger_uid, state FROM moa.execution_capacity_reservation \
         WHERE run_uid = $1 AND resource_dimension = 'scheduled_triggers' \
         ORDER BY controller_generation",
    )
    .bind(run.run_uid)
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        receipts,
        vec![
            (first.trigger.trigger_uid, "released".to_string()),
            (second.trigger.trigger_uid, "reserved".to_string()),
        ]
    );
    let buckets: Vec<(String, i64)> = sqlx::query_as(
        "SELECT scope_kind, reserved_quantity FROM moa.execution_capacity_bucket \
         WHERE resource_dimension = 'scheduled_triggers' ORDER BY scope_kind",
    )
    .fetch_all(&pool)
    .await?;
    assert_eq!(
        buckets,
        vec![("fleet".to_string(), 1), ("tenant".to_string(), 1)]
    );
    Ok(())
}

#[tokio::test]
async fn high_fanout_terminal_trigger_cleanup_is_strictly_activation_bounded_db() -> TestResult {
    // Pins: terminal trigger cancellation settles no more than the requested page in one
    // activation, durably queues exactly one continuation while work remains, and releases every
    // delayed outbox/capacity receipt without an unbounded finalizer scan.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let config = ExecutionConfig {
        max_fleet_scheduled_triggers: 16,
        max_tenant_scheduled_triggers: 16,
        ..ExecutionConfig::default()
    };
    let RunAdmissionOutcome::Admitted(run) = create_run_with_config(
        &repository,
        scope,
        &config,
        new_run(
            tenant_id,
            None,
            "bounded-terminal-trigger-drain",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?
    else {
        panic!("trigger-drain fixture must be admitted");
    };
    let mut run = *run;
    let due_at = run.approved_budget.deadline_at.expect("fixture deadline");
    for controller_generation in 2..=6 {
        repository
            .create_trigger(
                scope,
                &config,
                NewExecutionTrigger {
                    trigger_uid: Uuid::now_v7(),
                    tenant_id,
                    run_uid: Some(run.run_uid),
                    task_id: None,
                    compensation_id: None,
                    schedule_uid: None,
                    kind: ExecutionTriggerKind::RunDeadline,
                    controller_generation: Some(controller_generation),
                    attempt_generation: None,
                    compensation_generation: None,
                    compensation_attempt_generation: None,
                    schedule_incarnation: None,
                    occurrence_sequence: None,
                    due_at,
                    payload: json!({"run_uid": run.run_uid, "deadline_at": due_at}),
                },
            )
            .await?;
    }

    for expected_remaining in [4_i64, 2] {
        assert!(matches!(
            repository
                .claim_controller_wake(
                    scope,
                    run.run_uid,
                    run.controller_generation,
                    run.wake_epoch,
                )
                .await?,
            RunControllerClaimOutcome::Claimed(_)
        ));
        let RunTriggerDrainOutcome::PageDrained(commit) = repository
            .drain_run_triggers_page(
                scope,
                &config,
                RunTriggerDrainRequest {
                    run_uid: run.run_uid,
                    controller_generation: run.controller_generation,
                    wake_epoch: run.wake_epoch,
                    page_limit: 2,
                    now: Utc::now(),
                },
            )
            .await?
        else {
            panic!("non-final trigger page must durably enqueue one continuation");
        };
        assert_eq!(commit.drained_trigger_count, 2);
        assert_eq!(commit.continuation.wake_epoch, Some(commit.run.wake_epoch));
        run = commit.run;
        let active: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM moa.execution_trigger \
             WHERE run_uid = $1 AND state IN ('pending', 'dispatching')",
        )
        .bind(run.run_uid)
        .fetch_one(&pool)
        .await?;
        assert_eq!(active, expected_remaining);
    }
    assert!(matches!(
        repository
            .claim_controller_wake(
                scope,
                run.run_uid,
                run.controller_generation,
                run.wake_epoch,
            )
            .await?,
        RunControllerClaimOutcome::Claimed(_)
    ));
    assert!(matches!(
        repository
            .drain_run_triggers_page(
                scope,
                &config,
                RunTriggerDrainRequest {
                    run_uid: run.run_uid,
                    controller_generation: run.controller_generation,
                    wake_epoch: run.wake_epoch,
                    page_limit: 2,
                    now: Utc::now(),
                },
            )
            .await?,
        RunTriggerDrainOutcome::ReadyToFinalize {
            drained_trigger_count: 2,
            ..
        }
    ));
    let terminal_boundary: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
           (SELECT count(*) FROM moa.execution_trigger \
             WHERE run_uid = $1 AND state <> 'superseded'), \
           (SELECT count(*) FROM moa.execution_dispatch_outbox \
             WHERE run_uid = $1 AND trigger_uid IS NOT NULL AND state <> 'cancelled'), \
           (SELECT count(*) FROM moa.execution_capacity_reservation \
             WHERE run_uid = $1 AND resource_dimension = 'scheduled_triggers' \
               AND state <> 'released')",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(terminal_boundary, (0, 0, 0));
    let scheduled_reserved: i64 = sqlx::query_scalar(
        "SELECT sum(reserved_quantity)::BIGINT FROM moa.execution_capacity_bucket \
         WHERE resource_dimension = 'scheduled_triggers'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(scheduled_reserved, 0);
    Ok(())
}
