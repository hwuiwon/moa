//! Deterministic replan-stop and completion-gate service scenarios.

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, CoverageRequirement,
    ExecutionDeliverable, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionReference, ExecutionRequirement, ExecutionTaskResult,
    GeneratedAmendmentCandidate, MapTask, PlanAmendment, PlanAmendmentOperation, RetryPolicy,
};
use moa_core::config::ExecutionConfig;
use moa_core::events::Event;
use moa_execution::capability::{
    amendment_hash, amendment_operations_fingerprint, task_output_hash,
};
use moa_execution::compiler::{CompileExecutionRequest, compile};
use moa_execution::completion::CompletionCheckResult;
use moa_execution::state::{
    ExecutionRunStatus, ExecutionTaskProjection, ExecutionTaskStatus, ExecutionTerminalCause,
    ExecutionTerminalEvidence,
};
use moa_execution::wire::{
    ExecutionAmendmentRequest, ExecutionConflictReason, ExecutionMutationResponse,
    ExecutionPlanningContextRequest, ExecutionPlanningContextResponse, ExecutionRunRequest,
    ExecutionStartRequest, ExecutionStartResponse, ExecutionStatusResponse,
    ExecutionSynthesisEvidence, ExecutionSynthesisEvidenceRequest,
};
use moa_execution::{ReplanStopReason, bindings::extract_map_key};
use moa_test_support::{
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool, IsolatedTest,
    OrchestratorTestFixture, TestApiClient,
};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::execution_execution_support::fixtures::{
    POLL_INTERVAL, SERVICE_TIMEOUT, await_execution_terminal, list_execution_tasks,
    seed_allow_policy,
};

const SYNTHESIS_MATCH: &str = "Synthesize the final user response for execution run";
const SCRIPTED_SYNTHESIS: &str = "The deterministic execution scenario reached terminal state.";
const AMENDMENT_DELAY_MS: u64 = 3_000;
const USEFUL_OUTPUT_NODE: &str = "useful_output";
const USEFUL_OUTPUT_REQUIREMENT: &str = "useful_result";
const REPAIR_REQUIREMENT: &str = "repair_result";
const USEFUL_OUTPUT: &str = "preserved-useful-output";

struct StartedExecution {
    originating_user_sequence_num: u64,
    run: ExecutionRunRequest,
}

struct MapThenOutputPlan<'a> {
    map_node_id: &'a str,
    map_requirement_id: &'a str,
    reference: CapabilityReference,
    items: Value,
    item_key: &'a str,
    max_items: u64,
    output_requirement_id: &'a str,
    output: Value,
    output_schema: Value,
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn duplicate_plan_stops_replan_service_e2e() -> Result<()> {
    // Pins: returning to an already observed canonical plan stops a useful run as Partial.
    let agent_a = agent_node("agent_a", "DUPLICATE_PLAN_AGENT_A");
    let agent_b = agent_node("agent_b", "DUPLICATE_PLAN_AGENT_B");
    let first = replacement_amendment(
        1,
        "agent_a",
        agent_b.clone(),
        "replace A with B before duplicate-plan detection",
    );
    let duplicate = replacement_amendment(
        2,
        "agent_b",
        agent_a.clone(),
        "return to the exact initial plan",
    );
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        replan_script(
            &[
                ("duplicate-plan-trigger-a", &first),
                ("duplicate-plan-trigger-b", &duplicate),
            ],
            &[
                (
                    "DUPLICATE_PLAN_AGENT_A",
                    needs_replan_result("duplicate-plan-trigger-a"),
                ),
                (
                    "DUPLICATE_PLAN_AGENT_B",
                    needs_replan_result("duplicate-plan-trigger-b"),
                ),
            ],
        )?,
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "duplicate-plan-stop",
        "preserve useful output and stop a duplicate plan",
        None,
        move |_| Ok(useful_replan_contract(agent_a)),
    )
    .await?;

    await_waiting_replan(test.client(), &started.run, 1).await?;
    await_amendment_request_count(&fixture, 1).await?;
    assert_applied(
        apply_amendment(test.client(), &started, first.clone()).await?,
        2,
    );
    await_waiting_replan(test.client(), &started.run, 2).await?;
    await_amendment_request_count(&fixture, 2).await?;
    assert_applied(
        apply_amendment(test.client(), &started, duplicate.clone()).await?,
        2,
    );

    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_replan_stop(
        &terminal,
        ReplanStopReason::DuplicatePlan,
        "return to the exact initial plan",
        1,
        2,
        3,
    );
    assert_eq!(terminal.run.plan_revision, 2);
    assert_replayed(
        apply_amendment(test.client(), &started, duplicate.clone()).await?,
        2,
    );
    assert_semantic_conflict(test.client(), &started, duplicate).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn duplicate_amendment_stops_replan_service_e2e() -> Result<()> {
    // Pins: operation semantics repeat across revisions even when revision, reason, and hash differ.
    let agent_a = agent_node("agent_a", "DUPLICATE_AMENDMENT_AGENT_A");
    let agent_b = agent_node("agent_b", "DUPLICATE_AMENDMENT_AGENT_B");
    let first = replacement_amendment(1, "agent_a", agent_b, "first semantic replacement");
    let repeated = PlanAmendment {
        schema_version: 1,
        base_plan_revision: 2,
        reason: "same operations at a later revision".to_string(),
        evidence: json!({"planner_observation": "changed prose cannot evade loop identity"}),
        operations: first.operations.clone(),
    };
    assert_eq!(
        amendment_operations_fingerprint(&first)?,
        amendment_operations_fingerprint(&repeated)?,
        "the test must repeat semantic operations"
    );
    assert_ne!(
        amendment_hash(&first)?,
        amendment_hash(&repeated)?,
        "revision and prose must keep the exact amendment hashes distinct"
    );

    let fixture = OrchestratorTestFixture::with_execution_fixture(
        replan_script(
            &[
                ("duplicate-amendment-trigger-a", &first),
                ("duplicate-amendment-trigger-b", &repeated),
            ],
            &[
                (
                    "DUPLICATE_AMENDMENT_AGENT_A",
                    needs_replan_result("duplicate-amendment-trigger-a"),
                ),
                (
                    "DUPLICATE_AMENDMENT_AGENT_B",
                    needs_replan_result("duplicate-amendment-trigger-b"),
                ),
            ],
        )?,
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "duplicate-amendment-stop",
        "preserve useful output and stop repeated amendment semantics",
        None,
        move |_| Ok(useful_replan_contract(agent_a)),
    )
    .await?;

    await_waiting_replan(test.client(), &started.run, 1).await?;
    await_amendment_request_count(&fixture, 1).await?;
    assert_applied(apply_amendment(test.client(), &started, first).await?, 2);
    await_waiting_replan(test.client(), &started.run, 2).await?;
    await_amendment_request_count(&fixture, 2).await?;
    assert_applied(
        apply_amendment(test.client(), &started, repeated.clone()).await?,
        2,
    );

    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_replan_stop(
        &terminal,
        ReplanStopReason::DuplicateAmendment,
        "same operations at a later revision",
        1,
        2,
        3,
    );
    assert_eq!(terminal.run.plan_revision, 2);
    assert_replayed(apply_amendment(test.client(), &started, repeated).await?, 2);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn repeated_failure_stops_replan_service_e2e() -> Result<()> {
    // Pins: the configured failure threshold is enforced before replacement work is admitted.
    let agent_a = agent_node("agent_a", "REPEATED_FAILURE_AGENT_A");
    let replacement = replacement_amendment(
        1,
        "agent_a",
        agent_node("agent_b", "REPEATED_FAILURE_AGENT_B"),
        "replacement blocked by configured repeated failure",
    );
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        replan_script(
            &[("configured-repeated-failure", &replacement)],
            &[(
                "REPEATED_FAILURE_AGENT_A",
                needs_replan_result("configured-repeated-failure"),
            )],
        )?,
        FixtureCapabilityOptions {
            tools: Vec::new(),
            orchestrator_env: vec![(
                "MOA_EXECUTION_REPEATED_FAILURE_LIMIT".to_string(),
                "1".to_string(),
            )],
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "repeated-failure-stop",
        "preserve useful output and stop at the configured failure threshold",
        None,
        move |_| Ok(useful_replan_contract(agent_a)),
    )
    .await?;

    await_waiting_replan(test.client(), &started.run, 1).await?;
    await_amendment_request_count(&fixture, 1).await?;
    assert_applied(
        apply_amendment(test.client(), &started, replacement).await?,
        1,
    );
    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_replan_stop(
        &terminal,
        ReplanStopReason::RepeatedFailure,
        "replacement blocked by configured repeated failure",
        1,
        2,
        2,
    );
    assert_eq!(terminal.run.plan_revision, 1);
    assert_eq!(
        list_execution_tasks(test.client(), started.run.clone())
            .await?
            .tasks
            .iter()
            .filter(|task| task.node_id == "agent_b")
            .count(),
        0,
        "repeated-failure stop must not materialize replacement work"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn remove_only_amendment_is_no_progress_service_e2e() -> Result<()> {
    // Pins: a remove-only patch cannot claim progress on an unresolved requirement.
    let agent_a = agent_node("agent_a", "REMOVE_ONLY_AGENT_A");
    let remove_only = PlanAmendment {
        schema_version: 1,
        base_plan_revision: 1,
        reason: "remove-only amendment has no unresolved work".to_string(),
        evidence: json!({"kind": "remove_only"}),
        operations: vec![PlanAmendmentOperation::RemovePendingNode {
            node_id: "agent_a".to_string(),
        }],
    };
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        replan_script(
            &[("remove-only-trigger", &remove_only)],
            &[(
                "REMOVE_ONLY_AGENT_A",
                needs_replan_result("remove-only-trigger"),
            )],
        )?,
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "remove-only-no-progress",
        "preserve useful output and reject remove-only replanning",
        None,
        move |_| Ok(useful_replan_contract(agent_a)),
    )
    .await?;

    await_waiting_replan(test.client(), &started.run, 1).await?;
    await_amendment_request_count(&fixture, 1).await?;
    assert_applied(
        apply_amendment(test.client(), &started, remove_only.clone()).await?,
        1,
    );
    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_replan_stop(
        &terminal,
        ReplanStopReason::NoProgress,
        "remove-only amendment has no unresolved work",
        1,
        2,
        2,
    );
    assert_replayed(
        apply_amendment(test.client(), &started, remove_only.clone()).await?,
        1,
    );
    assert_semantic_conflict(test.client(), &started, remove_only).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn useful_amendments_stop_at_budget_service_e2e() -> Result<()> {
    // Pins: five useful accepted amendments consume the exact task budget before a sixth plan call.
    let agent_ids = [
        "agent_a", "agent_b", "agent_c", "agent_d", "agent_e", "agent_f",
    ];
    let instructions = [
        "BUDGET_AGENT_A",
        "BUDGET_AGENT_B",
        "BUDGET_AGENT_C",
        "BUDGET_AGENT_D",
        "BUDGET_AGENT_E",
        "BUDGET_AGENT_F",
    ];
    let failure_reasons = [
        "budget-replan-a",
        "budget-replan-b",
        "budget-replan-c",
        "budget-replan-d",
        "budget-replan-e",
        "budget-replan-f",
    ];
    let amendments = (0..5)
        .map(|index| {
            replacement_amendment(
                u64::try_from(index + 1).expect("small revision fits u64"),
                agent_ids[index],
                agent_node(agent_ids[index + 1], instructions[index + 1]),
                &format!("useful amendment {} of five", index + 1),
            )
        })
        .collect::<Vec<_>>();
    let amendment_script = amendments
        .iter()
        .enumerate()
        .map(|(index, amendment)| (failure_reasons[index], amendment))
        .collect::<Vec<_>>();
    let agent_script = instructions
        .iter()
        .zip(failure_reasons.iter())
        .map(|(instruction, reason)| (*instruction, needs_replan_result(reason)))
        .collect::<Vec<_>>();
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        replan_script(&amendment_script, &agent_script)?,
        FixtureCapabilityOptions {
            tools: Vec::new(),
            orchestrator_env: vec![("MOA_EXECUTION_MAX_TASKS".to_string(), "7".to_string())],
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "five-amendment-budget-stop",
        "preserve useful output across five amendments then stop at budget",
        None,
        move |_| {
            Ok(useful_replan_contract(agent_node(
                agent_ids[0],
                instructions[0],
            )))
        },
    )
    .await?;

    for (index, amendment) in amendments.iter().cloned().enumerate() {
        let revision = u64::try_from(index + 1).expect("small revision fits u64");
        await_waiting_replan(test.client(), &started.run, revision).await?;
        await_amendment_request_count(&fixture, index + 1).await?;
        assert_applied(
            apply_amendment(test.client(), &started, amendment).await?,
            revision + 1,
        );
    }

    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_replan_stop(
        &terminal,
        ReplanStopReason::BudgetExhausted,
        "budget exhausted: tasks",
        1,
        2,
        7,
    );
    assert_eq!(terminal.run.plan_revision, 6);
    assert_eq!(terminal.run.total_tasks, 7);
    assert_eq!(terminal.run.completed_tasks, 1);
    let tasks = list_execution_tasks(test.client(), started.run.clone()).await?;
    assert_eq!(tasks.tasks.len(), 7);
    assert_eq!(
        tasks
            .tasks
            .iter()
            .filter(|task| task.status == ExecutionTaskStatus::Completed)
            .map(|task| task.node_id.as_str())
            .collect::<Vec<_>>(),
        vec![USEFUL_OUTPUT_NODE]
    );
    assert!(
        tasks
            .tasks
            .iter()
            .filter(|task| task.node_id.starts_with("agent_"))
            .all(|task| task.status == ExecutionTaskStatus::Cancelled)
    );
    let amendment_calls = fixture
        .scripted_requests()?
        .iter()
        .filter(|request| {
            request
                .get("response_format")
                .and_then(|format| format.get("name"))
                .and_then(Value::as_str)
                == Some("generated_amendment_candidate_v1")
        })
        .count();
    assert_eq!(
        amendment_calls, 5,
        "budget exhaustion must stop before a sixth amendment planner call"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn useful_amendment_preserves_completed_work_service_e2e() -> Result<()> {
    // Pins: one valid amendment replaces only pending work and leaves completed output byte-exact.
    let agent_a = agent_node("agent_a", "USEFUL_AMENDMENT_AGENT_A");
    let amendment = replacement_amendment(
        1,
        "agent_a",
        agent_node("agent_b", "USEFUL_AMENDMENT_AGENT_B"),
        "replace only the waiting downstream task",
    );
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        replan_script_with_text_agents(
            &[("useful-amendment-trigger", &amendment)],
            &[
                (
                    "USEFUL_AMENDMENT_AGENT_A",
                    serde_json::to_string(&needs_replan_result("useful-amendment-trigger"))?,
                ),
                (
                    "USEFUL_AMENDMENT_AGENT_B",
                    serde_json::to_string(&json!({"repair": "complete"}))?,
                ),
            ],
        )?,
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "useful-amendment-preserves-work",
        "preserve completed output while replacing pending downstream work",
        None,
        move |_| Ok(useful_replan_contract(agent_a)),
    )
    .await?;

    await_waiting_replan(test.client(), &started.run, 1).await?;
    await_amendment_request_count(&fixture, 1).await?;
    let before_tasks = list_execution_tasks(test.client(), started.run.clone()).await?;
    let completed_before = task_by_node(&before_tasks.tasks, USEFUL_OUTPUT_NODE)?.clone();
    let output_before = completed_output(&completed_before)?.clone();
    let hash_before = task_output_hash(&output_before)?;

    assert_applied(
        apply_amendment(test.client(), &started, amendment.clone()).await?,
        2,
    );
    assert_replayed(
        apply_amendment(test.client(), &started, amendment.clone()).await?,
        2,
    );
    assert_semantic_conflict(test.client(), &started, amendment).await?;

    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_eq!(terminal.run.status, ExecutionRunStatus::Completed);
    assert_eq!(terminal.output, Some(output_before.clone()));
    assert_eq!(
        terminal.run.terminal_evidence,
        Some(ExecutionTerminalEvidence {
            cause: ExecutionTerminalCause::Completion { limit_stop: None },
            satisfied_requirement_count: 2,
            requirement_count: 2,
        })
    );
    assert!(terminal.gaps.is_empty());
    assert_eq!(terminal.run.plan_revision, 2);
    assert_eq!(terminal.run.total_tasks, 3);
    assert_eq!(terminal.run.completed_tasks, 2);
    assert_eq!(terminal.run.budget_ledger.consumed.tasks, 3);

    let after_tasks = list_execution_tasks(test.client(), started.run.clone()).await?;
    let completed_after = task_by_node(&after_tasks.tasks, USEFUL_OUTPUT_NODE)?;
    assert_eq!(completed_after, &completed_before);
    assert_eq!(
        task_output_hash(completed_output(completed_after)?)?,
        hash_before
    );
    assert_eq!(
        task_by_node(&after_tasks.tasks, "agent_a")?.status,
        ExecutionTaskStatus::Cancelled
    );
    assert_eq!(
        task_by_node(&after_tasks.tasks, "agent_b")?.status,
        ExecutionTaskStatus::Completed
    );
    let evidence = synthesis_evidence(test.client(), &started).await?;
    assert!(
        completion_results(&evidence)?
            .iter()
            .all(|result| result.passed),
        "completed amended run retained a failed completion check: {evidence:?}"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn completion_gate_missing_company_service_e2e() -> Result<()> {
    // Pins: 499 completed map items cannot satisfy the declared 500-company universe.
    const TOOL: &str = "screen_sp500_company";
    const MAP_NODE: &str = "sp500_screen";
    const MISSING_COMPANY: &str = "SP500-COMPANY-500";
    let expected_items = (1..=500)
        .map(|index| json!({"ticker": format!("SP500-COMPANY-{index:03}")}))
        .collect::<Vec<_>>();
    let actual_items = expected_items[..499].to_vec();
    let missing_key = extract_map_key(&json!({"ticker": MISSING_COMPANY}), "/ticker")?;
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        default_script(),
        FixtureCapabilityOptions {
            tools: vec![map_tool(TOOL, "/ticker")],
            orchestrator_env: Vec::new(),
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "missing-sp500-company",
        "screen every declared S&P 500 company",
        Some(TOOL),
        move |planning| {
            let reference = capability_reference(planning, TOOL)?;
            Ok(missing_company_contract(
                reference,
                actual_items,
                expected_items,
            ))
        },
    )
    .await?;
    let controller = fixture
        .fixture_capability()
        .context("execution fixture omitted map capability controller")?;
    let calls = controller.wait_for_calls(499, SERVICE_TIMEOUT).await?;
    assert_eq!(calls.len(), 499);
    assert!(calls.iter().all(|call| call.capability == TOOL));
    assert!(calls.iter().all(|call| call.item_key != missing_key));
    controller.release(499);

    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_completion_partial(&terminal, 1, 2, 500);
    assert!(
        terminal
            .gaps
            .contains(&"completion check coverage_sp500 failed".to_string())
    );
    assert!(
        terminal
            .gaps
            .contains(&"coverage coverage_sp500 failed".to_string())
    );
    let evidence = synthesis_evidence(test.client(), &started).await?;
    let check = completion_result(&evidence, "coverage_sp500")?;
    assert!(!check.passed);
    let coverage = check
        .evidence
        .as_array()
        .context("coverage evidence should be an array")?;
    assert_eq!(coverage.len(), 1);
    assert_eq!(coverage[0]["coverage_id"], "coverage_sp500");
    assert_eq!(coverage[0]["map_node_id"], MAP_NODE);
    assert_eq!(coverage[0]["passed"], false);
    assert_eq!(coverage[0]["missing_keys"], json!([missing_key]));
    assert_eq!(coverage[0]["extra_keys"], json!([]));
    assert_eq!(coverage[0]["failed_keys"], json!([]));
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn completion_gate_missing_citation_service_e2e() -> Result<()> {
    // Pins: a completed source task with zero citations remains a failed named gate.
    const TOOL: &str = "read_analyst_note";
    const MAP_NODE: &str = "source_scan";
    const SOURCE_ID: &str = "ANALYST-NOTE-42";
    let source_key = extract_map_key(&json!({"source_id": SOURCE_ID}), "/source_id")?;
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        default_script(),
        FixtureCapabilityOptions {
            tools: vec![map_tool(TOOL, "/source_id")],
            orchestrator_env: Vec::new(),
        },
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "missing-required-citation",
        "read one exact analyst note and preserve its citation",
        Some(TOOL),
        move |planning| {
            let reference = capability_reference(planning, TOOL)?;
            Ok(missing_citation_contract(reference, SOURCE_ID))
        },
    )
    .await?;
    let controller = fixture
        .fixture_capability()
        .context("execution fixture omitted citation capability controller")?;
    let calls = controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].item_key, source_key);
    controller.release(1);

    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_completion_partial(&terminal, 2, 2, 2);
    assert!(
        terminal
            .gaps
            .contains(&"completion check citations_required failed".to_string())
    );
    let evidence = synthesis_evidence(test.client(), &started).await?;
    let check = completion_result(&evidence, "citations_required")?;
    assert!(!check.passed);
    assert_eq!(
        check.evidence,
        json!({
            "insufficient_tasks": [{
                "node_id": MAP_NODE,
                "item_key": source_key,
                "count": 0
            }]
        })
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn completion_gate_missing_deliverable_service_e2e() -> Result<()> {
    // Pins: useful terminal output cannot hide a skipped required node or missing report pointer.
    let fixture = OrchestratorTestFixture::with_execution_fixture(
        default_script(),
        FixtureCapabilityOptions::default(),
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compiled_run(
        &fixture,
        &test,
        "missing-report-deliverable",
        "produce the required report deliverable",
        None,
        |_| Ok(missing_deliverable_contract()),
    )
    .await?;

    let terminal = await_execution_terminal(test.client(), &started.run).await?;
    assert_completion_partial(&terminal, 1, 2, 1);
    assert_eq!(
        terminal.output,
        Some(json!({"summary": "useful but incomplete"}))
    );
    assert!(
        terminal
            .gaps
            .contains(&"completion check deliverable_report failed".to_string())
    );
    assert!(
        terminal
            .gaps
            .contains(&"deliverable report is missing".to_string())
    );
    let evidence = synthesis_evidence(test.client(), &started).await?;
    let check = completion_result(&evidence, "deliverable_report")?;
    assert!(!check.passed);
    assert_eq!(
        check.evidence,
        json!({"incomplete_node_ids": ["report_builder"]})
    );
    Ok(())
}

async fn start_compiled_run<F>(
    fixture: &OrchestratorTestFixture,
    test: &IsolatedTest<'_>,
    label: &str,
    objective: &str,
    allowed_tool: Option<&str>,
    build: F,
) -> Result<StartedExecution>
where
    F: FnOnce(
        &ExecutionPlanningContextResponse,
    ) -> Result<(ExecutionGoalContract, ExecutionPlanDefinition)>,
{
    let session_id = test.create_session(label).await?;
    let session = test.client().get_session(session_id).await?;
    if let Some(tool_name) = allowed_tool {
        seed_allow_policy(fixture, test.client(), session.tenant_id, tool_name).await?;
    }
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
                requested_template: None,
            },
        )
        .await?;
    let (goal, plan) = build(&planning)?;
    let compiled = compile(CompileExecutionRequest {
        goal,
        plan,
        run_input: run_input_for_objective(objective),
        catalog: planning.snapshot.catalog.clone(),
        authorization: planning.snapshot.authorization.clone(),
        approved_budget: planning.snapshot.budget.clone(),
        config: ExecutionConfig::default(),
        now: chrono::Utc::now(),
    })
    .compiled
    .with_context(|| format!("compile deterministic scenario `{label}`"))?;
    let source_provenance = crate::test_source_provenance(&compiled.plan.plan_hash.to_string());
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
                idempotency_key: Some(format!("task10-{label}-{session_id}")),
                compiled,
                run_input: run_input_for_objective(objective),
                source_provenance,
            },
        )
        .await
        .with_context(|| format!("start deterministic scenario `{label}`"))?;
    assert!(started.created);
    assert!(!started.confirmation_required);
    let run = ExecutionRunRequest {
        tenant_id: session.tenant_id,
        contact_id: None,
        session_id,
        run_uid: started.run.run_uid,
    };
    Ok(StartedExecution {
        originating_user_sequence_num,
        run,
    })
}

fn run_input_for_objective(objective: &str) -> Value {
    if objective == "produce the required report deliverable" {
        json!({"build_report": false})
    } else {
        json!({})
    }
}

async fn await_waiting_replan(
    client: &TestApiClient,
    run: &ExecutionRunRequest,
    revision: u64,
) -> Result<ExecutionTaskProjection> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let status: ExecutionStatusResponse = client.post_call("/Execution/status", run).await?;
        let tasks = list_execution_tasks(client, run.clone()).await?;
        let waiting = tasks
            .tasks
            .iter()
            .filter(|task| task.status == ExecutionTaskStatus::WaitingReplan)
            .cloned()
            .collect::<Vec<_>>();
        if status.run.status == ExecutionRunStatus::WaitingReplan
            && status.run.plan_revision == revision
            && let [task] = waiting.as_slice()
        {
            return Ok(task.clone());
        }
        if status.run.status.is_terminal() {
            bail!(
                "run {} became terminal before WaitingReplan revision {revision}: {status:?}",
                run.run_uid
            );
        }
        let last_status = (status.run.status, status.run.plan_revision, waiting.len());
        if Instant::now() >= deadline {
            bail!(
                "run {} did not reach WaitingReplan revision {revision} within {SERVICE_TIMEOUT:?}; last={last_status:?}",
                run.run_uid
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn await_amendment_request_count(
    fixture: &OrchestratorTestFixture,
    expected: usize,
) -> Result<()> {
    let deadline = Instant::now() + SERVICE_TIMEOUT;
    loop {
        let count = fixture
            .scripted_requests()?
            .iter()
            .filter(|request| {
                request
                    .get("response_format")
                    .and_then(|format| format.get("name"))
                    .and_then(Value::as_str)
                    == Some("generated_amendment_candidate_v1")
            })
            .count();
        if count >= expected {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!(
                "workflow journaled {count} of {expected} expected amendment requests within {SERVICE_TIMEOUT:?}"
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn apply_amendment(
    client: &TestApiClient,
    started: &StartedExecution,
    amendment: PlanAmendment,
) -> Result<ExecutionMutationResponse> {
    client
        .post_call(
            "/Execution/apply_amendment",
            &ExecutionAmendmentRequest {
                run: started.run.clone(),
                expected_plan_revision: amendment.base_plan_revision,
                amendment,
            },
        )
        .await
        .context("apply deterministic service amendment")
}

async fn assert_semantic_conflict(
    client: &TestApiClient,
    started: &StartedExecution,
    mut amendment: PlanAmendment,
) -> Result<()> {
    amendment.reason.push_str(" with changed semantics");
    assert_eq!(
        apply_amendment(client, started, amendment).await?,
        ExecutionMutationResponse::Conflict {
            reason: ExecutionConflictReason::PlanRevisionMismatch,
        }
    );
    Ok(())
}

async fn synthesis_evidence(
    client: &TestApiClient,
    started: &StartedExecution,
) -> Result<ExecutionSynthesisEvidence> {
    client
        .post_call(
            "/Execution/synthesis_evidence",
            &ExecutionSynthesisEvidenceRequest {
                run: started.run.clone(),
                originating_user_sequence_num: started.originating_user_sequence_num,
            },
        )
        .await
        .context("load terminal synthesis evidence")
}

fn assert_applied(response: ExecutionMutationResponse, plan_revision: u64) {
    let ExecutionMutationResponse::Applied { run } = response else {
        panic!("expected applied amendment, got {response:?}")
    };
    assert_eq!(run.plan_revision, plan_revision);
}

fn assert_replayed(response: ExecutionMutationResponse, plan_revision: u64) {
    let ExecutionMutationResponse::Replayed { run } = response else {
        panic!("expected replayed amendment, got {response:?}")
    };
    assert_eq!(run.plan_revision, plan_revision);
}

fn assert_replan_stop(
    status: &ExecutionStatusResponse,
    reason: ReplanStopReason,
    detail: &str,
    satisfied_requirement_count: u64,
    requirement_count: u64,
    consumed_tasks: u64,
) {
    assert_eq!(status.run.status, ExecutionRunStatus::Partial);
    assert_eq!(status.output, Some(json!({"result": USEFUL_OUTPUT})));
    assert_eq!(
        status.run.terminal_evidence,
        Some(ExecutionTerminalEvidence {
            cause: ExecutionTerminalCause::ReplanStop { reason },
            satisfied_requirement_count,
            requirement_count,
        })
    );
    assert_eq!(status.run.budget_ledger.consumed.tasks, consumed_tasks);
    assert_eq!(status.run.budget_ledger.reserved.tasks, 0);
    assert_eq!(status.run.total_tasks, consumed_tasks);
    assert_eq!(status.run.completed_tasks, 1);
    assert!(
        status
            .gaps
            .contains(&format!("replan stop reason: {}", reason.as_str())),
        "typed replan reason gap missing: {:?}",
        status.gaps
    );
    assert!(
        status.gaps.contains(&format!("replan stopped: {detail}")),
        "exact replan detail gap missing: {:?}",
        status.gaps
    );
}

fn assert_completion_partial(
    status: &ExecutionStatusResponse,
    satisfied_requirement_count: u64,
    requirement_count: u64,
    consumed_tasks: u64,
) {
    assert_eq!(status.run.status, ExecutionRunStatus::Partial);
    assert_eq!(
        status.run.terminal_evidence,
        Some(ExecutionTerminalEvidence {
            cause: ExecutionTerminalCause::Completion { limit_stop: None },
            satisfied_requirement_count,
            requirement_count,
        })
    );
    assert_eq!(status.run.budget_ledger.consumed.tasks, consumed_tasks);
    assert_eq!(status.run.budget_ledger.reserved.tasks, 0);
}

fn useful_replan_contract(
    initial_agent: ExecutionNode,
) -> (ExecutionGoalContract, ExecutionPlanDefinition) {
    let output_schema = useful_output_schema();
    (
        ExecutionGoalContract {
            objective: "preserve useful output while repairing downstream work".to_string(),
            requirements: vec![
                requirement(USEFUL_OUTPUT_REQUIREMENT, "preserve one useful result"),
                requirement(REPAIR_REQUIREMENT, "complete repaired downstream work"),
            ],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "useful_output_schema".to_string(),
                description: "the useful output retains its exact schema".to_string(),
                requirement_ids: vec![USEFUL_OUTPUT_REQUIREMENT.to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: empty_input_schema(),
            output_schema: output_schema.clone(),
            nodes: vec![
                ExecutionNode {
                    id: USEFUL_OUTPUT_NODE.to_string(),
                    requirement_ids: vec![USEFUL_OUTPUT_REQUIREMENT.to_string()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({}),
                    output_schema,
                    operation: ExecutionOperation::Output {
                        value: json!({"result": USEFUL_OUTPUT}),
                    },
                    retry: no_retry(),
                    budget: None,
                },
                initial_agent,
            ],
        },
    )
}

fn agent_node(id: &str, instructions: &str) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec![REPAIR_REQUIREMENT.to_string()],
        depends_on: vec![USEFUL_OUTPUT_NODE.to_string()],
        when: None,
        input: json!({}),
        output_schema: json!({"type": "object"}),
        operation: ExecutionOperation::Agent {
            instructions: instructions.to_string(),
            skill_refs: Vec::new(),
            capability_refs: Vec::new(),
            max_turns: 1,
        },
        retry: no_retry(),
        budget: None,
    }
}

fn replacement_amendment(
    base_plan_revision: u64,
    old_node_id: &str,
    replacement: ExecutionNode,
    reason: &str,
) -> PlanAmendment {
    PlanAmendment {
        schema_version: 1,
        base_plan_revision,
        reason: reason.to_string(),
        evidence: json!({"replacement": replacement.id}),
        operations: vec![
            PlanAmendmentOperation::RemovePendingNode {
                node_id: old_node_id.to_string(),
            },
            PlanAmendmentOperation::AddNode { node: replacement },
        ],
    }
}

fn missing_company_contract(
    reference: CapabilityReference,
    actual_items: Vec<Value>,
    expected_items: Vec<Value>,
) -> (ExecutionGoalContract, ExecutionPlanDefinition) {
    const MAP_NODE: &str = "sp500_screen";
    let output_schema = report_schema();
    (
        ExecutionGoalContract {
            objective: "screen every declared S&P 500 company".to_string(),
            requirements: vec![
                requirement("coverage", "screen the complete company universe"),
                requirement("report", "produce a useful screening report"),
            ],
            deliverables: Vec::new(),
            coverage: vec![CoverageRequirement {
                id: "coverage_sp500".to_string(),
                description: "cover every canonical S&P 500 company key".to_string(),
                map_node_id: MAP_NODE.to_string(),
                expected_items: Value::Array(expected_items),
                require_all: true,
            }],
            constraints: Vec::new(),
            completion_checks: vec![
                CompletionCheck {
                    id: "coverage_sp500".to_string(),
                    description: "all 500 canonical company keys completed".to_string(),
                    requirement_ids: vec!["coverage".to_string()],
                    constraint_ids: Vec::new(),
                    kind: CompletionCheckKind::MapCoverage {
                        map_node_id: MAP_NODE.to_string(),
                    },
                },
                output_schema_check("report_schema", "report"),
            ],
        },
        map_then_output_plan(MapThenOutputPlan {
            map_node_id: MAP_NODE,
            map_requirement_id: "coverage",
            reference,
            items: Value::Array(actual_items),
            item_key: "/ticker",
            max_items: 500,
            output_requirement_id: "report",
            output: json!({"summary": "499 companies screened"}),
            output_schema,
        }),
    )
}

fn missing_citation_contract(
    reference: CapabilityReference,
    source_id: &str,
) -> (ExecutionGoalContract, ExecutionPlanDefinition) {
    const MAP_NODE: &str = "source_scan";
    let output_schema = report_schema();
    (
        ExecutionGoalContract {
            objective: "read one exact analyst note and preserve its citation".to_string(),
            requirements: vec![
                requirement("source", "read the exact analyst-note source"),
                requirement("report", "produce a useful source report"),
            ],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![
                CompletionCheck {
                    id: "citations_required".to_string(),
                    description: "every source task records one citation".to_string(),
                    requirement_ids: vec!["source".to_string()],
                    constraint_ids: Vec::new(),
                    kind: CompletionCheckKind::Citations {
                        node_ids: vec![MAP_NODE.to_string()],
                        min_per_task: 1,
                    },
                },
                output_schema_check("report_schema", "report"),
            ],
        },
        map_then_output_plan(MapThenOutputPlan {
            map_node_id: MAP_NODE,
            map_requirement_id: "source",
            reference,
            items: json!([{"source_id": source_id}]),
            item_key: "/source_id",
            max_items: 1,
            output_requirement_id: "report",
            output: json!({"summary": "source read without a citation"}),
            output_schema,
        }),
    )
}

fn map_then_output_plan(spec: MapThenOutputPlan<'_>) -> ExecutionPlanDefinition {
    let MapThenOutputPlan {
        map_node_id,
        map_requirement_id,
        reference,
        items,
        item_key,
        max_items,
        output_requirement_id,
        output,
        output_schema,
    } = spec;
    ExecutionPlanDefinition {
        schema_version: 1,
        input_schema: empty_input_schema(),
        output_schema: output_schema.clone(),
        nodes: vec![
            ExecutionNode {
                id: map_node_id.to_string(),
                requirement_ids: vec![map_requirement_id.to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({"$item": true}),
                output_schema: json!({"type": "object"}),
                operation: ExecutionOperation::Map {
                    items,
                    item_key: item_key.to_string(),
                    max_items,
                    item_output_schema: json!({"type": "object"}),
                    task: MapTask::Capability { reference },
                },
                retry: no_retry(),
                budget: None,
            },
            ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec![output_requirement_id.to_string()],
                depends_on: vec![map_node_id.to_string()],
                when: None,
                input: json!({}),
                output_schema,
                operation: ExecutionOperation::Output { value: output },
                retry: no_retry(),
                budget: None,
            },
        ],
    }
}

fn missing_deliverable_contract() -> (ExecutionGoalContract, ExecutionPlanDefinition) {
    let output_schema = report_schema();
    (
        ExecutionGoalContract {
            objective: "produce the required report deliverable".to_string(),
            requirements: vec![
                requirement("report_body", "build the required report body"),
                requirement("summary", "preserve a useful summary"),
            ],
            deliverables: vec![ExecutionDeliverable {
                id: "report".to_string(),
                description: "the final structured report".to_string(),
                output_pointer: "/report".to_string(),
                schema: json!({"type": "object"}),
            }],
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![
                CompletionCheck {
                    id: "deliverable_report".to_string(),
                    description: "the report builder completed".to_string(),
                    requirement_ids: vec!["report_body".to_string()],
                    constraint_ids: Vec::new(),
                    kind: CompletionCheckKind::RequiredNodes {
                        node_ids: vec!["report_builder".to_string()],
                    },
                },
                output_schema_check("summary_schema", "summary"),
            ],
        },
        ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["build_report"],
                "properties": {"build_report": {"type": "boolean"}}
            }),
            output_schema: output_schema.clone(),
            nodes: vec![
                ExecutionNode {
                    id: "report_builder".to_string(),
                    requirement_ids: vec!["report_body".to_string()],
                    depends_on: Vec::new(),
                    when: Some(moa_artifacts::execution_plan::ExecutionCondition::Equals {
                        reference: ExecutionReference {
                            path: "$.input.build_report".to_string(),
                        },
                        value: json!(true),
                    }),
                    input: json!({}),
                    output_schema: json!({"type": "object"}),
                    operation: ExecutionOperation::Agent {
                        instructions: "BUILD_REPORT_ONLY_WHEN_ENABLED".to_string(),
                        skill_refs: Vec::new(),
                        capability_refs: Vec::new(),
                        max_turns: 1,
                    },
                    retry: no_retry(),
                    budget: None,
                },
                ExecutionNode {
                    id: "output".to_string(),
                    requirement_ids: vec!["summary".to_string()],
                    depends_on: vec!["report_builder".to_string()],
                    when: None,
                    input: json!({}),
                    output_schema,
                    operation: ExecutionOperation::Output {
                        value: json!({"summary": "useful but incomplete"}),
                    },
                    retry: no_retry(),
                    budget: None,
                },
            ],
        },
    )
}

fn replan_script(
    amendments: &[(&str, &PlanAmendment)],
    agents: &[(&str, ExecutionTaskResult)],
) -> Result<Value> {
    let agents = agents
        .iter()
        .map(|(instruction, result)| Ok((*instruction, serde_json::to_string(result)?)))
        .collect::<Result<Vec<_>>>()?;
    replan_script_with_text_agents(amendments, &agents)
}

fn replan_script_with_text_agents(
    amendments: &[(&str, &PlanAmendment)],
    agents: &[(&str, String)],
) -> Result<Value> {
    let mut keyed = vec![keyed_completion(
        SYNTHESIS_MATCH,
        text_completion(SCRIPTED_SYNTHESIS),
    )];
    for (failure_reason, amendment) in amendments {
        let candidate = GeneratedAmendmentCandidate {
            amendment: (*amendment).clone(),
        };
        keyed.push(keyed_completion(
            failure_reason,
            delayed_text_completion(serde_json::to_string(&candidate)?),
        ));
    }
    for (instruction, result) in agents {
        keyed.push(keyed_completion(instruction, text_completion(result)));
    }
    Ok(json!({
        "default": text_completion(SCRIPTED_SYNTHESIS),
        "keyed": keyed,
    }))
}

fn default_script() -> Value {
    json!({"default": text_completion(SCRIPTED_SYNTHESIS)})
}

fn text_completion(content: impl Into<String>) -> Value {
    json!({"content": content.into(), "tool_calls": []})
}

fn delayed_text_completion(content: impl Into<String>) -> Value {
    json!({
        "content": content.into(),
        "tool_calls": [],
        "latency_ms": AMENDMENT_DELAY_MS,
        "ttft_ms": AMENDMENT_DELAY_MS,
    })
}

fn keyed_completion(match_substring: &str, completion: Value) -> Value {
    json!({"match": match_substring, "completion": completion})
}

fn needs_replan_result(reason: &str) -> ExecutionTaskResult {
    ExecutionTaskResult::NeedsReplan {
        reason: reason.to_string(),
        evidence: json!({"reason": reason, "source": "task-local-scripted-agent"}),
    }
}

fn capability_reference(
    planning: &ExecutionPlanningContextResponse,
    name: &str,
) -> Result<CapabilityReference> {
    planning
        .snapshot
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference.name == name)
        .map(|capability| capability.reference.clone())
        .with_context(|| format!("planning catalog omitted fixture capability `{name}`"))
}

fn map_tool(name: &str, item_key_pointer: &str) -> FixtureCapabilityTool {
    let field = item_key_pointer.trim_start_matches('/');
    FixtureCapabilityTool {
        name: name.to_string(),
        description: format!("Deterministically process one {field}"),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": [field],
            "properties": {(field): {"type": "string"}}
        }),
        item_key_pointer: Some(item_key_pointer.to_string()),
        outcomes: vec![FixtureCapabilityOutcome::SuccessWithInput {
            output: json!({"processed": true}),
        }],
    }
}

fn completion_results(evidence: &ExecutionSynthesisEvidence) -> Result<Vec<CompletionCheckResult>> {
    evidence
        .completion_check_results
        .iter()
        .cloned()
        .map(|value| serde_json::from_value(value).context("decode completion-check evidence"))
        .collect()
}

fn completion_result(
    evidence: &ExecutionSynthesisEvidence,
    check_id: &str,
) -> Result<CompletionCheckResult> {
    completion_results(evidence)?
        .into_iter()
        .find(|result| result.check_id == check_id)
        .with_context(|| format!("synthesis evidence omitted completion check `{check_id}`"))
}

fn completed_output(task: &ExecutionTaskProjection) -> Result<&Value> {
    let Some(outcome) = task.outcome.as_ref() else {
        bail!("completed task {} omitted its outcome", task.task_id);
    };
    let ExecutionTaskResult::Completed { output, .. } = &outcome.result else {
        bail!("task {} did not retain a completed output", task.task_id);
    };
    Ok(output)
}

fn task_by_node<'a>(
    tasks: &'a [ExecutionTaskProjection],
    node_id: &str,
) -> Result<&'a ExecutionTaskProjection> {
    let matches = tasks
        .iter()
        .filter(|task| task.node_id == node_id)
        .collect::<Vec<_>>();
    let [task] = matches.as_slice() else {
        bail!(
            "expected one task for node `{node_id}`, found {}",
            matches.len()
        );
    };
    Ok(task)
}

fn requirement(id: &str, description: &str) -> ExecutionRequirement {
    ExecutionRequirement {
        id: id.to_string(),
        description: description.to_string(),
    }
}

fn output_schema_check(id: &str, requirement_id: &str) -> CompletionCheck {
    CompletionCheck {
        id: id.to_string(),
        description: "terminal output matches its declared schema".to_string(),
        requirement_ids: vec![requirement_id.to_string()],
        constraint_ids: Vec::new(),
        kind: CompletionCheckKind::OutputSchema,
    }
}

fn empty_input_schema() -> Value {
    json!({"type": "object", "additionalProperties": false})
}

fn useful_output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["result"],
        "properties": {"result": {"const": USEFUL_OUTPUT}}
    })
}

fn report_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["summary"],
        "properties": {"summary": {"type": "string"}}
    })
}

fn no_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    }
}
