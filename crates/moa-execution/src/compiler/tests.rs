//! Unit tests for deterministic execution compilation.

use std::collections::BTreeMap;

use chrono::Utc;
use moa_artifacts::execution_plan::{
    CompletionCheck, CompletionCheckKind, ExecutionBudgetLimit, ExecutionCancelPolicy,
    ExecutionGoalContract, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
    ExecutionRequirement, PlanAmendment, PlanAmendmentOperation, RetryPolicy,
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
        projection: ExecutionProjection {
            plan_revision: 1,
            node_statuses: BTreeMap::from([(
                "output".to_string(),
                crate::state::ExecutionNodeStatus::Pending,
            )]),
            tasks: Vec::new(),
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
        deadline_at: None,
    }
}
