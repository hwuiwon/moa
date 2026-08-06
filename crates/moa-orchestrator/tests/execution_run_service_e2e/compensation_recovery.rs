//! Restate-level compensation ordering, recovery, and cancellation acceptance.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompensationInputBinding, CompensationInputMapping,
    CompensationValueSource, CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit,
    ExecutionCancelPolicy, ExecutionCompensation, ExecutionGoalContract, ExecutionNode,
    ExecutionOperation, ExecutionPlanDefinition, ExecutionRequirement, RetryPolicy,
};
use moa_config::ExecutionConfig;
use moa_core::{
    events::Event,
    types::{contact::SessionActorRef, identifiers::UserId},
};
use moa_execution::{
    capability::{
        CapabilitiesListRequest, CapabilitiesListResponse, CapabilityRollbackContract,
        ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog,
    },
    compiler::{CompileExecutionRequest, compile},
    repository::{ExecutionRepository, ExecutionScope},
    state::ExecutionRunStatus,
    wire::{
        ExecutionCancelRequest, ExecutionMutationResponse, ExecutionRunRequest,
        ExecutionStartRequest, ExecutionStartResponse,
    },
};
use moa_test_support::{
    FixtureCapabilityOptions, FixtureCapabilityOutcome, FixtureCapabilityTool, IsolatedTest,
    OrchestratorTestFixture,
    fixture_capability::{REVERSIBLE_FIXTURE_COMPENSATOR_TOOL, REVERSIBLE_FIXTURE_FORWARD_TOOL},
};
use serde_json::{Value, json};

use crate::execution_execution_support::fixtures::{
    SERVICE_TIMEOUT, await_execution_terminal, seed_allow_policy,
};

const REQUIREMENT_ID: &str = "effect";
const FORWARD_NODE_PREFIX: &str = "effect";

struct StartedCompensatedRun {
    response: ExecutionStartResponse,
    request: ExecutionRunRequest,
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn third_forward_failure_restarts_after_first_completed_reverse_undo_service_e2e()
-> Result<()> {
    // Pins: the third idempotent governed effect has a known failure after the first two commit,
    // compensation proceeds in reverse registration order, and a hard process
    // restart after the first undo's DB commit does not repeat that completed undo.
    let fixture = compensation_fixture(
        vec![
            FixtureCapabilityOutcome::SuccessWithInput {
                output: json!({"applied": true}),
            },
            FixtureCapabilityOutcome::SuccessWithInput {
                output: json!({"applied": true}),
            },
            FixtureCapabilityOutcome::TerminalFailure {
                message: "third fixture effect failed".to_string(),
            },
        ],
        true,
    )
    .await?;
    let test = fixture.isolated().await;
    let started = start_compensated_run(
        &fixture,
        &test,
        ExecutionCancelPolicy::CompensateCommitted,
        3,
        "third-forward-failure-restart",
    )
    .await?;
    let controller = fixture
        .fixture_capability()
        .context("compensation fixture omitted its capability controller")?;

    for expected in 1..=3 {
        let calls = controller.wait_for_calls(expected, SERVICE_TIMEOUT).await?;
        assert_eq!(
            calls[expected - 1].capability,
            REVERSIBLE_FIXTURE_FORWARD_TOOL
        );
        assert_eq!(
            calls[expected - 1].input,
            json!({"effect_id": format!("effect-{expected}")})
        );
        controller.release(1);
    }

    let calls = controller.wait_for_calls(4, SERVICE_TIMEOUT).await?;
    assert_eq!(calls[3].capability, REVERSIBLE_FIXTURE_COMPENSATOR_TOOL);
    assert_eq!(calls[3].input, json!({"effect_id": "effect-2"}));
    controller.release(1);

    await_completed_compensations(&fixture.postgres_url, started.response.run.run_uid, 1).await?;
    let calls = controller.wait_for_calls(5, SERVICE_TIMEOUT).await?;
    assert_eq!(calls[4].capability, REVERSIBLE_FIXTURE_COMPENSATOR_TOOL);
    assert_eq!(calls[4].input, json!({"effect_id": "effect-1"}));

    fixture
        .hard_crash_and_restart_orchestrator()
        .await
        .context("hard restart after the first reverse undo committed")?;
    await_transport_attempts(controller, 6).await?;
    controller.release(1);

    let request = started.request.clone();
    let terminal = await_execution_terminal(test.client(), &request).await?;
    assert_eq!(terminal.run.status, ExecutionRunStatus::Failed);

    let rows: Vec<(i64, String, Value)> = sqlx::query_as(
        "SELECT registered_sequence, status, mapped_input \
         FROM moa.execution_compensation WHERE run_uid = $1 \
         ORDER BY registered_sequence DESC",
    )
    .bind(started.response.run.run_uid)
    .fetch_all(&sqlx::PgPool::connect(&fixture.postgres_url).await?)
    .await?;
    assert_eq!(
        rows,
        vec![
            (2, "completed".to_string(), json!({"effect_id": "effect-2"})),
            (1, "completed".to_string(), json!({"effect_id": "effect-1"})),
        ]
    );
    assert!(
        !load_manual_repair_required(&fixture.postgres_url, started.response.run.run_uid).await?,
        "clean reverse completion must not require manual repair"
    );
    let final_calls = controller.calls();
    assert_eq!(final_calls.len(), 5, "completed undo must not repeat");
    assert_eq!(
        final_calls
            .iter()
            .filter(|call| call.capability == REVERSIBLE_FIXTURE_COMPENSATOR_TOOL)
            .count(),
        2
    );
    assert!(
        controller
            .transport_attempts()
            .iter()
            .any(|attempt| attempt.is_replay
                && attempt.capability == REVERSIBLE_FIXTURE_COMPENSATOR_TOOL),
        "the in-flight second undo must replay through the stable invocation id"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires the local Restate/Postgres/OpenFGA/Redis service fixture"]
async fn both_cancel_policies_join_admitted_late_effects_before_terminal_service_e2e() -> Result<()>
{
    // Pins: cancellation first fences new dispatch, then joins an already-admitted
    // effect. RetainEffects preserves its late commit; CompensateCommitted reverses
    // that exact commit before exposing the same cancelled terminal state.
    run_cancel_policy_case(ExecutionCancelPolicy::RetainEffects, false).await?;
    run_cancel_policy_case(ExecutionCancelPolicy::CompensateCommitted, true).await
}

async fn run_cancel_policy_case(
    cancel_policy: ExecutionCancelPolicy,
    expects_compensation: bool,
) -> Result<()> {
    let fixture = compensation_fixture(
        vec![FixtureCapabilityOutcome::SuccessWithInput {
            output: json!({"applied": true}),
        }],
        false,
    )
    .await?;
    let test = fixture.isolated().await;
    let label = match cancel_policy {
        ExecutionCancelPolicy::RetainEffects => "cancel-retain-effects",
        ExecutionCancelPolicy::CompensateCommitted => "cancel-compensate-committed",
    };
    let started = start_compensated_run(&fixture, &test, cancel_policy, 1, label).await?;
    let controller = fixture
        .fixture_capability()
        .context("compensation fixture omitted its capability controller")?;
    let calls = controller.wait_for_calls(1, SERVICE_TIMEOUT).await?;
    assert_eq!(calls[0].capability, REVERSIBLE_FIXTURE_FORWARD_TOOL);

    let request = started.request.clone();
    let cancel_client = test.client().clone();
    let cancel_request = request.clone();
    let cancel = tokio::spawn(async move {
        cancel_client
            .post_call::<_, ExecutionMutationResponse>(
                "/Execution/cancel",
                &ExecutionCancelRequest {
                    run: cancel_request,
                    reason: "deterministic service cancellation".to_string(),
                },
            )
            .await
    });
    await_pending_terminal(&fixture.postgres_url, started.response.run.run_uid).await?;
    controller.release(1);
    cancel.await.context("join cancellation request")??;

    if expects_compensation {
        let calls = controller.wait_for_calls(2, SERVICE_TIMEOUT).await?;
        assert_eq!(calls[1].capability, REVERSIBLE_FIXTURE_COMPENSATOR_TOOL);
        assert_eq!(calls[1].input, json!({"effect_id": "effect-1"}));
        controller.release(1);
    }
    let terminal = await_execution_terminal(test.client(), &request).await?;
    assert_eq!(terminal.run.status, ExecutionRunStatus::Cancelled);
    let compensation_statuses: Vec<String> = sqlx::query_scalar(
        "SELECT status FROM moa.execution_compensation \
         WHERE run_uid = $1 ORDER BY registered_sequence DESC",
    )
    .bind(started.response.run.run_uid)
    .fetch_all(&sqlx::PgPool::connect(&fixture.postgres_url).await?)
    .await?;
    if expects_compensation {
        assert_eq!(compensation_statuses, vec!["completed"]);
        assert_eq!(controller.calls().len(), 2);
    } else {
        assert_eq!(compensation_statuses, vec!["pending"]);
        assert_eq!(controller.calls().len(), 1);
    }
    assert!(
        !load_manual_repair_required(&fixture.postgres_url, started.response.run.run_uid).await?,
        "joined cancellation must not manufacture an ambiguous outcome"
    );
    Ok(())
}

async fn compensation_fixture(
    forward_outcomes: Vec<FixtureCapabilityOutcome>,
    forward_is_idempotent: bool,
) -> Result<OrchestratorTestFixture> {
    OrchestratorTestFixture::with_execution_fixture(
        json!({
            "default": {
                "completion": {
                    "content": "unused deterministic provider response",
                    "duration_ms": 1,
                    "input_tokens": 1,
                    "cached_input_tokens": 0,
                    "cache_write_input_tokens": 0,
                    "tool_calls": []
                }
            }
        }),
        FixtureCapabilityOptions {
            tools: vec![
                fixture_tool(
                    REVERSIBLE_FIXTURE_FORWARD_TOOL,
                    forward_is_idempotent,
                    forward_outcomes,
                ),
                fixture_tool(
                    REVERSIBLE_FIXTURE_COMPENSATOR_TOOL,
                    true,
                    vec![FixtureCapabilityOutcome::SuccessWithInput {
                        output: json!({"reverted": true}),
                    }],
                ),
            ],
            orchestrator_env: Vec::new(),
        },
    )
    .await
}

fn fixture_tool(
    name: &str,
    idempotent: bool,
    outcomes: Vec<FixtureCapabilityOutcome>,
) -> FixtureCapabilityTool {
    FixtureCapabilityTool {
        name: name.to_string(),
        description: format!("deterministic compensation fixture {name}"),
        input_schema: json!({
            "type": "object",
            "properties": {"effect_id": {"type": "string"}},
            "required": ["effect_id"],
            "additionalProperties": false
        }),
        item_key_pointer: None,
        idempotent,
        outcomes,
    }
}

async fn start_compensated_run(
    fixture: &OrchestratorTestFixture,
    test: &IsolatedTest<'_>,
    cancel_policy: ExecutionCancelPolicy,
    forward_node_count: usize,
    label: &str,
) -> Result<StartedCompensatedRun> {
    let session_id = test.create_session(label).await?;
    let session = test.client().get_session(session_id).await?;
    let originating_user_sequence_num = test
        .client()
        .append_event(
            session_id,
            Event::UserMessage {
                text: label.to_string(),
                attachments: Vec::new(),
            },
        )
        .await?;
    let owner_user_id = match session.created_by {
        Some(SessionActorRef::Identity { id }) => UserId::new(id.to_string()),
        other => bail!("fixture session has no identity owner: {other:?}"),
    };
    fixture
        .grant_default_tenant_admin(session.tenant_id)
        .await
        .context("grant tenant operator before listing fixture capabilities")?;
    let listed: CapabilitiesListResponse = test
        .client()
        .post_call(
            "/Execution/list_capabilities",
            &CapabilitiesListRequest {
                tenant_id: session.tenant_id,
            },
        )
        .await?;
    let mut capabilities = listed.catalog.capabilities;
    let forward_index = capability_index(&capabilities, REVERSIBLE_FIXTURE_FORWARD_TOOL)?;
    let compensator_index = capability_index(&capabilities, REVERSIBLE_FIXTURE_COMPENSATOR_TOOL)?;
    let forward_reference = capabilities[forward_index].reference.clone();
    let compensator_reference = capabilities[compensator_index].reference.clone();
    let mapping = compensation_mapping();
    capabilities[forward_index].rollback = Some(CapabilityRollbackContract {
        compensator: compensator_reference.clone(),
        input_mapping: mapping.clone(),
    });
    let catalog = ExecutionCapabilityCatalog::build(capabilities)?;
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: catalog
            .capabilities
            .iter()
            .map(|capability| capability.reference.clone())
            .collect(),
        skill_refs: Vec::new(),
    };
    seed_allow_policy(
        fixture,
        test.client(),
        session.tenant_id,
        &forward_reference.name,
    )
    .await?;
    seed_allow_policy(
        fixture,
        test.client(),
        session.tenant_id,
        &compensator_reference.name,
    )
    .await?;
    let budget = generous_budget();
    let compiled = compile(CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: label.to_string(),
            requirements: vec![ExecutionRequirement {
                id: REQUIREMENT_ID.to_string(),
                description: "apply every committed fixture effect exactly once".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "output_schema".to_string(),
                description: "terminal output satisfies its schema".to_string(),
                requirement_ids: vec![REQUIREMENT_ID.to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: compensated_plan(
            cancel_policy,
            forward_node_count,
            forward_reference,
            compensator_reference,
            mapping,
        ),
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: budget.clone(),
        config: ExecutionConfig::default(),
        now: moa_test_support::fixtures::pg_now(),
    })
    .compiled
    .context("fixture compensated plan should compile")?;
    let repository = ExecutionRepository::new(sqlx::PgPool::connect(&fixture.postgres_url).await?);
    let scope = ExecutionScope::Tenant {
        tenant_id: session.tenant_id,
    };
    let (planning_context_uid, planning_context_hash) = super::create_test_planning_context(
        &repository,
        scope,
        session.tenant_id,
        session_id,
        originating_user_sequence_num,
        owner_user_id,
        catalog,
        authorization,
        budget,
    )
    .await?;
    let source_provenance = super::test_source_provenance(&compiled.plan.plan_hash.to_string());
    let started: ExecutionStartResponse = test
        .client()
        .post_call(
            "/Execution/start",
            &ExecutionStartRequest {
                tenant_id: session.tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num,
                planning_context_uid,
                planning_context_hash: planning_context_hash.to_string(),
                idempotency_key: Some(format!("{label}-{session_id}")),
                compiled,
                run_input: json!({}),
                source_provenance,
            },
        )
        .await?;
    if started.confirmation_required {
        bail!("deterministic compensation fixture unexpectedly required confirmation");
    }
    Ok(StartedCompensatedRun {
        request: ExecutionRunRequest {
            tenant_id: session.tenant_id,
            contact_id: None,
            session_id,
            run_uid: started.run.run_uid,
        },
        response: started,
    })
}

fn capability_index(
    capabilities: &[moa_execution::capability::ExecutionCapability],
    remote_name: &str,
) -> Result<usize> {
    capabilities
        .iter()
        .position(|capability| capability.reference.name.ends_with(remote_name))
        .with_context(|| format!("fixture catalog omitted {remote_name}"))
}

fn compensated_plan(
    cancel_policy: ExecutionCancelPolicy,
    forward_node_count: usize,
    forward_reference: CapabilityReference,
    compensator_reference: CapabilityReference,
    mapping: CompensationInputMapping,
) -> ExecutionPlanDefinition {
    let mut nodes = Vec::with_capacity(forward_node_count + 1);
    for index in 1..=forward_node_count {
        let node_id = format!("{FORWARD_NODE_PREFIX}-{index}");
        nodes.push(ExecutionNode {
            id: node_id.clone(),
            requirement_ids: vec![REQUIREMENT_ID.to_string()],
            depends_on: (index > 1)
                .then(|| format!("{FORWARD_NODE_PREFIX}-{}", index - 1))
                .into_iter()
                .collect(),
            when: None,
            input: json!({"effect_id": node_id}),
            output_schema: json!({"type": "object"}),
            operation: ExecutionOperation::Capability {
                reference: forward_reference.clone(),
            },
            compensation: Some(ExecutionCompensation {
                compensator: compensator_reference.clone(),
                input_mapping: mapping.clone(),
            }),
            retry: no_retry(),
            budget: None,
        });
    }
    nodes.push(ExecutionNode {
        id: "output".to_string(),
        requirement_ids: vec![REQUIREMENT_ID.to_string()],
        depends_on: vec![format!("{FORWARD_NODE_PREFIX}-{forward_node_count}")],
        when: None,
        input: json!({}),
        output_schema: json!({"type": "object"}),
        operation: ExecutionOperation::Output {
            value: json!({"completed": true}),
        },
        compensation: None,
        retry: no_retry(),
        budget: None,
    });
    ExecutionPlanDefinition {
        cancel_policy,
        input_schema: json!({
            "type": "object",
            "additionalProperties": false
        }),
        output_schema: json!({"type": "object"}),
        nodes,
    }
}

fn compensation_mapping() -> CompensationInputMapping {
    CompensationInputMapping {
        bindings: vec![CompensationInputBinding {
            target_pointer: "/effect_id".to_string(),
            source: CompensationValueSource::OriginalInput {
                pointer: "/effect_id".to_string(),
            },
        }],
    }
}

fn generous_budget() -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: Some(1_000_000),
        max_tokens: Some(1_000_000),
        max_tasks: Some(64),
        max_tool_calls: Some(64),
        max_retrieved_bytes: Some(1_000_000),
        deadline_at: Some(moa_test_support::fixtures::pg_now() + chrono::Duration::minutes(5)),
    }
}

fn no_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    }
}

async fn await_completed_compensations(
    database_url: &str,
    run_uid: uuid::Uuid,
    expected: i64,
) -> Result<()> {
    let pool = sqlx::PgPool::connect(database_url).await?;
    tokio::time::timeout(SERVICE_TIMEOUT, async {
        loop {
            let observed: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM moa.execution_compensation \
                 WHERE run_uid = $1 AND status = 'completed'",
            )
            .bind(run_uid)
            .fetch_one(&pool)
            .await?;
            if observed == expected {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("run {run_uid} did not persist {expected} completed undo(s)"))??;
    Ok(())
}

async fn load_manual_repair_required(database_url: &str, run_uid: uuid::Uuid) -> Result<bool> {
    sqlx::query_scalar("SELECT manual_repair_required FROM moa.execution_run WHERE run_uid = $1")
        .bind(run_uid)
        .fetch_one(&sqlx::PgPool::connect(database_url).await?)
        .await
        .context("load execution manual-repair projection")
}

async fn await_pending_terminal(database_url: &str, run_uid: uuid::Uuid) -> Result<()> {
    let pool = sqlx::PgPool::connect(database_url).await?;
    tokio::time::timeout(SERVICE_TIMEOUT, async {
        loop {
            let pending: bool = sqlx::query_scalar(
                "SELECT pending_terminal_status IS NOT NULL \
                 FROM moa.execution_run WHERE run_uid = $1",
            )
            .bind(run_uid)
            .fetch_one(&pool)
            .await?;
            if pending {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("run {run_uid} never persisted its terminal fence"))??;
    Ok(())
}

async fn await_transport_attempts(
    controller: &moa_test_support::FixtureCapabilityController,
    expected: usize,
) -> Result<()> {
    tokio::time::timeout(SERVICE_TIMEOUT, async {
        loop {
            if controller.request_count() >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("fixture did not observe {expected} transport attempts"))?;
    Ok(())
}
