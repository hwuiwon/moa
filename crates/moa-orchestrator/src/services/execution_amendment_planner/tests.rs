//! Inline regressions for the bounded amendment-planning slice.

use super::preparation::{
    bounded_failure_evidence, narrow_amendment_context, narrow_authorized_capability_refs,
};
use super::{
    AmendmentPlanningOrigin, ReplanStopReason, amendment_planning_call_estimate,
    amendment_planning_call_usage, amendment_planning_identity, parked_run_needs_amendment,
    planner_stop_amendment,
};

use std::collections::BTreeSet;

use moa_artifacts::execution_plan::{CapabilityReference, ExecutionBudgetLimit};
use moa_core::types::action_policy::{ActionClass, ActionPolicyEffect, RiskLevel};
use moa_core::types::tools::{IdempotencyClass, ToolAsyncMode};
use moa_execution::capability::{
    CapabilityPolicyContext, CapabilitySource, ExecutionCapability, ExecutionCapabilityCatalog,
    ExecutionClass, ExecutionEstimate, amendment_hash,
};
use moa_execution::state::{ExecutionRunStatus, ExecutionTaskId};
use serde_json::json;
use uuid::Uuid;

fn unbounded_budget() -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: None,
        max_tokens: None,
        max_tasks: None,
        max_tool_calls: None,
        max_retrieved_bytes: None,
        deadline_at: None,
    }
}

#[test]
fn amendment_planner_call_estimate_and_usage_are_cost_and_token_only_offline() {
    // Pins: every automatic amendment model call reserves a conservative request-plus-output token
    // bound, then reconciles the provider's exact normalized tokens and model-priced cost.
    let mut request = moa_core::types::completion::CompletionRequest::new("repair the plan");
    request.model = Some(moa_core::types::identifiers::ModelId::new(
        "claude-sonnet-4-6",
    ));
    request.max_output_tokens = Some(32_768);
    let estimate =
        amendment_planning_call_estimate(&request, &moa_config::ExecutionConfig::default())
            .expect("bounded planner request should estimate");
    let expected_input =
        u64::try_from(moa_core::types::context::sum_message_tokens(&request.messages) * 2)
            .expect("fixture estimate should fit u64");
    assert_eq!(estimate.tokens, 32_768 + expected_input);
    assert!(estimate.cost_microusd >= 100_000);

    let response = moa_core::types::completion::CompletionResponse {
        text: "{}".to_string(),
        content: Vec::new(),
        stop_reason: moa_core::types::completion::StopReason::EndTurn,
        model: moa_core::types::identifiers::ModelId::new("claude-sonnet-4-6"),
        usage: moa_core::types::completion::TokenUsage {
            input_tokens_uncached: 1_000,
            input_tokens_cache_write: 200,
            input_tokens_cache_read: 300,
            output_tokens: 400,
        },
        duration_ms: 1,
        thought_signature: None,
    };
    let actual = amendment_planning_call_usage(&response)
        .expect("authoritative provider usage should reconcile");
    assert_eq!(actual.tokens, 1_900);
    assert!(actual.cost_microusd > 0);
}

fn reference(name: &str) -> CapabilityReference {
    CapabilityReference {
        name: name.to_string(),
        version: "1".to_string(),
    }
}

fn tool_capability(reference_name: &str, tool_name: &str) -> ExecutionCapability {
    let source = CapabilitySource::BuiltInTool {
        name: tool_name.to_string(),
    };
    ExecutionCapability {
        reference: reference(reference_name),
        contract_revision: "contract-v1".to_string(),
        description: format!("Capability {reference_name}"),
        input_schema: json!({"type": "object"}),
        output_schema: json!({"type": "object"}),
        action_class: ActionClass::Read,
        risk_level: RiskLevel::Low,
        default_effect: ActionPolicyEffect::Allow,
        idempotency_class: IdempotencyClass::Idempotent,
        async_mode: ToolAsyncMode::SynchronousOnly,
        execution_class: ExecutionClass::Data,
        requires_sandbox: false,
        policy_context: CapabilityPolicyContext::registered(source.clone()),
        source,
        estimate: ExecutionEstimate {
            tool_calls: 1,
            tasks: 1,
            ..ExecutionEstimate::default()
        },
        rollback: None,
    }
}

fn planning_context(
    capabilities: Vec<ExecutionCapability>,
    capability_refs: Vec<CapabilityReference>,
) -> moa_execution::wire::ExecutionPlanningContextSnapshot {
    moa_execution::wire::ExecutionPlanningContextSnapshot {
        schema_version: 1,
        tenant_id: moa_core::types::identifiers::TenantId::new(),
        contact_id: None,
        session_id: moa_core::types::identifiers::SessionId::new(),
        originating_user_sequence_num: 1,
        originating_user_event_hash: moa_execution::capability::ExecutionHash::from_bytes([7; 32])
            .to_string(),
        owner_user_id: moa_core::types::identifiers::UserId::new("planner-owner"),
        catalog: ExecutionCapabilityCatalog::build(capabilities)
            .expect("fixture capability catalog should build"),
        authorization: moa_execution::capability::ExecutionAuthorizationEnvelope {
            capability_refs,
            skill_refs: Vec::new(),
        },
        pinned_instruction_skills: Vec::new(),
        execution_templates: Vec::new(),
        budget: unbounded_budget(),
    }
}

#[test]
fn amendment_live_authority_check_only_removes_persisted_capabilities_offline() {
    // Pins: a live availability set is an intersection with persisted planning authority; it
    // cannot introduce a caller- or model-selected reference the planning context never froze.
    let persisted_a = reference("persisted-a");
    let persisted_b = reference("persisted-b");
    let live_only = reference("live-only");
    let mut authorized = vec![persisted_a, persisted_b.clone()];

    narrow_authorized_capability_refs(&mut authorized, &[persisted_b.clone(), live_only]);

    assert_eq!(authorized, vec![persisted_b]);
}

#[test]
fn amendment_context_drops_capabilities_whose_tool_is_no_longer_registered_offline() {
    // Pins: the narrowed snapshot the planner is shown may not advertise a capability whose
    // governed tool disappeared, so the model cannot propose work dispatch would reject.
    let context = planning_context(
        vec![
            tool_capability("kept", "registered_tool"),
            tool_capability("dropped", "unregistered_tool"),
        ],
        vec![reference("kept"), reference("dropped")],
    );
    let available = BTreeSet::from(["registered_tool".to_string()]);

    let narrowed = narrow_amendment_context(context, &available)
        .expect("narrowed authority should remain a valid planning context");

    assert_eq!(
        narrowed.authorization.capability_refs,
        vec![reference("kept")],
        "only the capability whose tool is still registered may stay authorized"
    );
}

#[test]
fn amendment_failure_evidence_is_preserved_and_bounded_before_provider_use_offline() {
    // Pins: runtime planning keeps exact structured NeedsReplan evidence, while rejecting an
    // over-cap value before any paid model call can be issued for it.
    let evidence = json!({"shape": ["a", "b"]});
    assert_eq!(
        bounded_failure_evidence("shape changed", &evidence)
            .expect("small evidence should remain available"),
        json!({"reason": "shape changed", "evidence": evidence})
    );
    let oversized = json!({
        "body": "x".repeat(moa_core::types::execution_planning::EXECUTION_REPORT_MAX_BYTES)
    });
    let error = bounded_failure_evidence("shape changed", &oversized)
        .expect_err("oversized evidence should fail before planning");
    let message = <restate_sdk::prelude::HandlerError as AsRef<
        dyn std::error::Error + Send + Sync,
    >>::as_ref(&error)
    .to_string();
    assert!(
        message.contains("exceeds the bounded planner envelope"),
        "unexpected rejection: {message}"
    );
}

#[test]
fn one_planning_slice_exists_per_run_revision_offline() {
    // Pins: the planner loop is bounded by plan revision. Repeated controller activations of the
    // same parked revision must coalesce onto one paid invocation, while an accepted amendment
    // that advances the revision must be allowed exactly one more.
    let run_uid = Uuid::from_u128(3);
    let first = amendment_planning_identity(run_uid, 1);
    assert_eq!(first, amendment_planning_identity(run_uid, 1));
    assert_ne!(first, amendment_planning_identity(run_uid, 2));
    assert_ne!(first, amendment_planning_identity(Uuid::from_u128(4), 1));
}

#[test]
fn planner_stop_identity_is_stable_per_reason_and_description_offline() {
    // Pins: `request_replan_stop` keys replay on the amendment hash, so a candidate-free planner
    // stop needs an identity derived only from its own frozen evidence. Two different verdicts
    // must not collide onto one persisted intent.
    let origin = AmendmentPlanningOrigin {
        run_uid: Uuid::from_u128(9),
        session_id: moa_core::types::identifiers::SessionId(Uuid::from_u128(10)),
        base_plan_revision: 2,
        task_id: ExecutionTaskId::from_uuid(Uuid::from_u128(11)),
        task_generation: 1,
    };
    let stop = |reason, detail: &str| {
        amendment_hash(&planner_stop_amendment(origin, reason, detail))
            .expect("planner stop amendment should hash")
    };

    assert_eq!(
        stop(ReplanStopReason::NoProgress, "planner stopped"),
        stop(ReplanStopReason::NoProgress, "planner stopped")
    );
    assert_ne!(
        stop(ReplanStopReason::NoProgress, "planner stopped"),
        stop(ReplanStopReason::BudgetExhausted, "planner stopped")
    );
    assert_ne!(
        stop(ReplanStopReason::NoProgress, "planner stopped"),
        stop(ReplanStopReason::NoProgress, "planner stopped differently")
    );
    assert!(
        planner_stop_amendment(origin, ReplanStopReason::NoProgress, "detail")
            .operations
            .is_empty(),
        "a planner stop must never carry plan operations"
    );
}

#[test]
fn only_a_clean_waiting_replan_park_selects_the_planner_offline() {
    // Pins: a run holding a terminal intent or awaiting manual repair is already being settled,
    // and selecting it for planning would race the settlement that owns it. A run parked for any
    // other reason must not trigger a paid planner call at all.
    assert!(parked_run_needs_amendment(
        ExecutionRunStatus::WaitingReplan,
        false,
        false,
        1
    ));
    for parked in [
        ExecutionRunStatus::Running,
        ExecutionRunStatus::WaitingInput,
        ExecutionRunStatus::WaitingTimer,
        ExecutionRunStatus::WaitingExternal,
        ExecutionRunStatus::Paused,
        ExecutionRunStatus::Completed,
    ] {
        assert!(
            !parked_run_needs_amendment(parked, false, false, 1),
            "{parked:?} must not select amendment planning"
        );
    }
    assert!(
        !parked_run_needs_amendment(ExecutionRunStatus::WaitingReplan, true, false, 1),
        "a run already carrying a terminal intent must not be replanned"
    );
    assert!(
        !parked_run_needs_amendment(ExecutionRunStatus::WaitingReplan, false, true, 1),
        "a run awaiting manual repair must not be replanned"
    );
    for count in [0, 2] {
        assert!(
            !parked_run_needs_amendment(ExecutionRunStatus::WaitingReplan, false, false, count),
            "an amendment may supersede exactly one WaitingReplan task, not {count}"
        );
    }
}

#[tokio::test]
async fn waiting_replan_uses_confirmed_budget_for_planning_apply_and_replay_db() {
    // Pins: confirmation may replace the budget frozen at planning time. Amendment planning must
    // then show the planner only the persisted run ledger, apply the resulting candidate through
    // the production amendment boundary, and replay the exact same submission idempotently.
    use moa_artifacts::execution_plan::{
        CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit, ExecutionCancelPolicy,
        ExecutionGoalContract, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
        ExecutionRequirement, ExecutionTaskOutcome, ExecutionTaskResult, ExecutionTemporalTarget,
        ExecutionUsage, ExecutionWaitExpiryAction, ExecutionWaitPolicy,
        GeneratedAmendmentCandidate, RetryPolicy,
    };
    use moa_core::types::execution_planning::{
        ExecutionSourceProvenance, GeneratedPlanPlannerProvenance,
    };
    use moa_core::types::identifiers::{ModelId, SessionId, TenantId, UserId};
    use moa_execution::compiler::{CompileExecutionRequest, compile};
    use moa_execution::repository::{
        ConfirmationOutcome, ExecutionRepository, ExecutionScope, NewExecutionRun,
        audit::{NewExecutionPlanningContext, PlanningContextWriteOutcome},
        ready::{ReadyMaterializationOutcome, ReadyMaterializationRequest},
        run::RunAdmissionOutcome,
        task::{
            TaskAttemptFence, TaskAttemptReleaseClaimOutcome, TaskAttemptSettlementOutcome,
            TaskAttemptStartOutcome,
        },
    };
    use moa_execution::state::{LogicalTask, LogicalTaskKind};
    use moa_execution::wire::{
        ExecutionAmendmentRequest, ExecutionMutationResponse, ExecutionRunRequest,
        planning_context_hash,
    };
    use moa_providers::ScriptedProvider;

    fn replan_budget(max_resource: u64, max_tasks: u64) -> ExecutionBudgetLimit {
        // Truncated to microseconds so equality against a Postgres round-trip is exact:
        // nanosecond-granular CI clocks otherwise fail assertions local clocks let pass.
        let deadline = chrono::Utc::now() + chrono::TimeDelta::hours(1);
        let deadline =
            chrono::DateTime::<chrono::Utc>::from_timestamp_micros(deadline.timestamp_micros())
                .expect("hour-offset deadline is representable at microsecond precision");
        ExecutionBudgetLimit {
            max_cost_microusd: Some(max_resource),
            max_tokens: Some(max_resource / 10),
            max_tasks: Some(max_tasks),
            max_tool_calls: Some(max_resource / 10_000),
            max_retrieved_bytes: Some(max_resource.saturating_mul(20)),
            deadline_at: Some(deadline),
        }
    }

    fn replan_goal() -> ExecutionGoalContract {
        ExecutionGoalContract {
            objective: "repair with the confirmed budget".to_string(),
            requirements: vec![
                ExecutionRequirement {
                    id: "req_inputs".to_string(),
                    description: "prepare report inputs".to_string(),
                },
                ExecutionRequirement {
                    id: "req_report".to_string(),
                    description: "produce the repaired report".to_string(),
                },
            ],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            // Both requirements are covered by the terminal output check so the linkage
            // survives amendments that rename the prepare/output nodes.
            completion_checks: vec![CompletionCheck {
                id: "check_output".to_string(),
                description: "validate the repaired output".to_string(),
                requirement_ids: vec!["req_inputs".to_string(), "req_report".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        }
    }

    fn replan_node(id: &str, depends_on: Vec<String>, value: serde_json::Value) -> ExecutionNode {
        ExecutionNode {
            id: id.to_string(),
            requirement_ids: vec!["req_report".to_string()],
            depends_on,
            when: None,
            input: json!({}),
            output_schema: json!({"type": "object"}),
            operation: ExecutionOperation::Output { value },
            compensation: None,
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            budget: None,
        }
    }

    fn replan_plan() -> ExecutionPlanDefinition {
        ExecutionPlanDefinition {
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
            input_wait_policy: ExecutionWaitPolicy {
                expiry: ExecutionTemporalTarget::At {
                    at: chrono::Utc::now() + chrono::TimeDelta::minutes(30),
                },
                on_expiry: ExecutionWaitExpiryAction::FailTask,
            },
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            nodes: vec![
                ExecutionNode {
                    id: "prepare".to_string(),
                    requirement_ids: vec!["req_inputs".to_string()],
                    depends_on: Vec::new(),
                    when: None,
                    input: json!({}),
                    output_schema: json!({"type": "object"}),
                    operation: ExecutionOperation::Agent {
                        instructions: "prepare report inputs".to_string(),
                        skill_refs: Vec::new(),
                        capability_refs: Vec::new(),
                        max_turns: 1,
                    },
                    compensation: None,
                    retry: RetryPolicy {
                        max_attempts: 1,
                        initial_backoff_ms: 0,
                        max_backoff_ms: 0,
                    },
                    budget: None,
                },
                replan_node(
                    "output",
                    vec!["prepare".to_string()],
                    json!({"value": "stale"}),
                ),
            ],
        }
    }

    fn replan_task(run_uid: Uuid, node_id: &str, value: serde_json::Value) -> LogicalTask {
        LogicalTask {
            task_id: ExecutionTaskId::derive(run_uid, node_id, "")
                .expect("fixture task id should derive"),
            node_id: node_id.to_string(),
            item_key: String::new(),
            requirement_ids: if node_id == "prepare" {
                vec!["req_inputs".to_string()]
            } else {
                vec!["req_report".to_string()]
            },
            plan_revision: 1,
            generation: 1,
            input: json!({}),
            kind: if node_id == "prepare" {
                LogicalTaskKind::Agent {
                    instructions: "prepare report inputs".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs: Vec::new(),
                    max_turns: 1,
                }
            } else {
                LogicalTaskKind::Output { value }
            },
            compensation: None,
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            reservation: ExecutionEstimate {
                cost_microusd: 2,
                tokens: 2,
                tasks: 1,
                tool_calls: 2,
                retrieved_bytes: 2,
            },
        }
    }

    fn replan_outcome(result: ExecutionTaskResult) -> ExecutionTaskOutcome {
        ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 1,
                tokens: 1,
                tool_calls: 1,
                retrieved_bytes: 1,
            },
            result,
        }
    }

    fn replan_amendment_value() -> serde_json::Value {
        json!({
            "base_plan_revision": 1,
            "reason": "replace stale output",
            "evidence": {"shape": "changed"},
            "operations": [
                {"kind": "remove_pending_node", "node_id": "output"},
                {
                    "kind": "add_node",
                    "node": {
                        "id": "replacement_output",
                        "requirement_ids": ["req_report"],
                        "depends_on": ["prepare"],
                        "when": null,
                        "input": {},
                        "output_schema": {"type": "object"},
                        "operation": {"kind": "output", "value": {"value": "repaired"}},
                        "compensation": null,
                        "retry": {
                            "max_attempts": 1,
                            "initial_backoff_ms": 0,
                            "max_backoff_ms": 0
                        },
                        "budget": null
                    }
                }
            ]
        })
    }

    /// Drives one node through the exact production materialize/admit/settle path.
    async fn run_node_to_outcome(
        repository: &ExecutionRepository,
        scope: ExecutionScope,
        config: &moa_config::ExecutionConfig,
        run_uid: Uuid,
        task: LogicalTask,
        outcome: ExecutionTaskOutcome,
    ) {
        let node_id = task.node_id.clone();
        assert!(matches!(
            repository
                .materialize_ready_page(
                    scope,
                    config,
                    ReadyMaterializationRequest {
                        run_uid,
                        plan_revision: 1,
                        node_id: node_id.clone(),
                        expected_cursor: 0,
                        reduce_cursor: None,
                        source_exhausted: true,
                        terminal_output: None,
                        condition_skipped: false,
                        tasks: vec![task],
                    },
                )
                .await
                .expect("ready page should materialize"),
            ReadyMaterializationOutcome::Applied { .. }
        ));
        let admission = repository
            .admit_ready_attempts(config, 1, chrono::Utc::now())
            .await
            .expect("ready admission should succeed")
            .admitted
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("node `{node_id}` must admit exactly one attempt"));
        let fence = TaskAttemptFence {
            tenant_id: admission.tenant_id,
            run_uid: admission.run_uid,
            task_id: admission.task_id,
            controller_generation: admission.controller_generation,
            attempt_generation: admission.attempt_generation,
            dispatch_uid: admission.dispatch_uid,
            capacity_reservation_uid: admission.capacity_reservation_uid,
            watchdog_trigger_uid: admission.watchdog_trigger_uid,
            attempt_deadline_at: admission.attempt_deadline_at,
        };
        let TaskAttemptStartOutcome::Started(started) = repository
            .start_task_attempt(fence)
            .await
            .expect("attempt start should succeed")
        else {
            panic!("node `{node_id}` attempt must start");
        };
        let settled_at = chrono::Utc::now();
        assert!(matches!(
            repository
                .begin_task_attempt_release(
                    fence,
                    started.task.generation,
                    "fixture_settlement",
                    settled_at,
                )
                .await
                .expect("attempt release should claim"),
            TaskAttemptReleaseClaimOutcome::Applied(_)
        ));
        assert!(matches!(
            repository
                .settle_released_task_attempt(config, fence, outcome, None, settled_at, None)
                .await
                .expect("attempt settlement should succeed"),
            TaskAttemptSettlementOutcome::Applied { .. }
        ));
    }

    let test_db = moa_test_support::postgres::bootstrap_test_db()
        .await
        .expect("execution test database should bootstrap");
    let pool = test_db.store().pool().clone();
    let repository = ExecutionRepository::new(pool.clone());
    let config = moa_config::ExecutionConfig::default();
    let tenant_id = TenantId::new();
    let session_id = SessionId::new();
    let owner_user_id = UserId::new("confirmed-replan-owner");
    let scope = ExecutionScope::Tenant { tenant_id };
    let catalog =
        ExecutionCapabilityCatalog::build(Vec::new()).expect("empty catalog should be valid");
    let authorization = moa_execution::capability::ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: Vec::new(),
    };
    let planning_budget = replan_budget(1_000_000, 10);
    let confirmed_budget = replan_budget(2_000_000, 3);
    let goal = replan_goal();
    let compile_outcome = compile(CompileExecutionRequest {
        goal: goal.clone(),
        plan: replan_plan(),
        run_input: json!({}),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        approved_budget: planning_budget.clone(),
        config: config.clone(),
        now: chrono::Utc::now(),
    });
    let compiled = compile_outcome.compiled.unwrap_or_else(|| {
        panic!(
            "replan fixture should compile within the initial planning budget: {:?}",
            compile_outcome.report.issues
        )
    });
    let compiled_plan = compiled.plan;
    let planning_snapshot = moa_execution::wire::ExecutionPlanningContextSnapshot {
        schema_version: 1,
        tenant_id,
        contact_id: None,
        session_id,
        originating_user_sequence_num: 17,
        originating_user_event_hash: moa_execution::capability::ExecutionHash::from_bytes([17; 32])
            .to_string(),
        owner_user_id: owner_user_id.clone(),
        catalog: catalog.clone(),
        authorization: authorization.clone(),
        pinned_instruction_skills: Vec::new(),
        execution_templates: Vec::new(),
        budget: planning_budget.clone(),
    };
    let planning_hash = planning_context_hash(&planning_snapshot)
        .expect("planning snapshot should have a canonical hash");
    let PlanningContextWriteOutcome::Created(planning_context) = repository
        .create_planning_context(
            scope,
            NewExecutionPlanningContext {
                snapshot: planning_snapshot,
                planning_context_hash: planning_hash,
            },
        )
        .await
        .expect("planning context should persist")
    else {
        panic!("fresh planning context should be created");
    };
    let admitted_identity = moa_core::traits::Identity {
        identity_type: moa_core::traits::IdentityType::Operator,
        id: Uuid::from_u128(1),
        tenant_id,
        api_key_id: None,
        acting_on_behalf_of: None,
    };
    let RunAdmissionOutcome::Admitted(run) = repository
        .create_run(
            scope,
            &config,
            NewExecutionRun {
                tenant_id,
                contact_id: None,
                session_id,
                originating_user_sequence_num: 17,
                planning_context_uid: planning_context.planning_context_uid,
                planning_context_hash: planning_context.planning_context_hash,
                owner_user_id,
                admitted_identity: admitted_identity.clone(),
                goal,
                plan: compiled_plan.clone(),
                catalog,
                authorization,
                pinned_instruction_skills: Vec::new(),
                source_provenance: ExecutionSourceProvenance::GeneratedPlan {
                    planner: GeneratedPlanPlannerProvenance {
                        model: "scripted-confirmed-replan".to_string(),
                        prompt_version: "confirmed-replan".to_string(),
                        candidate_hash: "a".repeat(64),
                        compiler_report_hash: "b".repeat(64),
                        final_plan_hash: compiled_plan.plan_hash.to_string(),
                        repair_attempts: 0,
                    },
                },
                input: json!({}),
                status: moa_execution::state::ExecutionRunStatus::AwaitingConfirmation,
                approved_budget: planning_budget.clone(),
                idempotency_key: Some("confirmed-replan-budget".to_string()),
            },
        )
        .await
        .expect("awaiting-confirmation run should persist")
    else {
        panic!("a fresh idempotency key should admit a new run");
    };
    let ConfirmationOutcome::Confirmed(confirmed) = repository
        .confirm_run(
            scope,
            run.run_uid,
            &run.active_plan_hash,
            confirmed_budget.clone(),
        )
        .await
        .expect("confirmation write should succeed")
    else {
        panic!("confirmation should replace the approved budget");
    };
    assert_eq!(confirmed.approved_budget, confirmed_budget);

    let prepare_task = replan_task(run.run_uid, "prepare", json!({"value": "prepared"}));
    let output_task = replan_task(run.run_uid, "output", json!({"value": "stale"}));
    let waiting_task_id = output_task.task_id;
    run_node_to_outcome(
        &repository,
        scope,
        &config,
        run.run_uid,
        prepare_task,
        replan_outcome(ExecutionTaskResult::Completed {
            output: json!({"value": "prepared"}),
            citations: Vec::new(),
        }),
    )
    .await;
    run_node_to_outcome(
        &repository,
        scope,
        &config,
        run.run_uid,
        output_task,
        replan_outcome(ExecutionTaskResult::NeedsReplan {
            reason: "shape changed".to_string(),
            evidence: json!({"kind": "confirmed-budget"}),
        }),
    )
    .await;

    let target = super::AmendmentPlanningTarget {
        tenant_id,
        contact_id: None,
        session_id,
        run_uid: run.run_uid,
        base_plan_revision: 1,
    };
    let super::AmendmentPlanningInputs::Ready(prepared) =
        super::prepare_amendment_planning(&repository, &config, target, chrono::Utc::now())
            .await
            .expect("confirmed WaitingReplan should prepare amendment planning")
    else {
        panic!("an active WaitingReplan revision should produce planner input");
    };
    assert_eq!(prepared.context.budget, confirmed_budget);
    assert_eq!(
        repository
            .load_planning_context(scope, planning_context.planning_context_uid)
            .await
            .expect("immutable planning context should reload")
            .expect("immutable planning context should remain present")
            .snapshot
            .budget,
        planning_budget,
        "the admission planning context must stay immutable"
    );
    assert_eq!(prepared.remaining_budget.max_cost_microusd, Some(1_999_997));
    assert_eq!(prepared.remaining_budget.max_tokens, Some(199_997));
    assert_eq!(prepared.remaining_budget.max_tasks, Some(1));
    assert_eq!(prepared.admitted_identity, admitted_identity);
    assert_eq!(prepared.origin.task_id, waiting_task_id);
    assert_eq!(
        prepared.evidence.failure_evidence,
        json!({"reason": "shape changed", "evidence": {"kind": "confirmed-budget"}}),
        "exact NeedsReplan evidence must reach the planner"
    );

    let provider = ScriptedProvider::new(moa_core::types::model::ModelCapabilities::default())
        .push_text(json!({"amendment": replan_amendment_value()}).to_string());
    let planned = moa_brain::execution_planning::plan_amendment(
        &provider,
        moa_brain::execution_planning::ExecutionAmendmentPlanningRequest {
            run_uid: run.run_uid,
            base_plan_revision: 1,
            context: prepared.context,
            evidence: prepared.evidence,
            remaining_budget: prepared.remaining_budget,
            planner_model: ModelId::new("scripted-confirmed-replan"),
            config: config.clone(),
            now: prepared.now,
        },
    )
    .await
    .expect("persisted confirmed budget should permit amendment planning");
    assert_eq!(provider.recorded_requests().len(), 1);
    let planner_prompt = serde_json::to_string(&provider.recorded_requests()[0].messages)
        .expect("recorded amendment request should serialize");
    assert!(
        planner_prompt.contains("1999997"),
        "amendment planner prompt must carry the reconciled confirmed budget"
    );
    let moa_brain::execution_planning::ExecutionAmendmentPlanningResultKind::Ready {
        amendment,
        ..
    } = planned.kind
    else {
        panic!("a valid confirmed-budget amendment should be ready");
    };

    let amendment_request = ExecutionAmendmentRequest {
        run: ExecutionRunRequest {
            tenant_id,
            contact_id: None,
            session_id,
            run_uid: run.run_uid,
        },
        expected_plan_revision: 1,
        amendment,
    };
    let applied = crate::services::execution::handlers::apply_amendment_inner(
        pool.clone(),
        config.clone(),
        amendment_request.clone(),
    )
    .await
    .expect("planned amendment should apply through the production service boundary")
    .into_response();
    assert!(
        matches!(applied, ExecutionMutationResponse::Applied { ref run } if run.plan_revision == 2),
        "planned amendment should apply revision two: {applied:?}"
    );
    let replayed = crate::services::execution::handlers::apply_amendment_inner(
        pool,
        config,
        amendment_request.clone(),
    )
    .await
    .expect("exact amendment replay should remain idempotent")
    .into_response();
    assert!(
        matches!(replayed, ExecutionMutationResponse::Replayed { ref run } if run.plan_revision == 2),
        "exact replay must not create a third revision: {replayed:?}"
    );

    let mut injected =
        serde_json::to_value(amendment_request).expect("amendment request should serialize");
    injected
        .as_object_mut()
        .expect("amendment request wire shape should be an object")
        .insert(
            "approved_budget".to_string(),
            json!({"max_tasks": 1_000_000}),
        );
    serde_json::from_value::<ExecutionAmendmentRequest>(injected)
        .expect_err("caller-supplied amendment budget authority must be rejected");
    serde_json::from_value::<GeneratedAmendmentCandidate>(json!({
        "amendment": replan_amendment_value(),
        "approved_budget": {"max_tasks": 1_000_000}
    }))
    .expect_err("model-supplied amendment budget authority must be rejected");
}
