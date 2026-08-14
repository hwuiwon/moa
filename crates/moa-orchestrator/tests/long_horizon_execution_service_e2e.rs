//! Deterministic accelerated long-horizon execution coverage over real services.
//!
//! Eight logical days are compressed to sixteen real seconds. All temporal work
//! still uses the production compiler, PostgreSQL trigger/outbox rows, Restate
//! delayed delivery, and database clock; no fake clock or direct state mutation
//! advances a run.

#[path = "long_horizon_execution_service_e2e/accelerated_week.rs"]
mod accelerated_week;
#[path = "long_horizon_execution_service_e2e/burst_admission.rs"]
mod burst_admission;
#[path = "long_horizon_execution_service_e2e/deadline_and_waits.rs"]
mod deadline_and_waits;
#[path = "long_horizon_execution_service_e2e/deployment_drain.rs"]
mod deployment_drain;
#[path = "long_horizon_execution_service_e2e/disaster_recovery.rs"]
mod disaster_recovery;
#[path = "long_horizon_execution_service_e2e/pause_and_external.rs"]
mod pause_and_external;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, TimeDelta, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit,
    ExecutionCancelPolicy, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionRequirement, ExecutionTemporalTarget,
    ExecutionWaitExpiryAction, ExecutionWaitPolicy, RetryPolicy,
};
use moa_config::ExecutionConfig;
use moa_core::{
    events::Event,
    types::{
        action_policy::ActionPolicyEffect,
        execution_planning::{ExecutionSourceProvenance, GeneratedPlanPlannerProvenance},
        identifiers::TenantId,
    },
};
use moa_execution::{
    capability::{
        CapabilitySource, ExecutionAuthorizationEnvelope, ExecutionCapability,
        ExecutionCapabilityCatalog,
    },
    compiler::{CompileExecutionRequest, compile},
    state::{ExecutionRunStatus, ExecutionTaskId, ExecutionTaskProjection, ExecutionTaskStatus},
    wire::{
        ExecutionPlanningContextRequest, ExecutionPlanningContextResponse, ExecutionRunRequest,
        ExecutionStartRequest, ExecutionStartResponse, ExecutionStatusResponse,
        ExecutionTaskListRequest, ExecutionTaskListResponse,
    },
};
use moa_orchestrator::services::action_policy::UpsertActionPolicyRuleRequest;
use moa_test_support::{
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool, IsolatedTest,
    OrchestratorTestFixture,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use tokio::time::Instant;
use uuid::Uuid;

/// Two real seconds represent one logical day in this deterministic lane.
const LOGICAL_DAY: Duration = Duration::from_secs(2);
/// Maximum real wait for one compressed scenario transition.
const SCENARIO_TIMEOUT: Duration = Duration::from_secs(45);
/// Poll cadence used only to observe durable state transitions.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const FIXTURE_CAPABILITY_VERSION: &str = "__fixture_current__";
const HAND_CAPABILITY_VERSION: &str = "__hand_current__";
const SANDBOX_TENANT_UUID: Uuid = Uuid::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0001);
const SANDBOX_PROVIDER_ACCOUNT_UUID: Uuid =
    Uuid::from_u128(0x3000_0000_0000_0000_0000_0000_0000_0012);

#[derive(Clone)]
struct StartedRun {
    request: ExecutionRunRequest,
    tenant_id: TenantId,
    run_uid: Uuid,
}

fn fixture_script() -> Value {
    json!({
        "default": {
            "completion": {
                "content": "fixture-only",
                "duration_ms": 1,
                "input_tokens": 1,
                "cached_input_tokens": 0,
                "cache_write_input_tokens": 0,
                "tool_calls": []
            }
        }
    })
}

async fn execution_fixture(extra_env: Vec<(String, String)>) -> Result<OrchestratorTestFixture> {
    execution_fixture_with_script(fixture_script(), extra_env).await
}

async fn execution_fixture_with_script(
    script: Value,
    extra_env: Vec<(String, String)>,
) -> Result<OrchestratorTestFixture> {
    OrchestratorTestFixture::with_execution_fixture(
        script,
        FixtureCapabilityOptions {
            tools: Vec::new(),
            orchestrator_env: extra_env,
        },
    )
    .await
}

async fn execution_fixture_with_tools(
    tools: Vec<FixtureCapabilityTool>,
    extra_env: Vec<(String, String)>,
) -> Result<OrchestratorTestFixture> {
    execution_fixture_with_script_and_tools(fixture_script(), tools, extra_env).await
}

async fn external_job_execution_fixture(
    extra_env: Vec<(String, String)>,
) -> Result<OrchestratorTestFixture> {
    OrchestratorTestFixture::with_external_job_execution_fixture(fixture_script(), extra_env).await
}

async fn execution_fixture_with_script_and_tools(
    script: Value,
    tools: Vec<FixtureCapabilityTool>,
    extra_env: Vec<(String, String)>,
) -> Result<OrchestratorTestFixture> {
    OrchestratorTestFixture::with_execution_fixture(
        script,
        FixtureCapabilityOptions {
            tools,
            orchestrator_env: extra_env,
        },
    )
    .await
}

async fn sandbox_execution_fixture() -> Result<OrchestratorTestFixture> {
    let provider_account_id = SANDBOX_PROVIDER_ACCOUNT_UUID;
    let tenant_id = SANDBOX_TENANT_UUID;
    OrchestratorTestFixture::with_sandbox_workspace_execution_fixture(
        fixture_script(),
        FixtureCapabilityOptions {
            tools: Vec::new(),
            orchestrator_env: vec![
                (
                    "MOA_LOCAL_PROVIDER_ACCOUNT_JSON".to_string(),
                    json!({
                        "provider_account_id": provider_account_id,
                        "generation": 1,
                        "isolation_cell": "long-horizon-task12"
                    })
                    .to_string(),
                ),
                ("MOA_LOCAL_DOCKER_ENABLED".to_string(), "false".to_string()),
                (
                    "MOA_SANDBOX_WORKSPACE_MODE".to_string(),
                    "admit".to_string(),
                ),
                (
                    "MOA_SANDBOX_WORKSPACE_CANARY_JSON".to_string(),
                    json!({
                        "provider_account_id": provider_account_id,
                        "provider_account_generation": 1,
                        "isolation_cell": "long-horizon-task12",
                        "tenant_allowlist": [tenant_id]
                    })
                    .to_string(),
                ),
                (
                    "MOA_SANDBOX_WORKSPACE_QUOTA_ROUTES_JSON".to_string(),
                    json!([{
                        "tenant_id": tenant_id,
                        "provider_account_id": provider_account_id,
                        "provider_account_generation": 1,
                        "max_workspaces": 8,
                        "max_active_hands": 2,
                        "max_checkpoints": 32,
                        "max_logical_bytes": 268_435_456_u64
                    }])
                    .to_string(),
                ),
                (
                    "MOA_AUTHZ_OPENFGA_MODEL_VERSION".to_string(),
                    "7".to_string(),
                ),
            ],
        },
    )
    .await
}

fn after_logical_days(days: u64) -> ExecutionTemporalTarget {
    ExecutionTemporalTarget::After {
        delay_seconds: LOGICAL_DAY.as_secs().saturating_mul(days),
    }
}

fn continue_wait(days: u64, output: Value) -> ExecutionWaitPolicy {
    ExecutionWaitPolicy {
        expiry: after_logical_days(days),
        on_expiry: ExecutionWaitExpiryAction::ContinueWith { output },
    }
}

fn node(
    id: &str,
    depends_on: &[&str],
    operation: ExecutionOperation,
    output_schema: Value,
) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["result".to_string()],
        depends_on: depends_on.iter().map(|id| (*id).to_string()).collect(),
        when: None,
        input: json!({}),
        output_schema,
        operation,
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 2,
            initial_backoff_ms: 50,
            max_backoff_ms: 50,
        },
        budget: None,
    }
}

fn output_node(depends_on: &[&str], value: Value) -> ExecutionNode {
    node(
        "output",
        depends_on,
        ExecutionOperation::Output {
            value: value.clone(),
        },
        json!({"type": "object"}),
    )
}

fn fixture_capability_node(id: &str, tool_name: &str, input: Value) -> ExecutionNode {
    let mut capability = node(
        id,
        &[],
        ExecutionOperation::Capability {
            reference: CapabilityReference {
                name: moa_hands::mcp_tool_reference("fixture-capability", tool_name),
                version: FIXTURE_CAPABILITY_VERSION.to_string(),
            },
        },
        json!({"type": "object"}),
    );
    capability.input = input;
    capability
}

fn external_job_capability_node(id: &str, input: Value) -> ExecutionNode {
    let mut capability = node(
        id,
        &[],
        ExecutionOperation::Capability {
            reference: CapabilityReference {
                name: "fixture_external_job".to_string(),
                version: FIXTURE_CAPABILITY_VERSION.to_string(),
            },
        },
        json!({"type": "object"}),
    );
    capability.input = input;
    capability
}

fn hand_capability_node(
    id: &str,
    depends_on: &[&str],
    tool_name: &str,
    input: Value,
) -> ExecutionNode {
    let mut capability = node(
        id,
        depends_on,
        ExecutionOperation::Capability {
            reference: CapabilityReference {
                name: tool_name.to_string(),
                version: HAND_CAPABILITY_VERSION.to_string(),
            },
        },
        json!({"type": "string"}),
    );
    capability.input = input;
    capability
}

async fn start_plan(
    test: &IsolatedTest<'_>,
    label: &str,
    nodes: Vec<ExecutionNode>,
    deadline_after: Duration,
) -> Result<StartedRun> {
    start_plan_with_capability_policy(
        test,
        label,
        nodes,
        deadline_after,
        Some(ActionPolicyEffect::Allow),
    )
    .await
}

async fn start_plan_with_policy(
    test: &IsolatedTest<'_>,
    label: &str,
    nodes: Vec<ExecutionNode>,
    deadline_after: Duration,
    configure_capability_policy: bool,
) -> Result<StartedRun> {
    start_plan_with_capability_policy(
        test,
        label,
        nodes,
        deadline_after,
        configure_capability_policy.then_some(ActionPolicyEffect::Allow),
    )
    .await
}

async fn start_plan_with_capability_policy(
    test: &IsolatedTest<'_>,
    label: &str,
    mut nodes: Vec<ExecutionNode>,
    deadline_after: Duration,
    capability_policy: Option<ActionPolicyEffect>,
) -> Result<StartedRun> {
    let session_id = test.create_session(label).await?;
    let session = test.client().get_session(session_id).await?;
    let objective = format!("deterministic long-horizon scenario {label}");
    let origin = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: objective.clone(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let requested_at = moa_test_support::fixtures::pg_now();
    let deadline = requested_at
        + TimeDelta::from_std(deadline_after).context("convert scenario deadline to chrono")?;
    let mut policy_tool_names = nodes
        .iter()
        .flat_map(|node| match &node.operation {
            ExecutionOperation::Capability { reference }
                if reference.version == FIXTURE_CAPABILITY_VERSION
                    || reference.version == HAND_CAPABILITY_VERSION =>
            {
                vec![reference.name.clone()]
            }
            ExecutionOperation::Agent {
                capability_refs, ..
            } => capability_refs
                .iter()
                .filter(|reference| {
                    reference.version == FIXTURE_CAPABILITY_VERSION
                        || reference.version == HAND_CAPABILITY_VERSION
                })
                .map(|reference| reference.name.clone())
                .collect(),
            _ => Vec::new(),
        })
        .collect::<Vec<_>>();
    policy_tool_names.sort();
    policy_tool_names.dedup();
    if capability_policy.is_some() && !policy_tool_names.is_empty() {
        test.fixture
            .grant_default_tenant_admin(session.tenant_id)
            .await
            .context("grant tenant admin before Task 12 capability policy upsert")?;
    }
    if let Some(effect) = capability_policy {
        for capability_name in &policy_tool_names {
            set_fixture_capability_policy(
                test.fixture,
                session.tenant_id,
                capability_name,
                effect,
                label,
            )
            .await?;
        }
    }
    let planning: ExecutionPlanningContextResponse = test
        .client()
        .post_call(
            "/Execution/planning_context",
            &ExecutionPlanningContextRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: origin,
                deadline_at: deadline,
                requested_template: None,
            },
        )
        .await?;
    for node in &mut nodes {
        match &mut node.operation {
            ExecutionOperation::Capability { reference } => {
                resolve_capability_placeholder(reference, &planning.snapshot.catalog.capabilities)?
            }
            ExecutionOperation::Agent {
                capability_refs, ..
            } => {
                for reference in capability_refs {
                    resolve_capability_placeholder(
                        reference,
                        &planning.snapshot.catalog.capabilities,
                    )?;
                }
            }
            _ => {}
        }
    }
    let compile_now = moa_test_support::fixtures::pg_now();
    let goal = ExecutionGoalContract {
        objective,
        requirements: vec![ExecutionRequirement {
            id: "result".to_string(),
            description: "produce the deterministic terminal object".to_string(),
        }],
        deliverables: Vec::new(),
        coverage: Vec::new(),
        constraints: Vec::new(),
        completion_checks: vec![CompletionCheck {
            id: "output-schema".to_string(),
            description: "terminal output matches the declared schema".to_string(),
            requirement_ids: vec!["result".to_string()],
            constraint_ids: Vec::new(),
            kind: CompletionCheckKind::OutputSchema,
        }],
    };
    let plan = ExecutionPlanDefinition {
        cancel_policy: ExecutionCancelPolicy::RetainEffects,
        input_schema: json!({"type": "object", "additionalProperties": false}),
        output_schema: json!({"type": "object"}),
        nodes,
    };
    let outcome = compile(CompileExecutionRequest {
        goal,
        plan,
        run_input: json!({}),
        catalog: planning.snapshot.catalog.clone(),
        authorization: planning.snapshot.authorization.clone(),
        approved_budget: planning.snapshot.budget.clone(),
        config: ExecutionConfig::default(),
        now: compile_now,
    });
    let compiled = outcome.compiled.with_context(|| {
        format!(
            "long-horizon plan `{label}` should compile: {:?}",
            outcome.report.issues
        )
    })?;
    let source_provenance = ExecutionSourceProvenance::GeneratedPlan {
        planner: GeneratedPlanPlannerProvenance {
            model: "fixture-only".to_string(),
            prompt_version: "long-horizon-execution-v1".to_string(),
            candidate_hash: "a".repeat(64),
            compiler_report_hash: "b".repeat(64),
            final_plan_hash: compiled.plan.plan_hash.to_string(),
            repair_attempts: 0,
        },
    };
    let started: ExecutionStartResponse = test
        .client()
        .post_call(
            "/Execution/start",
            &ExecutionStartRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: origin,
                planning_context_uid: planning.planning_context_uid,
                planning_context_hash: planning.planning_context_hash,
                idempotency_key: Some(format!("long-horizon-{label}-{session_id}")),
                compiled,
                run_input: json!({}),
                source_provenance,
            },
        )
        .await?;
    // Restate may replay this exact request after `Execution.start` committed its DB transaction
    // but the handler suspended before returning. The stable idempotency key then returns the same
    // durable run with `created=false`; confirmation is the only non-admitted response here.
    require_unconfirmed_start_admission(label, started.created, started.confirmation_required)?;
    let request = ExecutionRunRequest {
        tenant_id: session.tenant_id,
        contact_id: None,
        session_id,
        run_uid: started.run.run_uid,
    };
    Ok(StartedRun {
        request,
        tenant_id: session.tenant_id,
        run_uid: started.run.run_uid,
    })
}

fn require_unconfirmed_start_admission(
    label: &str,
    created: bool,
    confirmation_required: bool,
) -> Result<()> {
    if confirmation_required {
        bail!(
            "scenario `{label}` unexpectedly required confirmation: created={created}, \
             confirmation_required={confirmation_required}"
        );
    }
    Ok(())
}

fn resolve_capability_placeholder(
    reference: &mut CapabilityReference,
    capabilities: &[ExecutionCapability],
) -> Result<()> {
    if reference.version != FIXTURE_CAPABILITY_VERSION
        && reference.version != HAND_CAPABILITY_VERSION
    {
        return Ok(());
    }
    let resolved = capabilities
        .iter()
        .find(|capability| match reference.version.as_str() {
            FIXTURE_CAPABILITY_VERSION => capability.reference.name == reference.name,
            HAND_CAPABILITY_VERSION => matches!(
                &capability.source,
                CapabilitySource::HandTool { name } if name == &reference.name
            ),
            _ => false,
        })
        .with_context(|| {
            format!(
                "planning catalog omitted Task 12 capability `{}` with placeholder version `{}`",
                reference.name, reference.version
            )
        })?;
    *reference = resolved.reference.clone();
    Ok(())
}

async fn allow_fixture_capability(
    fixture: &OrchestratorTestFixture,
    tenant_id: TenantId,
    capability_name: &str,
    reason: &str,
) -> Result<()> {
    set_fixture_capability_policy(
        fixture,
        tenant_id,
        capability_name,
        ActionPolicyEffect::Allow,
        reason,
    )
    .await
}

async fn set_fixture_capability_policy(
    fixture: &OrchestratorTestFixture,
    tenant_id: TenantId,
    capability_name: &str,
    effect: ActionPolicyEffect,
    reason: &str,
) -> Result<()> {
    fixture
        .client
        .post_void(
            "/ActionPolicy/upsert_rule",
            &UpsertActionPolicyRuleRequest {
                tenant_id,
                contact_id: None,
                tool_name: capability_name.to_string(),
                pattern: "*".to_string(),
                effect,
                reason: Some(format!("Task 12 deterministic capability for {reason}")),
            },
        )
        .await
}

async fn status(test: &IsolatedTest<'_>, run: &StartedRun) -> Result<ExecutionStatusResponse> {
    test.client()
        .post_call("/Execution/status", &run.request)
        .await
}

async fn await_run_status(
    test: &IsolatedTest<'_>,
    run: &StartedRun,
    expected: ExecutionRunStatus,
) -> Result<ExecutionStatusResponse> {
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let current = status(test, run).await?;
        if current.run.status == expected {
            return Ok(current);
        }
        if current.run.status.is_terminal() || Instant::now() >= deadline {
            bail!(
                "run {} did not reach {expected:?}; current={:?}, waiting={:?}",
                run.run_uid,
                current.run.status,
                current.waiting
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn tasks(test: &IsolatedTest<'_>, run: &StartedRun) -> Result<Vec<ExecutionTaskProjection>> {
    let response: ExecutionTaskListResponse = test
        .client()
        .post_call(
            "/Execution/list_tasks",
            &ExecutionTaskListRequest {
                run: run.request.clone(),
                limit: Some(100),
                cursor: None,
            },
        )
        .await?;
    Ok(response.tasks)
}

async fn await_task_status(
    test: &IsolatedTest<'_>,
    run: &StartedRun,
    node_id: &str,
    expected: ExecutionTaskStatus,
) -> Result<ExecutionTaskProjection> {
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    loop {
        let current = tasks(test, run).await?;
        if let Some(task) = current
            .iter()
            .find(|task| task.node_id == node_id && task.status == expected)
        {
            return Ok(task.clone());
        }
        if Instant::now() >= deadline {
            bail!(
                "node `{node_id}` in run {} did not reach {expected:?}; tasks={current:?}",
                run.run_uid
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn assert_parked_has_no_active_compute(
    fixture: &OrchestratorTestFixture,
    pool: &PgPool,
    run: &StartedRun,
) -> Result<()> {
    let deadline = Instant::now() + SCENARIO_TIMEOUT;
    let (parked_run_reservations, active_attempt_reservations, active_dispatches, active_hands) = loop {
        let parked_run_reservations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.execution_capacity_reservation \
             WHERE run_uid = $1 AND resource_dimension = 'parked_runs' AND state <> 'released'",
        )
        .bind(run.run_uid)
        .fetch_one(pool)
        .await?;
        let active_attempt_reservations: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.execution_capacity_reservation \
             WHERE run_uid = $1 AND resource_dimension = 'active_tasks' AND state <> 'released'",
        )
        .bind(run.run_uid)
        .fetch_one(pool)
        .await?;
        let active_dispatches: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.execution_task \
             WHERE run_uid = $1 AND (active_dispatch_uid IS NOT NULL OR attempt_state IN ('dispatching', 'running'))",
        )
        .bind(run.run_uid)
        .fetch_one(pool)
        .await?;
        let active_hands: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM moa.sandbox_capacity_reservations AS reservation \
             JOIN moa.sandbox_workspaces AS workspace \
               ON workspace.tenant_id = reservation.tenant_id \
              AND workspace.workspace_id = reservation.workspace_id \
             WHERE workspace.scope_kind = 'execution_task' \
               AND workspace.scope_run_id = $1 \
               AND reservation.resource_dimension = 'active_hands' \
               AND reservation.reservation_state <> 'released'",
        )
        .bind(run.run_uid)
        .fetch_one(pool)
        .await?;
        if parked_run_reservations == 1
            && active_attempt_reservations == 0
            && active_dispatches == 0
            && active_hands == 0
        {
            break (
                parked_run_reservations,
                active_attempt_reservations,
                active_dispatches,
                active_hands,
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "run {} did not finish parking; parked_runs={parked_run_reservations}, \
                 active_tasks={active_attempt_reservations}, active_dispatches={active_dispatches}, \
                 active_hands={active_hands}",
                run.run_uid
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    let attempt_dispatches: Vec<Uuid> = sqlx::query_scalar(
        "SELECT dispatch_uid FROM moa.execution_dispatch_outbox \
         WHERE run_uid = $1 AND dispatch_kind = 'task_attempt'",
    )
    .bind(run.run_uid)
    .fetch_all(pool)
    .await?;
    let dispatch_keys = attempt_dispatches
        .iter()
        .map(|dispatch_uid| format!("'{dispatch_uid}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let attempt_clause = if dispatch_keys.is_empty() {
        "false".to_string()
    } else {
        format!(
            "target_service_name = 'ExecutionTaskAttempt' AND target_service_key IN ({dispatch_keys})"
        )
    };
    let invocation_query = format!(
        "SELECT id FROM sys_invocation WHERE \
         ((target_service_name = 'ExecutionRunController' AND target_service_key = '{}') \
          OR ({attempt_clause})) AND status NOT IN ('completed', 'killed')",
        run.run_uid,
    );
    let invocations = loop {
        let invocations = restate_rows(fixture, &invocation_query).await?;
        if invocations.is_empty() || Instant::now() >= deadline {
            break invocations;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    assert_eq!(
        parked_run_reservations, 1,
        "a durably parked run must own exactly one ParkedRuns receipt"
    );
    assert_eq!(
        active_attempt_reservations, 0,
        "parked run retained attempt capacity"
    );
    assert_eq!(
        active_dispatches, 0,
        "parked run retained an active dispatch"
    );
    assert_eq!(
        active_hands, 0,
        "parked tenant retained a live sandbox hand"
    );
    assert!(
        invocations.is_empty(),
        "parked run retained continuing compute invocations: {invocations:?}"
    );
    Ok(())
}

async fn restate_rows(fixture: &OrchestratorTestFixture, query: &str) -> Result<Vec<Value>> {
    let response = reqwest::Client::new()
        .post(format!("{}/query", fixture.admin_url.trim_end_matches('/')))
        .header(reqwest::header::ACCEPT, "application/json")
        .json(&json!({"query": query}))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;
    response
        .get("rows")
        .and_then(Value::as_array)
        .cloned()
        .context("Restate query omitted rows")
}

async fn persisted_trigger_rows(
    pool: &PgPool,
    run_uid: Uuid,
) -> Result<Vec<(String, DateTime<Utc>, i64)>> {
    let rows = sqlx::query(
        "SELECT trigger_kind, due_at, COALESCE(attempt_generation, controller_generation, 0) AS generation \
         FROM moa.execution_trigger WHERE run_uid = $1 ORDER BY due_at, trigger_uid",
    )
    .bind(run_uid)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("trigger_kind")?,
                row.try_get("due_at")?,
                row.try_get("generation")?,
            ))
        })
        .collect()
}

fn task_id(task: &ExecutionTaskProjection) -> ExecutionTaskId {
    task.task_id
}

#[cfg(test)]
mod fixture_contract_tests {
    use super::*;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-11T12:00:00Z")
            .expect("fixture timestamp should parse")
            .with_timezone(&Utc)
    }

    fn output_only_compile_request(
        now: DateTime<Utc>,
        deadline_at: DateTime<Utc>,
    ) -> CompileExecutionRequest {
        let goal = ExecutionGoalContract {
            objective: "deterministic fixture output".to_string(),
            requirements: vec![ExecutionRequirement {
                id: "result".to_string(),
                description: "produce deterministic output".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "output-schema".to_string(),
                description: "validate terminal output".to_string(),
                requirement_ids: vec!["result".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        };
        CompileExecutionRequest {
            goal,
            plan: ExecutionPlanDefinition {
                cancel_policy: ExecutionCancelPolicy::RetainEffects,
                input_schema: json!({"type": "object", "additionalProperties": false}),
                output_schema: json!({"type": "object"}),
                nodes: vec![output_node(&[], json!({"status": "complete"}))],
            },
            run_input: json!({}),
            catalog: ExecutionCapabilityCatalog::build(Vec::new())
                .expect("empty fixture catalog should build"),
            authorization: ExecutionAuthorizationEnvelope {
                capability_refs: Vec::new(),
                skill_refs: Vec::new(),
            },
            approved_budget: ExecutionBudgetLimit {
                max_cost_microusd: Some(1_000_000),
                max_tokens: Some(100_000),
                max_tasks: Some(100),
                max_tool_calls: Some(100),
                max_retrieved_bytes: Some(1_000_000),
                deadline_at: Some(deadline_at),
            },
            config: ExecutionConfig::default(),
            now,
        }
    }

    #[test]
    fn fixture_compile_matches_server_validation_after_setup_skew_offline() {
        // Pins: client compilation and server revalidation produce the exact
        // same canonical plan/hash even after setup consumes part of the horizon.
        let client_now = fixed_now();
        let deadline_at = client_now + TimeDelta::seconds(15);
        let request = output_only_compile_request(client_now, deadline_at);
        let client = compile(request.clone())
            .compiled
            .expect("client fixture should compile");
        let mut server_request = request;
        server_request.now = client_now + TimeDelta::seconds(3);
        let server = compile(server_request)
            .compiled
            .expect("server validation should retain enough temporal slack");

        assert_eq!(server, client);
        assert_eq!(server.plan.plan_hash, client.plan.plan_hash);
    }

    #[test]
    fn start_fixture_accepts_created_or_idempotent_replayed_admission_offline() {
        // Pins: `Execution.start` can return `created=false` when Restate re-enters the handler
        // after the idempotent DB admission committed; both responses retain the admitted run,
        // while either response still fails when explicit confirmation is required.
        for created in [true, false] {
            require_unconfirmed_start_admission("start-replay", created, false)
                .expect("created and replayed starts should both be admitted");
            assert!(
                require_unconfirmed_start_admission("start-replay", created, true).is_err(),
                "confirmation-required start must not be treated as admitted when created={created}"
            );
        }
    }
}
