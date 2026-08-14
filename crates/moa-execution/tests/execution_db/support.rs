//! Shared fixtures and contract helpers for execution PostgreSQL tests.

pub(crate) use chrono::{Duration, Utc};
pub(crate) use moa_artifacts::execution_plan::{
    ExecutionBudgetLimit, ExecutionCancelPolicy, ExecutionCitation, ExecutionFailureClass,
    ExecutionGoalContract, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionTemporalTarget,
    ExecutionUsage, ExecutionWaitExpiryAction, ExecutionWaitPolicy, PlanAmendment, RetryPolicy,
};
pub(crate) use moa_config::ExecutionConfig;
pub(crate) use moa_core::canonical_json::canonical_json_bytes;
pub(crate) use moa_core::traits::{Identity, IdentityType};
pub(crate) use moa_core::types::{
    contact::ContactId,
    execution_planning::{
        ExecutionAuditReport, ExecutionCompileOutcome, ExecutionCompileSource,
        ExecutionPlannerCallKind, ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope,
        ExecutionPlanningAuditPayload, ExecutionRouteClassifierOutcome, ExecutionRouteDecision,
        ExecutionRouteKind, ExecutionRouteProvenance, ExecutionRouteSource, ExecutionRouteStage,
        ExecutionRouteUsage, ExecutionSourceProvenance, ExecutionStrategy,
    },
    identifiers::{SessionId, TenantId, UserId},
};
pub(crate) use moa_execution::{
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash, amendment_hash,
    },
    compiler::{CanonicalExecutionPlan, ExecutionValidationReport},
    completion::{
        CompletionEvaluation, CompletionStatus, execution_terminal_reason,
        terminal_evidence_from_evaluation,
    },
    replan::{ReplanStopReason, failure_fingerprint},
    repository::audit::{
        CompileAuditWriteOutcome, NewExecutionPlanningContext, PlannerCallAuditWriteOutcome,
        PlanningContextWriteOutcome, RouteAuditWriteOutcome,
    },
    repository::run::RunAdmissionOutcome,
    repository::terminal::{FinalizationOutcome, RunFinalizationRequest},
    repository::{
        AmendmentReplayOutcome, AmendmentWrite, ConfirmationConflict, ConfirmationOutcome,
        ExecutionActivationState, ExecutionAttemptState, ExecutionNodeMaterialization,
        ExecutionRepository, ExecutionRunActivationCheckpoint, ExecutionRunRecord, ExecutionScope,
        ExecutionTaskPageRequest, ExecutionTaskRecord, MaterializationOutcome, NewExecutionRun,
        ReservationOutcome, ReservationRejection, RunActivationWriteOutcome,
        RunControllerClaimOutcome, RunControllerCompletionOutcome, RunControllerCompletionRequest,
        TaskOutcomeRejection, TaskOutcomeWrite, TransitionOutcome, ValidatedAmendment,
    },
    state::{
        ExecutionRunStatus, ExecutionSourceKind, ExecutionTaskId, ExecutionTaskStatus,
        ExecutionTerminalCause, ExecutionTerminalReason, FailureFingerprintInput, LogicalTask,
        LogicalTaskKind, PendingExecutionTerminal, TerminalProjection,
    },
    wire::{
        ExecutionActionReviewResolution, ExecutionPlanningContextSnapshot,
        execution_progress_from_run, planning_context_hash,
    },
};
pub(crate) use serde_json::json;
pub(crate) use tokio::task::JoinSet;
pub(crate) use uuid::Uuid;

/// Shared fallible result for concurrent database contract tests.
pub(crate) type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// Advances a newly admitted run through one bounded controller continuation.
pub(crate) async fn claim_running_controller(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    config: &ExecutionConfig,
    run: &ExecutionRunRecord,
) -> Result<ExecutionRunRecord, moa_execution::Error> {
    let claimed = match repository
        .claim_controller_wake(
            scope,
            run.run_uid,
            run.controller_generation,
            run.wake_epoch,
        )
        .await?
    {
        RunControllerClaimOutcome::Claimed(claimed) => claimed,
        outcome => {
            return Err(moa_execution::Error::InvalidRepositoryData {
                message: format!("initial controller wake was not claimable: {outcome:?}"),
            });
        }
    };
    let continued = match repository
        .complete_controller_wake(
            scope,
            config,
            claimed.run_uid,
            RunControllerCompletionRequest {
                controller_generation: claimed.controller_generation,
                wake_epoch: claimed.wake_epoch,
                checkpoint: ExecutionRunActivationCheckpoint {
                    status: ExecutionRunStatus::Running,
                    activation_state: ExecutionActivationState::Queued,
                    next_wake_at: claimed.next_wake_at,
                    waiting_since: None,
                    ready_task_count: claimed.ready_task_count,
                    active_task_count: claimed.active_task_count,
                },
                continuation_payload: Some(json!({"reason": "test_controller_continuation"})),
                continuation_not_before_at: Utc::now(),
            },
        )
        .await?
    {
        RunControllerCompletionOutcome::Applied { run, .. } => *run,
        outcome => {
            return Err(moa_execution::Error::InvalidRepositoryData {
                message: format!("initial controller continuation was not committed: {outcome:?}"),
            });
        }
    };
    match repository
        .claim_controller_wake(
            scope,
            continued.run_uid,
            continued.controller_generation,
            continued.wake_epoch,
        )
        .await?
    {
        RunControllerClaimOutcome::Claimed(running) => Ok(running),
        outcome => Err(moa_execution::Error::InvalidRepositoryData {
            message: format!("continued controller wake was not claimable: {outcome:?}"),
        }),
    }
}

/// Asserts complete, non-overlapping pagination for one run's expected tasks.
pub(crate) async fn assert_task_pages(
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

/// Loads one task from its run-scoped list projection.
pub(crate) async fn listed_task(
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

/// Returns whether the durable run contract permits one status transition.
pub(crate) fn run_transition_allowed(source: &str, target: &str) -> bool {
    match source {
        "awaiting_confirmation" => matches!(target, "queued" | "cancelled"),
        "queued" => matches!(
            target,
            "running"
                | "waiting_review"
                | "waiting_signal"
                | "waiting_timer"
                | "pause_requested"
                | "compensating"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "running" => matches!(
            target,
            "waiting_input"
                | "waiting_review"
                | "waiting_signal"
                | "waiting_timer"
                | "waiting_external"
                | "waiting_replan"
                | "pause_requested"
                | "compensating"
                | "completed"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "waiting_input" => matches!(
            target,
            "running"
                | "waiting_review"
                | "waiting_signal"
                | "waiting_timer"
                | "waiting_external"
                | "waiting_replan"
                | "pause_requested"
                | "compensating"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "waiting_review" => matches!(
            target,
            "running"
                | "waiting_input"
                | "waiting_signal"
                | "waiting_timer"
                | "waiting_external"
                | "waiting_replan"
                | "pause_requested"
                | "compensating"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "waiting_signal" => matches!(
            target,
            "running"
                | "waiting_input"
                | "waiting_review"
                | "waiting_timer"
                | "waiting_external"
                | "waiting_replan"
                | "pause_requested"
                | "compensating"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "waiting_timer" => matches!(
            target,
            "running"
                | "waiting_input"
                | "waiting_review"
                | "waiting_signal"
                | "waiting_external"
                | "waiting_replan"
                | "pause_requested"
                | "compensating"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "waiting_external" => matches!(
            target,
            "running"
                | "waiting_input"
                | "waiting_review"
                | "waiting_signal"
                | "waiting_timer"
                | "waiting_replan"
                | "pause_requested"
                | "compensating"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "waiting_replan" => matches!(
            target,
            "running"
                | "waiting_input"
                | "waiting_review"
                | "waiting_signal"
                | "waiting_timer"
                | "waiting_external"
                | "pause_requested"
                | "compensating"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "pause_requested" => matches!(target, "pausing" | "paused" | "running" | "cancelled"),
        "pausing" => matches!(target, "paused" | "failed" | "cancelled"),
        "paused" => matches!(target, "queued" | "compensating" | "failed" | "cancelled"),
        "compensating" => matches!(
            target,
            "pause_requested"
                | "completed"
                | "partial"
                | "blocked"
                | "unsupported"
                | "failed"
                | "cancelled"
        ),
        "completed" | "partial" | "blocked" | "unsupported" | "failed" | "cancelled" => false,
        other => panic!("unknown run status in contract table: {other}"),
    }
}

/// Returns a legal setup path to one run status for transition-matrix tests.
pub(crate) fn run_setup_path(status: &str) -> &'static [&'static str] {
    match status {
        "awaiting_confirmation" | "queued" => &[],
        "running" => &["running"],
        "waiting_input" => &["running", "waiting_input"],
        "waiting_review" => &["running", "waiting_review"],
        "waiting_signal" => &["running", "waiting_signal"],
        "waiting_timer" => &["running", "waiting_timer"],
        "waiting_external" => &["running", "waiting_external"],
        "waiting_replan" => &["running", "waiting_replan"],
        "pause_requested" => &["running", "pause_requested"],
        "pausing" => &["running", "pause_requested", "pausing"],
        "paused" => &["running", "pause_requested", "paused"],
        "compensating" => &["running", "compensating"],
        "completed" => &["running", "completed"],
        "partial" => &["running", "partial"],
        "blocked" => &["blocked"],
        "unsupported" => &["unsupported"],
        "failed" => &["failed"],
        "cancelled" => &["cancelled"],
        other => panic!("unknown run status setup: {other}"),
    }
}

/// Advances one run through the provided legal setup path.
pub(crate) async fn set_run_status_path(
    pool: &sqlx::PgPool,
    run_uid: Uuid,
    path: &[&str],
) -> TestResult {
    for status in path {
        let terminal_cause = match *status {
            "completed" | "partial" => Some(json!({"kind":"completion","limit_stop":null})),
            "blocked" => Some(json!({"kind":"completion","limit_stop":null})),
            "unsupported" => Some(json!({"kind":"task_failure","class":"unsupported"})),
            "failed" => Some(json!({"kind":"internal_failure"})),
            "cancelled" => Some(json!({"kind":"cancellation"})),
            _ => None,
        };
        let terminal_count = terminal_cause.as_ref().map(|_| 0_i64);
        let terminal_reason = match *status {
            "completed" => Some("completed"),
            "partial" => Some("goal_incomplete"),
            "blocked" => Some("blocked"),
            "unsupported" => Some("unsupported_plan"),
            "failed" => Some("internal_failure"),
            "cancelled" => Some("cancelled"),
            _ => None,
        };
        let pending_terminal_status = (*status == "compensating").then_some("failed");
        let pending_terminal_reason = (*status == "compensating").then_some("internal_failure");
        let pending_terminal_cause = (*status == "compensating").then(|| {
            json!({
                "terminal_evidence": {
                    "cause": {"kind": "internal_failure"},
                    "satisfied_requirement_count": 0,
                    "requirement_count": 0
                },
                "completion_check_results": [],
                "terminal_gaps": []
            })
        });
        assert_eq!(
            sqlx::query(
                "UPDATE moa.execution_run SET status = $2, terminal_cause = $3, terminal_satisfied_requirement_count = $4, terminal_requirement_count = $4, terminal_reason = $5, pending_terminal_status = $6, pending_terminal_reason = $7, pending_terminal_cause = $8 WHERE run_uid = $1",
            )
                .bind(run_uid)
                .bind(status)
                .bind(terminal_cause)
                .bind(terminal_count)
                .bind(terminal_reason)
                .bind(pending_terminal_status)
                .bind(pending_terminal_reason)
                .bind(pending_terminal_cause)
                .execute(pool)
                .await?
                .rows_affected(),
            1,
            "setup transition to {status} must apply"
        );
    }
    Ok(())
}

/// Returns whether the durable task contract permits one status transition.
pub(crate) fn task_transition_allowed(source: &str, target: &str) -> bool {
    match source {
        "pending" => matches!(
            target,
            "ready"
                | "reserved"
                | "waiting_review"
                | "waiting_signal"
                | "waiting_timer"
                | "failed"
                | "skipped"
                | "cancelled"
        ),
        "ready" => matches!(target, "dispatching" | "reserved" | "cancelled"),
        "reserved" => matches!(target, "dispatching" | "running" | "cancelled"),
        "dispatching" => matches!(target, "running" | "ready" | "failed" | "cancelled"),
        "running" => matches!(
            target,
            "ready"
                | "waiting_input"
                | "waiting_review"
                | "waiting_signal"
                | "waiting_timer"
                | "waiting_external"
                | "waiting_replan"
                | "completed"
                | "failed"
                | "cancelled"
                | "unknown_outcome"
        ),
        "waiting_input" | "waiting_review" | "waiting_signal" | "waiting_timer" => {
            matches!(target, "ready" | "cancelled")
        }
        "waiting_external" => matches!(
            target,
            "ready" | "completed" | "failed" | "cancelled" | "unknown_outcome"
        ),
        "waiting_replan" => matches!(target, "ready" | "cancelled"),
        "completed" | "skipped" | "failed" | "cancelled" | "unknown_outcome" => false,
        other => panic!("unknown task status in contract table: {other}"),
    }
}

/// Returns a legal setup path to one task status for transition-matrix tests.
pub(crate) fn task_setup_path(status: &str) -> &'static [&'static str] {
    match status {
        "pending" => &[],
        "ready" => &["ready"],
        "reserved" => &["reserved"],
        "dispatching" => &["ready", "dispatching"],
        "running" => &["reserved", "running"],
        "waiting_input" => &["reserved", "running", "waiting_input"],
        "waiting_review" => &["reserved", "running", "waiting_review"],
        "waiting_signal" => &["reserved", "running", "waiting_signal"],
        "waiting_timer" => &["reserved", "running", "waiting_timer"],
        "waiting_external" => &["reserved", "running", "waiting_external"],
        "waiting_replan" => &["reserved", "running", "waiting_replan"],
        "completed" => &["reserved", "running", "completed"],
        "skipped" => &["skipped"],
        "failed" => &["reserved", "running", "failed"],
        "unknown_outcome" => &["reserved", "running", "unknown_outcome"],
        "cancelled" => &["cancelled"],
        other => panic!("unknown task status setup: {other}"),
    }
}

/// Advances one task through the provided legal setup path.
pub(crate) async fn set_task_status_path(
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

/// Asserts that a database mutation failed with the expected contract message.
pub(crate) fn assert_db_error_contains(
    result: Result<sqlx::postgres::PgQueryResult, sqlx::Error>,
    expected: &str,
) {
    let error = result.expect_err("database guard must reject the mutation");
    assert!(
        error.to_string().contains(expected),
        "expected database error containing `{expected}`, got `{error}`"
    );
}

/// Counts route-audit rows after assuming the application role and installing
/// one exact tenant/contact/control-plane RLS scope for the transaction.
pub(crate) async fn count_route_audits_as_app_role(
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

/// Creates a run after ensuring its immutable planning context exists.
pub(crate) async fn create_run(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run: NewExecutionRun,
) -> Result<moa_execution::repository::ExecutionRunRecord, moa_execution::Error> {
    match create_run_with_config(repository, scope, &ExecutionConfig::default(), run).await? {
        RunAdmissionOutcome::Admitted(run) | RunAdmissionOutcome::Replayed(run) => Ok(*run),
        RunAdmissionOutcome::CapacitySaturated { dimension } => {
            Err(moa_execution::Error::CapacitySaturated {
                dimension: dimension.as_str(),
            })
        }
    }
}

/// Admits a run with explicit execution-capacity limits after seeding its planning context.
pub(crate) async fn create_run_with_config(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    config: &ExecutionConfig,
    mut run: NewExecutionRun,
) -> Result<RunAdmissionOutcome, moa_execution::Error> {
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
    repository.create_run(scope, config, run).await
}

/// Builds a minimal durable execution-run fixture.
pub(crate) fn new_run(
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
        admitted_identity: Identity {
            identity_type: if contact_id.is_some() {
                IdentityType::Contact
            } else {
                IdentityType::Operator
            },
            id: contact_id.map_or_else(Uuid::now_v7, |value| value.0),
            tenant_id,
            api_key_id: None,
            acting_on_behalf_of: None,
        },
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
            skill_template_ref: format!("skill://{key}"),
            skill_template_revision_uid: Uuid::now_v7(),
        },
        input: json!({ "query": key }),
        status,
        approved_budget,
        idempotency_key: Some(key.to_string()),
    }
}

/// Builds a deterministic canonical execution-plan fixture.
pub(crate) fn canonical_plan(seed: u8) -> CanonicalExecutionPlan {
    CanonicalExecutionPlan {
        definition: moa_artifacts::execution_plan::ExecutionPlanDefinition {
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
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

/// A deadline offset from now, truncated to what Postgres can round-trip.
///
/// TIMESTAMPTZ carries microseconds, so a nanosecond-precision deadline only
/// equals its repository round-trip when the wall clock happens to land on a
/// whole microsecond — true on microsecond-granular macOS clocks and false
/// almost always on nanosecond-granular Linux CI clocks. Every budget a test
/// may read back must build its deadline through this helper.
pub(crate) fn pg_deadline(offset: Duration) -> chrono::DateTime<Utc> {
    let deadline = moa_test_support::fixtures::pg_now() + offset;
    chrono::DateTime::<Utc>::from_timestamp_micros(deadline.timestamp_micros())
        .expect("test deadline offsets are representable at microsecond precision")
}

/// Builds a bounded execution-budget fixture.
pub(crate) fn budget(max_tasks: u64) -> ExecutionBudgetLimit {
    let deadline = pg_deadline(Duration::hours(1));
    ExecutionBudgetLimit {
        max_cost_microusd: Some(max_tasks.saturating_mul(100)),
        max_tokens: Some(max_tasks.saturating_mul(100)),
        max_tasks: Some(max_tasks),
        max_tool_calls: Some(max_tasks.saturating_mul(10)),
        max_retrieved_bytes: Some(max_tasks.saturating_mul(1_000)),
        deadline_at: Some(deadline),
    }
}

/// Builds a bounded execution-budget fixture without an absolute run deadline.
pub(crate) fn budget_without_deadline(max_tasks: u64) -> ExecutionBudgetLimit {
    let mut budget = budget(max_tasks);
    budget.deadline_at = None;
    budget
}

/// Builds a scaled execution-estimate fixture.
pub(crate) fn estimate(scale: u64) -> ExecutionEstimate {
    ExecutionEstimate {
        cost_microusd: scale,
        tokens: scale,
        tasks: 1,
        tool_calls: scale,
        retrieved_bytes: scale,
    }
}

/// Builds a deterministic logical-task fixture for one run.
pub(crate) fn logical_task(
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
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 3,
            initial_backoff_ms: 1,
            max_backoff_ms: 10,
        },
        reservation,
    }
}

/// Reserves and starts one materialized task.
pub(crate) async fn reserve_and_start(
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

/// Builds an execution-usage fixture with each dimension set to one value.
pub(crate) fn usage(value: u64) -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: value,
        tokens: value,
        tool_calls: value,
        retrieved_bytes: value,
    }
}

/// Asserts the complete durable projection for a terminal redispatch failure.
pub(crate) fn assert_terminal_redispatch_failure(
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

/// Builds a completed task-outcome fixture.
pub(crate) fn completed(value: u64) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(value),
        result: ExecutionTaskResult::Completed {
            output: json!({ "tokens": value }),
            citations: Vec::new(),
        },
    }
}

/// Builds a needs-input task-outcome fixture.
pub(crate) fn needs_input(value: u64) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(value),
        result: ExecutionTaskResult::NeedsInput {
            question: "continue?".to_string(),
            audience: moa_artifacts::execution_plan::InputAudience::User,
        },
    }
}

/// Builds a needs-replan task-outcome fixture.
pub(crate) fn needs_replan(value: u64) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(value),
        result: ExecutionTaskResult::NeedsReplan {
            reason: "source unavailable".to_string(),
            evidence: json!({ "retry": false }),
        },
    }
}

/// Builds a retryable-failure task-outcome fixture.
pub(crate) fn retryable(value: u64) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: usage(value),
        result: ExecutionTaskResult::Failed {
            class: moa_artifacts::execution_plan::ExecutionFailureClass::Retryable,
            message: "retry later".to_string(),
        },
    }
}
