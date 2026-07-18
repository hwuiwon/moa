//! Live Restate service coverage for one durable execution run.

mod execution_execution_support;

#[cfg(feature = "execution-planning-failpoints")]
#[path = "execution_run_service_e2e/admission_replay.rs"]
mod admission_replay;
#[path = "execution_run_service_e2e/bulk_and_recovery.rs"]
mod bulk_and_recovery;
#[path = "execution_run_service_e2e/evaluation.rs"]
mod evaluation;
#[path = "execution_run_service_e2e/observability.rs"]
mod observability;
#[path = "execution_run_service_e2e/replan_and_completion.rs"]
mod replan_and_completion;
#[path = "execution_run_service_e2e/routing.rs"]
mod routing;
#[path = "execution_run_service_e2e/task_lifecycle.rs"]
mod task_lifecycle;
#[path = "execution_run_service_e2e/terminal_matrix.rs"]
mod terminal_matrix;

use std::time::Duration;

use anyhow::{Context, Result};
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit, ExecutionGoalContract,
    ExecutionNode, ExecutionOperation, ExecutionPlanDefinition, ExecutionRequirement, RetryPolicy,
};
use moa_core::{
    config::ExecutionConfig,
    events::Event,
    types::execution_planning::{
        ExecutionAdmissionEstimate, ExecutionConfirmationEvidence, ExecutionEstimateMethodology,
        ExecutionRunAdmissionStatus, ExecutionRunStarted, ExecutionSourceProvenance,
        GeneratedPlanPlannerProvenance,
    },
    types::{
        contact::SessionActorRef,
        identifiers::{SessionId, TenantId, UserId},
    },
};
use moa_execution::{
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash,
    },
    compiler::{CompileExecutionRequest, compile},
    repository::{
        ConfirmationOutcome, ExecutionRepository, ExecutionScope, NewExecutionPlanningContext,
        NewExecutionRun, PlanningContextWriteOutcome, ReservationOutcome, TaskOutcomeWrite,
        TransitionOutcome,
    },
    state::{
        ExecutionRunStatus, ExecutionTaskId, ExecutionTaskStatus, ExecutionTerminalCause,
        LogicalTask, LogicalTaskKind,
    },
    wire::{
        ExecutionCancelRequest, ExecutionConfirmRequest, ExecutionMutationResponse,
        ExecutionPlanningContextRequest, ExecutionPlanningContextResponse,
        ExecutionPlanningContextSnapshot, ExecutionRunRequest, ExecutionStartRequest,
        ExecutionStartResponse, ExecutionStatusResponse, ExecutionTaskWorkflowRequest,
        planning_context_hash,
    },
};
use moa_orchestrator::objects::session::ExecutionRunStartedDelivery;
use moa_test_support::OrchestratorTestFixture;
use serde_json::json;

#[cfg(feature = "execution-planning-failpoints")]
#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, Redis, and execution-planning-failpoints"]
async fn execution_template_admission_replay_is_semantic_service_e2e() -> Result<()> {
    // Pins: a crash after the objective event commits but before Session records
    // its sequence replays through the real template/start path exactly once.
    admission_replay::run_execution_template_admission_replay().await
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn output_only_run_is_durable_detached_and_reaches_terminal_state() -> Result<()> {
    // Pins: Execution/start persists an immutable compiled snapshot, dispatches
    // its keyed run/task workflows, and status reads the terminal DB projection.
    let fixture = OrchestratorTestFixture::shared().await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("execution-output-only").await?;
    let session = test.client().get_session(session_id).await?;
    let objective = "return one durable structured value";
    let originating_user_sequence_num = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: objective.to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let planning: ExecutionPlanningContextResponse = test
        .client()
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
        .await?;
    let catalog = planning.snapshot.catalog.clone();
    let authorization = planning.snapshot.authorization.clone();
    let approved_budget = planning.snapshot.budget.clone();
    let compiled = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: objective.to_string(),
            requirements: vec![ExecutionRequirement {
                id: "result".to_string(),
                description: "return the expected result".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "output-schema".to_string(),
                description: "terminal output matches its schema".to_string(),
                requirement_ids: vec!["result".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object", "additionalProperties": false}),
            output_schema: json!({
                "type": "object",
                "properties": {"value": {"const": "durable"}},
                "required": ["value"],
                "additionalProperties": false
            }),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({
                    "type": "object",
                    "properties": {"value": {"const": "durable"}},
                    "required": ["value"],
                    "additionalProperties": false
                }),
                operation: ExecutionOperation::Output {
                    value: json!({"value": "durable"}),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: approved_budget.clone(),
        config: ExecutionConfig::default(),
        now: chrono::Utc::now(),
    })
    .compiled
    .context("output-only execution plan should compile")?;
    let source_provenance = test_source_provenance(&compiled.plan.plan_hash.to_string());

    let started: ExecutionStartResponse = test
        .client()
        .post_call(
            "/Execution/start",
            &ExecutionStartRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num,
                planning_context_uid: planning.planning_context_uid,
                planning_context_hash: planning.planning_context_hash,
                idempotency_key: Some(format!("execution-e2e-{session_id}")),
                compiled,
                run_input: json!({}),
                source_provenance,
            },
        )
        .await
        .context("start durable execution run")?;
    assert!(started.created);
    assert!(!started.confirmation_required);
    assert_eq!(started.run.queued_at, Some(started.run.created_at));

    let status_request = ExecutionRunRequest {
        tenant_id: session.tenant_id,
        contact_id: None,
        session_id,
        run_uid: started.run.run_uid,
    };
    let mut terminal = None;
    for _ in 0..100 {
        let status: ExecutionStatusResponse = test
            .client()
            .post_call("/Execution/status", &status_request)
            .await?;
        if status.run.status.is_terminal() {
            terminal = Some(status);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let terminal = terminal.context("execution run did not become terminal")?;
    assert_eq!(terminal.run.status, ExecutionRunStatus::Completed);
    assert_eq!(terminal.output, Some(json!({"value": "durable"})));
    assert_eq!(terminal.run.total_tasks, 1);
    assert_eq!(terminal.run.completed_tasks, 1);
    assert!(matches!(
        terminal.run.terminal_evidence,
        Some(moa_execution::state::ExecutionTerminalEvidence {
            cause: ExecutionTerminalCause::Completion { limit_stop: None },
            satisfied_requirement_count: 1,
            requirement_count: 1,
        })
    ));
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the fixture persists the same explicit immutable planning cohort as production"
)]
async fn create_test_planning_context(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    tenant_id: TenantId,
    session_id: SessionId,
    originating_user_sequence_num: u64,
    owner_user_id: UserId,
    catalog: ExecutionCapabilityCatalog,
    authorization: ExecutionAuthorizationEnvelope,
    budget: ExecutionBudgetLimit,
) -> Result<(uuid::Uuid, ExecutionHash)> {
    let snapshot = ExecutionPlanningContextSnapshot {
        schema_version: 1,
        tenant_id,
        contact_id: None,
        session_id,
        originating_user_sequence_num,
        originating_user_event_hash: ExecutionHash::from_bytes(
            [u8::try_from(originating_user_sequence_num).unwrap_or(u8::MAX); 32],
        )
        .to_string(),
        owner_user_id,
        catalog,
        authorization,
        pinned_instruction_skills: Vec::new(),
        execution_templates: Vec::new(),
        budget,
    };
    let hash = planning_context_hash(&snapshot)?;
    let persisted = repository
        .create_planning_context(
            scope,
            NewExecutionPlanningContext {
                snapshot,
                planning_context_hash: hash,
            },
        )
        .await?;
    match persisted {
        PlanningContextWriteOutcome::Created(record)
        | PlanningContextWriteOutcome::Replayed(record) => {
            Ok((record.planning_context_uid, record.planning_context_hash))
        }
        PlanningContextWriteOutcome::Conflict => {
            anyhow::bail!("test planning context conflicted with an existing origin")
        }
    }
}

fn test_source_provenance(final_plan_hash: &str) -> ExecutionSourceProvenance {
    ExecutionSourceProvenance::GeneratedPlan {
        planner: GeneratedPlanPlannerProvenance {
            model: "scripted-fixture".to_string(),
            prompt_version: "execution-run-service-e2e".to_string(),
            candidate_hash: "a".repeat(64),
            compiler_report_hash: "b".repeat(64),
            final_plan_hash: final_plan_hash.to_string(),
            repair_attempts: 0,
        },
    }
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn cancellation_preserves_preconfirmation_null_and_postqueue_timestamp() -> Result<()> {
    // Pins: Execution/cancel preserves whether the run ever entered the queue,
    // including on exact replay, instead of fabricating queue/start evidence.
    let fixture = OrchestratorTestFixture::shared().await?;
    let test = fixture.isolated().await;
    let session_id = test
        .create_session("execution-cancellation-queue-history")
        .await?;
    let session = test.client().get_session(session_id).await?;
    let owner_user_id = match session.created_by {
        Some(SessionActorRef::Identity { id }) => UserId::new(id.to_string()),
        other => anyhow::bail!("fixture session has no identity owner: {other:?}"),
    };
    let catalog = ExecutionCapabilityCatalog::build(Vec::new())?;
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: Vec::new(),
    };
    let approved_budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(1_000),
        max_tokens: Some(1_000),
        max_tasks: Some(1),
        max_tool_calls: Some(1),
        max_retrieved_bytes: Some(1_000),
        deadline_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
    };
    let compiled = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: "preserve cancellation queue history".to_string(),
            requirements: vec![ExecutionRequirement {
                id: "result".to_string(),
                description: "return a result if dispatched".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: Vec::new(),
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({"type": "object"}),
                operation: ExecutionOperation::Output {
                    value: json!({"value": "unused"}),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: approved_budget.clone(),
        config: ExecutionConfig::default(),
        now: chrono::Utc::now(),
    })
    .compiled
    .context("cancellation queue-history plan should compile")?;
    let repository = ExecutionRepository::new(
        sqlx::PgPool::connect(&fixture.postgres_url)
            .await
            .context("connect to fixture Postgres")?,
    );
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let (awaiting_context_uid, awaiting_context_hash) = create_test_planning_context(
        &repository,
        scope,
        session.tenant_id,
        session_id,
        1,
        owner_user_id.clone(),
        catalog.clone(),
        authorization.clone(),
        approved_budget.clone(),
    )
    .await?;
    let awaiting_source_provenance = test_source_provenance(&compiled.plan.plan_hash.to_string());

    let awaiting = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: 1,
                planning_context_uid: awaiting_context_uid,
                planning_context_hash: awaiting_context_hash,
                owner_user_id: owner_user_id.clone(),
                goal: compiled.goal.clone(),
                plan: compiled.plan.clone(),
                catalog: catalog.clone(),
                authorization: authorization.clone(),
                pinned_instruction_skills: Vec::new(),
                source_provenance: awaiting_source_provenance,
                input: json!({}),
                status: ExecutionRunStatus::AwaitingConfirmation,
                approved_budget: approved_budget.clone(),
                idempotency_key: Some(format!("preconfirm-cancel-{session_id}")),
            },
        )
        .await?;
    let preconfirm_request = ExecutionCancelRequest {
        run: ExecutionRunRequest {
            tenant_id: session.tenant_id,
            contact_id: None,
            session_id,
            run_uid: awaiting.run_uid,
        },
        reason: "cancel before confirmation".to_string(),
    };
    let applied: ExecutionMutationResponse = test
        .client()
        .post_call("/Execution/cancel", &preconfirm_request)
        .await?;
    assert!(matches!(
        applied,
        ExecutionMutationResponse::Applied { ref run }
            if run.status == ExecutionRunStatus::Cancelled && run.queued_at.is_none()
    ));
    let preconfirm = repository
        .load_run(scope, awaiting.run_uid)
        .await?
        .context("pre-confirm cancelled run should remain queryable")?;
    assert!(preconfirm.queued_at.is_none());
    assert!(preconfirm.confirmed_at.is_none());
    assert!(preconfirm.confirmed_plan_hash.is_none());
    assert!(preconfirm.started_at.is_none());
    let replay: ExecutionMutationResponse = test
        .client()
        .post_call("/Execution/cancel", &preconfirm_request)
        .await?;
    assert!(matches!(
        replay,
        ExecutionMutationResponse::Replayed { ref run }
            if run.status == ExecutionRunStatus::Cancelled && run.queued_at.is_none()
    ));

    let (queued_context_uid, queued_context_hash) = create_test_planning_context(
        &repository,
        scope,
        session.tenant_id,
        session_id,
        2,
        owner_user_id.clone(),
        catalog.clone(),
        authorization.clone(),
        approved_budget.clone(),
    )
    .await?;
    let queued_source_provenance = test_source_provenance(&compiled.plan.plan_hash.to_string());
    let queued = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: 2,
                planning_context_uid: queued_context_uid,
                planning_context_hash: queued_context_hash,
                owner_user_id,
                goal: compiled.goal,
                plan: compiled.plan,
                catalog,
                authorization,
                pinned_instruction_skills: Vec::new(),
                source_provenance: queued_source_provenance,
                input: json!({}),
                status: ExecutionRunStatus::Queued,
                approved_budget,
                idempotency_key: Some(format!("postqueue-cancel-{session_id}")),
            },
        )
        .await?;
    let queued_at = queued
        .queued_at
        .context("direct queued run must have a queue timestamp")?;
    let postqueue: ExecutionMutationResponse = test
        .client()
        .post_call(
            "/Execution/cancel",
            &ExecutionCancelRequest {
                run: ExecutionRunRequest {
                    tenant_id: session.tenant_id,
                    contact_id: None,
                    session_id,
                    run_uid: queued.run_uid,
                },
                reason: "cancel after queue".to_string(),
            },
        )
        .await?;
    assert!(matches!(
        postqueue,
        ExecutionMutationResponse::Applied { ref run }
            if run.status == ExecutionRunStatus::Cancelled
                && run.queued_at == Some(queued_at)
    ));
    assert_eq!(
        repository
            .load_run(scope, queued.run_uid)
            .await?
            .context("post-queue cancelled run should remain queryable")?
            .queued_at,
        Some(queued_at)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn wake_after_db_ack_before_workflow_state_advance_is_not_lost() -> Result<()> {
    // Pins: a persisted wake delivered in the exact ack-to-workflow-state gap
    // either prevents parking or resolves the promise advertised before the CAS.
    run_wake_handoff_case("delay").await
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn wake_after_db_ack_survives_execution_run_failure_and_restart() -> Result<()> {
    // Pins: a wake committed after DB acknowledgement remains attached to the
    // same promise across one forced handler failure and Restate replay/restart.
    run_wake_handoff_case("crash_once").await
}

async fn run_wake_handoff_case(mode: &str) -> Result<()> {
    let fixture = OrchestratorTestFixture::with_script_and_env(
        json!({
            "default": {
                "completion": {
                    "content": "ok",
                    "duration_ms": 1,
                    "input_tokens": 1,
                    "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0,
                    "tool_calls": []
                }
            }
        }),
        vec![(
            "MOA_EXECUTION_TEST_WAKE_HANDOFF".to_string(),
            mode.to_string(),
        )],
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test
        .create_session(&format!("execution-wake-handoff-{mode}"))
        .await?;
    let session = test.client().get_session(session_id).await?;
    let originating_user_sequence_num = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: "complete after a wake handoff".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let owner_user_id = match session.created_by {
        Some(SessionActorRef::Identity { id }) => UserId::new(id.to_string()),
        other => anyhow::bail!("fixture session has no identity owner: {other:?}"),
    };
    let catalog = ExecutionCapabilityCatalog::build(Vec::new())?;
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: Vec::new(),
    };
    let approved_budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(1),
        max_tokens: Some(1),
        max_tasks: Some(1),
        max_tool_calls: Some(1),
        max_retrieved_bytes: Some(1),
        deadline_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
    };
    let compiled = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: "complete after a wake handoff".to_string(),
            requirements: vec![ExecutionRequirement {
                id: "result".to_string(),
                description: "persist the handoff result".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "output-schema".to_string(),
                description: "terminal output matches its schema".to_string(),
                requirement_ids: vec!["result".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({"type": "object"}),
                operation: ExecutionOperation::Output {
                    value: json!({"handoff": "completed"}),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: approved_budget.clone(),
        config: ExecutionConfig::default(),
        now: chrono::Utc::now(),
    })
    .compiled
    .context("wake handoff plan should compile")?;
    let pool = sqlx::PgPool::connect(&fixture.postgres_url).await?;
    let repository = ExecutionRepository::new(pool);
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let (planning_context_uid, planning_context_hash) = create_test_planning_context(
        &repository,
        scope,
        session.tenant_id,
        session_id,
        originating_user_sequence_num,
        owner_user_id.clone(),
        catalog.clone(),
        authorization.clone(),
        approved_budget.clone(),
    )
    .await?;
    let source_provenance = test_source_provenance(&compiled.plan.plan_hash.to_string());
    let run = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num,
                planning_context_uid,
                planning_context_hash,
                owner_user_id,
                goal: compiled.goal,
                plan: compiled.plan,
                catalog,
                authorization,
                pinned_instruction_skills: Vec::new(),
                source_provenance,
                input: json!({}),
                status: ExecutionRunStatus::AwaitingConfirmation,
                approved_budget: approved_budget.clone(),
                idempotency_key: Some(format!("wake-handoff-{mode}-{session_id}")),
            },
        )
        .await?;
    assert!(run.queued_at.is_none());
    let initial_epoch = run.wake_epoch;
    test.client()
        .post_void(
            &format!("/Session/{session_id}/execution_run_started"),
            &ExecutionRunStartedDelivery {
                started: ExecutionRunStarted {
                    run_uid: run.run_uid,
                    originating_user_sequence_num,
                    plan_revision: run.plan_revision,
                    status: ExecutionRunAdmissionStatus::AwaitingConfirmation,
                    confirmation: Some(ExecutionConfirmationEvidence {
                        active_plan_hash: run.active_plan_hash.to_string(),
                        estimate: ExecutionAdmissionEstimate {
                            cost_microusd: run.active_plan.estimate.cost_microusd,
                            tokens: run.active_plan.estimate.tokens,
                            tasks: run.active_plan.estimate.tasks,
                            tool_calls: run.active_plan.estimate.tool_calls,
                            retrieved_bytes: run.active_plan.estimate.retrieved_bytes,
                        },
                        methodology: ExecutionEstimateMethodology::ConservativeWorstCase,
                    }),
                },
                approved_budget: run.approved_budget.clone(),
            },
        )
        .await
        .context("activate the wake-handoff run through Session")?;

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let observed = repository
                .load_run(scope, run.run_uid)
                .await?
                .context("wake handoff run disappeared")?;
            if observed.processed_wake_epoch == initial_epoch {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("driver never reached the post-ack injection checkpoint")??;

    let ConfirmationOutcome::Confirmed(confirmed) = repository
        .confirm_run(
            scope,
            run.run_uid,
            &run.active_plan_hash,
            approved_budget.clone(),
        )
        .await?
    else {
        anyhow::bail!("wake handoff confirmation did not apply");
    };
    let queued_at = confirmed
        .queued_at
        .context("confirmation must persist queued_at")?;
    let replay: ExecutionMutationResponse = test
        .client()
        .post_call(
            "/Execution/confirm",
            &ExecutionConfirmRequest {
                run: ExecutionRunRequest {
                    tenant_id: session.tenant_id,
                    contact_id: None,
                    session_id,
                    run_uid: run.run_uid,
                },
                expected_plan_hash: run.active_plan_hash,
                approved_budget,
            },
        )
        .await?;
    assert!(matches!(
        replay,
        ExecutionMutationResponse::Replayed { ref run }
            if run.run_uid == confirmed.run_uid && run.queued_at == Some(queued_at)
    ));

    let terminal = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let current = repository
                .load_run(scope, run.run_uid)
                .await?
                .context("completed wake handoff run disappeared")?;
            if current.status.is_terminal() {
                return Ok::<_, anyhow::Error>(current);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("wake was lost across the handoff checkpoint")??;
    assert_eq!(terminal.status, ExecutionRunStatus::Completed);
    assert_eq!(terminal.output, Some(json!({"handoff": "completed"})));
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn waiting_replan_with_exhausted_budget_finalizes_without_amendment() -> Result<()> {
    // Pins: a waiting-replan task whose consumed plus reserved task capacity
    // exactly exhausts the approved envelope cannot park forever waiting for a
    // Task 7 amendment; ExecutionRun records evidence and terminates immediately.
    let fixture = OrchestratorTestFixture::shared().await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("execution-replan-budget-stop").await?;
    let session = test.client().get_session(session_id).await?;
    let originating_user_sequence_num = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: "stop an exhausted replan wait".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let owner_user_id = match session.created_by {
        Some(SessionActorRef::Identity { id }) => UserId::new(id.to_string()),
        other => anyhow::bail!("fixture session has no identity owner: {other:?}"),
    };
    let catalog = ExecutionCapabilityCatalog::build(Vec::new())?;
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: Vec::new(),
    };
    let approved_budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(100),
        max_tokens: Some(1),
        max_tasks: Some(3),
        max_tool_calls: Some(100),
        max_retrieved_bytes: Some(100),
        deadline_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
    };
    let compiled = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: "stop an exhausted replan wait".to_string(),
            requirements: vec![ExecutionRequirement {
                id: "result".to_string(),
                description: "return a result when resources permit".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "output-schema".to_string(),
                description: "terminal output matches its schema".to_string(),
                requirement_ids: vec!["result".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({"type": "object"}),
                operation: ExecutionOperation::Output {
                    value: json!({"value": "unreachable"}),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: approved_budget.clone(),
        config: ExecutionConfig::default(),
        now: chrono::Utc::now(),
    })
    .compiled
    .context("waiting-replan fixture plan should compile")?;
    let pool = sqlx::PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect to fixture Postgres")?;
    let repository = ExecutionRepository::new(pool);
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let (planning_context_uid, planning_context_hash) = create_test_planning_context(
        &repository,
        scope,
        session.tenant_id,
        session_id,
        originating_user_sequence_num,
        owner_user_id.clone(),
        catalog.clone(),
        authorization.clone(),
        approved_budget.clone(),
    )
    .await?;
    let source_provenance = test_source_provenance(&compiled.plan.plan_hash.to_string());
    let run = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num,
                planning_context_uid,
                planning_context_hash,
                owner_user_id,
                goal: compiled.goal,
                plan: compiled.plan,
                catalog,
                authorization,
                pinned_instruction_skills: Vec::new(),
                source_provenance,
                input: json!({}),
                status: ExecutionRunStatus::Queued,
                approved_budget,
                idempotency_key: Some(format!("replan-budget-stop-{session_id}")),
            },
        )
        .await?;
    let task_id = ExecutionTaskId::derive(run.run_uid, "output", "")?;
    let task = LogicalTask {
        task_id,
        node_id: "output".to_string(),
        item_key: String::new(),
        requirement_ids: vec!["result".to_string()],
        plan_revision: 1,
        generation: 1,
        input: json!({}),
        kind: LogicalTaskKind::Output {
            value: json!({"value": "unreachable"}),
        },
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        reservation: ExecutionEstimate {
            cost_microusd: 0,
            tokens: 1,
            tasks: 1,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
    };
    let pending_task = LogicalTask {
        task_id: ExecutionTaskId::derive(run.run_uid, "output", "pending_cleanup")?,
        node_id: "output".to_string(),
        item_key: "pending_cleanup".to_string(),
        requirement_ids: vec!["result".to_string()],
        plan_revision: 1,
        generation: 1,
        input: json!({}),
        kind: LogicalTaskKind::Output { value: json!({}) },
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        reservation: ExecutionEstimate {
            cost_microusd: 0,
            tokens: 0,
            tasks: 1,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
    };
    let running_task = LogicalTask {
        task_id: ExecutionTaskId::derive(run.run_uid, "output", "running_cleanup")?,
        node_id: "output".to_string(),
        item_key: "running_cleanup".to_string(),
        requirement_ids: vec!["result".to_string()],
        plan_revision: 1,
        generation: 1,
        input: json!({}),
        kind: LogicalTaskKind::Output { value: json!({}) },
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        reservation: ExecutionEstimate {
            cost_microusd: 0,
            tokens: 0,
            tasks: 1,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
    };
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![task, pending_task.clone(), running_task.clone()],
        )
        .await?;
    assert!(matches!(
        repository
            .reserve_task(scope, run.run_uid, task_id, 1)
            .await?,
        ReservationOutcome::Reserved(_)
    ));
    assert!(matches!(
        repository
            .mark_task_running(scope, run.run_uid, task_id, 1)
            .await?,
        TransitionOutcome::Applied(_)
    ));
    assert!(matches!(
        repository
            .reserve_task(scope, run.run_uid, running_task.task_id, 1)
            .await?,
        ReservationOutcome::Reserved(_)
    ));
    assert!(matches!(
        repository
            .mark_task_running(scope, run.run_uid, running_task.task_id, 1)
            .await?,
        TransitionOutcome::Applied(_)
    ));
    assert!(matches!(
        repository
            .record_task_outcome(
                scope,
                run.run_uid,
                task_id,
                1,
                moa_artifacts::execution_plan::ExecutionTaskOutcome {
                    schema_version: 1,
                    usage: moa_artifacts::execution_plan::ExecutionUsage {
                        cost_microusd: 0,
                        tokens: 0,
                        tool_calls: 0,
                        retrieved_bytes: 0,
                    },
                    result: moa_artifacts::execution_plan::ExecutionTaskResult::NeedsReplan {
                        reason: "replacement requires exhausted resources".to_string(),
                        evidence: json!({"reserved_task_capacity": 1}),
                    },
                },
            )
            .await?,
        TaskOutcomeWrite::Applied {
            budget_overrun: false,
            ..
        }
    ));

    test.client()
        .post_void(
            &format!("/Session/{session_id}/execution_run_started"),
            &ExecutionRunStartedDelivery {
                started: ExecutionRunStarted {
                    run_uid: run.run_uid,
                    originating_user_sequence_num,
                    plan_revision: run.plan_revision,
                    status: ExecutionRunAdmissionStatus::Queued,
                    confirmation: None,
                },
                approved_budget: run.approved_budget.clone(),
            },
        )
        .await
        .context("activate the exhausted replan run through Session")?;
    let finalized = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = repository
                .load_run(scope, run.run_uid)
                .await?
                .context("finalized run should remain queryable")?;
            if current.status.is_terminal() {
                return Ok::<_, anyhow::Error>(current);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("ExecutionRun parked instead of finalizing exhausted replan")??;
    assert_eq!(finalized.status, ExecutionRunStatus::Blocked);
    assert!(finalized.completed_at.is_some());
    assert!(matches!(
        finalized
            .terminal_evidence
            .as_ref()
            .map(|evidence| &evidence.cause),
        Some(ExecutionTerminalCause::ReplanStop {
            reason: moa_execution::ReplanStopReason::BudgetExhausted,
        })
    ));
    assert!(
        finalized
            .terminal_gaps
            .iter()
            .any(|gap| gap == "replan stopped: budget exhausted: tokens"),
        "terminal evidence should name the exhausted replan stop: {:?}",
        finalized.terminal_gaps
    );
    assert_eq!(finalized.reserved, ExecutionEstimate::default());
    assert_eq!(finalized.consumed.tasks, 3);
    assert_eq!(finalized.progress_cancelled_tasks, 3);
    let cancelled_task = repository
        .load_task(scope, run.run_uid, task_id)
        .await?
        .context("stopped waiting-replan task should remain queryable")?;
    assert_eq!(cancelled_task.status, ExecutionTaskStatus::Cancelled);
    assert_eq!(cancelled_task.reserved, ExecutionEstimate::default());
    assert_eq!(cancelled_task.actual_tasks, 1);
    assert!(matches!(
        cancelled_task.current_outcome.as_ref().map(|outcome| &outcome.result),
        Some(moa_artifacts::execution_plan::ExecutionTaskResult::Cancelled { reason })
            if reason == "replan stopped: budget exhausted: tokens"
    ));
    assert!(cancelled_task.outcome_audit.iter().any(|entry| {
        entry.get("kind").and_then(serde_json::Value::as_str) == Some("replan_stopped")
            && entry
                .get("terminal_status")
                .and_then(serde_json::Value::as_str)
                == Some("blocked")
    }));
    for task_id in [pending_task.task_id, running_task.task_id] {
        let task = repository
            .load_task(scope, run.run_uid, task_id)
            .await?
            .context("every active task should remain queryable after replan stop")?;
        assert_eq!(task.status, ExecutionTaskStatus::Cancelled);
        assert_eq!(task.reserved, ExecutionEstimate::default());
        assert!(matches!(
            task.current_outcome.as_ref().map(|outcome| &outcome.result),
            Some(moa_artifacts::execution_plan::ExecutionTaskResult::Cancelled { reason })
                if reason == "replan stopped: budget exhausted: tokens"
        ));
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn elapsed_reservation_persists_typed_failure_and_run_finalizes() -> Result<()> {
    // Pins: a deadline that elapses before ExecutionTask reserves work becomes
    // a generation-fenced DeadlineExceeded outcome, wakes the run, and leaves
    // neither the task nor run parked for retry.
    let fixture = OrchestratorTestFixture::shared().await?;
    let test = fixture.isolated().await;
    let session_id = test
        .create_session("execution-reservation-deadline")
        .await?;
    let session = test.client().get_session(session_id).await?;
    let originating_user_sequence_num = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: "record an elapsed reservation".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let owner_user_id = match session.created_by {
        Some(SessionActorRef::Identity { id }) => UserId::new(id.to_string()),
        other => anyhow::bail!("fixture session has no identity owner: {other:?}"),
    };
    let catalog = ExecutionCapabilityCatalog::build(Vec::new())?;
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: Vec::new(),
    };
    let compile_budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(1),
        max_tokens: Some(1),
        max_tasks: Some(1),
        max_tool_calls: Some(1),
        max_retrieved_bytes: Some(1),
        deadline_at: Some(chrono::Utc::now() + chrono::Duration::minutes(5)),
    };
    let compiled = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: "record an elapsed reservation".to_string(),
            requirements: vec![ExecutionRequirement {
                id: "result".to_string(),
                description: "record the terminal reservation outcome".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: Vec::new(),
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({"type": "object"}),
                operation: ExecutionOperation::Output {
                    value: json!({"value": "late"}),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: compile_budget.clone(),
        config: ExecutionConfig::default(),
        now: chrono::Utc::now(),
    })
    .compiled
    .context("elapsed reservation fixture plan should compile")?;
    let repository = ExecutionRepository::new(
        sqlx::PgPool::connect(&fixture.postgres_url)
            .await
            .context("connect to fixture Postgres")?,
    );
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let runtime_budget = ExecutionBudgetLimit {
        deadline_at: Some(chrono::Utc::now() - chrono::Duration::seconds(1)),
        ..compile_budget
    };
    let (planning_context_uid, planning_context_hash) = create_test_planning_context(
        &repository,
        scope,
        session.tenant_id,
        session_id,
        originating_user_sequence_num,
        owner_user_id.clone(),
        catalog.clone(),
        authorization.clone(),
        runtime_budget.clone(),
    )
    .await?;
    let source_provenance = test_source_provenance(&compiled.plan.plan_hash.to_string());
    let run = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num,
                planning_context_uid,
                planning_context_hash,
                owner_user_id,
                goal: compiled.goal,
                plan: compiled.plan,
                catalog,
                authorization,
                pinned_instruction_skills: Vec::new(),
                source_provenance,
                input: json!({}),
                status: ExecutionRunStatus::Queued,
                approved_budget: runtime_budget,
                idempotency_key: Some(format!("elapsed-reservation-{session_id}")),
            },
        )
        .await?;
    let task_id = ExecutionTaskId::derive(run.run_uid, "output", "")?;
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![LogicalTask {
                task_id,
                node_id: "output".to_string(),
                item_key: String::new(),
                requirement_ids: vec!["result".to_string()],
                plan_revision: 1,
                generation: 1,
                input: json!({}),
                kind: LogicalTaskKind::Output {
                    value: json!({"value": "late"}),
                },
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                reservation: ExecutionEstimate {
                    cost_microusd: 0,
                    tokens: 0,
                    tasks: 1,
                    tool_calls: 0,
                    retrieved_bytes: 0,
                },
            }],
        )
        .await?;
    test.client()
        .post_void(
            &format!("/ExecutionTask/{task_id}/run"),
            &ExecutionTaskWorkflowRequest {
                run_uid: run.run_uid,
                task_id,
                generation: 1,
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
            },
        )
        .await
        .context("reservation rejection should complete the task workflow")?;
    test.client()
        .post_void(
            &format!("/Session/{session_id}/execution_run_started"),
            &ExecutionRunStartedDelivery {
                started: ExecutionRunStarted {
                    run_uid: run.run_uid,
                    originating_user_sequence_num,
                    plan_revision: run.plan_revision,
                    status: ExecutionRunAdmissionStatus::Queued,
                    confirmation: None,
                },
                approved_budget: run.approved_budget.clone(),
            },
        )
        .await
        .context("activate the directly persisted run through Session")?;
    let mut finalized = None;
    for _ in 0..200 {
        let current = repository
            .load_run(scope, run.run_uid)
            .await?
            .context("elapsed reservation run should remain queryable")?;
        if current.status.is_terminal() {
            finalized = Some(current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let finalized = finalized.context("typed reservation outcome should finalize the run")?;
    let failed_task = repository
        .load_task(scope, run.run_uid, task_id)
        .await?
        .context("failed reservation task should remain queryable")?;
    assert_eq!(failed_task.status, ExecutionTaskStatus::Failed);
    assert!(matches!(
        failed_task.current_outcome.map(|outcome| outcome.result),
        Some(moa_artifacts::execution_plan::ExecutionTaskResult::Failed {
            class: moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
            ..
        })
    ));
    assert!(finalized.status.is_terminal());
    assert!(finalized.completed_at.is_some());
    Ok(())
}
