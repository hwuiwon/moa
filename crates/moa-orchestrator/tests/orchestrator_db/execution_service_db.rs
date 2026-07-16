//! DB contract coverage behind the public Execution service.

use chrono::{Duration, Utc};
use moa_artifacts::execution_plan::{
    ExecutionBudgetLimit, ExecutionCitation, ExecutionGoalContract, ExecutionPlanDefinition,
    ExecutionTaskOutcome, ExecutionTaskResult, ExecutionUsage, RetryPolicy,
};
use moa_core::{
    events::ExecutionTaskResultsRef,
    types::{
        execution_planning::{ExecutionRouteReason, ExecutionSourceProvenanceV1},
        identifiers::{SessionId, TenantId, UserId},
    },
};
use moa_execution::{
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash,
    },
    compiler::{CanonicalExecutionPlan, ExecutionValidationReport},
    completion::{
        CompletionEvaluation, CompletionStatus, execution_terminal_reason,
        terminal_evidence_from_evaluation,
    },
    repository::{
        ExecutionRepository, ExecutionScope, FinalizationOutcome, NewExecutionPlanningContext,
        NewExecutionRun, PlanningContextWriteOutcome, ReservationOutcome, RunFinalizationRequest,
        TaskOutcomeWrite, TransitionOutcome,
    },
    state::{
        ExecutionRunStatus, ExecutionTaskId, ExecutionTaskStatus, ExecutionTerminalCause,
        LogicalTask, LogicalTaskKind, TerminalProjection,
    },
    wire::{
        EXECUTION_TERMINAL_MAX_CITATION_IDS, ExecutionPlanningContextSnapshotV1,
        planning_context_hash,
    },
};
use serde_json::json;
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn execution_task_citation_lineage_survives_reload_and_terminal_summary_db() -> TestResult {
    // Pins: an exact completed ExecutionTask outcome keeps its source lineage through scoped
    // repository reload/replay and into the bounded, sorted, deduplicated terminal run summary.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let other_tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let other_scope = ExecutionScope::Tenant {
        tenant_id: other_tenant_id,
    };
    let session_id = SessionId::new();
    let owner_user_id = UserId::new("lineage-owner");
    let catalog = ExecutionCapabilityCatalog::build(Vec::new())?;
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: Vec::new(),
    };
    let budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(100),
        max_tokens: Some(100),
        max_tasks: Some(1),
        max_tool_calls: Some(10),
        max_retrieved_bytes: Some(1_000),
        deadline_at: Some(Utc::now() + Duration::hours(1)),
    };
    let planning_snapshot = ExecutionPlanningContextSnapshotV1 {
        schema_version: 1,
        tenant_id,
        contact_id: None,
        session_id,
        originating_user_sequence_num: 73,
        originating_user_event_hash: ExecutionHash::from_bytes([73; 32]).to_string(),
        owner_user_id: owner_user_id.clone(),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        pinned_instruction_skills: Vec::new(),
        execution_templates: Vec::new(),
        budget: budget.clone(),
    };
    let planning_hash = planning_context_hash(&planning_snapshot)?;
    let planning_context = repository
        .create_planning_context(
            scope,
            NewExecutionPlanningContext {
                snapshot: planning_snapshot,
                planning_context_hash: planning_hash,
            },
        )
        .await?;
    let PlanningContextWriteOutcome::Created(planning_context) = planning_context else {
        panic!("fresh execution lineage fixture must create its planning context");
    };
    let plan = CanonicalExecutionPlan {
        definition: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            nodes: Vec::new(),
        },
        plan_hash: ExecutionHash::from_bytes([11; 32]),
        catalog_hash: catalog.catalog_hash,
        estimate: ExecutionEstimate {
            cost_microusd: 10,
            tokens: 10,
            tasks: 1,
            tool_calls: 1,
            retrieved_bytes: 10,
        },
        report: ExecutionValidationReport::default(),
    };
    let run = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: 73,
                planning_context_uid: planning_context.planning_context_uid,
                planning_context_hash: planning_hash,
                owner_user_id,
                goal: ExecutionGoalContract {
                    objective: "preserve exact execution citation lineage".to_string(),
                    requirements: Vec::new(),
                    deliverables: Vec::new(),
                    coverage: Vec::new(),
                    constraints: Vec::new(),
                    completion_checks: Vec::new(),
                },
                plan,
                catalog,
                authorization,
                pinned_instruction_skills: Vec::new(),
                source_provenance: ExecutionSourceProvenanceV1::SkillTemplate {
                    route_reason: ExecutionRouteReason::SelectedExecutionTemplate,
                    skill_template_ref: "skill://execution-lineage".to_string(),
                    skill_template_revision_uid: Uuid::now_v7(),
                },
                input: json!({"query": "lineage"}),
                status: ExecutionRunStatus::Queued,
                approved_budget: budget,
                idempotency_key: Some("execution-lineage".to_string()),
            },
        )
        .await?;
    let TransitionOutcome::RunApplied(running) = repository
        .transition_run_wait(
            scope,
            run.run_uid,
            ExecutionRunStatus::Queued,
            ExecutionRunStatus::Running,
        )
        .await?
    else {
        panic!("execution lineage fixture must transition its run to running");
    };
    let task_id = ExecutionTaskId::derive(run.run_uid, "collect", "primary")?;
    let task = LogicalTask {
        task_id,
        node_id: "collect".to_string(),
        item_key: "primary".to_string(),
        requirement_ids: vec!["lineage".to_string()],
        plan_revision: running.plan_revision,
        generation: 1,
        input: json!({"source_set": "primary"}),
        kind: LogicalTaskKind::Output {
            value: json!({"answer": "durable"}),
        },
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        reservation: ExecutionEstimate {
            cost_microusd: 10,
            tokens: 10,
            tasks: 1,
            tool_calls: 1,
            retrieved_bytes: 10,
        },
    };
    let tasks = repository
        .materialize_tasks(scope, run.run_uid, running.plan_revision, vec![task])
        .await?;
    assert_eq!(tasks.len(), 1, "fixture must materialize exactly one task");
    assert_eq!(tasks[0].task_id, task_id);
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

    let citations = (0..=EXECUTION_TERMINAL_MAX_CITATION_IDS)
        .rev()
        .map(|index| ExecutionCitation {
            source_id: format!("source-{index:03}"),
            uri: Some(format!("https://sources.example/{index:03}")),
            locator: Some(json!({"page": index})),
        })
        .chain(std::iter::once(ExecutionCitation {
            source_id: "source-050".to_string(),
            uri: Some("https://sources.example/duplicate".to_string()),
            locator: Some(json!({"page": "duplicate"})),
        }))
        .collect::<Vec<_>>();
    let outcome = ExecutionTaskOutcome {
        schema_version: 1,
        usage: ExecutionUsage {
            cost_microusd: 1,
            tokens: 2,
            tool_calls: 1,
            retrieved_bytes: 3,
        },
        result: ExecutionTaskResult::Completed {
            output: json!({"answer": "durable"}),
            citations: citations.clone(),
        },
    };
    let TaskOutcomeWrite::Applied {
        run: applied_run,
        task: applied_task,
        budget_overrun: false,
    } = repository
        .record_task_outcome(scope, run.run_uid, task_id, 1, outcome.clone())
        .await?
    else {
        panic!("first completed task outcome must be applied");
    };
    assert_eq!(applied_task.run_uid, run.run_uid);
    assert_eq!(applied_task.tenant_id, tenant_id);
    assert_eq!(applied_task.task_id, task_id);
    assert_eq!(applied_task.status, ExecutionTaskStatus::Completed);
    assert_eq!(applied_task.current_outcome.as_ref(), Some(&outcome));
    assert_eq!(applied_task.citations, citations);

    let TaskOutcomeWrite::Replayed {
        run: replayed_run,
        task: replayed_task,
        budget_overrun: false,
    } = repository
        .record_task_outcome(scope, run.run_uid, task_id, 1, outcome.clone())
        .await?
    else {
        panic!("exact completed task outcome retry must replay");
    };
    assert_eq!(replayed_run, applied_run);
    assert_eq!(replayed_task, applied_task);
    assert_eq!(
        repository.load_task(scope, run.run_uid, task_id).await?,
        Some(applied_task.clone())
    );
    assert_eq!(
        repository
            .load_task(other_scope, run.run_uid, task_id)
            .await?,
        None,
        "another tenant must not load the execution task lineage"
    );
    assert_eq!(
        repository.load_run(other_scope, run.run_uid).await?,
        None,
        "another tenant must not load the owning execution run"
    );

    let completion = CompletionEvaluation {
        status: CompletionStatus::Completed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: Vec::new(),
    };
    let cause = ExecutionTerminalCause::Completion { limit_stop: None };
    let terminal = TerminalProjection::Completed {
        output: json!({"answer": "durable"}),
    };
    let finalization = RunFinalizationRequest {
        run_uid: run.run_uid,
        expected_revision: replayed_run.plan_revision,
        expected_wake_epoch: replayed_run.wake_epoch,
        terminal_projection: terminal.clone(),
        completion_evaluation: completion.clone(),
        terminal_evidence: terminal_evidence_from_evaluation(cause.clone(), &completion)?,
        terminal_reason: execution_terminal_reason(&cause, &terminal, &completion)?,
    };
    let FinalizationOutcome::Finalized(finalized_run) =
        repository.finalize_run(scope, finalization.clone()).await?
    else {
        panic!("execution lineage fixture must finalize exactly once");
    };
    assert_eq!(finalized_run.status, ExecutionRunStatus::Completed);
    let FinalizationOutcome::Replayed(replayed_finalized_run) =
        repository.finalize_run(scope, finalization).await?
    else {
        panic!("exact run finalization retry must replay");
    };
    assert_eq!(replayed_finalized_run, finalized_run);

    let delivery = repository
        .load_terminal_delivery(scope, run.run_uid)
        .await?
        .expect("completed execution run must expose terminal delivery");
    let expected_source_ids = (0..EXECUTION_TERMINAL_MAX_CITATION_IDS)
        .map(|index| format!("source-{index:03}"))
        .collect::<Vec<_>>();
    assert_eq!(delivery.status, ExecutionRunStatus::Completed);
    assert_eq!(delivery.summary.run_uid, run.run_uid);
    assert_eq!(delivery.summary.originating_user_sequence_num, 73);
    assert_eq!(delivery.summary.citation_ids, expected_source_ids);
    assert_eq!(
        delivery.summary.task_results,
        ExecutionTaskResultsRef::ExecutionTaskTable {
            run_uid: run.run_uid
        }
    );
    assert_eq!(
        repository
            .load_terminal_delivery(other_scope, run.run_uid)
            .await?,
        None,
        "another tenant must not derive the execution terminal summary"
    );
    Ok(())
}

#[tokio::test]
async fn execution_service_rows_require_parent_session_and_keep_authorization_immutable_db()
-> TestResult {
    // Pins: the service cannot persist an ownerless run and neither recovery nor
    // a later request can replace its immutable catalog/authorization snapshots.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool();
    let tenant_id = Uuid::new_v4();
    let run_uid = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let planning_context_uid = Uuid::new_v4();
    let hash = "0".repeat(64);
    sqlx::query(
        r#"
        INSERT INTO moa.execution_planning_context (
            planning_context_uid, tenant_id, session_id,
            originating_user_sequence_num, originating_user_event_hash,
            owner_user_id, planning_context_hash, snapshot
        ) VALUES ($1, $2, $3, 0, $4, 'owner', $4, '{}'::JSONB)
        "#,
    )
    .bind(planning_context_uid)
    .bind(tenant_id)
    .bind(session_id)
    .bind(&hash)
    .execute(pool)
    .await?;
    let missing_parent = sqlx::query(
        r#"
        INSERT INTO moa.execution_run (
            run_uid, tenant_id, session_id, originating_user_sequence_num,
            planning_context_uid, planning_context_hash, owner_user_id, goal_contract,
            initial_plan, active_plan, initial_plan_hash, active_plan_hash,
            capability_catalog, authorization_envelope, pinned_instruction_skills,
            source_provenance, source_kind, route_mode, route_reason, input, status
        ) VALUES ($1, $2, NULL, 0, $3, $4, 'owner', '{}'::JSONB, '{}'::JSONB, '{}'::JSONB,
                  $4, $4, '{}'::JSONB, '{}'::JSONB, '[]'::JSONB,
                  '{"kind":"generated_plan","route_reason":"explicit_run"}'::JSONB,
                  'generated_plan', 'run', 'explicit_run', '{}'::JSONB, 'queued')
        "#,
    )
    .bind(run_uid)
    .bind(tenant_id)
    .bind(planning_context_uid)
    .bind(&hash)
    .execute(pool)
    .await
    .expect_err("parent session is mandatory");
    assert_eq!(
        missing_parent
            .as_database_error()
            .and_then(|error| error.code()),
        Some(std::borrow::Cow::Borrowed("23502"))
    );

    sqlx::query(
        r#"
        INSERT INTO moa.execution_run (
            run_uid, tenant_id, session_id, originating_user_sequence_num,
            planning_context_uid, planning_context_hash, owner_user_id, goal_contract,
            initial_plan, active_plan, initial_plan_hash, active_plan_hash,
            capability_catalog, authorization_envelope, pinned_instruction_skills,
            source_provenance, source_kind, route_mode, route_reason,
            input, status, queued_at
        ) VALUES ($1, $2, $3, 0, $4, $5, 'owner', '{}'::JSONB, '{}'::JSONB, '{}'::JSONB,
                  $5, $5, '{"schema_version":1}'::JSONB,
                  '{"capability_refs":[],"skill_refs":[]}'::JSONB, '[]'::JSONB,
                  '{"kind":"generated_plan","route_reason":"explicit_run"}'::JSONB,
                  'generated_plan', 'run', 'explicit_run', '{}'::JSONB, 'queued', NOW())
        "#,
    )
    .bind(run_uid)
    .bind(tenant_id)
    .bind(session_id)
    .bind(planning_context_uid)
    .bind(hash)
    .execute(pool)
    .await?;
    let immutable = sqlx::query(
        "UPDATE moa.execution_run SET authorization_envelope = '{\"capability_refs\":[{\"name\":\"bash\",\"version\":\"1\"}],\"skill_refs\":[]}'::JSONB WHERE run_uid = $1",
    )
    .bind(run_uid)
    .execute(pool)
    .await
    .expect_err("authorization envelope must remain immutable");
    assert!(
        immutable
            .to_string()
            .contains("execution run immutable fields cannot change")
    );
    Ok(())
}
