//! Deterministic service coverage for task admission, retry, cancellation, input, and review.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit, ExecutionFailureClass,
    ExecutionGoalContract, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
    ExecutionRequirement, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionUsage, InputAudience,
    RetryPolicy,
};
use moa_core::{
    events::Event,
    types::{
        action_policy::{ActionPolicyEffect, ActionReviewStatus},
        execution_planning::{ExecutionSourceProvenance, GeneratedPlanPlannerProvenance},
        identifiers::{SessionId, TenantId},
    },
};
use moa_eval::execution::ExecutionInvariantSpec;
use moa_execution::{
    capability::{ExecutionCapability, ExecutionEstimate},
    compiler::{CompileExecutionRequest, CompiledExecution, compile},
    repository::{
        ExecutionRepository, ExecutionRunRecord, ExecutionScope, ExecutionTaskRecord,
        NewExecutionRun, ReservationOutcome, TaskOutcomeRejection, TaskOutcomeWrite,
        TransitionOutcome,
    },
    state::{
        ExecutionLimitStop, ExecutionRunStatus, ExecutionTaskId, ExecutionTaskStatus,
        ExecutionTerminalCause, ExecutionTerminalEvidence, LogicalTask, LogicalTaskKind,
    },
    wire::{
        ExecutionCancelRequest, ExecutionInputRequest, ExecutionMutationResponse,
        ExecutionPlanningContextRequest, ExecutionPlanningContextResponse, ExecutionRunRequest,
        ExecutionRunWorkflowRequest, ExecutionStartRequest, ExecutionStartResponse,
        ExecutionStatusResponse, ExecutionTaskWorkflowRequest,
    },
};
use moa_orchestrator::services::{
    action_policy::UpsertActionPolicyRuleRequest,
    action_reviews::{
        ActionReviewDecisionKind, ActionReviewSummary, DecideActionReviewRequest,
        ListActionReviewsRequest,
    },
    action_reviews_reaper::ActionReviewReaper,
};
use moa_test_support::{
    FixtureCapabilityController, FixtureCapabilityOptions, FixtureCapabilityOutcome,
    FixtureCapabilityTool, OrchestratorTestFixture,
};
use serde_json::{Value, json};
use tokio::time::Instant;
use uuid::Uuid;

use crate::evaluation::{assert_execution_eval_case, assert_repository_execution_eval_case};
use crate::execution_execution_support::fixtures::{
    POLL_INTERVAL, SERVICE_TIMEOUT, await_execution_terminal, list_execution_tasks,
};

const CAPABILITY_NODE_ID: &str = "capability";
const OUTPUT_NODE_ID: &str = "output";
const REQUIREMENT_ID: &str = "result";

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn reservation_budget_rejection_dispatches_zero_service_e2e() -> Result<()> {
    // Pins: an atomic reservation rejection consumes no logical task unit and starts no MCP call.
    let tool_name = "lifecycle_budget_probe";
    let fixture = direct_execution_fixture(tool_name, success_outcomes()).await?;
    let prepared = prepare_capability_run(
        &fixture,
        "reservation-budget-rejection",
        tool_name,
        no_retry(),
        ActionPolicyEffect::Allow,
    )
    .await?;
    let runtime_budget = ExecutionBudgetLimit {
        max_tasks: Some(0),
        ..prepared.planning.snapshot.budget.clone()
    };
    let run = create_direct_run(&prepared, runtime_budget, None).await?;

    let terminal = drive_run_workflow(&fixture, &run).await?;
    let controller = fixture_capability(&fixture)?;
    assert!(controller.calls().is_empty());
    assert!(controller.transport_attempts().is_empty());

    let task = load_task(&run).await?;
    assert_failed_task(&task, ExecutionFailureClass::BudgetExceeded);
    assert_eq!(task.actual_tasks, 0);
    assert_eq!(task.actual, zero_usage());
    assert_eq!(task.reserved, ExecutionEstimate::default());
    let terminal_run = load_run(&run).await?;
    assert_eq!(terminal_run.consumed, ExecutionEstimate::default());
    assert_eq!(terminal_run.reserved, ExecutionEstimate::default());
    assert_eq!(terminal_run.progress_failed_tasks, 1);

    assert_terminal(
        &terminal,
        ExecutionRunStatus::Failed,
        ExecutionTerminalCause::TaskFailure {
            class: ExecutionFailureClass::BudgetExceeded,
        },
        0,
        1,
    );
    assert_eq!(
        terminal.run.budget_ledger.consumed,
        ExecutionEstimate::default()
    );
    assert_eq!(
        terminal.run.budget_ledger.reserved,
        ExecutionEstimate::default()
    );
    assert!(
        terminal
            .gaps
            .iter()
            .any(|gap| gap == "execution task reservation rejected: BudgetExceeded"),
        "terminal gaps omitted the exact budget rejection: {:?}",
        terminal.gaps
    );
    assert_repository_execution_eval_case(
        &fixture,
        &run.repository,
        run.scope,
        &run.request,
        "reservation-budget-rejection-zero-dispatch",
        &[
            ExecutionInvariantSpec::MustNotComplete,
            ExecutionInvariantSpec::TerminalStatusIn {
                statuses: vec![ExecutionRunStatus::Failed],
            },
            ExecutionInvariantSpec::TerminalGapContains {
                text: "execution task reservation rejected: BudgetExceeded".to_string(),
            },
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoDuplicateLogicalEffects,
            ExecutionInvariantSpec::NoRawTaskOutputEvents,
        ],
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn elapsed_deadline_dispatches_zero_service_e2e() -> Result<()> {
    // Pins: deadline admission has precedence, persists typed evidence, and dispatches no MCP call.
    let tool_name = "lifecycle_deadline_probe";
    let fixture = direct_execution_fixture(tool_name, success_outcomes()).await?;
    let prepared = prepare_capability_run(
        &fixture,
        "elapsed-deadline",
        tool_name,
        no_retry(),
        ActionPolicyEffect::Allow,
    )
    .await?;
    assert_eq!(
        prepared.capability.estimate.tasks, 1,
        "fixture reservation must exceed the zero-task budget"
    );
    let runtime_budget = ExecutionBudgetLimit {
        max_tasks: Some(0),
        deadline_at: Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
        ..prepared.planning.snapshot.budget.clone()
    };
    let run = create_direct_run(&prepared, runtime_budget, None).await?;

    let terminal = drive_run_workflow(&fixture, &run).await?;
    let controller = fixture_capability(&fixture)?;
    assert!(controller.calls().is_empty());
    assert!(controller.transport_attempts().is_empty());

    let task = load_task(&run).await?;
    assert_failed_task(&task, ExecutionFailureClass::DeadlineExceeded);
    assert_eq!(task.actual_tasks, 0);
    assert_eq!(task.actual, zero_usage());
    assert_eq!(task.reserved, ExecutionEstimate::default());

    assert_terminal(
        &terminal,
        ExecutionRunStatus::Failed,
        ExecutionTerminalCause::LimitStop {
            reason: ExecutionLimitStop::DeadlineExceeded,
        },
        0,
        1,
    );
    assert_eq!(
        terminal.run.budget_ledger.consumed,
        ExecutionEstimate::default()
    );
    assert_eq!(
        terminal.run.budget_ledger.reserved,
        ExecutionEstimate::default()
    );
    assert!(
        terminal
            .gaps
            .iter()
            .any(|gap| gap == "execution task reservation rejected: DeadlineElapsed"),
        "terminal gaps omitted the exact deadline rejection: {:?}",
        terminal.gaps
    );
    assert_repository_execution_eval_case(
        &fixture,
        &run.repository,
        run.scope,
        &run.request,
        "elapsed-deadline-zero-dispatch",
        &[
            ExecutionInvariantSpec::MustNotComplete,
            ExecutionInvariantSpec::TerminalStatusIn {
                statuses: vec![ExecutionRunStatus::Failed],
            },
            ExecutionInvariantSpec::TerminalGapContains {
                text: "execution task reservation rejected: DeadlineElapsed".to_string(),
            },
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoDuplicateLogicalEffects,
            ExecutionInvariantSpec::NoRawTaskOutputEvents,
        ],
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn execution_eval_rate_limit_reuses_task_identity_service_e2e() -> Result<()> {
    // Pins: one 429 with Retry-After retries under a new generation of the same task ID.
    let tool_name = "lifecycle_retry_probe";
    let fixture = execution_fixture(
        tool_name,
        vec![
            FixtureCapabilityOutcome::HttpFailure {
                status: 429,
                retry_after_ms: Some(1),
                message: "fixture rate limit".to_string(),
            },
            FixtureCapabilityOutcome::Success {
                output: json!({"result": "retried"}),
            },
        ],
        Vec::new(),
    )
    .await?;
    let prepared = prepare_capability_run(
        &fixture,
        "retryable-failure",
        tool_name,
        RetryPolicy {
            max_attempts: 2,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        ActionPolicyEffect::Allow,
    )
    .await?;
    let run = start_service_run(&fixture, &prepared).await?;
    let controller = fixture_capability(&fixture)?;

    let first = controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].capability, tool_name);
    controller.release(1);
    let second = controller.wait_for_calls(2, SERVICE_TIMEOUT).await?;
    assert_eq!(second.len(), 2);
    assert_eq!(second[0].input, second[1].input);
    assert_ne!(second[0].invocation_id, second[1].invocation_id);
    controller.release(1);

    let terminal = await_execution_terminal(&fixture.client, &run.request).await?;
    assert_terminal(
        &terminal,
        ExecutionRunStatus::Completed,
        ExecutionTerminalCause::Completion { limit_stop: None },
        1,
        1,
    );
    let task = load_task(&run).await?;
    assert_eq!(task.task_id, run.task_id);
    assert_eq!(task.attempt, 2);
    assert_eq!(task.generation, 2);
    assert_eq!(task.generation_history.len(), 2);
    assert_eq!(task.status, ExecutionTaskStatus::Completed);
    assert_eq!(terminal.run.budget_ledger.consumed.tasks, 2);
    assert_eq!(terminal.run.budget_ledger.consumed.tool_calls, 2);
    assert_eq!(controller.calls().len(), 2);
    let transport_attempts = controller.transport_attempts();
    assert_eq!(transport_attempts.len(), 4);
    assert_eq!(
        transport_attempts
            .iter()
            .filter(|attempt| attempt.is_replay)
            .count(),
        2,
        "the governed MCP retry loop must replay one logical generation"
    );
    assert!(
        transport_attempts[..3]
            .iter()
            .all(|attempt| attempt.invocation_id == first[0].invocation_id)
    );
    assert_eq!(transport_attempts[3].invocation_id, second[1].invocation_id);
    assert_eq!(
        controller
            .calls()
            .iter()
            .map(|call| call.invocation_id.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        2
    );
    assert_execution_eval_case(
        &fixture,
        &fixture.client,
        &run.request,
        Some(controller),
        "rate-limit-reuses-task-identity",
        &[
            ExecutionInvariantSpec::TerminalStatusIn {
                statuses: vec![ExecutionRunStatus::Completed],
            },
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoDuplicateLogicalEffects,
            ExecutionInvariantSpec::NoRawTaskOutputEvents,
        ],
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn execution_eval_wrong_schema_is_invalid_output_service_e2e() -> Result<()> {
    // Pins: a successful capability transport with malformed structured output is terminalized as
    // InvalidOutput and cannot flow into the dependent output node.
    let tool_name = "lifecycle_wrong_schema_probe";
    let fixture = execution_fixture(
        tool_name,
        vec![FixtureCapabilityOutcome::Success {
            output: json!("not-an-object"),
        }],
        Vec::new(),
    )
    .await?;
    let prepared = prepare_capability_run(
        &fixture,
        "wrong-schema",
        tool_name,
        no_retry(),
        ActionPolicyEffect::Allow,
    )
    .await?;
    let prepared = recompile_with_node_output_schema(
        prepared,
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["result"],
            "properties": {"result": {"type": "string"}}
        }),
    )?;
    let run = start_service_run(&fixture, &prepared).await?;
    let controller = fixture_capability(&fixture)?;
    controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    controller.release(1);

    let terminal = await_execution_terminal(&fixture.client, &run.request).await?;
    assert_terminal(
        &terminal,
        ExecutionRunStatus::Failed,
        ExecutionTerminalCause::TaskFailure {
            class: ExecutionFailureClass::InvalidOutput,
        },
        0,
        1,
    );
    let task = load_task(&run).await?;
    assert_failed_task(&task, ExecutionFailureClass::InvalidOutput);
    let tasks = list_execution_tasks(&fixture.client, run.request.clone()).await?;
    assert_eq!(
        tasks.tasks.len(),
        1,
        "dependent output must not materialize"
    );
    assert_eq!(tasks.tasks[0].node_id, CAPABILITY_NODE_ID);
    assert_eq!(controller.calls().len(), 1);
    assert_execution_eval_case(
        &fixture,
        &fixture.client,
        &run.request,
        Some(controller),
        "wrong-schema-is-invalid-output",
        &[
            ExecutionInvariantSpec::MustNotComplete,
            ExecutionInvariantSpec::TerminalStatusIn {
                statuses: vec![ExecutionRunStatus::Failed],
            },
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoDuplicateLogicalEffects,
            ExecutionInvariantSpec::NoRawTaskOutputEvents,
        ],
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn terminal_failure_does_not_retry_service_e2e() -> Result<()> {
    // Pins: a one-attempt terminalized tool error makes exactly one dispatch and no new generation.
    let tool_name = "lifecycle_terminal_probe";
    let fixture = execution_fixture(
        tool_name,
        vec![FixtureCapabilityOutcome::TerminalFailure {
            message: "terminal fixture failure".to_string(),
        }],
        Vec::new(),
    )
    .await?;
    let prepared = prepare_capability_run(
        &fixture,
        "terminal-failure",
        tool_name,
        no_retry(),
        ActionPolicyEffect::Allow,
    )
    .await?;
    let run = start_service_run(&fixture, &prepared).await?;
    let controller = fixture_capability(&fixture)?;
    let calls = controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    assert_eq!(calls.len(), 1);
    controller.release(1);

    let terminal = await_execution_terminal(&fixture.client, &run.request).await?;
    assert_terminal(
        &terminal,
        ExecutionRunStatus::Failed,
        ExecutionTerminalCause::TaskFailure {
            class: ExecutionFailureClass::Terminal,
        },
        0,
        1,
    );
    let task = load_task(&run).await?;
    assert_failed_task(&task, ExecutionFailureClass::Terminal);
    assert_eq!(task.attempt, 1);
    assert_eq!(task.generation, 1);
    assert_eq!(task.generation_history.len(), 1);
    assert_eq!(task.actual_tasks, 1);
    assert_eq!(controller.calls().len(), 1);
    assert_eq!(controller.transport_attempts().len(), 1);
    assert_eq!(terminal.run.budget_ledger.consumed.tasks, 1);
    assert_eq!(terminal.run.budget_ledger.consumed.tool_calls, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn cancellation_releases_reservations_and_prevents_dispatch_service_e2e() -> Result<()> {
    // Pins: service cancellation releases every reserved dimension before a late task delivery.
    let tool_name = "lifecycle_cancel_probe";
    let fixture = direct_execution_fixture(tool_name, success_outcomes()).await?;
    let prepared = prepare_capability_run(
        &fixture,
        "cancellation-release",
        tool_name,
        no_retry(),
        ActionPolicyEffect::Allow,
    )
    .await?;
    let reservation = ExecutionEstimate {
        cost_microusd: 11,
        tokens: 12,
        tasks: 1,
        tool_calls: 1,
        retrieved_bytes: 13,
    };
    let run = create_direct_run(
        &prepared,
        prepared.planning.snapshot.budget.clone(),
        Some(reservation),
    )
    .await?;
    let ReservationOutcome::Reserved(reserved_task) = run
        .repository
        .reserve_task(run.scope, run.run_uid, run.task_id, 1)
        .await?
    else {
        bail!("cancellation fixture task did not reserve")
    };
    assert_eq!(reserved_task.reserved, reservation);
    assert_eq!(load_run(&run).await?.reserved, reservation);

    let cancelled: ExecutionMutationResponse = fixture
        .client
        .post_call(
            "/Execution/cancel",
            &ExecutionCancelRequest {
                run: run.request.clone(),
                reason: "operator cancelled before dispatch".to_string(),
            },
        )
        .await?;
    assert!(matches!(
        cancelled,
        ExecutionMutationResponse::Applied { ref run }
            if run.status == ExecutionRunStatus::Cancelled
    ));
    let terminal: ExecutionStatusResponse = fixture
        .client
        .post_call("/Execution/status", &run.request)
        .await?;
    assert_terminal(
        &terminal,
        ExecutionRunStatus::Cancelled,
        ExecutionTerminalCause::Cancellation,
        0,
        1,
    );
    assert_eq!(
        terminal.run.budget_ledger.reserved,
        ExecutionEstimate::default()
    );
    assert_eq!(
        terminal.run.budget_ledger.consumed,
        ExecutionEstimate {
            tasks: 1,
            ..ExecutionEstimate::default()
        }
    );
    let task = load_task(&run).await?;
    assert_eq!(task.status, ExecutionTaskStatus::Cancelled);
    assert_eq!(task.reserved, ExecutionEstimate::default());
    assert_eq!(task.actual_tasks, 1);

    let controller = fixture_capability(&fixture)?;
    let calls_before = controller.calls().len();
    let attempts_before = controller.transport_attempts().len();
    drive_task_workflow(&fixture, &run, 1).await?;
    assert_eq!(controller.calls().len(), calls_before);
    assert_eq!(controller.transport_attempts().len(), attempts_before);
    let replay: ExecutionMutationResponse = fixture
        .client
        .post_call(
            "/Execution/cancel",
            &ExecutionCancelRequest {
                run: run.request.clone(),
                reason: "operator cancelled before dispatch".to_string(),
            },
        )
        .await?;
    assert!(matches!(replay, ExecutionMutationResponse::Replayed { .. }));
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn input_resume_preserves_attempt_and_history_service_e2e() -> Result<()> {
    // Pins: exact input replay preserves attempt one and one append-only payload at generation two,
    // then dispatches that generation exactly once.
    let tool_name = "lifecycle_input_probe";
    let fixture = direct_execution_fixture(tool_name, success_outcomes()).await?;
    let prepared = prepare_capability_run(
        &fixture,
        "input-resume",
        tool_name,
        no_retry(),
        ActionPolicyEffect::Allow,
    )
    .await?;
    let run = create_direct_run(&prepared, prepared.planning.snapshot.budget.clone(), None).await?;
    reserve_and_mark_running(&run).await?;
    let waiting = ExecutionTaskOutcome {
        schema_version: 1,
        usage: zero_usage(),
        result: ExecutionTaskResult::NeedsInput {
            question: "Which source should be used?".to_string(),
            audience: InputAudience::User,
        },
    };
    assert!(matches!(
        run.repository
            .record_task_outcome(run.scope, run.run_uid, run.task_id, 1, waiting)
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    let input = json!({"source": "analyst-notes"});
    let request = ExecutionInputRequest {
        tenant_id: run.tenant_id,
        contact_id: None,
        session_id: Some(run.session_id),
        run_uid: run.run_uid,
        task_id: run.task_id,
        expected_generation: 1,
        audience: InputAudience::User,
        input: input.clone(),
    };

    let applied: ExecutionMutationResponse = fixture
        .client
        .post_call("/Execution/deliver_input", &request)
        .await?;
    assert!(matches!(applied, ExecutionMutationResponse::Applied { .. }));
    let resumed = load_task(&run).await?;
    assert_eq!(resumed.attempt, 1);
    assert_eq!(resumed.generation, 2);
    assert_eq!(resumed.status, ExecutionTaskStatus::Running);
    assert_eq!(resumed.resume_input_history, vec![input.clone()]);
    assert_eq!(resumed.generation_history.len(), 2);

    let replayed: ExecutionMutationResponse = fixture
        .client
        .post_call("/Execution/deliver_input", &request)
        .await?;
    assert!(matches!(
        replayed,
        ExecutionMutationResponse::Replayed { .. }
    ));
    let after_replay = load_task(&run).await?;
    assert_eq!(after_replay.attempt, 1);
    assert_eq!(after_replay.generation, 2);
    assert_eq!(after_replay.resume_input_history, vec![input]);
    assert_eq!(after_replay.generation_history, resumed.generation_history);
    assert_eq!(after_replay.outcome_audit, resumed.outcome_audit);
    let controller = fixture_capability(&fixture)?;
    assert!(controller.calls().is_empty());

    let client = fixture.client.clone();
    let task_path = format!("/ExecutionTask/{}/run", run.task_id);
    let task_request = ExecutionTaskWorkflowRequest {
        run_uid: run.run_uid,
        task_id: run.task_id,
        generation: 2,
        tenant_id: run.tenant_id,
        contact_id: None,
        session_id: run.session_id,
    };
    let driver = tokio::spawn(async move { client.post_void(&task_path, &task_request).await });
    let calls = controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    assert_eq!(calls.len(), 1);
    controller.release(1);
    tokio::time::timeout(SERVICE_TIMEOUT, driver)
        .await
        .context("resumed execution task did not finish")???;
    assert_eq!(
        load_task(&run).await?.status,
        ExecutionTaskStatus::Completed
    );
    assert_eq!(controller.calls().len(), 1);
    assert_eq!(controller.transport_attempts().len(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn action_review_terminal_states_deliver_once_service_e2e() -> Result<()> {
    // Pins: cleared, denied, timed-out, and replayed review outbox deliveries are UID-idempotent.
    let tool_name = "lifecycle_review_probe";
    let fixture = execution_fixture(tool_name, success_outcomes(), Vec::new()).await?;
    let pool = sqlx::PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect to action-review fixture Postgres")?;
    let controller = fixture_capability(&fixture)?;
    let reaper =
        ActionReviewReaper::with_restate_ingress(pool.clone(), fixture.ingress_url.clone());

    let cleared = prepare_capability_run(
        &fixture,
        "review-cleared",
        tool_name,
        no_retry(),
        ActionPolicyEffect::AdminReview,
    )
    .await?;
    let cleared_run = start_service_run(&fixture, &cleared).await?;
    let cleared_review =
        await_execution_review(&fixture, cleared_run.tenant_id, cleared_run.task_id).await?;
    let client = fixture.client.clone();
    let cleared_tenant = cleared_run.tenant_id;
    let cleared_review_id = cleared_review.id;
    let clear = tokio::spawn(async move {
        client
            .post_void(
                "/ActionReviews/decide",
                &DecideActionReviewRequest {
                    tenant_id: cleared_tenant,
                    review_id: cleared_review_id,
                    decision: ActionReviewDecisionKind::Cleared,
                    reason: None,
                },
            )
            .await
    });
    let clear_calls = controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    assert_eq!(clear_calls.len(), 1);
    assert_eq!(clear_calls[0].capability, tool_name);
    controller.release(1);
    tokio::time::timeout(SERVICE_TIMEOUT, clear)
        .await
        .context("cleared action-review decision timed out")???;
    assert_eq!(
        reaper.dispatch_execution_review_resolutions().await?,
        1,
        "the cleared review resolution must be dispatched through the production outbox"
    );
    let cleared_terminal =
        await_review_terminal("cleared", &fixture, &cleared_run, Duration::from_secs(15)).await?;
    assert_terminal(
        &cleared_terminal,
        ExecutionRunStatus::Completed,
        ExecutionTerminalCause::Completion { limit_stop: None },
        1,
        1,
    );
    assert_review_status(&pool, cleared_review.id, "cleared").await?;
    let cleared_outbox = await_outbox_delivered(&pool, cleared_review.id, 1).await?;
    assert_eq!(cleared_outbox.resolution_status, "completed");
    assert_review_audit_once(&cleared_run, cleared_review.id, 1).await?;

    let denied = prepare_capability_run(
        &fixture,
        "review-denied",
        tool_name,
        no_retry(),
        ActionPolicyEffect::AdminReview,
    )
    .await?;
    let denied_run = start_service_run(&fixture, &denied).await?;
    let denied_review =
        await_execution_review(&fixture, denied_run.tenant_id, denied_run.task_id).await?;
    fixture
        .client
        .post_void(
            "/ActionReviews/decide",
            &DecideActionReviewRequest {
                tenant_id: denied_run.tenant_id,
                review_id: denied_review.id,
                decision: ActionReviewDecisionKind::Denied,
                reason: Some("tenant denied fixture action".to_string()),
            },
        )
        .await?;
    assert_eq!(
        reaper.dispatch_execution_review_resolutions().await?,
        1,
        "the denied review resolution must be dispatched through the production outbox"
    );
    let denied_terminal =
        await_review_terminal("denied", &fixture, &denied_run, Duration::from_secs(15)).await?;
    assert_terminal(
        &denied_terminal,
        ExecutionRunStatus::Blocked,
        ExecutionTerminalCause::TaskFailure {
            class: ExecutionFailureClass::AuthorizationDenied,
        },
        0,
        1,
    );
    assert_review_status(&pool, denied_review.id, "denied").await?;
    let denied_outbox = await_outbox_delivered(&pool, denied_review.id, 1).await?;
    assert_eq!(denied_outbox.resolution_status, "denied");
    assert_review_audit_once(&denied_run, denied_review.id, 1).await?;
    assert_eq!(controller.calls().len(), 1);

    let timed_out = prepare_capability_run(
        &fixture,
        "review-timeout",
        tool_name,
        no_retry(),
        ActionPolicyEffect::AdminReview,
    )
    .await?;
    let timed_out_run = start_service_run(&fixture, &timed_out).await?;
    let timed_out_review =
        await_execution_review(&fixture, timed_out_run.tenant_id, timed_out_run.task_id).await?;
    sqlx::query(
        "UPDATE tenant_action_reviews SET expires_at = NOW() - INTERVAL '1 second' WHERE id = $1",
    )
    .bind(timed_out_review.id)
    .execute(&pool)
    .await
    .context("expire execution action review deterministically")?;
    assert_eq!(
        reaper.sweep().await?,
        1,
        "the production timeout sweep must claim the exact expired review"
    );
    let timeout_delivery: (i32, Option<chrono::DateTime<chrono::Utc>>, Option<String>) =
        sqlx::query_as(
            "SELECT attempt_count, delivered_at, last_error \
             FROM moa.execution_action_review_outbox WHERE review_uid = $1",
        )
        .bind(timed_out_review.id)
        .fetch_one(&pool)
        .await?;
    assert!(
        timeout_delivery.1.is_some(),
        "timeout resolution was not delivered: attempts={}, last_error={:?}",
        timeout_delivery.0,
        timeout_delivery.2
    );
    let timed_out_terminal = await_review_terminal(
        "timed-out",
        &fixture,
        &timed_out_run,
        Duration::from_secs(15),
    )
    .await?;
    assert_terminal(
        &timed_out_terminal,
        ExecutionRunStatus::Failed,
        ExecutionTerminalCause::TaskFailure {
            class: ExecutionFailureClass::DeadlineExceeded,
        },
        0,
        1,
    );
    assert_review_status(&pool, timed_out_review.id, "timeout").await?;
    let timeout_outbox = await_outbox_delivered(&pool, timed_out_review.id, 1).await?;
    assert_eq!(timeout_outbox.resolution_status, "timed_out");
    assert_review_audit_once(&timed_out_run, timed_out_review.id, 1).await?;
    assert_eq!(controller.calls().len(), 1);

    sqlx::query(
        "UPDATE moa.execution_action_review_outbox \
         SET delivered_at = NULL, claimed_at = NULL, next_attempt_at = NOW(), last_error = NULL \
         WHERE review_uid = $1",
    )
    .bind(cleared_review.id)
    .execute(&pool)
    .await
    .context("requeue exact delivered action-review resolution")?;
    assert_eq!(
        reaper.dispatch_execution_review_resolutions().await?,
        1,
        "the exact requeued resolution must be claimed once"
    );
    let replayed_outbox = await_outbox_delivered(&pool, cleared_review.id, 2).await?;
    assert_eq!(replayed_outbox.resolution_status, "completed");
    assert!(replayed_outbox.attempt_count >= 2);
    assert_review_audit_once(&cleared_run, cleared_review.id, 1).await?;
    assert_eq!(controller.calls().len(), 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn stale_generation_is_audit_only_service_e2e() -> Result<()> {
    // Pins: a stale completion appends one rejected audit without changing projection or usage.
    let tool_name = "lifecycle_stale_probe";
    let fixture = direct_execution_fixture(tool_name, success_outcomes()).await?;
    let prepared = prepare_capability_run(
        &fixture,
        "stale-generation",
        tool_name,
        RetryPolicy {
            max_attempts: 2,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        ActionPolicyEffect::Allow,
    )
    .await?;
    let run = create_direct_run(&prepared, prepared.planning.snapshot.budget.clone(), None).await?;
    reserve_and_mark_running(&run).await?;
    let retryable = ExecutionTaskOutcome {
        schema_version: 1,
        usage: zero_usage(),
        result: ExecutionTaskResult::Failed {
            class: ExecutionFailureClass::Retryable,
            message: "retry under generation two".to_string(),
        },
    };
    assert!(matches!(
        run.repository
            .record_task_outcome(run.scope, run.run_uid, run.task_id, 1, retryable)
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    assert!(matches!(
        run.repository
            .retry_task(run.scope, run.run_uid, run.task_id, 1)
            .await?,
        TransitionOutcome::Applied(_)
    ));
    let run_before = load_run(&run).await?;
    let task_before = load_task(&run).await?;
    assert_eq!(task_before.attempt, 2);
    assert_eq!(task_before.generation, 2);
    let controlled_before = controlled_task_projection(&task_before);

    let stale = completed_outcome(json!({"result": "stale"}), zero_usage());
    let rejected = run
        .repository
        .record_task_outcome(run.scope, run.run_uid, run.task_id, 1, stale)
        .await?;
    let TaskOutcomeWrite::Rejected { task, reason } = rejected else {
        bail!("stale generation completion was not audit-only")
    };
    assert_eq!(reason, TaskOutcomeRejection::StaleGeneration);
    assert_eq!(controlled_task_projection(&task), controlled_before);
    assert_eq!(
        task.outcome_audit.len(),
        task_before.outcome_audit.len() + 1
    );
    let stale_audit = task
        .outcome_audit
        .last()
        .context("stale completion omitted its audit record")?;
    assert_eq!(stale_audit.get("accepted"), Some(&json!(false)));
    assert_eq!(stale_audit.get("received_generation"), Some(&json!(1)));
    assert_eq!(stale_audit.get("received_attempt"), Some(&json!(1)));
    assert_eq!(
        stale_audit.get("rejection"),
        Some(&json!("stale_generation"))
    );
    assert_eq!(load_run(&run).await?, run_before);
    let controller = fixture_capability(&fixture)?;
    assert!(controller.calls().is_empty());
    assert_repository_execution_eval_case(
        &fixture,
        &run.repository,
        run.scope,
        &run.request,
        "stale-generation-write-is-fenced",
        &[
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoDuplicateLogicalEffects,
            ExecutionInvariantSpec::NoRawTaskOutputEvents,
        ],
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn duplicate_completion_does_not_double_account_service_e2e() -> Result<()> {
    // Pins: exact task completion replay is byte-identical and consumes one logical task once.
    let tool_name = "lifecycle_duplicate_probe";
    let fixture = direct_execution_fixture(tool_name, success_outcomes()).await?;
    let prepared = prepare_capability_run(
        &fixture,
        "duplicate-completion",
        tool_name,
        no_retry(),
        ActionPolicyEffect::Allow,
    )
    .await?;
    let run = create_direct_run(&prepared, prepared.planning.snapshot.budget.clone(), None).await?;
    reserve_and_mark_running(&run).await?;
    let completion = completed_outcome(
        json!({"result": "accepted-once"}),
        ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 1,
            retrieved_bytes: 32,
        },
    );
    assert!(matches!(
        run.repository
            .record_task_outcome(run.scope, run.run_uid, run.task_id, 1, completion.clone(),)
            .await?,
        TaskOutcomeWrite::Applied {
            budget_overrun: false,
            ..
        }
    ));
    let run_before = load_run(&run).await?;
    let task_before = load_task(&run).await?;
    assert_eq!(run_before.consumed.tasks, 1);
    assert_eq!(run_before.consumed.tool_calls, 1);
    assert_eq!(run_before.progress_completed_tasks, 1);
    assert_eq!(task_before.actual_tasks, 1);

    let replayed = run
        .repository
        .record_task_outcome(run.scope, run.run_uid, run.task_id, 1, completion)
        .await?;
    assert!(matches!(
        replayed,
        TaskOutcomeWrite::Replayed {
            budget_overrun: false,
            ..
        }
    ));
    assert_eq!(load_run(&run).await?, run_before);
    assert_eq!(load_task(&run).await?, task_before);

    let terminal = drive_run_workflow(&fixture, &run).await?;
    assert_terminal(
        &terminal,
        ExecutionRunStatus::Completed,
        ExecutionTerminalCause::Completion { limit_stop: None },
        1,
        1,
    );
    assert_eq!(terminal.run.budget_ledger.consumed.tasks, 2);
    assert_eq!(terminal.run.budget_ledger.consumed.tool_calls, 1);
    assert_eq!(terminal.run.completed_tasks, 2);
    let controller = fixture_capability(&fixture)?;
    assert!(controller.calls().is_empty());
    assert_repository_execution_eval_case(
        &fixture,
        &run.repository,
        run.scope,
        &run.request,
        "duplicate-completion-is-idempotent",
        &[
            ExecutionInvariantSpec::TerminalStatusIn {
                statuses: vec![ExecutionRunStatus::Completed],
            },
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoDuplicateLogicalEffects,
            ExecutionInvariantSpec::NoRawTaskOutputEvents,
        ],
    )
    .await?;
    Ok(())
}

struct PreparedCapabilityRun {
    tenant_id: TenantId,
    session_id: SessionId,
    originating_user_sequence_num: u64,
    planning: ExecutionPlanningContextResponse,
    compiled: CompiledExecution,
    capability: ExecutionCapability,
    repository: ExecutionRepository,
    scope: ExecutionScope,
    retry: RetryPolicy,
}

#[derive(Clone)]
struct RunningCapabilityRun {
    tenant_id: TenantId,
    session_id: SessionId,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
    request: ExecutionRunRequest,
    repository: ExecutionRepository,
    scope: ExecutionScope,
}

async fn execution_fixture(
    tool_name: &str,
    outcomes: Vec<FixtureCapabilityOutcome>,
    orchestrator_env: Vec<(String, String)>,
) -> Result<OrchestratorTestFixture> {
    OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": {
                "content": "unused lifecycle scripted response",
                "tool_calls": []
            }
        }),
        FixtureCapabilityOptions {
            tools: vec![FixtureCapabilityTool {
                name: tool_name.to_string(),
                description: "Execute one deterministic task-lifecycle probe".to_string(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["case"],
                    "properties": {"case": {"type": "string"}}
                }),
                item_key_pointer: None,
                outcomes,
            }],
            orchestrator_env,
        },
    )
    .await
}

async fn direct_execution_fixture(
    tool_name: &str,
    outcomes: Vec<FixtureCapabilityOutcome>,
) -> Result<OrchestratorTestFixture> {
    execution_fixture(
        tool_name,
        outcomes,
        vec![(
            "MOA_EXECUTION_TEST_SKIP_SESSION_DELIVERY".to_string(),
            "true".to_string(),
        )],
    )
    .await
}

async fn prepare_capability_run(
    fixture: &OrchestratorTestFixture,
    label: &str,
    tool_name: &str,
    retry: RetryPolicy,
    policy_effect: ActionPolicyEffect,
) -> Result<PreparedCapabilityRun> {
    let test = fixture.isolated().await;
    let session_id = test.create_session(label).await?;
    let session = fixture.client.get_session(session_id).await?;
    fixture
        .grant_default_tenant_admin(session.tenant_id)
        .await
        .context("grant tenant admin before exact lifecycle policy upsert")?;
    fixture
        .client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &UpsertActionPolicyRuleRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                tool_name: tool_name.to_string(),
                pattern: "*".to_string(),
                effect: policy_effect,
                reason: Some(format!("Task 10 lifecycle fixture: {label}")),
            },
        )
        .await
        .context("upsert exact lifecycle fixture policy")?;
    let objective = format!("Execute the deterministic Task 10 lifecycle case {label}");
    let originating_user_sequence_num = fixture
        .client
        .append_event(
            session_id,
            Event::UserMessage {
                text: objective.clone(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let planning: ExecutionPlanningContextResponse = fixture
        .client
        .post_call(
            "/Execution/planning_context",
            &ExecutionPlanningContextRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num,
                requested_template: None,
            },
        )
        .await
        .context("load production lifecycle planning context")?;
    let capability = planning
        .snapshot
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference.name == tool_name)
        .cloned()
        .with_context(|| format!("planning catalog omitted fixture capability `{tool_name}`"))?;
    let compiled = compile(CompileExecutionRequest {
        goal: lifecycle_goal(objective),
        plan: lifecycle_plan(&capability, retry.clone()),
        run_input: json!({}),
        catalog: planning.snapshot.catalog.clone(),
        authorization: planning.snapshot.authorization.clone(),
        approved_budget: planning.snapshot.budget.clone(),
        config: moa_core::config::ExecutionConfig::default(),
        now: chrono::Utc::now(),
    })
    .compiled
    .with_context(|| format!("compile lifecycle plan for `{label}`"))?;
    let pool = sqlx::PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect lifecycle repository to fixture Postgres")?;
    Ok(PreparedCapabilityRun {
        tenant_id: session.tenant_id,
        session_id,
        originating_user_sequence_num,
        planning,
        compiled,
        capability,
        repository: ExecutionRepository::new(pool),
        scope: ExecutionScope::Tenant {
            tenant_id: session.tenant_id,
        },
        retry,
    })
}

fn recompile_with_node_output_schema(
    mut prepared: PreparedCapabilityRun,
    output_schema: Value,
) -> Result<PreparedCapabilityRun> {
    let objective = prepared.compiled.goal.objective.clone();
    let outcome = compile(CompileExecutionRequest {
        goal: lifecycle_goal(objective),
        plan: lifecycle_plan_with_output_schema(
            &prepared.capability,
            prepared.retry.clone(),
            output_schema,
        ),
        run_input: json!({}),
        catalog: prepared.planning.snapshot.catalog.clone(),
        authorization: prepared.planning.snapshot.authorization.clone(),
        approved_budget: prepared.planning.snapshot.budget.clone(),
        config: moa_core::config::ExecutionConfig::default(),
        now: chrono::Utc::now(),
    });
    prepared.compiled = outcome.compiled.with_context(|| {
        format!(
            "compile lifecycle plan with strict node output schema: {:?}",
            outcome.report.issues
        )
    })?;
    Ok(prepared)
}

async fn start_service_run(
    fixture: &OrchestratorTestFixture,
    prepared: &PreparedCapabilityRun,
) -> Result<RunningCapabilityRun> {
    let started: ExecutionStartResponse = fixture
        .client
        .post_call(
            "/Execution/start",
            &ExecutionStartRequest {
                tenant_id: prepared.tenant_id,
                contact_id: None,
                session_id: prepared.session_id,
                originating_user_sequence_num: prepared.originating_user_sequence_num,
                planning_context_uid: prepared.planning.planning_context_uid,
                planning_context_hash: prepared.planning.planning_context_hash.clone(),
                idempotency_key: Some(format!("task-lifecycle-{}", prepared.session_id)),
                compiled: prepared.compiled.clone(),
                run_input: json!({}),
                source_provenance: generated_source(&prepared.compiled.plan.plan_hash.to_string()),
            },
        )
        .await
        .context("start lifecycle run through Execution service")?;
    assert!(started.created);
    assert!(!started.confirmation_required);
    let task_id = ExecutionTaskId::derive(started.run.run_uid, CAPABILITY_NODE_ID, "")?;
    Ok(RunningCapabilityRun {
        tenant_id: prepared.tenant_id,
        session_id: prepared.session_id,
        run_uid: started.run.run_uid,
        task_id,
        request: ExecutionRunRequest {
            tenant_id: prepared.tenant_id,
            contact_id: None,
            session_id: prepared.session_id,
            run_uid: started.run.run_uid,
        },
        repository: prepared.repository.clone(),
        scope: prepared.scope,
    })
}

async fn create_direct_run(
    prepared: &PreparedCapabilityRun,
    approved_budget: ExecutionBudgetLimit,
    reservation_override: Option<ExecutionEstimate>,
) -> Result<RunningCapabilityRun> {
    let run = prepared
        .repository
        .create_run(
            prepared.scope,
            NewExecutionRun {
                tenant_id: prepared.tenant_id,
                contact_id: None,
                session_id: prepared.session_id,
                originating_user_sequence_num: prepared.originating_user_sequence_num,
                planning_context_uid: prepared.planning.planning_context_uid,
                planning_context_hash: prepared.planning.planning_context_hash.parse()?,
                owner_user_id: prepared.planning.snapshot.owner_user_id.clone(),
                goal: prepared.compiled.goal.clone(),
                plan: prepared.compiled.plan.clone(),
                catalog: prepared.planning.snapshot.catalog.clone(),
                authorization: prepared.planning.snapshot.authorization.clone(),
                pinned_instruction_skills: prepared
                    .planning
                    .snapshot
                    .pinned_instruction_skills
                    .clone(),
                source_provenance: generated_source(&prepared.compiled.plan.plan_hash.to_string()),
                input: json!({}),
                status: ExecutionRunStatus::Queued,
                approved_budget,
                idempotency_key: Some(format!("task-lifecycle-direct-{}", prepared.session_id)),
            },
        )
        .await?;
    let task_id = ExecutionTaskId::derive(run.run_uid, CAPABILITY_NODE_ID, "")?;
    prepared
        .repository
        .materialize_tasks(
            prepared.scope,
            run.run_uid,
            1,
            vec![LogicalTask {
                task_id,
                node_id: CAPABILITY_NODE_ID.to_string(),
                item_key: String::new(),
                requirement_ids: vec![REQUIREMENT_ID.to_string()],
                plan_revision: 1,
                generation: 1,
                input: json!({"case": "task-lifecycle"}),
                kind: LogicalTaskKind::Capability {
                    reference: prepared.capability.reference.clone(),
                },
                retry: prepared.retry.clone(),
                reservation: reservation_override.unwrap_or(prepared.capability.estimate),
            }],
        )
        .await?;
    Ok(RunningCapabilityRun {
        tenant_id: prepared.tenant_id,
        session_id: prepared.session_id,
        run_uid: run.run_uid,
        task_id,
        request: ExecutionRunRequest {
            tenant_id: prepared.tenant_id,
            contact_id: None,
            session_id: prepared.session_id,
            run_uid: run.run_uid,
        },
        repository: prepared.repository.clone(),
        scope: prepared.scope,
    })
}

fn lifecycle_goal(objective: String) -> ExecutionGoalContract {
    ExecutionGoalContract {
        objective,
        requirements: vec![ExecutionRequirement {
            id: REQUIREMENT_ID.to_string(),
            description: "produce the deterministic lifecycle result".to_string(),
        }],
        deliverables: Vec::new(),
        coverage: Vec::new(),
        constraints: Vec::new(),
        completion_checks: vec![CompletionCheck {
            id: "output_schema".to_string(),
            description: "terminal output satisfies its schema".to_string(),
            requirement_ids: vec![REQUIREMENT_ID.to_string()],
            constraint_ids: Vec::new(),
            kind: CompletionCheckKind::OutputSchema,
        }],
    }
}

fn lifecycle_plan(capability: &ExecutionCapability, retry: RetryPolicy) -> ExecutionPlanDefinition {
    lifecycle_plan_with_output_schema(capability, retry, capability.output_schema.clone())
}

fn lifecycle_plan_with_output_schema(
    capability: &ExecutionCapability,
    retry: RetryPolicy,
    output_schema: Value,
) -> ExecutionPlanDefinition {
    ExecutionPlanDefinition {
        schema_version: 1,
        input_schema: json!({"type": "object", "additionalProperties": false}),
        output_schema: output_schema.clone(),
        nodes: vec![
            ExecutionNode {
                id: CAPABILITY_NODE_ID.to_string(),
                requirement_ids: vec![REQUIREMENT_ID.to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({"case": "task-lifecycle"}),
                output_schema: output_schema.clone(),
                operation: ExecutionOperation::Capability {
                    reference: capability.reference.clone(),
                },
                retry,
                budget: None,
            },
            ExecutionNode {
                id: OUTPUT_NODE_ID.to_string(),
                requirement_ids: vec![REQUIREMENT_ID.to_string()],
                depends_on: vec![CAPABILITY_NODE_ID.to_string()],
                when: None,
                input: json!({}),
                output_schema,
                operation: ExecutionOperation::Output {
                    value: json!({"$ref": "$.nodes.capability.output"}),
                },
                retry: no_retry(),
                budget: None,
            },
        ],
    }
}

fn generated_source(final_plan_hash: &str) -> ExecutionSourceProvenance {
    ExecutionSourceProvenance::GeneratedPlan {
        route_rationale: "The requested workflow should persist as a durable execution."
            .to_string(),
        planner: GeneratedPlanPlannerProvenance {
            model: "scripted-fixture".to_string(),
            prompt_version: "task-lifecycle-service-e2e".to_string(),
            candidate_hash: "a".repeat(64),
            compiler_report_hash: "b".repeat(64),
            final_plan_hash: final_plan_hash.to_string(),
            repair_attempts: 0,
        },
    }
}

fn no_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    }
}

fn success_outcomes() -> Vec<FixtureCapabilityOutcome> {
    vec![FixtureCapabilityOutcome::Success {
        output: json!({"result": "ok"}),
    }]
}

fn zero_usage() -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

fn completed_outcome(output: Value, usage: ExecutionUsage) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage,
        result: ExecutionTaskResult::Completed {
            output,
            citations: Vec::new(),
        },
    }
}

async fn reserve_and_mark_running(run: &RunningCapabilityRun) -> Result<()> {
    assert!(matches!(
        run.repository
            .reserve_task(run.scope, run.run_uid, run.task_id, 1)
            .await?,
        ReservationOutcome::Reserved(_)
    ));
    assert!(matches!(
        run.repository
            .mark_task_running(run.scope, run.run_uid, run.task_id, 1)
            .await?,
        TransitionOutcome::Applied(_)
    ));
    Ok(())
}

async fn drive_task_workflow(
    fixture: &OrchestratorTestFixture,
    run: &RunningCapabilityRun,
    generation: u64,
) -> Result<()> {
    tokio::time::timeout(
        SERVICE_TIMEOUT,
        fixture.client.post_void(
            &format!("/ExecutionTask/{}/run", run.task_id),
            &ExecutionTaskWorkflowRequest {
                run_uid: run.run_uid,
                task_id: run.task_id,
                generation,
                tenant_id: run.tenant_id,
                contact_id: None,
                session_id: run.session_id,
            },
        ),
    )
    .await
    .context("ExecutionTask workflow exceeded the bounded service timeout")??;
    Ok(())
}

async fn drive_run_workflow(
    fixture: &OrchestratorTestFixture,
    run: &RunningCapabilityRun,
) -> Result<ExecutionStatusResponse> {
    fixture
        .client
        .post_send(
            &format!("/ExecutionTask/{}/run", run.task_id),
            &ExecutionTaskWorkflowRequest {
                run_uid: run.run_uid,
                task_id: run.task_id,
                generation: 1,
                tenant_id: run.tenant_id,
                contact_id: None,
                session_id: run.session_id,
            },
        )
        .await
        .context("send deterministic root ExecutionTask workflow")?;
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        if load_task(run).await?.status.is_terminal() {
            break;
        }
        if Instant::now() >= deadline {
            bail!("direct execution task did not terminalize before run drive");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    fixture
        .client
        .post_send(
            &format!("/ExecutionRun/{}/run", run.run_uid),
            &ExecutionRunWorkflowRequest {
                run_uid: run.run_uid,
                tenant_id: run.tenant_id,
                contact_id: None,
                session_id: run.session_id,
            },
        )
        .await
        .context("send deterministic ExecutionRun workflow")?;
    await_execution_terminal(&fixture.client, &run.request).await
}

async fn load_run(run: &RunningCapabilityRun) -> Result<ExecutionRunRecord> {
    run.repository
        .load_run(run.scope, run.run_uid)
        .await?
        .context("lifecycle run disappeared")
}

async fn load_task(run: &RunningCapabilityRun) -> Result<ExecutionTaskRecord> {
    run.repository
        .load_task(run.scope, run.run_uid, run.task_id)
        .await?
        .context("lifecycle task disappeared")
}

async fn await_review_terminal(
    label: &str,
    fixture: &OrchestratorTestFixture,
    running: &RunningCapabilityRun,
    timeout: Duration,
) -> Result<ExecutionStatusResponse> {
    match tokio::time::timeout(
        timeout,
        await_execution_terminal(&fixture.client, &running.request),
    )
    .await
    {
        Ok(terminal) => terminal,
        Err(_) => {
            let task = load_task(running).await?;
            let run = load_run(running).await?;
            bail!(
                "{label} review did not terminalize within {timeout:?}; run={run:#?}; task={task:#?}"
            );
        }
    }
}

fn fixture_capability(fixture: &OrchestratorTestFixture) -> Result<&FixtureCapabilityController> {
    fixture
        .fixture_capability()
        .context("execution fixture omitted its capability controller")
}

fn assert_failed_task(task: &ExecutionTaskRecord, expected_class: ExecutionFailureClass) {
    assert_eq!(task.status, ExecutionTaskStatus::Failed);
    assert!(matches!(
        task.current_outcome.as_ref().map(|outcome| &outcome.result),
        Some(ExecutionTaskResult::Failed { class, .. }) if class == &expected_class
    ));
}

fn assert_terminal(
    status: &ExecutionStatusResponse,
    expected_status: ExecutionRunStatus,
    cause: ExecutionTerminalCause,
    satisfied_requirement_count: u64,
    requirement_count: u64,
) {
    assert_eq!(status.run.status, expected_status);
    assert_eq!(
        status.run.terminal_evidence,
        Some(ExecutionTerminalEvidence {
            cause,
            satisfied_requirement_count,
            requirement_count,
        })
    );
}

async fn await_execution_review(
    fixture: &OrchestratorTestFixture,
    tenant_id: TenantId,
    task_id: ExecutionTaskId,
) -> Result<ActionReviewSummary> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let reviews: Vec<ActionReviewSummary> = fixture
            .client
            .post_call(
                "/ActionReviews/list_pending",
                &ListActionReviewsRequest { tenant_id },
            )
            .await?;
        if let Some(review) = reviews.into_iter().find(|review| {
            review
                .envelope
                .execution_origin
                .is_some_and(|origin| origin.task_uid == task_id.as_uuid())
        }) {
            assert_eq!(review.status, ActionReviewStatus::Pending);
            return Ok(review);
        }
        if Instant::now() >= deadline {
            bail!("task {task_id} did not create an action review within {SERVICE_TIMEOUT:?}")
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

struct OutboxSnapshot {
    resolution_status: String,
    attempt_count: i32,
}

async fn await_outbox_delivered(
    pool: &sqlx::PgPool,
    review_uid: Uuid,
    minimum_attempts: i32,
) -> Result<OutboxSnapshot> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let row: Option<(String, i32, Option<chrono::DateTime<chrono::Utc>>)> = sqlx::query_as(
            "SELECT resolution->>'status', attempt_count, delivered_at \
             FROM moa.execution_action_review_outbox WHERE review_uid = $1",
        )
        .bind(review_uid)
        .fetch_optional(pool)
        .await?;
        if let Some((resolution_status, attempt_count, Some(_))) = row
            && attempt_count >= minimum_attempts
        {
            return Ok(OutboxSnapshot {
                resolution_status,
                attempt_count,
            });
        }
        if Instant::now() >= deadline {
            bail!(
                "review outbox {review_uid} was not delivered at attempt {minimum_attempts} within {SERVICE_TIMEOUT:?}"
            )
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn assert_review_status(pool: &sqlx::PgPool, review_uid: Uuid, expected: &str) -> Result<()> {
    let status: String =
        sqlx::query_scalar("SELECT status FROM tenant_action_reviews WHERE id = $1")
            .bind(review_uid)
            .fetch_one(pool)
            .await?;
    assert_eq!(status, expected);
    Ok(())
}

async fn assert_review_audit_once(
    run: &RunningCapabilityRun,
    review_uid: Uuid,
    generation: u64,
) -> Result<()> {
    let task = load_task(run).await?;
    let review_uid = review_uid.to_string();
    let entries = task
        .outcome_audit
        .iter()
        .filter(|entry| {
            entry.get("kind").and_then(Value::as_str) == Some("execution_action_review_resolution")
                && entry.get("review_uid").and_then(Value::as_str) == Some(review_uid.as_str())
                && entry.get("generation").and_then(Value::as_u64) == Some(generation)
        })
        .collect::<Vec<_>>();
    assert_eq!(
        entries.len(),
        1,
        "review {review_uid} must have one generation-fenced outcome audit"
    );
    assert_eq!(entries[0].get("accepted"), Some(&json!(true)));
    Ok(())
}

fn controlled_task_projection(task: &ExecutionTaskRecord) -> Value {
    json!({
        "task_id": task.task_id,
        "status": task.status,
        "attempt": task.attempt,
        "generation": task.generation,
        "input": task.input,
        "resume_input_history": task.resume_input_history,
        "generation_history": task.generation_history,
        "reserved": task.reserved,
        "actual": task.actual,
        "actual_tasks": task.actual_tasks,
        "current_outcome": task.current_outcome,
        "output": task.output,
        "error": task.error,
        "citations": task.citations,
    })
}
