//! Durable controller wake claim/complete compare-and-set and crashed-activation recovery.

use moa_execution::{
    repository::run::{ResumedControllerRecoveryOutcome, ResumedControllerRecoveryRequest},
    repository::terminal::{PendingTerminalAdvanceOutcome, PendingTerminalAdvanceStage},
    state::ExecutionTerminalEvidence,
};

use super::support::*;

/// Admits one queued run whose current wake is claimable.
async fn queued_run(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    tenant_id: TenantId,
    key: &str,
) -> Result<ExecutionRunRecord, Box<dyn std::error::Error + Send + Sync>> {
    Ok(create_run(
        repository,
        scope,
        new_run(tenant_id, None, key, ExecutionRunStatus::Queued, budget(10)),
    )
    .await?)
}

/// Builds the bounded continuation checkpoint the controller commits for a crashed activation.
fn continuation_checkpoint(
    run: &ExecutionRunRecord,
    status: ExecutionRunStatus,
) -> ExecutionRunActivationCheckpoint {
    ExecutionRunActivationCheckpoint {
        status,
        activation_state: ExecutionActivationState::Queued,
        next_wake_at: run.next_wake_at,
        waiting_since: run.waiting_since,
        ready_task_count: run.ready_task_count,
        active_task_count: run.active_task_count,
    }
}

/// Builds the recovery request the controller issues for one resumed wake.
fn recovery_request(
    run: &ExecutionRunRecord,
    status: ExecutionRunStatus,
    maximum_consecutive_failures: u64,
) -> ResumedControllerRecoveryRequest {
    ResumedControllerRecoveryRequest {
        controller_generation: run.controller_generation,
        wake_epoch: run.wake_epoch,
        checkpoint: continuation_checkpoint(run, status),
        continuation_payload: json!({"cause": "resumed_activation_recovery"}),
        continuation_not_before_at: Utc::now(),
        maximum_consecutive_failures,
    }
}

/// Reads the durable activation-failure budget consumed by one run.
async fn activation_failure_count(pool: &sqlx::PgPool, run_uid: Uuid) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT activation_failure_count FROM moa.execution_run WHERE run_uid=$1")
        .bind(run_uid)
        .fetch_one(pool)
        .await
}

/// Counts the run-activation outbox rows owning one exact wake epoch.
async fn run_activation_rows(
    pool: &sqlx::PgPool,
    run_uid: Uuid,
    wake_epoch: i64,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_dispatch_outbox \
         WHERE run_uid=$1 AND dispatch_kind='run_activation' AND wake_epoch=$2",
    )
    .bind(run_uid)
    .bind(wake_epoch)
    .fetch_one(pool)
    .await
}

#[tokio::test]
async fn claim_controller_wake_replays_an_already_processed_wake_db() -> TestResult {
    // Pins: the acknowledgement fence, not the caller, decides whether a redelivered activation
    // may run again. A wake whose epoch is already acknowledged must resolve as a replay and must
    // leave the durable activation state untouched, so a duplicate delivery of the same dispatch
    // can never re-enter the bounded scheduler work that wake already committed.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = queued_run(&repository, scope, tenant_id, "controller-replayed-wake").await?;

    assert_eq!(
        sqlx::query(
            "UPDATE moa.execution_run SET processed_wake_epoch = wake_epoch WHERE run_uid=$1"
        )
        .bind(run.run_uid)
        .execute(&pool)
        .await?
        .rows_affected(),
        1
    );

    let outcome = repository
        .claim_controller_wake(
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
        )
        .await?;

    assert!(
        matches!(outcome, RunControllerClaimOutcome::Replayed(_)),
        "an acknowledged wake must replay, got {outcome:?}"
    );
    let activation_state: String =
        sqlx::query_scalar("SELECT activation_state FROM moa.execution_run WHERE run_uid=$1")
            .bind(run.run_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(
        activation_state, "queued",
        "a replayed claim must not advance the durable activation state"
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_controller_wake_claims_admit_exactly_one_activation_db() -> TestResult {
    // Pins: the claim is a real compare-and-set under the run row lock, not a read-then-write.
    // Two deliveries racing for the same exact wake must produce exactly one Claimed activation;
    // the loser must observe Resumed, which is the signal that a prior activation already owns the
    // wake and that recovery — not a second bounded page — is the only legal continuation.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = queued_run(&repository, scope, tenant_id, "controller-claim-race").await?;

    let (left, right) = tokio::join!(
        repository.claim_controller_wake(
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
        ),
        repository.claim_controller_wake(
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
        ),
    );
    let outcomes = [left?, right?];

    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, RunControllerClaimOutcome::Claimed(_)))
            .count(),
        1,
        "exactly one racing delivery may claim the wake, got {outcomes:?}"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, RunControllerClaimOutcome::Resumed(_)))
            .count(),
        1,
        "the losing delivery must observe the in-flight activation, got {outcomes:?}"
    );
    Ok(())
}

#[tokio::test]
async fn claim_controller_wake_refuses_an_unqueued_activation_state_db() -> TestResult {
    // Pins: the claim predicate requires an actually queued activation. A pending wake epoch on a
    // run whose activation state says no activation is queued is inconsistent durable state, and
    // claiming it would start bounded work the scheduler never admitted.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = queued_run(&repository, scope, tenant_id, "controller-unqueued-claim").await?;

    assert_eq!(
        sqlx::query(
            "UPDATE moa.execution_run SET activation_state='idle', wake_epoch = wake_epoch + 1 \
             WHERE run_uid=$1",
        )
        .bind(run.run_uid)
        .execute(&pool)
        .await?
        .rows_affected(),
        1
    );

    assert_eq!(
        repository
            .claim_controller_wake(
                scope,
                run.run_uid,
                run.controller_generation,
                run.wake_epoch + 1,
            )
            .await?,
        RunControllerClaimOutcome::InvalidState
    );
    Ok(())
}

#[tokio::test]
async fn resumed_recovery_enqueues_exactly_one_replacement_activation_db() -> TestResult {
    // Pins: recovering a crashed activation acknowledges the claimed wake exactly once, mints
    // exactly one successor wake, and charges exactly one unit of the bounded failure budget.
    // A recovery that enqueued zero successors would strand the run; one that enqueued two, or
    // that skipped an epoch, would let concurrent activations race the same durable scheduler
    // state; one that forgot to charge the budget would restore the unbounded stall this exists
    // to end.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = ExecutionConfig::default();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = queued_run(
        &repository,
        scope,
        tenant_id,
        "controller-recovery-successor",
    )
    .await?;
    let RunControllerClaimOutcome::Claimed(claimed) = repository
        .claim_controller_wake(
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
        )
        .await?
    else {
        panic!("the admitted queued wake must be claimable");
    };

    let outcome = repository
        .recover_resumed_controller_wake(
            scope,
            &config,
            claimed.run_uid,
            recovery_request(&claimed, ExecutionRunStatus::Running, 5),
        )
        .await?;

    let ResumedControllerRecoveryOutcome::Recovered {
        run: recovered,
        continuation,
        consecutive_failures,
    } = outcome
    else {
        panic!("a first crashed activation must recover, got {outcome:?}");
    };
    assert_eq!(consecutive_failures, 1);
    assert_eq!(recovered.wake_epoch, claimed.wake_epoch + 1);
    assert_eq!(recovered.processed_wake_epoch, claimed.wake_epoch);
    assert_eq!(
        recovered.controller_generation,
        claimed.controller_generation
    );
    assert_eq!(recovered.activation_state, ExecutionActivationState::Queued);
    assert_eq!(continuation.wake_epoch, Some(recovered.wake_epoch));
    assert_eq!(continuation.run_uid, Some(recovered.run_uid));
    assert_eq!(
        run_activation_rows(&pool, run.run_uid, i64::try_from(recovered.wake_epoch)?).await?,
        1,
        "recovery must own exactly one successor activation"
    );
    assert_eq!(activation_failure_count(&pool, run.run_uid).await?, 1);
    Ok(())
}

#[tokio::test]
async fn resumed_recovery_fails_the_run_once_its_budget_is_exhausted_db() -> TestResult {
    // Pins: a deterministically failing activation stops being re-enqueued. On exhaustion the
    // recovery must acknowledge nothing — the wake stays current and unacknowledged — precisely so
    // the caller can fence an explicit terminal intent against that same wake in one transaction.
    // If exhaustion acknowledged the wake, the terminal fence would see a replayed epoch, decline
    // to install the intent, and the run would park forever with no product failure: the exact
    // permanent stall this budget exists to end.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = ExecutionConfig::default();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = queued_run(
        &repository,
        scope,
        tenant_id,
        "controller-recovery-exhausted",
    )
    .await?;
    let RunControllerClaimOutcome::Claimed(claimed) = repository
        .claim_controller_wake(
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
        )
        .await?
    else {
        panic!("the admitted queued wake must be claimable");
    };
    assert_eq!(
        sqlx::query("UPDATE moa.execution_run SET activation_failure_count = 5 WHERE run_uid=$1")
            .bind(run.run_uid)
            .execute(&pool)
            .await?
            .rows_affected(),
        1
    );

    let outcome = repository
        .recover_resumed_controller_wake(
            scope,
            &config,
            claimed.run_uid,
            recovery_request(&claimed, ExecutionRunStatus::Running, 5),
        )
        .await?;

    assert_eq!(
        outcome,
        ResumedControllerRecoveryOutcome::BudgetExhausted {
            consecutive_failures: 6
        }
    );
    let stalled = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("the exhausted run must remain visible");
    assert_eq!(
        stalled.processed_wake_epoch, claimed.processed_wake_epoch,
        "exhaustion must not acknowledge the claimed wake"
    );
    assert_eq!(stalled.wake_epoch, claimed.wake_epoch);
    assert_eq!(
        stalled.activation_state,
        ExecutionActivationState::Advancing
    );
    assert_eq!(
        run_activation_rows(&pool, run.run_uid, i64::try_from(claimed.wake_epoch + 1)?).await?,
        0,
        "exhaustion must not enqueue another activation"
    );

    // The unacknowledged wake is exactly what lets the caller commit its explicit terminal intent.
    let fenced = repository
        .fence_completion_terminal_and_enqueue_settlement(
            &config,
            scope,
            run.run_uid,
            claimed.controller_generation,
            claimed.wake_epoch,
            PendingExecutionTerminal {
                status: ExecutionRunStatus::Failed,
                reason: ExecutionTerminalReason::InternalFailure,
                terminal_evidence: ExecutionTerminalEvidence {
                    cause: ExecutionTerminalCause::InternalFailure,
                    satisfied_requirement_count: 0,
                    requirement_count: 0,
                },
                completion_check_results: Vec::new(),
                terminal_gaps: vec![
                    "controller activation failed 6 consecutive times and requires manual repair"
                        .to_string(),
                ],
                output: None,
                cancellation_reason: None,
            },
            moa_test_support::fixtures::pg_now(),
            1,
        )
        .await?;

    let PendingTerminalAdvanceOutcome::Applied(commit) = fenced else {
        panic!("the terminal intent must commit against the still-current wake, got {fenced:?}");
    };
    assert_eq!(
        commit.stage,
        PendingTerminalAdvanceStage::Finalized,
        "a wedged run with no outstanding work must finalize in its first bounded page"
    );
    assert_eq!(commit.run.status, ExecutionRunStatus::Failed);
    assert_eq!(
        activation_failure_count(&pool, run.run_uid).await?,
        0,
        "successful pending-terminal wake settlement resets the consecutive-failure budget"
    );
    assert_eq!(
        commit.run.terminal_reason,
        Some(ExecutionTerminalReason::InternalFailure)
    );
    let (status, terminal_reason): (String, Option<String>) =
        sqlx::query_as("SELECT status, terminal_reason FROM moa.execution_run WHERE run_uid=$1")
            .bind(run.run_uid)
            .fetch_one(&pool)
            .await?;
    assert_eq!(status, "failed");
    assert_eq!(
        terminal_reason.as_deref(),
        Some("internal_failure"),
        "the exhausted activation must surface an explicit durable product failure"
    );
    Ok(())
}

#[tokio::test]
async fn acknowledged_wake_resets_the_activation_failure_budget_db() -> TestResult {
    // Pins: the budget counts *consecutive* crashes. One activation that reaches its checkpoint
    // proves the run is not deterministically wedged, so the budget must return to zero. A counter
    // that only ever accumulated would eventually fail a healthy long-running run for repair.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = ExecutionConfig::default();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = queued_run(&repository, scope, tenant_id, "controller-budget-reset").await?;
    let RunControllerClaimOutcome::Claimed(claimed) = repository
        .claim_controller_wake(
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
        )
        .await?
    else {
        panic!("the admitted queued wake must be claimable");
    };
    let ResumedControllerRecoveryOutcome::Recovered { run: recovered, .. } = repository
        .recover_resumed_controller_wake(
            scope,
            &config,
            claimed.run_uid,
            recovery_request(&claimed, ExecutionRunStatus::Running, 5),
        )
        .await?
    else {
        panic!("a first crashed activation must recover");
    };
    assert_eq!(activation_failure_count(&pool, run.run_uid).await?, 1);
    let RunControllerClaimOutcome::Claimed(successor) = repository
        .claim_controller_wake(
            scope,
            recovered.run_uid,
            recovered.controller_generation,
            recovered.wake_epoch,
        )
        .await?
    else {
        panic!("the replacement wake must be claimable");
    };

    let completed = repository
        .complete_controller_wake(
            scope,
            &config,
            successor.run_uid,
            RunControllerCompletionRequest {
                controller_generation: successor.controller_generation,
                wake_epoch: successor.wake_epoch,
                checkpoint: ExecutionRunActivationCheckpoint {
                    status: ExecutionRunStatus::Running,
                    activation_state: ExecutionActivationState::Idle,
                    next_wake_at: successor.next_wake_at,
                    waiting_since: None,
                    ready_task_count: 0,
                    active_task_count: 0,
                },
                continuation_payload: None,
                continuation_not_before_at: Utc::now(),
            },
        )
        .await?;

    assert!(
        matches!(completed, RunControllerCompletionOutcome::Applied { .. }),
        "the replacement activation must reach its checkpoint, got {completed:?}"
    );
    assert_eq!(
        activation_failure_count(&pool, run.run_uid).await?,
        0,
        "an acknowledged wake must clear the consecutive-failure budget"
    );
    Ok(())
}

#[tokio::test]
async fn resumed_recovery_keeps_a_compensating_run_compensating_db() -> TestResult {
    // Pins: a controller crash while draining a compensating run must preserve the run phase.
    // `trigger_is_current` resolves a CompensationWatchdog only while its run row still satisfies
    // `status='compensating' AND controller_generation=<trigger generation>`; a continuation that
    // rewrote either conjunct would make the in-flight watchdog non-current and let
    // `prepare_watchdog_trigger` supersede it, permanently disarming ambiguity resolution for that
    // compensation attempt. The second half pins why preserving it is mandatory rather than
    // cosmetic: the durable transition table has no `compensating -> running` edge at all, so the
    // rewrite does not quietly degrade the phase — it aborts the whole recovery transaction.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = ExecutionConfig::default();
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = queued_run(
        &repository,
        scope,
        tenant_id,
        "controller-recovery-compensating",
    )
    .await?;
    set_run_status_path(&pool, run.run_uid, &["running", "compensating"]).await?;
    let compensating = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("the compensating fixture run must be visible");
    assert_eq!(compensating.status, ExecutionRunStatus::Compensating);
    assert!(compensating.pending_terminal.is_some());
    let RunControllerClaimOutcome::Claimed(claimed) = repository
        .claim_controller_wake(
            scope,
            compensating.run_uid,
            compensating.controller_generation,
            compensating.wake_epoch,
        )
        .await?
    else {
        panic!("a compensating run's queued wake must be claimable");
    };

    let outcome = repository
        .recover_resumed_controller_wake(
            scope,
            &config,
            claimed.run_uid,
            recovery_request(&claimed, ExecutionRunStatus::Compensating, 5),
        )
        .await?;

    let ResumedControllerRecoveryOutcome::Recovered { run: recovered, .. } = outcome else {
        panic!("recovering a compensating run must commit, got {outcome:?}");
    };
    assert_eq!(
        recovered.status,
        ExecutionRunStatus::Compensating,
        "recovery must not rewrite the compensation phase the watchdog is fenced against"
    );
    assert_eq!(
        recovered.controller_generation, claimed.controller_generation,
        "recovery must not rewrite the controller generation the watchdog is fenced against"
    );

    let rewritten = queued_run(
        &repository,
        scope,
        tenant_id,
        "controller-recovery-compensating-rewrite",
    )
    .await?;
    set_run_status_path(&pool, rewritten.run_uid, &["running", "compensating"]).await?;
    let rewritten = repository
        .load_run(scope, rewritten.run_uid)
        .await?
        .expect("the second compensating fixture run must be visible");
    let RunControllerClaimOutcome::Claimed(rewritten_claim) = repository
        .claim_controller_wake(
            scope,
            rewritten.run_uid,
            rewritten.controller_generation,
            rewritten.wake_epoch,
        )
        .await?
    else {
        panic!("the second compensating run's queued wake must be claimable");
    };

    let error = repository
        .recover_resumed_controller_wake(
            scope,
            &config,
            rewritten_claim.run_uid,
            recovery_request(&rewritten_claim, ExecutionRunStatus::Running, 5),
        )
        .await
        .expect_err("rewriting a compensating run to running must be rejected durably");

    assert!(
        error
            .to_string()
            .contains("invalid execution run status transition: compensating -> running"),
        "expected the durable transition guard to reject the rewrite, got `{error}`"
    );
    Ok(())
}
