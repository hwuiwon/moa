//! Strict routing, planning-audit, and bounded terminal service coverage.
//!
//! Natural routing, planner, amendment, and status reads use the production service
//! boundaries. Experiment-template and skill-regression compile producers remain owned by
//! their dedicated binaries; this module does not duplicate those full workflows.

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionRequirement, ExecutionTaskResult,
    GeneratedAmendmentCandidate, GeneratedExecutionCandidate, PlanAmendment,
    PlanAmendmentOperation, RetryPolicy,
};
use moa_core::{
    events::Event,
    types::{
        execution_planning::{
            ExecutionCompileOutcome, ExecutionCompileSource, ExecutionPlannerCallKind,
            ExecutionPlannerOutcome, ExecutionPlanningAuditEnvelope, ExecutionPlanningAuditPayload,
            ExecutionRouteKind, ExecutionRouteStage, ExecutionStrategy,
        },
        identifiers::SessionId,
        session::SessionStatus,
    },
};
use moa_execution::{
    state::{ExecutionRunStatus, ExecutionSourceKind},
    wire::ExecutionStatusResponse,
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
            input_wait_policy: moa_artifacts::execution_plan::ExecutionWaitPolicy {
                expiry: moa_artifacts::execution_plan::ExecutionTemporalTarget::After {
                    delay_seconds: 86_400,
                },
                on_expiry: moa_artifacts::execution_plan::ExecutionWaitExpiryAction::FailRun,
            },
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
            input_wait_policy: moa_artifacts::execution_plan::ExecutionWaitPolicy {
                expiry: moa_artifacts::execution_plan::ExecutionTemporalTarget::After {
                    delay_seconds: 86_400,
                },
                on_expiry: moa_artifacts::execution_plan::ExecutionWaitExpiryAction::FailRun,
            },
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
