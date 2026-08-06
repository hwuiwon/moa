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
use moa_config::ExecutionConfig;
use moa_core::{
    events::Event,
    types::{
        contact::SessionActorRef,
        execution_planning::{
            ExecutionCompileOutcome, ExecutionCompileSource, ExecutionPlannerCallKind,
            ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload,
            ExecutionRouteKind, ExecutionRouteStage, ExecutionStrategy,
        },
        identifiers::{SessionId, TenantId, UserId},
        session::SessionStatus,
    },
};
use moa_execution::{
    capability::{ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog, ExecutionEstimate},
    compiler::{CompileExecutionRequest, CompiledExecution, compile},
    completion::{
        CompletionEvaluation, CompletionStatus, execution_terminal_reason,
        terminal_evidence_from_evaluation,
    },
    replan::ReplanStopReason,
    repository::{
        ExecutionRepository, ExecutionRunRecord, ExecutionScope, FencedTerminalFinalizationOutcome,
        FinalizationOutcome, NewExecutionRun, ReservationOutcome, RunFinalizationRequest,
        TaskOutcomeWrite, TerminalFenceCommit, TerminalFenceOutcome, TransitionOutcome,
    },
    state::{
        ExecutionLimitStop, ExecutionRunStatus, ExecutionSourceKind, ExecutionTaskFailure,
        ExecutionTaskId, ExecutionTerminalCause, ExecutionTerminalEvidence,
        ExecutionTerminalReason, LogicalTask, LogicalTaskKind, PendingExecutionTerminal,
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
use moa_wire::turn::{TurnOutcomeKind, TurnOutcomeKind::Accepted};
use serde_json::{Value, json};

use crate::execution_execution_support::assertions::{
    assert_completed_terminal, assert_no_execution_lifecycle_events, journal_requests,
    planning_audits,
};
use crate::execution_execution_support::fixtures::{
    POLL_INTERVAL, RouteFixture, SERVICE_TIMEOUT, await_execution_terminal, await_session_settled,
    await_turn_outcome, execution_run_request, raw_events, route_classifier_completion,
    route_classifier_needs_input_completion, seed_allow_policy, start_turn, start_turn_in_session,
};

const PLANNER_MATCH: &str = "<frozen_planning_context>";
const AMENDMENT_MATCH: &str = "<frozen_amendment_context>";
const SYNTHESIS_MATCH: &str = "Synthesize the final user response for execution run";
const ESCALATION_OBJECTIVE: &str = "Investigate the unusual failure and explain it";
/// Name the loopback capability fixture publishes the escalation tool under.
const ESCALATION_TOOL: &str = "discover_fixture_work_scope";

/// Server-qualified reference the model calls and the `ToolCall` event records.
fn escalation_tool_reference() -> String {
    moa_hands::mcp_tool_reference("fixture-capability", ESCALATION_TOOL)
}
const DURABLE_UPGRADE_CONTROL: &str = "request_durable_execution";
const DURABLE_UPGRADE_CONTROL_BARRIER_MS: u64 = 15_000;
const REPLAN_SEED_AGENT_SENTINEL: &str = "TERMINAL_MATRIX_REPLAN_SEED_AGENT";
const REPLAN_AGENT_SENTINEL: &str = "TERMINAL_MATRIX_REPLAN_AGENT";
const REPAIRED_AGENT_SENTINEL: &str = "TERMINAL_MATRIX_REPAIRED_AGENT";
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
                RouteFixture::NeedsInput,
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
        &audits[0].payload,
        ExecutionPlanningAuditPayload::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteKind::NeedsInput,
            strategy: None,
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
async fn execute_inline_upgrades_once_to_durable_with_preserved_evidence_service_e2e() -> Result<()>
{
    // Pins: an Inline probe can preserve its evidence through the workflow-owned control,
    // record one one-way Durable upgrade, and admit exactly one generated run without
    // trusting arbitrary tool output as control data or reclassifying the objective.
    let candidate = output_candidate(ESCALATION_OBJECTIVE, 1, json!({"answer": "escalated"}));
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionRouteKind::Execute,
                    RouteFixture::Inline
                ),
                keyed_completion(SYNTHESIS_MATCH, text_completion("escalated run complete")),
                keyed_completion(
                    PLANNER_MATCH,
                    text_completion(serde_json::to_string(&candidate)?)
                ),
                keyed_completion(
                    "company_count",
                    json!({
                        "content": "",
                        "latency_ms": DURABLE_UPGRADE_CONTROL_BARRIER_MS,
                        "ttft_ms": DURABLE_UPGRADE_CONTROL_BARRIER_MS,
                        "tool_calls": [{
                            "name": DURABLE_UPGRADE_CONTROL,
                            "id": "terminal-matrix-durable-upgrade-control",
                            "input": {
                                "rationale": "Newly discovered work requires durable continuation.",
                                "evidence": [{
                                    "source": format!("tool:{ESCALATION_TOOL}"),
                                    "summary": "the bounded probe discovered collection-wide work",
                                    "value": {"company_count": 500}
                                }]
                            }
                        }]
                    })
                ),
                keyed_completion(
                    ESCALATION_OBJECTIVE,
                    json!({
                        "content": "",
                        "tool_calls": [{
                            "name": escalation_tool_reference(),
                            "id": "terminal-matrix-durable-upgrade",
                            "input": {"query": "unusual failure"}
                        }]
                    })
                )
            ]
        }),
        FixtureCapabilityOptions {
            tools: vec![
                FixtureCapabilityTool {
                    name: ESCALATION_TOOL.to_string(),
                    description: "Discover a deterministic durable execution shape".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["query"],
                        "properties": {"query": {"type": "string"}}
                    }),
                    item_key_pointer: None,
                    idempotent: true,
                    outcomes: vec![FixtureCapabilityOutcome::Success {
                        output: json!({"company_count": 500}),
                    }],
                },
                FixtureCapabilityTool {
                    name: DURABLE_UPGRADE_CONTROL.to_string(),
                    description: "Untrusted fixture schema that must never reach the model"
                        .to_string(),
                    input_schema: json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["untrusted_override"],
                        "properties": {"untrusted_override": {"type": "boolean"}}
                    }),
                    item_key_pointer: None,
                    idempotent: true,
                    outcomes: vec![FixtureCapabilityOutcome::Success {
                        output: json!({"unexpected": true}),
                    }],
                },
            ],
            orchestrator_env: Vec::new(),
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("strict-durable-upgrade").await?;
    let session = test.client().get_session(session_id).await?;
    seed_allow_policy(
        &fixture,
        test.client(),
        session.tenant_id,
        &escalation_tool_reference(),
    )
    .await?;
    let started = start_turn_in_session(&test, session_id, ESCALATION_OBJECTIVE, None).await?;
    let controller = fixture
        .fixture_capability()
        .context("execution fixture omitted its capability controller")?;
    let calls = controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, ESCALATION_TOOL);
    controller.release(1);
    await_successful_probe_result(test.client(), started.session_id).await?;
    let first_control_attempt = await_scripted_control_request_attempts(&fixture, 1).await?;
    assert_eq!(
        first_control_attempt.len(),
        1,
        "the first process must block exactly one control-response attempt"
    );
    let pre_restart_audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_eq!(
        pre_restart_audits.len(),
        1,
        "control response escaped its provider barrier before restart: {pre_restart_audits:#?}"
    );
    assert!(matches!(
        pre_restart_audits[0].payload,
        ExecutionPlanningAuditPayload::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteKind::Execute,
            strategy: Some(ExecutionStrategy::Inline),
            ..
        }
    ));
    let pre_restart_events = raw_events(test.client(), started.session_id).await?;
    assert!(
        pre_restart_events
            .iter()
            .all(|record| !matches!(record.event, Event::ExecutionRunStarted(_))),
        "control response admitted Durable execution before restart: {pre_restart_events:#?}"
    );
    fixture
        .restart_orchestrator()
        .await
        .context("restart while the control response remains blocked before Durable admission")?;
    let replayed_control_attempts = await_scripted_control_request_attempts(&fixture, 2).await?;
    assert_eq!(
        replayed_control_attempts[0], replayed_control_attempts[1],
        "replayed control request changed across orchestrator restart"
    );

    let outcome = await_turn_outcome(test.client(), &started).await?;
    let Accepted {
        execution_run_uid: outcome_run_uid,
    } = outcome.kind
    else {
        bail!("Inline Durable-upgrade turn did not reach Accepted: {outcome:?}");
    };
    let execution_run_uid =
        await_single_execution_run_started_uid(test.client(), started.session_id, &started.turn_id)
            .await?;
    assert_eq!(
        outcome_run_uid, execution_run_uid,
        "public Session outcome and persisted run-start event disagree"
    );
    let status = await_execution_terminal(
        test.client(),
        &execution_run_request(&started, execution_run_uid),
    )
    .await?;
    assert_completed_terminal(&status, 1, 1);
    assert_eq!(status.run.source_kind, ExecutionSourceKind::GeneratedPlan);
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Idle
    );

    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_durable_upgrade_audits(&audits);
    assert_eq!(
        planning_audits(&fixture.postgres_url, started.session_id).await?,
        audits
    );
    let events = raw_events(test.client(), started.session_id).await?;
    assert_eq!(
        events
            .iter()
            .filter_map(|record| match &record.event {
                Event::ExecutionRunStarted(started) => Some(started.run_uid),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![execution_run_uid],
        "one Inline turn must admit exactly one Durable run"
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::ExecutionCompleted(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::ExecutionSynthesisRequested(_)))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::ToolCall { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|record| matches!(record.event, Event::ToolResult { success: true, .. }))
            .count(),
        1
    );
    assert_eq!(
        crate::execution_execution_support::assertions::final_brain_response(&events)?,
        "escalated run complete"
    );
    let requests = journal_requests(fixture.scripted_requests()?)?;
    assert_eq!(
        crate::execution_execution_support::assertions::journal_roles(&requests),
        vec![
            crate::execution_execution_support::assertions::JournalRequestRole::Normal,
            crate::execution_execution_support::assertions::JournalRequestRole::Normal,
            crate::execution_execution_support::assertions::JournalRequestRole::Normal,
            crate::execution_execution_support::assertions::JournalRequestRole::Normal,
            crate::execution_execution_support::assertions::JournalRequestRole::InitialPlanner,
            crate::execution_execution_support::assertions::JournalRequestRole::Synthesis,
        ]
    );
    let roles = crate::execution_execution_support::assertions::journal_roles(&requests);
    let inline_requests = requests
        .iter()
        .zip(&roles)
        .filter(|(request, role)| {
            **role == crate::execution_execution_support::assertions::JournalRequestRole::Normal
                && request.response_format.is_none()
        })
        .map(|(request, _)| request)
        .collect::<Vec<_>>();
    assert_eq!(
        inline_requests.len(),
        3,
        "the eligible root Inline loop should journal its initial request plus byte-identical original/replayed control attempts"
    );
    let authoritative_control_schema = authoritative_durable_upgrade_control_schema();
    for request in inline_requests {
        let control_schemas = durable_upgrade_control_schemas(request);
        assert_eq!(
            control_schemas.len(),
            1,
            "eligible root request did not expose exactly one Durable-upgrade control: {control_schemas:#?}"
        );
        assert_eq!(
            control_schemas[0], &authoritative_control_schema,
            "eligible root request exposed a conflicting Durable-upgrade control schema"
        );
    }
    let synthesis_request = requests
        .iter()
        .zip(&roles)
        .find_map(|(request, role)| {
            (*role == crate::execution_execution_support::assertions::JournalRequestRole::Synthesis)
                .then_some(request)
        })
        .context("Durable run omitted its terminal synthesis request")?;
    assert!(
        durable_upgrade_control_schemas(synthesis_request).is_empty(),
        "ineligible synthesis request exposed the root-only Durable-upgrade control"
    );
    assert_eq!(
        controller.calls().len(),
        1,
        "workflow-owned Durable-upgrade control must not dispatch through the conflicting MCP capability"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request
                    .response_format
                    .as_ref()
                    .is_some_and(|format| format.name == "execution_route_classifier")
            })
            .count(),
        1,
        "Durable upgrade must not reclassify the root objective"
    );
    // The execution planner embeds the candidate schema in-prompt as
    // `<response_schema>…</response_schema>` and sends no provider-native
    // strict response format (planner candidates carry free-form JSON that
    // strict schemas cannot represent), so the planner request is identified
    // by that marker rather than a `response_format` name.
    let planner = requests
        .iter()
        .find(|request| {
            request.response_format.is_none()
                && request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("<response_schema>"))
        })
        .context("Durable upgrade omitted its sole generated planner request")?;
    let planner_context = planner
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let frozen = planner_context
        .split_once("<frozen_planning_context>")
        .and_then(|(_, suffix)| suffix.split_once("</frozen_planning_context>"))
        .map(|(json, _)| json)
        .context("Durable upgrade planner omitted frozen planning context")?;
    let frozen: Value = serde_json::from_str(frozen)?;
    assert_eq!(
        frozen.pointer("/durable_upgrade/objective"),
        Some(&json!(ESCALATION_OBJECTIVE))
    );
    assert_eq!(
        frozen.pointer("/durable_upgrade/rationale"),
        Some(&json!(
            "Newly discovered work requires durable continuation."
        ))
    );
    assert_eq!(
        frozen.pointer("/durable_upgrade/evidence"),
        Some(&json!([{
            "source": format!("tool:{ESCALATION_TOOL}"),
            "summary": "the bounded probe discovered collection-wide work",
            "value": {"company_count": 500}
        }]))
    );
    Ok(())
}

async fn await_successful_probe_result(
    client: &TestApiClient,
    session_id: SessionId,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + SERVICE_TIMEOUT;
    loop {
        let events = raw_events(client, session_id).await?;
        let probe_tool_id = events.iter().find_map(|record| match &record.event {
            Event::ToolCall {
                tool_id, tool_name, ..
            } if tool_name == &escalation_tool_reference() => Some(tool_id),
            _ => None,
        });
        if let Some(probe_tool_id) = probe_tool_id {
            if events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::ToolError { tool_id, .. } if tool_id == probe_tool_id
                )
            }) {
                bail!("Inline probe failed before the restart checkpoint: {events:#?}");
            }
            if events.iter().any(|record| {
                matches!(
                    &record.event,
                    Event::ToolResult {
                        tool_id,
                        success: true,
                        ..
                    } if tool_id == probe_tool_id
                )
            }) {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "Inline probe ToolResult was not durably persisted within {SERVICE_TIMEOUT:?}; events: {events:#?}"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn await_scripted_control_request_attempts(
    fixture: &OrchestratorTestFixture,
    expected_count: usize,
) -> Result<Vec<moa_core::types::completion::CompletionRequest>> {
    let deadline = tokio::time::Instant::now() + SERVICE_TIMEOUT;
    loop {
        let requests = journal_requests(fixture.scripted_requests()?)?;
        let control_attempts = requests
            .into_iter()
            .filter(|request| {
                request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("company_count"))
            })
            .collect::<Vec<_>>();
        if control_attempts.len() >= expected_count {
            return Ok(control_attempts);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "scripted provider journal recorded {} of {expected_count} blocked control-response attempts within {SERVICE_TIMEOUT:?}",
                control_attempts.len()
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn await_single_execution_run_started_uid(
    client: &TestApiClient,
    session_id: SessionId,
    root_turn_id: &str,
) -> Result<uuid::Uuid> {
    let deadline = tokio::time::Instant::now() + SERVICE_TIMEOUT;
    loop {
        let events = raw_events(client, session_id).await?;
        let run_uids = events
            .iter()
            .filter_map(|record| match &record.event {
                Event::ExecutionRunStarted(started) => Some(started.run_uid),
                _ => None,
            })
            .collect::<Vec<_>>();
        match run_uids.as_slice() {
            [run_uid] => return Ok(*run_uid),
            [] => {}
            _ => {
                bail!(
                    "root turn {root_turn_id} admitted multiple execution runs before its terminal observation: {run_uids:?}"
                );
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "root turn {root_turn_id} did not durably publish ExecutionRunStarted within {SERVICE_TIMEOUT:?}; events: {events:#?}"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Returns every offered schema claiming the durable-upgrade control's name.
///
/// The fixture publishes a decoy connector tool under this same name, and the
/// assertion is that only MOA's authoritative control ever reaches the model.
/// Server-qualified connector references now make that shadowing *structurally*
/// impossible — the decoy registers as `mcp__fixture-capability__…`, so it can
/// no longer contest the bare name at all. The check is kept because it pins the
/// stronger guarantee at the layer the model actually sees, and would still
/// catch a regression that reintroduced unqualified connector registration.
fn durable_upgrade_control_schemas(
    request: &moa_core::types::completion::CompletionRequest,
) -> Vec<&Value> {
    request
        .tools
        .iter()
        .filter(|schema| {
            schema.get("name").and_then(Value::as_str) == Some(DURABLE_UPGRADE_CONTROL)
        })
        .collect()
}

fn authoritative_durable_upgrade_control_schema() -> Value {
    json!({
        "name": DURABLE_UPGRADE_CONTROL,
        "description": "Request the one-way transition from bounded Inline work to a durable execution plan after the current turn has discovered concrete evidence that the remaining work needs durability, resumability, approval or signal handling, or broad fan-out. Call this control by itself.",
        "input_schema": {
            "type": "object",
            "additionalProperties": false,
            "required": ["rationale", "evidence"],
            "properties": {
                "rationale": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 240,
                    "description": "One short sentence explaining why the discovered work now needs Durable execution."
                },
                "evidence": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 32,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["source", "summary", "value"],
                        "properties": {
                            "source": {"type": "string", "description": "Stable label for the already-observed source."},
                            "summary": {"type": "string", "description": "Concise summary of the observed fact."},
                            "value": {"description": "Structured evidence already gathered during this Inline turn."}
                        }
                    }
                }
            }
        }
    })
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
                    ExecutionRouteKind::Execute,
                    RouteFixture::Durable
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
        SessionStatus::Idle
    );

    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_repair_audits(&audits);
    assert_eq!(
        planning_audits(&fixture.postgres_url, started.session_id).await?,
        audits
    );

    let requests = journal_requests(fixture.scripted_requests()?)?;
    // Planner schemas are prompt guidance because candidate payloads contain free-form JSON;
    // identify both strict calls by their frozen input marker, not `response_format`.
    let strict_initial_calls = requests
        .iter()
        .filter(|request| {
            request
                .messages
                .iter()
                .any(|message| message.content.contains(PLANNER_MATCH))
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
                ExecutionRouteKind::Execute,
                RouteFixture::Durable
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
        &audits[0].payload,
        ExecutionPlanningAuditPayload::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteKind::Execute,
            strategy: Some(ExecutionStrategy::Durable),
            ..
        }
    ));
    assert!(matches!(
        audits[1].payload,
        ExecutionPlanningAuditPayload::PlannerCall {
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
    assert!(
        audits
            .iter()
            .all(|audit| !matches!(audit.payload, ExecutionPlanningAuditPayload::Compile { .. }))
    );
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
                    ExecutionRouteKind::Execute,
                    RouteFixture::Durable
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
        SessionStatus::Idle
    );

    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_amendment_audits(&audits, execution_run_uid);
    assert_eq!(
        planning_audits(&fixture.postgres_url, started.session_id).await?,
        audits
    );

    let requests = journal_requests(fixture.scripted_requests()?)?;
    // Initial and amendment schemas live in their prompts rather than provider-native response
    // formats, so retain the exact planning-call order using their frozen input markers.
    let planning_calls = requests
        .iter()
        .filter_map(|request| {
            if request
                .response_format
                .as_ref()
                .is_some_and(|format| format.name == "execution_route_classifier")
            {
                Some("route")
            } else if request
                .messages
                .iter()
                .any(|message| message.content.contains(AMENDMENT_MATCH))
            {
                Some("amendment")
            } else if request
                .messages
                .iter()
                .any(|message| message.content.contains(PLANNER_MATCH))
            {
                Some("initial")
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(planning_calls, vec!["route", "initial", "amendment"]);
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
            | ExecutionTerminalCause::InternalFailure
            | ExecutionTerminalCause::CompensationFailure { .. } => None,
        },
        checks: Vec::new(),
        satisfied_requirement_ids: case.satisfied.clone(),
        unsatisfied_requirement_ids: case.unsatisfied.clone(),
        gaps: case.gaps.clone(),
    };
    let evidence = terminal_evidence_from_evaluation(case.cause.clone(), &evaluation)?;
    let terminal_reason = execution_terminal_reason(&case.cause, &case.projection, &evaluation)?;
    if case.status != ExecutionRunStatus::Completed {
        let pending_terminal = PendingExecutionTerminal {
            status: case.status,
            reason: terminal_reason,
            terminal_evidence: evidence.clone(),
            output: case.output.clone(),
            completion_check_results: evaluation
                .checks
                .iter()
                .map(serde_json::to_value)
                .collect::<std::result::Result<Vec<_>, _>>()?,
            terminal_gaps: case.gaps.clone(),
            cancellation_reason: None,
        };
        let first = repository
            .fence_run_for_terminal(
                scope,
                run.run_uid,
                run.plan_revision,
                run.wake_epoch,
                pending_terminal.clone(),
            )
            .await?;
        let TerminalFenceOutcome::Applied(first_commit) = first else {
            bail!(
                "{} did not enter the compensation fence: {first:?}",
                case.label
            );
        };
        let replay = repository
            .fence_run_for_terminal(
                scope,
                run.run_uid,
                run.plan_revision,
                run.wake_epoch,
                pending_terminal.clone(),
            )
            .await?;
        let TerminalFenceOutcome::Replayed(replayed_commit) = replay else {
            bail!(
                "{} did not replay its terminal fence: {replay:?}",
                case.label
            );
        };
        assert_eq!(replayed_commit, first_commit);
        let mut conflict = pending_terminal.clone();
        conflict.terminal_evidence.satisfied_requirement_count =
            conflicting_satisfied_count(&evidence);
        assert_eq!(
            repository
                .fence_run_for_terminal(
                    scope,
                    run.run_uid,
                    run.plan_revision,
                    run.wake_epoch,
                    conflict,
                )
                .await?,
            TerminalFenceOutcome::Conflict,
            "{} accepted conflicting terminal-fence evidence",
            case.label
        );
        assert_pending_terminal_projection(
            repository,
            scope,
            run.run_uid,
            run.status,
            &pending_terminal,
        )
        .await?;
        settle_fenced_terminal(repository, scope, &first_commit).await?;
        return assert_status_projection(
            client,
            tenant_id,
            session_id,
            run.run_uid,
            case.status,
            case.output,
            case.gaps,
            evidence,
        )
        .await;
    }
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
    let pending_terminal = PendingExecutionTerminal {
        status: ExecutionRunStatus::Partial,
        reason: terminal_reason,
        terminal_evidence: evidence.clone(),
        output: Some(json!({"useful": true})),
        completion_check_results: Vec::new(),
        terminal_gaps: evaluation.gaps,
        cancellation_reason: None,
    };
    let first = repository
        .fence_run_for_terminal(
            scope,
            run.run_uid,
            1,
            waiting_run.wake_epoch,
            pending_terminal.clone(),
        )
        .await?;
    let TerminalFenceOutcome::Applied(first_commit) = first else {
        bail!("{label} did not enter the compensation fence: {first:?}");
    };
    let replay = repository
        .fence_run_for_terminal(
            scope,
            run.run_uid,
            1,
            waiting_run.wake_epoch,
            pending_terminal.clone(),
        )
        .await?;
    let TerminalFenceOutcome::Replayed(replayed_commit) = replay else {
        bail!("{label} did not replay through the compensation fence: {replay:?}");
    };
    assert_eq!(
        replayed_commit, first_commit,
        "{label} replay changed persisted bytes"
    );

    let mut count_conflict = pending_terminal.clone();
    count_conflict.terminal_evidence.satisfied_requirement_count = 0;
    assert_eq!(
        repository
            .fence_run_for_terminal(
                scope,
                run.run_uid,
                1,
                waiting_run.wake_epoch,
                count_conflict,
            )
            .await?,
        TerminalFenceOutcome::Conflict,
        "{label} accepted conflicting requirement counts"
    );
    let mut cause_conflict = pending_terminal.clone();
    cause_conflict.terminal_evidence.cause = ExecutionTerminalCause::ReplanStop {
        reason: alternate_replan_reason(reason),
    };
    assert_eq!(
        repository
            .fence_run_for_terminal(
                scope,
                run.run_uid,
                1,
                waiting_run.wake_epoch,
                cause_conflict,
            )
            .await?,
        TerminalFenceOutcome::Conflict,
        "{label} accepted a conflicting replan cause"
    );

    assert_pending_terminal_projection(
        repository,
        scope,
        run.run_uid,
        waiting_run.status,
        &pending_terminal,
    )
    .await?;
    settle_fenced_terminal(repository, scope, &first_commit).await?;
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
    assert_eq!(first_run.status, ExecutionRunStatus::Running);
    assert!(first_run.terminal_evidence.is_none());

    let replay: ExecutionMutationResponse = client.post_call("/Execution/cancel", &request).await?;
    assert_eq!(
        replay,
        ExecutionMutationResponse::Replayed {
            run: first_run.clone()
        }
    );
    let pending_terminal = PendingExecutionTerminal {
        status: ExecutionRunStatus::Cancelled,
        reason: ExecutionTerminalReason::Cancelled,
        terminal_evidence: evidence.clone(),
        output: None,
        completion_check_results: Vec::new(),
        terminal_gaps: Vec::new(),
        cancellation_reason: Some(reason.clone()),
    };
    assert_pending_terminal_projection(
        repository,
        scope,
        run.run_uid,
        ExecutionRunStatus::Running,
        &pending_terminal,
    )
    .await?;
    let persisted = repository
        .load_run(scope, run.run_uid)
        .await?
        .context("cancellation-fenced run disappeared")?;
    let finalized = settle_fenced_terminal(
        repository,
        scope,
        &TerminalFenceCommit {
            run: persisted,
            tasks_to_settle: Vec::new(),
        },
    )
    .await?;
    assert_eq!(finalized.cancellation_reason, Some(reason));
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
    let status = await_execution_terminal(
        client,
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

async fn assert_pending_terminal_projection(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    run_uid: uuid::Uuid,
    expected_status: ExecutionRunStatus,
    expected_pending: &PendingExecutionTerminal,
) -> Result<()> {
    let run = repository.load_run(scope, run_uid).await?;
    let run = run.context("fenced run disappeared before its pending projection was asserted")?;
    assert_eq!(run.status, expected_status);
    assert_eq!(run.pending_terminal.as_ref(), Some(expected_pending));
    assert!(run.terminal_evidence.is_none());
    Ok(())
}

async fn settle_fenced_terminal(
    repository: &ExecutionRepository,
    scope: ExecutionScope,
    fence: &TerminalFenceCommit,
) -> Result<ExecutionRunRecord> {
    for task in &fence.tasks_to_settle {
        let outcome = ExecutionTaskOutcome {
            schema_version: 1,
            usage: task.actual.clone(),
            result: ExecutionTaskResult::Cancelled {
                reason: "terminal-matrix forward settlement".to_string(),
            },
        };
        assert!(matches!(
            repository
                .record_task_outcome(
                    scope,
                    fence.run.run_uid,
                    task.task_id,
                    task.generation,
                    outcome,
                )
                .await?,
            TaskOutcomeWrite::Applied { .. } | TaskOutcomeWrite::Replayed { .. }
        ));
    }
    let settled = repository
        .load_run(scope, fence.run.run_uid)
        .await?
        .context("fenced run disappeared before terminal settlement")?;
    let finalized = repository
        .finalize_fenced_terminal(
            scope,
            settled.run_uid,
            settled.plan_revision,
            settled.wake_epoch,
        )
        .await?;
    let FencedTerminalFinalizationOutcome::Finalized(finalized) = finalized else {
        bail!("fenced terminal did not finalize after forward settlement: {finalized:?}");
    };
    let replay = repository
        .finalize_fenced_terminal(
            scope,
            finalized.run_uid,
            finalized.plan_revision,
            finalized.wake_epoch,
        )
        .await?;
    assert_eq!(
        replay,
        FencedTerminalFinalizationOutcome::Replayed(finalized.clone()),
        "terminal-fence finalization did not replay exactly"
    );
    Ok(finalized)
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
            completion_checks: vec![CompletionCheck {
                id: "terminal_output_schema".to_string(),
                description: "terminal output satisfies its declared schema".to_string(),
                requirement_ids: vec![REQ_USEFUL.to_string(), REQ_REMAINING.to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
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
                compensation: None,
                retry: no_retry(),
                budget: None,
            }],
        },
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: budget.clone(),
        config: ExecutionConfig::default(),
        now: moa_test_support::fixtures::pg_now(),
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
        compensation: None,
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
        compensation: None,
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
        deadline_at: Some(moa_test_support::fixtures::pg_now() + chrono::Duration::hours(1)),
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
            cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
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
                compensation: None,
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
            cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
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
                    compensation: None,
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
                    compensation: None,
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
                    compensation: None,
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
            base_plan_revision,
            reason: "replace unsupported research with deterministic output".to_string(),
            evidence: json!({"shape": "unsupported"}),
            operations: vec![
                PlanAmendmentOperation::RemovePendingNode {
                    node_id: "research".to_string(),
                },
                PlanAmendmentOperation::AddNode {
                    node: ExecutionNode {
                        id: "replacement_research".to_string(),
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
                        compensation: None,
                        retry: no_retry(),
                        budget: None,
                    },
                },
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "output".to_string(),
                    node: ExecutionNode {
                        id: "replacement_output".to_string(),
                        requirement_ids: vec!["result".to_string()],
                        depends_on: vec!["replacement_research".to_string()],
                        when: None,
                        input: json!({}),
                        output_schema: schema,
                        operation: ExecutionOperation::Output {
                            value: json!({"$ref": "$.nodes.replacement_research.output"}),
                        },
                        compensation: None,
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
    goal.completion_checks.push(CompletionCheck {
        id: "setup_seed".to_string(),
        description: "the seed dependency completed before replanning".to_string(),
        requirement_ids: vec!["setup".to_string()],
        constraint_ids: Vec::new(),
        kind: CompletionCheckKind::RequiredNodes {
            node_ids: vec!["seed".to_string()],
        },
    });
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

fn assert_durable_upgrade_audits(audits: &[ExecutionPlanningAuditEnvelope]) {
    assert_eq!(
        audits.len(),
        4,
        "Inline Durable upgrade must emit two routes, planner, compile"
    );
    assert!(matches!(
        &audits[0].payload,
        ExecutionPlanningAuditPayload::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteKind::Execute,
            strategy: Some(ExecutionStrategy::Inline),
            ..
        }
    ));
    assert!(matches!(
        &audits[1].payload,
        ExecutionPlanningAuditPayload::Route {
            stage: ExecutionRouteStage::DurableUpgrade,
            decision: ExecutionRouteKind::Execute,
            strategy: Some(ExecutionStrategy::Durable),
            ..
        }
    ));
    assert!(matches!(
        audits[2].payload,
        ExecutionPlanningAuditPayload::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            outcome: ExecutionPlannerOutcome::Accepted,
            ..
        }
    ));
    assert!(matches!(
        audits[3].payload,
        ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            outcome: ExecutionCompileOutcome::Accepted,
            ..
        }
    ));
}

fn assert_initial_repair_audits(audits: &[ExecutionPlanningAuditEnvelope]) {
    assert_eq!(
        audits.len(),
        5,
        "repair history must be route plus four operations"
    );
    assert!(matches!(
        &audits[0].payload,
        ExecutionPlanningAuditPayload::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteKind::Execute,
            strategy: Some(ExecutionStrategy::Durable),
            ..
        }
    ));
    assert!(matches!(
        audits[1].payload,
        ExecutionPlanningAuditPayload::PlannerCall {
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
        ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            outcome: ExecutionCompileOutcome::Rejected,
            final_plan_hash: None,
            ..
        }
    ));
    assert!(matches!(
        audits[3].payload,
        ExecutionPlanningAuditPayload::PlannerCall {
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
        ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::GeneratedPlan,
            outcome: ExecutionCompileOutcome::Accepted,
            final_plan_hash: Some(_),
            ..
        }
    ));
}

fn assert_amendment_audits(audits: &[ExecutionPlanningAuditEnvelope], run_uid: uuid::Uuid) {
    assert_eq!(
        audits.len(),
        5,
        "amended run must retain route, initial planning, and amendment planning"
    );
    assert!(matches!(
        &audits[0].payload,
        ExecutionPlanningAuditPayload::Route {
            stage: ExecutionRouteStage::Initial,
            decision: ExecutionRouteKind::Execute,
            strategy: Some(ExecutionStrategy::Durable),
            ..
        }
    ));
    assert!(matches!(
        audits[1].payload,
        ExecutionPlanningAuditPayload::PlannerCall {
            call_kind: ExecutionPlannerCallKind::InitialPlan,
            call_ordinal: 0,
            outcome: ExecutionPlannerOutcome::Accepted,
            ..
        }
    ));
    assert!(matches!(
        audits[2].payload,
        ExecutionPlanningAuditPayload::Compile {
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
                ExecutionPlanningAuditPayload::PlannerCall {
                    call_kind: ExecutionPlannerCallKind::Amendment,
                    ..
                } | ExecutionPlanningAuditPayload::Compile {
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
        ExecutionPlanningAuditPayload::PlannerCall {
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
        ExecutionPlanningAuditPayload::Compile {
            source: ExecutionCompileSource::Amendment,
            run_uid: Some(actual_run_uid),
            plan_revision: Some(1),
            outcome: ExecutionCompileOutcome::Accepted,
            final_plan_hash: Some(_),
            ..
        } if actual_run_uid == run_uid
    ));
}
