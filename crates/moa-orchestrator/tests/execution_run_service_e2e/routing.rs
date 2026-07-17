//! Deterministic service coverage for Respond, Act, template, and generated-plan routing.
//!
//! Instruction-skill activation in conversational Act mode remains pinned by
//! `integration/agent_artifacts_e2e::support_agent_selects_refund_skill_without_starting_execution_run`;
//! this module adds the non-duplicative task-local Agent half.

use anyhow::{Context, Result};
use moa_artifacts::document::ArtifactKind;
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionGoalContract, ExecutionGoalTemplate,
    ExecutionNode, ExecutionOperation, ExecutionPlanDefinition, ExecutionPlanTemplate,
    ExecutionRequirement, GeneratedExecutionCandidate, RetryPolicy,
};
use moa_artifacts::reference::ArtifactRef;
use moa_core::events::Event;
use moa_core::types::execution_planning::{
    ExecutionMode, ExecutionRouteReason, ExecutionRunAdmissionStatus, ExecutionSourceProvenanceV1,
    ExecutionTemplateInvocation, PinnedExecutionTemplateRef,
};
use moa_core::types::session::SessionStatus;
use moa_core::wire::turn::TurnOutcomeKind;
use moa_eval::execution::ExecutionInvariantSpecV1;
use moa_execution::{
    repository::{ExecutionRepository, ExecutionScope},
    state::{ExecutionRunStatus, ExecutionTaskStatus},
};
use moa_test_support::{
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool,
    OrchestratorTestFixture,
};
use serde_json::{Value, json};

use crate::evaluation::{assert_execution_eval_case, assert_non_run_eval};
use crate::execution_execution_support::assertions::{
    JournalRequestRole, assert_completed_terminal, assert_generated_plan_audits,
    assert_initial_route, assert_no_execution_lifecycle_events, assert_no_planner_or_compile,
    assert_skill_template_audits, assert_strict_event_order, event_count, final_brain_response,
    journal_requests, journal_roles, planning_audits, sole_event_sequence,
};
use crate::execution_execution_support::fixtures::{
    SERVICE_TIMEOUT, await_active_execution_progress, await_execution_terminal,
    await_run_started_event, await_session_settled, await_turn_outcome, execution_run_request,
    list_execution_tasks, publish_skill, raw_events, route_classifier_completion,
    seed_allow_policy, start_turn, start_turn_in_session,
};

const RESPOND_OBJECTIVE: &str = "What is a DAG?";
const RESPOND_FINAL: &str = "A DAG is a directed acyclic graph.";
const ACT_OBJECTIVE: &str = "Investigate the unusual failure and explain it";
const ACT_TOOL_NAME: &str = "inspect_fixture_failure";
const ACT_TOOL_RESULT: &str = "fixture-analysis-complete";
const ACT_FINAL: &str = "The fixture analysis found the bounded cause.";
const SYNTHESIS_MATCH: &str = "Synthesize the final user response for execution run";
const TEMPLATE_SKILL_NAME: &str = "service-template-report";
const TEMPLATE_FINAL: &str = "The pinned template produced the requested report.";
const RESEARCH_AGENT_SENTINEL: &str = "NO_SKILL_RESEARCH_AGENT_V1";
const RESEARCH_FINAL: &str = "The durable no-skill research run completed.";
const INSTRUCTION_SKILL_NAME: &str = "agent-task-research";
const INSTRUCTION_SKILL_SENTINEL: &str = "AGENT_TASK_SKILL_SENTINEL_42";
const INSTRUCTION_AGENT_SENTINEL: &str = "USE_PINNED_AGENT_TASK_SKILL_V1";
const INSTRUCTION_FINAL: &str = "The pinned instruction skill completed inside the Agent task.";
const PLANNER_MATCH: &str = "<frozen_planning_context>";

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn respond_simple_question_uses_no_tools_planner_or_run_service_e2e() -> Result<()> {
    // Pins: a deterministic Respond route performs one no-tools model call and admits no run.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion(RESPOND_FINAL),
            "keyed": [route_classifier_completion(
                ExecutionMode::Respond,
                ExecutionRouteReason::SimpleResponse,
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
    assert_initial_route(
        &audits,
        ExecutionMode::Respond,
        ExecutionRouteReason::SimpleResponse,
    );
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
    assert_non_run_eval(
        &audits,
        &events,
        ExecutionMode::Respond,
        ExecutionRouteReason::SimpleResponse,
    );
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
        Some("execution_route_classifier_v1")
    );
    assert!(requests.iter().all(|request| request.tools.is_empty()));
    assert!(requests[1].response_format.is_none());
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn act_executes_bounded_tool_loop_without_run_service_e2e() -> Result<()> {
    // Pins: Act uses the governed MCP path once, then completes conversationally without a run.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionMode::Act,
                    ExecutionRouteReason::BoundedInteractiveWork,
                ),
                keyed_completion(ACT_TOOL_RESULT, text_completion(ACT_FINAL)),
                keyed_completion(
                    ACT_OBJECTIVE,
                    json!({
                        "content": "",
                        "tool_calls": [{
                            "name": ACT_TOOL_NAME,
                            "id": "act-fixture-tool-call",
                            "input": {"query": "unusual failure"}
                        }]
                    })
                )
            ]
        }),
        FixtureCapabilityOptions {
            tools: vec![FixtureCapabilityTool {
                name: ACT_TOOL_NAME.to_string(),
                description: "Inspect one deterministic fixture failure".to_string(),
                input_schema: json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["query"],
                    "properties": {"query": {"type": "string"}}
                }),
                item_key_pointer: None,
                outcomes: vec![FixtureCapabilityOutcome::Success {
                    output: json!({"result": ACT_TOOL_RESULT}),
                }],
            }],
            orchestrator_env: Vec::new(),
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let session_id = test.create_session("act-tool-loop").await?;
    let session = test.client().get_session(session_id).await?;
    seed_allow_policy(&fixture, test.client(), session.tenant_id, ACT_TOOL_NAME).await?;
    let started = start_turn_in_session(&test, session_id, ACT_OBJECTIVE, None).await?;

    let controller = fixture
        .fixture_capability()
        .context("execution fixture omitted capability controller")?;
    let calls = tokio::select! {
        calls = controller.wait_for_calls(1, SERVICE_TIMEOUT) => {
            calls.context("wait for bounded Act fixture call")?
        }
        outcome = await_turn_outcome(test.client(), &started) => {
            let outcome = outcome.context("await Act outcome before fixture call")?;
            anyhow::bail!(
                "Act turn reached terminal outcome before invoking `{ACT_TOOL_NAME}`: {outcome:?}"
            );
        }
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].capability, ACT_TOOL_NAME);
    assert_eq!(calls[0].item_key, "");
    assert_eq!(calls[0].input, json!({"query": "unusual failure"}));
    controller.release(1);

    let outcome = await_turn_outcome(test.client(), &started).await?;
    assert_eq!(outcome.kind, TurnOutcomeKind::Completed);
    assert_eq!(outcome.message, ACT_FINAL);
    assert_eq!(controller.calls().len(), 1);
    assert_eq!(controller.transport_attempts().len(), 1);

    let events = raw_events(test.client(), started.session_id).await?;
    let audits = planning_audits(&fixture.postgres_url, started.session_id).await?;
    assert_initial_route(
        &audits,
        ExecutionMode::Act,
        ExecutionRouteReason::BoundedInteractiveWork,
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
            if tool_name == ACT_TOOL_NAME && input == &json!({"query": "unusual failure"})
    )));
    assert_no_execution_lifecycle_events(&events);
    assert_non_run_eval(
        &audits,
        &events,
        ExecutionMode::Act,
        ExecutionRouteReason::BoundedInteractiveWork,
    );
    assert_eq!(final_brain_response(&events)?, ACT_FINAL);

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
        Some("execution_route_classifier_v1")
    );
    assert!(
        requests[1..]
            .iter()
            .all(|request| request.response_format.is_none())
    );
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
        ExecutionMode::Run,
        ExecutionRouteReason::SelectedExecutionTemplate,
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
            .is_none_or(|format| format.name != "generated_execution_candidate_v1")
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
                    ExecutionMode::Run,
                    ExecutionRouteReason::ExplicitRun,
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
        ExecutionMode::Run,
        ExecutionRouteReason::ExplicitRun,
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
            ExecutionInvariantSpecV1::TerminalStatusIn {
                statuses: vec![ExecutionRunStatus::Completed],
            },
            ExecutionInvariantSpecV1::TaskCount {
                node_id: "research".to_string(),
                exact: 1,
            },
            ExecutionInvariantSpecV1::BudgetWithinApproved,
            ExecutionInvariantSpecV1::ProgressMatchesTasks,
            ExecutionInvariantSpecV1::NoRawTaskOutputEvents,
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
        Some("generated_execution_candidate_v1")
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
                    ExecutionMode::Run,
                    ExecutionRouteReason::ExplicitRun,
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
        ExecutionMode::Run,
        ExecutionRouteReason::ExplicitRun,
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
        "api_version: moa.artifact/v1\nkind: skill\nmetadata:\n  name: {INSTRUCTION_SKILL_NAME}\n  description: Agent task research instructions for deterministic service verification.\n  tags: [agent-task-research, deterministic]\nstatus: draft\ndefinition:\n  type: skill\n  spec:\n    instructions:\n      path: SKILL.md\n    inputs: {{\"type\":\"object\"}}\n    outputs: {{\"type\":\"object\"}}\n"
    )
}

fn instruction_skill_markdown() -> &'static str {
    r#"---
name: agent-task-research
description: Agent task research instructions for deterministic service verification.
metadata:
  moa-tags: "agent-task-research,deterministic"
---

# Agent Task Research

AGENT_TASK_SKILL_SENTINEL_42

Return a concise structured research result for the task input.
"#
}

fn assert_persisted_skill_template_provenance(
    actual: &ExecutionSourceProvenanceV1,
    expected_skill_ref: &str,
    expected_revision_uid: uuid::Uuid,
) -> Result<()> {
    let ExecutionSourceProvenanceV1::SkillTemplate {
        route_reason,
        skill_template_ref,
        skill_template_revision_uid,
    } = actual
    else {
        anyhow::bail!("persisted execution source is not a skill template: {actual:?}");
    };
    anyhow::ensure!(
        *route_reason == ExecutionRouteReason::SelectedExecutionTemplate,
        "persisted skill-template route reason mismatch: {route_reason:?}"
    );
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
    use moa_core::types::execution_planning::{ExecutionRouteReason, ExecutionSourceProvenanceV1};

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
        let provenance = ExecutionSourceProvenanceV1::SkillTemplate {
            route_reason: ExecutionRouteReason::SelectedExecutionTemplate,
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
