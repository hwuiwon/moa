use chrono::{DateTime, Utc};
use moa_artifacts::document::{ArtifactDocument, ArtifactStatus};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompensationInputBinding, CompensationInputMapping,
    CompensationValueSource, CompletionCheck, CompletionCheckKind, CoverageRequirement,
    ExecutionCancelPolicy, ExecutionCompensation, ExecutionCondition, ExecutionConstraint,
    ExecutionDeliverable, ExecutionGoalContract, ExecutionNode, ExecutionOperation,
    ExecutionPlanDefinition, ExecutionReducer, ExecutionReference, ExecutionRequirement,
    ExecutionTaskOutcome, ExecutionTaskResult, ExecutionTemporalTarget, ExecutionUsage,
    ExecutionWaitExpiryAction, ExecutionWaitPolicy, InputAudience, MapTask, PlanAmendment,
    PlanAmendmentOperation, RetryPolicy,
};
use moa_artifacts::reference::ArtifactRef;
use moa_artifacts::validation::{
    ValidationReport, validate_execution_goal_contract, validate_execution_plan_definition,
    validate_execution_task_outcome, validate_for_status, validate_plan_amendment,
};
use serde_json::{Value, json};

#[test]
fn all_eight_execution_operations_round_trip_exact_json_and_yaml() {
    // Pins: the public v1 wire shape has exactly the eight contract operations.
    let cases = [
        (
            ExecutionOperation::Capability {
                reference: capability("orders.lookup"),
            },
            json!({
                "kind": "capability",
                "reference": { "name": "orders.lookup", "version": "v1" }
            }),
        ),
        (
            ExecutionOperation::Agent {
                instructions: "Investigate the order.".to_string(),
                skill_refs: vec![skill_ref("support-triage")],
                capability_refs: vec![capability("orders.lookup")],
                max_turns: 3,
            },
            json!({
                "kind": "agent",
                "instructions": "Investigate the order.",
                "skill_refs": ["skill://support-triage"],
                "capability_refs": [{ "name": "orders.lookup", "version": "v1" }],
                "max_turns": 3
            }),
        ),
        (
            ExecutionOperation::Map {
                items: json!({ "$ref": "$.input.orders" }),
                item_key: "/id".to_string(),
                max_items: 100,
                item_output_schema: json!({ "type": "object" }),
                task: MapTask::Capability {
                    reference: capability("orders.lookup"),
                },
            },
            json!({
                "kind": "map",
                "items": { "$ref": "$.input.orders" },
                "item_key": "/id",
                "max_items": 100,
                "item_output_schema": { "type": "object" },
                "task": {
                    "kind": "capability",
                    "reference": { "name": "orders.lookup", "version": "v1" }
                }
            }),
        ),
        (
            ExecutionOperation::Reduce {
                items: json!({ "$ref": "$.nodes.lookup_orders.output.items" }),
                max_items: 100,
                reducer: ExecutionReducer::Agent {
                    instructions: "Merge each structured batch.".to_string(),
                    skill_refs: vec![],
                    capability_refs: vec![capability("reports.merge")],
                    max_turns: 2,
                },
                batch_size: 10,
            },
            json!({
                "kind": "reduce",
                "items": { "$ref": "$.nodes.lookup_orders.output.items" },
                "max_items": 100,
                "reducer": {
                    "kind": "agent",
                    "instructions": "Merge each structured batch.",
                    "skill_refs": [],
                    "capability_refs": [{ "name": "reports.merge", "version": "v1" }],
                    "max_turns": 2
                },
                "batch_size": 10
            }),
        ),
        (
            ExecutionOperation::Review {
                prompt: "Approve the report?".to_string(),
                wait_policy: wait_policy(ExecutionWaitExpiryAction::FailTask),
            },
            json!({
                "kind": "review",
                "prompt": "Approve the report?",
                "wait_policy": {
                    "expiry": { "kind": "after", "delay_seconds": 3600 },
                    "on_expiry": { "kind": "fail_task" }
                }
            }),
        ),
        (
            ExecutionOperation::WaitSignal {
                signal_name: "source_ready".to_string(),
                wait_policy: wait_policy(ExecutionWaitExpiryAction::FailTask),
            },
            json!({
                "kind": "wait_signal",
                "signal_name": "source_ready",
                "wait_policy": {
                    "expiry": { "kind": "after", "delay_seconds": 3600 },
                    "on_expiry": { "kind": "fail_task" }
                }
            }),
        ),
        (
            ExecutionOperation::WaitUntil {
                wake: ExecutionTemporalTarget::At {
                    at: at("2030-01-02T02:00:00Z"),
                },
                result: json!({ "window": "open" }),
            },
            json!({
                "kind": "wait_until",
                "wake": { "kind": "at", "at": "2030-01-02T02:00:00Z" },
                "result": { "window": "open" }
            }),
        ),
        (
            ExecutionOperation::Output {
                value: json!({ "$ref": "$.nodes.review.output" }),
            },
            json!({
                "kind": "output",
                "value": { "$ref": "$.nodes.review.output" }
            }),
        ),
    ];

    assert_eq!(cases.len(), 8);
    for (operation, expected_json) in cases {
        let actual_json = serde_json::to_value(&operation).expect("serialize operation to JSON");
        assert_eq!(actual_json, expected_json);
        assert_eq!(
            serde_json::from_value::<ExecutionOperation>(actual_json)
                .expect("deserialize exact operation JSON"),
            operation
        );

        let yaml = serde_yaml::to_string(&operation).expect("serialize operation to YAML");
        assert_eq!(
            serde_yaml::from_str::<ExecutionOperation>(&yaml)
                .expect("deserialize exact operation YAML"),
            operation
        );
    }
}

#[test]
fn wait_expiry_actions_round_trip_with_canonical_tagged_shapes() {
    // Pins: wait expiry is explicit and every settlement action has one stable wire shape.
    let cases = [
        (
            ExecutionWaitExpiryAction::FailTask,
            json!({ "kind": "fail_task" }),
        ),
        (
            ExecutionWaitExpiryAction::ContinueWith {
                output: json!({ "timed_out": true }),
            },
            json!({
                "kind": "continue_with",
                "output": { "timed_out": true }
            }),
        ),
    ];

    for (action, expected_json) in cases {
        let actual_json = serde_json::to_value(&action).expect("serialize wait expiry action");
        assert_eq!(actual_json, expected_json);
        assert_eq!(
            serde_json::from_value::<ExecutionWaitExpiryAction>(actual_json)
                .expect("deserialize exact wait expiry action"),
            action
        );
    }

    let missing_output = serde_json::from_value::<ExecutionWaitExpiryAction>(json!({
        "kind": "continue_with"
    }));
    assert!(
        missing_output.is_err(),
        "continue_with must declare its deterministic output"
    );
    let unknown_action = serde_json::from_value::<ExecutionWaitExpiryAction>(json!({
        "kind": "skip"
    }));
    assert!(
        unknown_action.is_err(),
        "undeclared wait expiry actions must reject"
    );
    let removed_fail_run = serde_json::from_value::<ExecutionWaitExpiryAction>(json!({
        "kind": "fail_run"
    }));
    assert!(
        removed_fail_run.is_err(),
        "the removed fail_run wire spelling must reject rather than alias fail_task"
    );
}

#[test]
fn temporal_targets_round_trip_and_reject_zero_relative_delay() {
    // Pins: timers use one typed absolute-or-relative target and relative waits always advance time.
    let cases = [
        (
            absolute_target(),
            json!({ "kind": "at", "at": "2030-01-02T02:00:00Z" }),
        ),
        (
            ExecutionTemporalTarget::After {
                delay_seconds: 3_600,
            },
            json!({ "kind": "after", "delay_seconds": 3600 }),
        ),
    ];
    for (target, expected_json) in cases {
        let actual_json = serde_json::to_value(&target).expect("serialize temporal target");
        assert_eq!(actual_json, expected_json);
        assert_eq!(
            serde_json::from_value::<ExecutionTemporalTarget>(actual_json)
                .expect("deserialize exact temporal target"),
            target
        );
    }

    let mut zero_input_wait = valid_plan();
    zero_input_wait.input_wait_policy.expiry = ExecutionTemporalTarget::After { delay_seconds: 0 };
    assert_error(
        &validate_execution_plan_definition(&zero_input_wait),
        "execution_plan.input_wait_policy.expiry",
        "temporal delay_seconds must be at least one",
    );

    let mut zero_timer = valid_plan();
    zero_timer.nodes[0].operation = ExecutionOperation::WaitUntil {
        wake: ExecutionTemporalTarget::After { delay_seconds: 0 },
        result: json!({}),
    };
    assert_error(
        &validate_execution_plan_definition(&zero_timer),
        "execution_plan.nodes[0].operation.wake",
        "temporal delay_seconds must be at least one",
    );
}

#[test]
fn reusable_plan_templates_reject_absolute_temporal_targets_at_every_wait_surface() {
    // Pins: reusable skill templates stay valid over time by expressing waits relative to when
    // each wait state is entered; exact UTC targets remain valid only for one-off plans.
    let mut standalone = valid_plan();
    standalone.input_wait_policy.expiry = absolute_target();
    assert!(
        validate_execution_plan_definition(&standalone).is_ok(),
        "one-off plans may declare an exact UTC input-wait expiry"
    );

    let mut cases = Vec::new();
    cases.push((
        standalone,
        "definition.spec.execution_plan.plan.input_wait_policy.expiry",
    ));

    let mut review = valid_plan();
    review.nodes[0].operation = ExecutionOperation::Review {
        prompt: "Approve?".to_string(),
        wait_policy: ExecutionWaitPolicy {
            expiry: absolute_target(),
            on_expiry: ExecutionWaitExpiryAction::FailTask,
        },
    };
    cases.push((
        review,
        "definition.spec.execution_plan.plan.nodes[0].operation.wait_policy.expiry",
    ));

    let mut signal = valid_plan();
    signal.nodes[0].operation = ExecutionOperation::WaitSignal {
        signal_name: "ready".to_string(),
        wait_policy: ExecutionWaitPolicy {
            expiry: absolute_target(),
            on_expiry: ExecutionWaitExpiryAction::FailTask,
        },
    };
    cases.push((
        signal,
        "definition.spec.execution_plan.plan.nodes[0].operation.wait_policy.expiry",
    ));

    let mut timer = valid_plan();
    timer.nodes[0].operation = ExecutionOperation::WaitUntil {
        wake: absolute_target(),
        result: json!({ "ready": true }),
    };
    cases.push((
        timer,
        "definition.spec.execution_plan.plan.nodes[0].operation.wake",
    ));

    for (plan, expected_path) in cases {
        let document = skill_document_with_plan(plan);
        assert_error(
            &validate_for_status(&document, ArtifactStatus::Draft),
            expected_path,
            "reusable execution templates require an after temporal target",
        );
    }
}

#[test]
fn skill_schema_and_rust_types_require_the_same_wait_contract() {
    // Pins: the hand-maintained skill schema cannot accept stale wait operations or plans that
    // the canonical Rust artifact types reject.
    let skill_schema: Value = serde_json::from_str(include_str!(
        "../../../../docs/schemas/moa-skill-v1.schema.json"
    ))
    .expect("parse skill JSON schema");
    let operation_validator =
        jsonschema::validator_for(&schema_entry_document(&skill_schema, "ExecutionOperation"))
            .expect("compile execution operation schema");
    let plan_validator = jsonschema::validator_for(&schema_entry_document(
        &skill_schema,
        "ExecutionPlanDefinition",
    ))
    .expect("compile execution plan schema");

    let wait_operations = [
        ExecutionOperation::Review {
            prompt: "Approve?".to_string(),
            wait_policy: wait_policy(ExecutionWaitExpiryAction::FailTask),
        },
        ExecutionOperation::WaitSignal {
            signal_name: "ready".to_string(),
            wait_policy: wait_policy(ExecutionWaitExpiryAction::ContinueWith {
                output: json!({ "ready": false }),
            }),
        },
        ExecutionOperation::WaitUntil {
            wake: ExecutionTemporalTarget::After {
                delay_seconds: 3_600,
            },
            result: json!({ "ready": true }),
        },
    ];
    for operation in wait_operations {
        let encoded = serde_json::to_value(operation).expect("serialize wait operation");
        assert!(
            operation_validator.is_valid(&encoded),
            "skill schema must accept canonical operation: {encoded}"
        );
    }

    // A skill is a reusable template, so `validate_skill` passes
    // `allow_absolute_temporal_targets = false` and rejects every absolute target.
    // The schema used to advertise the `at` branch anyway, which meant it accepted
    // documents the canonical validator always refused. Both layers now agree.
    let absolute_timer = json!({
        "kind": "wait_until",
        "wake": { "kind": "at", "at": "2030-01-02T02:00:00Z" },
        "result": { "ready": true }
    });
    assert!(
        serde_json::from_value::<ExecutionOperation>(absolute_timer.clone()).is_ok(),
        "the canonical Rust type still carries the absolute branch for compiled plans"
    );
    assert!(
        !operation_validator.is_valid(&absolute_timer),
        "skill schema must reject an absolute temporal target that skill validation always refuses"
    );

    let stale_review = json!({
        "kind": "review",
        "prompt": "Approve?"
    });
    assert!(
        serde_json::from_value::<ExecutionOperation>(stale_review.clone()).is_err(),
        "Rust type must reject a review without wait_policy"
    );
    assert!(
        !operation_validator.is_valid(&stale_review),
        "skill schema must reject a review without wait_policy"
    );
    let review_with_extra_expiry_field = json!({
        "kind": "review",
        "prompt": "Approve?",
        "wait_policy": {
            "expiry": { "kind": "after", "delay_seconds": 3600 },
            "on_expiry": { "kind": "fail_task", "output": null }
        }
    });
    assert!(
        !operation_validator.is_valid(&review_with_extra_expiry_field),
        "skill schema must reject fields not declared by a fieldless expiry action"
    );
    let zero_delay_timer = json!({
        "kind": "wait_until",
        "wake": { "kind": "after", "delay_seconds": 0 },
        "result": {}
    });
    assert!(
        !operation_validator.is_valid(&zero_delay_timer),
        "skill schema must reject a zero-second relative timer"
    );

    let plan_json = serde_json::to_value(valid_plan()).expect("serialize canonical plan");
    assert!(
        plan_validator.is_valid(&plan_json),
        "skill schema must accept the canonical Rust plan"
    );
    let mut stale_plan = plan_json.clone();
    stale_plan
        .as_object_mut()
        .expect("plan is an object")
        .remove("input_wait_policy");
    assert!(
        serde_json::from_value::<ExecutionPlanDefinition>(stale_plan.clone()).is_err(),
        "Rust type must reject a plan without input_wait_policy"
    );
    assert!(
        !plan_validator.is_valid(&stale_plan),
        "skill schema must reject a plan without input_wait_policy"
    );

    // The plan-level policy settles whichever task returned NeedsInput, so a declared
    // continue_with output has no node output_schema to be checked against. The
    // canonical validator refuses it, and the schema must refuse it at the same
    // place — while still accepting continue_with on a node-owned wait, which does
    // have an owning schema.
    let mut continued_input_wait = plan_json;
    continued_input_wait["input_wait_policy"]["on_expiry"] =
        json!({ "kind": "continue_with", "output": { "approved": true } });
    assert!(
        !plan_validator.is_valid(&continued_input_wait),
        "skill schema must reject continue_with on the plan-level input wait policy"
    );
    let mut definition =
        serde_json::from_value::<ExecutionPlanDefinition>(continued_input_wait.clone())
            .expect("plan-level continue_with is still a well-formed value");
    assert!(
        validate_execution_plan_definition(&definition)
            .errors
            .iter()
            .any(|error| error.path == "execution_plan.input_wait_policy.on_expiry"),
        "canonical validation must reject continue_with on the input wait policy"
    );
    definition.input_wait_policy.on_expiry = ExecutionWaitExpiryAction::FailTask;
    assert!(
        plan_validator.is_valid(&serde_json::to_value(&definition).expect("serialize plan")),
        "schema must still accept a failing input wait expiry"
    );
}

#[test]
fn execution_plan_rejects_unknown_fields_old_operations_and_nested_maps() {
    // Pins: strict v1 model output cannot smuggle controls or revive old/recursive node kinds.
    let mut unknown_plan = serde_json::to_value(valid_plan()).expect("serialize valid plan");
    unknown_plan
        .as_object_mut()
        .expect("plan is an object")
        .insert("authorization".to_string(), json!({ "allow": true }));
    assert!(
        serde_json::from_value::<ExecutionPlanDefinition>(unknown_plan).is_err(),
        "plan-level unknown fields must reject"
    );
    assert!(
        serde_json::from_value::<ExecutionOperation>(json!({
            "kind": "capability",
            "reference": {
                "name": "orders.lookup",
                "version": "v1",
                "catalog_override": true
            }
        }))
        .is_err(),
        "nested compiler-facing objects must reject unknown fields"
    );

    for old_kind in [
        "start",
        "parallel",
        "join",
        "worker",
        "tool",
        "action",
        "skill_action",
        "memory_read",
        "memory_write",
    ] {
        assert!(
            serde_json::from_value::<ExecutionOperation>(json!({ "kind": old_kind })).is_err(),
            "old operation {old_kind} must reject"
        );
    }

    let nested_map = json!({
        "kind": "map",
        "items": [],
        "item_key": "",
        "task": {
            "kind": "map",
            "items": [],
            "item_key": ""
        }
    });
    assert!(
        serde_json::from_value::<ExecutionOperation>(nested_map).is_err(),
        "MapTask must make nested maps unrepresentable"
    );
}

#[test]
fn skill_reference_paths_cover_agent_map_and_reducer_agents_only() {
    // Pins: every execution-plan skill_ref flows through ArtifactDocument's existing resolver input.
    let document = ArtifactDocument::from_json(
        &json!({
            "api_version": "moa.artifact/v1",
            "kind": "skill",
            "metadata": { "name": "reference-paths" },
            "definition": {
                "type": "skill",
                "spec": {
                    "execution_plan": {
                        "goal": {
                            "requirements": [{
                                "id": "req_one",
                                "description": "Resolve all referenced skill work."
                            }],
                            "deliverables": [],
                            "coverage": [],
                            "constraints": [],
                            "completion_checks": []
                        },
                        "plan": {
                            "cancel_policy": "retain_effects",
                            "input_wait_policy": {
                                "expiry": { "kind": "after", "delay_seconds": 3600 },
                                "on_expiry": { "kind": "fail_task" }
                            },
                            "input_schema": { "type": "object" },
                            "output_schema": { "type": "object" },
                            "nodes": [
                            operation_node_json("agent", json!({
                                "kind": "agent",
                                "instructions": "Use the agent skill.",
                                "skill_refs": ["skill://agent-skill"],
                                "capability_refs": [{ "name": "agent.capability", "version": "v1" }],
                                "max_turns": 1
                            })),
                            operation_node_json("map", json!({
                                "kind": "map",
                                "items": [{ "id": "one" }],
                                "item_key": "/id",
                                "max_items": 1,
                                "item_output_schema": { "type": "object" },
                                "task": {
                                    "kind": "agent",
                                    "instructions": "Use the map skill.",
                                    "skill_refs": ["skill://map-skill"],
                                    "capability_refs": [],
                                    "max_turns": 1
                                }
                            })),
                            operation_node_json("reduce", json!({
                                "kind": "reduce",
                                "items": [1, 2],
                                "max_items": 2,
                                "reducer": {
                                    "kind": "agent",
                                    "instructions": "Use the reducer skill.",
                                    "skill_refs": ["skill://reducer-skill"],
                                    "capability_refs": [],
                                    "max_turns": 1
                                },
                                "batch_size": 2
                            })),
                            {
                                "id": "output",
                                "requirement_ids": ["req_one"],
                                "depends_on": ["agent", "map", "reduce"],
                                "input": {},
                                "output_schema": { "type": "object" },
                                "operation": { "kind": "output", "value": {} },
                                "retry": {
                                    "max_attempts": 1,
                                    "initial_backoff_ms": 0,
                                    "max_backoff_ms": 0
                                }
                            }
                            ]
                        }
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("parse execution-plan reference fixture");

    let paths = document
        .reference_paths()
        .into_iter()
        .map(|(path, artifact_ref)| (path, artifact_ref.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            (
                "definition.spec.execution_plan.plan.nodes[0].operation.skill_refs[0]".to_string(),
                "skill://agent-skill".to_string(),
            ),
            (
                "definition.spec.execution_plan.plan.nodes[1].operation.task.skill_refs[0]"
                    .to_string(),
                "skill://map-skill".to_string(),
            ),
            (
                "definition.spec.execution_plan.plan.nodes[2].operation.reducer.skill_refs[0]"
                    .to_string(),
                "skill://reducer-skill".to_string(),
            ),
        ]
    );
}

#[test]
fn execution_plan_rejects_invalid_schema_and_unstable_or_duplicate_ids() {
    // Pins: plans, nodes, and requirement mappings have valid schemas and stable identities.
    let mut plan = valid_plan();
    plan.input_schema = json!([]);
    plan.nodes[0].id = "Bad.ID".to_string();
    plan.nodes[0].requirement_ids = vec!["req_one".to_string(), "req_one".to_string()];
    plan.nodes[1].id = "Bad.ID".to_string();

    let report = validate_execution_plan_definition(&plan);
    assert_error(
        &report,
        "execution_plan.input_schema",
        "JSON schema must be an object",
    );
    assert_error(
        &report,
        "execution_plan.nodes[0].id",
        "execution node id must match [a-z][a-z0-9_-]{0,63}",
    );
    assert_error(
        &report,
        "execution_plan.nodes[0].requirement_ids[1]",
        "duplicate requirement id",
    );
}

#[test]
fn execution_plan_round_trips_without_a_nested_version() {
    // Pins: persisted execution plans use the current canonical shape without nested metadata.
    let plan = valid_plan();
    let encoded = serde_json::to_value(&plan).expect("serialize plan");
    assert!(encoded.get("schema_version").is_none());
    assert_eq!(encoded["cancel_policy"], json!("retain_effects"));
    assert_eq!(
        encoded["input_wait_policy"],
        json!({
            "expiry": { "kind": "after", "delay_seconds": 3600 },
            "on_expiry": { "kind": "fail_task" }
        })
    );
    assert_eq!(
        serde_json::from_value::<ExecutionPlanDefinition>(encoded).expect("deserialize exact plan"),
        plan
    );
}

#[test]
fn compensation_mapping_is_direct_bounded_unique_and_sorted() {
    // Pins: rollback input is a bounded deterministic mapping available only to direct capabilities.
    let mut plan = valid_plan();
    plan.nodes[0].compensation = Some(compensation());
    assert!(
        validate_execution_plan_definition(&plan).is_ok(),
        "well-formed direct capability compensation should validate"
    );

    plan.nodes[0].operation = ExecutionOperation::Agent {
        instructions: "do work".to_string(),
        skill_refs: vec![],
        capability_refs: vec![],
        max_turns: 1,
    };
    assert_error(
        &validate_execution_plan_definition(&plan),
        "execution_plan.nodes[0].compensation",
        "compensation is supported only on direct capability nodes",
    );

    plan.nodes[0].operation = ExecutionOperation::Capability {
        reference: capability("unknown.but.well_formed"),
    };
    {
        let mapping = &mut plan.nodes[0]
            .compensation
            .as_mut()
            .expect("compensation fixture")
            .input_mapping
            .bindings;
        mapping.insert(0, mapping[0].clone());
    }
    assert_error(
        &validate_execution_plan_definition(&plan),
        "execution_plan.nodes[0].compensation.input_mapping.bindings[1].target_pointer",
        "duplicate compensation target pointer",
    );

    {
        let mapping = &mut plan.nodes[0]
            .compensation
            .as_mut()
            .expect("compensation fixture")
            .input_mapping
            .bindings;
        mapping.clear();
        for index in 0..65 {
            mapping.push(CompensationInputBinding {
                target_pointer: format!("/field_{index:02}"),
                source: CompensationValueSource::OriginalInput {
                    pointer: String::new(),
                },
            });
        }
    }
    assert_error(
        &validate_execution_plan_definition(&plan),
        "execution_plan.nodes[0].compensation.input_mapping.bindings",
        "compensation input mapping must include at most 64 bindings",
    );
}

#[test]
fn compensation_mapping_rejects_overlapping_targets_and_malformed_escapes() {
    // Pins: mapping shape is deterministic before execution, including decoded parent/child paths
    // and both source and target RFC 6901 syntax.
    let mut collision = valid_plan();
    collision.nodes[0].compensation = Some(ExecutionCompensation {
        compensator: capability("orders.rollback"),
        input_mapping: CompensationInputMapping {
            bindings: vec![
                CompensationInputBinding {
                    target_pointer: "/resource".to_string(),
                    source: CompensationValueSource::OriginalOutput {
                        pointer: String::new(),
                    },
                },
                CompensationInputBinding {
                    target_pointer: "/resource/id".to_string(),
                    source: CompensationValueSource::OriginalOutput {
                        pointer: "/id".to_string(),
                    },
                },
            ],
        },
    });
    assert_error(
        &validate_execution_plan_definition(&collision),
        "execution_plan.nodes[0].compensation.input_mapping.bindings[1].target_pointer",
        "compensation target pointers must not overlap by parent/child path",
    );

    let mut malformed_source = valid_plan();
    malformed_source.nodes[0].compensation = Some(compensation());
    malformed_source.nodes[0]
        .compensation
        .as_mut()
        .expect("compensation fixture")
        .input_mapping
        .bindings[0]
        .source = CompensationValueSource::OriginalInput {
        pointer: "/order~2id".to_string(),
    };
    assert_error(
        &validate_execution_plan_definition(&malformed_source),
        "execution_plan.nodes[0].compensation.input_mapping.bindings[0].source.pointer",
        "value must be an RFC 6901 JSON Pointer",
    );

    let mut malformed_target = valid_plan();
    malformed_target.nodes[0].compensation = Some(compensation());
    malformed_target.nodes[0]
        .compensation
        .as_mut()
        .expect("compensation fixture")
        .input_mapping
        .bindings[0]
        .target_pointer = "/order~".to_string();
    assert_error(
        &validate_execution_plan_definition(&malformed_target),
        "execution_plan.nodes[0].compensation.input_mapping.bindings[0].target_pointer",
        "value must be an RFC 6901 JSON Pointer",
    );
}

#[test]
fn execution_goal_contract_rejects_unstable_and_duplicate_ids() {
    // Pins: goal requirements and companion contract entries remain individually identifiable.
    let contract = ExecutionGoalContract {
        objective: "Produce a report.".to_string(),
        requirements: vec![ExecutionRequirement {
            id: "Bad".to_string(),
            description: "Cover every order.".to_string(),
        }],
        deliverables: vec![ExecutionDeliverable {
            id: "shared_id".to_string(),
            description: "Final report.".to_string(),
            output_pointer: "not-a-pointer".to_string(),
            schema: json!([]),
        }],
        coverage: vec![CoverageRequirement {
            id: "shared_id".to_string(),
            description: "All orders.".to_string(),
            map_node_id: "Bad.Map".to_string(),
            expected_items: json!([]),
            require_all: true,
        }],
        constraints: vec![ExecutionConstraint {
            id: "constraint_one".to_string(),
            description: "Do not invent facts.".to_string(),
        }],
        completion_checks: vec![CompletionCheck {
            id: "check_one".to_string(),
            description: "Verify the report.".to_string(),
            requirement_ids: vec!["Bad".to_string()],
            constraint_ids: vec!["constraint_one".to_string()],
            kind: CompletionCheckKind::AgentVerifier {
                instructions: "Verify coverage.".to_string(),
                max_turns: 0,
            },
        }],
    };

    let report = validate_execution_goal_contract(&contract);
    assert_error(
        &report,
        "goal_contract.requirements[0].id",
        "goal contract id must match [a-z][a-z0-9_-]{0,63}",
    );
    assert_error(
        &report,
        "goal_contract.coverage[0].id",
        "duplicate goal contract id",
    );
    assert_error(
        &report,
        "goal_contract.deliverables[0].output_pointer",
        "value must be an RFC 6901 JSON Pointer",
    );
    assert_error(
        &report,
        "goal_contract.deliverables[0].schema",
        "JSON schema must be an object",
    );
    assert_error(
        &report,
        "goal_contract.completion_checks[0].kind.max_turns",
        "agent verifier max_turns must be at least one",
    );
}

#[test]
fn execution_plan_rejects_missing_dependencies() {
    // Pins: every declared dependency must identify a node in the same immutable plan.
    let mut plan = valid_plan();
    plan.nodes[1].depends_on = vec!["missing".to_string()];
    let report = validate_execution_plan_definition(&plan);
    assert_error(
        &report,
        "execution_plan.nodes[1].depends_on[0]",
        "execution dependency node does not exist",
    );
}

#[test]
fn execution_plan_rejects_cycles() {
    // Pins: execution plans remain DAGs even when every referenced node exists.
    let mut plan = valid_plan();
    plan.nodes[0].depends_on = vec!["output".to_string()];
    let report = validate_execution_plan_definition(&plan);
    assert_error(
        &report,
        "execution_plan.nodes",
        "execution plan dependencies must be acyclic",
    );
}

#[test]
fn execution_plan_requires_one_terminal_output_with_all_nodes_as_ancestors() {
    // Pins: terminal output is singular, has no dependents, and joins all plan work.
    let mut no_output = valid_plan();
    no_output.nodes[1].operation = ExecutionOperation::Review {
        prompt: "Review".to_string(),
        wait_policy: wait_policy(ExecutionWaitExpiryAction::FailTask),
    };
    assert_error(
        &validate_execution_plan_definition(&no_output),
        "execution_plan.nodes",
        "execution plan must contain exactly one output node",
    );

    let mut multiple = valid_plan();
    multiple.nodes[0].operation = ExecutionOperation::Output { value: json!({}) };
    assert_error(
        &validate_execution_plan_definition(&multiple),
        "execution_plan.nodes",
        "execution plan must contain exactly one output node",
    );

    let mut output_has_dependent = valid_plan();
    output_has_dependent.nodes.push(node(
        "after_output",
        &["output"],
        ExecutionOperation::Review {
            prompt: "Impossible review".to_string(),
            wait_policy: wait_policy(ExecutionWaitExpiryAction::FailTask),
        },
    ));
    let report = validate_execution_plan_definition(&output_has_dependent);
    assert_error(
        &report,
        "execution_plan.nodes[2].depends_on",
        "output node must not have dependents",
    );
    assert_error(
        &report,
        "execution_plan.nodes[2].id",
        "every non-output node must be an ancestor of the output node",
    );

    let mut orphan = valid_plan();
    orphan.nodes.push(node(
        "orphan",
        &[],
        ExecutionOperation::Review {
            prompt: "Unused review".to_string(),
            wait_policy: wait_policy(ExecutionWaitExpiryAction::FailTask),
        },
    ));
    assert_error(
        &validate_execution_plan_definition(&orphan),
        "execution_plan.nodes[2].id",
        "every non-output node must be an ancestor of the output node",
    );
}

#[test]
fn execution_plan_rejects_retry_turn_and_batch_bound_violations() {
    // Pins: retries and all agent/reducer loops are explicitly bounded.
    let mut retry = valid_plan();
    retry.nodes[0].retry.max_attempts = 0;
    retry.nodes[0].retry.initial_backoff_ms = 20;
    retry.nodes[0].retry.max_backoff_ms = 10;
    let report = validate_execution_plan_definition(&retry);
    assert_error(
        &report,
        "execution_plan.nodes[0].retry.max_attempts",
        "retry max_attempts must be at least one",
    );
    assert_error(
        &report,
        "execution_plan.nodes[0].retry.max_backoff_ms",
        "retry max_backoff_ms must be greater than or equal to initial_backoff_ms",
    );

    let mut agent = valid_plan();
    agent.nodes[0].operation = ExecutionOperation::Agent {
        instructions: "Investigate".to_string(),
        skill_refs: vec![],
        capability_refs: vec![],
        max_turns: 0,
    };
    assert_error(
        &validate_execution_plan_definition(&agent),
        "execution_plan.nodes[0].operation.max_turns",
        "agent max_turns must be at least one",
    );

    let mut map_agent = valid_plan();
    map_agent.nodes[0].input = json!({ "item": { "$item": true } });
    map_agent.nodes[0].operation = ExecutionOperation::Map {
        items: json!([{ "id": "one" }]),
        item_key: "/id".to_string(),
        max_items: 1,
        item_output_schema: json!({ "type": "object" }),
        task: MapTask::Agent {
            instructions: "Inspect item".to_string(),
            skill_refs: vec![],
            capability_refs: vec![],
            max_turns: 0,
        },
    };
    assert_error(
        &validate_execution_plan_definition(&map_agent),
        "execution_plan.nodes[0].operation.task.max_turns",
        "agent max_turns must be at least one",
    );

    let mut reduce = valid_plan();
    reduce.nodes[0].operation = ExecutionOperation::Reduce {
        items: json!([1, 2]),
        max_items: 2,
        reducer: ExecutionReducer::Agent {
            instructions: "Reduce".to_string(),
            skill_refs: vec![],
            capability_refs: vec![],
            max_turns: 0,
        },
        batch_size: 1,
    };
    let report = validate_execution_plan_definition(&reduce);
    assert_error(
        &report,
        "execution_plan.nodes[0].operation.batch_size",
        "reduce batch_size must be at least two",
    );
    assert_error(
        &report,
        "execution_plan.nodes[0].operation.reducer.max_turns",
        "agent max_turns must be at least one",
    );
}

#[test]
fn execution_plan_validates_capability_and_skill_reference_syntax_only() {
    // Pins: artifact validation checks syntax/kind without requiring a capability catalog.
    let mut plan = valid_plan();
    plan.nodes[0].operation = ExecutionOperation::Agent {
        instructions: "Investigate".to_string(),
        skill_refs: vec![ArtifactRef::tool("not-a-skill")],
        capability_refs: vec![CapabilityReference {
            name: "bad capability".to_string(),
            version: "bad version".to_string(),
        }],
        max_turns: 1,
    };
    let report = validate_execution_plan_definition(&plan);
    assert_error(
        &report,
        "execution_plan.nodes[0].operation.skill_refs[0]",
        "reference must use skill://",
    );
    assert_error(
        &report,
        "execution_plan.nodes[0].operation.capability_refs[0].name",
        "capability name must be a non-empty ASCII name of at most 256 characters",
    );
    assert_error(
        &report,
        "execution_plan.nodes[0].operation.capability_refs[0].version",
        "capability version must be a non-empty ASCII version of at most 64 characters",
    );

    let unknown_but_well_formed = valid_plan();
    assert!(
        validate_execution_plan_definition(&unknown_but_well_formed).is_ok(),
        "well-formed capability names must not require catalog resolution"
    );
}

#[test]
fn execution_plan_rejects_malformed_hidden_and_recursive_references() {
    // Pins: references use the restricted grammar and only read run input or direct dependencies.
    let mut malformed = valid_plan();
    malformed.nodes[0].input = json!({ "$ref": "$.state.value" });
    assert_error(
        &validate_execution_plan_definition(&malformed),
        "execution_plan.nodes[0].input",
        "execution reference must target $.input or $.nodes.<id>.output",
    );

    let mut hidden = valid_plan();
    hidden.nodes.insert(
        1,
        node(
            "middle",
            &["lookup"],
            ExecutionOperation::Review {
                prompt: "Review".to_string(),
                wait_policy: wait_policy(ExecutionWaitExpiryAction::FailTask),
            },
        ),
    );
    hidden.nodes[2].depends_on = vec!["middle".to_string()];
    hidden.nodes[2].operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup.output" }),
    };
    assert_error(
        &validate_execution_plan_definition(&hidden),
        "execution_plan.nodes[2].operation.value",
        "execution reference may only read a declared dependency output",
    );

    let mut hidden_timer_result = valid_plan();
    hidden_timer_result.nodes[0].operation = ExecutionOperation::WaitUntil {
        wake: ExecutionTemporalTarget::At {
            at: at("2030-01-02T02:00:00Z"),
        },
        result: json!({ "$ref": "$.nodes.output.output" }),
    };
    assert_error(
        &validate_execution_plan_definition(&hidden_timer_result),
        "execution_plan.nodes[0].operation.result",
        "execution reference may only read a declared dependency output",
    );

    let mut hidden_expiry_output = valid_plan();
    hidden_expiry_output.nodes[0].operation = ExecutionOperation::Review {
        prompt: "Approve?".to_string(),
        wait_policy: wait_policy(ExecutionWaitExpiryAction::ContinueWith {
            output: json!({ "$ref": "$.nodes.output.output" }),
        }),
    };
    assert_error(
        &validate_execution_plan_definition(&hidden_expiry_output),
        "execution_plan.nodes[0].operation.wait_policy.on_expiry.output",
        "execution reference may only read a declared dependency output",
    );

    let mut recursive = valid_plan();
    recursive.nodes[0].input = json!({ "$ref": "$.nodes.lookup.output" });
    assert_error(
        &validate_execution_plan_definition(&recursive),
        "execution_plan.nodes[0].input",
        "execution reference cannot recursively reference its node",
    );

    let mut extra_binding_field = valid_plan();
    extra_binding_field.nodes[0].input =
        json!({ "$ref": "$.input.order_id", "default": "missing" });
    assert_error(
        &validate_execution_plan_definition(&extra_binding_field),
        "execution_plan.nodes[0].input",
        "dynamic binding must be an object containing exactly one supported key",
    );
}

#[test]
fn execution_plan_rejects_map_variables_outside_map_task_input() {
    // Pins: $item and $item_key cannot leak into ordinary nodes, map item sources, or output.
    let mut plan = valid_plan();
    plan.nodes[0].input = json!({ "$item": true });
    let report = validate_execution_plan_definition(&plan);
    assert_error(
        &report,
        "execution_plan.nodes[0].input",
        "map variables are only valid inside a map task input",
    );

    let mut valid_map = valid_plan();
    valid_map.nodes[0].input = json!({
        "item": { "$item": true },
        "key": { "$item_key": true }
    });
    valid_map.nodes[0].operation = ExecutionOperation::Map {
        items: json!([{ "id": "one" }, { "id": "two" }]),
        item_key: "/id".to_string(),
        max_items: 2,
        item_output_schema: json!({ "type": "object" }),
        task: MapTask::Capability {
            reference: capability("orders.lookup"),
        },
    };
    assert!(
        validate_execution_plan_definition(&valid_map).is_ok(),
        "map task input should admit both map variables"
    );

    if let ExecutionOperation::Map { items, .. } = &mut valid_map.nodes[0].operation {
        *items = json!({ "$item": true });
    }
    assert_error(
        &validate_execution_plan_definition(&valid_map),
        "execution_plan.nodes[0].operation.items",
        "map variables are only valid inside a map task input",
    );
}

#[test]
fn execution_plan_rejects_invalid_map_pointers_and_duplicate_static_keys() {
    // Pins: static maps expose valid RFC 6901 pointers and stable unique logical task keys.
    let mut plan = valid_plan();
    plan.nodes[0].input = json!({ "item": { "$item": true } });
    plan.nodes[0].operation = ExecutionOperation::Map {
        items: json!([{ "id": "same" }, { "id": "same" }]),
        item_key: "id".to_string(),
        max_items: 2,
        item_output_schema: json!({ "type": "object" }),
        task: MapTask::Capability {
            reference: capability("orders.lookup"),
        },
    };
    assert_error(
        &validate_execution_plan_definition(&plan),
        "execution_plan.nodes[0].operation.item_key",
        "value must be an RFC 6901 JSON Pointer",
    );

    if let ExecutionOperation::Map { item_key, .. } = &mut plan.nodes[0].operation {
        *item_key = "/id".to_string();
    }
    assert_error(
        &validate_execution_plan_definition(&plan),
        "execution_plan.nodes[0].operation.items[1]",
        "map item_key values must be unique",
    );
}

#[test]
fn outcome_and_amendment_envelopes_reject_graph_replacement() {
    // Pins: task outcomes and amendments cannot carry undeclared graph, budget, or authorization controls.
    let outcome = ExecutionTaskOutcome {
        schema_version: 2,
        usage: ExecutionUsage {
            cost_microusd: 1,
            tokens: 2,
            tool_calls: 3,
            retrieved_bytes: 4,
        },
        result: ExecutionTaskResult::Completed {
            output: json!({ "ok": true }),
            citations: vec![],
        },
    };
    assert_error(
        &validate_execution_task_outcome(&outcome),
        "execution_task_outcome.schema_version",
        "schema_version must equal 1",
    );
    assert!(
        serde_json::from_value::<ExecutionTaskOutcome>(json!({
            "schema_version": 1,
            "status": "needs_input",
            "question": "Which source?",
            "audience": "user",
            "plan": { "nodes": [] }
        }))
        .is_err(),
        "outcome unknown graph fields must reject"
    );

    let valid_amendment = PlanAmendment {
        base_plan_revision: 3,
        reason: "Need another source.".to_string(),
        evidence: json!({ "missing": "source" }),
        operations: vec![],
    };
    let encoded = serde_json::to_value(&valid_amendment).expect("serialize amendment");
    assert!(encoded.get("schema_version").is_none());
    assert_eq!(
        serde_json::from_value::<PlanAmendment>(encoded).expect("deserialize exact amendment"),
        valid_amendment
    );

    let amendment = PlanAmendment {
        base_plan_revision: 3,
        reason: "Need another source.".to_string(),
        evidence: json!({ "missing": "source" }),
        operations: vec![PlanAmendmentOperation::RemovePendingNode {
            node_id: "Bad.Node".to_string(),
        }],
    };
    let report = validate_plan_amendment(&amendment);
    assert_error(
        &report,
        "plan_amendment.operations[0].node_id",
        "pending node id must match [a-z][a-z0-9_-]{0,63}",
    );

    for forbidden in ["plan", "nodes", "budget", "authorization"] {
        let mut value = json!({
            "base_plan_revision": 3,
            "reason": "Need another source.",
            "evidence": {},
            "operations": []
        });
        value
            .as_object_mut()
            .expect("amendment is an object")
            .insert(forbidden.to_string(), json!({}));
        assert!(
            serde_json::from_value::<PlanAmendment>(value).is_err(),
            "amendment field {forbidden} must reject"
        );
    }
    assert!(
        serde_json::from_value::<PlanAmendmentOperation>(json!({
            "kind": "replace_plan",
            "plan": valid_plan()
        }))
        .is_err(),
        "full graph replacement must not be representable"
    );
}

#[test]
fn execution_task_outcome_enforces_512_character_citation_id_limit() {
    // Pins: citation/source identifiers use the documented character bound, so a 512-character
    // Unicode identifier remains valid while the 513th character is rejected before persistence.
    let outcome_with_source_id = |source_id: String| ExecutionTaskOutcome {
        schema_version: 1,
        usage: ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
        result: ExecutionTaskResult::Completed {
            output: json!({ "ok": true }),
            citations: vec![moa_artifacts::execution_plan::ExecutionCitation {
                source_id,
                uri: None,
                locator: None,
            }],
        },
    };

    let boundary = validate_execution_task_outcome(&outcome_with_source_id("é".repeat(512)));
    assert!(
        boundary.is_ok(),
        "512 Unicode scalar values must remain valid, got {:?}",
        boundary.errors
    );

    assert_error(
        &validate_execution_task_outcome(&outcome_with_source_id("é".repeat(513))),
        "execution_task_outcome.citations[0].source_id",
        "citation source_id must be at most 512 characters",
    );
}

#[test]
fn conditional_nodes_are_effectful_leaves_nothing_reads_or_depends_on_for_completion() {
    // Pins: the artifact layer refuses a plan whose meaning needs a conditional node to have
    // run, before the plan ever reaches a compiler. A false condition commits a skipped node
    // with a null output, so reading that output, making it the terminal output, or serving a
    // requirement only through conditional nodes each describes a run nobody can satisfy.
    let mut read_output = valid_plan();
    read_output.nodes[0].when = Some(ExecutionCondition::Exists {
        reference: ExecutionReference {
            path: "$.input.order_id".to_string(),
        },
    });
    assert_error(
        &validate_execution_plan_definition(&read_output),
        "execution_plan.nodes[1].operation.value",
        "a conditional node's output cannot be read; it is null when the condition is false",
    );

    let mut conditional_output = valid_plan();
    conditional_output.nodes[1].when = Some(ExecutionCondition::Exists {
        reference: ExecutionReference {
            path: "$.input.order_id".to_string(),
        },
    });
    assert_error(
        &validate_execution_plan_definition(&conditional_output),
        "execution_plan.nodes[1].when",
        "the terminal output node must not be conditional",
    );

    let mut only_conditional = valid_plan();
    only_conditional.nodes[0].when = Some(ExecutionCondition::Exists {
        reference: ExecutionReference {
            path: "$.input.order_id".to_string(),
        },
    });
    only_conditional.nodes[0].requirement_ids = vec!["req_branch".to_string()];
    assert_error(
        &validate_execution_plan_definition(&only_conditional),
        "execution_plan.nodes",
        "every node serving requirement `req_branch` is conditional, so all branches \
         evaluating false would leave it with no eligible node",
    );
}

#[test]
fn conditions_use_the_same_reference_visibility_rules() {
    // Pins: conditional execution cannot inspect undeclared node output.
    let mut plan = valid_plan();
    plan.nodes[0].when = Some(ExecutionCondition::Exists {
        reference: ExecutionReference {
            path: "$.nodes.output.output".to_string(),
        },
    });
    assert_error(
        &validate_execution_plan_definition(&plan),
        "execution_plan.nodes[0].when.reference.$ref",
        "execution reference may only read a declared dependency output",
    );
}

#[test]
fn task_outcome_variants_round_trip_without_extra_envelope_fields() {
    // Pins: every typed task result shares the same versioned flattened envelope.
    let outcomes = [
        ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 1,
                tokens: 2,
                tool_calls: 3,
                retrieved_bytes: 4,
            },
            result: ExecutionTaskResult::NeedsInput {
                question: "Approve?".to_string(),
                audience: InputAudience::TenantAdmin,
            },
        },
        ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 5,
                tokens: 6,
                tool_calls: 7,
                retrieved_bytes: 8,
            },
            result: ExecutionTaskResult::NeedsReplan {
                reason: "Capability unavailable".to_string(),
                evidence: json!({ "capability": "orders.lookup" }),
            },
        },
        ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 9,
                tokens: 10,
                tool_calls: 11,
                retrieved_bytes: 12,
            },
            result: ExecutionTaskResult::UnknownOutcome {
                message: "effect settlement is ambiguous".to_string(),
            },
        },
    ];
    for outcome in outcomes {
        let value = serde_json::to_value(&outcome).expect("serialize task outcome");
        assert_eq!(
            serde_json::from_value::<ExecutionTaskOutcome>(value)
                .expect("deserialize task outcome"),
            outcome
        );
    }
}

fn valid_plan() -> ExecutionPlanDefinition {
    ExecutionPlanDefinition {
        cancel_policy: ExecutionCancelPolicy::RetainEffects,
        input_wait_policy: wait_policy(ExecutionWaitExpiryAction::FailTask),
        input_schema: json!({ "type": "object" }),
        output_schema: json!({ "type": "object" }),
        nodes: vec![
            node(
                "lookup",
                &[],
                ExecutionOperation::Capability {
                    reference: capability("unknown.but.well_formed"),
                },
            ),
            node(
                "output",
                &["lookup"],
                ExecutionOperation::Output {
                    value: json!({ "$ref": "$.nodes.lookup.output" }),
                },
            ),
        ],
    }
}

fn wait_policy(on_expiry: ExecutionWaitExpiryAction) -> ExecutionWaitPolicy {
    ExecutionWaitPolicy {
        expiry: ExecutionTemporalTarget::After {
            delay_seconds: 3_600,
        },
        on_expiry,
    }
}

fn at(value: &str) -> DateTime<Utc> {
    value.parse().expect("fixture timestamp must be RFC 3339")
}

fn absolute_target() -> ExecutionTemporalTarget {
    ExecutionTemporalTarget::At {
        at: at("2030-01-02T02:00:00Z"),
    }
}

fn skill_document_with_plan(plan: ExecutionPlanDefinition) -> ArtifactDocument {
    ArtifactDocument::from_json(
        &json!({
            "api_version": "moa.artifact/v1",
            "kind": "skill",
            "metadata": { "name": "temporal-template" },
            "definition": {
                "type": "skill",
                "spec": {
                    "execution_plan": {
                        "goal": {
                            "requirements": [{
                                "id": "req_one",
                                "description": "Complete the temporal work."
                            }],
                            "deliverables": [],
                            "coverage": [],
                            "constraints": [],
                            "completion_checks": []
                        },
                        "plan": plan
                    }
                }
            }
        })
        .to_string(),
    )
    .expect("parse temporal skill template fixture")
}

fn schema_entry_document(schema: &Value, definition: &str) -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$ref": format!("#/$defs/{definition}"),
        "$defs": schema["$defs"].clone()
    })
}

fn operation_node_json(id: &str, operation: Value) -> Value {
    json!({
        "id": id,
        "requirement_ids": ["req_one"],
        "depends_on": [],
        "input": if id == "map" {
            json!({ "item": { "$item": true } })
        } else {
            json!({})
        },
        "output_schema": { "type": "object" },
        "operation": operation,
        "retry": {
            "max_attempts": 1,
            "initial_backoff_ms": 0,
            "max_backoff_ms": 0
        }
    })
}

fn node(id: &str, depends_on: &[&str], operation: ExecutionOperation) -> ExecutionNode {
    ExecutionNode {
        id: id.to_string(),
        requirement_ids: vec!["req_one".to_string()],
        depends_on: depends_on.iter().map(|id| (*id).to_string()).collect(),
        when: None,
        input: Value::Object(serde_json::Map::new()),
        output_schema: json!({ "type": "object" }),
        operation,
        compensation: None,
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            max_backoff_ms: 0,
        },
        budget: None,
    }
}

fn compensation() -> ExecutionCompensation {
    ExecutionCompensation {
        compensator: capability("orders.rollback"),
        input_mapping: CompensationInputMapping {
            bindings: vec![CompensationInputBinding {
                target_pointer: "/order_id".to_string(),
                source: CompensationValueSource::OriginalInput {
                    pointer: "/order_id".to_string(),
                },
            }],
        },
    }
}

fn capability(name: &str) -> CapabilityReference {
    CapabilityReference {
        name: name.to_string(),
        version: "v1".to_string(),
    }
}

fn skill_ref(name: &str) -> ArtifactRef {
    ArtifactRef::artifact(
        moa_artifacts::document::ArtifactKind::Skill,
        name.to_string(),
    )
}

fn assert_error(report: &ValidationReport, path: &str, message: &str) {
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.path == path && error.message == message),
        "expected validation error at {path} with message {message:?}, got {:?}",
        report.errors
    );
}
