//! Unit tests for pure execution scheduling.

use chrono::{Duration, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompensationInputMapping, ExecutionBudgetLimit, ExecutionCancelPolicy,
    ExecutionCompensation, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, MapTask, RetryPolicy,
};

use super::*;
use crate::{
    capability::{ExecutionCapabilityCatalog, ExecutionEstimate, ExecutionHash},
    compiler::ExecutionValidationReport,
};

#[test]
fn map_execution_task_validates_the_item_output_schema() {
    // Pins: each materialized map task validates its own result before the
    // scheduler builds and validates the aggregate map-node output.
    let item_schema = serde_json::json!({"type": "object", "required": ["symbol"]});
    let catalog = ExecutionCapabilityCatalog::build(Vec::new()).expect("build empty catalog");
    let plan = CanonicalExecutionPlan {
        definition: ExecutionPlanDefinition {
            schema_version: 2,
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
            input_schema: serde_json::json!({}),
            output_schema: serde_json::json!({}),
            nodes: vec![ExecutionNode {
                id: "quotes".to_string(),
                requirement_ids: vec!["prices".to_string()],
                depends_on: Vec::new(),
                when: None,
                input: serde_json::json!({}),
                output_schema: serde_json::json!({
                    "type": "object",
                    "required": ["items"]
                }),
                operation: ExecutionOperation::Map {
                    items: serde_json::json!([]),
                    item_key: "/symbol".to_string(),
                    max_items: 10,
                    item_output_schema: item_schema,
                    task: MapTask::Agent {
                        instructions: "quote".to_string(),
                        skill_refs: Vec::new(),
                        capability_refs: Vec::new(),
                        max_turns: 1,
                    },
                },
                compensation: None,
                retry: RetryPolicy {
                    max_attempts: 1,
                    initial_backoff_ms: 1,
                    max_backoff_ms: 1,
                },
                budget: None,
            }],
        },
        plan_hash: ExecutionHash::from_bytes([1; 32]),
        catalog_hash: catalog.catalog_hash,
        estimate: ExecutionEstimate::default(),
        report: ExecutionValidationReport::default(),
    };
    let outcome = ExecutionTaskOutcome {
        schema_version: 1,
        usage: moa_artifacts::execution_plan::ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
        result: ExecutionTaskResult::Completed {
            output: serde_json::json!({"symbol": "MOA"}),
            citations: Vec::new(),
        },
    };

    assert!(matches!(
        validate_task_outcome(
            &plan,
            "quotes",
            &LogicalTaskKind::Agent {
                instructions: "quote".to_string(),
                skill_refs: Vec::new(),
                capability_refs: Vec::new(),
                max_turns: 1,
            },
            outcome,
        )
        .result,
        ExecutionTaskResult::Completed { .. }
    ));
}

#[test]
fn empty_map_is_reported_as_first_materialization_without_a_logical_task() {
    // Pins: a valid zero-item map produces a durable marker candidate even though
    // `schedule` cannot return a task row for it.
    let catalog = ExecutionCapabilityCatalog::build(Vec::new()).expect("build empty catalog");
    let map_node = ExecutionNode {
        id: "empty-map".to_string(),
        requirement_ids: Vec::new(),
        depends_on: Vec::new(),
        when: None,
        input: serde_json::json!({}),
        output_schema: serde_json::json!({}),
        operation: ExecutionOperation::Map {
            items: serde_json::json!([]),
            item_key: String::new(),
            max_items: 4,
            item_output_schema: serde_json::json!({}),
            task: MapTask::Agent {
                instructions: "inspect".to_string(),
                skill_refs: Vec::new(),
                capability_refs: Vec::new(),
                max_turns: 1,
            },
        },
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
        },
        budget: None,
    };
    let request = ScheduleRequest {
        run_uid: Uuid::now_v7(),
        goal: ExecutionGoalContract {
            objective: "accept empty input".to_string(),
            requirements: Vec::new(),
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: Vec::new(),
        },
        plan: CanonicalExecutionPlan {
            definition: ExecutionPlanDefinition {
                schema_version: 2,
                cancel_policy: ExecutionCancelPolicy::RetainEffects,
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                nodes: vec![map_node],
            },
            plan_hash: ExecutionHash::from_bytes([1; 32]),
            catalog_hash: catalog.catalog_hash,
            estimate: ExecutionEstimate::default(),
            report: ExecutionValidationReport::default(),
        },
        catalog,
        run_input: serde_json::json!({}),
        projection: ExecutionProjection {
            plan_revision: 1,
            node_statuses: BTreeMap::new(),
            tasks: Vec::new(),
        },
        config: ExecutionConfig::default(),
        budget_ledger: BudgetLedger::new(ExecutionBudgetLimit {
            max_cost_microusd: None,
            max_tokens: None,
            max_tasks: Some(10),
            max_tool_calls: None,
            max_retrieved_bytes: None,
            deadline_at: Some(Utc::now() + Duration::hours(1)),
        }),
        now: Utc::now(),
    };

    assert_eq!(
        ready_empty_map_nodes(&request).expect("derive empty map marker"),
        vec!["empty-map".to_string()]
    );

    let mut nonempty = request;
    let ExecutionOperation::Map { items, .. } = &mut nonempty.plan.definition.nodes[0].operation
    else {
        unreachable!("test plan node must remain a map");
    };
    *items = serde_json::json!([{"id": 1}]);
    assert!(
        ready_empty_map_nodes(&nonempty)
            .expect("derive nonempty map")
            .is_empty()
    );
}

#[test]
fn only_direct_capability_task_materializes_compensation_contract() {
    // Pins: compensation belongs only to a direct capability node; aggregate task
    // expansion cannot inherit or synthesize the node-level rollback contract.
    let catalog = ExecutionCapabilityCatalog::build(Vec::new()).expect("build empty catalog");
    let compensation = ExecutionCompensation {
        compensator: CapabilityReference {
            name: "delete-created-resource".to_string(),
            version: "1".to_string(),
        },
        input_mapping: CompensationInputMapping {
            bindings: Vec::new(),
        },
    };
    let direct_node = ExecutionNode {
        id: "create".to_string(),
        requirement_ids: Vec::new(),
        depends_on: Vec::new(),
        when: None,
        input: serde_json::json!({}),
        output_schema: serde_json::json!({}),
        operation: ExecutionOperation::Capability {
            reference: CapabilityReference {
                name: "create-resource".to_string(),
                version: "1".to_string(),
            },
        },
        compensation: Some(compensation.clone()),
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 1,
            max_backoff_ms: 1,
        },
        budget: None,
    };
    let request = ScheduleRequest {
        run_uid: Uuid::now_v7(),
        goal: ExecutionGoalContract {
            objective: "create then possibly undo".to_string(),
            requirements: Vec::new(),
            deliverables: Vec::new(),
            coverage: Vec::new(),
            constraints: Vec::new(),
            completion_checks: Vec::new(),
        },
        plan: CanonicalExecutionPlan {
            definition: ExecutionPlanDefinition {
                schema_version: 2,
                cancel_policy: ExecutionCancelPolicy::CompensateCommitted,
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                nodes: vec![direct_node.clone()],
            },
            plan_hash: ExecutionHash::from_bytes([2; 32]),
            catalog_hash: catalog.catalog_hash,
            estimate: ExecutionEstimate::default(),
            report: ExecutionValidationReport::default(),
        },
        catalog,
        run_input: serde_json::json!({}),
        projection: ExecutionProjection {
            plan_revision: 1,
            node_statuses: BTreeMap::new(),
            tasks: Vec::new(),
        },
        config: ExecutionConfig::default(),
        budget_ledger: BudgetLedger::new(ExecutionBudgetLimit {
            max_cost_microusd: None,
            max_tokens: None,
            max_tasks: None,
            max_tool_calls: None,
            max_retrieved_bytes: None,
            deadline_at: None,
        }),
        now: Utc::now(),
    };

    let direct = logical_task(
        &request,
        &direct_node,
        String::new(),
        serde_json::json!({}),
        LogicalTaskKind::Capability {
            reference: CapabilityReference {
                name: "create-resource".to_string(),
                version: "1".to_string(),
            },
        },
        ExecutionEstimate {
            tasks: 1,
            ..ExecutionEstimate::default()
        },
    )
    .expect("direct capability task should materialize");
    assert_eq!(direct.compensation, Some(compensation.clone()));

    let mut aggregate_node = direct_node;
    aggregate_node.operation = ExecutionOperation::Map {
        items: serde_json::json!([]),
        item_key: "/id".to_string(),
        max_items: 1,
        item_output_schema: serde_json::json!({}),
        task: MapTask::Capability {
            reference: CapabilityReference {
                name: "create-resource".to_string(),
                version: "1".to_string(),
            },
        },
    };
    let aggregate = logical_task(
        &request,
        &aggregate_node,
        "item-1".to_string(),
        serde_json::json!({}),
        LogicalTaskKind::Capability {
            reference: CapabilityReference {
                name: "create-resource".to_string(),
                version: "1".to_string(),
            },
        },
        ExecutionEstimate {
            tasks: 1,
            ..ExecutionEstimate::default()
        },
    )
    .expect("aggregate capability task should materialize");
    assert_eq!(aggregate.compensation, None);
}
