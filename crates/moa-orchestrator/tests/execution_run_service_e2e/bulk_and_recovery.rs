//! Deterministic bulk-map and orchestrator-recovery service scenarios.

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, CoverageRequirement,
    ExecutionGoalContract, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
    ExecutionReducer, ExecutionRequirement, ExecutionTaskResult, GeneratedExecutionCandidate,
    MapTask, RetryPolicy,
};
use moa_core::config::ToolBudgetConfig;
use moa_core::events::{Event, ExecutionTaskResultsRef};
use moa_core::types::action_policy::{ActionClass, ActionPolicyEffect, RiskLevel};
use moa_core::types::session::SessionStatus;
use moa_core::types::tools::{IdempotencyClass, ToolDiffStrategy, ToolInputShape, ToolPolicySpec};
use moa_core::wire::turn::TurnOutcomeKind;
use moa_eval::execution::ExecutionInvariantSpec;
use moa_execution::bindings::extract_map_key;
use moa_execution::capability::{ExecutionEstimate, capability_version};
use moa_execution::state::{ExecutionTaskId, ExecutionTaskProjection, ExecutionTaskStatus};
use moa_execution::wire::{
    ExecutionRunRequest, ExecutionStatusResponse, ExecutionTaskListRequest,
    ExecutionTaskListResponse,
};
use moa_test_support::{
    FixtureCapabilityAttempt, FixtureCapabilityCall, FixtureCapabilityOptions,
    FixtureCapabilityOutcome, FixtureCapabilityTool, OrchestratorTestFixture,
};
use serde_json::{Value, json};
use sqlx::postgres::PgListener;
use sqlx::{Connection, PgConnection, PgPool};
use tokio::time::Instant;

use crate::evaluation::assert_execution_eval_case;
use crate::execution_execution_support::assertions::{
    JournalRequestRole, assert_completed_terminal, assert_generated_plan_audits,
    assert_initial_route, final_brain_response, journal_requests, journal_roles, planning_audits,
};
use crate::execution_execution_support::fixtures::{
    POLL_INTERVAL, RouteFixture, await_execution_terminal_with_timeout, await_run_started_event,
    await_session_settled, await_turn_outcome, execution_run_request, raw_events,
    route_classifier_completion, seed_allow_policy, start_turn_in_session,
};
use moa_core::types::execution_planning::{
    ExecutionPlanningAuditPayload, ExecutionRouteKind, ExecutionStrategy,
};

const COMPANY_COUNT: usize = 500;
const PARTIAL_COMPLETION_COUNT: usize = 137;
const BULK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
const MAP_NODE_ID: &str = "screen_companies";
const REDUCE_NODE_ID: &str = "aggregate_mentions";
const OUTPUT_NODE_ID: &str = "report";
const MAP_TOOL_NAME: &str = "fixture_screen_company";
const REDUCE_TOOL_NAME: &str = "fixture_reduce_company_mentions";
const FIXTURE_MCP_SERVER_NAME: &str = "fixture-capability";
const PLANNER_MATCH: &str = "<frozen_planning_context>";
const SYNTHESIS_MATCH: &str = "Synthesize the final user response for execution run";
const OBJECTIVE: &str =
    "Start an execution run to screen all 500 S&P-like companies and report AI mentions";
const RAW_MAP_SENTINEL: &str = "RAW_MAP_COMPANY_PAYLOAD_MUST_STAY_TASK_LOCAL";
const FINAL_RESPONSE: &str = "The bounded aggregate reports 500 AI mentions across 500 companies.";

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn bulk_500_map_exact_coverage_service_e2e() -> Result<()> {
    // Pins: generated production capability discovery executes every one of 500 stable map
    // items, one reducer, and one output without leaking raw item payloads into root context.
    let fixture = bulk_fixture().await?;
    let test = fixture.isolated().await;

    let observed = run_bulk_scenario(&fixture, &test, "bulk-500", RecoveryPoint::None).await?;

    assert_eq!(observed.map_transport_attempts, COMPANY_COUNT);
    assert_eq!(observed.reducer_transport_attempts, 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn recovery_after_materialization_is_exactly_once_service_e2e() -> Result<()> {
    // Pins: replaying the run driver after all map rows commit cannot rematerialize or double
    // account any stable task, including if Restate has not journaled the completed operation.
    let fixture = bulk_fixture().await?;
    let test = fixture.isolated().await;

    let baseline = run_bulk_scenario(
        &fixture,
        &test,
        "materialization-baseline",
        RecoveryPoint::None,
    )
    .await?;
    let observed = run_bulk_scenario(
        &fixture,
        &test,
        "recovery-materialized",
        RecoveryPoint::AfterMaterialization,
    )
    .await?;

    assert_eq!(
        observed.canonical_final, baseline.canonical_final,
        "materialization recovery changed final usage, progress, terminal evidence, or output"
    );
    assert!(
        observed.map_transport_attempts >= COMPANY_COUNT,
        "recovery lost map transport attempts: {observed:?}"
    );
    assert!(observed.reducer_transport_attempts >= 1);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn recovery_after_137_completions_is_exactly_once_service_e2e() -> Result<()> {
    // Pins: a child-only restart at exactly 137 durable map completions preserves completed work,
    // resumes every pending effect, and yields the same final ledger/progress as no restart.
    let fixture = bulk_fixture().await?;
    let test = fixture.isolated().await;

    let baseline =
        run_bulk_scenario(&fixture, &test, "recovery-baseline", RecoveryPoint::None).await?;
    let recovered = run_bulk_scenario(
        &fixture,
        &test,
        "recovery-137",
        RecoveryPoint::AfterCompletions(PARTIAL_COMPLETION_COUNT),
    )
    .await?;

    assert_eq!(
        recovered.canonical_final, baseline.canonical_final,
        "partial recovery changed final usage, progress, terminal evidence, or output"
    );
    assert!(recovered.map_transport_attempts >= COMPANY_COUNT);
    assert!(recovered.reducer_transport_attempts >= 1);
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum RecoveryPoint {
    None,
    AfterMaterialization,
    AfterCompletions(usize),
}

#[derive(Debug)]
struct BulkObservation {
    canonical_final: Value,
    map_transport_attempts: usize,
    reducer_transport_attempts: usize,
}

async fn bulk_fixture() -> Result<OrchestratorTestFixture> {
    let map_tool = map_tool();
    let reducer_tool = reducer_tool();
    let candidate = bulk_candidate(&map_tool, &reducer_tool)?;
    OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": text_completion("unexpected scripted-provider fallback"),
            "keyed": [
                route_classifier_completion(
                    ExecutionRouteKind::Execute,
                    RouteFixture::Durable,
                ),
                keyed_completion(SYNTHESIS_MATCH, text_completion(FINAL_RESPONSE)),
                keyed_completion(
                    PLANNER_MATCH,
                    text_completion(serde_json::to_string(&candidate)?)
                )
            ]
        }),
        FixtureCapabilityOptions {
            tools: vec![map_tool, reducer_tool],
            orchestrator_env: vec![
                ("MOA_DATABASE_MAX_CONNECTIONS".to_string(), "80".to_string()),
                ("RUST_LOG".to_string(), "error".to_string()),
            ],
        },
    )
    .await
}

async fn run_bulk_scenario(
    fixture: &OrchestratorTestFixture,
    test: &moa_test_support::IsolatedTest<'_>,
    label: &str,
    recovery: RecoveryPoint,
) -> Result<BulkObservation> {
    let controller = fixture
        .fixture_capability()
        .context("execution fixture omitted capability controller")?;
    controller.reset();
    fixture
        .reset_scripted_requests()
        .context("reset scripted request journal before bulk scenario")?;
    let pool = PgPool::connect(&fixture.postgres_url)
        .await
        .context("connect to retained fixture Postgres")?;
    let mut materialization_signal = if matches!(recovery, RecoveryPoint::AfterMaterialization) {
        Some(MaterializationSignal::install(&fixture.postgres_url, &pool).await?)
    } else {
        None
    };

    let session_id = test.create_session(label).await?;
    let session = test.client().get_session(session_id).await?;
    seed_allow_policy(fixture, test.client(), session.tenant_id, MAP_TOOL_NAME).await?;
    seed_allow_policy(fixture, test.client(), session.tenant_id, REDUCE_TOOL_NAME).await?;
    let started = start_turn_in_session(test, session_id, OBJECTIVE, None).await?;
    let outcome = await_turn_outcome(test.client(), &started).await?;
    let TurnOutcomeKind::Accepted { execution_run_uid } = outcome.kind else {
        let validation_reports = planning_audits(&fixture.postgres_url, session_id)
            .await?
            .into_iter()
            .filter_map(|audit| match audit.payload {
                ExecutionPlanningAuditPayload::Compile {
                    validation_report, ..
                } => Some(validation_report),
                ExecutionPlanningAuditPayload::Route { .. }
                | ExecutionPlanningAuditPayload::PlannerCall { .. } => None,
            })
            .collect::<Vec<_>>();
        let planner_capabilities =
            planner_capabilities(&journal_requests(fixture.scripted_requests()?)?)?;
        bail!(
            "bulk generated turn did not admit a run: {outcome:?}; \
             planner fixture capabilities: {planner_capabilities:#?}; \
             compiler validation reports: {validation_reports:#?}"
        );
    };
    await_run_started_event(test.client(), session_id, execution_run_uid).await?;
    let run_request = execution_run_request(&started, execution_run_uid);

    match recovery {
        RecoveryPoint::None => {
            wait_for_map_calls(fixture, controller, &pool, execution_run_uid, COMPANY_COUNT)
                .await?;
            controller.release(COMPANY_COUNT);
        }
        RecoveryPoint::AfterMaterialization => {
            let signal = materialization_signal
                .as_mut()
                .context("materialization recovery omitted its commit signal")?;
            signal.wait_for_run(&pool, execution_run_uid).await?;
            assert!(
                controller.calls().is_empty(),
                "materialization checkpoint raced into logical capability dispatch: {:#?}",
                controller.calls()
            );
            assert!(
                controller.transport_attempts().is_empty(),
                "materialization checkpoint raced into MCP transport dispatch: {:#?}",
                controller.transport_attempts()
            );
            fixture
                .restart_orchestrator()
                .await
                .context("restart child after exact map materialization")?;
            signal.release_and_remove(&pool).await?;
            wait_for_map_calls(fixture, controller, &pool, execution_run_uid, COMPANY_COUNT)
                .await?;
            controller.release(COMPANY_COUNT);
        }
        RecoveryPoint::AfterCompletions(completed) => {
            if completed >= COMPANY_COUNT {
                bail!("partial recovery checkpoint must be below {COMPANY_COUNT}");
            }
            wait_for_map_calls(fixture, controller, &pool, execution_run_uid, COMPANY_COUNT)
                .await?;
            controller.release(completed);
            wait_for_completed_map_count(&pool, execution_run_uid, completed).await?;
            fixture
                .restart_orchestrator()
                .await
                .with_context(|| format!("restart child after exactly {completed} completions"))?;
            controller.release(COMPANY_COUNT - completed);
        }
    }

    let calls = match controller
        .wait_for_calls(COMPANY_COUNT + 1, BULK_TIMEOUT)
        .await
    {
        Ok(calls) => calls,
        Err(error) => {
            let task_statuses = task_status_diagnostics(&pool, execution_run_uid).await?;
            let process_exit = fixture.unexpected_orchestrator_exit().await?;
            bail!(
                "wait for 500 map effects and sole reducer effect: {error:#}; \
                 task statuses: {task_statuses}; orchestrator exit: {process_exit:#?}"
            );
        }
    };
    assert_call_partition(&calls)?;
    controller.release(1);

    let terminal =
        await_execution_terminal_with_timeout(test.client(), &run_request, BULK_TIMEOUT).await?;
    assert_eq!(
        await_session_settled(test.client(), session_id).await?,
        SessionStatus::Paused
    );
    let tasks = list_all_tasks(test.client(), run_request.clone()).await?;
    let attempts = controller.transport_attempts();
    let events = raw_events(test.client(), session_id).await?;
    let requests = journal_requests(fixture.scripted_requests()?)?;

    assert_bulk_terminal(&terminal, &tasks);
    assert_bulk_task_identity_and_outcomes(execution_run_uid, &tasks, &calls, controller)?;
    assert_bulk_audit_counts(&pool, execution_run_uid).await?;
    assert_bounded_session_and_context(&pool, session_id, &terminal, &events, &requests).await?;

    let audits = planning_audits(&fixture.postgres_url, session_id).await?;
    assert_initial_route(
        &audits,
        ExecutionRouteKind::Execute,
        Some(ExecutionStrategy::Durable),
        RouteFixture::Durable,
    );
    assert_generated_plan_audits(&audits);
    assert_eq!(final_brain_response(&events)?, FINAL_RESPONSE);
    assert_eq!(
        journal_roles(&requests),
        vec![
            JournalRequestRole::Normal,
            JournalRequestRole::InitialPlanner,
            JournalRequestRole::Synthesis
        ],
        "bulk no-verifier run must make exactly one planner and one synthesis request"
    );

    let map_transport_attempts = attempts
        .iter()
        .filter(|attempt| attempt.capability == MAP_TOOL_NAME)
        .count();
    let reducer_transport_attempts = attempts
        .iter()
        .filter(|attempt| attempt.capability == REDUCE_TOOL_NAME)
        .count();
    assert_transport_attempts(&attempts);

    let expected_keys = companies()
        .iter()
        .map(|company| extract_map_key(company, "/company"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_execution_eval_case(
        fixture,
        test.client(),
        &run_request,
        Some(controller),
        label,
        &[
            ExecutionInvariantSpec::TerminalStatusIn {
                statuses: vec![moa_execution::state::ExecutionRunStatus::Completed],
            },
            ExecutionInvariantSpec::TaskCount {
                node_id: MAP_NODE_ID.to_string(),
                exact: COMPANY_COUNT as u64,
            },
            ExecutionInvariantSpec::TaskCount {
                node_id: REDUCE_NODE_ID.to_string(),
                exact: 1,
            },
            ExecutionInvariantSpec::MapCoverage {
                node_id: MAP_NODE_ID.to_string(),
                expected_keys,
                require_all_when_completed: true,
            },
            ExecutionInvariantSpec::CompletionCheckPassed {
                check_id: "coverage_complete".to_string(),
            },
            ExecutionInvariantSpec::BudgetWithinApproved,
            ExecutionInvariantSpec::ProgressMatchesTasks,
            ExecutionInvariantSpec::NoDuplicateLogicalEffects,
            ExecutionInvariantSpec::AllowedCapabilitiesOnly {
                references: vec![
                    fixture_capability_reference(&map_tool())?,
                    fixture_capability_reference(&reducer_tool())?,
                ],
            },
            ExecutionInvariantSpec::SessionEventCountAtMost {
                event_kind: "progress".to_string(),
                max: 20,
            },
            ExecutionInvariantSpec::NoRawTaskOutputEvents,
        ],
    )
    .await?;

    Ok(BulkObservation {
        canonical_final: load_canonical_final_projection(&pool, execution_run_uid).await?,
        map_transport_attempts,
        reducer_transport_attempts,
    })
}

const MATERIALIZATION_CHANNEL: &str = "moa_bulk_map_materialized_service_e2e";

struct MaterializationSignal {
    listener: PgListener,
    reservation_lock: PgConnection,
    reservation_lock_key: i64,
}

impl MaterializationSignal {
    async fn install(database_url: &str, pool: &PgPool) -> Result<Self> {
        let mut listener = PgListener::connect(database_url)
            .await
            .context("connect materialization LISTEN client")?;
        listener
            .listen(MATERIALIZATION_CHANNEL)
            .await
            .context("listen for map materialization commit")?;
        let lock_uuid = uuid::Uuid::new_v4();
        let reservation_lock_key = i64::from_be_bytes(
            lock_uuid.as_bytes()[..8]
                .try_into()
                .context("derive unique materialization reservation barrier key")?,
        );
        let mut reservation_lock = PgConnection::connect(database_url)
            .await
            .context("connect materialization reservation barrier")?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(reservation_lock_key)
            .execute(&mut reservation_lock)
            .await
            .context("hold map reservation barrier before execution admission")?;
        let barrier_ddl = format!(
            r#"
            CREATE OR REPLACE FUNCTION moa.notify_bulk_map_materialized_service_e2e()
            RETURNS TRIGGER
            LANGUAGE plpgsql
            AS $$
            BEGIN
                IF NEW.node_id = 'screen_companies' THEN
                    PERFORM pg_notify(
                        'moa_bulk_map_materialized_service_e2e',
                        NEW.run_uid::TEXT
                    );
                END IF;
                RETURN NEW;
            END;
            $$;
            DROP TRIGGER IF EXISTS notify_bulk_map_materialized_service_e2e
                ON moa.execution_task;
            CREATE TRIGGER notify_bulk_map_materialized_service_e2e
                AFTER INSERT ON moa.execution_task
                FOR EACH ROW
                EXECUTE FUNCTION moa.notify_bulk_map_materialized_service_e2e();

            CREATE OR REPLACE FUNCTION moa.block_bulk_map_reservation_service_e2e()
            RETURNS TRIGGER
            LANGUAGE plpgsql
            AS $$
            BEGIN
                IF OLD.status = 'pending'
                   AND NEW.status = 'reserved'
                   AND NEW.node_id = '{MAP_NODE_ID}'
                THEN
                    PERFORM pg_advisory_xact_lock({reservation_lock_key});
                END IF;
                RETURN NEW;
            END;
            $$;
            DROP TRIGGER IF EXISTS block_bulk_map_reservation_service_e2e
                ON moa.execution_task;
            CREATE TRIGGER block_bulk_map_reservation_service_e2e
                BEFORE UPDATE ON moa.execution_task
                FOR EACH ROW
                EXECUTE FUNCTION moa.block_bulk_map_reservation_service_e2e();
            "#
        );
        sqlx::raw_sql(&barrier_ddl)
            .execute(pool)
            .await
            .context("install map materialization signal and pre-dispatch reservation barrier")?;
        Ok(Self {
            listener,
            reservation_lock,
            reservation_lock_key,
        })
    }

    async fn wait_for_run(&mut self, pool: &PgPool, run_uid: uuid::Uuid) -> Result<()> {
        let deadline = Instant::now() + BULK_TIMEOUT;
        loop {
            let notification = tokio::time::timeout_at(deadline, self.listener.recv())
                .await
                .with_context(|| {
                    format!(
                        "run {run_uid} did not emit its map materialization commit signal within {BULK_TIMEOUT:?}"
                    )
                })??;
            if notification.payload() != run_uid.to_string() {
                continue;
            }
            return wait_for_map_row_count(pool, run_uid, COMPANY_COUNT).await;
        }
    }

    async fn release_and_remove(&mut self, pool: &PgPool) -> Result<()> {
        let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(self.reservation_lock_key)
            .fetch_one(&mut self.reservation_lock)
            .await
            .context("release map reservation barrier after orchestrator restart")?;
        if !unlocked {
            bail!("map reservation barrier was not held by its owning test connection");
        }
        sqlx::raw_sql(
            r#"
            DROP TRIGGER IF EXISTS block_bulk_map_reservation_service_e2e
                ON moa.execution_task;
            DROP TRIGGER IF EXISTS notify_bulk_map_materialized_service_e2e
                ON moa.execution_task;
            DROP FUNCTION IF EXISTS moa.block_bulk_map_reservation_service_e2e();
            DROP FUNCTION IF EXISTS moa.notify_bulk_map_materialized_service_e2e();
            "#,
        )
        .execute(pool)
        .await
        .context("remove map materialization signal and reservation barrier")?;
        Ok(())
    }
}

fn map_tool() -> FixtureCapabilityTool {
    FixtureCapabilityTool {
        name: MAP_TOOL_NAME.to_string(),
        description: "Count AI mentions for one company-like map item".to_string(),
        input_schema: map_input_schema(),
        item_key_pointer: Some("/company".to_string()),
        outcomes: vec![FixtureCapabilityOutcome::SuccessWithInput {
            output: json!({
                "ai_mentions": 1,
                "raw_payload": RAW_MAP_SENTINEL,
            }),
        }],
    }
}

fn reducer_tool() -> FixtureCapabilityTool {
    FixtureCapabilityTool {
        name: REDUCE_TOOL_NAME.to_string(),
        description: "Aggregate one bounded batch of company mention results".to_string(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["round", "batch_index", "items"],
            "properties": {
                "round": {"const": 1},
                "batch_index": {"const": 0},
                "items": {"type": "array", "minItems": COMPANY_COUNT, "maxItems": COMPANY_COUNT}
            }
        }),
        item_key_pointer: None,
        outcomes: vec![FixtureCapabilityOutcome::Success {
            output: final_report(),
        }],
    }
}

fn bulk_candidate(
    map_tool: &FixtureCapabilityTool,
    reducer_tool: &FixtureCapabilityTool,
) -> Result<GeneratedExecutionCandidate> {
    let companies = companies();
    let report_schema = report_schema();
    let map_reference = fixture_capability_reference(map_tool)?;
    let reducer_reference = fixture_capability_reference(reducer_tool)?;
    Ok(GeneratedExecutionCandidate {
        goal: ExecutionGoalContract {
            objective: OBJECTIVE.to_string(),
            requirements: vec![ExecutionRequirement {
                id: "complete_report".to_string(),
                description: "screen every company and produce one bounded aggregate report"
                    .to_string(),
            }],
            deliverables: Vec::new(),
            coverage: vec![CoverageRequirement {
                id: "coverage_sp500".to_string(),
                description: "all 500 company-like keys complete successfully".to_string(),
                map_node_id: MAP_NODE_ID.to_string(),
                expected_items: Value::Array(companies.clone()),
                require_all: true,
            }],
            constraints: Vec::new(),
            completion_checks: vec![
                CompletionCheck {
                    id: "coverage_complete".to_string(),
                    description: "all expected company keys are covered".to_string(),
                    requirement_ids: vec!["complete_report".to_string()],
                    constraint_ids: Vec::new(),
                    kind: CompletionCheckKind::MapCoverage {
                        map_node_id: MAP_NODE_ID.to_string(),
                    },
                },
                CompletionCheck {
                    id: "report_schema".to_string(),
                    description: "the bounded aggregate report matches its schema".to_string(),
                    requirement_ids: vec!["complete_report".to_string()],
                    constraint_ids: Vec::new(),
                    kind: CompletionCheckKind::OutputSchema,
                },
            ],
        },
        plan: ExecutionPlanDefinition {
            schema_version: 1,
            input_schema: json!({"type": "object", "additionalProperties": false}),
            output_schema: report_schema.clone(),
            nodes: vec![
                ExecutionNode {
                    id: MAP_NODE_ID.to_string(),
                    requirement_ids: vec!["complete_report".to_string()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({"$item": true}),
                    output_schema: json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["items"],
                        "properties": {
                            "items": {
                                "type": "array",
                                "minItems": COMPANY_COUNT,
                                "maxItems": COMPANY_COUNT
                            }
                        }
                    }),
                    operation: ExecutionOperation::Map {
                        items: Value::Array(companies),
                        item_key: "/company".to_string(),
                        max_items: COMPANY_COUNT as u64,
                        item_output_schema: map_output_schema(),
                        task: MapTask::Capability {
                            reference: map_reference,
                        },
                    },
                    retry: no_retry(),
                    budget: None,
                },
                ExecutionNode {
                    id: REDUCE_NODE_ID.to_string(),
                    requirement_ids: vec!["complete_report".to_string()],
                    depends_on: vec![MAP_NODE_ID.to_string()],
                    when: None,
                    input: json!({}),
                    output_schema: report_schema.clone(),
                    operation: ExecutionOperation::Reduce {
                        items: json!({"$ref": format!("$.nodes.{MAP_NODE_ID}.output.items")}),
                        max_items: COMPANY_COUNT as u64,
                        reducer: ExecutionReducer::Capability {
                            reference: reducer_reference,
                        },
                        batch_size: COMPANY_COUNT as u32,
                    },
                    retry: no_retry(),
                    budget: None,
                },
                ExecutionNode {
                    id: OUTPUT_NODE_ID.to_string(),
                    requirement_ids: vec!["complete_report".to_string()],
                    depends_on: vec![REDUCE_NODE_ID.to_string()],
                    when: None,
                    input: json!({}),
                    output_schema: report_schema,
                    operation: ExecutionOperation::Output {
                        value: json!({"$ref": format!("$.nodes.{REDUCE_NODE_ID}.output")}),
                    },
                    retry: no_retry(),
                    budget: None,
                },
            ],
        },
        run_input: json!({}),
    })
}

fn fixture_capability_reference(tool: &FixtureCapabilityTool) -> Result<CapabilityReference> {
    let policy = ToolPolicySpec {
        risk_level: RiskLevel::High,
        default_effect: ActionPolicyEffect::AdminReview,
        action_class: ActionClass::ExternalWrite,
        input_shape: ToolInputShape::Json,
        diff_strategy: ToolDiffStrategy::None,
    };
    let version = capability_version(
        "moa.execution.capability.mcp",
        &json!({
            "name": tool.name,
            "input_schema": tool.input_schema,
            "policy": policy,
            "idempotency_class": IdempotencyClass::Idempotent,
            "max_output_tokens": ToolBudgetConfig::default().for_tool(&tool.name),
            "owner": {"kind": "mcp", "server": FIXTURE_MCP_SERVER_NAME},
        }),
    )?;
    Ok(CapabilityReference {
        name: tool.name.clone(),
        version,
    })
}

fn companies() -> Vec<Value> {
    (0..COMPANY_COUNT)
        .map(|index| json!({"company": format!("SP500-{index:03}")}))
        .collect()
}

fn map_input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["company"],
        "properties": {"company": {"type": "string", "pattern": "^SP500-[0-9]{3}$"}}
    })
}

fn map_output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["company", "ai_mentions", "raw_payload"],
        "properties": {
            "company": {"type": "string", "pattern": "^SP500-[0-9]{3}$"},
            "ai_mentions": {"const": 1},
            "raw_payload": {"type": "string"}
        }
    })
}

fn report_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["company_count", "total_ai_mentions", "report"],
        "properties": {
            "company_count": {"const": COMPANY_COUNT},
            "total_ai_mentions": {"const": COMPANY_COUNT},
            "report": {"const": "bounded-sp500-ai-mention-report"}
        }
    })
}

fn final_report() -> Value {
    json!({
        "company_count": COMPANY_COUNT,
        "total_ai_mentions": COMPANY_COUNT,
        "report": "bounded-sp500-ai-mention-report"
    })
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

fn planner_capabilities(
    requests: &[moa_core::types::completion::CompletionRequest],
) -> Result<Vec<Value>> {
    let context = requests
        .iter()
        .flat_map(|request| &request.messages)
        .find_map(|message| {
            message
                .content
                .split_once("<frozen_planning_context>")
                .and_then(|(_, suffix)| suffix.split_once("</frozen_planning_context>"))
                .map(|(context, _)| context)
        })
        .context("planner request journal omitted frozen planning context")?;
    let context: Value = serde_json::from_str(context).context("decode frozen planning context")?;
    Ok(context
        .pointer("/catalog/capabilities")
        .and_then(Value::as_array)
        .context("frozen planning context omitted capability catalog")?
        .iter()
        .filter(|capability| {
            capability
                .pointer("/reference/name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.starts_with("fixture_"))
        })
        .cloned()
        .collect())
}

async fn wait_for_map_calls(
    fixture: &OrchestratorTestFixture,
    controller: &moa_test_support::FixtureCapabilityController,
    pool: &PgPool,
    run_uid: uuid::Uuid,
    expected: usize,
) -> Result<Vec<FixtureCapabilityCall>> {
    let calls = match controller.wait_for_calls(expected, BULK_TIMEOUT).await {
        Ok(calls) => calls,
        Err(error) => {
            let task_statuses = task_status_diagnostics(pool, run_uid).await?;
            let process_exit = fixture.unexpected_orchestrator_exit().await?;
            bail!(
                "wait for exactly {expected} unique map effects: {error:#}; \
                 task statuses: {task_statuses}; orchestrator exit: {process_exit:#?}"
            );
        }
    };
    let map_calls = calls
        .iter()
        .filter(|call| call.capability == MAP_TOOL_NAME)
        .count();
    if map_calls != expected || calls.len() != expected {
        bail!(
            "expected only {expected} map calls before reducer; map={map_calls}, total={}, calls={calls:#?}",
            calls.len()
        );
    }
    Ok(calls)
}

async fn wait_for_map_row_count(pool: &PgPool, run_uid: uuid::Uuid, expected: usize) -> Result<()> {
    wait_for_sql_count(pool, run_uid, expected, false).await
}

async fn wait_for_completed_map_count(
    pool: &PgPool,
    run_uid: uuid::Uuid,
    expected: usize,
) -> Result<()> {
    wait_for_sql_count(pool, run_uid, expected, true).await
}

async fn wait_for_sql_count(
    pool: &PgPool,
    run_uid: uuid::Uuid,
    expected: usize,
    completed_only: bool,
) -> Result<()> {
    let expected = i64::try_from(expected).context("convert SQL checkpoint count")?;
    let deadline = Instant::now() + BULK_TIMEOUT;
    loop {
        let count: i64 = if completed_only {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM moa.execution_task \
                 WHERE run_uid = $1 AND node_id = $2 AND status = 'completed'",
            )
            .bind(run_uid)
            .bind(MAP_NODE_ID)
            .fetch_one(pool)
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM moa.execution_task WHERE run_uid = $1 AND node_id = $2",
            )
            .bind(run_uid)
            .bind(MAP_NODE_ID)
            .fetch_one(pool)
            .await?
        };
        if count == expected {
            return Ok(());
        }
        if count > expected {
            bail!(
                "run {run_uid} passed exact {} map checkpoint: expected {expected}, observed {count}; statuses={}",
                if completed_only {
                    "completed"
                } else {
                    "materialized"
                },
                task_status_diagnostics(pool, run_uid).await?
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "run {run_uid} reached {count}/{expected} {} map rows within {BULK_TIMEOUT:?}; statuses={}",
                if completed_only {
                    "completed"
                } else {
                    "materialized"
                },
                task_status_diagnostics(pool, run_uid).await?
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn task_status_diagnostics(pool: &PgPool, run_uid: uuid::Uuid) -> Result<Value> {
    sqlx::query_scalar(
        "SELECT COALESCE(jsonb_object_agg(status, count), '{}'::jsonb) \
         FROM (SELECT status, COUNT(*) AS count FROM moa.execution_task \
               WHERE run_uid = $1 GROUP BY status ORDER BY status) statuses",
    )
    .bind(run_uid)
    .fetch_one(pool)
    .await
    .context("load exact task-status timeout diagnostics")
}

async fn list_all_tasks(
    client: &moa_test_support::TestApiClient,
    run: ExecutionRunRequest,
) -> Result<Vec<ExecutionTaskProjection>> {
    let page: ExecutionTaskListResponse = client
        .post_call(
            "/Execution/list_tasks",
            &ExecutionTaskListRequest {
                run,
                limit: Some(1_000),
                cursor: None,
            },
        )
        .await
        .context("list complete bulk task projection")?;
    if page.next_cursor.is_some() {
        bail!(
            "502-task scenario unexpectedly exceeded the 1,000-row page: {:?}",
            page.next_cursor
        );
    }
    Ok(page.tasks)
}

fn assert_call_partition(calls: &[FixtureCapabilityCall]) -> Result<()> {
    let map_calls = calls
        .iter()
        .filter(|call| call.capability == MAP_TOOL_NAME)
        .collect::<Vec<_>>();
    let reducer_calls = calls
        .iter()
        .filter(|call| call.capability == REDUCE_TOOL_NAME)
        .collect::<Vec<_>>();
    assert_eq!(map_calls.len(), COMPANY_COUNT);
    assert_eq!(reducer_calls.len(), 1);
    assert_eq!(calls.len(), COMPANY_COUNT + 1);
    assert_eq!(reducer_calls[0].item_key, "");
    assert_eq!(reducer_calls[0].input["round"], json!(1));
    assert_eq!(reducer_calls[0].input["batch_index"], json!(0));
    let reducer_items = reducer_calls[0]
        .input
        .get("items")
        .and_then(Value::as_array)
        .context("sole reducer call omitted its structured items")?;
    assert_eq!(reducer_items.len(), COMPANY_COUNT);

    let unique_invocations = calls
        .iter()
        .map(|call| call.invocation_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(unique_invocations.len(), COMPANY_COUNT + 1);
    let unique_keys = map_calls
        .iter()
        .map(|call| call.item_key.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(unique_keys.len(), COMPANY_COUNT);
    Ok(())
}

fn assert_bulk_terminal(status: &ExecutionStatusResponse, tasks: &[ExecutionTaskProjection]) {
    assert_completed_terminal(status, 1, 1);
    assert_eq!(status.output, Some(final_report()));
    assert_eq!(status.run.total_tasks, 502);
    assert_eq!(status.run.completed_tasks, 502);
    assert_eq!(status.run.failed_tasks, 0);
    assert_eq!(
        status.run.budget_ledger.reserved,
        ExecutionEstimate::default()
    );
    assert_eq!(status.run.budget_ledger.consumed.tasks, 502);
    assert_eq!(status.run.budget_ledger.consumed.tool_calls, 501);
    assert_eq!(status.run.budget_ledger.consumed.cost_microusd, 0);
    assert_eq!(status.run.budget_ledger.consumed.tokens, 0);
    assert!(!status.run.budget_ledger.overrun);

    let summed_usage = tasks
        .iter()
        .fold(ExecutionEstimate::default(), |mut sum, task| {
            let outcome = task
                .outcome
                .as_ref()
                .expect("every terminal bulk task must retain its current outcome");
            sum.cost_microusd = sum
                .cost_microusd
                .saturating_add(outcome.usage.cost_microusd);
            sum.tokens = sum.tokens.saturating_add(outcome.usage.tokens);
            sum.tool_calls = sum.tool_calls.saturating_add(outcome.usage.tool_calls);
            sum.retrieved_bytes = sum
                .retrieved_bytes
                .saturating_add(outcome.usage.retrieved_bytes);
            sum.tasks = sum.tasks.saturating_add(1);
            sum
        });
    assert_eq!(status.run.budget_ledger.consumed, summed_usage);
}

fn assert_bulk_task_identity_and_outcomes(
    run_uid: uuid::Uuid,
    tasks: &[ExecutionTaskProjection],
    calls: &[FixtureCapabilityCall],
    controller: &moa_test_support::FixtureCapabilityController,
) -> Result<()> {
    assert_eq!(tasks.len(), 502);
    assert!(
        tasks
            .iter()
            .all(|task| task.status == ExecutionTaskStatus::Completed)
    );
    let map_tasks = tasks
        .iter()
        .filter(|task| task.node_id == MAP_NODE_ID)
        .collect::<Vec<_>>();
    let reducer_tasks = tasks
        .iter()
        .filter(|task| task.node_id == REDUCE_NODE_ID)
        .collect::<Vec<_>>();
    let output_tasks = tasks
        .iter()
        .filter(|task| task.node_id == OUTPUT_NODE_ID)
        .collect::<Vec<_>>();
    assert_eq!(map_tasks.len(), COMPANY_COUNT);
    assert_eq!(reducer_tasks.len(), 1);
    assert_eq!(output_tasks.len(), 1);
    assert_eq!(reducer_tasks[0].item_key, "r1:b0");
    assert_eq!(output_tasks[0].item_key, "");
    assert_eq!(
        reducer_tasks[0].task_id,
        ExecutionTaskId::derive(run_uid, REDUCE_NODE_ID, "r1:b0")?
    );
    assert_eq!(
        output_tasks[0].task_id,
        ExecutionTaskId::derive(run_uid, OUTPUT_NODE_ID, "")?
    );

    let expected_keys = companies()
        .iter()
        .map(|company| extract_map_key(company, "/company"))
        .collect::<moa_execution::Result<BTreeSet<_>>>()?;
    let task_keys = map_tasks
        .iter()
        .map(|task| task.item_key.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(task_keys, expected_keys);
    for task in &map_tasks {
        assert_eq!(
            task.task_id,
            ExecutionTaskId::derive(run_uid, MAP_NODE_ID, &task.item_key)?
        );
        assert_eq!(task.task_id.as_uuid().get_version_num(), 5);
        let outcome = task.outcome.as_ref().context("map task omitted outcome")?;
        let ExecutionTaskResult::Completed { output, citations } = &outcome.result else {
            bail!("map task {} did not complete: {outcome:?}", task.task_id);
        };
        assert!(citations.is_empty());
        assert_eq!(output.get("ai_mentions"), Some(&json!(1)));
        assert_eq!(output.get("raw_payload"), Some(&json!(RAW_MAP_SENTINEL)));
        assert_eq!(extract_map_key(output, "/company")?, task.item_key);
    }
    for task in reducer_tasks.iter().chain(output_tasks.iter()) {
        let outcome = task
            .outcome
            .as_ref()
            .context("aggregate task omitted outcome")?;
        assert!(matches!(
            &outcome.result,
            ExecutionTaskResult::Completed { output, citations }
                if output == &final_report() && citations.is_empty()
        ));
    }

    let derived = controller
        .derived_task_ids(run_uid, MAP_NODE_ID)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let task_ids = map_tasks
        .iter()
        .map(|task| task.task_id)
        .collect::<BTreeSet<_>>();
    assert_eq!(derived, task_ids);
    let map_call_keys = calls
        .iter()
        .filter(|call| call.capability == MAP_TOOL_NAME)
        .map(|call| call.item_key.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(map_call_keys, task_keys);
    Ok(())
}

async fn assert_bulk_audit_counts(pool: &PgPool, run_uid: uuid::Uuid) -> Result<()> {
    let counts: Value = sqlx::query_scalar(
        "SELECT jsonb_build_object( \
            'tasks', COUNT(*), \
            'current_outcomes', COUNT(*) FILTER (WHERE current_outcome IS NOT NULL), \
            'map_current_outcomes', COUNT(*) FILTER (WHERE node_id = $2 AND current_outcome IS NOT NULL), \
            'accepted', COALESCE(SUM((SELECT COUNT(*) FROM jsonb_array_elements(outcome_audit) entry \
                WHERE (entry ->> 'accepted')::boolean IS TRUE)), 0), \
            'rejected', COALESCE(SUM((SELECT COUNT(*) FROM jsonb_array_elements(outcome_audit) entry \
                WHERE (entry ->> 'accepted')::boolean IS FALSE)), 0)) \
         FROM moa.execution_task WHERE run_uid = $1",
    )
    .bind(run_uid)
    .bind(MAP_NODE_ID)
    .fetch_one(pool)
    .await
    .context("load accepted/rejected outcome-audit counts")?;
    assert_eq!(
        counts,
        json!({
            "tasks": 502,
            "current_outcomes": 502,
            "map_current_outcomes": 500,
            "accepted": 502,
            "rejected": 0,
        })
    );
    Ok(())
}

async fn assert_bounded_session_and_context(
    pool: &PgPool,
    session_id: moa_core::types::identifiers::SessionId,
    terminal: &ExecutionStatusResponse,
    events: &[moa_core::types::events_stream::EventRecord],
    requests: &[moa_core::types::completion::CompletionRequest],
) -> Result<()> {
    let event_json = serde_json::to_string(events)?;
    assert!(
        !event_json.contains(RAW_MAP_SENTINEL),
        "session events leaked raw per-company task output"
    );
    let completed = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ExecutionCompleted(summary) => Some(summary),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].output, Some(final_report()));
    assert_eq!(
        completed[0].task_results,
        ExecutionTaskResultsRef::ExecutionTaskTable {
            run_uid: terminal.run.run_uid
        }
    );

    let compiled_requests = serde_json::to_string(requests)?;
    assert!(
        !compiled_requests.contains(RAW_MAP_SENTINEL),
        "compiled provider context leaked raw per-company task output"
    );
    let synthesis = requests
        .iter()
        .find(|request| {
            journal_roles(std::slice::from_ref(request)) == vec![JournalRequestRole::Synthesis]
        })
        .context("bulk journal omitted synthesis request")?;
    let synthesis_text = synthesis
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(synthesis_text.contains("bounded-sp500-ai-mention-report"));
    assert!(
        synthesis_text.len() < 100_000,
        "synthesis context was not bounded"
    );

    let snapshot: Option<Value> =
        sqlx::query_scalar("SELECT payload FROM context_snapshots WHERE session_id = $1 LIMIT 1")
            .bind(session_id.0)
            .fetch_optional(pool)
            .await
            .context("load optional compiled context snapshot")?;
    if let Some(snapshot) = snapshot {
        assert!(
            !snapshot.to_string().contains(RAW_MAP_SENTINEL),
            "persisted compiled context snapshot leaked raw map output"
        );
    }
    Ok(())
}

fn assert_transport_attempts(attempts: &[FixtureCapabilityAttempt]) {
    let logical_effects = attempts
        .iter()
        .map(|attempt| attempt.invocation_id.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(logical_effects.len(), COMPANY_COUNT + 1);
    let first_arrivals = attempts.iter().filter(|attempt| !attempt.is_replay).count();
    assert_eq!(first_arrivals, COMPANY_COUNT + 1);
    assert!(attempts.len() >= logical_effects.len());
}

async fn load_canonical_final_projection(pool: &PgPool, run_uid: uuid::Uuid) -> Result<Value> {
    sqlx::query_scalar(
        "SELECT jsonb_build_object( \
            'status', status, \
            'output', output, \
            'terminal_gaps', terminal_gaps, \
            'terminal_cause', terminal_cause, \
            'terminal_satisfied_requirement_count', terminal_satisfied_requirement_count, \
            'terminal_requirement_count', terminal_requirement_count, \
            'reserved_cost_microusd', reserved_cost_microusd, \
            'reserved_tokens', reserved_tokens, \
            'reserved_tasks', reserved_tasks, \
            'reserved_tool_calls', reserved_tool_calls, \
            'reserved_retrieved_bytes', reserved_retrieved_bytes, \
            'consumed_cost_microusd', consumed_cost_microusd, \
            'consumed_tokens', consumed_tokens, \
            'consumed_tasks', consumed_tasks, \
            'consumed_tool_calls', consumed_tool_calls, \
            'consumed_retrieved_bytes', consumed_retrieved_bytes, \
            'budget_overrun', budget_overrun, \
            'progress_total_tasks', progress_total_tasks, \
            'progress_completed_tasks', progress_completed_tasks, \
            'progress_failed_tasks', progress_failed_tasks, \
            'progress_cancelled_tasks', progress_cancelled_tasks) \
         FROM moa.execution_run WHERE run_uid = $1",
    )
    .bind(run_uid)
    .fetch_one(pool)
    .await
    .context("load canonical final run ledger/progress projection")
}
