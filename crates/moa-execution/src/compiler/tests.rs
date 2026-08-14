//! Unit tests for deterministic execution compilation.

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit, ExecutionCancelPolicy,
    ExecutionGoalContract, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
    ExecutionRequirement, ExecutionTemporalTarget, PlanAmendment, PlanAmendmentOperation,
    RetryPolicy,
};
use moa_config::ExecutionConfig;
use serde_json::json;

use super::*;

#[test]
fn execution_planning_compiler_rejects_unserved_immutable_requirement() {
    // Pins: a generated plan cannot silently drop one immutable user requirement.
    let mut request = output_only_compile_request();
    request.goal.requirements.push(ExecutionRequirement {
        id: "req_omitted".to_string(),
        description: "Preserve this second requirement.".to_string(),
    });

    let outcome = compile(request);

    assert!(outcome.compiled.is_none());
    assert_eq!(
        outcome
            .report
            .issues
            .iter()
            .filter(|issue| issue.code == "unserved_requirement")
            .count(),
        1
    );
}

#[test]
fn execution_planning_amendment_cannot_remove_sole_goal_serving_output() {
    // Pins: restricted amendment validation cannot erase goal coverage or terminal output.
    let request = output_only_compile_request();
    let compiled = compile(request.clone())
        .compiled
        .expect("output-only fixture should compile");
    let outcome = validate_amendment(ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            base_plan_revision: 1,
            reason: "Remove the required output".to_string(),
            evidence: json!({ "failure": "none" }),
            operations: vec![PlanAmendmentOperation::RemovePendingNode {
                node_id: "output".to_string(),
            }],
        },
        projection: ExecutionAmendmentProjection {
            plan_revision: 1,
            node_statuses: BTreeMap::from([(
                "output".to_string(),
                crate::state::ExecutionNodeStatus::Pending,
            )]),
            started_node_ids: BTreeSet::new(),
            replan_tasks: Vec::new(),
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: Utc::now(),
    });

    assert!(outcome.plan.is_none());
    assert!(
        outcome.report.issues.iter().any(|issue| {
            issue.code == "unserved_requirement" || issue.code == "plan_structure"
        })
    );
}

#[test]
fn execution_planning_compiler_rejects_completion_metadata_over_activation_bound() {
    // Pins: bounded terminal evaluation never inherits an unbounded goal/check collection from
    // an otherwise valid plan; completion metadata has its own ceiling within the activation.
    let mut request = output_only_compile_request();
    request.config.maximum_activation_steps = 1;

    let outcome = compile(request);

    assert!(outcome.compiled.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| { issue.code == "completion_metadata_exceeds_activation_bound" })
    );
}

#[test]
fn execution_planning_compiler_rejects_plan_nodes_over_activation_bound() {
    // Pins: run admission can seed every node aggregate atomically because canonical plans cannot
    // encode high cardinality as nodes; large work must remain inside pageable map/reduce tasks.
    let mut request = output_only_compile_request();
    request.config.maximum_activation_steps = 2;
    let template = request.plan.nodes[0].clone();
    request.plan.nodes = (0..2)
        .map(|index| ExecutionNode {
            id: format!("output_{index}"),
            ..template.clone()
        })
        .collect();
    let exact_bound = compile(request.clone());
    assert!(
        exact_bound
            .report
            .issues
            .iter()
            .all(|issue| issue.code != "plan_nodes_exceed_activation_bound")
    );

    request.plan.nodes.push(ExecutionNode {
        id: "output_2".to_string(),
        ..template
    });

    let outcome = compile(request);

    assert!(outcome.compiled.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "plan_nodes_exceed_activation_bound")
    );
}

#[test]
fn execution_planning_amendment_rejects_result_over_activation_bound() {
    // Pins: an amendment cannot bypass the initial node bound by appending many small nodes.
    let request = output_only_compile_request();
    let compiled = compile(request.clone())
        .compiled
        .expect("output-only fixture should compile");
    let template = compiled.plan.definition.nodes[0].clone();
    let outcome = validate_amendment(ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            base_plan_revision: 1,
            reason: "Attempt to bypass the node bound".to_string(),
            evidence: json!({}),
            operations: (0..2)
                .map(|index| PlanAmendmentOperation::AddNode {
                    node: ExecutionNode {
                        id: format!("extra_{index}"),
                        ..template.clone()
                    },
                })
                .collect(),
        },
        projection: ExecutionAmendmentProjection {
            plan_revision: 1,
            node_statuses: BTreeMap::from([(
                "output".to_string(),
                crate::state::ExecutionNodeStatus::Completed,
            )]),
            started_node_ids: BTreeSet::from(["output".to_string()]),
            replan_tasks: Vec::new(),
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig {
            maximum_activation_steps: 2,
            ..ExecutionConfig::default()
        },
        now: Utc::now(),
    });

    assert!(outcome.plan.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "plan_nodes_exceed_activation_bound")
    );
}

#[test]
fn execution_planning_compiler_rejects_verifiers_over_dispatch_batch() {
    // Pins: verifier materialization cannot commit more compute-ready tasks than one hard
    // dispatcher batch even when the general activation-step limit is larger.
    let mut request = output_only_compile_request();
    request.config.dispatch_batch_size = 1;
    for index in 0..2 {
        request.goal.completion_checks.push(CompletionCheck {
            id: format!("verifier_{index}"),
            description: format!("Verify completion pass {index}."),
            requirement_ids: vec!["req_report".to_string()],
            constraint_ids: Vec::new(),
            kind: CompletionCheckKind::AgentVerifier {
                instructions: "Return a persisted pass/fail verdict.".to_string(),
                max_turns: 1,
            },
        });
    }

    let outcome = compile(request);

    assert!(outcome.compiled.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| { issue.code == "completion_verifiers_exceed_dispatch_bound" })
    );
}

#[test]
fn execution_planning_compiler_admits_wait_chain_inside_deadline() {
    // Pins: declared-wait feasibility rejects only chains that cannot fit; a plan whose
    // sequential waits sum to less than the horizon still compiles.
    let request = wait_chain_compile_request(&[2, 2, 2], 7);

    let outcome = compile(request);

    assert!(
        outcome.report.issues.is_empty(),
        "feasible wait chain should compile cleanly: {:?}",
        outcome.report.issues
    );
    assert!(outcome.compiled.is_some());
}

#[test]
fn execution_planning_compiler_rejects_sequential_waits_beyond_deadline() {
    // Pins: three sequential three-day waits inside a seven-day run each fit the horizon
    // individually while the chain they form needs nine days; only summing along the path
    // catches it before the run burns six days and dies with partial output.
    let request = wait_chain_compile_request(&[3, 3, 3], 7);

    let outcome = compile(request);

    assert!(outcome.compiled.is_none());
    let issue = outcome
        .report
        .issues
        .iter()
        .find(|issue| issue.code == "declared_waits_exceed_deadline")
        .expect("sequential wait chain should be rejected as infeasible");
    assert_eq!(issue.path, "plan.nodes[2]");
    assert!(
        issue.message.contains("`wait_0` -> `wait_1` -> `wait_2`"),
        "message must name the offending chain: {}",
        issue.message
    );
    assert!(
        issue.message.contains("777600 seconds"),
        "message must state the chain total: {}",
        issue.message
    );
    assert!(
        outcome
            .report
            .issues
            .iter()
            .all(|issue| issue.code != "temporal_target_after_deadline"),
        "each individual wait fits the horizon, so only the path sum may reject this plan"
    );
}

#[test]
fn execution_planning_compiler_rejects_single_wait_beyond_deadline() {
    // Pins: the per-wait horizon rule survives the path check; one oversized wait is still
    // rejected at the wait that declares it.
    let request = wait_chain_compile_request(&[9], 7);

    let outcome = compile(request);

    assert!(outcome.compiled.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "temporal_target_after_deadline"
                && issue.path == "plan.nodes[0].operation.wake")
    );
}

#[test]
fn execution_planning_amendment_rejects_added_wait_beyond_deadline() {
    // Pins: an amendment cannot append a wait that makes the remaining chain overrun the
    // narrowed deadline, and waits already served by completed nodes are not recharged.
    let request = wait_chain_compile_request(&[3, 3], 9);
    let compiled = compile(request.clone())
        .compiled
        .expect("feasible wait chain should compile");
    let now = request.now;
    let outcome = validate_amendment(ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            base_plan_revision: 1,
            reason: "Await one more downstream confirmation".to_string(),
            evidence: json!({ "failure": "none" }),
            operations: vec![PlanAmendmentOperation::AddNode {
                node: wait_node("wait_2", Some("wait_1"), 3),
            }],
        },
        projection: ExecutionAmendmentProjection {
            plan_revision: 1,
            node_statuses: BTreeMap::from([
                ("wait_0".to_string(), ExecutionNodeStatus::Completed),
                ("wait_1".to_string(), ExecutionNodeStatus::Pending),
                ("output".to_string(), ExecutionNodeStatus::Pending),
            ]),
            started_node_ids: BTreeSet::from(["wait_0".to_string()]),
            replan_tasks: Vec::new(),
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: ExecutionBudgetLimit {
            deadline_at: Some(now + chrono::Duration::days(5)),
            ..generous_budget()
        },
        config: ExecutionConfig::default(),
        now,
    });

    assert!(outcome.plan.is_none());
    let issue = outcome
        .report
        .issues
        .iter()
        .find(|issue| issue.code == "declared_waits_exceed_deadline")
        .expect("amendment must not introduce an infeasible wait chain");
    assert!(
        issue.message.contains("518400 seconds"),
        "the completed wait must not be recharged to the remaining chain: {}",
        issue.message
    );
}

fn wait_chain_compile_request(delay_days: &[u64], horizon_days: i64) -> CompileExecutionRequest {
    let mut request = output_only_compile_request();
    let mut nodes = Vec::with_capacity(delay_days.len() + 1);
    let mut previous = None;
    for (index, delay) in delay_days.iter().enumerate() {
        let id = format!("wait_{index}");
        nodes.push(wait_node(&id, previous.as_deref(), *delay));
        previous = Some(id);
    }
    let mut output = request.plan.nodes[0].clone();
    output.depends_on = previous.into_iter().collect();
    nodes.push(output);

    request.plan.nodes = nodes;
    request.approved_budget.deadline_at = Some(request.now + chrono::Duration::days(horizon_days));
    request
}

fn wait_node(id: &str, depends_on: Option<&str>, delay_days: u64) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["req_report".to_string()],
        depends_on: depends_on.map(ToString::to_string).into_iter().collect(),
        when: None,
        input: json!({}),
        output_schema: json!({ "type": "object" }),
        operation: ExecutionOperation::WaitUntil {
            wake: ExecutionTemporalTarget::After {
                delay_seconds: delay_days * 24 * 60 * 60,
            },
            result: json!({}),
        },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        budget: None,
    }
}

fn output_only_compile_request() -> CompileExecutionRequest {
    let catalog = ExecutionCapabilityCatalog::build(Vec::new())
        .expect("empty capability catalog should be valid");
    CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: "Produce the requested report.".to_string(),
            requirements: vec![ExecutionRequirement {
                id: "req_report".to_string(),
                description: "Produce the requested report.".to_string(),
            }],
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: vec![CompletionCheck {
                id: "check_output".to_string(),
                description: "Validate the output schema.".to_string(),
                requirement_ids: vec!["req_report".to_string()],
                constraint_ids: Vec::new(),
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            nodes: vec![ExecutionNode {
                id: "output".to_string(),
                requirement_ids: vec!["req_report".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: json!({}),
                output_schema: json!({ "type": "object" }),
                operation: ExecutionOperation::Output {
                    value: json!({ "status": "complete" }),
                },
                compensation: None,
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 0,
                    max_backoff_ms: 0,
                },
                budget: None,
            }],
        },
        run_input: json!({}),
        authorization: ExecutionAuthorizationEnvelope {
            capability_refs: Vec::new(),
            skill_refs: Vec::new(),
        },
        approved_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: Utc::now(),
        catalog,
    }
}

fn generous_budget() -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: Some(1_000_000),
        max_tokens: Some(100_000),
        max_tasks: Some(100),
        max_tool_calls: Some(100),
        max_retrieved_bytes: Some(1_000_000),
        deadline_at: Some(Utc::now() + chrono::Duration::days(1)),
    }
}
