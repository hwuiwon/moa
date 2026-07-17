//! Strict terminal-cause and remaining planning-audit service coverage.
//!
//! Natural routing, planner, amendment, cancellation, and status reads use the
//! production service boundaries. Defensive scheduler no-progress, infrastructure
//! failure and alternate terminal-write conflicts have no
//! public mutation API; those cases deliberately enter through [`ExecutionRepository`]
//! and then assert the public `Execution/status` projection. Experiment-template and
//! skill-regression compile producers remain owned by their dedicated binaries; this
//! module does not duplicate those full workflows.

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit, ExecutionFailureClass,
    ExecutionGoalContract, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
    ExecutionRequirement, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionUsage,
    GeneratedAmendmentCandidate, GeneratedExecutionCandidate, PlanAmendment,
    PlanAmendmentOperation, RetryPolicy,
};
use moa_core::{
    config::ExecutionConfig,
    events::Event,
    types::{
        contact::SessionActorRef,
        execution_planning::{
            ExecutionCompileOutcome, ExecutionCompileSource, ExecutionMode,
            ExecutionPlannerCallKind, ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelopeV1,
            ExecutionPlanningAuditPayloadV1, ExecutionRouteDecisionKind, ExecutionRouteReason,
            ExecutionRouteStage,
        },
        identifiers::{SessionId, TenantId, UserId},
        session::SessionStatus,
    },
    wire::turn::TurnOutcomeKind,
};
use moa_execution::{
    capability::{
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate,
        ExecutionHash,
    },
    compiler::{CompileExecutionRequest, CompiledExecution, compile},
    completion::{
        CompletionEvaluation, CompletionStatus, execution_terminal_reason,
        terminal_evidence_from_evaluation,
    },
    replan::ReplanStopReason,
    repository::{
        CancellationOutcome, CancellationRequest, ExecutionRepository, ExecutionRunRecord,
        ExecutionScope, FinalizationOutcome, NewExecutionRun, ReplanStopOutcome, ReplanStopRequest,
        ReservationOutcome, RunFinalizationRequest, TaskOutcomeWrite, TransitionOutcome,
    },
    state::{
        ExecutionLimitStop, ExecutionRunStatus, ExecutionTaskFailure, ExecutionTaskId,
        ExecutionTerminalCause, ExecutionTerminalEvidence, LogicalTask, LogicalTaskKind,
        TerminalProjection,
    },
    wire::{
        ExecutionCancelRequest, ExecutionMutationResponse, ExecutionRunRequest,
        ExecutionStatusResponse,
    },
};
use moa_test_support::{
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool,
    OrchestratorTestFixture, TestApiClient,
};
use serde_json::{Value, json};

use crate::execution_execution_support::assertions::{
    assert_completed_terminal, assert_no_execution_lifecycle_events, journal_requests,
    planning_audits,
};
use crate::execution_execution_support::fixtures::{
    SERVICE_TIMEOUT, await_execution_terminal, await_session_settled, await_turn_outcome,
    execution_run_request, raw_events, route_classifier_completion,
    route_classifier_needs_input_completion, seed_allow_policy, start_turn, start_turn_in_session,
};

const PLANNER_MATCH: &str = "<frozen_planning_context>";
const AMENDMENT_MATCH: &str = "<frozen_amendment_context>";
const SYNTHESIS_MATCH: &str = "Synthesize the final user response for execution run";
const ESCALATION_OBJECTIVE: &str = "Investigate the unusual failure and explain it";
const ESCALATION_TOOL: &str = "discover_fixture_execution_shape";
const REPLAN_SEED_AGENT_SENTINEL: &str = "TERMINAL_MATRIX_REPLAN_SEED_AGENT_V1";
const REPLAN_AGENT_SENTINEL: &str = "TERMINAL_MATRIX_REPLAN_AGENT_V1";
const REPAIRED_AGENT_SENTINEL: &str = "TERMINAL_MATRIX_REPAIRED_AGENT_V1";
const REQ_USEFUL: &str = "useful";
const REQ_REMAINING: &str = "remaining";

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn no_run_needs_input_persists_strict_route_audit_service_e2e() -> Result<()> {
    // Pins: the small classifier can request concrete missing input without
    // invoking the planner, compiler, tools, or execution runtime.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected provider call"),
            "keyed": [route_classifier_needs_input_completion(
                ExecutionRouteReason::PreflightInputMissing,
                &["target"]
            )]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_turn(&test, "strict-needs-input", "do it", None).await?;
    let outcome = await_turn_outcome(test.client(), &started).await?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);

    let events = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_eq!(
        audits.len(),
        1,
        "preflight must emit exactly one route audit"
    );
    assert!(matches!(
        audits[0].payload,
        ExecutionPlanningAuditPayloadV1::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteDecisionKind::NeedsInput,
            mode: None,
            reason: ExecutionRouteReason::PreflightInputMissing,
            ..
        }
    ));
    assert_no_execution_lifecycle_events(&events);
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::ToolCall { .. } | Event::ToolResult { .. }
            ))
            .count(),
        0
    );
    let requests = fixture.scripted_requests()?;
    assert_eq!(requests.len(), 1, "only the route classifier may run");
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn act_escalation_persists_strict_route_and_compile_history_service_e2e() -> Result<()> {
    // Pins: structured evidence discovered by a bounded Act turn records a
    // second typed route before one generated plan is admitted.
    let candidate = output_candidate(ESCALATION_OBJECTIVE, 1, json!({"answer": "escalated"}));
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionMode::Act,
                    ExecutionRouteReason::BoundedInteractiveWork
                ),
                keyed_completion(SYNTHESIS_MATCH, text_completion("escalated run complete")),
                keyed_completion(
                    PLANNER_MATCH,
                    text_completion(serde_json::to_string(&candidate)?)
                ),
                keyed_completion(
                    ESCALATION_OBJECTIVE,
                    json!({
                        "content": "",
                        "tool_calls": [{
                            "name": ESCALATION_TOOL,
                            "id": "terminal-matrix-act-escalation",
                            "input": {"query": "unusual failure"}
                        }]
                    })
                )
            ]
        }),
        FixtureCapabilityOptions {
            tools: vec![FixtureCapabilityTool {
                name: ESCALATION_TOOL.to_string(),
                description: "Discover a deterministic durable execution shape".to_string(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["query"],
                    "properties": {"query": {"type": "string"}}
                }),
                item_key_pointer: None,
                outcomes: vec![FixtureCapabilityOutcome::Success {
                    output: json!({
                        "execution_shape": {
                            "reason": "bulk_collection",
                            "summary": "the bounded probe discovered collection-wide work",
                            "value": {"company_count": 500}
                        }
                    }),
                }],
            }],
            orchestrator_env: Vec::new(),
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("strict-act-escalation").await?;
    let session = test.client().get_session(session_id).await?;
    seed_allow_policy(&fixture, test.client(), session.tenant_id, ESCALATION_TOOL).await?;
    let started = start_turn_in_session(&test, session_id, ESCALATION_OBJECTIVE, None).await?;
    let controller = fixture
        .fixture_capability()
        .context("execution fixture omitted its capability controller")?;
    let calls = controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, ESCALATION_TOOL);
    controller.release(1);

    let outcome = await_turn_outcome(test.client(), &started).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind else {
        bail!("Act escalation did not admit a run: {outcome:?}");
    };
    let status = await_execution_terminal(
        test.client(),
        &execution_run_request(&started, execution_run_uid),
    )
    .await?;
    assert_completed_terminal(&status, 1, 1);
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Paused
    );

    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_act_escalation_audits(&audits);
    assert_eq!(
        planning_audits(&fixture.postgres_url, started.session_id).await?,
        audits
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn rejected_initial_candidate_and_sole_repair_persist_strict_audits_service_e2e() -> Result<()>
{
    // Pins: one strict compiler rejection records both original operations,
    // permits one repair, and records exactly one repaired planner/compiler pair.
    let objective = "Start an execution run for a repaired deterministic report";
    let rejected = output_candidate(objective, 0, json!({"answer": "invalid retry policy"}));
    let accepted = output_candidate(objective, 1, json!({"answer": "repaired"}));
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "responses": [
                text_completion(serde_json::to_string(&rejected)?),
                text_completion(serde_json::to_string(&accepted)?)
            ],
            "keyed": [
                route_classifier_completion(
                    ExecutionMode::Run,
                    ExecutionRouteReason::ExplicitRun
                ),
                keyed_completion(SYNTHESIS_MATCH, text_completion("repair run complete"))
            ]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_turn(&test, "strict-initial-repair", objective, None).await?;
    let outcome = await_turn_outcome(test.client(), &started).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind else {
        bail!("repaired candidate did not admit a run: {outcome:?}");
    };
    let status = await_execution_terminal(
        test.client(),
        &execution_run_request(&started, execution_run_uid),
    )
    .await?;
    assert_completed_terminal(&status, 1, 1);
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Paused
    );

    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_repair_audits(&audits);
    assert_eq!(
        planning_audits(&fixture.postgres_url, started.session_id).await?,
        audits
    );

    let requests = journal_requests(fixture.scripted_requests()?)?;
    let strict_initial_calls = requests
        .iter()
        .filter(|request| {
            request
                .response_format
                .as_ref()
                .is_some_and(|format| format.name == "generated_execution_candidate_v1")
        })
        .count();
    assert_eq!(
        strict_initial_calls, 2,
        "initial planning may repair exactly once"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn oversized_initial_candidate_is_terminal_without_repair_service_e2e() -> Result<()> {
    // Pins: an oversized initial response records one typed planner audit and
    // cannot invoke the compiler, repair planner, or execution runtime.
    let objective = "Start an execution run for an oversized planner response";
    let oversized =
        "x".repeat(moa_brain::execution_planning::EXECUTION_PLANNER_CANDIDATE_MAX_BYTES + 1);
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "responses": [text_completion(oversized)],
            "keyed": [route_classifier_completion(
                ExecutionMode::Run,
                ExecutionRouteReason::ExplicitRun
            )]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_turn(&test, "strict-oversized-candidate", objective, None).await?;
    let outcome = await_turn_outcome(test.client(), &started).await?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);

    let before = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_eq!(
        audits.len(),
        2,
        "oversized planning emits route plus one planner call"
    );
    assert!(matches!(
        audits[0].payload,
        ExecutionPlanningAuditPayloadV1::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteDecisionKind::Routed,
            mode: Some(ExecutionMode::Run),
            reason: ExecutionRouteReason::ExplicitRun,
            ..
        }
    ));
    assert!(matches!(
        audits[1].payload,
        ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            run_uid: None,
            plan_revision: None,
            outcome: ExecutionPlannerOutcome::Oversized,
            candidate_hash: Some(_),
            candidate_json: None,
            compiler_report: Some(_),
            ..
        }
    ));
    assert!(audits.iter().all(|audit| !matches!(
        audit.payload,
        ExecutionPlanningAuditPayloadV1::Compile { .. }
    )));
    assert_no_execution_lifecycle_events(&before);
    assert_eq!(
        planning_audits(&fixture.postgres_url, started.session_id).await?,
        audits
    );
    assert_eq!(fixture.scripted_requests()?.len(), 2);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn amendment_planning_persists_revision_fenced_strict_audits_service_e2e() -> Result<()> {
    // Pins: a task-produced NeedsReplan invokes one amendment planner/compiler
    // pair keyed by the persisted run and revision, then preserves its replay identity.
    let objective = "Start an execution run that needs one deterministic amendment";
    let candidate = replan_candidate(objective);
    let amendment = useful_amendment_candidate(1);
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionMode::Run,
                    ExecutionRouteReason::ExplicitRun
                ),
                keyed_completion(SYNTHESIS_MATCH, text_completion("amended run complete")),
                keyed_completion(
                    AMENDMENT_MATCH,
                    text_completion(serde_json::to_string(&amendment)?)
                ),
                keyed_completion(
                    REPLAN_AGENT_SENTINEL,
                    text_completion(serde_json::to_string(&ExecutionTaskResult::NeedsReplan {
                        reason: "the initial research shape is unsupported".to_string(),
                        evidence: json!({"shape": "unsupported"}),
                    })?)
                ),
                keyed_completion(
                    REPLAN_SEED_AGENT_SENTINEL,
                    text_completion(serde_json::to_string(&json!({"answer": "seed"}))?)
                ),
                keyed_completion(
                    REPAIRED_AGENT_SENTINEL,
                    text_completion(serde_json::to_string(&json!({"answer": "repaired"}))?)
                ),
                keyed_completion(
                    PLANNER_MATCH,
                    text_completion(serde_json::to_string(&candidate)?)
                )
            ]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_turn(&test, "strict-amendment-audits", objective, None).await?;
    let outcome = await_turn_outcome(test.client(), &started).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind else {
        bail!("replan fixture did not admit a run: {outcome:?}");
    };
    let run_request = execution_run_request(&started, execution_run_uid);
    let status = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        await_execution_terminal(test.client(), &run_request),
    )
    .await
    {
        Ok(status) => status?,
        Err(_) => {
            let current: ExecutionStatusResponse = test
                .client()
                .post_call("/Execution/status", &run_request)
                .await?;
            let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
            let requests = journal_requests(fixture.scripted_requests()?)?;
            let response_formats = requests
                .iter()
                .filter_map(|request| request.response_format.as_ref())
                .map(|format| format.name.as_str())
                .collect::<Vec<_>>();
            bail!(
                "automatic amendment did not finish in 15s; status={current:#?}; audits={audits:#?}; response_formats={response_formats:?}"
            );
        }
    };
    if status.run.status != ExecutionRunStatus::Completed {
        bail!("accepted amendment did not complete: {status:#?}");
    }
    assert_completed_terminal(&status, 2, 2);
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Paused
    );

    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_amendment_audits(&audits, execution_run_uid);
    assert_eq!(
        planning_audits(&fixture.postgres_url, started.session_id).await?,
        audits
    );

    let requests = journal_requests(fixture.scripted_requests()?)?;
    let response_formats = requests
        .iter()
        .filter_map(|request| {
            request
                .response_format
                .as_ref()
                .map(|format| format.name.as_str())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        response_formats,
        vec![
            "execution_route_classifier_v1",
            "generated_execution_candidate_v1",
            "generated_amendment_candidate_v1"
        ]
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn strict_terminal_cause_matrix_is_exhaustive_service_e2e() -> Result<()> {
    // Pins: the repository replay boundary and public status API retain every
    // runtime terminal cohort, useful/empty limit distinction, and exact counts.
    let fixture = OrchestratorTestFixture::shared().await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("strict-terminal-matrix").await?;
    let session = test.client().get_session(session_id).await?;
    let owner_user_id = session_owner(&session.created_by)?;
    let pool = sqlx::PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect strict terminal matrix repository")?;
    let repository = ExecutionRepository::new(pool);
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let blueprint = terminal_blueprint()?;

    for (index, case) in ordinary_terminal_cases().into_iter().enumerate() {
        assert_repository_terminal_case(
            &repository,
            scope,
            test.client(),
            session.tenant_id,
            session_id,
            owner_user_id.clone(),
            &blueprint,
            20 + index as u64,
            case,
        )
        .await?;
    }

    for (index, reason) in replan_reasons().into_iter().enumerate() {
        assert_replan_terminal_case(
            &repository,
            scope,
            test.client(),
            session.tenant_id,
            session_id,
            owner_user_id.clone(),
            &blueprint,
            80 + index as u64,
            reason,
        )
        .await?;
    }

    assert_cancellation_terminal_case(
        &repository,
        scope,
        test.client(),
        session.tenant_id,
        session_id,
        owner_user_id,
        &blueprint,
        100,
    )
    .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires local Restate, Postgres, OpenFGA, and the service-e2e feature lane"]
async fn internal_failure_persists_typed_cause_service_e2e() -> Result<()> {
    // Pins: infrastructure-failure finalization is not inferred from failure
    // prose; the closed InternalFailure cause survives repository restart and status reads.
    let fixture = OrchestratorTestFixture::shared().await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("typed-internal-failure").await?;
    let session = test.client().get_session(session_id).await?;
    let owner_user_id = session_owner(&session.created_by)?;
    let repository = ExecutionRepository::new(
        sqlx::PgPool::connect(&fixture.postgres_url)
            .await
            .context("connect internal-failure repository")?,
    );
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let blueprint = terminal_blueprint()?;
    assert_repository_terminal_case(
        &repository,
        scope,
        test.client(),
        session.tenant_id,
        session_id,
        owner_user_id,
        &blueprint,
        120,
        TerminalCase {
            label: "internal-failure",
            cause: ExecutionTerminalCause::InternalFailure,
            status: ExecutionRunStatus::Failed,
            completion_status: CompletionStatus::Failed,
            projection: terminal_failure_projection(
                ExecutionFailureClass::Terminal,
                "injected infrastructure failure",
            ),
            output: None,
            gaps: vec!["internal execution failure".to_string()],
            satisfied: Vec::new(),
            unsatisfied: vec![REQ_USEFUL.to_string(), REQ_REMAINING.to_string()],
        },
    )
    .await
}

#[derive(Clone)]
struct RunBlueprint {
    compiled: CompiledExecution,
    catalog: ExecutionCapabilityCatalog,
    authorization: ExecutionAuthorizationEnvelope,
    budget: ExecutionBudgetLimit,
}

#[derive(Clone)]
struct TerminalCase {
    label: &'static str,
    cause: ExecutionTerminalCause,
    status: ExecutionRunStatus,
    completion_status: CompletionStatus,
    projection: TerminalProjection,
    output: Option<Value>,
    gaps: Vec<String>,
    satisfied: Vec<String>,
    unsatisfied: Vec<String>,
}

fn ordinary_terminal_cases() -> Vec<TerminalCase> {
    vec![
        TerminalCase {
            label: "completion",
            cause: ExecutionTerminalCause::Completion { limit_stop: None },
            status: ExecutionRunStatus::Completed,
            completion_status: CompletionStatus::Completed,
            projection: TerminalProjection::Completed {
                output: json!({"result": "complete"}),
            },
            output: Some(json!({"result": "complete"})),
            gaps: Vec::new(),
            satisfied: vec![REQ_USEFUL.to_string(), REQ_REMAINING.to_string()],
            unsatisfied: Vec::new(),
        },
        TerminalCase {
            label: "task-failure",
            cause: ExecutionTerminalCause::TaskFailure {
                class: ExecutionFailureClass::InvalidOutput,
            },
            status: ExecutionRunStatus::Failed,
            completion_status: CompletionStatus::Failed,
            projection: terminal_failure_projection(
                ExecutionFailureClass::InvalidOutput,
                "task output violated its schema",
            ),
            output: None,
            gaps: vec!["task output violated its schema".to_string()],
            satisfied: Vec::new(),
            unsatisfied: vec![REQ_USEFUL.to_string(), REQ_REMAINING.to_string()],
        },
        limit_case(ExecutionLimitStop::DeadlineExceeded, true),
        limit_case(ExecutionLimitStop::DeadlineExceeded, false),
        limit_case(ExecutionLimitStop::BudgetExceeded, true),
        limit_case(ExecutionLimitStop::BudgetExceeded, false),
        TerminalCase {
            label: "scheduler-no-progress",
            cause: ExecutionTerminalCause::SchedulerNoProgress,
            status: ExecutionRunStatus::Failed,
            completion_status: CompletionStatus::Failed,
            projection: terminal_failure_projection(
                ExecutionFailureClass::Terminal,
                "scheduler made no progress",
            ),
            output: None,
            gaps: vec!["scheduler made no progress".to_string()],
            satisfied: Vec::new(),
            unsatisfied: vec![REQ_USEFUL.to_string(), REQ_REMAINING.to_string()],
        },
    ]
}

fn limit_case(reason: ExecutionLimitStop, useful: bool) -> TerminalCase {
    let (label, failure_class, gap) = match reason {
        ExecutionLimitStop::DeadlineExceeded => (
            if useful {
                "deadline-useful"
            } else {
                "deadline-empty"
            },
            ExecutionFailureClass::DeadlineExceeded,
            "execution deadline exceeded",
        ),
        ExecutionLimitStop::BudgetExceeded => (
            if useful {
                "budget-useful"
            } else {
                "budget-empty"
            },
            ExecutionFailureClass::BudgetExceeded,
            "execution budget exceeded",
        ),
    };
    TerminalCase {
        label,
        cause: ExecutionTerminalCause::LimitStop { reason },
        status: if useful {
            ExecutionRunStatus::Partial
        } else {
            ExecutionRunStatus::Failed
        },
        completion_status: if useful {
            CompletionStatus::Partial
        } else {
            CompletionStatus::Failed
        },
        projection: if useful {
            TerminalProjection::Partial {
                output: Some(json!({"useful": true})),
                gaps: vec![gap.to_string()],
            }
        } else {
            terminal_failure_projection(failure_class, gap)
        },
        output: useful.then(|| json!({"useful": true})),
        gaps: vec![gap.to_string()],
        satisfied: useful.then(|| REQ_USEFUL.to_string()).into_iter().collect(),
        unsatisfied: if useful {
            vec![REQ_REMAINING.to_string()]
        } else {
            vec![REQ_USEFUL.to_string(), REQ_REMAINING.to_string()]
        },
    }
}

fn replan_reasons() -> [ReplanStopReason; 6] {
    [
        ReplanStopReason::DuplicatePlan,
        ReplanStopReason::DuplicateAmendment,
        ReplanStopReason::RepeatedFailure,
        ReplanStopReason::NoProgress,
        ReplanStopReason::DeadlineExceeded,
        ReplanStopReason::BudgetExhausted,
    ]
}

#[allow(
    clippy::too_many_arguments,
    reason = "the service scenario keeps tenant, session, owner, and immutable run cohort explicit"
)]
async fn assert_repository_terminal_case(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    client: &TestApiClient,
    tenant_id: TenantId,
    session_id: SessionId,
    owner_user_id: UserId,
    blueprint: &RunBlueprint,
    origin: u64,
    case: TerminalCase,
) -> Result<()> {
    let run = create_active_run(
        repository,
        scope,
        tenant_id,
        session_id,
        owner_user_id,
        blueprint,
        origin,
        case.label,
    )
    .await?;
    let evaluation = CompletionEvaluation {
        status: case.completion_status,
        limit_stop: match &case.cause {
            ExecutionTerminalCause::Completion { limit_stop } => *limit_stop,
            ExecutionTerminalCause::TaskFailure { .. }
            | ExecutionTerminalCause::LimitStop { .. }
            | ExecutionTerminalCause::SchedulerNoProgress
            | ExecutionTerminalCause::ReplanStop { .. }
            | ExecutionTerminalCause::Cancellation
            | ExecutionTerminalCause::InternalFailure => None,
        },
        checks: Vec::new(),
        satisfied_requirement_ids: case.satisfied.clone(),
        unsatisfied_requirement_ids: case.unsatisfied.clone(),
        gaps: case.gaps.clone(),
    };
    let evidence = terminal_evidence_from_evaluation(case.cause.clone(), &evaluation)?;
    let terminal_reason = execution_terminal_reason(&case.cause, &case.projection, &evaluation)?;
    let request = RunFinalizationRequest {
        run_uid: run.run_uid,
        expected_revision: run.plan_revision,
        expected_wake_epoch: run.wake_epoch,
        terminal_projection: case.projection.clone(),
        completion_evaluation: evaluation,
        terminal_evidence: evidence.clone(),
        terminal_reason,
    };
    let first = repository.finalize_run(scope, request.clone()).await?;
    let FinalizationOutcome::Finalized(first_record) = first else {
        bail!("{} did not finalize on first write: {first:?}", case.label);
    };
    assert_eq!(first_record.status, case.status, "{} status", case.label);
    assert_eq!(
        first_record.terminal_evidence,
        Some(evidence.clone()),
        "{} evidence",
        case.label
    );

    let replay = repository.finalize_run(scope, request.clone()).await?;
    let FinalizationOutcome::Replayed(replayed_record) = replay else {
        bail!("{} did not replay exactly: {replay:?}", case.label);
    };
    assert_eq!(
        replayed_record, first_record,
        "{} replay changed persisted bytes",
        case.label
    );

    let mut count_conflict = request.clone();
    count_conflict.terminal_evidence.satisfied_requirement_count =
        conflicting_satisfied_count(&evidence);
    assert_eq!(
        repository.finalize_run(scope, count_conflict).await?,
        FinalizationOutcome::Conflict,
        "{} accepted conflicting requirement counts",
        case.label
    );
    if let Some(alternate_cause) =
        alternate_terminal_cause(&case.cause, &request.terminal_projection)
    {
        let mut cause_conflict = request;
        cause_conflict.terminal_evidence.cause = alternate_cause;
        cause_conflict.terminal_reason = execution_terminal_reason(
            &cause_conflict.terminal_evidence.cause,
            &cause_conflict.terminal_projection,
            &cause_conflict.completion_evaluation,
        )?;
        assert_eq!(
            repository.finalize_run(scope, cause_conflict).await?,
            FinalizationOutcome::Conflict,
            "{} accepted a conflicting typed cause",
            case.label
        );
    }

    assert_status_projection(
        client,
        tenant_id,
        session_id,
        run.run_uid,
        case.status,
        case.output,
        case.gaps,
        evidence,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "each replan matrix row keeps its persisted run and task fences explicit"
)]
async fn assert_replan_terminal_case(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    client: &TestApiClient,
    tenant_id: TenantId,
    session_id: SessionId,
    owner_user_id: UserId,
    blueprint: &RunBlueprint,
    origin: u64,
    reason: ReplanStopReason,
) -> Result<()> {
    let label = reason.as_str();
    let run = create_active_run(
        repository,
        scope,
        tenant_id,
        session_id,
        owner_user_id,
        blueprint,
        origin,
        label,
    )
    .await?;
    let completed_task = logical_output_task(
        run.run_uid,
        "useful_output",
        vec![REQ_USEFUL.to_string()],
        json!({"useful": true}),
    )?;
    let waiting_task = logical_agent_task(
        run.run_uid,
        "waiting_replan",
        vec![REQ_REMAINING.to_string()],
    )?;
    repository
        .materialize_tasks(
            scope,
            run.run_uid,
            1,
            vec![completed_task.clone(), waiting_task.clone()],
        )
        .await?;
    reserve_and_start(repository, scope, run.run_uid, completed_task.task_id).await?;
    assert!(matches!(
        repository
            .record_task_outcome(
                scope,
                run.run_uid,
                completed_task.task_id,
                1,
                completed_outcome(json!({"useful": true})),
            )
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    reserve_and_start(repository, scope, run.run_uid, waiting_task.task_id).await?;
    assert!(matches!(
        repository
            .record_task_outcome(
                scope,
                run.run_uid,
                waiting_task.task_id,
                1,
                needs_replan_outcome(label),
            )
            .await?,
        TaskOutcomeWrite::Applied { .. }
    ));
    let waiting_run = repository
        .load_run(scope, run.run_uid)
        .await?
        .context("waiting-replan run disappeared")?;
    assert_eq!(waiting_run.status, ExecutionRunStatus::WaitingReplan);

    let gap = format!("replan stopped: {label}");
    let projection = TerminalProjection::Partial {
        output: Some(json!({"useful": true})),
        gaps: vec![gap.clone()],
    };
    let evaluation = CompletionEvaluation {
        status: CompletionStatus::Partial,
        limit_stop: None,
        checks: Vec::new(),
        satisfied_requirement_ids: vec![REQ_USEFUL.to_string()],
        unsatisfied_requirement_ids: vec![REQ_REMAINING.to_string()],
        gaps: vec![gap.clone()],
    };
    let evidence = terminal_evidence_from_evaluation(
        ExecutionTerminalCause::ReplanStop { reason },
        &evaluation,
    )?;
    let terminal_reason = execution_terminal_reason(
        &ExecutionTerminalCause::ReplanStop { reason },
        &projection,
        &evaluation,
    )?;
    let request = ReplanStopRequest {
        run_uid: run.run_uid,
        expected_revision: 1,
        expected_wake_epoch: waiting_run.wake_epoch,
        task_id: waiting_task.task_id,
        expected_generation: 1,
        amendment_hash: Some(ExecutionHash::from_bytes([origin as u8; 32])),
        cancellation_reason: gap.clone(),
        terminal_projection: projection,
        completion_evaluation: evaluation,
        terminal_evidence: evidence.clone(),
        terminal_reason,
    };
    let first = repository
        .finalize_replan_stop(scope, request.clone())
        .await?;
    let ReplanStopOutcome::Finalized(first_commit) = first else {
        bail!("{label} did not finalize through the replan transaction: {first:?}");
    };
    let replay = repository
        .finalize_replan_stop(scope, request.clone())
        .await?;
    let ReplanStopOutcome::Replayed(replayed_commit) = replay else {
        bail!("{label} did not replay through the replan transaction: {replay:?}");
    };
    assert_eq!(
        replayed_commit, first_commit,
        "{label} replay changed persisted bytes"
    );

    let mut count_conflict = request.clone();
    count_conflict.terminal_evidence.satisfied_requirement_count = 0;
    assert_eq!(
        repository
            .finalize_replan_stop(scope, count_conflict)
            .await?,
        ReplanStopOutcome::Conflict,
        "{label} accepted conflicting requirement counts"
    );
    let mut cause_conflict = request;
    cause_conflict.terminal_evidence.cause = ExecutionTerminalCause::ReplanStop {
        reason: alternate_replan_reason(reason),
    };
    cause_conflict.terminal_reason = execution_terminal_reason(
        &cause_conflict.terminal_evidence.cause,
        &cause_conflict.terminal_projection,
        &cause_conflict.completion_evaluation,
    )?;
    assert_eq!(
        repository
            .finalize_replan_stop(scope, cause_conflict)
            .await?,
        ReplanStopOutcome::Conflict,
        "{label} accepted a conflicting replan cause"
    );

    assert_status_projection(
        client,
        tenant_id,
        session_id,
        run.run_uid,
        ExecutionRunStatus::Partial,
        Some(json!({"useful": true})),
        vec![gap],
        evidence,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "the cancellation service case keeps its complete parent and run scope explicit"
)]
async fn assert_cancellation_terminal_case(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    client: &TestApiClient,
    tenant_id: TenantId,
    session_id: SessionId,
    owner_user_id: UserId,
    blueprint: &RunBlueprint,
    origin: u64,
) -> Result<()> {
    let run = create_active_run(
        repository,
        scope,
        tenant_id,
        session_id,
        owner_user_id,
        blueprint,
        origin,
        "cancellation",
    )
    .await?;
    let reason = "cancel strict terminal matrix".to_string();
    let request = ExecutionCancelRequest {
        run: ExecutionRunRequest {
            tenant_id,
            contact_id: None,
            session_id,
            run_uid: run.run_uid,
        },
        reason: reason.clone(),
    };
    let first: ExecutionMutationResponse = client.post_call("/Execution/cancel", &request).await?;
    let ExecutionMutationResponse::Applied { run: first_run } = first else {
        bail!("cancellation did not apply: {first:?}");
    };
    let evidence = ExecutionTerminalEvidence {
        cause: ExecutionTerminalCause::Cancellation,
        satisfied_requirement_count: 0,
        requirement_count: 2,
    };
    assert_eq!(first_run.status, ExecutionRunStatus::Cancelled);
    assert_eq!(first_run.terminal_evidence, Some(evidence.clone()));

    let replay: ExecutionMutationResponse = client.post_call("/Execution/cancel", &request).await?;
    assert_eq!(
        replay,
        ExecutionMutationResponse::Replayed {
            run: first_run.clone()
        }
    );
    let mut conflicting_counts = evidence.clone();
    conflicting_counts.satisfied_requirement_count = 1;
    assert_eq!(
        repository
            .cancel_run(
                scope,
                run.run_uid,
                CancellationRequest {
                    reason: reason.clone(),
                    terminal_evidence: conflicting_counts,
                },
            )
            .await?,
        CancellationOutcome::Conflict
    );
    let invalid_cause = repository
        .cancel_run(
            scope,
            run.run_uid,
            CancellationRequest {
                reason: reason.clone(),
                terminal_evidence: ExecutionTerminalEvidence {
                    cause: ExecutionTerminalCause::InternalFailure,
                    satisfied_requirement_count: 0,
                    requirement_count: 2,
                },
            },
        )
        .await;
    assert!(
        invalid_cause.is_err(),
        "cancellation accepted a non-cancellation cause"
    );

    let persisted = repository
        .load_run(scope, run.run_uid)
        .await?
        .context("cancelled run disappeared")?;
    assert_eq!(persisted.cancellation_reason, Some(reason));
    assert_status_projection(
        client,
        tenant_id,
        session_id,
        run.run_uid,
        ExecutionRunStatus::Cancelled,
        None,
        Vec::new(),
        evidence,
    )
    .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "run creation mirrors the explicit immutable production persistence cohort"
)]
async fn create_active_run(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    tenant_id: TenantId,
    session_id: SessionId,
    owner_user_id: UserId,
    blueprint: &RunBlueprint,
    origin: u64,
    label: &str,
) -> Result<ExecutionRunRecord> {
    let (planning_context_uid, planning_context_hash) = crate::create_test_planning_context(
        repository,
        scope,
        tenant_id,
        session_id,
        origin,
        owner_user_id.clone(),
        blueprint.catalog.clone(),
        blueprint.authorization.clone(),
        blueprint.budget.clone(),
    )
    .await?;
    let created = repository
        .create_run(
            scope,
            NewExecutionRun {
                tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: origin,
                planning_context_uid,
                planning_context_hash,
                owner_user_id,
                goal: blueprint.compiled.goal.clone(),
                plan: blueprint.compiled.plan.clone(),
                catalog: blueprint.catalog.clone(),
                authorization: blueprint.authorization.clone(),
                pinned_instruction_skills: Vec::new(),
                source_provenance: crate::test_source_provenance(
                    &blueprint.compiled.plan.plan_hash.to_string(),
                ),
                input: json!({}),
                status: ExecutionRunStatus::Queued,
                approved_budget: blueprint.budget.clone(),
                idempotency_key: Some(format!("terminal-matrix-{session_id}-{origin}-{label}")),
            },
        )
        .await?;
    match repository
        .transition_run_wait(
            scope,
            created.run_uid,
            ExecutionRunStatus::Queued,
            ExecutionRunStatus::Running,
        )
        .await?
    {
        TransitionOutcome::RunApplied(running) => Ok(running),
        other => bail!("{label} did not enter running state: {other:?}"),
    }
}

async fn reserve_and_start(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: uuid::Uuid,
    task_id: ExecutionTaskId,
) -> Result<()> {
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

#[allow(
    clippy::too_many_arguments,
    reason = "the status assertion names every externally visible terminal field"
)]
async fn assert_status_projection(
    client: &TestApiClient,
    tenant_id: TenantId,
    session_id: SessionId,
    run_uid: uuid::Uuid,
    expected_status: ExecutionRunStatus,
    expected_output: Option<Value>,
    expected_gaps: Vec<String>,
    expected_evidence: ExecutionTerminalEvidence,
) -> Result<()> {
    let status: ExecutionStatusResponse = client
        .post_call(
            "/Execution/status",
            &ExecutionRunRequest {
                tenant_id,
                contact_id: None,
                session_id,
                run_uid,
            },
        )
        .await?;
    assert_eq!(status.run.status, expected_status);
    assert_eq!(status.output, expected_output);
    assert_eq!(status.gaps, expected_gaps);
    assert_eq!(status.run.terminal_evidence, Some(expected_evidence));
    Ok(())
}

fn terminal_blueprint() -> Result<RunBlueprint> {
    let catalog = ExecutionCapabilityCatalog::build(Vec::new())?;
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: Vec::new(),
    };
    let budget = generous_budget();
    let output_schema = json!({"type": "object"});
    let outcome = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: "persist strict terminal evidence".to_string(),
            requirements: vec![
                ExecutionRequirement {
                    id: REQ_USEFUL.to_string(),
                    description: "preserve useful work".to_string(),
                },
                ExecutionRequirement {
                    id: REQ_REMAINING.to_string(),
                    description: "represent remaining work".to_string(),
                },
            ],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: Vec::new(),
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: empty_object_schema(),
            output_schema: output_schema.clone(),
            nodes: vec![ExecutionNode {
                id: "terminal_output".to_string(),
                requirement_ids: vec![REQ_USEFUL.to_string(), REQ_REMAINING.to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema,
                operation: ExecutionOperation::Output {
                    value: json!({"result": "terminal-matrix"}),
                },
                retry: no_retry(),
                budget: None,
            }],
        },
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: budget.clone(),
        config: ExecutionConfig::default(),
        now: chrono::Utc::now(),
    });
    let compiled = outcome.compiled.with_context(|| {
        format!(
            "terminal matrix plan should compile: {:?}",
            outcome.report.issues
        )
    })?;
    Ok(RunBlueprint {
        compiled,
        catalog,
        authorization,
        budget,
    })
}

fn logical_output_task(
    run_uid: uuid::Uuid,
    node_id: &str,
    requirement_ids: Vec<String>,
    value: Value,
) -> Result<LogicalTask> {
    Ok(LogicalTask {
        task_id: ExecutionTaskId::derive(run_uid, node_id, "")?,
        node_id: node_id.to_string(),
        item_key: String::new(),
        requirement_ids,
        plan_revision: 1,
        generation: 1,
        input: json!({}),
        kind: LogicalTaskKind::Output { value },
        retry: no_retry(),
        reservation: one_task_estimate(),
    })
}

fn logical_agent_task(
    run_uid: uuid::Uuid,
    node_id: &str,
    requirement_ids: Vec<String>,
) -> Result<LogicalTask> {
    Ok(LogicalTask {
        task_id: ExecutionTaskId::derive(run_uid, node_id, "")?,
        node_id: node_id.to_string(),
        item_key: String::new(),
        requirement_ids,
        plan_revision: 1,
        generation: 1,
        input: json!({}),
        kind: LogicalTaskKind::Agent {
            instructions: "wait for deterministic replan".to_string(),
            skill_refs: Vec::new(),
            capability_refs: Vec::new(),
            max_turns: 1,
        },
        retry: no_retry(),
        reservation: one_task_estimate(),
    })
}

fn completed_outcome(output: Value) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: empty_usage(),
        result: ExecutionTaskResult::Completed {
            output,
            citations: Vec::new(),
        },
    }
}

fn needs_replan_outcome(reason: &str) -> ExecutionTaskOutcome {
    ExecutionTaskOutcome {
        schema_version: 1,
        usage: empty_usage(),
        result: ExecutionTaskResult::NeedsReplan {
            reason: reason.to_string(),
            evidence: json!({"reason": reason}),
        },
    }
}

fn empty_usage() -> ExecutionUsage {
    ExecutionUsage {
        cost_microusd: 0,
        tokens: 0,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

fn one_task_estimate() -> ExecutionEstimate {
    ExecutionEstimate {
        cost_microusd: 0,
        tokens: 0,
        tasks: 1,
        tool_calls: 0,
        retrieved_bytes: 0,
    }
}

fn terminal_failure_projection(class: ExecutionFailureClass, message: &str) -> TerminalProjection {
    TerminalProjection::Failed {
        failure: ExecutionTaskFailure {
            class,
            message: message.to_string(),
            capability_ref: None,
        },
    }
}

fn conflicting_satisfied_count(evidence: &ExecutionTerminalEvidence) -> u64 {
    if evidence.satisfied_requirement_count == 0 {
        1
    } else {
        0
    }
}

fn alternate_terminal_cause(
    cause: &ExecutionTerminalCause,
    projection: &TerminalProjection,
) -> Option<ExecutionTerminalCause> {
    match projection {
        TerminalProjection::Completed { .. } | TerminalProjection::Cancelled { .. } => None,
        TerminalProjection::Failed { .. } => {
            Some(if *cause == ExecutionTerminalCause::InternalFailure {
                ExecutionTerminalCause::SchedulerNoProgress
            } else {
                ExecutionTerminalCause::InternalFailure
            })
        }
        TerminalProjection::Partial { .. }
        | TerminalProjection::Blocked { .. }
        | TerminalProjection::Unsupported { .. } => {
            Some(if *cause == ExecutionTerminalCause::SchedulerNoProgress {
                ExecutionTerminalCause::TaskFailure {
                    class: ExecutionFailureClass::Terminal,
                }
            } else {
                ExecutionTerminalCause::SchedulerNoProgress
            })
        }
    }
}

fn alternate_replan_reason(reason: ReplanStopReason) -> ReplanStopReason {
    if reason == ReplanStopReason::DuplicatePlan {
        ReplanStopReason::NoProgress
    } else {
        ReplanStopReason::DuplicatePlan
    }
}

fn session_owner(created_by: &Option<SessionActorRef>) -> Result<UserId> {
    match created_by {
        Some(SessionActorRef::Identity { id }) => Ok(UserId::new(id.to_string())),
        other => bail!("fixture session has no identity owner: {other:?}"),
    }
}

fn generous_budget() -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: Some(100_000_000),
        max_tokens: Some(1_000_000),
        max_tasks: Some(100),
        max_tool_calls: Some(100),
        max_retrieved_bytes: Some(1_000_000),
        deadline_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
    }
}

fn output_candidate(
    objective: &str,
    max_attempts: u32,
    output: Value,
) -> GeneratedExecutionCandidate {
    let schema = answer_schema();
    GeneratedExecutionCandidate {
        goal: single_requirement_goal(objective),
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: empty_object_schema(),
            output_schema: schema.clone(),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: schema,
                operation: ExecutionOperation::Output { value: output },
                retry: RetryPolicy {
                    max_attempts,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
        run_input: json!({}),
    }
}

fn replan_candidate(objective: &str) -> GeneratedExecutionCandidate {
    let schema = answer_schema();
    GeneratedExecutionCandidate {
        goal: replan_goal(objective),
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: empty_object_schema(),
            output_schema: schema.clone(),
            nodes: vec![
                ExecutionNode {
                    id: "seed".to_string(),
                    requirement_ids: vec!["setup".to_string()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({}),
                    output_schema: schema.clone(),
                    operation: ExecutionOperation::Agent {
                        instructions: REPLAN_SEED_AGENT_SENTINEL.to_string(),
                        skill_refs: Vec::new(),
                        capability_refs: Vec::new(),
                        max_turns: 1,
                    },
                    retry: no_retry(),
                    budget: None,
                },
                ExecutionNode {
                    id: "research".to_string(),
                    requirement_ids: vec!["result".to_string()],
                    depends_on: vec!["seed".to_string()],
                    when: None,
                    input: json!({}),
                    output_schema: schema.clone(),
                    operation: ExecutionOperation::Agent {
                        instructions: REPLAN_AGENT_SENTINEL.to_string(),
                        skill_refs: Vec::new(),
                        capability_refs: Vec::new(),
                        max_turns: 1,
                    },
                    retry: no_retry(),
                    budget: None,
                },
                ExecutionNode {
                    id: "output".to_string(),
                    requirement_ids: vec!["result".to_string()],
                    depends_on: vec!["research".to_string()],
                    when: None,
                    input: json!({}),
                    output_schema: schema,
                    operation: ExecutionOperation::Output {
                        value: json!({"$ref": "$.nodes.research.output"}),
                    },
                    retry: no_retry(),
                    budget: None,
                },
            ],
        },
        run_input: json!({}),
    }
}

fn useful_amendment_candidate(base_plan_revision: u64) -> GeneratedAmendmentCandidate {
    let schema = answer_schema();
    GeneratedAmendmentCandidate {
        amendment: PlanAmendment {
            schema_version: 1,
            base_plan_revision,
            reason: "replace unsupported research with deterministic output".to_string(),
            evidence: json!({"shape": "unsupported"}),
            operations: vec![
                PlanAmendmentOperation::RemovePendingNode {
                    node_id: "research".to_string(),
                },
                PlanAmendmentOperation::AddNode {
                    node: ExecutionNode {
                        id: "research_v2".to_string(),
                        requirement_ids: vec!["result".to_string()],
                        depends_on: vec!["seed".to_string()],
                        when: None,
                        input: json!({}),
                        output_schema: schema.clone(),
                        operation: ExecutionOperation::Agent {
                            instructions: REPAIRED_AGENT_SENTINEL.to_string(),
                            skill_refs: Vec::new(),
                            capability_refs: Vec::new(),
                            max_turns: 1,
                        },
                        retry: no_retry(),
                        budget: None,
                    },
                },
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "output".to_string(),
                    node: ExecutionNode {
                        id: "output_v2".to_string(),
                        requirement_ids: vec!["result".to_string()],
                        depends_on: vec!["research_v2".to_string()],
                        when: None,
                        input: json!({}),
                        output_schema: schema,
                        operation: ExecutionOperation::Output {
                            value: json!({"$ref": "$.nodes.research_v2.output"}),
                        },
                        retry: no_retry(),
                        budget: None,
                    },
                },
            ],
        },
    }
}

fn single_requirement_goal(objective: &str) -> ExecutionGoalContract {
    ExecutionGoalContract {
        objective: objective.to_string(),
        requirements: vec![ExecutionRequirement {
            id: "result".to_string(),
            description: "produce one deterministic result".to_string(),
        }],
        deliverables: Vec::new(),
        coverage: Vec::new(),
        constraints: Vec::new(),
        completion_checks: vec![CompletionCheck {
            id: "output_schema".to_string(),
            description: "terminal output satisfies its schema".to_string(),
            requirement_ids: vec!["result".to_string()],
            constraint_ids: Vec::new(),
            kind: CompletionCheckKind::OutputSchema,
        }],
    }
}

fn replan_goal(objective: &str) -> ExecutionGoalContract {
    let mut goal = single_requirement_goal(objective);
    goal.requirements.insert(
        0,
        ExecutionRequirement {
            id: "setup".to_string(),
            description: "establish one completed dependency before replanning".to_string(),
        },
    );
    goal
}

fn answer_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["answer"],
        "properties": {"answer": {"type": "string"}}
    })
}

fn empty_object_schema() -> Value {
    json!({"type": "object", "additionalProperties": false})
}

fn no_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    }
}

fn text_completion(content: impl Into<String>) -> Value {
    json!({"content": content.into(), "tool_calls": []})
}

fn keyed_completion(match_substring: &str, completion: Value) -> Value {
    json!({"match": match_substring, "completion": completion})
}

fn assert_act_escalation_audits(audits: &[ExecutionPlanningAuditEnvelopeV1]) {
    assert_eq!(
        audits.len(),
        4,
        "Act escalation must emit two routes, planner, compile"
    );
    assert!(matches!(
        audits[0].payload,
        ExecutionPlanningAuditPayloadV1::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteDecisionKind::Routed,
            mode: Some(ExecutionMode::Act),
            reason: ExecutionRouteReason::BoundedInteractiveWork,
            ..
        }
    ));
    assert!(matches!(
        audits[1].payload,
        ExecutionPlanningAuditPayloadV1::Route {
            stage: ExecutionRouteStage::ActEscalation,
            decision: ExecutionRouteDecisionKind::Routed,
            mode: Some(ExecutionMode::Run),
            reason: ExecutionRouteReason::ActEscalation,
            ..
        }
    ));
    assert!(matches!(
        audits[2].payload,
        ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            outcome: ExecutionPlannerOutcome::Accepted,
            ..
        }
    ));
    assert!(matches!(
        audits[3].payload,
        ExecutionPlanningAuditPayloadV1::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            outcome: ExecutionCompileOutcome::Accepted,
            ..
        }
    ));
}

fn assert_initial_repair_audits(audits: &[ExecutionPlanningAuditEnvelopeV1]) {
    assert_eq!(
        audits.len(),
        5,
        "repair history must be route plus four operations"
    );
    assert!(matches!(
        audits[0].payload,
        ExecutionPlanningAuditPayloadV1::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteDecisionKind::Routed,
            mode: Some(ExecutionMode::Run),
            reason: ExecutionRouteReason::ExplicitRun,
            ..
        }
    ));
    assert!(matches!(
        audits[1].payload,
        ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            outcome: ExecutionPlannerOutcome::CompilerRejected,
            candidate_hash: Some(_),
            candidate_json: Some(_),
            compiler_report: Some(_),
            ..
        }
    ));
    assert!(matches!(
        audits[2].payload,
        ExecutionPlanningAuditPayloadV1::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            outcome: ExecutionCompileOutcome::Rejected,
            final_plan_hash: None,
            ..
        }
    ));
    assert!(matches!(
        audits[3].payload,
        ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialRepair,
            call_ordinal: 1,
            outcome: ExecutionPlannerOutcome::Accepted,
            candidate_hash: Some(_),
            candidate_json: Some(_),
            compiler_report: Some(_),
            ..
        }
    ));
    assert!(matches!(
        audits[4].payload,
        ExecutionPlanningAuditPayloadV1::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            outcome: ExecutionCompileOutcome::Accepted,
            final_plan_hash: Some(_),
            ..
        }
    ));
}

fn assert_amendment_audits(audits: &[ExecutionPlanningAuditEnvelopeV1], run_uid: uuid::Uuid) {
    assert_eq!(
        audits.len(),
        5,
        "amended run must retain route, initial planning, and amendment planning"
    );
    assert!(matches!(
        audits[0].payload,
        ExecutionPlanningAuditPayloadV1::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteDecisionKind::Routed,
            mode: Some(ExecutionMode::Run),
            reason: ExecutionRouteReason::ExplicitRun,
            ..
        }
    ));
    assert!(matches!(
        audits[1].payload,
        ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            outcome: ExecutionPlannerOutcome::Accepted,
            ..
        }
    ));
    assert!(matches!(
        audits[2].payload,
        ExecutionPlanningAuditPayloadV1::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            outcome: ExecutionCompileOutcome::Accepted,
            ..
        }
    ));
    let amendment = audits
        .iter()
        .filter(|audit| {
            matches!(
                audit.payload,
                ExecutionPlanningAuditPayloadV1::PlannerCall {
                    call_kind: ExecutionPlannerCallKind::Amendment,
                    ..
                } | ExecutionPlanningAuditPayloadV1::Compile {
                    source: ExecutionCompileSource::Amendment,
                    ..
                }
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        amendment.len(),
        2,
        "one amendment must emit planner plus compiler"
    );
    assert!(matches!(
        amendment[0].payload,
        ExecutionPlanningAuditPayloadV1::PlannerCall {
            call_kind: ExecutionPlannerCallKind::Amendment,
            call_ordinal: 0,
            run_uid: Some(actual_run_uid),
            plan_revision: Some(1),
            outcome: ExecutionPlannerOutcome::Accepted,
            candidate_hash: Some(_),
            candidate_json: Some(_),
            compiler_report: Some(_),
            ..
        } if actual_run_uid == run_uid
    ));
    assert!(matches!(
        amendment[1].payload,
        ExecutionPlanningAuditPayloadV1::Compile {
            source: ExecutionCompileSource::Amendment,
            run_uid: Some(actual_run_uid),
            plan_revision: Some(1),
            outcome: ExecutionCompileOutcome::Accepted,
            final_plan_hash: Some(_),
            ..
        } if actual_run_uid == run_uid
    ));
}
