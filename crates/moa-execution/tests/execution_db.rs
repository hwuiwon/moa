//! Concurrent PostgreSQL contract coverage for durable execution-run persistence.

use std::sync::Arc;

use chrono::{Duration, Utc};
use moa_artifacts::execution_plan::{
    ExecutionBudgetLimit, ExecutionCitation, ExecutionFailureClass, ExecutionGoalContract,
    ExecutionNode, ExecutionOperation, ExecutionRequirement, ExecutionTaskOutcome,
    ExecutionTaskResult, ExecutionUsage, PlanAmendment, RetryPolicy,
};
use moa_core::events::ExecutionTaskResultsRef;
use moa_core::types::{
    contact::ContactId,
    execution_planning::{
        ExecutionAuditReport, ExecutionCompileOutcome, ExecutionCompileSource,
        ExecutionPlannerCallKind, ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope,
        ExecutionPlanningAuditPayload, ExecutionRouteClassifierOutcome, ExecutionRouteDecision,
        ExecutionRouteKind, ExecutionRouteProvenance, ExecutionRouteSource, ExecutionRouteStage,
        ExecutionRouteUsage, ExecutionSourceProvenance, ExecutionStrategy, canonical_json_bytes,
    },
    identifiers::{SessionId, TenantId, UserId},
};
use moa_execution::{
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash, amendment_hash,
    },
    compiler::{CanonicalExecutionPlan, ExecutionValidationReport},
    completion::{
        CompletionEvaluation, CompletionStatus, cancellation_terminal_evidence,
        execution_terminal_reason, terminal_evidence_from_evaluation,
    },
    replan::{ReplanStopReason, failure_fingerprint},
    repository::{
        ActionReviewResolutionWrite, AmendmentReplayOutcome, AmendmentWrite, CancellationOutcome,
        CancellationRequest, CompileAuditWriteOutcome, ConfirmationConflict, ConfirmationOutcome,
        ExecutionNodeMaterialization, ExecutionRepository, ExecutionRunPageRequest, ExecutionScope,
        ExecutionTaskPageRequest, ExecutionTaskRecord, FinalizationOutcome, MaterializationOutcome,
        NewExecutionPlanningContext, NewExecutionRun, PlannerCallAuditWriteOutcome,
        PlanningContextWriteOutcome, ReplanStopRequest, ReservationOutcome, ReservationRejection,
        RouteAuditWriteOutcome, RunFinalizationRequest, TaskOutcomeRejection, TaskOutcomeWrite,
        TransitionOutcome, ValidatedAmendment, WakeAckOutcome,
    },
    state::{
        ExecutionLimitStop, ExecutionRunStatus, ExecutionSourceKind, ExecutionTaskId,
        ExecutionTaskStatus, ExecutionTerminalCause, ExecutionTerminalReason,
        FailureFingerprintInput, LogicalTask, LogicalTaskKind, TerminalProjection,
    },
    wire::{
        ExecutionActionReviewResolution, ExecutionPlanningContextSnapshot,
        execution_progress_from_run, planning_context_hash,
    },
};
use serde_json::json;
use tokio::{sync::Barrier, task::JoinSet};
use uuid::Uuid;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

#[tokio::test]
async fn planning_context_snapshot_is_immutable_and_exactly_replayed_db() -> TestResult {
    // Pins: one session origin owns one byte-exact authority snapshot; exact retries return the
    // first UID/timestamp, changed authority conflicts, and SQL mutation is trigger-rejected.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let snapshot = ExecutionPlanningContextSnapshot {
        schema_version: 1,
        tenant_id,
        contact_id: None,
        session_id: SessionId::new(),
        originating_user_sequence_num: 17,
        originating_user_event_hash: ExecutionHash::from_bytes([17; 32]).to_string(),
        owner_user_id: UserId::new("planning-owner"),
        catalog: ExecutionCapabilityCatalog::build(Vec::new())?,
        authorization: ExecutionAuthorizationEnvelope {
            capability_refs: Vec::new(),
            skill_refs: Vec::new(),
        },
        pinned_instruction_skills: Vec::new(),
        execution_templates: Vec::new(),
        budget: budget(10),
    };
    let hash = planning_context_hash(&snapshot)?;
    let created = repository
        .create_planning_context(
            scope,
            NewExecutionPlanningContext {
                snapshot: snapshot.clone(),
                planning_context_hash: hash,
            },
        )
        .await?;
    let PlanningContextWriteOutcome::Created(created) = created else {
        panic!("first write must create the planning context");
    };
    let replayed = repository
        .create_planning_context(
            scope,
            NewExecutionPlanningContext {
                snapshot: snapshot.clone(),
                planning_context_hash: hash,
            },
        )
        .await?;
    let PlanningContextWriteOutcome::Replayed(replayed) = replayed else {
        panic!("exact retry must replay the planning context");
    };
    assert_eq!(replayed, created);
    assert_eq!(
        repository
            .load_planning_context(scope, created.planning_context_uid)
            .await?,
        Some(created.clone())
    );

    let mut changed = snapshot;
    changed.owner_user_id = UserId::new("broadened-owner");
    let changed_hash = planning_context_hash(&changed)?;
    assert_eq!(
        repository
            .create_planning_context(
                scope,
                NewExecutionPlanningContext {
                    snapshot: changed,
                    planning_context_hash: changed_hash,
                },
            )
            .await?,
        PlanningContextWriteOutcome::Conflict
    );

    let update_error = sqlx::query(
        "UPDATE moa.execution_planning_context SET planning_context_hash = planning_context_hash WHERE planning_context_uid = $1",
    )
    .bind(created.planning_context_uid)
    .execute(&pool)
    .await
    .expect_err("planning-context UPDATE must be rejected");
    assert!(update_error.to_string().contains("immutable"));
    let delete_error =
        sqlx::query("DELETE FROM moa.execution_planning_context WHERE planning_context_uid = $1")
            .bind(created.planning_context_uid)
            .execute(&pool)
            .await
            .expect_err("planning-context DELETE must be rejected");
    assert!(delete_error.to_string().contains("immutable"));
    Ok(())
}

#[tokio::test]
async fn normalized_planning_audits_return_first_measurements_and_conflict_db() -> TestResult {
    // Pins: V337 audit rows are the durable mutation evidence; commit-before-result retries
    // replay the first timing, while changed semantic payloads conflict without a second row.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let session_id = SessionId::new();
    let first_at = Utc::now();

    let decision = ExecutionRouteDecision::Execute {
        strategy: ExecutionStrategy::Durable,
        rationale: "The workflow requires durable execution.".to_string(),
    };
    let route = ExecutionPlanningAuditEnvelope::route(
        tenant_id,
        None,
        session_id,
        7,
        ExecutionRouteStage::Initial,
        &decision,
        ExecutionRouteProvenance {
            source: ExecutionRouteSource::Classifier,
            classifier_outcome: ExecutionRouteClassifierOutcome::Accepted,
            provider_model: Some("route-model".to_string()),
            prompt_version: Some("execution-router".to_string()),
            objective_hash: "a".repeat(64),
            response_hash: Some("b".repeat(64)),
            confidence_bps: Some(9_500),
            missing_input_count: 0,
            usage: ExecutionRouteUsage {
                input_tokens_uncached: 11,
                input_tokens_cache_write: 2,
                input_tokens_cache_read: 3,
                output_tokens: 5,
            },
            cost_microusd: 7,
            duration_micros: 9,
        },
        first_at,
    );
    let RouteAuditWriteOutcome::Applied(route_evidence) =
        repository.write_route_audit(scope, &route).await?
    else {
        panic!("first route audit must apply");
    };
    assert_eq!(route_evidence.decision, ExecutionRouteKind::Execute);
    assert_eq!(route_evidence.strategy, Some(ExecutionStrategy::Durable));
    assert_eq!(
        route_evidence.rationale,
        "The workflow requires durable execution."
    );
    let sql_audit_uid: Uuid =
        sqlx::query_scalar("SELECT moa.execution_route_audit_uid($1,$2,$3,$4,$5)")
            .bind(tenant_id.0)
            .bind(Option::<Uuid>::None)
            .bind(session_id.0)
            .bind(7_i64)
            .bind("initial")
            .fetch_one(&pool)
            .await?;
    assert_eq!(route_evidence.audit_uid, sql_audit_uid);
    let mut route_retry = route.clone();
    let ExecutionPlanningAuditPayload::Route {
        accepted_at,
        provenance,
        ..
    } = &mut route_retry.payload
    else {
        unreachable!("route fixture must remain a route");
    };
    *accepted_at += Duration::seconds(1);
    provenance.duration_micros += 1;
    assert_eq!(
        repository.write_route_audit(scope, &route_retry).await?,
        RouteAuditWriteOutcome::Replayed(route_evidence.clone())
    );
    let mut route_conflict = route;
    let ExecutionPlanningAuditPayload::Route { rationale, .. } = &mut route_conflict.payload else {
        unreachable!("route fixture must remain a route");
    };
    *rationale = "The workflow requires a different durable execution shape.".to_string();
    assert!(matches!(
        repository.write_route_audit(scope, &route_conflict).await?,
        RouteAuditWriteOutcome::Conflict { audit_uid }
            if audit_uid == route_evidence.audit_uid
    ));

    let contact_id = ContactId::new();
    let contact_scope = ExecutionScope::Contact {
        tenant_id,
        contact_id,
    };
    let contact_session_id = SessionId::new();
    let contact_decision = ExecutionRouteDecision::Execute {
        strategy: ExecutionStrategy::Durable,
        rationale: "The caller selected a pinned execution template.".to_string(),
    };
    let contact_route = ExecutionPlanningAuditEnvelope::route(
        tenant_id,
        Some(contact_id),
        contact_session_id,
        8,
        ExecutionRouteStage::Initial,
        &contact_decision,
        ExecutionRouteProvenance {
            source: ExecutionRouteSource::SelectedExecutionTemplate,
            classifier_outcome: ExecutionRouteClassifierOutcome::NotCalled,
            provider_model: None,
            prompt_version: None,
            objective_hash: "c".repeat(64),
            response_hash: None,
            confidence_bps: None,
            missing_input_count: 0,
            usage: ExecutionRouteUsage::default(),
            cost_microusd: 0,
            duration_micros: 0,
        },
        first_at,
    );
    let RouteAuditWriteOutcome::Applied(contact_evidence) = repository
        .write_route_audit(contact_scope, &contact_route)
        .await?
    else {
        panic!("contact-scoped route audit must apply");
    };
    assert_eq!(
        repository
            .write_route_audit(contact_scope, &contact_route)
            .await?,
        RouteAuditWriteOutcome::Replayed(contact_evidence)
    );

    let planner = ExecutionPlanningAuditEnvelope {
        schema_version: 1,
        tenant_id,
        contact_id: None,
        session_id: Some(session_id),
        originating_sequence: Some(7),
        payload: ExecutionPlanningAuditPayload::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            run_uid: None,
            plan_revision: None,
            outcome: ExecutionPlannerOutcome::ProviderError,
            provider_model: "planner-test".to_string(),
            prompt_version: "execution-planner".to_string(),
            candidate_hash: None,
            candidate_json: None,
            compiler_report: None,
            duration_micros: 17,
            created_at: first_at,
        },
    };
    let PlannerCallAuditWriteOutcome::Applied(planner_evidence) =
        repository.write_planner_call_audit(scope, &planner).await?
    else {
        panic!("first planner audit must apply");
    };
    let mut planner_retry = planner.clone();
    let ExecutionPlanningAuditPayload::PlannerCall {
        duration_micros,
        created_at,
        ..
    } = &mut planner_retry.payload
    else {
        unreachable!("planner fixture must remain a planner call");
    };
    *duration_micros = 999;
    *created_at += Duration::seconds(1);
    assert_eq!(
        repository
            .write_planner_call_audit(scope, &planner_retry)
            .await?,
        PlannerCallAuditWriteOutcome::Replayed(planner_evidence.clone())
    );
    assert_eq!(planner_evidence.duration_micros, 17);
    let mut planner_conflict = planner;
    let ExecutionPlanningAuditPayload::PlannerCall { provider_model, .. } =
        &mut planner_conflict.payload
    else {
        unreachable!("planner fixture must remain a planner call");
    };
    *provider_model = "changed-model".to_string();
    assert!(matches!(
        repository
            .write_planner_call_audit(scope, &planner_conflict)
            .await?,
        PlannerCallAuditWriteOutcome::Conflict { audit_uid }
            if audit_uid == planner_evidence.audit_uid
    ));

    let validation_report =
        String::from_utf8(canonical_json_bytes(&ExecutionAuditReport::Compiler {
            violations: Vec::new(),
            omitted_violations: 0,
            full_report_hash: "b".repeat(64),
        })?)?;
    let compile = ExecutionPlanningAuditEnvelope {
        schema_version: 1,
        tenant_id,
        contact_id: None,
        session_id: Some(session_id),
        originating_sequence: Some(7),
        payload: ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            operation_key: format!("session:{session_id}:7:generated:0"),
            run_uid: None,
            plan_revision: None,
            outcome: ExecutionCompileOutcome::Rejected,
            candidate_hash: "c".repeat(64),
            final_plan_hash: None,
            validation_report,
            duration_micros: 23,
            created_at: first_at,
        },
    };
    let CompileAuditWriteOutcome::Applied(compile_evidence) =
        repository.write_compile_audit(scope, &compile).await?
    else {
        panic!("first compile audit must apply");
    };
    let mut compile_retry = compile.clone();
    let ExecutionPlanningAuditPayload::Compile {
        duration_micros,
        created_at,
        ..
    } = &mut compile_retry.payload
    else {
        unreachable!("compile fixture must remain a compile audit");
    };
    *duration_micros = 777;
    *created_at += Duration::seconds(1);
    assert_eq!(
        repository
            .write_compile_audit(scope, &compile_retry)
            .await?,
        CompileAuditWriteOutcome::Replayed(compile_evidence.clone())
    );
    assert_eq!(compile_evidence.duration_micros, 23);
    let mut compile_conflict = compile;
    let ExecutionPlanningAuditPayload::Compile { candidate_hash, .. } =
        &mut compile_conflict.payload
    else {
        unreachable!("compile fixture must remain a compile audit");
    };
    *candidate_hash = "d".repeat(64);
    assert!(matches!(
        repository
            .write_compile_audit(scope, &compile_conflict)
            .await?,
        CompileAuditWriteOutcome::Conflict { audit_uid }
            if audit_uid == compile_evidence.audit_uid
    ));

    let counts: (i64, i64, i64) = sqlx::query_as(
        "SELECT \
            (SELECT COUNT(*) FROM moa.execution_route_audit), \
            (SELECT COUNT(*) FROM moa.execution_planner_call_audit), \
            (SELECT COUNT(*) FROM moa.execution_compile_audit)",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(counts, (2, 1, 1));
    assert_eq!(
        count_route_audits_as_app_role(&pool, Some(tenant_id), None, false).await?,
        1
    );
    assert_eq!(
        count_route_audits_as_app_role(&pool, Some(tenant_id), Some(contact_id), false).await?,
        1
    );
    assert_eq!(
        count_route_audits_as_app_role(&pool, Some(tenant_id), Some(ContactId::new()), false,)
            .await?,
        0
    );
    assert_eq!(
        count_route_audits_as_app_role(&pool, Some(TenantId::new()), None, false).await?,
        0
    );
    assert_eq!(
        count_route_audits_as_app_role(&pool, None, None, true).await?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn aggregate_materialization_marker_applies_once_including_empty_map_db() -> TestResult {
    // Pins: the immutable node marker, not task insertion, is first-application evidence.
    // Empty maps and reducers apply once, exact retries replay, conflicts do not mutate, and a
    // transaction that fails after marker insertion rolls the marker back.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "aggregate-materialization",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;

    let empty_map = ExecutionNodeMaterialization::Map {
        node_id: "empty-map".to_string(),
        fanout_items: 0,
    };
    let MaterializationOutcome::Applied(applied) = repository
        .materialize_node(scope, run.run_uid, 1, Some(empty_map.clone()), Vec::new())
        .await?
    else {
        panic!("empty map marker must first apply");
    };
    assert_eq!(applied.marker, Some(empty_map.clone()));
    assert!(applied.tasks.is_empty());
    assert!(applied.inserted_task_ids.is_empty());
    assert_eq!(
        repository
            .materialize_node(scope, run.run_uid, 1, Some(empty_map), Vec::new())
            .await?,
        MaterializationOutcome::Replayed { tasks: Vec::new() }
    );
    assert_eq!(
        repository
            .materialize_node(
                scope,
                run.run_uid,
                1,
                Some(ExecutionNodeMaterialization::Map {
                    node_id: "empty-map".to_string(),
                    fanout_items: 1,
                }),
                Vec::new(),
            )
            .await?,
        MaterializationOutcome::Conflict
    );

    let reducer = ExecutionNodeMaterialization::Reduce {
        node_id: "reduce".to_string(),
        reducer_depth: 3,
    };
    assert!(matches!(
        repository
            .materialize_node(scope, run.run_uid, 1, Some(reducer.clone()), Vec::new())
            .await?,
        MaterializationOutcome::Applied(_)
    ));
    assert_eq!(
        repository
            .materialize_node(scope, run.run_uid, 1, Some(reducer), Vec::new())
            .await?,
        MaterializationOutcome::Replayed { tasks: Vec::new() }
    );

    let rollback_marker = ExecutionNodeMaterialization::Map {
        node_id: "rollback-map".to_string(),
        fanout_items: 1,
    };
    let mut invalid_task = logical_task(
        run.run_uid,
        "rollback-map",
        "one",
        ExecutionEstimate {
            cost_microusd: 1,
            tokens: 1,
            tasks: 1,
            tool_calls: 1,
            retrieved_bytes: 1,
        },
    );
    invalid_task.generation = 2;
    assert!(
        repository
            .materialize_node(
                scope,
                run.run_uid,
                1,
                Some(rollback_marker.clone()),
                vec![invalid_task],
            )
            .await
            .is_err()
    );
    assert!(matches!(
        repository
            .materialize_node(scope, run.run_uid, 1, Some(rollback_marker), Vec::new())
            .await?,
        MaterializationOutcome::Applied(_)
    ));
    let marker_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM moa.execution_node_materialization WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert_eq!(marker_count, 3);
    Ok(())
}

#[tokio::test]
async fn originating_user_sequence_num_round_trips_to_execution_run_db() -> TestResult {
    // Pins: execution admission persists the exact user-event sequence as immutable run
    // provenance instead of deriving it from current session state.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut new_run = new_run(
        tenant_id,
        None,
        "originating-sequence",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    new_run.originating_user_sequence_num = 41;

    let created = create_run(&repository, scope, new_run).await?;

    assert_eq!(created.originating_user_sequence_num, 41);
    assert_ne!(created.planning_context_uid, Uuid::nil());
    let planning_context = repository
        .load_planning_context(scope, created.planning_context_uid)
        .await?
        .expect("run fixture must persist its normalized planning context");
    assert_eq!(
        created.planning_context_hash,
        planning_context.planning_context_hash
    );
    Ok(())
}

#[tokio::test]
async fn execution_analytics_metadata_round_trips_normalized_source_and_terminal_fields_db()
-> TestResult {
    // Pins: the repository writes V337's normalized execution dimensions and advances the
    // sequence-backed analytics cursor on later state changes without mining provenance JSON.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let new_run = new_run(
        tenant_id,
        None,
        "execution-analytics-metadata",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    let (expected_template_ref, expected_template_revision_uid) = match &new_run.source_provenance {
        ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref,
            skill_template_revision_uid,
            ..
        } => (skill_template_ref.clone(), *skill_template_revision_uid),
        provenance => panic!("unexpected analytics fixture provenance: {provenance:?}"),
    };
    let run = create_run(&repository, scope, new_run).await?;
    let initial_change_seq: i64 =
        sqlx::query_scalar("SELECT analytics_change_seq FROM moa.execution_run WHERE run_uid = $1")
            .bind(run.run_uid)
            .fetch_one(&pool)
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
        panic!("analytics fixture must transition to running");
    };
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Completed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: Vec::new(),
    };
    let cause = ExecutionTerminalCause::Completion { limit_stop: None };
    let terminal = TerminalProjection::Completed {
        output: json!({ "status": "complete" }),
    };
    let evidence = terminal_evidence_from_evaluation(cause.clone(), &evaluation)?;
    let terminal_reason = execution_terminal_reason(&cause, &terminal, &evaluation)?;
    assert!(matches!(
        repository
            .finalize_run(
                scope,
                RunFinalizationRequest {
                    run_uid: run.run_uid,
                    expected_revision: running.plan_revision,
                    expected_wake_epoch: running.wake_epoch,
                    terminal_projection: terminal,
                    completion_evaluation: evaluation,
                    terminal_evidence: evidence,
                    terminal_reason,
                },
            )
            .await?,
        FinalizationOutcome::Finalized(_)
    ));

    let row: (
        i64,
        String,
        String,
        Option<String>,
        Option<Uuid>,
        Option<String>,
    ) = sqlx::query_as(
        r#"
            SELECT analytics_change_seq, source_kind, route_rationale,
                   skill_template_ref, skill_template_revision_uid, terminal_reason
            FROM moa.execution_run
            WHERE run_uid = $1
            "#,
    )
    .bind(run.run_uid)
    .fetch_one(&pool)
    .await?;
    assert!(row.0 > initial_change_seq);
    assert_eq!(row.1, "skill_template");
    assert_eq!(row.2, "The caller selected a pinned execution template.");
    assert_eq!(row.3.as_deref(), Some(expected_template_ref.as_str()));
    assert_eq!(row.4, Some(expected_template_revision_uid));
    assert_eq!(row.5.as_deref(), Some("completed"));
    let persisted = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("finalized analytics fixture must round trip through the repository");
    assert_eq!(
        persisted.source_provenance,
        ExecutionSourceProvenance::SkillTemplate {
            route_rationale: "The caller selected a pinned execution template.".to_string(),
            skill_template_ref: expected_template_ref.clone(),
            skill_template_revision_uid: expected_template_revision_uid,
        }
    );
    assert_eq!(persisted.source_kind, ExecutionSourceKind::SkillTemplate);
    assert_eq!(
        persisted.route.rationale,
        "The caller selected a pinned execution template."
    );
    assert_eq!(
        persisted.skill_template_ref.as_deref(),
        Some(expected_template_ref.as_str())
    );
    assert_eq!(
        persisted.skill_template_revision_uid,
        Some(expected_template_revision_uid)
    );
    assert_eq!(
        persisted.terminal_reason,
        Some(ExecutionTerminalReason::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn terminal_delivery_is_derived_from_durable_run_state_db() -> TestResult {
    // Pins: terminal session delivery reuses the persisted origin and canonical full output,
    // rather than accepting caller-supplied projection fields.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut new_run = new_run(
        tenant_id,
        None,
        "terminal-delivery",
        ExecutionRunStatus::Queued,
        budget(10),
    );
    new_run.originating_user_sequence_num = 57;
    let run = create_run(&repository, scope, new_run).await?;
    let TransitionOutcome::RunApplied(_running) = repository
        .transition_run_wait(
            scope,
            run.run_uid,
            ExecutionRunStatus::Queued,
            ExecutionRunStatus::Running,
        )
        .await?
    else {
        panic!("terminal delivery fixture must transition to running");
    };
    let tasks = repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![
                logical_task(run.run_uid, "collect", "a", estimate(3)),
                logical_task(run.run_uid, "collect", "b", estimate(3)),
            ],
        )
        .await?;
    for task in &tasks {
        reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    }
    assert!(matches!(
        repository
            .record_task_outcome(
                scope,
                run.run_uid,
                tasks[0].task_id,
                1,
                ExecutionTaskOutcome {
                    schema_version: 1,
                    usage: usage(1),
                    result: ExecutionTaskResult::Completed {
                        output: json!({ "part": "a" }),
                        citations: vec![
                            ExecutionCitation {
                                source_id: "source-b".to_string(),
                                uri: None,
                                locator: None,
                            },
                            ExecutionCitation {
                                source_id: "source-a".to_string(),
                                uri: None,
                                locator: None,
                            },
                            ExecutionCitation {
                                source_id: "source-a".to_string(),
                                uri: None,
                                locator: None,
                            },
                        ],
                    },
                },
            )
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    assert!(matches!(
        repository
            .record_task_outcome(
                scope,
                run.run_uid,
                tasks[1].task_id,
                1,
                ExecutionTaskOutcome {
                    schema_version: 1,
                    usage: usage(1),
                    result: ExecutionTaskResult::Failed {
                        class: ExecutionFailureClass::Terminal,
                        message: "source b failed".to_string(),
                    },
                },
            )
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    let prefinal = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run remains visible before finalization");
    let progress = execution_progress_from_run(&prefinal);
    assert_eq!(progress.run_uid, run.run_uid);
    assert_eq!(progress.originating_user_sequence_num, 57);
    assert_eq!(progress.plan_revision, 1);
    assert_eq!(progress.status, "running");
    assert_eq!(progress.total, 2);
    assert_eq!(progress.completed, 1);
    assert_eq!(progress.failed, 1);
    assert_eq!(progress.cancelled, 0);
    let output = json!({ "z": 1, "a": [2, 3] });
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Partial,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: vec!["source b missing".to_string()],
    };
    let cause = ExecutionTerminalCause::Completion { limit_stop: None };
    let terminal = TerminalProjection::Partial {
        output: Some(output.clone()),
        gaps: vec!["source b missing".to_string()],
    };
    let evidence = terminal_evidence_from_evaluation(cause.clone(), &evaluation)?;
    let terminal_reason = execution_terminal_reason(&cause, &terminal, &evaluation)?;
    let finalized = repository
        .finalize_run(
            scope,
            RunFinalizationRequest {
                run_uid: run.run_uid,
                expected_revision: 1,
                expected_wake_epoch: prefinal.wake_epoch,
                terminal_projection: terminal,
                completion_evaluation: evaluation,
                terminal_evidence: evidence,
                terminal_reason,
            },
        )
        .await?;
    assert!(matches!(finalized, FinalizationOutcome::Finalized(_)));

    let delivery = repository
        .load_terminal_delivery(scope, run.run_uid)
        .await?
        .expect("finalized run must have terminal delivery");
    let canonical = moa_artifacts::canonical::canonical_json_bytes(&output)?;
    assert_eq!(delivery.status, ExecutionRunStatus::Partial);
    assert_eq!(delivery.summary.run_uid, run.run_uid);
    assert_eq!(delivery.summary.originating_user_sequence_num, 57);
    assert_eq!(delivery.summary.output, Some(output));
    assert_eq!(
        delivery.summary.output_hash,
        *blake3::hash(&canonical).as_bytes()
    );
    assert_eq!(
        delivery.summary.citation_ids,
        vec!["source-a".to_string(), "source-b".to_string()]
    );
    assert_eq!(
        delivery.summary.failures,
        vec!["source b failed".to_string()]
    );
    assert_eq!(delivery.summary.gaps, vec!["source b missing".to_string()]);
    assert_eq!(
        delivery.summary.task_results,
        ExecutionTaskResultsRef::ExecutionTaskTable {
            run_uid: run.run_uid
        }
    );
    Ok(())
}

#[tokio::test]
async fn wake_epoch_acknowledgement_is_lossless_and_compare_and_set_db() -> TestResult {
    // Pins: a scheduler can acknowledge only the exact persisted wake epoch, and a later wake remains pending.
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
            "wake-epoch-cas",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;

    let initial_epoch = run.wake_epoch;
    assert_eq!(run.processed_wake_epoch, 0);
    let task = logical_task(run.run_uid, "wake", "one", estimate(1));
    let materialized = repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task])
        .await?;
    assert_eq!(materialized.len(), 1);

    assert_eq!(
        repository
            .ack_run_wake(scope, run.run_uid, initial_epoch)
            .await?,
        WakeAckOutcome::Changed {
            current_wake_epoch: initial_epoch + 1,
        }
    );
    assert_eq!(
        repository
            .ack_run_wake(scope, run.run_uid, initial_epoch + 1)
            .await?,
        WakeAckOutcome::Acknowledged {
            processed_wake_epoch: initial_epoch + 1,
        }
    );
    assert_eq!(
        repository
            .ack_run_wake(scope, run.run_uid, initial_epoch + 1)
            .await?,
        WakeAckOutcome::Replayed {
            processed_wake_epoch: initial_epoch + 1,
        }
    );

    let page = repository
        .list_runs(scope, ExecutionRunPageRequest::default())
        .await?;
    assert_eq!(page.runs.len(), 1);
    assert_eq!(page.runs[0].processed_wake_epoch, initial_epoch + 1);
    let snapshot = repository
        .load_scheduling_snapshot(scope, run.run_uid)
        .await?
        .expect("repeatable-read scheduling snapshot should load");
    assert_eq!(snapshot.run.run_uid, run.run_uid);
    assert_eq!(snapshot.projection.tasks.len(), 1);
    Ok(())
}

#[tokio::test]
async fn tenant_contact_and_control_plane_scopes_are_isolated_db() -> TestResult {
    // Pins: apply_contact_rls exposes exactly control-plane, tenant-null-contact, and matching-contact rows.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let contact_a = ContactId::new();
    let peer = ContactId::new();
    let tenant_scope = ExecutionScope::Tenant {
        tenant_id: tenant_a,
    };
    let contact_scope = ExecutionScope::Contact {
        tenant_id: tenant_a,
        contact_id: contact_a,
    };

    let tenant_run = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_a,
        },
        new_run(
            tenant_a,
            None,
            "tenant-a",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let contact_run = create_run(
        &repository,
        ExecutionScope::Contact {
            tenant_id: tenant_a,
            contact_id: contact_a,
        },
        new_run(
            tenant_a,
            Some(contact_a),
            "contact-a",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let tenant_b_run = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_b,
        },
        new_run(
            tenant_b,
            None,
            "tenant-b",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;

    let tenant_tasks = (0..3)
        .map(|index| {
            logical_task(
                tenant_run.run_uid,
                "tenant",
                &index.to_string(),
                estimate(1),
            )
        })
        .collect::<Vec<_>>();
    let contact_tasks = (0..3)
        .map(|index| {
            logical_task(
                contact_run.run_uid,
                "contact",
                &index.to_string(),
                estimate(1),
            )
        })
        .collect::<Vec<_>>();
    let tenant_b_tasks = (0..3)
        .map(|index| {
            logical_task(
                tenant_b_run.run_uid,
                "tenant-b",
                &index.to_string(),
                estimate(1),
            )
        })
        .collect::<Vec<_>>();
    repository
        .materialize_tasks(tenant_scope, tenant_run.run_uid, 1, tenant_tasks.clone())
        .await?;
    repository
        .materialize_tasks(contact_scope, contact_run.run_uid, 1, contact_tasks.clone())
        .await?;
    repository
        .materialize_tasks(
            ExecutionScope::Tenant {
                tenant_id: tenant_b,
            },
            tenant_b_run.run_uid,
            1,
            tenant_b_tasks.clone(),
        )
        .await?;

    assert!(
        repository
            .load_run(tenant_scope, tenant_run.run_uid)
            .await?
            .is_some()
    );
    assert!(
        repository
            .load_run(tenant_scope, contact_run.run_uid)
            .await?
            .is_none()
    );
    assert_task_pages(
        &repository,
        tenant_scope,
        tenant_run.run_uid,
        &tenant_tasks,
        2,
    )
    .await?;
    assert!(
        repository
            .list_tasks(
                tenant_scope,
                contact_run.run_uid,
                ExecutionTaskPageRequest::default(),
            )
            .await?
            .tasks
            .is_empty()
    );
    assert!(
        repository
            .load_run(tenant_scope, tenant_b_run.run_uid)
            .await?
            .is_none()
    );
    assert_task_pages(
        &repository,
        contact_scope,
        contact_run.run_uid,
        &contact_tasks,
        1,
    )
    .await?;
    assert!(
        repository
            .list_tasks(
                contact_scope,
                tenant_run.run_uid,
                ExecutionTaskPageRequest::default(),
            )
            .await?
            .tasks
            .is_empty()
    );
    assert!(
        repository
            .list_tasks(
                ExecutionScope::Contact {
                    tenant_id: tenant_a,
                    contact_id: peer,
                },
                contact_run.run_uid,
                ExecutionTaskPageRequest::default(),
            )
            .await?
            .tasks
            .is_empty()
    );

    assert!(
        repository
            .load_run(contact_scope, contact_run.run_uid)
            .await?
            .is_some()
    );
    assert!(
        repository
            .load_run(contact_scope, tenant_run.run_uid)
            .await?
            .is_none()
    );
    assert!(
        repository
            .load_run(
                ExecutionScope::Contact {
                    tenant_id: tenant_a,
                    contact_id: peer,
                },
                contact_run.run_uid,
            )
            .await?
            .is_none()
    );

    for run_uid in [
        tenant_run.run_uid,
        contact_run.run_uid,
        tenant_b_run.run_uid,
    ] {
        assert!(
            repository
                .load_run(ExecutionScope::ControlPlane, run_uid)
                .await?
                .is_some(),
            "control plane must see {run_uid}"
        );
    }
    assert_task_pages(
        &repository,
        ExecutionScope::ControlPlane,
        tenant_b_run.run_uid,
        &tenant_b_tasks,
        2,
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn terminal_run_rows_require_typed_cause_and_requirement_counts_db() -> TestResult {
    // Pins: the final execution schema requires terminal cause, status-compatible
    // reason, and requirement coverage evidence to be persisted atomically.
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
            "terminal-evidence-required",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;

    let missing_evidence = sqlx::query(
        "UPDATE moa.execution_run SET status = 'failed', completed_at = NOW() WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(missing_evidence, "execution_run_terminal_evidence");

    for cause in [
        json!({"kind":"internal_failure","extra":true}),
        json!({"kind":"cancellation"}),
        json!({"kind":"limit_stop","reason":"not_a_limit"}),
    ] {
        let malformed = sqlx::query(
            "UPDATE moa.execution_run SET status = 'failed', terminal_cause = $2, terminal_satisfied_requirement_count = 0, terminal_requirement_count = 0, completed_at = NOW() WHERE run_uid = $1",
        )
        .bind(run.run_uid)
        .bind(cause)
        .execute(test_db.store().pool())
        .await;
        assert_db_error_contains(malformed, "execution_run_terminal_evidence");
    }
    let invalid_counts = sqlx::query(
        "UPDATE moa.execution_run SET status = 'failed', terminal_cause = '{\"kind\":\"internal_failure\"}'::JSONB, terminal_satisfied_requirement_count = 2, terminal_requirement_count = 1, completed_at = NOW() WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(invalid_counts, "execution_run_terminal_evidence");
    let partial_evidence = sqlx::query(
        "UPDATE moa.execution_run SET status = 'failed', terminal_cause = '{\"kind\":\"internal_failure\"}'::JSONB, terminal_satisfied_requirement_count = 0, terminal_requirement_count = NULL, completed_at = NOW() WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(partial_evidence, "execution_run_terminal_evidence");

    let unsupported_cause_run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "unsupported-terminal-evidence-shape",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;
    let unsupported_cause = sqlx::query(
        "UPDATE moa.execution_run SET status = 'failed', terminal_cause = '{\"kind\":\"removed_runtime\"}'::JSONB, terminal_satisfied_requirement_count = 0, terminal_requirement_count = 0, completed_at = NOW() WHERE run_uid = $1",
    )
    .bind(unsupported_cause_run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(unsupported_cause, "execution_run_terminal_evidence");
    let unchanged = repository
        .load_run(scope, unsupported_cause_run.run_uid)
        .await?
        .expect("rejected terminal mutation leaves the run visible");
    assert_eq!(unchanged.status, ExecutionRunStatus::Queued);
    assert!(unchanged.terminal_evidence.is_none());
    Ok(())
}

#[tokio::test]
async fn terminal_finalization_persists_every_runtime_cause_and_replays_exactly_db() -> TestResult {
    // Pins: completion, typed task failure, zero-dispatch limits, scheduler
    // no-progress, and internal failure persist one complete replay identity.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut cases = vec![
        (
            "completion",
            ExecutionTerminalCause::Completion { limit_stop: None },
            CompletionStatus::Completed,
            TerminalProjection::Completed { output: json!({}) },
        ),
        (
            "completion-deadline-partial",
            ExecutionTerminalCause::Completion {
                limit_stop: Some(ExecutionLimitStop::DeadlineExceeded),
            },
            CompletionStatus::Partial,
            TerminalProjection::Partial {
                output: Some(json!({"useful": true})),
                gaps: vec!["deadline".to_string()],
            },
        ),
        (
            "completion-budget-no-result",
            ExecutionTerminalCause::Completion {
                limit_stop: Some(ExecutionLimitStop::BudgetExceeded),
            },
            CompletionStatus::Failed,
            terminal_failure_projection(ExecutionFailureClass::BudgetExceeded),
        ),
        (
            "limit-deadline-partial",
            ExecutionTerminalCause::LimitStop {
                reason: ExecutionLimitStop::DeadlineExceeded,
            },
            CompletionStatus::Partial,
            TerminalProjection::Partial {
                output: Some(json!({"useful": true})),
                gaps: vec!["deadline".to_string()],
            },
        ),
        (
            "limit-budget-no-result",
            ExecutionTerminalCause::LimitStop {
                reason: ExecutionLimitStop::BudgetExceeded,
            },
            CompletionStatus::Failed,
            terminal_failure_projection(ExecutionFailureClass::BudgetExceeded),
        ),
        (
            "scheduler-no-progress",
            ExecutionTerminalCause::SchedulerNoProgress,
            CompletionStatus::Failed,
            terminal_failure_projection(ExecutionFailureClass::Terminal),
        ),
        (
            "internal-failure",
            ExecutionTerminalCause::InternalFailure,
            CompletionStatus::Failed,
            terminal_failure_projection(ExecutionFailureClass::Terminal),
        ),
    ];
    for class in [
        ExecutionFailureClass::Retryable,
        ExecutionFailureClass::DependencyFailed,
        ExecutionFailureClass::InvalidInput,
        ExecutionFailureClass::InvalidOutput,
        ExecutionFailureClass::AuthorizationDenied,
        ExecutionFailureClass::BudgetExceeded,
        ExecutionFailureClass::DeadlineExceeded,
        ExecutionFailureClass::Cancelled,
        ExecutionFailureClass::Unsupported,
        ExecutionFailureClass::Terminal,
    ] {
        cases.push((
            "task-failure",
            ExecutionTerminalCause::TaskFailure {
                class: class.clone(),
            },
            CompletionStatus::Failed,
            terminal_failure_projection(class),
        ));
    }

    for (index, (name, cause, status, terminal)) in cases.into_iter().enumerate() {
        let run = create_run(
            &repository,
            scope,
            new_run(
                tenant_id,
                None,
                &format!("terminal-cause-{index}-{name}"),
                ExecutionRunStatus::Queued,
                budget(1),
            ),
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
            panic!("terminal fixture must transition to running");
        };
        let expected_wake_epoch = running.wake_epoch;
        let evaluation = CompletionEvaluation {
            status,
            limit_stop: match &cause {
                ExecutionTerminalCause::Completion { limit_stop } => *limit_stop,
                ExecutionTerminalCause::TaskFailure { .. }
                | ExecutionTerminalCause::LimitStop { .. }
                | ExecutionTerminalCause::SchedulerNoProgress
                | ExecutionTerminalCause::ReplanStop { .. }
                | ExecutionTerminalCause::Cancellation
                | ExecutionTerminalCause::InternalFailure => None,
            },
            checks: Vec::new(),
            satisfied_requirement_ids: Vec::new(),
            unsatisfied_requirement_ids: Vec::new(),
            gaps: match &terminal {
                TerminalProjection::Partial { gaps, .. }
                | TerminalProjection::Blocked { gaps, .. }
                | TerminalProjection::Unsupported { gaps, .. } => gaps.clone(),
                TerminalProjection::Completed { .. }
                | TerminalProjection::Failed { .. }
                | TerminalProjection::Cancelled { .. } => Vec::new(),
            },
        };
        let evidence = terminal_evidence_from_evaluation(cause.clone(), &evaluation)?;
        let terminal_reason = execution_terminal_reason(&cause, &terminal, &evaluation)?;
        let finalized = repository
            .finalize_run(
                scope,
                RunFinalizationRequest {
                    run_uid: run.run_uid,
                    expected_revision: 1,
                    expected_wake_epoch,
                    terminal_projection: terminal.clone(),
                    completion_evaluation: evaluation.clone(),
                    terminal_evidence: evidence.clone(),
                    terminal_reason,
                },
            )
            .await?;
        let FinalizationOutcome::Finalized(finalized) = finalized else {
            panic!("{name} must finalize on first delivery: {finalized:?}");
        };
        assert_eq!(
            finalized.terminal_evidence,
            Some(evidence.clone()),
            "{name}"
        );

        let restarted_repository = ExecutionRepository::new(pool.clone());
        assert!(matches!(
            restarted_repository
                .finalize_run(
                    scope,
                    RunFinalizationRequest {
                        run_uid: run.run_uid,
                        expected_revision: 1,
                        expected_wake_epoch,
                        terminal_projection: terminal.clone(),
                        completion_evaluation: evaluation.clone(),
                        terminal_evidence: evidence.clone(),
                        terminal_reason,
                    },
                )
                .await?,
            FinalizationOutcome::Replayed(_)
        ));
        let mut conflicting = evidence;
        conflicting.satisfied_requirement_count = 1;
        conflicting.requirement_count = 1;
        assert_eq!(
            restarted_repository
                .finalize_run(
                    scope,
                    RunFinalizationRequest {
                        run_uid: run.run_uid,
                        expected_revision: 1,
                        expected_wake_epoch,
                        terminal_projection: terminal,
                        completion_evaluation: evaluation,
                        terminal_evidence: conflicting,
                        terminal_reason,
                    },
                )
                .await?,
            FinalizationOutcome::Conflict,
            "{name} conflicting replay"
        );
    }
    Ok(())
}

#[tokio::test]
async fn terminal_finalization_rejects_a_projection_changed_after_evaluation_db() -> TestResult {
    // Pins: cause and counts computed before a scheduling-relevant task mutation
    // cannot be committed as evidence for the newer locked projection.
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
            "stale-terminal-projection",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
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
        panic!("fixture must be running");
    };
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Failed,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: Vec::new(),
        gaps: Vec::new(),
    };
    let terminal_evidence =
        terminal_evidence_from_evaluation(ExecutionTerminalCause::InternalFailure, &evaluation)?;
    let terminal = terminal_failure_projection(ExecutionFailureClass::Terminal);
    let terminal_reason = execution_terminal_reason(
        &ExecutionTerminalCause::InternalFailure,
        &terminal,
        &evaluation,
    )?;
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![logical_task(run.run_uid, "late-task", "", estimate(1))],
        )
        .await?;
    assert_eq!(
        repository
            .finalize_run(
                scope,
                RunFinalizationRequest {
                    run_uid: run.run_uid,
                    expected_revision: 1,
                    expected_wake_epoch: running.wake_epoch,
                    terminal_projection: terminal,
                    completion_evaluation: evaluation,
                    terminal_evidence,
                    terminal_reason,
                },
            )
            .await?,
        FinalizationOutcome::Conflict
    );
    assert_eq!(
        repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("run remains visible")
            .status,
        ExecutionRunStatus::Running
    );
    Ok(())
}

#[tokio::test]
async fn directly_queued_run_uses_one_database_timestamp_for_created_and_queued_db() -> TestResult {
    // Pins: a direct queued start cannot acquire an application-clock queue
    // timestamp distinct from the database-owned creation timestamp.
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
            "direct-queued-at",
            ExecutionRunStatus::Queued,
            budget(1),
        ),
    )
    .await?;
    let (created_at, queued_at): (chrono::DateTime<Utc>, chrono::DateTime<Utc>) =
        sqlx::query_as("SELECT created_at, queued_at FROM moa.execution_run WHERE run_uid = $1")
            .bind(run.run_uid)
            .fetch_one(test_db.store().pool())
            .await?;
    assert_eq!(created_at, queued_at);
    Ok(())
}

#[tokio::test]
async fn idempotency_is_scoped_and_null_contact_is_not_distinct_db() -> TestResult {
    // Pins: the partial unique key treats null contact as one tenant scope without crossing tenants or contacts.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_a = TenantId::new();
    let tenant_b = TenantId::new();
    let contact = ContactId::new();
    let key = "same-key";

    let first = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_a,
        },
        new_run(tenant_a, None, key, ExecutionRunStatus::Queued, budget(10)),
    )
    .await?;
    let duplicate = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_a,
        },
        new_run(tenant_a, None, key, ExecutionRunStatus::Queued, budget(20)),
    )
    .await?;
    let other_tenant = create_run(
        &repository,
        ExecutionScope::Tenant {
            tenant_id: tenant_b,
        },
        new_run(tenant_b, None, key, ExecutionRunStatus::Queued, budget(10)),
    )
    .await?;
    let contact_scope = ExecutionScope::Contact {
        tenant_id: tenant_a,
        contact_id: contact,
    };
    let contact_run = create_run(
        &repository,
        contact_scope,
        new_run(
            tenant_a,
            Some(contact),
            key,
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;

    assert_eq!(duplicate.run_uid, first.run_uid);
    assert_eq!(duplicate.approved_budget, first.approved_budget);
    assert_ne!(other_tenant.run_uid, first.run_uid);
    assert_ne!(contact_run.run_uid, first.run_uid);
    Ok(())
}

#[tokio::test]
async fn database_rejects_every_illegal_run_and_task_transition_db() -> TestResult {
    // Pins: every status class and counter/history guard is enforced by PostgreSQL, not repository convention.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    const RUN_STATUSES: [&str; 12] = [
        "awaiting_confirmation",
        "queued",
        "running",
        "waiting_input",
        "waiting_review",
        "waiting_replan",
        "completed",
        "partial",
        "blocked",
        "unsupported",
        "failed",
        "cancelled",
    ];
    const TASK_STATUSES: [&str; 9] = [
        "pending",
        "reserved",
        "running",
        "waiting_input",
        "waiting_replan",
        "completed",
        "skipped",
        "failed",
        "cancelled",
    ];

    let mut rejected_run_edges = 0;
    for source in RUN_STATUSES {
        for target in RUN_STATUSES {
            if source == target || run_transition_allowed(source, target) {
                continue;
            }
            let initial_status = if source == "awaiting_confirmation" {
                ExecutionRunStatus::AwaitingConfirmation
            } else {
                ExecutionRunStatus::Queued
            };
            let run = create_run(
                &repository,
                scope,
                new_run(
                    tenant_id,
                    None,
                    &format!("run-{source}-to-{target}"),
                    initial_status,
                    budget(10),
                ),
            )
            .await?;
            set_run_status_path(test_db.store().pool(), run.run_uid, run_setup_path(source))
                .await?;
            let error = sqlx::query("UPDATE moa.execution_run SET status = $2 WHERE run_uid = $1")
                .bind(run.run_uid)
                .bind(target)
                .execute(test_db.store().pool())
                .await
                .expect_err("contract-disallowed run transition must fail");
            assert!(
                error
                    .to_string()
                    .contains("invalid execution run status transition"),
                "unexpected error for run {source} -> {target}: {error}"
            );
            assert_eq!(
                sqlx::query_scalar::<_, String>(
                    "SELECT status FROM moa.execution_run WHERE run_uid = $1",
                )
                .bind(run.run_uid)
                .fetch_one(test_db.store().pool())
                .await?,
                source,
                "rejected run transition must not mutate the row"
            );
            rejected_run_edges += 1;
        }
    }
    assert_eq!(rejected_run_edges, 98);

    let task_run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "task-transition-matrix",
            ExecutionRunStatus::Queued,
            budget(100),
        ),
    )
    .await?;
    let mut task_cases = Vec::new();
    for source in TASK_STATUSES {
        for target in TASK_STATUSES {
            if source == target || task_transition_allowed(source, target) {
                continue;
            }
            let task = logical_task(
                task_run.run_uid,
                "transition",
                &format!("{source}-to-{target}"),
                estimate(1),
            );
            task_cases.push((source, target, task));
        }
    }
    repository
        .materialize_tasks(
            scope,
            task_run.run_uid,
            1,
            task_cases.iter().map(|(_, _, task)| task.clone()).collect(),
        )
        .await?;
    for (source, target, task) in &task_cases {
        set_task_status_path(
            test_db.store().pool(),
            task.task_id,
            task_setup_path(source),
        )
        .await?;
        let error = sqlx::query("UPDATE moa.execution_task SET status = $2 WHERE task_id = $1")
            .bind(task.task_id.as_uuid())
            .bind(target)
            .execute(test_db.store().pool())
            .await
            .expect_err("contract-disallowed task transition must fail");
        assert!(
            error
                .to_string()
                .contains("invalid execution task status transition"),
            "unexpected error for task {source} -> {target}: {error}"
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT status FROM moa.execution_task WHERE task_id = $1",
            )
            .bind(task.task_id.as_uuid())
            .fetch_one(test_db.store().pool())
            .await?,
            *source,
            "rejected task transition must not mutate the row"
        );
    }
    assert_eq!(task_cases.len(), 59);

    let guard_tasks = [
        logical_task(task_run.run_uid, "guard", "retry", estimate(1)),
        logical_task(task_run.run_uid, "guard", "resume", estimate(1)),
        logical_task(task_run.run_uid, "guard", "counter", estimate(1)),
        logical_task(task_run.run_uid, "guard", "immutable", estimate(1)),
        logical_task(task_run.run_uid, "guard", "history", estimate(1)),
    ];
    repository
        .materialize_tasks(scope, task_run.run_uid, 1, guard_tasks.to_vec())
        .await?;
    set_task_status_path(
        test_db.store().pool(),
        guard_tasks[0].task_id,
        task_setup_path("running"),
    )
    .await?;
    let retry_before =
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[0].task_id).await?;
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_task \
             SET attempt = attempt + 1, \
                 generation = generation + 2, \
                 generation_history = generation_history || jsonb_build_array( \
                     jsonb_build_object( \
                         'kind', 'retry', \
                         'attempt', attempt + 1, \
                         'generation', generation + 2 \
                     ) \
                 ) \
             WHERE task_id = $1",
        )
        .bind(guard_tasks[0].task_id.as_uuid())
        .execute(test_db.store().pool())
        .await,
        "execution retry must increment attempt and generation together",
    );
    assert_eq!(
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[0].task_id,).await?,
        retry_before,
        "malformed retry counters and history must roll back together"
    );
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_task \
             SET attempt = attempt + 1, \
                 generation = generation + 1, \
                 generation_history = '[]'::JSONB \
             WHERE task_id = $1",
        )
        .bind(guard_tasks[0].task_id.as_uuid())
        .execute(test_db.store().pool())
        .await,
        "execution task histories are append-only",
    );
    assert_eq!(
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[0].task_id,).await?,
        retry_before,
        "paired retry counters cannot replace generation history"
    );
    set_task_status_path(
        test_db.store().pool(),
        guard_tasks[1].task_id,
        task_setup_path("waiting_input"),
    )
    .await?;
    let resume_before =
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[1].task_id).await?;
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_task \
             SET status = 'running', \
                 attempt = attempt + 1, \
                 generation = generation + 1, \
                 generation_history = generation_history || jsonb_build_array( \
                     jsonb_build_object( \
                         'kind', 'input_resume', \
                         'attempt', attempt + 1, \
                         'generation', generation + 1 \
                     ) \
                 ) \
             WHERE task_id = $1",
        )
        .bind(guard_tasks[1].task_id.as_uuid())
        .execute(test_db.store().pool())
        .await,
        "execution input resume must increment only generation",
    );
    assert_eq!(
        listed_task(&repository, scope, task_run.run_uid, guard_tasks[1].task_id,).await?,
        resume_before,
        "input resume attempt mutation must roll back the full task projection"
    );
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_task SET generation = generation + 1 WHERE task_id = $1")
            .bind(guard_tasks[2].task_id.as_uuid())
            .execute(test_db.store().pool())
            .await,
        "execution task counters changed outside retry or input resume",
    );
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_task SET node_id = 'changed' WHERE task_id = $1")
            .bind(guard_tasks[3].task_id.as_uuid())
            .execute(test_db.store().pool())
            .await,
        "execution task immutable fields cannot change",
    );
    assert_db_error_contains(
        sqlx::query(
            "UPDATE moa.execution_task SET generation_history = '[]'::JSONB WHERE task_id = $1",
        )
        .bind(guard_tasks[4].task_id.as_uuid())
        .execute(test_db.store().pool())
        .await,
        "execution task histories are append-only",
    );

    let run_guard = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "run-update-guards",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_run SET input = '{}'::JSONB WHERE run_uid = $1")
            .bind(run_guard.run_uid)
            .execute(test_db.store().pool())
            .await,
        "execution run immutable fields cannot change",
    );
    assert_db_error_contains(
        sqlx::query("UPDATE moa.execution_run SET active_plan = '{}'::JSONB WHERE run_uid = $1")
            .bind(run_guard.run_uid)
            .execute(test_db.store().pool())
            .await,
        "execution run plan changes require one fenced history append",
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_hundred_concurrent_reservations_never_exceed_budget_db() -> TestResult {
    // Pins: concurrent task reservations lock the run ledger and admit exactly the approved count.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let mut limit = budget(7);
    limit.max_cost_microusd = Some(14);
    limit.max_tokens = Some(21);
    limit.max_tool_calls = Some(7);
    limit.max_retrieved_bytes = Some(28);
    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "reservation-race",
            ExecutionRunStatus::Queued,
            limit,
        ),
    )
    .await?;
    let tasks = (0..100)
        .map(|index| {
            logical_task(
                run.run_uid,
                "screen",
                &format!("company-{index:03}"),
                ExecutionEstimate {
                    cost_microusd: 2,
                    tokens: 3,
                    tasks: 1,
                    tool_calls: 1,
                    retrieved_bytes: 4,
                },
            )
        })
        .collect::<Vec<_>>();
    repository
        .materialize_tasks(scope, run.run_uid, 1, tasks.clone())
        .await?;

    let mut joins = JoinSet::new();
    for task in tasks {
        let repository = repository.clone();
        joins.spawn(async move {
            repository
                .reserve_task(scope, run.run_uid, task.task_id, 1)
                .await
        });
    }
    let mut reserved = 0;
    let mut rejected = 0;
    while let Some(result) = joins.join_next().await {
        match result?? {
            ReservationOutcome::Reserved(_) => reserved += 1,
            ReservationOutcome::Terminalized(terminalized)
                if terminalized.rejection == ReservationRejection::BudgetExceeded =>
            {
                rejected += 1;
            }
            other => panic!("unexpected concurrent reservation result: {other:?}"),
        }
    }
    assert_eq!(reserved, 7);
    assert_eq!(rejected, 93);
    let loaded = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run remains visible");
    assert_eq!(
        loaded.reserved,
        ExecutionEstimate {
            cost_microusd: 14,
            tokens: 21,
            tasks: 7,
            tool_calls: 7,
            retrieved_bytes: 28,
        }
    );
    Ok(())
}

#[tokio::test]
async fn reservation_near_bigint_limit_returns_budget_exceeded_db() -> TestResult {
    // Pins: valid near-BIGINT ledgers reject over-budget work without PostgreSQL arithmetic overflow.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let maximum = i64::MAX as u64;
    let approved = ExecutionBudgetLimit {
        max_cost_microusd: Some(maximum),
        max_tokens: Some(maximum),
        max_tasks: Some(maximum),
        max_tool_calls: Some(maximum),
        max_retrieved_bytes: Some(maximum),
        deadline_at: Some(Utc::now() + Duration::hours(1)),
    };
    let cases = [
        (
            "cost",
            "UPDATE moa.execution_run SET consumed_cost_microusd = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 1,
                tokens: 0,
                tasks: 1,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
        ),
        (
            "tokens",
            "UPDATE moa.execution_run SET consumed_tokens = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 0,
                tokens: 1,
                tasks: 1,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
        ),
        (
            "tasks",
            "UPDATE moa.execution_run SET consumed_tasks = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 0,
                tokens: 0,
                tasks: 1,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
        ),
        (
            "tool-calls",
            "UPDATE moa.execution_run SET consumed_tool_calls = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 0,
                tokens: 0,
                tasks: 1,
                tool_calls: 1,
                retrieved_bytes: 0,
            },
        ),
        (
            "retrieved-bytes",
            "UPDATE moa.execution_run SET consumed_retrieved_bytes = 9223372036854775807 WHERE run_uid = $1",
            ExecutionEstimate {
                cost_microusd: 0,
                tokens: 0,
                tasks: 1,
                tool_calls: 0,
                retrieved_bytes: 1,
            },
        ),
    ];

    for (dimension, fixture_sql, reservation) in cases {
        let run = create_run(
            &repository,
            scope,
            new_run(
                tenant_id,
                None,
                &format!("bigint-{dimension}"),
                ExecutionRunStatus::Queued,
                approved.clone(),
            ),
        )
        .await?;
        let task = logical_task(run.run_uid, dimension, "", reservation);
        repository
            .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
            .await?;
        assert_eq!(
            sqlx::query(fixture_sql)
                .bind(run.run_uid)
                .execute(test_db.store().pool())
                .await?
                .rows_affected(),
            1,
            "fixture must place {dimension} at BIGINT max"
        );
        let reservation = repository
            .reserve_task(scope, run.run_uid, task.task_id, 1)
            .await?;
        assert!(
            matches!(
                &reservation,
                ReservationOutcome::Terminalized(terminalized)
                    if terminalized.rejection == ReservationRejection::BudgetExceeded
            ),
            "{dimension} overflow must be classified as budget exhaustion: {reservation:?}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn reservation_budget_or_deadline_rejection_consumes_zero_task_units_db() -> TestResult {
    // Pins: one reservation-admission transaction that loses to an elapsed
    // deadline or exhausted budget records the exact typed failure under its
    // current generation, leaves admission usage unconsumed, and wakes the run
    // without a second repository call.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let cases = [
        (
            "deadline",
            ExecutionBudgetLimit {
                deadline_at: Some(Utc::now() - Duration::seconds(1)),
                ..budget(1)
            },
            ReservationRejection::DeadlineElapsed,
            moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
        ),
        (
            "budget",
            ExecutionBudgetLimit {
                max_tasks: Some(0),
                ..budget(1)
            },
            ReservationRejection::BudgetExceeded,
            moa_artifacts::execution_plan::ExecutionFailureClass::BudgetExceeded,
        ),
    ];

    for (name, approved_budget, rejection, failure_class) in cases {
        let run = create_run(
            &repository,
            scope,
            new_run(
                tenant_id,
                None,
                &format!("reservation-terminal-{name}"),
                ExecutionRunStatus::Queued,
                approved_budget,
            ),
        )
        .await?;
        let task = logical_task(run.run_uid, name, "", estimate(1));
        repository
            .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
            .await?;
        let before_rejection = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("run remains visible before reservation admission");
        let admission = repository
            .reserve_task(scope, run.run_uid, task.task_id, task.generation)
            .await?;
        assert!(
            matches!(
                &admission,
                ReservationOutcome::Terminalized(terminalized)
                    if terminalized.rejection == rejection
            ),
            "{name} admission must return its committed typed terminal result: {admission:?}"
        );
        let outcome = ExecutionTaskOutcome {
            schema_version: 1,
            usage: usage(0),
            result: ExecutionTaskResult::Failed {
                class: failure_class.clone(),
                message: format!("execution task reservation rejected: {rejection:?}"),
            },
        };

        let persisted = repository
            .load_task(scope, run.run_uid, task.task_id)
            .await?
            .expect("reservation rejection must leave a queryable terminal task");
        assert_eq!(persisted.status, ExecutionTaskStatus::Failed);
        assert_eq!(persisted.current_outcome, Some(outcome));
        assert_eq!(persisted.generation, 1);
        assert_eq!(persisted.reserved, ExecutionEstimate::default());
        assert_eq!(persisted.actual, usage(0));
        assert_eq!(persisted.actual_tasks, 0);
        assert_eq!(persisted.reserved_at, None);
        assert_eq!(persisted.started_at, None);
        assert_eq!(persisted.outcome_audit.len(), 1);
        assert_eq!(
            persisted.outcome_audit[0]
                .get("accepted")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        let after_rejection = repository
            .load_run(scope, run.run_uid)
            .await?
            .expect("run remains visible after reservation rejection");
        assert_eq!(after_rejection.reserved, ExecutionEstimate::default());
        assert_eq!(after_rejection.consumed, ExecutionEstimate::default());
        assert!(!after_rejection.budget_overrun);
        assert_eq!(after_rejection.progress_failed_tasks, 1);
        assert_eq!(after_rejection.wake_epoch, before_rejection.wake_epoch + 1);

        let replay = repository
            .reserve_task(scope, run.run_uid, task.task_id, task.generation)
            .await?;
        assert!(
            matches!(
                &replay,
                ReservationOutcome::AlreadyTerminalized(terminalized)
                    if terminalized.rejection == rejection
                        && terminalized.task == persisted
                        && terminalized.run == after_rejection
            ),
            "exact replay must return the committed typed result: {replay:?}"
        );
        assert_eq!(
            repository
                .load_run(scope, run.run_uid)
                .await?
                .expect("run remains visible after replay"),
            after_rejection,
            "exact replay must not repeat accounting or wake advancement"
        );
        assert_eq!(
            repository
                .reserve_task(scope, run.run_uid, task.task_id, task.generation + 1)
                .await?,
            ReservationOutcome::Rejected(ReservationRejection::GenerationMismatch),
            "a terminalized admission must retain its generation fence"
        );
    }
    Ok(())
}

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
                    deadline_at: Some(Utc::now() + Duration::milliseconds(150)),
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
                    deadline_at: Some(Utc::now() + Duration::hours(1)),
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
async fn duplicate_task_materialization_is_exact_and_non_mutating_db() -> TestResult {
    // Pins: logical identity replay returns the existing task and rejects semantic drift.
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
            "materialization",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    let task = logical_task(run.run_uid, "lookup", "AAPL", estimate(1));
    let first = repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    let replay = repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    assert_eq!(replay, first);

    let mut drifted = task;
    drifted.input = json!({ "company": "MSFT" });
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![drifted])
        .await
        .expect_err("same identity with different input must reject");
    let loaded = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run remains visible");
    assert_eq!(loaded.progress_total_tasks, 1);
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
async fn confirmation_is_plan_hash_bound_and_exact_replay_only_db() -> TestResult {
    // Pins: confirmation replay requires persisted proof of the exact plan and approved envelope.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let direct_budget = budget(3);
    let direct_queued = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "direct-queued-confirmation",
            ExecutionRunStatus::Queued,
            direct_budget.clone(),
        ),
    )
    .await?;
    assert!(direct_queued.confirmed_at.is_none());
    assert!(direct_queued.confirmed_plan_hash.is_none());
    assert_eq!(direct_queued.queued_at, Some(direct_queued.created_at));
    assert_eq!(
        repository
            .confirm_run(
                scope,
                direct_queued.run_uid,
                &direct_queued.active_plan_hash,
                direct_budget,
            )
            .await?,
        ConfirmationOutcome::Conflict(ConfirmationConflict::InvalidStatus)
    );

    let run = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "confirmation",
            ExecutionRunStatus::AwaitingConfirmation,
            budget(2),
        ),
    )
    .await?;
    assert!(run.queued_at.is_none());
    let skipped_queue = sqlx::query(
        "UPDATE moa.execution_run SET status = 'cancelled', queued_at = NOW(), terminal_cause = '{\"kind\":\"cancellation\"}'::JSONB, terminal_satisfied_requirement_count = 0, terminal_requirement_count = 0 WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(skipped_queue, "execution run queued timestamp is immutable");
    let approved = budget(5);
    let task = logical_task(run.run_uid, "blocked-before-confirmation", "", estimate(1));
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await
        .expect_err("awaiting confirmation cannot materialize work");

    let wrong_hash = ExecutionHash::from_bytes([9; 32]);
    assert_eq!(
        repository
            .confirm_run(scope, run.run_uid, &wrong_hash, approved.clone())
            .await?,
        ConfirmationOutcome::Conflict(ConfirmationConflict::PlanHashMismatch)
    );
    let confirmed = repository
        .confirm_run(scope, run.run_uid, &run.active_plan_hash, approved.clone())
        .await?;
    let ConfirmationOutcome::Confirmed(confirmed) = confirmed else {
        panic!("expected confirmation, got {confirmed:?}");
    };
    assert_eq!(confirmed.status, ExecutionRunStatus::Queued);
    assert_eq!(confirmed.approved_budget, approved);
    assert!(confirmed.confirmed_at.is_some());
    assert_eq!(confirmed.confirmed_plan_hash, Some(run.active_plan_hash));
    let queued_at = confirmed
        .queued_at
        .expect("successful confirmation sets the queue timestamp");
    assert!(queued_at >= run.created_at);
    let replay = repository
        .confirm_run(
            scope,
            run.run_uid,
            &run.active_plan_hash,
            confirmed.approved_budget.clone(),
        )
        .await?;
    assert!(matches!(
        replay,
        ConfirmationOutcome::AlreadyConfirmed(ref replay) if replay.queued_at == Some(queued_at)
    ));
    assert_eq!(
        repository
            .confirm_run(scope, run.run_uid, &run.active_plan_hash, budget(6))
            .await?,
        ConfirmationOutcome::Conflict(ConfirmationConflict::BudgetMismatch)
    );

    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    assert!(matches!(
        repository
            .confirm_run(
                scope,
                run.run_uid,
                &run.active_plan_hash,
                confirmed.approved_budget.clone(),
            )
            .await?,
        ConfirmationOutcome::AlreadyConfirmed(ref replay)
            if replay.status == ExecutionRunStatus::Running && replay.queued_at == Some(queued_at)
    ));
    assert_eq!(
        repository
            .confirm_run(
                scope,
                run.run_uid,
                &wrong_hash,
                confirmed.approved_budget.clone(),
            )
            .await?,
        ConfirmationOutcome::Conflict(ConfirmationConflict::PlanHashMismatch)
    );
    assert_eq!(
        repository
            .confirm_run(scope, run.run_uid, &run.active_plan_hash, budget(6))
            .await?,
        ConfirmationOutcome::Conflict(ConfirmationConflict::BudgetMismatch)
    );

    for waiting_status in ["waiting_input", "waiting_review", "waiting_replan"] {
        assert_eq!(
            sqlx::query("UPDATE moa.execution_run SET status = $2 WHERE run_uid = $1")
                .bind(run.run_uid)
                .bind(waiting_status)
                .execute(test_db.store().pool())
                .await?
                .rows_affected(),
            1
        );
        assert!(matches!(
            repository
                .confirm_run(
                    scope,
                    run.run_uid,
                    &run.active_plan_hash,
                    confirmed.approved_budget.clone(),
                )
                .await?,
            ConfirmationOutcome::AlreadyConfirmed(ref replay)
                if replay.status.as_str() == waiting_status
        ));
        assert_eq!(
            sqlx::query("UPDATE moa.execution_run SET status = 'running' WHERE run_uid = $1")
                .bind(run.run_uid)
                .execute(test_db.store().pool())
                .await?
                .rows_affected(),
            1
        );
    }

    let cleared_queued_at =
        sqlx::query("UPDATE moa.execution_run SET queued_at = NULL WHERE run_uid = $1")
            .bind(run.run_uid)
            .execute(test_db.store().pool())
            .await;
    assert_db_error_contains(
        cleared_queued_at,
        "execution run queued timestamp is immutable",
    );
    let changed_queued_at = sqlx::query(
        "UPDATE moa.execution_run SET queued_at = queued_at + INTERVAL '1 second' WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await;
    assert_db_error_contains(
        changed_queued_at,
        "execution run queued timestamp is immutable",
    );

    let terminal_request = cancellation_request(
        &repository,
        scope,
        run.run_uid,
        "confirmation test terminalization".to_string(),
    )
    .await?;
    assert!(matches!(
        repository
            .cancel_run(scope, run.run_uid, terminal_request)
            .await?,
        CancellationOutcome::Cancelled { .. }
    ));
    assert_eq!(
        repository
            .confirm_run(
                scope,
                run.run_uid,
                &run.active_plan_hash,
                confirmed.approved_budget,
            )
            .await?,
        ConfirmationOutcome::Conflict(ConfirmationConflict::InvalidStatus)
    );
    Ok(())
}

#[tokio::test]
async fn amendment_append_is_revision_fenced_and_preserves_initial_plan_db() -> TestResult {
    // Pins: accepted replans preserve confirmation identity, append history, and supersede one waiting task.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let created = create_run(
        &repository,
        scope,
        new_run(
            tenant_id,
            None,
            "amendment",
            ExecutionRunStatus::AwaitingConfirmation,
            budget(10),
        ),
    )
    .await?;
    let ConfirmationOutcome::Confirmed(run) = repository
        .confirm_run(
            scope,
            created.run_uid,
            &created.active_plan_hash,
            created.approved_budget,
        )
        .await?
    else {
        panic!("amendment fixture must begin from a confirmed plan");
    };
    let task = logical_task(run.run_uid, "replan", "", estimate(1));
    repository
        .materialize_tasks(scope, run.run_uid, 1, vec![task.clone()])
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, task.task_id, 1, needs_replan(1),)
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));

    let amendment = PlanAmendment {
        schema_version: 1,
        base_plan_revision: 1,
        reason: "switch source".to_string(),
        evidence: json!({ "source": "unavailable" }),
        operations: Vec::new(),
    };
    let validated = ValidatedAmendment {
        amendment_hash: amendment_hash(&amendment)?,
        amendment,
        active_plan: canonical_plan(2),
        requirement_mapping: [("replacement".to_string(), vec!["req".to_string()])]
            .into_iter()
            .collect(),
        superseded_task_id: task.task_id,
    };
    let amendment_digest = validated.amendment_hash;
    assert_eq!(
        repository
            .append_amendment(scope, run.run_uid, 2, validated.clone())
            .await?,
        AmendmentWrite::Conflict
    );
    let AmendmentWrite::Applied {
        commit: amended,
        metrics,
    } = repository
        .append_amendment(scope, run.run_uid, 1, validated.clone())
        .await?
    else {
        panic!("expected applied amendment");
    };
    assert_eq!(metrics.run.prior_status, ExecutionRunStatus::WaitingReplan);
    assert_eq!(metrics.run.status, ExecutionRunStatus::Running);
    assert_eq!(metrics.tasks.len(), 1);
    assert_eq!(
        metrics.tasks[0].prior_status,
        ExecutionTaskStatus::WaitingReplan
    );
    assert_eq!(metrics.tasks[0].status, ExecutionTaskStatus::Cancelled);
    assert_eq!(metrics.tasks[0].kind, task.kind);
    assert!(metrics.tasks[0].completed_at.is_some());
    assert_eq!(amended.task_ids_to_release, vec![task.task_id]);
    let applied_wake_epoch = amended.run.wake_epoch;
    let AmendmentWrite::Replayed(replayed) = repository
        .append_amendment(scope, run.run_uid, 1, validated)
        .await?
    else {
        panic!("exact amendment replay must recover its committed handoff");
    };
    assert_eq!(replayed.run.wake_epoch, applied_wake_epoch);
    assert_eq!(replayed.task_ids_to_release, vec![task.task_id]);
    let AmendmentReplayOutcome::Replayed(recovered) = repository
        .recover_amendment_handoff(scope, run.run_uid, 1, &amendment_digest)
        .await?
    else {
        panic!(
            "persisted amendment revision/hash/audit must recover before stale-revision rejection"
        );
    };
    assert_eq!(recovered.run.wake_epoch, applied_wake_epoch);
    assert_eq!(recovered.task_ids_to_release, vec![task.task_id]);
    assert_eq!(
        repository
            .recover_amendment_handoff(scope, run.run_uid, 1, &ExecutionHash::from_bytes([99; 32]),)
            .await?,
        AmendmentReplayOutcome::Conflict
    );
    let amended = amended.run;
    assert_eq!(amended.plan_revision, 2);
    assert_eq!(amended.status, ExecutionRunStatus::Running);
    assert_eq!(amended.initial_plan_hash, run.initial_plan_hash);
    assert_eq!(amended.active_plan_hash, canonical_plan(2).plan_hash);
    assert_eq!(amended.confirmed_plan_hash, Some(run.active_plan_hash));
    assert_ne!(amended.confirmed_plan_hash, Some(amended.active_plan_hash));
    assert_eq!(amended.plan_history.len(), 1);
    let triggering_failure = FailureFingerprintInput {
        class: moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
        node_id: "replan".to_string(),
        capability_ref: None,
        message: "source unavailable".to_string(),
    };
    let triggering_failure_fingerprint = failure_fingerprint(&triggering_failure)?.to_string();
    assert_eq!(
        amended.plan_history[0]
            .get("failure_fingerprint")
            .and_then(serde_json::Value::as_str),
        Some(triggering_failure_fingerprint.as_str())
    );
    assert_eq!(
        amended.plan_history[0]
            .get("failure_fingerprint_count")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );
    assert_eq!(amended.reserved.tasks, 0);
    assert_eq!(amended.consumed.tasks, 1);
    assert_eq!(
        repository
            .confirm_run(
                scope,
                run.run_uid,
                &amended.active_plan_hash,
                amended.approved_budget.clone(),
            )
            .await?,
        ConfirmationOutcome::Conflict(ConfirmationConflict::InvalidStatus)
    );
    let page = repository
        .list_tasks(scope, run.run_uid, ExecutionTaskPageRequest::default())
        .await?;
    assert_eq!(page.tasks[0].status, ExecutionTaskStatus::Cancelled);
    assert_eq!(page.tasks[0].actual_tasks, 1);
    assert_eq!(
        page.tasks[0].current_outcome,
        Some(ExecutionTaskOutcome {
            schema_version: 1,
            usage: usage(1),
            result: ExecutionTaskResult::Cancelled {
                reason: "superseded_by_plan_revision".to_string(),
            },
        })
    );
    let persisted_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("amended run remains visible");
    let persisted_task = page.tasks[0].clone();
    assert!(!persisted_task.outcome_audit.is_empty());
    for replacement in [json!([]), json!([{ "replacement": true }])] {
        assert_db_error_contains(
            sqlx::query("UPDATE moa.execution_run SET plan_history = $2 WHERE run_uid = $1")
                .bind(run.run_uid)
                .bind(replacement)
                .execute(test_db.store().pool())
                .await,
            "execution run plan history is append-only",
        );
        assert_eq!(
            repository
                .load_run(scope, run.run_uid)
                .await?
                .expect("rejected history mutation must preserve the run"),
            persisted_run,
            "failed plan-history replacement must roll back the whole run row"
        );
    }
    for replacement in [json!([]), json!([{ "replacement": true }])] {
        assert_db_error_contains(
            sqlx::query("UPDATE moa.execution_task SET outcome_audit = $2 WHERE task_id = $1")
                .bind(task.task_id.as_uuid())
                .bind(replacement)
                .execute(test_db.store().pool())
                .await,
            "execution task histories are append-only",
        );
        assert_eq!(
            listed_task(&repository, scope, run.run_uid, task.task_id).await?,
            persisted_task,
            "failed outcome-audit replacement must roll back the whole task row"
        );
    }
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
    // remain auditable without resolving or mutating the current task projection.
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

#[tokio::test]
async fn replan_stop_terminalizes_every_active_task_and_replays_atomically_db() -> TestResult {
    // Pins: a terminal replan stop is one run-wide transaction: every active
    // task receives typed cancellation evidence, all reservations reconcile,
    // completed evidence survives, and an exact replay changes nothing.
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
            "replan-stop-all-active",
            ExecutionRunStatus::Queued,
            budget(20),
        ),
    )
    .await?;
    let pending = logical_task(run.run_uid, "pending", "", estimate(2));
    let running = logical_task(run.run_uid, "running", "", estimate(3));
    let waiting_input = logical_task(run.run_uid, "input", "", estimate(4));
    let waiting_replan = logical_task(run.run_uid, "replan", "", estimate(5));
    let completed_task = logical_task(run.run_uid, "completed", "", estimate(1));
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
                completed_task.clone(),
            ],
        )
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, running.task_id).await?;
    reserve_and_start(&repository, scope, run.run_uid, waiting_input.task_id).await?;
    reserve_and_start(&repository, scope, run.run_uid, waiting_replan.task_id).await?;
    reserve_and_start(&repository, scope, run.run_uid, completed_task.task_id).await?;
    repository
        .record_task_outcome(scope, run.run_uid, completed_task.task_id, 1, completed(1))
        .await?;
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

    let reason = "replan stopped: repeated failure".to_string();
    let gaps = vec![reason.clone()];
    let terminal = TerminalProjection::Blocked {
        output: None,
        gaps: gaps.clone(),
    };
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Blocked,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: Vec::new(),
        unsatisfied_requirement_ids: vec!["req".to_string()],
        gaps,
    };
    let terminal_evidence = terminal_evidence_from_evaluation(
        ExecutionTerminalCause::ReplanStop {
            reason: ReplanStopReason::RepeatedFailure,
        },
        &evaluation,
    )?;
    let terminal_reason = execution_terminal_reason(
        &ExecutionTerminalCause::ReplanStop {
            reason: ReplanStopReason::RepeatedFailure,
        },
        &terminal,
        &evaluation,
    )?;
    let stop_amendment_hash = ExecutionHash::from_bytes([77; 32]);
    let expected_wake_epoch = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("replan-stop run remains visible")
        .wake_epoch;
    let first = repository
        .finalize_replan_stop(
            scope,
            ReplanStopRequest {
                run_uid: run.run_uid,
                expected_revision: 1,
                expected_wake_epoch,
                task_id: waiting_replan.task_id,
                expected_generation: 1,
                amendment_hash: Some(stop_amendment_hash),
                cancellation_reason: reason.clone(),
                terminal_projection: terminal.clone(),
                completion_evaluation: evaluation.clone(),
                terminal_evidence: terminal_evidence.clone(),
                terminal_reason,
            },
        )
        .await?;
    assert!(matches!(
        first,
        moa_execution::repository::ReplanStopOutcome::Finalized(_)
    ));

    let AmendmentReplayOutcome::Replayed(recovered) = repository
        .recover_amendment_handoff(scope, run.run_uid, 1, &stop_amendment_hash)
        .await?
    else {
        panic!("terminal replan-stop amendment must recover its persisted handoff");
    };
    let mut expected_release_ids = vec![
        pending.task_id,
        running.task_id,
        waiting_input.task_id,
        waiting_replan.task_id,
    ];
    expected_release_ids.sort();
    let mut recovered_release_ids = recovered.task_ids_to_release;
    recovered_release_ids.sort();
    assert_eq!(recovered_release_ids, expected_release_ids);

    let page = repository
        .list_tasks(scope, run.run_uid, ExecutionTaskPageRequest::default())
        .await?;
    assert_eq!(page.tasks.len(), 5);
    for task in page
        .tasks
        .iter()
        .filter(|task| task.task_id != completed_task.task_id)
    {
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
            task.outcome_audit
                .iter()
                .filter(|entry| entry["kind"] == "replan_stopped")
                .count(),
            1,
            "{} must receive one bounded stop audit",
            task.node_id
        );
    }
    let completed = page
        .tasks
        .iter()
        .find(|task| task.task_id == completed_task.task_id)
        .expect("completed evidence remains present");
    assert_eq!(completed.status, ExecutionTaskStatus::Completed);
    assert_eq!(completed.output, Some(json!({"tokens": 1})));
    let finalized_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("finalized run remains visible");
    assert_eq!(finalized_run.status, ExecutionRunStatus::Blocked);
    assert_eq!(finalized_run.reserved, ExecutionEstimate::default());
    assert_eq!(finalized_run.consumed.tasks, 5);
    assert_eq!(finalized_run.progress_completed_tasks, 1);
    assert_eq!(finalized_run.progress_cancelled_tasks, 4);

    let before_replay = page.tasks;
    let replay = repository
        .finalize_replan_stop(
            scope,
            ReplanStopRequest {
                run_uid: run.run_uid,
                expected_revision: 1,
                expected_wake_epoch,
                task_id: waiting_replan.task_id,
                expected_generation: 1,
                amendment_hash: Some(stop_amendment_hash),
                cancellation_reason: reason.clone(),
                terminal_projection: terminal.clone(),
                completion_evaluation: evaluation.clone(),
                terminal_evidence: terminal_evidence.clone(),
                terminal_reason,
            },
        )
        .await?;
    assert!(matches!(
        replay,
        moa_execution::repository::ReplanStopOutcome::Replayed(_)
    ));
    assert_eq!(
        repository
            .list_tasks(scope, run.run_uid, ExecutionTaskPageRequest::default())
            .await?
            .tasks,
        before_replay
    );
    let mut conflicting_evidence = terminal_evidence;
    conflicting_evidence.satisfied_requirement_count = 1;
    assert_eq!(
        repository
            .finalize_replan_stop(
                scope,
                ReplanStopRequest {
                    run_uid: run.run_uid,
                    expected_revision: 1,
                    expected_wake_epoch,
                    task_id: waiting_replan.task_id,
                    expected_generation: 1,
                    amendment_hash: Some(stop_amendment_hash),
                    cancellation_reason: reason,
                    terminal_projection: terminal,
                    completion_evaluation: evaluation,
                    terminal_evidence: conflicting_evidence,
                    terminal_reason,
                },
            )
            .await?,
        moa_execution::repository::ReplanStopOutcome::Conflict
    );
    Ok(())
}

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
    let CancellationOutcome::Cancelled {
        commit: preconfirm,
        metrics,
    } = repository
        .cancel_run(scope, awaiting.run_uid, preconfirm_request)
        .await?
    else {
        panic!("pre-confirm cancellation must commit");
    };
    assert_eq!(
        metrics.run.prior_status,
        ExecutionRunStatus::AwaitingConfirmation
    );
    assert_eq!(metrics.run.status, ExecutionRunStatus::Cancelled);
    assert!(metrics.tasks.is_empty());
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
    let CancellationOutcome::Cancelled {
        commit: postqueue,
        metrics,
    } = repository
        .cancel_run(scope, queued.run_uid, postqueue_request)
        .await?
    else {
        panic!("post-queue cancellation must commit");
    };
    assert_eq!(metrics.run.prior_status, ExecutionRunStatus::Queued);
    assert_eq!(metrics.run.status, ExecutionRunStatus::Cancelled);
    assert!(metrics.tasks.is_empty());
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
    let CancellationOutcome::Cancelled {
        commit: cancelled, ..
    } = repository.cancel_run(scope, run.run_uid, request).await?
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
    let CancellationOutcome::Cancelled {
        commit: cancelled,
        metrics,
    } = repository.cancel_run(scope, run.run_uid, request).await?
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
    assert_eq!(metrics.run.prior_status, ExecutionRunStatus::WaitingReplan);
    assert_eq!(metrics.run.status, ExecutionRunStatus::Cancelled);
    assert_eq!(metrics.tasks.len(), 4);
    let metric_transitions = metrics
        .tasks
        .iter()
        .map(|transition| {
            (
                transition.kind.clone(),
                transition.prior_status,
                transition.status,
                transition.started_at,
                transition.completed_at,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        metric_transitions
            .iter()
            .map(|(_, prior, status, _, _)| (*prior, *status))
            .collect::<Vec<_>>(),
        vec![
            (
                ExecutionTaskStatus::WaitingInput,
                ExecutionTaskStatus::Cancelled,
            ),
            (ExecutionTaskStatus::Pending, ExecutionTaskStatus::Cancelled,),
            (
                ExecutionTaskStatus::WaitingReplan,
                ExecutionTaskStatus::Cancelled,
            ),
            (ExecutionTaskStatus::Running, ExecutionTaskStatus::Cancelled,),
        ]
    );
    assert!(
        metric_transitions
            .iter()
            .all(|(_, _, _, _, completed_at)| completed_at.is_some()),
        "every committed terminal transition must carry its persisted completion timestamp"
    );
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
    assert!(matches!(
        cancel.await??,
        CancellationOutcome::Cancelled { .. }
    ));

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
        CancellationOutcome::Cancelled { .. }
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

async fn assert_task_pages(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: Uuid,
    expected: &[LogicalTask],
    limit: u32,
) -> TestResult {
    let mut cursor = None;
    let mut actual_ids = Vec::new();
    let mut page_sizes = Vec::new();
    loop {
        let page = repository
            .list_tasks(scope, run_uid, ExecutionTaskPageRequest { limit, cursor })
            .await?;
        assert!(page.tasks.len() <= limit as usize);
        for task in &page.tasks {
            if let Some(previous) = actual_ids.last() {
                assert!(
                    task.task_id.as_uuid() != *previous,
                    "pagination must not repeat its boundary task"
                );
            }
            actual_ids.push(task.task_id.as_uuid());
        }
        page_sizes.push(page.tasks.len());
        let Some(next_cursor) = page.next_cursor else {
            break;
        };
        assert_eq!(page.tasks.len(), limit as usize);
        cursor = Some(next_cursor);
        assert!(
            page_sizes.len() <= expected.len(),
            "pagination did not make bounded progress"
        );
    }

    let mut expected_ids = expected
        .iter()
        .map(|task| task.task_id.as_uuid())
        .collect::<Vec<_>>();
    actual_ids.sort_unstable();
    expected_ids.sort_unstable();
    assert_eq!(actual_ids, expected_ids);
    let mut expected_page_sizes = Vec::new();
    let mut remaining = expected.len();
    while remaining > 0 {
        let page_size = remaining.min(limit as usize);
        expected_page_sizes.push(page_size);
        remaining -= page_size;
    }
    assert_eq!(page_sizes, expected_page_sizes);
    Ok(())
}

async fn listed_task(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
) -> Result<ExecutionTaskRecord, moa_execution::Error> {
    let page = repository
        .list_tasks(scope, run_uid, ExecutionTaskPageRequest::default())
        .await?;
    Ok(page
        .tasks
        .into_iter()
        .find(|task| task.task_id == task_id)
        .expect("requested task must be present in its run projection"))
}

fn run_transition_allowed(source: &str, target: &str) -> bool {
    match source {
        "awaiting_confirmation" => matches!(target, "queued" | "cancelled"),
        "queued" => matches!(
            target,
            "running" | "blocked" | "unsupported" | "failed" | "cancelled"
        ),
        "running" => matches!(
            target,
            "waiting_input"
                | "waiting_review"
                | "waiting_replan"
                | "completed"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "waiting_input" | "waiting_review" | "waiting_replan" => matches!(
            target,
            "running" | "partial" | "blocked" | "unsupported" | "failed" | "cancelled"
        ),
        "completed" | "partial" | "blocked" | "unsupported" | "failed" | "cancelled" => false,
        other => panic!("unknown run status in contract table: {other}"),
    }
}

fn run_setup_path(status: &str) -> &'static [&'static str] {
    match status {
        "awaiting_confirmation" | "queued" => &[],
        "running" => &["running"],
        "waiting_input" => &["running", "waiting_input"],
        "waiting_review" => &["running", "waiting_review"],
        "waiting_replan" => &["running", "waiting_replan"],
        "completed" => &["running", "completed"],
        "partial" => &["running", "partial"],
        "blocked" => &["blocked"],
        "unsupported" => &["unsupported"],
        "failed" => &["failed"],
        "cancelled" => &["cancelled"],
        other => panic!("unknown run status setup: {other}"),
    }
}

async fn set_run_status_path(pool: &sqlx::PgPool, run_uid: Uuid, path: &[&str]) -> TestResult {
    for status in path {
        let terminal_cause = match *status {
            "completed" | "partial" => Some(json!({"kind":"completion","limit_stop":null})),
            "blocked" => Some(json!({"kind":"scheduler_no_progress"})),
            "unsupported" => Some(json!({"kind":"task_failure","class":"unsupported"})),
            "failed" => Some(json!({"kind":"internal_failure"})),
            "cancelled" => Some(json!({"kind":"cancellation"})),
            _ => None,
        };
        let terminal_count = terminal_cause.as_ref().map(|_| 0_i64);
        let terminal_reason = match *status {
            "completed" => Some("completed"),
            "partial" => Some("goal_incomplete"),
            "blocked" => Some("no_progress"),
            "unsupported" => Some("unsupported_plan"),
            "failed" => Some("internal_failure"),
            "cancelled" => Some("cancelled"),
            _ => None,
        };
        assert_eq!(
            sqlx::query(
                "UPDATE moa.execution_run SET status = $2, terminal_cause = $3, terminal_satisfied_requirement_count = $4, terminal_requirement_count = $4, terminal_reason = $5 WHERE run_uid = $1",
            )
                .bind(run_uid)
                .bind(status)
                .bind(terminal_cause)
                .bind(terminal_count)
                .bind(terminal_reason)
                .execute(pool)
                .await?
                .rows_affected(),
            1,
            "setup transition to {status} must apply"
        );
    }
    Ok(())
}

fn task_transition_allowed(source: &str, target: &str) -> bool {
    match source {
        "pending" => matches!(target, "reserved" | "skipped" | "cancelled"),
        "reserved" => matches!(target, "running" | "cancelled"),
        "running" => matches!(
            target,
            "waiting_input" | "waiting_replan" | "completed" | "failed" | "cancelled"
        ),
        "waiting_input" => matches!(target, "running" | "cancelled"),
        "waiting_replan" => target == "cancelled",
        "completed" | "skipped" | "failed" | "cancelled" => false,
        other => panic!("unknown task status in contract table: {other}"),
    }
}

fn task_setup_path(status: &str) -> &'static [&'static str] {
    match status {
        "pending" => &[],
        "reserved" => &["reserved"],
        "running" => &["reserved", "running"],
        "waiting_input" => &["reserved", "running", "waiting_input"],
        "waiting_replan" => &["reserved", "running", "waiting_replan"],
        "completed" => &["reserved", "running", "completed"],
        "skipped" => &["skipped"],
        "failed" => &["reserved", "running", "failed"],
        "cancelled" => &["cancelled"],
        other => panic!("unknown task status setup: {other}"),
    }
}

async fn set_task_status_path(
    pool: &sqlx::PgPool,
    task_id: ExecutionTaskId,
    path: &[&str],
) -> TestResult {
    for status in path {
        assert_eq!(
            sqlx::query("UPDATE moa.execution_task SET status = $2 WHERE task_id = $1")
                .bind(task_id.as_uuid())
                .bind(status)
                .execute(pool)
                .await?
                .rows_affected(),
            1,
            "setup transition to {status} must apply"
        );
    }
    Ok(())
}

fn assert_db_error_contains(
    result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
    expected: &str,
) {
    let error = result.expect_err("database guard must reject the mutation");
    assert!(
        error.to_string().contains(expected),
        "expected database error containing `{expected}`, got `{error}`"
    );
}

async fn cancellation_request(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: Uuid,
    reason: String,
) -> Result<CancellationRequest, moa_execution::Error> {
    let snapshot = repository
        .load_scheduling_snapshot(scope, run_uid)
        .await?
        .ok_or_else(|| moa_execution::Error::InvalidRepositoryInput {
            message: "test cancellation run is missing".to_string(),
        })?;
    Ok(CancellationRequest {
        reason,
        terminal_evidence: cancellation_terminal_evidence(
            &snapshot.run.goal,
            &snapshot.run.active_plan,
            &snapshot.projection,
        )?,
    })
}

/// Counts route-audit rows after assuming the application role and installing
/// one exact tenant/contact/control-plane RLS scope for the transaction.
async fn count_route_audits_as_app_role(
    pool: &sqlx::PgPool,
    tenant_id: Option<TenantId>,
    contact_id: Option<ContactId>,
    control_plane: bool,
) -> Result<i64, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SET LOCAL ROLE moa_app")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        r#"
        SELECT
            pg_catalog.set_config('moa.tenant_id', $1, true),
            pg_catalog.set_config('moa.contact_id', $2, true),
            pg_catalog.set_config('moa.control_plane', $3, true)
        "#,
    )
    .bind(tenant_id.map(|id| id.to_string()).unwrap_or_default())
    .bind(contact_id.map(|id| id.to_string()).unwrap_or_default())
    .bind(if control_plane { "true" } else { "false" })
    .execute(&mut *transaction)
    .await?;
    let count = sqlx::query_scalar("SELECT COUNT(*) FROM moa.execution_route_audit")
        .fetch_one(&mut *transaction)
        .await?;
    transaction.rollback().await?;
    Ok(count)
}

async fn create_run(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    mut run: NewExecutionRun,
) -> Result<moa_execution::repository::ExecutionRunRecord, moa_execution::Error> {
    if repository
        .load_planning_context(scope, run.planning_context_uid)
        .await?
        .is_none()
    {
        let snapshot = ExecutionPlanningContextSnapshot {
            schema_version: 1,
            tenant_id: run.tenant_id,
            contact_id: run.contact_id,
            session_id: run.session_id,
            originating_user_sequence_num: run.originating_user_sequence_num,
            originating_user_event_hash: ExecutionHash::from_bytes([19; 32]).to_string(),
            owner_user_id: run.owner_user_id.clone(),
            catalog: run.catalog.clone(),
            authorization: run.authorization.clone(),
            pinned_instruction_skills: run.pinned_instruction_skills.clone(),
            execution_templates: Vec::new(),
            budget: run.approved_budget.clone(),
        };
        let context_hash = planning_context_hash(&snapshot)?;
        let context = repository
            .create_planning_context(
                scope,
                NewExecutionPlanningContext {
                    snapshot,
                    planning_context_hash: context_hash,
                },
            )
            .await?;
        let context = match context {
            PlanningContextWriteOutcome::Created(context)
            | PlanningContextWriteOutcome::Replayed(context) => context,
            PlanningContextWriteOutcome::Conflict => {
                return Err(moa_execution::Error::InvalidRepositoryInput {
                    message: "contact test fixture planning context conflicted".to_string(),
                });
            }
        };
        run.planning_context_uid = context.planning_context_uid;
        run.planning_context_hash = context_hash;
    }
    repository.create_run(scope, run).await
}

fn new_run(
    tenant_id: TenantId,
    contact_id: Option<ContactId>,
    key: &str,
    status: ExecutionRunStatus,
    approved_budget: ExecutionBudgetLimit,
) -> NewExecutionRun {
    let catalog = ExecutionCapabilityCatalog::build(Vec::new()).expect("empty test catalog");
    let mut plan = canonical_plan(1);
    plan.catalog_hash = catalog.catalog_hash;
    NewExecutionRun {
        tenant_id,
        contact_id,
        session_id: SessionId::new(),
        originating_user_sequence_num: 1,
        planning_context_uid: Uuid::now_v7(),
        planning_context_hash: ExecutionHash::from_bytes([97; 32]),
        owner_user_id: UserId::new("researcher"),
        goal: ExecutionGoalContract {
            objective: "test durable execution".to_string(),
            requirements: Vec::new(),
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: Vec::new(),
        },
        plan,
        catalog,
        authorization: ExecutionAuthorizationEnvelope {
            capability_refs: Vec::new(),
            skill_refs: Vec::new(),
        },
        pinned_instruction_skills: Vec::new(),
        source_provenance: ExecutionSourceProvenance::SkillTemplate {
            route_rationale: "The caller selected a pinned execution template.".to_string(),
            skill_template_ref: format!("skill://{key}"),
            skill_template_revision_uid: Uuid::now_v7(),
        },
        input: json!({ "query": key }),
        status,
        approved_budget,
        idempotency_key: Some(key.to_string()),
    }
}

fn canonical_plan(seed: u8) -> CanonicalExecutionPlan {
    CanonicalExecutionPlan {
        definition: moa_artifacts::execution_plan::ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            nodes: Vec::new(),
        },
        plan_hash: ExecutionHash::from_bytes([seed; 32]),
        catalog_hash: ExecutionHash::from_bytes([seed.wrapping_add(32); 32]),
        estimate: ExecutionEstimate {
            cost_microusd: 1,
            tokens: 1,
            tasks: 1,
            tool_calls: 1,
            retrieved_bytes: 1,
        },
        report: ExecutionValidationReport::default(),
    }
}

fn budget(max_tasks: u64) -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: Some(max_tasks.saturating_mul(100)),
        max_tokens: Some(max_tasks.saturating_mul(100)),
        max_tasks: Some(max_tasks),
        max_tool_calls: Some(max_tasks.saturating_mul(10)),
        max_retrieved_bytes: Some(max_tasks.saturating_mul(1_000)),
        deadline_at: Some(Utc::now() + Duration::hours(1)),
    }
}

fn estimate(scale: u64) -> ExecutionEstimate {
    ExecutionEstimate {
        cost_microusd: scale,
        tokens: scale,
        tasks: 1,
        tool_calls: scale,
        retrieved_bytes: scale,
    }
}

fn logical_task(
    run_uid: Uuid,
    node_id: &str,
    item_key: &str,
    reservation: ExecutionEstimate,
) -> LogicalTask {
    LogicalTask {
        task_id: ExecutionTaskId::derive(run_uid, node_id, item_key).expect("derive task id"),
        node_id: node_id.to_string(),
        item_key: item_key.to_string(),
        requirement_ids: vec!["req".to_string()],
        plan_revision: 1,
        generation: 1,
        input: json!({ "company": item_key }),
        kind: LogicalTaskKind::Output {
            value: json!({ "company": item_key }),
        },
        retry: RetryPolicy {
            max_attempts: 3,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
        },
        reservation,
    }
}

fn output_node(id: &str, requirement_id: &str) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec![requirement_id.to_string()],
        depends_on: Vec::new(),
        when: None,
        input: json!({}),
        output_schema: json!({"type":"object"}),
        operation: ExecutionOperation::Output { value: json!({}) },
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
        },
        budget: None,
    }
}

fn terminal_failure_projection(class: ExecutionFailureClass) -> TerminalProjection {
    TerminalProjection::Failed {
        failure: moa_execution::state::ExecutionTaskFailure {
            class,
            message: "terminal test failure".to_string(),
            capability_ref: None,
        },
    }
}

async fn reserve_and_start(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: Uuid,
    task_id: ExecutionTaskId,
) -> TestResult {
    assert!(matches!(
        repository.reserve_task(scope, run_uid, task_id, 1).await?,
        ReservationOutcome::Reserved(_)
    ));
    assert!(matches!(
        repository
            .mark_task_running(scope, run_uid, task_id, 1)
            .await?,
        TransitionOutcome::Applied(_)
    ));
    Ok(())
}

fn usage(value: u64) -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: value,
        tokens: value,
        tool_calls: value,
        retrieved_bytes: value,
    }
}

fn assert_terminal_redispatch_failure(
    task: &ExecutionTaskRecord,
    expected_class: moa_artifacts::execution_plan::ExecutionFailureClass,
) {
    assert_eq!(task.status, ExecutionTaskStatus::Failed);
    assert_eq!(task.generation, 2);
    assert_eq!(task.reserved, ExecutionEstimate::default());
    assert_eq!(task.actual_tasks, 1);
    assert!(task.completed_at.is_some());
    assert!(matches!(
        task.current_outcome.as_ref().map(|outcome| &outcome.result),
        Some(ExecutionTaskResult::Failed { class, .. }) if *class == expected_class
    ));
}

fn completed(value: u64) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(value),
        result: ExecutionTaskResult::Completed {
            output: json!({ "tokens": value }),
            citations: Vec::new(),
        },
    }
}

fn needs_input(value: u64) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(value),
        result: ExecutionTaskResult::NeedsInput {
            question: "continue?".to_string(),
            audience: moa_artifacts::execution_plan::InputAudience::User,
        },
    }
}

fn needs_replan(value: u64) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(value),
        result: ExecutionTaskResult::NeedsReplan {
            reason: "source unavailable".to_string(),
            evidence: json!({ "retry": false }),
        },
    }
}

fn retryable(value: u64) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(value),
        result: ExecutionTaskResult::Failed {
            class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
            message: "retry later".to_string(),
        },
    }
}
