//! Planning-context, normalized-audit, confirmation, and amendment persistence contracts.

use super::support::*;
use moa_artifacts::execution_plan::{ExecutionNode, ExecutionOperation};
use moa_execution::capability::node_output_hash;
use moa_execution::repository::planning_budget::{
    AmendmentPlanningCallReconcileOutcome, AmendmentPlanningCallReconcileRequest,
    AmendmentPlanningCallReservation, AmendmentPlanningCallReservationOutcome,
    AmendmentPlanningCallReservationRequest, PlanningUsage,
};

#[tokio::test]
async fn amendment_planner_call_budget_is_reserved_and_reconciled_exactly_once_db() -> TestResult {
    // Pins: one automatic amendment provider call has durable cost/token attribution, consumes
    // no logical-task budget, and exact reserve/reconcile replays cannot double charge the run.
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
            "amendment-planning-budget",
            ExecutionRunStatus::Queued,
            budget(10),
        ),
    )
    .await?;
    sqlx::query("UPDATE moa.execution_run SET status='running' WHERE run_uid=$1")
        .bind(run.run_uid)
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE moa.execution_run SET status='waiting_replan' WHERE run_uid=$1")
        .bind(run.run_uid)
        .execute(&pool)
        .await?;

    let denied = AmendmentPlanningCallReservationRequest {
        run_uid: run.run_uid,
        base_plan_revision: 1,
        call_ordinal: 9,
        reservation: AmendmentPlanningCallReservation {
            cost_microusd: 10_000,
            tokens: 10_000,
        },
        now: moa_test_support::fixtures::pg_now(),
    };
    assert!(matches!(
        repository
            .reserve_amendment_planning_call(scope, denied)
            .await?,
        AmendmentPlanningCallReservationOutcome::Denied(_)
    ));
    let after_denial = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("denied run remains visible");
    assert_eq!(after_denial.reserved, Default::default());
    assert_eq!(after_denial.consumed, Default::default());
    assert_eq!(
        repository
            .load_amendment_planning_call(scope, run.run_uid, 1, 9)
            .await?,
        None,
        "denial must leave no attribution row for work that cannot run"
    );

    let request = AmendmentPlanningCallReservationRequest {
        run_uid: run.run_uid,
        base_plan_revision: 1,
        call_ordinal: 2,
        reservation: AmendmentPlanningCallReservation {
            cost_microusd: 100,
            tokens: 80,
        },
        now: moa_test_support::fixtures::pg_now(),
    };
    let AmendmentPlanningCallReservationOutcome::Granted(open) = repository
        .reserve_amendment_planning_call(scope, request)
        .await?
    else {
        panic!("the exact live amendment revision must authorize one provider call");
    };
    assert_eq!(open.reserved, request.reservation);
    assert_eq!(open.actual, None);
    assert_eq!(
        repository
            .reserve_amendment_planning_call(scope, request)
            .await?,
        AmendmentPlanningCallReservationOutcome::ReplayedOpen(open.clone())
    );
    let reserved_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run remains visible");
    assert_eq!(reserved_run.reserved.cost_microusd, 100);
    assert_eq!(reserved_run.reserved.tokens, 80);
    assert_eq!(reserved_run.reserved.tasks, 0);

    let reconcile = AmendmentPlanningCallReconcileRequest {
        run_uid: run.run_uid,
        base_plan_revision: 1,
        call_ordinal: 2,
        actual: PlanningUsage {
            cost_microusd: 70,
            tokens: 50,
        },
        settled_at: moa_test_support::fixtures::pg_now(),
    };
    let AmendmentPlanningCallReconcileOutcome::Applied(settled) = repository
        .reconcile_amendment_planning_call(scope, reconcile.clone())
        .await?
    else {
        panic!("the first exact reconciliation must apply");
    };
    assert_eq!(settled.actual.as_ref(), Some(&reconcile.actual));
    assert_eq!(
        repository
            .reconcile_amendment_planning_call(scope, reconcile.clone())
            .await?,
        AmendmentPlanningCallReconcileOutcome::Replayed(settled.clone())
    );
    assert_eq!(
        repository
            .reserve_amendment_planning_call(scope, request)
            .await?,
        AmendmentPlanningCallReservationOutcome::AlreadySettled(settled.clone()),
        "post-reconcile Restate replay must recover the immutable settled authorization"
    );
    let settled_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run remains visible");
    assert_eq!(settled_run.reserved.cost_microusd, 0);
    assert_eq!(settled_run.reserved.tokens, 0);
    assert_eq!(settled_run.consumed.cost_microusd, 70);
    assert_eq!(settled_run.consumed.tokens, 50);
    assert_eq!(settled_run.consumed.tasks, 0);
    assert_eq!(
        repository
            .load_amendment_planning_call(scope, run.run_uid, 1, 2)
            .await?,
        Some(settled)
    );

    let mut conflicting = reconcile.clone();
    conflicting.actual.cost_microusd += 1;
    assert_eq!(
        repository
            .reconcile_amendment_planning_call(scope, conflicting)
            .await?,
        AmendmentPlanningCallReconcileOutcome::Conflict
    );

    let repair = AmendmentPlanningCallReservationRequest {
        call_ordinal: 3,
        reservation: AmendmentPlanningCallReservation {
            cost_microusd: 40,
            tokens: 30,
        },
        ..request
    };
    assert!(matches!(
        repository
            .reserve_amendment_planning_call(scope, repair)
            .await?,
        AmendmentPlanningCallReservationOutcome::Granted(_)
    ));
    let repair_reconcile = AmendmentPlanningCallReconcileRequest {
        call_ordinal: repair.call_ordinal,
        actual: PlanningUsage {
            cost_microusd: 20,
            tokens: 10,
        },
        ..reconcile
    };
    let AmendmentPlanningCallReconcileOutcome::Applied(repair_settled) = repository
        .reconcile_amendment_planning_call(scope, repair_reconcile.clone())
        .await?
    else {
        panic!("the repair call must reconcile independently");
    };
    assert_eq!(repair_settled.actual, Some(repair_reconcile.actual));
    assert_eq!(
        repository
            .reconcile_amendment_planning_call(scope, repair_reconcile)
            .await?,
        AmendmentPlanningCallReconcileOutcome::Replayed(repair_settled)
    );
    let after_repair = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("run remains visible");
    assert_eq!(after_repair.reserved, Default::default());
    assert_eq!(after_repair.consumed.cost_microusd, 90);
    assert_eq!(after_repair.consumed.tokens, 60);
    assert_eq!(after_repair.consumed.tasks, 0);
    Ok(())
}

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
    // Pins: execution audit rows retain only normalized routing evidence; the shared DB reader
    // reconstructs that typed payload without persisting classifier rationale, exact retries
    // replay the first timing, and changed semantic payloads conflict without a second row.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let session_id = SessionId::new();
    let first_at = moa_test_support::fixtures::pg_now();

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
    let reconstructed = moa_test_support::execution_audits::load_execution_planning_audits(
        test_db.database_url(),
        session_id,
    )
    .await?;
    let mut expected_reconstructed = route.clone();
    let ExecutionPlanningAuditPayload::Route { accepted_at, .. } =
        &mut expected_reconstructed.payload
    else {
        unreachable!("route fixture must remain a route");
    };
    *accepted_at = route_evidence.accepted_at;
    assert_eq!(reconstructed, vec![expected_reconstructed]);
    assert!(
        serde_json::to_value(&reconstructed[0])?
            .pointer("/payload/rationale")
            .is_none(),
        "normalized route audits must not reconstruct classifier rationale"
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
    let ExecutionPlanningAuditPayload::Route { strategy, .. } = &mut route_conflict.payload else {
        unreachable!("route fixture must remain a route");
    };
    *strategy = Some(ExecutionStrategy::Inline);
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

    let planner_report = String::from_utf8(canonical_json_bytes(&ExecutionAuditReport::Schema {
        violations: Vec::new(),
        omitted_violations: 0,
        full_report_hash: "d".repeat(64),
    })?)?;
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
            outcome: ExecutionPlannerOutcome::SchemaRejected,
            provider_model: "planner-test".to_string(),
            prompt_version: "execution-planner".to_string(),
            usage: ExecutionRouteUsage {
                input_tokens_uncached: 21,
                input_tokens_cache_write: 3,
                input_tokens_cache_read: 5,
                output_tokens: 8,
            },
            cost_microusd: 29,
            candidate_hash: Some("e".repeat(64)),
            candidate_json: None,
            compiler_report: Some(planner_report),
            duration_micros: 17,
            created_at: first_at,
        },
    };
    let PlannerCallAuditWriteOutcome::Applied(planner_evidence) =
        repository.write_planner_call_audit(scope, &planner).await?
    else {
        panic!("first planner audit must apply");
    };
    assert_eq!(planner_evidence.usage.input_tokens_uncached, 21);
    assert_eq!(planner_evidence.usage.input_tokens_cache_write, 3);
    assert_eq!(planner_evidence.usage.input_tokens_cache_read, 5);
    assert_eq!(planner_evidence.usage.output_tokens, 8);
    assert_eq!(planner_evidence.cost_microusd, 29);
    let reconstructed = moa_test_support::execution_audits::load_execution_planning_audits(
        test_db.database_url(),
        session_id,
    )
    .await?;
    assert_eq!(
        reconstructed.len(),
        2,
        "route and planner audit must both reconstruct"
    );
    let ExecutionPlanningAuditPayload::PlannerCall {
        usage,
        cost_microusd,
        ..
    } = &reconstructed[1].payload
    else {
        panic!("second reconstructed audit must be the planner call");
    };
    assert_eq!(
        *usage,
        ExecutionRouteUsage {
            input_tokens_uncached: 21,
            input_tokens_cache_write: 3,
            input_tokens_cache_read: 5,
            output_tokens: 8,
        }
    );
    assert_eq!(*cost_microusd, 29);
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
    assert_eq!(confirmed.wake_epoch, run.wake_epoch + 1);
    let confirmation_dispatch: (i64, String, String) = sqlx::query_as(
        "SELECT wake_epoch, dispatch_kind, state FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1",
    )
    .bind(run.run_uid)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(
        confirmation_dispatch,
        (
            i64::try_from(confirmed.wake_epoch)?,
            "run_activation".into(),
            "pending".into(),
        )
    );
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
    let confirmation_dispatch_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM moa.execution_dispatch_outbox WHERE run_uid = $1")
            .bind(run.run_uid)
            .fetch_one(test_db.store().pool())
            .await?;
    assert_eq!(confirmation_dispatch_count, 1);
    assert_eq!(
        repository
            .confirm_run(scope, run.run_uid, &run.active_plan_hash, budget(6))
            .await?,
        ConfirmationOutcome::Conflict(ConfirmationConflict::BudgetMismatch)
    );

    sqlx::query("UPDATE moa.execution_run SET status='running' WHERE run_uid=$1")
        .bind(run.run_uid)
        .execute(test_db.store().pool())
        .await?;
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

    let current = repository
        .load_run(scope, run.run_uid)
        .await?
        .expect("confirmation test run remains visible before fencing");
    let terminal_evidence =
        moa_execution::completion::cancellation_terminal_evidence_from_completed_nodes(
            &current.goal,
            &current.active_plan,
            &std::collections::BTreeSet::<String>::new(),
        )?;
    assert!(matches!(
        repository
            .fence_completion_terminal_and_enqueue_settlement(
                &ExecutionConfig::default(),
                scope,
                run.run_uid,
                current.controller_generation,
                current.wake_epoch,
                PendingExecutionTerminal {
                    status: ExecutionRunStatus::Cancelled,
                    reason: ExecutionTerminalReason::Cancelled,
                    terminal_evidence,
                    output: None,
                    completion_check_results: Vec::new(),
                    terminal_gaps: Vec::new(),
                    cancellation_reason: Some("confirmation test terminalization".to_string()),
                },
                moa_test_support::fixtures::pg_now(),
                1,
            )
            .await?,
        moa_execution::repository::terminal::PendingTerminalAdvanceOutcome::Applied(_)
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
    // Pins: accepted replans atomically supersede their wait and replace only mutable node state;
    // completed dependencies survive while replacement nodes become immediately actionable.
    let test_db = moa_test_support::postgres::bootstrap_test_db().await?;
    let repository = ExecutionRepository::new(test_db.store().pool().clone());
    let tenant_id = TenantId::new();
    let scope = ExecutionScope::Tenant { tenant_id };
    let retry = RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    };
    let node = |id: &str, depends_on: Vec<String>| ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["req".to_string()],
        depends_on,
        when: None,
        input: json!({}),
        output_schema: json!({"type": "object"}),
        operation: ExecutionOperation::Output { value: json!({}) },
        compensation: None,
        retry: retry.clone(),
        budget: None,
    };
    let preserved = node("preserved", Vec::new());
    let review_wait_policy = ExecutionWaitPolicy {
        expiry: ExecutionTemporalTarget::After { delay_seconds: 600 },
        on_expiry: ExecutionWaitExpiryAction::FailTask,
    };
    let review = ExecutionNode {
        id: "preserved_review".to_string(),
        requirement_ids: vec!["req".to_string()],
        depends_on: Vec::new(),
        when: None,
        input: json!({}),
        output_schema: json!({"type": "object"}),
        operation: ExecutionOperation::Review {
            prompt: "approve preserved work".to_string(),
            wait_policy: review_wait_policy.clone(),
        },
        compensation: None,
        retry: retry.clone(),
        budget: None,
    };
    let replan = node("replan", vec!["preserved".to_string()]);
    let old_terminal = node("old_terminal", vec!["replan".to_string()]);
    let replacement = node("replacement", vec!["preserved".to_string()]);
    let new_terminal = node("new_terminal", vec!["replacement".to_string()]);
    let mut candidate = new_run(
        tenant_id,
        None,
        "amendment",
        ExecutionRunStatus::AwaitingConfirmation,
        budget(10),
    );
    candidate.plan.definition.nodes = vec![preserved.clone(), review.clone(), replan, old_terminal];
    candidate.plan.estimate.tasks = 4;
    let created = create_run(&repository, scope, candidate).await?;
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
    let mut review_task = logical_task(run.run_uid, "preserved_review", "", estimate(1));
    review_task.kind = LogicalTaskKind::Review {
        prompt: "approve preserved work".to_string(),
        wait_policy: review_wait_policy,
    };
    let task = logical_task(run.run_uid, "replan", "", estimate(1));
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![review_task.clone(), task.clone()],
        )
        .await?;
    reserve_and_start(&repository, scope, run.run_uid, task.task_id).await?;
    assert!(matches!(
        repository
            .record_task_outcome(scope, run.run_uid, task.task_id, 1, needs_replan(1),)
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    set_task_status_path(
        test_db.store().pool(),
        review_task.task_id,
        task_setup_path("waiting_review"),
    )
    .await?;
    let review_waiting_since = moa_test_support::fixtures::pg_now() - Duration::minutes(1);
    let review_expiry_at = review_waiting_since + Duration::minutes(10);
    sqlx::query(
        "UPDATE moa.execution_task SET attempt_state='waiting',waiting_since=$2,updated_at=NOW() \
         WHERE task_id=$1",
    )
    .bind(review_task.task_id.as_uuid())
    .bind(review_waiting_since)
    .execute(test_db.store().pool())
    .await?;
    let preserved_output = json!({"result": "kept"});
    sqlx::query(
        "UPDATE moa.execution_node_state SET node_status='completed', \
             materialization_complete=TRUE,aggregate_complete=TRUE,aggregate_output=$3, \
             aggregate_output_hash=$4 WHERE run_uid=$1 AND node_id=$2",
    )
    .bind(run.run_uid)
    .bind("preserved")
    .bind(&preserved_output)
    .bind(node_output_hash(&preserved_output)?.to_string())
    .execute(test_db.store().pool())
    .await?;
    sqlx::query(
        "UPDATE moa.execution_node_state SET node_status='waiting', \
             materialization_complete=TRUE,total_task_count=1,waiting_task_count=1 \
         WHERE run_uid=$1 AND node_id='replan'",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await?;
    sqlx::query(
        "UPDATE moa.execution_node_state SET node_status='waiting', \
             materialization_complete=TRUE,total_task_count=1,waiting_task_count=1 \
         WHERE run_uid=$1 AND node_id='preserved_review'",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await?;
    let preserved_review_reason = moa_execution::state::WaitingReason::Review {
        task_id: review_task.task_id,
        prompt: "approve preserved work".to_string(),
        wait_policy: ExecutionWaitPolicy {
            expiry: ExecutionTemporalTarget::At {
                at: review_expiry_at,
            },
            on_expiry: ExecutionWaitExpiryAction::FailTask,
        },
    };
    sqlx::query(
        "UPDATE moa.execution_run SET waiting_task_count=2,waiting_review_task_count=1, \
             waiting_replan_task_count=1,waiting_since=$2,waiting_reasons=$3, \
             waiting_reasons_truncated=TRUE WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .bind(review_waiting_since)
    .bind(serde_json::to_value([preserved_review_reason.clone()])?)
    .execute(test_db.store().pool())
    .await?;

    let amendment = PlanAmendment {
        base_plan_revision: 1,
        reason: "switch source".to_string(),
        evidence: json!({ "source": "unavailable" }),
        operations: Vec::new(),
    };
    let mut replacement_plan = canonical_plan(2);
    replacement_plan.definition.nodes = vec![preserved, review, replacement, new_terminal];
    replacement_plan.estimate.tasks = 3;
    let validated = ValidatedAmendment {
        amendment_hash: amendment_hash(&amendment)?,
        amendment,
        active_plan: replacement_plan.clone(),
        requirement_mapping: [("replacement".to_string(), vec!["req".to_string()])]
            .into_iter()
            .collect(),
        superseded_task_id: task.task_id,
    };
    let amendment_digest = validated.amendment_hash;
    assert_eq!(
        repository
            .append_amendment(
                scope,
                &ExecutionConfig::default(),
                run.run_uid,
                2,
                validated.clone(),
            )
            .await?,
        AmendmentWrite::Conflict
    );
    sqlx::query(
        "UPDATE moa.execution_run SET waiting_task_count=3,waiting_replan_task_count=2 \
         WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await?;
    assert_eq!(
        repository
            .append_amendment(
                scope,
                &ExecutionConfig::default(),
                run.run_uid,
                1,
                validated.clone(),
            )
            .await?,
        AmendmentWrite::Conflict,
        "one planner amendment must supersede exactly one WaitingReplan task"
    );
    assert_eq!(
        listed_task(&repository, scope, run.run_uid, task.task_id)
            .await?
            .status,
        ExecutionTaskStatus::WaitingReplan,
        "the exact-count conflict must roll back task supersession"
    );
    sqlx::query(
        "UPDATE moa.execution_run SET waiting_task_count=2,waiting_replan_task_count=1 \
         WHERE run_uid=$1",
    )
    .bind(run.run_uid)
    .execute(test_db.store().pool())
    .await?;
    let AmendmentWrite::Applied(amended) = repository
        .append_amendment(
            scope,
            &ExecutionConfig::default(),
            run.run_uid,
            1,
            validated.clone(),
        )
        .await?
    else {
        panic!("expected applied amendment");
    };
    let assert_preserved_review_wait = |projection: &ExecutionRunRecord| {
        assert_eq!(projection.status, ExecutionRunStatus::WaitingReview);
        assert_eq!(projection.waiting_task_count, 1);
        assert_eq!(projection.waiting_review_task_count, 1);
        assert_eq!(projection.waiting_replan_task_count, 0);
        assert_eq!(
            projection.waiting_reasons,
            vec![preserved_review_reason.clone()]
        );
        assert!(!projection.waiting_reasons_truncated);
        assert_eq!(projection.waiting_since, Some(review_waiting_since));
    };
    assert_eq!(amended.task_ids_to_release, vec![task.task_id]);
    assert_preserved_review_wait(&amended.run);
    let applied_wake_epoch = amended.run.wake_epoch;
    let amendment_dispatch: (i64, String) = sqlx::query_as(
        "SELECT wake_epoch, payload->>'reason' FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND wake_epoch = $2",
    )
    .bind(run.run_uid)
    .bind(i64::try_from(applied_wake_epoch)?)
    .fetch_one(test_db.store().pool())
    .await?;
    assert_eq!(
        amendment_dispatch,
        (i64::try_from(applied_wake_epoch)?, "plan_amended".into())
    );
    let AmendmentWrite::Replayed(replayed) = repository
        .append_amendment(
            scope,
            &ExecutionConfig::default(),
            run.run_uid,
            1,
            validated,
        )
        .await?
    else {
        panic!("exact amendment replay must recover its committed handoff");
    };
    assert_eq!(replayed.run.wake_epoch, applied_wake_epoch);
    assert_eq!(replayed.task_ids_to_release, vec![task.task_id]);
    assert_preserved_review_wait(&replayed.run);
    let AmendmentReplayOutcome::Replayed(recovered) = repository
        .recover_amendment_handoff(scope, run.run_uid, run.session_id, 1, &amendment_digest)
        .await?
    else {
        panic!(
            "persisted amendment revision/hash/audit must recover before stale-revision rejection"
        );
    };
    assert_eq!(recovered.run.wake_epoch, applied_wake_epoch);
    assert_eq!(recovered.task_ids_to_release, vec![task.task_id]);
    assert_preserved_review_wait(&recovered.run);
    assert_eq!(
        repository
            .recover_amendment_handoff(
                scope,
                run.run_uid,
                run.session_id,
                1,
                &ExecutionHash::from_bytes([99; 32]),
            )
            .await?,
        AmendmentReplayOutcome::Conflict
    );
    let amended = amended.run;
    assert_eq!(amended.plan_revision, 2);
    assert_preserved_review_wait(&amended);
    assert_eq!(amended.initial_plan_hash, run.initial_plan_hash);
    assert_eq!(amended.active_plan_hash, replacement_plan.plan_hash);
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
    let node_states: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT node_id,node_status,dependency_count,remaining_dependency_count \
         FROM moa.execution_node_state WHERE run_uid=$1 ORDER BY node_order",
    )
    .bind(run.run_uid)
    .fetch_all(test_db.store().pool())
    .await?;
    assert_eq!(
        node_states,
        vec![
            ("preserved".to_string(), "completed".to_string(), 0, 0),
            ("preserved_review".to_string(), "waiting".to_string(), 0, 0,),
            ("replacement".to_string(), "pending".to_string(), 1, 0),
            ("new_terminal".to_string(), "pending".to_string(), 1, 1),
        ]
    );
    let activation = repository
        .load_activation_projection(scope, run.run_uid, 10)
        .await?
        .expect("amended run remains schedulable");
    assert_eq!(
        activation
            .nodes
            .iter()
            .map(|node| node.node_id.as_str())
            .collect::<Vec<_>>(),
        vec!["replacement"]
    );
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
    let persisted_task = page
        .tasks
        .iter()
        .find(|persisted| persisted.task_id == task.task_id)
        .expect("superseded replan task must remain in the run task projection")
        .clone();
    assert_eq!(persisted_task.status, ExecutionTaskStatus::Cancelled);
    assert_eq!(persisted_task.actual_tasks, 1);
    assert_eq!(
        persisted_task.current_outcome,
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
