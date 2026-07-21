//! Deterministic service coverage for Respond, Execute, template, and generated-plan routing.
//!
//! This module pins instruction-only skill activation independently in the bounded Inline loop
//! and inside a Durable Agent task so skill shape cannot become a hidden routing contract.

use anyhow::{Context, Result};
use moa_artifacts::document::ArtifactKind;
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionGoalContract, ExecutionGoalTemplate,
    ExecutionNode, ExecutionOperation, ExecutionPlanDefinition, ExecutionPlanTemplate,
    ExecutionRequirement, GeneratedExecutionCandidate, RetryPolicy,
};
use moa_artifacts::reference::ArtifactRef;
use moa_core::events::{
    Event, ExecutionRunEvidenceRef, ExecutionSynthesisRequested, ExecutionTaskResultsRef,
    ExecutionTerminalSummary,
};
use moa_core::traits::{Identity, IdentityType};
use moa_core::types::execution_planning::{
    ExecutionRouteKind, ExecutionRunAdmissionStatus, ExecutionSourceProvenance, ExecutionStrategy,
    ExecutionTemplateInvocation, PinnedExecutionTemplateRef,
};
use moa_core::types::session::SessionStatus;
use moa_eval::execution::ExecutionInvariantSpec;
use moa_execution::{
    repository::{ExecutionRepository, ExecutionScope},
    state::{ExecutionRunStatus, ExecutionTaskStatus},
};
use moa_test_support::{
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool,
    OrchestratorTestFixture,
};
use moa_wire::turn::{RunTurnRequest, TurnOutcome, TurnOutcomeKind, TurnTrigger};
use serde_json::{Value, json};

use crate::evaluation::{assert_execution_eval_case, assert_non_durable_eval};
use crate::execution_execution_support::assertions::{
    JournalRequestRole, assert_completed_terminal, assert_generated_plan_audits,
    assert_initial_route, assert_no_execution_lifecycle_events, assert_no_planner_or_compile,
    assert_skill_template_audits, assert_strict_event_order, event_count, final_brain_response,
    journal_requests, journal_roles, planning_audits, sole_event_sequence,
};
use crate::execution_execution_support::fixtures::{
    RouteFixture, SERVICE_TIMEOUT, await_active_execution_progress, await_execution_terminal,
    await_run_started_event, await_session_settled, await_turn_outcome, execution_run_request,
    list_execution_tasks, publish_skill, raw_events, route_classifier_completion,
    seed_allow_policy, start_turn, start_turn_in_session,
};

const RESPOND_OBJECTIVE: &str = "What is a DAG?";
const RESPOND_FINAL: &str = "A DAG is a directed acyclic graph.";
const INLINE_OBJECTIVE: &str = "Investigate the unusual failure and explain it";
const INLINE_TOOL_NAME: &str = "inspect_fixture_failure";
const INLINE_TOOL_RESULT: &str = "fixture-analysis-complete";
const INLINE_FINAL: &str = "The fixture analysis found the bounded cause.";
const SYNTHESIS_MATCH: &str = "Synthesize the final user response for execution run";
const TEMPLATE_SKILL_NAME: &str = "service-template-report";
const TEMPLATE_FINAL: &str = "The pinned template produced the requested report.";
const RESEARCH_AGENT_SENTINEL: &str = "NO_SKILL_RESEARCH_AGENT";
const RESEARCH_FINAL: &str = "The durable no-skill research run completed.";
const INSTRUCTION_SKILL_NAME: &str = "agent-task-research";
const INSTRUCTION_SKILL_SENTINEL: &str = "AGENT_TASK_SKILL_SENTINEL_42";
const INSTRUCTION_AGENT_SENTINEL: &str = "USE_PINNED_AGENT_TASK_SKILL";
const INSTRUCTION_FINAL: &str = "The pinned instruction skill completed inside the Agent task.";
const INLINE_INSTRUCTION_OBJECTIVE: &str =
    "Use the agent-task-research instruction skill to inspect this bounded case";
const INLINE_INSTRUCTION_FINAL: &str =
    "The instruction-only skill guided the bounded Inline result.";
const INSTRUCTION_SKILL_PATH: &str = ".moa/skills/agent-task-research/SKILL.md";
const PLANNER_MATCH: &str = "<frozen_planning_context>";

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn respond_simple_question_uses_no_tools_planner_or_run_service_e2e() -> Result<()> {
    // Pins: a deterministic Respond route performs one no-tools model call and admits no run.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion(RESPOND_FINAL),
            "keyed": [route_classifier_completion(
                ExecutionRouteKind::Respond,
                RouteFixture::Respond,
            )]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_turn(&test, "respond-simple", RESPOND_OBJECTIVE, None).await?;

    let outcome = await_turn_outcome(test.client(), &started).await?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(outcome.message, RESPOND_FINAL);
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Paused
    );

    let events = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_route(&audits, ExecutionRouteKind::Respond, None);
    assert_no_planner_or_compile(&audits);
    assert_eq!(
        event_count(&events, |event| matches!(event, Event::ToolCall { .. })),
        0
    );
    assert_eq!(
        event_count(&events, |event| matches!(event, Event::ToolResult { .. })),
        0
    );
    assert_no_execution_lifecycle_events(&events);
    assert_non_durable_eval(&audits, &events, ExecutionRouteKind::Respond, None);
    assert_eq!(final_brain_response(&events)?, RESPOND_FINAL);

    let requests = journal_requests(fixture.scripted_requests()?)?;
    assert_eq!(
        journal_roles(&requests),
        vec![JournalRequestRole::Normal, JournalRequestRole::Normal]
    );
    assert_eq!(
        requests[0]
            .response_format
            .as_ref()
            .map(|format| format.name.as_str()),
        Some("execution_route_classifier")
    );
    assert!(requests.iter().all(|request| request.tools.is_empty()));
    assert!(requests[1].response_format.is_none());
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn execute_inline_runs_bounded_tool_loop_without_durable_run_service_e2e() -> Result<()> {
    // Pins: Execute/Inline uses the governed MCP path once, then completes without a run.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionRouteKind::Execute,
                    RouteFixture::Inline,
                ),
                keyed_completion(INLINE_TOOL_RESULT, text_completion(INLINE_FINAL)),
                keyed_completion(
                    INLINE_OBJECTIVE,
                    json!({
                        "content": "",
                        "tool_calls": [{
                            "name": INLINE_TOOL_NAME,
                            "id": "inline-fixture-tool-call",
                            "input": {"query": "unusual failure"}
                        }]
                    })
                )
            ]
        }),
        FixtureCapabilityOptions {
            tools: vec![FixtureCapabilityTool {
                name: INLINE_TOOL_NAME.to_string(),
                description: "Inspect one deterministic fixture failure".to_string(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["query"],
                    "properties": {"query": {"type": "string"}}
                }),
                item_key_pointer: None,
                outcomes: vec![FixtureCapabilityOutcome::Success {
                    output: json!({"result": INLINE_TOOL_RESULT}),
                }],
            }],
            orchestrator_env: Vec::new(),
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("execute-inline-tool-loop").await?;
    let session = test.client().get_session(session_id).await?;
    seed_allow_policy(&fixture, test.client(), session.tenant_id, INLINE_TOOL_NAME).await?;
    let started = start_turn_in_session(&test, session_id, INLINE_OBJECTIVE, None).await?;

    let controller = fixture
        .fixture_capability()
        .context("execution fixture omitted capability controller")?;
    let calls = tokio::select! {
        calls = controller.wait_for_calls(1, SERVICE_TIMEOUT) => {
            calls.context("wait for bounded Inline fixture call")?
        }
        outcome = await_turn_outcome(test.client(), &started) => {
            let outcome = outcome.context("await Inline outcome before fixture call")?;
            anyhow::bail!(
                "Inline turn reached terminal outcome before invoking `{INLINE_TOOL_NAME}`: {outcome:?}"
            );
        }
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, INLINE_TOOL_NAME);
    assert_eq!(calls[0].item_key, "");
    assert_eq!(calls[0].input, json!({"query": "unusual failure"}));
    controller.release(1);

    let outcome = await_turn_outcome(test.client(), &started).await?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(outcome.message, INLINE_FINAL);
    assert_eq!(controller.calls().len(), 1);
    assert_eq!(controller.transport_attempts().len(), 1);

    let events = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_route(
        &audits,
        ExecutionRouteKind::Execute,
        Some(ExecutionStrategy::Inline),
    );
    assert_no_planner_or_compile(&audits);
    assert_eq!(
        event_count(&events, |event| matches!(event, Event::ToolCall { .. })),
        1
    );
    assert_eq!(
        event_count(&events, |event| matches!(
            event,
            Event::ToolResult { success: true, .. }
        )),
        1
    );
    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::ToolCall { tool_name, input, .. }
            if tool_name == INLINE_TOOL_NAME && input == &json!({"query": "unusual failure"})
    )));
    assert_no_execution_lifecycle_events(&events);
    assert_non_durable_eval(
        &audits,
        &events,
        ExecutionRouteKind::Execute,
        Some(ExecutionStrategy::Inline),
    );
    assert_eq!(final_brain_response(&events)?, INLINE_FINAL);

    let requests = journal_requests(fixture.scripted_requests()?)?;
    assert_eq!(
        journal_roles(&requests),
        vec![
            JournalRequestRole::Normal,
            JournalRequestRole::Normal,
            JournalRequestRole::Normal,
        ]
    );
    assert_eq!(
        requests[0]
            .response_format
            .as_ref()
            .map(|format| format.name.as_str()),
        Some("execution_route_classifier")
    );
    assert!(
        requests[1..]
            .iter()
            .all(|request| request.response_format.is_none())
    );
    assert!(requests[1..].iter().all(|request| {
        request
            .messages
            .iter()
            .all(|message| !message.content.contains("Pinned instruction skills:"))
    }));
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn execute_inline_uses_instruction_only_skill_without_durable_run_service_e2e() -> Result<()>
{
    // Pins: selecting and reading an instruction-only skill changes Inline guidance without
    // changing Execute/Inline into Durable execution or invoking the planner.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionRouteKind::Execute,
                    RouteFixture::Inline,
                ),
                keyed_completion(
                    INSTRUCTION_SKILL_SENTINEL,
                    text_completion(INLINE_INSTRUCTION_FINAL)
                ),
                keyed_completion(
                    INLINE_INSTRUCTION_OBJECTIVE,
                    json!({
                        "content": "",
                        "tool_calls": [{
                            "name": "file_read",
                            "id": "inline-instruction-skill-read",
                            "input": {"path": INSTRUCTION_SKILL_PATH}
                        }]
                    })
                )
            ]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test
        .create_session("execute-inline-instruction-skill")
        .await?;
    let session = test.client().get_session(session_id).await?;
    let published = publish_skill(
        &fixture,
        test.client(),
        session.tenant_id,
        INSTRUCTION_SKILL_NAME,
        instruction_skill_source(),
        instruction_skill_markdown(),
    )
    .await?;
    assert_eq!(
        published.skill_ref,
        ArtifactRef::artifact(ArtifactKind::Skill, INSTRUCTION_SKILL_NAME).to_string()
    );
    let started =
        start_turn_in_session(&test, session_id, INLINE_INSTRUCTION_OBJECTIVE, None).await?;

    let outcome = await_turn_outcome(test.client(), &started).await?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(outcome.message, INLINE_INSTRUCTION_FINAL);
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Paused
    );

    let events = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_route(
        &audits,
        ExecutionRouteKind::Execute,
        Some(ExecutionStrategy::Inline),
    );
    assert_no_planner_or_compile(&audits);
    assert_eq!(
        event_count(&events, |event| matches!(event, Event::ToolCall { .. })),
        1
    );
    assert_eq!(
        event_count(&events, |event| matches!(
            event,
            Event::ToolResult { success: true, .. }
        )),
        1
    );
    assert!(events.iter().any(|record| matches!(
        &record.event,
        Event::ToolCall { tool_name, input, .. }
            if tool_name == "file_read" && input == &json!({"path": INSTRUCTION_SKILL_PATH})
    )));
    assert_no_execution_lifecycle_events(&events);
    assert_non_durable_eval(
        &audits,
        &events,
        ExecutionRouteKind::Execute,
        Some(ExecutionStrategy::Inline),
    );
    assert_eq!(final_brain_response(&events)?, INLINE_INSTRUCTION_FINAL);

    let requests = journal_requests(fixture.scripted_requests()?)?;
    assert_eq!(
        journal_roles(&requests),
        vec![
            JournalRequestRole::Normal,
            JournalRequestRole::Normal,
            JournalRequestRole::Normal,
        ]
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
        1
    );
    let inline_request = &requests[1];
    assert!(
        serde_json::to_string(&inline_request.tools)?.contains("file_read"),
        "selected instruction skill did not make its declared file_read capability available"
    );
    let inline_context = inline_request
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(inline_context.contains(INSTRUCTION_SKILL_NAME));
    let post_read_context = requests[2]
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(post_read_context.contains(INSTRUCTION_SKILL_SENTINEL));
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn published_skill_template_starts_without_plan_generation_service_e2e() -> Result<()> {
    // Pins: an exact pinned published template bypasses the planner and enters canonical runtime.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [keyed_completion(SYNTHESIS_MATCH, text_completion(TEMPLATE_FINAL))]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("published-template").await?;
    let session = test.client().get_session(session_id).await?;
    let published = publish_skill(
        &fixture,
        test.client(),
        session.tenant_id,
        TEMPLATE_SKILL_NAME,
        template_skill_source(),
        template_skill_markdown(),
    )
    .await?;
    let template_input = json!({"case_id": "case-42", "resolution": "resolved"});
    let started = start_turn_in_session(
        &test,
        session_id,
        "Produce the exact requested report from the pinned template.",
        Some(ExecutionTemplateInvocation {
            template: PinnedExecutionTemplateRef {
                skill_ref: published.skill_ref.clone(),
                revision_uid: published.revision_uid,
            },
            input: template_input.clone(),
        }),
    )
    .await?;

    let outcome = await_turn_outcome(test.client(), &started).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind else {
        anyhow::bail!("template root turn did not admit a run: {outcome:?}");
    };
    let admitted =
        await_run_started_event(test.client(), started.session_id, execution_run_uid).await?;
    assert_eq!(admitted.status, ExecutionRunAdmissionStatus::Queued);
    let run_request = execution_run_request(&started, execution_run_uid);
    let terminal = await_execution_terminal(test.client(), &run_request).await?;
    assert_completed_terminal(&terminal, 1, 1);
    assert_eq!(terminal.output, Some(template_input));
    assert_eq!(terminal.run.total_tasks, 1);
    assert_eq!(terminal.run.completed_tasks, 1);
    let repository = ExecutionRepository::new(
        sqlx::PgPool::connect(&fixture.postgres_url)
            .await
            .context("connect published-template provenance repository")?,
    );
    let persisted_run = repository
        .load_run(
            ExecutionScope::Tenant {
                tenant_id: started.tenant_id,
            },
            execution_run_uid,
        )
        .await?
        .context("published-template run should remain queryable")?;
    assert_persisted_skill_template_provenance(
        &persisted_run.source_provenance,
        &published.skill_ref,
        published.revision_uid,
    )?;
    let tasks = list_execution_tasks(test.client(), run_request.clone()).await?;
    assert!(tasks.next_cursor.is_none());
    assert_eq!(tasks.tasks.len(), 1);
    assert_eq!(tasks.tasks[0].node_id, "output");
    assert_eq!(tasks.tasks[0].status, ExecutionTaskStatus::Completed);
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Paused
    );

    let events = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_route(
        &audits,
        ExecutionRouteKind::Execute,
        Some(ExecutionStrategy::Durable),
    );
    assert_skill_template_audits(&audits);
    assert_eq!(
        event_count(&events, |event| matches!(
            event,
            Event::ExecutionCompleted(_)
        )),
        1
    );
    assert_eq!(
        event_count(&events, |event| matches!(
            event,
            Event::ExecutionSynthesisRequested(_)
        )),
        1
    );
    assert_eq!(final_brain_response(&events)?, TEMPLATE_FINAL);

    let requests = journal_requests(fixture.scripted_requests()?)?;
    assert_eq!(
        journal_roles(&requests),
        vec![JournalRequestRole::Synthesis]
    );
    assert!(requests.iter().all(|request| {
        request
            .response_format
            .as_ref()
            .is_none_or(|format| format.name != "generated_execution_candidate")
    }));
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn no_skill_research_compiles_executes_streams_and_synthesizes_service_e2e() -> Result<()> {
    // Pins: generated no-skill research admits Agent→Output, exposes progress, and auto-synthesizes.
    let objective = "Start an execution run to research the deterministic service fixture";
    let candidate = research_candidate(objective, RESEARCH_AGENT_SENTINEL, None);
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionRouteKind::Execute,
                    RouteFixture::Durable,
                ),
                keyed_completion(SYNTHESIS_MATCH, text_completion(RESEARCH_FINAL)),
                keyed_completion(
                    RESEARCH_AGENT_SENTINEL,
                    json!({
                        "content": serde_json::to_string(&json!({"answer": "research-complete"}))?,
                        "tool_calls": [],
                        "latency_ms": 3_000,
                        "ttft_ms": 3_000
                    })
                ),
                keyed_completion(
                    PLANNER_MATCH,
                    text_completion(&serde_json::to_string(&candidate)?)
                )
            ]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_turn(&test, "no-skill-research", objective, None).await?;

    let outcome = await_turn_outcome(test.client(), &started).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind else {
        anyhow::bail!("generated research turn did not admit a run: {outcome:?}");
    };
    await_run_started_event(test.client(), started.session_id, execution_run_uid).await?;
    let run_request = execution_run_request(&started, execution_run_uid);
    let active = await_active_execution_progress(test.client(), &run_request).await?;
    assert_eq!(active.run_uid, execution_run_uid);
    assert_eq!(active.completed, 0);
    assert!(active.total >= 1);

    let terminal = await_execution_terminal(test.client(), &run_request).await?;
    assert_completed_terminal(&terminal, 1, 1);
    assert_eq!(
        terminal.output,
        Some(json!({"answer": "research-complete"}))
    );
    assert_eq!(terminal.run.total_tasks, 2);
    assert_eq!(terminal.run.completed_tasks, 2);
    let tasks = list_execution_tasks(test.client(), run_request.clone()).await?;
    assert_eq!(tasks.tasks.len(), 2);
    assert_eq!(
        tasks
            .tasks
            .iter()
            .map(|task| (task.node_id.as_str(), task.status))
            .collect::<Vec<_>>(),
        vec![
            ("output", ExecutionTaskStatus::Completed),
            ("research", ExecutionTaskStatus::Completed),
        ]
    );
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Paused
    );

    let events = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_route(
        &audits,
        ExecutionRouteKind::Execute,
        Some(ExecutionStrategy::Durable),
    );
    assert_generated_plan_audits(&audits);
    assert_eq!(final_brain_response(&events)?, RESEARCH_FINAL);
    assert_generated_execution_event_order(&events);
    assert_execution_eval_case(
        &fixture,
        test.client(),
        &run_request,
        None,
        "generated-run-executes-and-synthesizes",
        &[
            ExecutionInvariantSpec::TerminalStatusIn {
                statuses: vec![ExecutionRunStatus::Completed],
            },
            ExecutionInvariantSpec::TaskCount {
                node_id: "research".to_string(),
                exact: 1,
            },
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoRawTaskOutputEvents,
        ],
    )
    .await?;

    let requests = journal_requests(fixture.scripted_requests()?)?;
    assert_eq!(
        journal_roles(&requests),
        vec![
            JournalRequestRole::Normal,
            JournalRequestRole::InitialPlanner,
            JournalRequestRole::AgentTask,
            JournalRequestRole::Synthesis,
        ]
    );
    assert!(requests[0].tools.is_empty());
    assert_eq!(
        requests[1]
            .response_format
            .as_ref()
            .map(|format| format.name.as_str()),
        Some("generated_execution_candidate")
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn instruction_only_skill_is_available_inside_agent_task_service_e2e() -> Result<()> {
    // Pins: a published skill without a template is pinned and injected into task-local Agent work.
    let objective =
        "Start an execution run using the agent-task-research instruction skill for this case";
    let skill_ref = ArtifactRef::artifact(ArtifactKind::Skill, INSTRUCTION_SKILL_NAME);
    let candidate = research_candidate(
        objective,
        INSTRUCTION_AGENT_SENTINEL,
        Some(skill_ref.clone()),
    );
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionRouteKind::Execute,
                    RouteFixture::Durable,
                ),
                keyed_completion(SYNTHESIS_MATCH, text_completion(INSTRUCTION_FINAL)),
                keyed_completion(
                    INSTRUCTION_SKILL_SENTINEL,
                    text_completion(&serde_json::to_string(&json!({
                        "answer": "instruction-skill-complete"
                    }))?)
                ),
                keyed_completion(
                    PLANNER_MATCH,
                    text_completion(&serde_json::to_string(&candidate)?)
                )
            ]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("instruction-agent-task").await?;
    let session = test.client().get_session(session_id).await?;
    let published = publish_skill(
        &fixture,
        test.client(),
        session.tenant_id,
        INSTRUCTION_SKILL_NAME,
        instruction_skill_source(),
        instruction_skill_markdown(),
    )
    .await?;
    assert_eq!(published.skill_ref, skill_ref.to_string());
    let started = start_turn_in_session(&test, session_id, objective, None).await?;

    let outcome = await_turn_outcome(test.client(), &started).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind else {
        anyhow::bail!("instruction-skill turn did not admit a run: {outcome:?}");
    };
    let run_request = execution_run_request(&started, execution_run_uid);
    let terminal = await_execution_terminal(test.client(), &run_request).await?;
    assert_completed_terminal(&terminal, 1, 1);
    assert_eq!(
        terminal.output,
        Some(json!({"answer": "instruction-skill-complete"}))
    );
    let tasks = list_execution_tasks(test.client(), run_request).await?;
    assert_eq!(tasks.tasks.len(), 2);
    assert!(
        tasks
            .tasks
            .iter()
            .all(|task| task.status == ExecutionTaskStatus::Completed)
    );
    assert_eq!(
        await_session_settled(test.client(), started.session_id).await?,
        SessionStatus::Paused
    );

    let events = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_route(
        &audits,
        ExecutionRouteKind::Execute,
        Some(ExecutionStrategy::Durable),
    );
    assert_generated_plan_audits(&audits);
    assert_eq!(final_brain_response(&events)?, INSTRUCTION_FINAL);
    let requests = journal_requests(fixture.scripted_requests()?)?;
    assert_eq!(
        journal_roles(&requests),
        vec![
            JournalRequestRole::Normal,
            JournalRequestRole::InitialPlanner,
            JournalRequestRole::AgentTask,
            JournalRequestRole::Synthesis,
        ]
    );
    let agent_request = requests
        .iter()
        .find(|request| {
            journal_roles(std::slice::from_ref(request)) == vec![JournalRequestRole::AgentTask]
        })
        .context("journal omitted task-local Agent request")?;
    assert!(
        agent_request
            .messages
            .iter()
            .any(|message| message.content.contains(INSTRUCTION_SKILL_SENTINEL))
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn non_root_continuations_cannot_enter_or_upgrade_to_durable_service_e2e() -> Result<()> {
    // Pins: worker-result and child-signal continuations cannot honor a Durable classifier
    // decision, while execution synthesis bypasses routing; none may persist route/upgrade audits
    // or admit a run.
    const CONTINUATION_OBJECTIVE: &str =
        "Start durable execution for this internally generated continuation";
    const SYNTHESIS_FINAL: &str = "The internal continuation stayed bounded.";
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion(SYNTHESIS_FINAL),
            "keyed": [route_classifier_completion(
                ExecutionRouteKind::Execute,
                RouteFixture::Durable,
            )]
        }),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;

    for trigger in [TurnTrigger::WorkerResults, TurnTrigger::ChildSignal] {
        let session_id = test
            .create_session(&format!("non-root-durable-{trigger:?}"))
            .await?;
        let session = test.client().get_session(session_id).await?;
        let turn_id = uuid::Uuid::now_v7().to_string();
        let outcome: TurnOutcome = test
            .client()
            .post_call(
                &format!("/TurnExecution/{turn_id}/run"),
                &RunTurnRequest {
                    session_id: session_id.to_string(),
                    turn_id,
                    identity: fixture_identity(&session)?,
                    contact: None,
                    user_message: CONTINUATION_OBJECTIVE.to_string(),
                    attachments: Vec::new(),
                    model: None,
                    max_turns: Some(1),
                    trigger,
                    child_signal_id: None,
                    execution_template: None,
                },
            )
            .await?;
        assert_eq!(outcome.kind, TurnOutcomeKind::Failed);
        assert!(
            outcome
                .message
                .contains("durable_execution_requires_user_message_origin"),
            "non-root Durable rejection lost its stable reason: {outcome:?}"
        );
        let events = raw_events(test.client(), session_id).await?;
        assert_no_execution_lifecycle_events(&events);
        assert!(
            planning_audits(&fixture.postgres_url, session_id)
                .await?
                .is_empty(),
            "non-root continuation persisted a route or Durable-upgrade audit"
        );
    }

    let synthesis_session_id = test.create_session("non-root-execution-synthesis").await?;
    let synthesis_session = test.client().get_session(synthesis_session_id).await?;
    let synthesis_turn_id = uuid::Uuid::now_v7().to_string();
    let synthesis_origin = test
        .client()
        .append_event(
            synthesis_session_id,
            Event::UserMessage {
                text: CONTINUATION_OBJECTIVE.to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let synthesis_run_uid = uuid::Uuid::now_v7();
    let synthesis_terminal = ExecutionTerminalSummary {
        run_uid: synthesis_run_uid,
        originating_user_sequence_num: synthesis_origin,
        output: Some(json!({"result": "bounded synthesis fixture"})),
        output_hash: [0; 32],
        citation_ids: Vec::new(),
        failures: Vec::new(),
        gaps: Vec::new(),
        task_results: ExecutionTaskResultsRef::ExecutionTaskTable {
            run_uid: synthesis_run_uid,
        },
    };
    test.client()
        .append_event(
            synthesis_session_id,
            Event::ExecutionSynthesisRequested(ExecutionSynthesisRequested {
                run_uid: synthesis_run_uid,
                originating_user_sequence_num: synthesis_origin,
                turn_id: synthesis_turn_id.clone(),
                terminal: synthesis_terminal,
                run_evidence: ExecutionRunEvidenceRef::ExecutionRun {
                    run_uid: synthesis_run_uid,
                },
            }),
        )
        .await?;
    let synthesis: TurnOutcome = test
        .client()
        .post_call(
            &format!("/TurnExecution/{synthesis_turn_id}/run"),
            &RunTurnRequest {
                session_id: synthesis_session_id.to_string(),
                turn_id: synthesis_turn_id.clone(),
                identity: fixture_identity(&synthesis_session)?,
                contact: None,
                user_message: CONTINUATION_OBJECTIVE.to_string(),
                attachments: Vec::new(),
                model: None,
                max_turns: Some(1),
                trigger: TurnTrigger::ExecutionSynthesis,
                child_signal_id: None,
                execution_template: None,
            },
        )
        .await?;
    assert_eq!(synthesis.kind, TurnOutcomeKind::Completed);
    assert_eq!(synthesis.message, SYNTHESIS_FINAL);
    let synthesis_events = raw_events(test.client(), synthesis_session_id).await?;
    assert_eq!(
        synthesis_events
            .iter()
            .filter(|record| matches!(
                record.event,
                Event::ExecutionRunStarted(_)
                    | Event::ExecutionProgress(_)
                    | Event::ExecutionInputRequired(_)
                    | Event::ExecutionCompleted(_)
                    | Event::ExecutionFailed { .. }
                    | Event::ExecutionCancelled(_)
            ))
            .count(),
        0,
        "execution synthesis continuation admitted or advanced a Durable run"
    );
    assert_eq!(
        synthesis_events
            .iter()
            .filter(|record| matches!(
                &record.event,
                Event::ExecutionSynthesisRequested(requested)
                    if requested.run_uid == synthesis_run_uid
                        && requested.originating_user_sequence_num == synthesis_origin
                        && requested.turn_id == synthesis_turn_id
            ))
            .count(),
        1,
        "execution synthesis fixture lost its exact durable trigger"
    );
    assert!(
        planning_audits(&fixture.postgres_url, synthesis_session_id)
            .await?
            .is_empty(),
        "execution synthesis persisted a route or Durable-upgrade audit"
    );

    let requests = journal_requests(fixture.scripted_requests()?)?;
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
        2,
        "only worker-result and child-signal continuations may reach the classifier"
    );
    assert!(requests.iter().all(|request| {
        request
            .response_format
            .as_ref()
            .is_none_or(|format| format.name != "generated_execution_candidate")
    }));
    Ok(())
}

fn fixture_identity(meta: &moa_core::types::session::SessionMeta) -> Result<Identity> {
    let moa_core::types::contact::SessionActorRef::Identity { id } = meta
        .created_by
        .as_ref()
        .context("fixture session omitted its owning identity")?
    else {
        anyhow::bail!(
            "fixture session owner is not an identity: {:?}",
            meta.created_by
        );
    };
    Ok(Identity {
        identity_type: IdentityType::Operator,
        id: *id,
        tenant_id: meta.tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    })
}

fn text_completion(content: impl Into<String>) -> Value {
    json!({"content": content.into(), "tool_calls": []})
}

fn keyed_completion(match_substring: &str, completion: Value) -> Value {
    json!({"match": match_substring, "completion": completion})
}

fn research_candidate(
    objective: &str,
    instructions: &str,
    skill_ref: Option<ArtifactRef>,
) -> GeneratedExecutionCandidate {
    let output_schema = answer_schema();
    GeneratedExecutionCandidate {
        goal: goal_contract(objective),
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: empty_input_schema(),
            output_schema: output_schema.clone(),
            nodes: vec![
                ExecutionNode {
                    id: "research".to_string(),
                    requirement_ids: vec!["research_result".to_string()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({}),
                    output_schema: output_schema.clone(),
                    operation: ExecutionOperation::Agent {
                        instructions: instructions.to_string(),
                        skill_refs: skill_ref.into_iter().collect(),
                        capability_refs: Vec::new(),
                        max_turns: 1,
                    },
                    retry: no_retry(),
                    budget: None,
                },
                ExecutionNode {
                    id: "output".to_string(),
                    requirement_ids: vec!["research_result".to_string()],
                    depends_on: vec!["research".to_string()],
                    when: None,
                    input: json!({}),
                    output_schema,
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

fn goal_contract(objective: &str) -> ExecutionGoalContract {
    ExecutionGoalContract {
        objective: objective.to_string(),
        requirements: vec![ExecutionRequirement {
            id: "research_result".to_string(),
            description: "produce the requested deterministic result".to_string(),
        }],
        deliverables: Vec::new(),
        coverage: Vec::new(),
        constraints: Vec::new(),
        completion_checks: vec![CompletionCheck {
            id: "output_schema".to_string(),
            description: "terminal output satisfies the declared schema".to_string(),
            requirement_ids: vec!["research_result".to_string()],
            constraint_ids: Vec::new(),
            kind: CompletionCheckKind::OutputSchema,
        }],
    }
}

fn answer_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["answer"],
        "properties": {"answer": {"type": "string"}}
    })
}

fn empty_input_schema() -> Value {
    json!({"type": "object", "additionalProperties": false})
}

fn no_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    }
}

fn template_skill_source() -> String {
    let template = ExecutionPlanTemplate {
        goal: ExecutionGoalTemplate {
            requirements: vec![ExecutionRequirement {
                id: "template_result".to_string(),
                description: "produce the exact pinned template result".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "template_output_schema".to_string(),
                description: "template output satisfies its schema".to_string(),
                requirement_ids: vec!["template_result".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: template_io_schema(),
            output_schema: template_io_schema(),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["template_result".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: template_io_schema(),
                operation: ExecutionOperation::Output {
                    value: json!({
                        "case_id": {"$ref": "$.input.case_id"},
                        "resolution": {"$ref": "$.input.resolution"}
                    }),
                },
                retry: no_retry(),
                budget: None,
            }],
        },
    };
    format!(
        "api_version: moa.artifact/v1\nkind: skill\nmetadata:\n  name: {TEMPLATE_SKILL_NAME}\n  description: Deterministic pinned service template.\nstatus: draft\ndefinition:\n  type: skill\n  spec:\n    instructions:\n      path: SKILL.md\n    inputs: {}\n    outputs: {}\n    execution_plan: {}\n",
        serde_json::to_string(&template_io_schema()).expect("serialize template input schema"),
        serde_json::to_string(&template_io_schema()).expect("serialize template output schema"),
        serde_json::to_string(&template).expect("serialize execution template")
    )
}

fn template_io_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["case_id", "resolution"],
        "properties": {
            "case_id": {"type": "string"},
            "resolution": {"type": "string"}
        }
    })
}

fn template_skill_markdown() -> &'static str {
    r#"---
name: service-template-report
description: Deterministic pinned service template.
---

# Service Template Report

Use the exact structured input supplied to the pinned execution template.
"#
}

fn instruction_skill_source() -> String {
    format!(
        "api_version: moa.artifact/v1\nkind: skill\nmetadata:\n  name: {INSTRUCTION_SKILL_NAME}\n  description: Agent task research instructions for deterministic service verification.\n  tags: [agent-task-research, deterministic]\nstatus: draft\ndefinition:\n  type: skill\n  spec:\n    instructions:\n      path: SKILL.md\n    inputs: {{\"type\":\"object\"}}\n    outputs: {{\"type\":\"object\"}}\n    allowed_tools: [file_read]\n"
    )
}

fn instruction_skill_markdown() -> &'static str {
    r#"---
name: agent-task-research
description: Agent task research instructions for deterministic service verification.
allowed-tools: file_read
metadata:
  moa-tags: "agent-task-research,deterministic"
---

# Agent Task Research

AGENT_TASK_SKILL_SENTINEL_42

Return a concise structured research result for the task input.
"#
}

fn assert_persisted_skill_template_provenance(
    actual: &ExecutionSourceProvenance,
    expected_skill_ref: &str,
    expected_revision_uid: uuid::Uuid,
) -> Result<()> {
    let ExecutionSourceProvenance::SkillTemplate {
        skill_template_ref,
        skill_template_revision_uid,
    } = actual
    else {
        anyhow::bail!("persisted execution source is not a skill template: {actual:?}");
    };
    anyhow::ensure!(
        skill_template_ref == expected_skill_ref,
        "persisted canonical skill ref mismatch; expected {expected_skill_ref:?}, actual {skill_template_ref:?}"
    );
    anyhow::ensure!(
        *skill_template_revision_uid == expected_revision_uid,
        "persisted skill-template revision mismatch; expected {expected_revision_uid}, actual {skill_template_revision_uid}"
    );
    Ok(())
}

fn assert_generated_execution_event_order(events: &[moa_core::types::events_stream::EventRecord]) {
    let progress_sequence = events
        .iter()
        .find_map(|record| {
            matches!(record.event, Event::ExecutionProgress(_)).then_some(record.sequence_num)
        })
        .context("generated execution emitted no progress event")
        .expect("progress assertion must retain diagnostics");
    assert_strict_event_order(&[
        (
            "run started",
            sole_event_sequence(events, "ExecutionRunStarted", |event| {
                matches!(event, Event::ExecutionRunStarted(_))
            }),
        ),
        ("execution progress", progress_sequence),
        (
            "execution completed",
            sole_event_sequence(events, "ExecutionCompleted", |event| {
                matches!(event, Event::ExecutionCompleted(_))
            }),
        ),
        (
            "synthesis requested",
            sole_event_sequence(events, "ExecutionSynthesisRequested", |event| {
                matches!(event, Event::ExecutionSynthesisRequested(_))
            }),
        ),
        (
            "final BrainResponse",
            sole_event_sequence(events, "BrainResponse", |event| {
                matches!(event, Event::BrainResponse { .. })
            }),
        ),
    ]);
}

#[cfg(test)]
mod tests {
    use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument};
    use moa_core::types::execution_planning::ExecutionSourceProvenance;

    use super::{
        INSTRUCTION_SKILL_NAME, TEMPLATE_SKILL_NAME, assert_persisted_skill_template_provenance,
        instruction_skill_source, template_skill_source,
    };

    #[test]
    fn scenario_skill_sources_and_persisted_template_provenance_are_strict() {
        // Pins: fixtures use canonical skill documents and exact persisted template revisions.
        let template = ArtifactDocument::from_yaml(&template_skill_source())
            .expect("parse deterministic template skill source");
        assert_eq!(template.metadata.name, TEMPLATE_SKILL_NAME);
        let ArtifactDefinition::Skill(template_skill) = template.definition else {
            panic!("template fixture must parse as a skill artifact");
        };
        assert!(template_skill.execution_plan.is_some());

        let instruction = ArtifactDocument::from_yaml(&instruction_skill_source())
            .expect("parse deterministic instruction skill source");
        assert_eq!(instruction.metadata.name, INSTRUCTION_SKILL_NAME);
        let ArtifactDefinition::Skill(instruction_skill) = instruction.definition else {
            panic!("instruction fixture must parse as a skill artifact");
        };
        assert!(instruction_skill.execution_plan.is_none());

        let revision_uid = uuid::Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .expect("parse deterministic template revision");
        let provenance = ExecutionSourceProvenance::SkillTemplate {
            skill_template_ref: "skill://service-template-report".to_string(),
            skill_template_revision_uid: revision_uid,
        };
        assert_persisted_skill_template_provenance(
            &provenance,
            "skill://service-template-report",
            revision_uid,
        )
        .expect("exact persisted template provenance should match");
        let wrong_revision = uuid::Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            .expect("parse mismatched template revision");
        let error = assert_persisted_skill_template_provenance(
            &provenance,
            "skill://service-template-report",
            wrong_revision,
        )
        .expect_err("different pinned revision must not match persisted provenance");
        assert!(error.to_string().contains("revision mismatch"));
    }
}
