//! Live Restate service coverage for one durable execution run.

mod execution_execution_support;

#[cfg(feature = "execution-planning-failpoints")]
#[path = "execution_run_service_e2e/admission_replay.rs"]
mod admission_replay;
#[path = "execution_run_service_e2e/bulk_and_recovery.rs"]
mod bulk_and_recovery;
#[path = "execution_run_service_e2e/compensation_recovery.rs"]
mod compensation_recovery;
#[path = "execution_run_service_e2e/controller_activation.rs"]
mod controller_activation;
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
use moa_config::ExecutionConfig;
use moa_core::{
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
    capability::{ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionHash},
    compiler::{CompileExecutionRequest, compile},
    repository::{
        ExecutionRepository, ExecutionScope, NewExecutionRun,
        audit::{NewExecutionPlanningContext, PlanningContextWriteOutcome},
        run::RunAdmissionOutcome,
    },
    state::{ExecutionRunStatus, ExecutionTerminalCause},
    wire::{
        ExecutionCancelRequest, ExecutionMutationResponse, ExecutionPlanningContextRequest,
        ExecutionPlanningContextResponse, ExecutionPlanningContextSnapshot, ExecutionRunRequest,
        ExecutionStartRequest, ExecutionStartResponse, ExecutionStatusResponse,
        planning_context_hash,
    },
};
use moa_orchestrator::objects::session::ExecutionRunStartedDelivery;
use moa_test_support::OrchestratorTestFixture;
use serde_json::json;

use execution_execution_support::fixtures::await_execution_terminal;

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
                deadline_at: chrono::Utc::now() + chrono::TimeDelta::days(1),
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
            cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
            input_wait_policy: moa_artifacts::execution_plan::ExecutionWaitPolicy {
                expiry: moa_artifacts::execution_plan::ExecutionTemporalTarget::After {
                    delay_seconds: 86_400,
                },
                on_expiry: moa_artifacts::execution_plan::ExecutionWaitExpiryAction::FailTask,
            },
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
                compensation: None,
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
        now: moa_test_support::fixtures::pg_now(),
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
        deadline_at: Some(moa_test_support::fixtures::pg_now() + chrono::Duration::minutes(5)),
    };
    let compile_outcome = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: "preserve cancellation queue history".to_string(),
            requirements: vec![ExecutionRequirement {
                id: "result".to_string(),
                description: "return a result if dispatched".to_string(),
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
            cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
            input_wait_policy: moa_artifacts::execution_plan::ExecutionWaitPolicy {
                expiry: moa_artifacts::execution_plan::ExecutionTemporalTarget::After {
                    delay_seconds: 86_400,
                },
                on_expiry: moa_artifacts::execution_plan::ExecutionWaitExpiryAction::FailTask,
            },
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
                compensation: None,
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
        now: moa_test_support::fixtures::pg_now(),
    });
    let compiled = compile_outcome.compiled.with_context(|| {
        format!(
            "cancellation queue-history plan should compile: {:?}",
            compile_outcome.report.issues
        )
    })?;
    let repository = ExecutionRepository::new(
        sqlx::PgPool::connect(&fixture.postgres_url)
            .await
            .context("connect to fixture Postgres")?,
    );
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let awaiting_origin_sequence = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: "cancel before confirmation".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let (awaiting_context_uid, awaiting_context_hash) = create_test_planning_context(
        &repository,
        scope,
        session.tenant_id,
        session_id,
        awaiting_origin_sequence,
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
            &ExecutionConfig::default(),
            NewExecutionRun {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: awaiting_origin_sequence,
                planning_context_uid: awaiting_context_uid,
                planning_context_hash: awaiting_context_hash,
                owner_user_id: owner_user_id.clone(),
                admitted_identity: test
                    .client()
                    .identity()
                    .cloned()
                    .context("fixture client must carry an admitted identity")?,
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
    let RunAdmissionOutcome::Admitted(awaiting) = awaiting else {
        anyhow::bail!("preconfirmation run was not admitted: {awaiting:?}")
    };
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
            if run.status == ExecutionRunStatus::AwaitingConfirmation
                && run.queued_at.is_none()
                && run.completed_at.is_none()
    ));
    test.client()
        .post_void(
            &format!("/Session/{session_id}/execution_run_started"),
            &ExecutionRunStartedDelivery {
                started: ExecutionRunStarted {
                    run_uid: awaiting.run_uid,
                    originating_user_sequence_num: awaiting_origin_sequence,
                    plan_revision: awaiting.plan_revision,
                    status: ExecutionRunAdmissionStatus::AwaitingConfirmation,
                    confirmation: Some(ExecutionConfirmationEvidence {
                        active_plan_hash: awaiting.active_plan_hash.to_string(),
                        estimate: ExecutionAdmissionEstimate {
                            cost_microusd: awaiting.active_plan.estimate.cost_microusd,
                            tokens: awaiting.active_plan.estimate.tokens,
                            tasks: awaiting.active_plan.estimate.tasks,
                            tool_calls: awaiting.active_plan.estimate.tool_calls,
                            retrieved_bytes: awaiting.active_plan.estimate.retrieved_bytes,
                        },
                        methodology: ExecutionEstimateMethodology::ConservativeWorstCase,
                    }),
                },
                approved_budget: awaiting.approved_budget.clone(),
            },
        )
        .await
        .context("activate the pre-confirmation cancellation fence")?;
    let terminal = await_execution_terminal(test.client(), &preconfirm_request.run).await?;
    assert_eq!(terminal.run.status, ExecutionRunStatus::Cancelled);
    assert!(terminal.run.queued_at.is_none());
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

    let queued_origin_sequence = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: "cancel after queue admission".to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let (queued_context_uid, queued_context_hash) = create_test_planning_context(
        &repository,
        scope,
        session.tenant_id,
        session_id,
        queued_origin_sequence,
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
            &ExecutionConfig::default(),
            NewExecutionRun {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: queued_origin_sequence,
                planning_context_uid: queued_context_uid,
                planning_context_hash: queued_context_hash,
                owner_user_id,
                admitted_identity: test
                    .client()
                    .identity()
                    .cloned()
                    .context("fixture client must carry an admitted identity")?,
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
    let RunAdmissionOutcome::Admitted(queued) = queued else {
        anyhow::bail!("postqueue run was not admitted: {queued:?}")
    };
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
            if run.status == ExecutionRunStatus::Queued
                && run.queued_at == Some(queued_at)
                && run.completed_at.is_none()
    ));
    test.client()
        .post_void(
            &format!("/Session/{session_id}/execution_run_started"),
            &ExecutionRunStartedDelivery {
                started: ExecutionRunStarted {
                    run_uid: queued.run_uid,
                    originating_user_sequence_num: queued_origin_sequence,
                    plan_revision: queued.plan_revision,
                    status: ExecutionRunAdmissionStatus::Queued,
                    confirmation: None,
                },
                approved_budget: queued.approved_budget.clone(),
            },
        )
        .await
        .context("activate the post-queue cancellation fence")?;
    let terminal = await_execution_terminal(
        test.client(),
        &ExecutionRunRequest {
            tenant_id: session.tenant_id,
            contact_id: None,
            session_id,
            run_uid: queued.run_uid,
        },
    )
    .await?;
    assert_eq!(terminal.run.status, ExecutionRunStatus::Cancelled);
    assert_eq!(terminal.run.queued_at, Some(queued_at));
    let postqueue = repository
        .load_run(scope, queued.run_uid)
        .await?
        .context("post-queue cancelled run should remain queryable")?;
    assert_eq!(postqueue.queued_at, Some(queued_at));
    assert!(postqueue.confirmed_at.is_none());
    assert!(postqueue.started_at.is_none());
    Ok(())
}
