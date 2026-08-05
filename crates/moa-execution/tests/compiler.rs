use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use chrono::{TimeZone, Utc};
use moa_artifacts::execution_plan::{
    CapabilityReference, CompensationInputBinding, CompensationInputMapping,
    CompensationValueSource, CompletionCheck, CompletionCheckKind, CoverageRequirement,
    EXECUTION_PLAN_SCHEMA_VERSION, ExecutionBudgetLimit, ExecutionCancelPolicy,
    ExecutionCompensation, ExecutionCondition, ExecutionDeliverable, ExecutionGoalContract,
    ExecutionNode, ExecutionOperation, ExecutionPlanDefinition, ExecutionReducer,
    ExecutionReference, ExecutionRequirement, ExecutionTaskOutcome, ExecutionTaskResult,
    ExecutionUsage, MapTask, PLAN_AMENDMENT_SCHEMA_VERSION, PlanAmendment, PlanAmendmentOperation,
    RetryPolicy,
};
use moa_artifacts::reference::ArtifactRef;
use moa_config::ExecutionConfig;
use moa_core::types::{
    action_policy::{ActionClass, ActionPolicyEffect, RiskLevel},
    identifiers::ConnectorConnectionId,
    tools::IdempotencyClass,
};
use moa_execution::{
    capability::{
        CapabilityPolicyContext, CapabilityRollbackContract, CapabilitySource,
        ExecutionAuthorizationEnvelope, ExecutionCapability, ExecutionCapabilityCatalog,
        ExecutionClass, ExecutionEstimate, catalog_hash, plan_hash,
    },
    compiler::{
        CompileExecutionOutcome, CompileExecutionRequest, ExecutionValidationIssue,
        ExecutionValidationSeverity, ValidateAmendmentRequest, compile, validate_amendment,
    },
    state::{
        ExecutionNodeStatus, ExecutionProjection, ExecutionTaskId, ExecutionTaskProjection,
        ExecutionTaskStatus,
    },
};
use proptest::{
    prelude::*,
    test_runner::{Config as ProptestConfig, FileFailurePersistence},
};
use serde_json::json;
use uuid::Uuid;

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn property_compiled_plans_are_acyclic_with_one_reachable_output(
        capability_node_count in 1_usize..=8,
        inject_cycle in any::<bool>(),
    ) {
        // Pins: every accepted generated plan is a DAG with one reachable terminal output.
        let mut request = valid_request();
        let reference = request.catalog.capabilities[0].reference.clone();
        let mut nodes = Vec::with_capacity(capability_node_count + 1);
        for index in 0..capability_node_count {
            let id = format!("work_{index}");
            let depends_on = index
                .checked_sub(1)
                .map(|dependency| vec![format!("work_{dependency}")])
                .unwrap_or_default();
            nodes.push(ExecutionNode {
                id,
                requirement_ids: vec!["req_one".to_string()],
                depends_on,
                when: None,
                input: json!({ "order_id": { "$ref": "$.input.order_id" } }),
                output_schema: json!({ "type": "object" }),
                operation: ExecutionOperation::Capability {
                    reference: reference.clone(),
                },
                compensation: None,
                retry: retry(1),
                budget: None,
            });
        }
        let last_work = format!("work_{}", capability_node_count - 1);
        nodes.push(ExecutionNode {
            id: "output".to_string(),
            requirement_ids: vec!["req_one".to_string()],
            depends_on: vec![last_work.clone()],
            when: None,
            input: json!({}),
            output_schema: json!({ "type": "object" }),
            operation: ExecutionOperation::Output {
                value: json!({ "$ref": format!("$.nodes.{last_work}.output") }),
            },
            compensation: None,
            retry: retry(1),
            budget: None,
        });
        if inject_cycle {
            nodes[0].depends_on = vec!["output".to_string()];
        }
        request.plan.nodes = nodes;

        let outcome = compile(request);
        match outcome.compiled {
            Some(compiled) => {
                prop_assert!(!inject_cycle, "compiler accepted an injected cycle");
                let compiled_nodes = &compiled.plan.definition.nodes;
                prop_assert!(is_acyclic(compiled_nodes));
                let output_ids = compiled_nodes
                    .iter()
                    .filter(|node| matches!(node.operation, ExecutionOperation::Output { .. }))
                    .map(|node| node.id.as_str())
                    .collect::<Vec<_>>();
                prop_assert_eq!(output_ids.len(), 1);
                let reachable = reachable_node_ids(compiled_nodes);
                prop_assert!(reachable.contains(output_ids[0]));
            }
            None => {
                prop_assert!(
                    inject_cycle,
                    "compiler rejected an acyclic generated plan: {:?}",
                    outcome.report.issues
                );
            }
        }
    }
}

fn property_config() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "proptest-regressions/properties.txt",
        ))),
        ..ProptestConfig::default()
    }
}

fn is_acyclic(nodes: &[ExecutionNode]) -> bool {
    let mut remaining = nodes
        .iter()
        .map(|node| (node.id.as_str(), node.depends_on.len()))
        .collect::<BTreeMap<_, _>>();
    let mut completed = BTreeSet::new();
    loop {
        let ready = remaining
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if ready.is_empty() {
            break;
        }
        for id in ready {
            remaining.remove(id);
            completed.insert(id);
            for node in nodes
                .iter()
                .filter(|node| node.depends_on.iter().any(|dep| dep == id))
            {
                if let Some(count) = remaining.get_mut(node.id.as_str()) {
                    *count -= 1;
                }
            }
        }
    }
    completed.len() == nodes.len()
}

fn reachable_node_ids(nodes: &[ExecutionNode]) -> BTreeSet<&str> {
    let mut reachable = nodes
        .iter()
        .filter(|node| node.depends_on.is_empty())
        .map(|node| node.id.as_str())
        .collect::<BTreeSet<_>>();
    loop {
        let before = reachable.len();
        for node in nodes {
            if node
                .depends_on
                .iter()
                .all(|dependency| reachable.contains(dependency.as_str()))
            {
                reachable.insert(node.id.as_str());
            }
        }
        if reachable.len() == before {
            return reachable;
        }
    }
}

#[test]
fn compile_returns_canonical_hashes_and_exact_retry_estimate() {
    // Pins: compiler output carries exact domain hashes and retry attempts multiply resources only.
    let request = valid_request();
    let expected_plan_hash = plan_hash(&request.plan).expect("hash plan");
    let expected_catalog_hash = request.catalog.catalog_hash;

    let outcome = compile(request);
    assert!(
        outcome.report.issues.is_empty(),
        "{:?}",
        outcome.report.issues
    );
    let compiled = outcome.compiled.expect("valid plan compiles");
    assert_eq!(compiled.plan.plan_hash, expected_plan_hash);
    assert_eq!(compiled.plan.catalog_hash, expected_catalog_hash);
    assert_eq!(
        compiled.plan.estimate,
        ExecutionEstimate {
            cost_microusd: 14,
            tokens: 22,
            tool_calls: 6,
            retrieved_bytes: 26,
            tasks: 2,
        }
    );
}

#[test]
fn plan_hash_treats_node_declaration_order_as_nonsemantic() {
    // Pins: an amended DAG that returns to the same nodes cannot evade duplicate-plan detection
    // merely because remove/add operations changed node array order.
    let request = valid_request();
    let mut reordered = request.plan.clone();
    reordered.nodes.reverse();

    assert_eq!(
        plan_hash(&request.plan).expect("hash original plan"),
        plan_hash(&reordered).expect("hash reordered plan")
    );
}

#[test]
fn compile_rejects_cycles_before_canonicalization() {
    // Pins: cycle rejection is a compiler gate and mutation-check target.
    let mut request = valid_request();
    request.plan.nodes[0].depends_on = vec!["output".to_string()];

    let outcome = compile(request);
    assert!(outcome.compiled.is_none());
    assert!(outcome.report.issues.iter().any(|issue| {
        issue.code == "plan_structure"
            && issue.message == "execution plan dependencies must be acyclic"
    }));
}

#[test]
fn compile_rejects_an_unpinned_capability_contract() {
    // Pins: every durable capability must carry the exact governed contract
    // revision later checked by policy evaluation and ToolExecutor dispatch.
    let mut request = valid_request();
    request.catalog.capabilities[0].contract_revision.clear();
    request.catalog.catalog_hash = catalog_hash(1, &request.catalog.capabilities)
        .expect("rehash catalog with missing contract revision");

    let outcome = compile(request);
    assert!(outcome.compiled.is_none());
    assert!(outcome.report.issues.iter().any(|issue| {
        issue.code == "empty_capability_contract_revision"
            && issue.path == "catalog.capabilities[0].contract_revision"
    }));
}

#[test]
fn compile_rejects_external_schema_refs_unsorted_catalog_and_budget_excess() {
    // Pins: schemas cannot retrieve remote content and catalog/budget admission is deterministic.
    let mut request = valid_request();
    request.plan.input_schema = json!({ "$ref": "https://example.com/schema.json" });
    request.catalog.capabilities.push(capability("aaa.first"));
    request.catalog.catalog_hash =
        catalog_hash(1, &request.catalog.capabilities).expect("rehash unsorted catalog");

    let outcome = compile(request);
    assert!(outcome.compiled.is_none());
    let codes = outcome
        .report
        .issues
        .iter()
        .map(|issue| issue.code.as_str())
        .collect::<Vec<_>>();
    assert!(codes.contains(&"invalid_json_schema"));
    assert!(codes.contains(&"unsorted_collection"));

    let mut over_budget = valid_request();
    over_budget.approved_budget.max_tokens = Some(1);
    let outcome = compile(over_budget);
    assert!(outcome.compiled.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "approved_budget_exceeded")
    );
}

#[test]
fn compile_rejects_malformed_deliverable_schema() {
    // Pins: every goal deliverable schema is compiled as Draft 2020-12 before admission.
    let mut request = valid_request();
    request.goal.deliverables.push(ExecutionDeliverable {
        id: "result".to_string(),
        description: "Validated result".to_string(),
        output_pointer: String::new(),
        schema: json!({ "type": 7 }),
    });

    let outcome = compile(request);
    assert!(outcome.compiled.is_none());
    assert_eq!(
        outcome
            .report
            .issues
            .iter()
            .filter(|issue| {
                issue.code == "invalid_json_schema" && issue.path == "goal.deliverables[0].schema"
            })
            .count(),
        1
    );
}

#[test]
fn compile_requires_every_requirement_in_at_least_one_completion_check() {
    // Pins: compiler admission rejects an unchecked requirement while allowing different check
    // kinds to divide requirement coverage according to what each check actually verifies.
    let mut request = valid_request();
    request.goal.requirements.push(ExecutionRequirement {
        id: "req_two".to_string(),
        description: "Return the validated order result".to_string(),
    });
    for node in &mut request.plan.nodes {
        node.requirement_ids.push("req_two".to_string());
    }

    let rejected = compile(request.clone());

    assert!(rejected.compiled.is_none());
    assert_eq!(
        rejected.report.issues,
        vec![ExecutionValidationIssue {
            severity: ExecutionValidationSeverity::Error,
            code: "unchecked_requirement".to_string(),
            path: "goal.requirements[1].id".to_string(),
            message: "every requirement must be linked to at least one completion check"
                .to_string(),
        }]
    );

    request.goal.completion_checks.push(CompletionCheck {
        id: "required_nodes".to_string(),
        description: "Validate the node serving the second requirement".to_string(),
        requirement_ids: vec!["req_two".to_string()],
        constraint_ids: Vec::new(),
        kind: CompletionCheckKind::RequiredNodes {
            node_ids: vec!["output".to_string()],
        },
    });
    let accepted = compile(request);
    assert!(accepted.compiled.is_some(), "{:?}", accepted.report.issues);
    assert!(accepted.report.issues.is_empty());
}

#[test]
fn compile_rejects_every_reference_outside_catalog_or_authorization() {
    // Pins: catalog and authorization envelopes are independent compiler admission gates.
    let mut outside_catalog = valid_request();
    outside_catalog.catalog.capabilities.clear();
    outside_catalog.catalog.catalog_hash =
        catalog_hash(1, &[]).expect("hash empty capability catalog");
    let outcome = compile(outside_catalog);
    assert!(outcome.compiled.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "capability_not_in_catalog")
    );

    let mut unauthorized_capability = valid_request();
    unauthorized_capability
        .authorization
        .capability_refs
        .clear();
    let outcome = compile(unauthorized_capability);
    assert!(outcome.compiled.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "capability_not_authorized")
    );

    let mut unauthorized_skill = valid_request();
    let skill_ref =
        ArtifactRef::from_str("skill://restricted").expect("parse restricted skill reference");
    unauthorized_skill.plan.nodes[0].operation = ExecutionOperation::Agent {
        instructions: "Use the restricted skill".to_string(),
        skill_refs: vec![skill_ref],
        capability_refs: vec![],
        max_turns: 1,
    };
    let outcome = compile(unauthorized_skill);
    assert!(outcome.compiled.is_none());
    assert!(
        outcome
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "skill_not_authorized")
    );
}

#[test]
fn compile_accepts_exact_rollback_contract_and_reserves_undo_cost() {
    // Pins: an opted-in rollback uses the forward retry bound for resources while reserving one
    // additional logical compensation task.
    let request = compensated_request();

    let outcome = compile(request);

    assert!(
        outcome.report.issues.is_empty(),
        "{:?}",
        outcome.report.issues
    );
    assert_eq!(
        outcome
            .compiled
            .expect("exact compensated plan should compile")
            .plan
            .estimate,
        ExecutionEstimate {
            cost_microusd: 28,
            tokens: 44,
            tool_calls: 12,
            retrieved_bytes: 52,
            tasks: 3,
        }
    );
}

#[test]
fn compensate_committed_requires_direct_coverage_and_rejects_indirect_effects() {
    // Pins: the cancellation policy cannot promise rollback for effects the first implementation
    // cannot register and compensate deterministically.
    let mut uncovered = valid_request();
    uncovered.plan.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    uncovered.catalog.capabilities[0].action_class = ActionClass::ExternalWrite;
    uncovered.catalog.capabilities[0].execution_class = ExecutionClass::External;
    rehash_catalog(&mut uncovered);
    let direct = compile(uncovered.clone());
    assert_eq!(
        direct
            .report
            .issues
            .iter()
            .filter(|issue| issue.code == "uncompensated_cancel_effect")
            .count(),
        1
    );

    uncovered.plan.cancel_policy = ExecutionCancelPolicy::RetainEffects;
    let retained = compile(uncovered.clone());
    assert!(
        retained.compiled.is_some(),
        "retain_effects should admit an uncompensated side effect: {:?}",
        retained.report.issues
    );

    uncovered.plan.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    let reference = uncovered.catalog.capabilities[0].reference.clone();
    uncovered.plan.nodes[0].operation = ExecutionOperation::Agent {
        instructions: "perform the governed operation".to_string(),
        skill_refs: vec![],
        capability_refs: vec![reference],
        max_turns: 1,
    };
    let indirect = compile(uncovered);
    assert_eq!(
        indirect
            .report
            .issues
            .iter()
            .filter(|issue| issue.code == "indirect_compensation_unsupported")
            .count(),
        1
    );

    let covered = compile(compensated_request());
    assert!(
        covered.compiled.is_some(),
        "fully compensated direct effects should compile: {:?}",
        covered.report.issues
    );
}

#[test]
fn compile_rejects_unpromised_read_non_idempotent_and_unauthorized_compensators() {
    // Pins: durable rollback is admitted only for an exact side-effecting idempotent capability
    // inside the original catalog and authorization envelope.
    let mut mismatched = compensated_request();
    mismatched.plan.nodes[0]
        .compensation
        .as_mut()
        .expect("compensation fixture")
        .input_mapping
        .bindings[0]
        .source = CompensationValueSource::OriginalInput {
        pointer: "/order_id".to_string(),
    };
    assert_issue_code(compile(mismatched), "compensation_contract_mismatch");

    let mut unpromised = compensated_request();
    unpromised.catalog.capabilities[0].rollback = None;
    rehash_catalog(&mut unpromised);
    assert_issue_code(compile(unpromised), "compensation_not_promised");

    let mut forward_read = compensated_request();
    forward_read.catalog.capabilities[0].action_class = ActionClass::Read;
    rehash_catalog(&mut forward_read);
    assert_issue_code(compile(forward_read), "compensation_on_read");

    let mut read_compensator = compensated_request();
    read_compensator.catalog.capabilities[1].action_class = ActionClass::Read;
    rehash_catalog(&mut read_compensator);
    assert_issue_code(compile(read_compensator), "read_compensator");

    let mut non_idempotent = compensated_request();
    non_idempotent.catalog.capabilities[1].idempotency_class = IdempotencyClass::NonIdempotent;
    rehash_catalog(&mut non_idempotent);
    assert_issue_code(compile(non_idempotent), "non_idempotent_compensator");

    let mut unauthorized = compensated_request();
    unauthorized.authorization.capability_refs.pop();
    assert_issue_code(compile(unauthorized), "capability_not_authorized");
}

#[test]
fn compile_rejects_compensation_mapping_outside_governed_schemas() {
    // Pins: rollback mappings can read only committed forward fields and write declared undo input.
    let mut bad_source = compensated_request();
    bad_source.plan.nodes[0]
        .compensation
        .as_mut()
        .expect("compensation fixture")
        .input_mapping
        .bindings[0]
        .source = CompensationValueSource::OriginalOutput {
        pointer: "/missing".to_string(),
    };
    bad_source.catalog.capabilities[0].rollback = bad_source.plan.nodes[0]
        .compensation
        .clone()
        .map(|compensation| CapabilityRollbackContract {
            compensator: compensation.compensator,
            input_mapping: compensation.input_mapping,
        });
    rehash_catalog(&mut bad_source);
    assert_issue_code(compile(bad_source), "unguaranteed_compensation_source_path");

    let mut bad_target = compensated_request();
    bad_target.plan.nodes[0]
        .compensation
        .as_mut()
        .expect("compensation fixture")
        .input_mapping
        .bindings[0]
        .target_pointer = "/missing".to_string();
    bad_target.catalog.capabilities[0].rollback = bad_target.plan.nodes[0]
        .compensation
        .clone()
        .map(|compensation| CapabilityRollbackContract {
            compensator: compensation.compensator,
            input_mapping: compensation.input_mapping,
        });
    rehash_catalog(&mut bad_target);
    assert_issue_code(compile(bad_target), "unknown_compensation_target_path");

    let mut optional_source = compensated_request();
    optional_source.catalog.capabilities[0].output_schema["required"] = json!([]);
    rehash_catalog(&mut optional_source);
    assert_issue_code(
        compile(optional_source),
        "unguaranteed_compensation_source_path",
    );

    let mut incompatible = compensated_request();
    incompatible.catalog.capabilities[1].input_schema["properties"]["order_id"]["type"] =
        json!("number");
    rehash_catalog(&mut incompatible);
    assert_issue_code(compile(incompatible), "incompatible_compensation_mapping");

    for (source_pointer, target_pointer, expected_code) in [
        (
            "/order~2id",
            "/order_id",
            "invalid_compensation_source_pointer",
        ),
        (
            "/order_id",
            "/order~2id",
            "invalid_compensation_target_pointer",
        ),
    ] {
        let mut malformed = compensated_request();
        let compensation = malformed.plan.nodes[0]
            .compensation
            .as_mut()
            .expect("compensation fixture");
        compensation.input_mapping.bindings[0].source = CompensationValueSource::OriginalOutput {
            pointer: source_pointer.to_string(),
        };
        compensation.input_mapping.bindings[0].target_pointer = target_pointer.to_string();
        malformed.catalog.capabilities[0].rollback = Some(CapabilityRollbackContract {
            compensator: compensation.compensator.clone(),
            input_mapping: compensation.input_mapping.clone(),
        });
        rehash_catalog(&mut malformed);
        assert_issue_code(compile(malformed), expected_code);
    }
}

#[test]
fn compile_rejects_decoded_parent_child_compensation_targets() {
    // Pins: overlapping rollback writes are rejected before a forward effect can commit, including
    // paths whose relationship is visible only after RFC 6901 decoding.
    let mut request = compensated_request();
    let forward_output = request.catalog.capabilities[0].output_schema.clone();
    request.catalog.capabilities[1].input_schema = json!({
        "type": "object",
        "required": ["resource"],
        "properties": { "resource": forward_output }
    });
    let compensation = request.plan.nodes[0]
        .compensation
        .as_mut()
        .expect("compensation fixture");
    compensation.input_mapping.bindings = vec![
        CompensationInputBinding {
            target_pointer: "/resource".to_string(),
            source: CompensationValueSource::OriginalOutput {
                pointer: String::new(),
            },
        },
        CompensationInputBinding {
            target_pointer: "/resource/order_id".to_string(),
            source: CompensationValueSource::OriginalOutput {
                pointer: "/order_id".to_string(),
            },
        },
    ];
    request.catalog.capabilities[0].rollback = Some(CapabilityRollbackContract {
        compensator: compensation.compensator.clone(),
        input_mapping: compensation.input_mapping.clone(),
    });
    rehash_catalog(&mut request);

    assert_issue_code(compile(request), "compensation_target_collision");
}

#[test]
fn compile_requires_complete_nested_compensator_input_coverage() {
    // Pins: a forward effect cannot commit unless its mapping constructs every required undo
    // input; an exact binding at a required parent safely covers that whole subtree.
    let mut incomplete = compensated_request();
    incomplete.catalog.capabilities[1].input_schema = json!({
        "type": "object",
        "required": ["resource"],
        "properties": {
            "resource": {
                "type": "object",
                "required": ["id"],
                "properties": {
                    "id": { "type": "string" },
                    "note": { "type": "string" }
                }
            }
        }
    });
    let compensation = incomplete.plan.nodes[0]
        .compensation
        .as_mut()
        .expect("compensation fixture");
    compensation.input_mapping.bindings[0].target_pointer = "/resource/note".to_string();
    incomplete.catalog.capabilities[0].rollback = Some(CapabilityRollbackContract {
        compensator: compensation.compensator.clone(),
        input_mapping: compensation.input_mapping.clone(),
    });
    rehash_catalog(&mut incomplete);
    assert_issue_code(compile(incomplete), "unmapped_required_compensation_target");

    let mut parent_bound = compensated_request();
    let forward_output = parent_bound.catalog.capabilities[0].output_schema.clone();
    parent_bound.catalog.capabilities[1].input_schema = json!({
        "type": "object",
        "required": ["resource"],
        "properties": { "resource": forward_output }
    });
    let compensation = parent_bound.plan.nodes[0]
        .compensation
        .as_mut()
        .expect("compensation fixture");
    compensation.input_mapping.bindings[0] = CompensationInputBinding {
        target_pointer: "/resource".to_string(),
        source: CompensationValueSource::OriginalOutput {
            pointer: String::new(),
        },
    };
    parent_bound.catalog.capabilities[0].rollback = Some(CapabilityRollbackContract {
        compensator: compensation.compensator.clone(),
        input_mapping: compensation.input_mapping.clone(),
    });
    rehash_catalog(&mut parent_bound);
    let accepted = compile(parent_bound);
    assert!(
        accepted.compiled.is_some(),
        "exact parent binding should cover its required subtree: {:?}",
        accepted.report.issues
    );
}

#[test]
fn installed_connector_catalog_hash_pins_exact_dispatch_identity_across_serde_replay() {
    // Pins: durable installed-connector catalogs preserve typed provenance and
    // change identity when a connection generation changes, without parsing the tool name.
    let capability = installed_connector_capability(7, installed_connector_tool_name());
    let first = ExecutionCapabilityCatalog::build(vec![capability.clone()])
        .expect("complete installed connector pins should build a catalog");
    let repeated = ExecutionCapabilityCatalog::build(vec![capability])
        .expect("the same installed connector pins should rebuild deterministically");
    assert_eq!(repeated.catalog_hash, first.catalog_hash);

    let encoded = serde_json::to_vec(&first).expect("serialize installed connector catalog");
    let replayed: ExecutionCapabilityCatalog =
        serde_json::from_slice(&encoded).expect("deserialize installed connector catalog");
    assert_eq!(replayed, first);
    assert_eq!(
        replayed.capabilities[0].source,
        installed_connector_source(7, installed_connector_tool_name())
    );
    assert_eq!(
        replayed.capabilities[0].source.model_visible_tool_name(),
        Some(installed_connector_tool_name())
    );

    let advanced = ExecutionCapabilityCatalog::build(vec![installed_connector_capability(
        8,
        installed_connector_tool_name(),
    )])
    .expect("next-generation installed connector pins should build a catalog");
    assert_ne!(advanced.catalog_hash, first.catalog_hash);

    let mut request = valid_request();
    let reference = first.capabilities[0].reference.clone();
    install_catalog(&mut request, first.clone());
    request.plan.nodes[0].operation = ExecutionOperation::Capability { reference };
    let outcome = compile(request);
    assert!(
        outcome.report.issues.is_empty(),
        "complete installed connector capability should compile: {:?}",
        outcome.report.issues
    );
    assert_eq!(
        outcome
            .compiled
            .expect("complete installed connector capability should compile")
            .plan
            .catalog_hash,
        first.catalog_hash
    );
}

#[test]
fn compile_rejects_installed_connector_policy_context_drift_after_deserialization() {
    // Pins: a correctly rehashed wire catalog cannot weaken the installed
    // connector review floor independently of its durable dispatch source.
    let mut capability = installed_connector_capability(7, installed_connector_tool_name());
    capability.policy_context.minimum_effect = ActionPolicyEffect::Allow;
    let catalog = ExecutionCapabilityCatalog {
        schema_version: 1,
        catalog_hash: catalog_hash(1, std::slice::from_ref(&capability))
            .expect("hash intentionally drifted wire catalog"),
        capabilities: vec![capability],
    };
    let encoded = serde_json::to_vec(&catalog).expect("serialize drifted wire catalog");
    let catalog: ExecutionCapabilityCatalog =
        serde_json::from_slice(&encoded).expect("deserialize drifted wire catalog");
    let mut request = valid_request();
    let reference = catalog.capabilities[0].reference.clone();
    install_catalog(&mut request, catalog);
    request.plan.nodes[0].operation = ExecutionOperation::Capability { reference };

    let outcome = compile(request);
    assert!(outcome.compiled.is_none());
    assert_eq!(outcome.report.issues.len(), 1);
    assert_eq!(
        outcome.report.issues[0].severity,
        ExecutionValidationSeverity::Error
    );
    assert_eq!(
        outcome.report.issues[0].code,
        "invalid_capability_policy_context"
    );
    assert_eq!(
        outcome.report.issues[0].path,
        "catalog.capabilities[0].policy_context"
    );
    assert_eq!(
        outcome.report.issues[0].message,
        "invalid execution projection: capability action://billing.refund has invalid policy context: installed connector minimum effect must match its source"
    );
}

#[test]
fn installed_connector_catalog_rejects_a_zero_generation_pin() {
    // Pins: a missing connection generation cannot become a durable installed
    // connector capability, even when every other binding field is present.
    let capability = installed_connector_capability(0, installed_connector_tool_name());

    let error = ExecutionCapabilityCatalog::build(vec![capability])
        .expect_err("zero-generation installed connector source must fail closed");
    assert!(
        matches!(error, moa_execution::Error::InvalidProjection { ref message }
            if message == "capability action://billing.refund has invalid policy context: installed connector dispatch pins must be complete and non-empty"),
        "unexpected zero-generation error: {error:?}"
    );
}

#[test]
fn compile_rejects_ambiguous_agent_capability_tool_for_task_agents_in_both_orders() {
    // Pins: task-local agents cannot choose between raw and governed aliases of one tool by list order.
    for alias_kind in [
        GovernedAliasKind::Action,
        GovernedAliasKind::InstalledConnector,
        GovernedAliasKind::SkillAction,
    ] {
        let (catalog, raw_reference, alias_reference) = aliased_tool_catalog(alias_kind);
        for capability_refs in both_reference_orders(&raw_reference, &alias_reference) {
            let mut request = valid_request();
            install_catalog(&mut request, catalog.clone());
            request.plan.nodes[0].operation = ExecutionOperation::Agent {
                instructions: "Refund the order safely".to_string(),
                skill_refs: Vec::new(),
                capability_refs,
                max_turns: 1,
            };

            assert_ambiguous_agent_tool_rejected(
                compile(request),
                "plan.nodes[0].operation.capability_refs",
                alias_kind,
            );
        }
    }
}

#[test]
fn compile_rejects_ambiguous_agent_capability_tool_for_map_agents_in_both_orders() {
    // Pins: map agents cannot silently select one policy identity for duplicate visible tools.
    for alias_kind in [
        GovernedAliasKind::Action,
        GovernedAliasKind::InstalledConnector,
        GovernedAliasKind::SkillAction,
    ] {
        let (catalog, raw_reference, alias_reference) = aliased_tool_catalog(alias_kind);
        for capability_refs in both_reference_orders(&raw_reference, &alias_reference) {
            let mut request = valid_request();
            install_catalog(&mut request, catalog.clone());
            request.plan.nodes[0].operation = ExecutionOperation::Map {
                items: json!([{ "id": "ord-1" }]),
                item_key: "/id".to_string(),
                max_items: 1,
                item_output_schema: json!({ "type": "object" }),
                task: MapTask::Agent {
                    instructions: "Refund each order safely".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs,
                    max_turns: 1,
                },
            };

            assert_ambiguous_agent_tool_rejected(
                compile(request),
                "plan.nodes[0].operation.task.capability_refs",
                alias_kind,
            );
        }
    }
}

#[test]
fn compile_rejects_ambiguous_agent_capability_tool_for_reduce_agents_in_both_orders() {
    // Pins: reduce agents cannot weaken review by resolving duplicate tool names to the first ref.
    for alias_kind in [
        GovernedAliasKind::Action,
        GovernedAliasKind::InstalledConnector,
        GovernedAliasKind::SkillAction,
    ] {
        let (catalog, raw_reference, alias_reference) = aliased_tool_catalog(alias_kind);
        for capability_refs in both_reference_orders(&raw_reference, &alias_reference) {
            let mut request = valid_request();
            install_catalog(&mut request, catalog.clone());
            request.plan.nodes[0].operation = ExecutionOperation::Reduce {
                items: json!([{ "id": "ord-1" }, { "id": "ord-2" }]),
                max_items: 2,
                reducer: ExecutionReducer::Agent {
                    instructions: "Summarize the refund results safely".to_string(),
                    skill_refs: Vec::new(),
                    capability_refs,
                    max_turns: 1,
                },
                batch_size: 2,
            };

            assert_ambiguous_agent_tool_rejected(
                compile(request),
                "plan.nodes[0].operation.reducer.capability_refs",
                alias_kind,
            );
        }
    }
}

#[test]
fn compile_rejects_unknown_input_reference_path_before_persistence() {
    // Pins: a syntactically valid run-input tail must be declared by the plan input schema.
    let mut request = valid_request();
    request.plan.nodes[0].input = json!({
        "order_id": { "$ref": "$.input.missing" }
    });

    let outcome = compile(request);

    assert!(outcome.compiled.is_none());
    assert_eq!(
        outcome.report.issues,
        vec![unknown_reference_issue("plan.nodes[0].input.order_id")]
    );
}

#[test]
fn compile_rejects_unknown_dependency_output_reference_path_before_persistence() {
    // Pins: a syntactically valid dependency-output tail must be declared by that node's output schema.
    let mut request = valid_request();
    request.plan.nodes[1].operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup.output.missing" }),
    };

    let outcome = compile(request);

    assert!(outcome.compiled.is_none());
    assert_eq!(
        outcome.report.issues,
        vec![unknown_reference_issue("plan.nodes[1].operation.value")]
    );
}

#[test]
fn compile_checks_reference_paths_in_conditions_map_and_reduce_items() {
    // Pins: every condition and operation-level collection binding receives the same schema-aware path check.
    let mut condition = valid_request();
    condition.plan.nodes[0].when = Some(ExecutionCondition::Exists {
        reference: ExecutionReference {
            path: "$.input.missing".to_string(),
        },
    });

    let mut map = valid_request();
    map.plan.nodes[0].operation = ExecutionOperation::Map {
        items: json!({ "$ref": "$.input.missing" }),
        item_key: String::new(),
        max_items: 2,
        item_output_schema: json!({ "type": "object" }),
        task: MapTask::Capability {
            reference: capability("orders.lookup").reference,
        },
    };

    let mut reduce = valid_request();
    reduce.plan.nodes[0].operation = ExecutionOperation::Reduce {
        items: json!({ "$ref": "$.input.missing" }),
        max_items: 2,
        reducer: ExecutionReducer::Capability {
            reference: capability("orders.lookup").reference,
        },
        batch_size: 2,
    };

    let outcomes = [
        (
            "condition",
            compile(condition),
            "plan.nodes[0].when.reference.$ref",
        ),
        ("map items", compile(map), "plan.nodes[0].operation.items"),
        (
            "reduce items",
            compile(reduce),
            "plan.nodes[0].operation.items",
        ),
    ];
    for (location, outcome, path) in outcomes {
        assert!(
            outcome.compiled.is_none(),
            "accepted unknown {location} path"
        );
        assert_eq!(
            outcome.report.issues,
            vec![unknown_reference_issue(path)],
            "unexpected issue for {location}"
        );
    }
}

#[test]
fn compile_accepts_declared_nested_reference_paths() {
    // Pins: local schema references and allOf composition preserve declared nested input and dependency-output paths.
    let mut request = valid_request();
    request.plan.input_schema = json!({
        "$defs": {
            "RunInput": {
                "type": "object",
                "required": ["request"],
                "properties": {
                    "request": { "$ref": "#/$defs/Request" }
                }
            },
            "Request": {
                "type": "object",
                "required": ["order"],
                "properties": {
                    "order": {
                        "type": "object",
                        "required": ["id"],
                        "properties": { "id": { "type": "string" } }
                    }
                }
            }
        },
        "$ref": "#/$defs/RunInput"
    });
    request.run_input = json!({ "request": { "order": { "id": "ord-1" } } });
    request.plan.nodes[0].input = json!({
        "order_id": { "$ref": "$.input.request.order.id" },
        "run_input": { "$ref": "$.input" }
    });
    request.plan.nodes[0].output_schema = json!({
        "$defs": {
            "LookupOutput": {
                "type": "object",
                "properties": {
                    "result": {
                        "type": "object",
                        "properties": {
                            "order": {
                                "type": "object",
                                "properties": { "id": { "type": "string" } }
                            }
                        }
                    }
                }
            }
        },
        "allOf": [{ "$ref": "#/$defs/LookupOutput" }]
    });
    request.plan.nodes[1].when = Some(ExecutionCondition::Exists {
        reference: ExecutionReference {
            path: "$.nodes.lookup.output.result.order.id".to_string(),
        },
    });
    request.plan.nodes[1].output_schema = json!({ "type": "string" });
    request.plan.nodes[1].operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup.output.result.order.id" }),
    };
    request.plan.output_schema = json!({ "type": "string" });

    let outcome = compile(request);

    assert!(outcome.compiled.is_some(), "{:?}", outcome.report.issues);
    assert!(outcome.report.issues.is_empty());
}

#[test]
fn compile_schema_checks_coverage_expected_items_reference_paths() {
    // Pins: completion coverage bindings are rejected before persistence unless their tails are declared by a source schema.
    let mut request = valid_request();
    request.goal.coverage.push(CoverageRequirement {
        id: "all_orders".to_string(),
        description: "Cover every requested order".to_string(),
        map_node_id: "lookup".to_string(),
        expected_items: json!({ "$ref": "$.input.orders" }),
        require_all: true,
    });
    request.plan.nodes[0].operation = ExecutionOperation::Map {
        items: json!([{ "id": "ord-1" }]),
        item_key: "/id".to_string(),
        max_items: 1,
        item_output_schema: json!({ "type": "object" }),
        task: MapTask::Capability {
            reference: capability("orders.lookup").reference,
        },
    };

    let rejected = compile(request.clone());
    assert!(rejected.compiled.is_none());
    assert_eq!(
        rejected.report.issues,
        vec![unknown_reference_issue("goal.coverage[0].expected_items")]
    );

    request.plan.input_schema = json!({
        "type": "object",
        "required": ["order_id", "orders"],
        "properties": {
            "order_id": { "type": "string" },
            "orders": {
                "type": "array",
                "items": { "type": "object" }
            }
        }
    });
    request.run_input = json!({
        "order_id": "ord-1",
        "orders": [{ "id": "ord-1" }]
    });

    let accepted = compile(request);
    assert!(accepted.compiled.is_some(), "{:?}", accepted.report.issues);
    assert!(accepted.report.issues.is_empty());
}

#[test]
fn amendment_rejects_unknown_reference_path_before_persistence() {
    // Pins: amended nodes are checked against durable plan schemas even though amendment validation has no run input.
    let request = valid_request();
    let compiled = compile(request.clone())
        .compiled
        .expect("compile active plan");
    let mut replacement = compiled.plan.definition.nodes[1].clone();
    replacement.id = "replacement_output".to_string();
    replacement.operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup.output.missing" }),
    };

    let outcome = validate_amendment(ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 4,
            reason: "Replace the pending terminal projection".to_string(),
            evidence: json!({}),
            operations: vec![PlanAmendmentOperation::ReplacePendingNode {
                node_id: "output".to_string(),
                node: replacement,
            }],
        },
        projection: ExecutionProjection {
            plan_revision: 4,
            node_statuses: BTreeMap::new(),
            tasks: vec![],
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    });

    assert!(outcome.plan.is_none());
    assert_eq!(
        outcome.report.issues,
        vec![unknown_reference_issue("plan.nodes[1].operation.value")]
    );
}

#[test]
fn amendment_replaces_only_pending_work_with_a_distinct_identity() {
    // Pins: genuinely pending work remains replaceable under a distinct new node identity.
    let request = valid_request();
    let compiled = compile(request.clone())
        .compiled
        .expect("compile active plan");
    let mut statuses = BTreeMap::new();
    statuses.insert("lookup".to_string(), ExecutionNodeStatus::Completed);
    statuses.insert("output".to_string(), ExecutionNodeStatus::Pending);
    let projection = ExecutionProjection {
        plan_revision: 4,
        node_statuses: statuses,
        tasks: vec![],
    };
    let replacement = ExecutionNode {
        id: "replacement_output".to_string(),
        requirement_ids: vec!["req_one".to_string()],
        depends_on: vec!["lookup".to_string()],
        when: None,
        input: json!({}),
        output_schema: json!({ "type": "object" }),
        operation: ExecutionOperation::Output {
            value: json!({ "$ref": "$.nodes.lookup.output" }),
        },
        compensation: None,
        retry: retry(1),
        budget: None,
    };
    let outcome = validate_amendment(ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 4,
            reason: "Replace pending terminal projection".to_string(),
            evidence: json!({ "reason": "new shape" }),
            operations: vec![PlanAmendmentOperation::ReplacePendingNode {
                node_id: "output".to_string(),
                node: replacement,
            }],
        },
        projection,
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    });

    assert!(
        outcome.report.issues.is_empty(),
        "{:?}",
        outcome.report.issues
    );
    let plan = outcome.plan.expect("valid amendment");
    assert!(
        plan.definition
            .nodes
            .iter()
            .any(|node| node.id == "replacement_output")
    );
    assert!(!plan.definition.nodes.iter().any(|node| node.id == "output"));
}

#[test]
fn amendment_cannot_remove_compensation_after_forward_work_starts() {
    // Pins: once a forward effect can have committed, amendment cannot discard its undo contract.
    let request = compensated_request();
    let compiled = compile(request.clone())
        .compiled
        .expect("compile compensated amendment fixture");
    let mut replacement = compiled.plan.definition.nodes[0].clone();
    replacement.id = "replacement_create".to_string();
    replacement.compensation = None;

    let outcome = validate_amendment(ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 3,
            reason: "Attempt to discard rollback after dispatch".to_string(),
            evidence: json!({}),
            operations: vec![PlanAmendmentOperation::ReplacePendingNode {
                node_id: "lookup".to_string(),
                node: replacement,
            }],
        },
        projection: ExecutionProjection {
            plan_revision: 3,
            node_statuses: BTreeMap::from([("lookup".to_string(), ExecutionNodeStatus::Running)]),
            tasks: vec![task_projection("lookup", ExecutionTaskStatus::Running)],
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    });

    assert!(outcome.plan.is_none());
    assert_eq!(
        outcome
            .report
            .issues
            .iter()
            .filter(|issue| issue.code == "compensation_locked")
            .count(),
        1
    );
}

#[test]
fn amendment_validation_retains_remaining_estimate_without_completed_work() {
    // Pins: replan budget policy consumes the compiler's one remaining-work estimate instead of
    // charging the completed capability node again through CanonicalExecutionPlan::estimate.
    let request = valid_request();
    let compiled = compile(request.clone())
        .compiled
        .expect("compile active plan");
    let mut replacement = compiled.plan.definition.nodes[1].clone();
    replacement.id = "replacement_output".to_string();
    let outcome = validate_amendment(ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 4,
            reason: "Replace only unfinished output work".to_string(),
            evidence: json!({}),
            operations: vec![PlanAmendmentOperation::ReplacePendingNode {
                node_id: "output".to_string(),
                node: replacement,
            }],
        },
        projection: ExecutionProjection {
            plan_revision: 4,
            node_statuses: BTreeMap::from([
                ("lookup".to_string(), ExecutionNodeStatus::Completed),
                ("output".to_string(), ExecutionNodeStatus::Pending),
            ]),
            tasks: vec![task_projection("lookup", ExecutionTaskStatus::Completed)],
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: ExecutionBudgetLimit {
            max_tasks: Some(1),
            ..generous_budget()
        },
        config: ExecutionConfig::default(),
        now: now(),
    });

    assert!(
        outcome.report.issues.is_empty(),
        "{:?}",
        outcome.report.issues
    );
    assert_eq!(
        outcome
            .remaining_estimate
            .expect("valid amendment should retain its remaining estimate")
            .tasks,
        1
    );
    assert_eq!(
        outcome.plan.expect("valid amendment").estimate.tasks,
        2,
        "canonical plan must continue to retain its full immutable estimate"
    );
}

#[test]
fn amendment_rejects_replace_and_remove_when_running_task_has_no_node_status() {
    // Pins: a missing node-status entry cannot hide a Running task from amendment immutability.
    for (operation, replace, message) in [
        ("replace", true, "only a pending node may be replaced"),
        (
            "remove",
            false,
            "only a pending node or the originating WaitingReplan node may be removed",
        ),
    ] {
        let outcome = validate_amendment(amendment_validation_for_output(
            None,
            ExecutionTaskStatus::Running,
            replace,
        ));

        assert!(
            outcome.plan.is_none(),
            "accepted {operation} of running work"
        );
        assert_eq!(
            outcome.report.issues,
            vec![immutable_node_issue(message)],
            "unexpected issue for {operation} of running work"
        );
    }
}

#[test]
fn amendment_rejects_stale_pending_node_with_reserved_or_completed_task() {
    // Pins: a stale Pending node status cannot downgrade Reserved or Completed task evidence.
    for task_status in [
        ExecutionTaskStatus::Reserved,
        ExecutionTaskStatus::Completed,
    ] {
        for (operation, replace, message) in [
            ("replace", true, "only a pending node may be replaced"),
            (
                "remove",
                false,
                "only a pending node or the originating WaitingReplan node may be removed",
            ),
        ] {
            let outcome = validate_amendment(amendment_validation_for_output(
                Some(ExecutionNodeStatus::Pending),
                task_status,
                replace,
            ));

            assert!(
                outcome.plan.is_none(),
                "accepted {operation} with {task_status:?} task evidence"
            );
            assert_eq!(
                outcome.report.issues,
                vec![immutable_node_issue(message)],
                "unexpected issue for {operation} with {task_status:?} task evidence"
            );
        }
    }
}

#[test]
fn amendment_rejects_references_unused_by_the_active_plan() {
    // Pins: an amendment cannot broaden the active plan's capability or skill reference sets.
    let mut request = valid_request();
    let added_capability = capability("orders.lookup_v2");
    request
        .authorization
        .capability_refs
        .push(added_capability.reference.clone());
    request.catalog.capabilities.push(added_capability.clone());
    request.catalog.catalog_hash = catalog_hash(1, &request.catalog.capabilities)
        .expect("hash catalog with unused capability");
    let added_skill =
        ArtifactRef::from_str("skill://unused-skill").expect("parse unused skill reference");
    request.authorization.skill_refs.push(added_skill.clone());

    let compiled = compile(request.clone())
        .compiled
        .expect("compile active plan with broader authorization snapshot");
    let mut replacement = compiled.plan.definition.nodes[0].clone();
    replacement.id = "lookup_v2".to_string();
    replacement.operation = ExecutionOperation::Capability {
        reference: added_capability.reference,
    };
    let mut output = compiled.plan.definition.nodes[1].clone();
    output.id = "output_v2".to_string();
    output.depends_on = vec!["lookup_v2".to_string()];
    output.operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup_v2.output" }),
    };
    let validation = ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 3,
            reason: "Try a newly authorized capability".to_string(),
            evidence: json!({}),
            operations: vec![
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "lookup".to_string(),
                    node: replacement,
                },
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "output".to_string(),
                    node: output,
                },
            ],
        },
        projection: ExecutionProjection {
            plan_revision: 3,
            node_statuses: BTreeMap::new(),
            tasks: vec![],
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    };

    let capability_result = validate_amendment(validation.clone());
    assert!(capability_result.plan.is_none());
    assert_eq!(
        capability_result
            .report
            .issues
            .iter()
            .filter(|issue| issue.code == "authorization_broadened")
            .count(),
        1
    );

    let mut skill_validation = validation;
    let PlanAmendmentOperation::ReplacePendingNode { node, .. } =
        &mut skill_validation.amendment.operations[0]
    else {
        panic!("capability replacement operation");
    };
    node.id = "lookup_agent".to_string();
    node.operation = ExecutionOperation::Agent {
        instructions: "Use the newly available skill".to_string(),
        skill_refs: vec![added_skill],
        capability_refs: vec![],
        max_turns: 1,
    };
    let PlanAmendmentOperation::ReplacePendingNode { node, .. } =
        &mut skill_validation.amendment.operations[1]
    else {
        panic!("output replacement operation");
    };
    node.depends_on = vec!["lookup_agent".to_string()];
    node.operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup_agent.output" }),
    };

    let skill_result = validate_amendment(skill_validation);
    assert!(skill_result.plan.is_none());
    assert_eq!(
        skill_result
            .report
            .issues
            .iter()
            .filter(|issue| issue.code == "authorization_broadened")
            .count(),
        1
    );
}

#[test]
fn amendment_rejects_removed_or_increased_pending_node_budget() {
    // Pins: replacement work may preserve or narrow, but never remove or raise, a node budget.
    let active_budget = ExecutionBudgetLimit {
        max_cost_microusd: Some(100),
        max_tokens: Some(100),
        max_tasks: Some(2),
        max_tool_calls: Some(100),
        max_retrieved_bytes: Some(100),
        deadline_at: Some(
            Utc.with_ymd_and_hms(2029, 1, 1, 0, 0, 0)
                .single()
                .expect("active budget deadline"),
        ),
    };
    let mut request = valid_request();
    request.plan.nodes[0].budget = Some(active_budget.clone());
    let compiled = compile(request.clone())
        .compiled
        .expect("compile budgeted active plan");

    let mut replacement = compiled.plan.definition.nodes[0].clone();
    replacement.id = "lookup_budgeted".to_string();
    replacement.budget = Some(ExecutionBudgetLimit {
        max_cost_microusd: Some(14),
        max_tokens: Some(22),
        max_tasks: Some(1),
        max_tool_calls: Some(6),
        max_retrieved_bytes: Some(26),
        deadline_at: Some(
            Utc.with_ymd_and_hms(2028, 1, 1, 0, 0, 0)
                .single()
                .expect("narrowed budget deadline"),
        ),
    });
    let mut output = compiled.plan.definition.nodes[1].clone();
    output.id = "output_budgeted".to_string();
    output.depends_on = vec!["lookup_budgeted".to_string()];
    output.operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup_budgeted.output" }),
    };
    let validation = ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 5,
            reason: "Replace pending work within its budget".to_string(),
            evidence: json!({}),
            operations: vec![
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "lookup".to_string(),
                    node: replacement,
                },
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "output".to_string(),
                    node: output,
                },
            ],
        },
        projection: ExecutionProjection {
            plan_revision: 5,
            node_statuses: BTreeMap::new(),
            tasks: vec![],
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    };

    let narrowed = validate_amendment(validation.clone());
    assert!(narrowed.plan.is_some(), "{:?}", narrowed.report.issues);

    let later_deadline = Utc
        .with_ymd_and_hms(2030, 1, 1, 0, 0, 0)
        .single()
        .expect("broadened budget deadline");
    let broader_budgets = vec![
        ("removed", None),
        (
            "cost",
            Some(ExecutionBudgetLimit {
                max_cost_microusd: Some(101),
                ..active_budget.clone()
            }),
        ),
        (
            "tokens",
            Some(ExecutionBudgetLimit {
                max_tokens: Some(101),
                ..active_budget.clone()
            }),
        ),
        (
            "tasks",
            Some(ExecutionBudgetLimit {
                max_tasks: Some(3),
                ..active_budget.clone()
            }),
        ),
        (
            "tool_calls",
            Some(ExecutionBudgetLimit {
                max_tool_calls: Some(101),
                ..active_budget.clone()
            }),
        ),
        (
            "retrieved_bytes",
            Some(ExecutionBudgetLimit {
                max_retrieved_bytes: Some(101),
                ..active_budget.clone()
            }),
        ),
        (
            "deadline",
            Some(ExecutionBudgetLimit {
                deadline_at: Some(later_deadline),
                ..active_budget
            }),
        ),
    ];
    for (dimension, budget) in broader_budgets {
        let mut broadened = validation.clone();
        let PlanAmendmentOperation::ReplacePendingNode { node, .. } =
            &mut broadened.amendment.operations[0]
        else {
            panic!("budgeted replacement operation");
        };
        node.budget = budget;

        let outcome = validate_amendment(broadened);
        assert!(
            outcome.plan.is_none(),
            "accepted broader {dimension} budget"
        );
        assert_eq!(
            outcome
                .report
                .issues
                .iter()
                .filter(|issue| issue.code == "node_budget_broadened")
                .count(),
            1,
            "missing broader {dimension} budget issue"
        );
    }
}

#[test]
fn waiting_replan_amendment_removes_origin_and_replaces_every_pending_dependent() {
    // Pins: a WaitingReplan origin is removed, replacement work is distinct, and direct pending dependents are replaced.
    let mut request = valid_request();
    let mut seed = request.plan.nodes[0].clone();
    seed.id = "seed".to_string();
    request.plan.nodes[0].depends_on = vec!["seed".to_string()];
    request.plan.nodes.insert(0, seed);
    let compiled = compile(request.clone())
        .compiled
        .expect("compile replan fixture");
    let projection = ExecutionProjection {
        plan_revision: 7,
        node_statuses: BTreeMap::from([
            ("seed".to_string(), ExecutionNodeStatus::Completed),
            ("lookup".to_string(), ExecutionNodeStatus::Waiting),
            ("output".to_string(), ExecutionNodeStatus::Pending),
        ]),
        tasks: vec![waiting_replan_task("lookup")],
    };
    let lookup = compiled
        .plan
        .definition
        .nodes
        .iter()
        .find(|node| node.id == "lookup")
        .expect("lookup node")
        .clone();
    let output = compiled
        .plan
        .definition
        .nodes
        .iter()
        .find(|node| node.id == "output")
        .expect("output node")
        .clone();
    let mut replacement = lookup;
    replacement.id = "lookup_v2".to_string();
    let mut dependent = output;
    dependent.id = "output_v2".to_string();
    dependent.depends_on = vec!["lookup_v2".to_string()];
    dependent.operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup_v2.output" }),
    };
    let validation = ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 7,
            reason: "Replace unsupported lookup".to_string(),
            evidence: json!({ "failure": "unsupported" }),
            operations: vec![
                PlanAmendmentOperation::RemovePendingNode {
                    node_id: "lookup".to_string(),
                },
                PlanAmendmentOperation::AddNode {
                    node: replacement.clone(),
                },
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "output".to_string(),
                    node: dependent,
                },
            ],
        },
        projection,
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    };

    let accepted = validate_amendment(validation.clone());
    assert!(
        accepted.report.issues.is_empty(),
        "{:?}",
        accepted.report.issues
    );
    let definition = accepted.plan.expect("valid replan").definition;
    assert!(definition.nodes.iter().any(|node| node.id == "lookup_v2"));
    assert!(definition.nodes.iter().any(|node| node.id == "output_v2"));
    assert!(!definition.nodes.iter().any(|node| node.id == "lookup"));
    assert!(!definition.nodes.iter().any(|node| node.id == "output"));

    let mut removed_dependent = validation.clone();
    removed_dependent.amendment.operations[2] = PlanAmendmentOperation::RemovePendingNode {
        node_id: "output".to_string(),
    };
    let rejected = validate_amendment(removed_dependent);
    assert!(rejected.plan.is_none());
    assert!(
        rejected
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "stale_replan_dependent")
    );

    let mut reused_origin = validation;
    reused_origin.amendment.operations[1] = PlanAmendmentOperation::AddNode {
        node: ExecutionNode {
            id: "lookup".to_string(),
            compensation: None,
            ..replacement
        },
    };
    let rejected = validate_amendment(reused_origin);
    assert!(rejected.plan.is_none());
    assert!(
        rejected
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "reused_task_identity")
    );
}

#[test]
fn map_replacement_accepts_literal_subset_and_rejects_scope_broadening() {
    // Pins: a pending map may narrow its literal items and max_items but cannot broaden either.
    let mut request = valid_request();
    request.plan.nodes[0].input = json!({ "item": { "$item": true } });
    request.plan.nodes[0].operation = ExecutionOperation::Map {
        items: json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }]),
        item_key: "/id".to_string(),
        max_items: 3,
        item_output_schema: json!({ "type": "object" }),
        task: MapTask::Capability {
            reference: capability("orders.lookup").reference,
        },
    };
    let compiled = compile(request.clone())
        .compiled
        .expect("compile map fixture");
    let mut replacement = compiled.plan.definition.nodes[0].clone();
    replacement.id = "lookup_narrow".to_string();
    if let ExecutionOperation::Map {
        items, max_items, ..
    } = &mut replacement.operation
    {
        *items = json!([{ "id": 1 }, { "id": 3 }]);
        *max_items = 2;
    }
    let mut output = compiled.plan.definition.nodes[1].clone();
    output.id = "output_narrow".to_string();
    output.depends_on = vec!["lookup_narrow".to_string()];
    output.operation = ExecutionOperation::Output {
        value: json!({ "$ref": "$.nodes.lookup_narrow.output" }),
    };
    let validation = ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 2,
            reason: "Narrow failed map scope".to_string(),
            evidence: json!({}),
            operations: vec![
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "lookup".to_string(),
                    node: replacement,
                },
                PlanAmendmentOperation::ReplacePendingNode {
                    node_id: "output".to_string(),
                    node: output,
                },
            ],
        },
        projection: ExecutionProjection {
            plan_revision: 2,
            node_statuses: BTreeMap::new(),
            tasks: vec![],
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    };

    let accepted = validate_amendment(validation.clone());
    assert!(
        accepted.report.issues.is_empty(),
        "{:?}",
        accepted.report.issues
    );

    let mut broadened = validation;
    let PlanAmendmentOperation::ReplacePendingNode { node, .. } =
        &mut broadened.amendment.operations[0]
    else {
        panic!("map replacement operation");
    };
    let ExecutionOperation::Map {
        items, max_items, ..
    } = &mut node.operation
    else {
        panic!("map replacement node");
    };
    *items = json!([{ "id": 1 }, { "id": 2 }, { "id": 3 }, { "id": 4 }]);
    *max_items = 4;
    let rejected = validate_amendment(broadened);
    assert!(rejected.plan.is_none());
    assert!(
        rejected
            .report
            .issues
            .iter()
            .any(|issue| issue.code == "map_scope_broadened")
    );
}

fn valid_request() -> CompileExecutionRequest {
    let capability = capability("orders.lookup");
    let catalog_hash = catalog_hash(1, std::slice::from_ref(&capability)).expect("hash catalog");
    let reference = capability.reference.clone();
    CompileExecutionRequest {
        goal: ExecutionGoalContract {
            objective: "Return the order".to_string(),
            requirements: vec![ExecutionRequirement {
                id: "req_one".to_string(),
                description: "Look up the order".to_string(),
            }],
            deliverables: vec![],
            coverage: vec![],
            constraints: vec![],
            completion_checks: vec![CompletionCheck {
                id: "output_schema".to_string(),
                description: "Validate output".to_string(),
                requirement_ids: vec!["req_one".to_string()],
                constraint_ids: vec![],
                kind: CompletionCheckKind::OutputSchema,
            }],
        },
        plan: ExecutionPlanDefinition {
            schema_version: EXECUTION_PLAN_SCHEMA_VERSION,
            cancel_policy: ExecutionCancelPolicy::RetainEffects,
            input_schema: json!({
                "type": "object",
                "required": ["order_id"],
                "properties": { "order_id": { "type": "string" } }
            }),
            output_schema: json!({ "type": "object" }),
            nodes: vec![
                ExecutionNode {
                    id: "lookup".to_string(),
                    requirement_ids: vec!["req_one".to_string()],
                    depends_on: vec![],
                    when: None,
                    input: json!({ "order_id": { "$ref": "$.input.order_id" } }),
                    output_schema: json!({ "type": "object" }),
                    operation: ExecutionOperation::Capability {
                        reference: reference.clone(),
                    },
                    compensation: None,
                    retry: retry(2),
                    budget: None,
                },
                ExecutionNode {
                    id: "output".to_string(),
                    requirement_ids: vec!["req_one".to_string()],
                    depends_on: vec!["lookup".to_string()],
                    when: None,
                    input: json!({}),
                    output_schema: json!({ "type": "object" }),
                    operation: ExecutionOperation::Output {
                        value: json!({ "$ref": "$.nodes.lookup.output" }),
                    },
                    compensation: None,
                    retry: retry(1),
                    budget: None,
                },
            ],
        },
        run_input: json!({ "order_id": "ord-1" }),
        catalog: ExecutionCapabilityCatalog {
            schema_version: 1,
            capabilities: vec![capability],
            catalog_hash,
        },
        authorization: ExecutionAuthorizationEnvelope {
            capability_refs: vec![reference],
            skill_refs: vec![],
        },
        approved_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    }
}

fn compensated_request() -> CompileExecutionRequest {
    let mut request = valid_request();
    let mut forward = capability("orders.create");
    forward.action_class = ActionClass::ExternalWrite;
    forward.execution_class = ExecutionClass::External;
    forward.input_schema = json!({
        "type": "object",
        "required": ["order_id"],
        "properties": { "order_id": { "type": "string" } }
    });
    forward.output_schema = json!({
        "type": "object",
        "required": ["order_id"],
        "properties": { "order_id": { "type": "string" } }
    });

    let mut compensator = capability("orders.rollback");
    compensator.action_class = ActionClass::ExternalWrite;
    compensator.execution_class = ExecutionClass::External;
    compensator.input_schema = json!({
        "type": "object",
        "required": ["order_id"],
        "properties": { "order_id": { "type": "string" } }
    });
    let compensation = ExecutionCompensation {
        compensator: compensator.reference.clone(),
        input_mapping: CompensationInputMapping {
            bindings: vec![CompensationInputBinding {
                target_pointer: "/order_id".to_string(),
                source: CompensationValueSource::OriginalOutput {
                    pointer: "/order_id".to_string(),
                },
            }],
        },
    };
    forward.rollback = Some(CapabilityRollbackContract {
        compensator: compensation.compensator.clone(),
        input_mapping: compensation.input_mapping.clone(),
    });

    let catalog = ExecutionCapabilityCatalog::build(vec![forward.clone(), compensator])
        .expect("build compensated capability catalog");
    request.plan.cancel_policy = ExecutionCancelPolicy::CompensateCommitted;
    request.plan.nodes[0].operation = ExecutionOperation::Capability {
        reference: forward.reference,
    };
    request.plan.nodes[0].compensation = Some(compensation);
    request.catalog = catalog;
    request.authorization.capability_refs = request
        .catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    request
}

fn rehash_catalog(request: &mut CompileExecutionRequest) {
    request.catalog.catalog_hash = catalog_hash(
        request.catalog.schema_version,
        &request.catalog.capabilities,
    )
    .expect("rehash modified catalog fixture");
}

fn assert_issue_code(outcome: CompileExecutionOutcome, code: &str) {
    assert!(
        outcome.compiled.is_none(),
        "compiler unexpectedly accepted {code}"
    );
    assert!(
        outcome.report.issues.iter().any(|issue| issue.code == code),
        "expected issue code {code}, got {:?}",
        outcome.report.issues
    );
}

fn unknown_reference_issue(path: &str) -> ExecutionValidationIssue {
    ExecutionValidationIssue {
        severity: ExecutionValidationSeverity::Error,
        code: "unknown_reference_path".to_string(),
        path: path.to_string(),
        message: "execution reference path is not declared by its source schema".to_string(),
    }
}

fn immutable_node_issue(message: &str) -> ExecutionValidationIssue {
    ExecutionValidationIssue {
        severity: ExecutionValidationSeverity::Error,
        code: "immutable_node".to_string(),
        path: "amendment.operations[0].node_id".to_string(),
        message: message.to_string(),
    }
}

fn amendment_validation_for_output(
    node_status: Option<ExecutionNodeStatus>,
    task_status: ExecutionTaskStatus,
    replace: bool,
) -> ValidateAmendmentRequest {
    let request = valid_request();
    let compiled = compile(request.clone())
        .compiled
        .expect("compile active amendment fixture");
    let operation = if replace {
        let mut replacement = compiled.plan.definition.nodes[1].clone();
        replacement.id = "replacement_output".to_string();
        PlanAmendmentOperation::ReplacePendingNode {
            node_id: "output".to_string(),
            node: replacement,
        }
    } else {
        PlanAmendmentOperation::RemovePendingNode {
            node_id: "output".to_string(),
        }
    };
    let node_statuses = node_status
        .map(|status| BTreeMap::from([("output".to_string(), status)]))
        .unwrap_or_default();

    ValidateAmendmentRequest {
        goal: compiled.goal,
        active_plan: compiled.plan,
        amendment: PlanAmendment {
            schema_version: PLAN_AMENDMENT_SCHEMA_VERSION,
            base_plan_revision: 9,
            reason: "Validate task-backed amendment immutability".to_string(),
            evidence: json!({}),
            operations: vec![operation],
        },
        projection: ExecutionProjection {
            plan_revision: 9,
            node_statuses,
            tasks: vec![task_projection("output", task_status)],
        },
        catalog: request.catalog,
        authorization: request.authorization,
        remaining_budget: generous_budget(),
        config: ExecutionConfig::default(),
        now: now(),
    }
}

fn capability(name: &str) -> ExecutionCapability {
    let source = CapabilitySource::BuiltInTool {
        name: name.to_string(),
    };
    ExecutionCapability {
        reference: CapabilityReference {
            name: name.to_string(),
            version: "v1".to_string(),
        },
        contract_revision: "contract-v1".to_string(),
        description: format!("Capability {name}"),
        input_schema: json!({ "type": "object" }),
        output_schema: json!({ "type": "object" }),
        action_class: ActionClass::Read,
        risk_level: RiskLevel::Low,
        default_effect: ActionPolicyEffect::Allow,
        idempotency_class: IdempotencyClass::Idempotent,
        execution_class: ExecutionClass::Data,
        policy_context: CapabilityPolicyContext::registered(source.clone()),
        source,
        estimate: ExecutionEstimate {
            cost_microusd: 7,
            tokens: 11,
            tool_calls: 3,
            retrieved_bytes: 13,
            tasks: 1,
        },
        rollback: None,
    }
}

#[derive(Clone, Copy, Debug)]
enum GovernedAliasKind {
    Action,
    InstalledConnector,
    SkillAction,
}

fn aliased_tool_catalog(
    alias_kind: GovernedAliasKind,
) -> (
    ExecutionCapabilityCatalog,
    CapabilityReference,
    CapabilityReference,
) {
    let backing_tool_name = "orders.refund";
    let mut raw = capability("raw.orders_refund");
    let raw_source = CapabilitySource::BuiltInTool {
        name: backing_tool_name.to_string(),
    };
    raw.source = raw_source.clone();
    raw.policy_context = CapabilityPolicyContext::registered(raw_source);

    let revision_uid = Uuid::from_u128(202);
    let canonical_action_ref = ArtifactRef::action_artifact("reviewed-refund");
    let (reference_name, source, canonical_action_ref, artifact_uid, revision_uid) =
        match alias_kind {
            GovernedAliasKind::Action => (
                "action://reviewed-refund",
                CapabilitySource::ActionArtifact {
                    action_ref: canonical_action_ref.clone(),
                    revision_uid,
                    tool_name: backing_tool_name.to_string(),
                },
                canonical_action_ref.clone(),
                Uuid::from_u128(101),
                revision_uid,
            ),
            GovernedAliasKind::InstalledConnector => (
                "action://billing.refund",
                installed_connector_source(7, backing_tool_name),
                ArtifactRef::action("billing", "refund"),
                Uuid::from_u128(303),
                Uuid::from_u128(304),
            ),
            GovernedAliasKind::SkillAction => (
                "skill://reviewed-operations#refund",
                CapabilitySource::SkillAction {
                    skill_ref: ArtifactRef::from_str("skill://reviewed-operations")
                        .expect("fixture: parse skill reference"),
                    revision_uid,
                    action_id: "refund".to_string(),
                    tool_name: backing_tool_name.to_string(),
                },
                canonical_action_ref,
                Uuid::from_u128(101),
                revision_uid,
            ),
        };
    let mut alias = capability(reference_name);
    if let CapabilitySource::InstalledConnectorAction {
        governed_contract_revision,
        ..
    } = &source
    {
        alias.contract_revision = governed_contract_revision.clone();
    }
    alias.default_effect = ActionPolicyEffect::AdminReview;
    alias.source = source.clone();
    alias.policy_context = CapabilityPolicyContext::artifact(
        source,
        Some(canonical_action_ref),
        artifact_uid,
        revision_uid,
        ActionPolicyEffect::AdminReview,
    );

    let raw_reference = raw.reference.clone();
    let alias_reference = alias.reference.clone();
    let catalog = ExecutionCapabilityCatalog::build(vec![raw, alias])
        .expect("fixture: aliased capabilities should form a valid catalog");
    (catalog, raw_reference, alias_reference)
}

fn install_catalog(request: &mut CompileExecutionRequest, catalog: ExecutionCapabilityCatalog) {
    request.authorization.capability_refs = catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.clone())
        .collect();
    request.catalog = catalog;
    request.approved_budget.max_retrieved_bytes = None;
}

fn both_reference_orders(
    raw: &CapabilityReference,
    alias: &CapabilityReference,
) -> [Vec<CapabilityReference>; 2] {
    [
        vec![raw.clone(), alias.clone()],
        vec![alias.clone(), raw.clone()],
    ]
}

fn assert_ambiguous_agent_tool_rejected(
    outcome: CompileExecutionOutcome,
    path: &str,
    alias_kind: GovernedAliasKind,
) {
    let alias_reference = match alias_kind {
        GovernedAliasKind::Action => "action://reviewed-refund@v1",
        GovernedAliasKind::InstalledConnector => "action://billing.refund@v1",
        GovernedAliasKind::SkillAction => "skill://reviewed-operations#refund@v1",
    };
    let mut reference_labels = [alias_reference, "raw.orders_refund@v1"];
    reference_labels.sort();
    assert!(
        outcome.compiled.is_none(),
        "compiler accepted {alias_kind:?} and raw capability aliases of one model-visible tool"
    );
    assert_eq!(
        outcome.report.issues,
        vec![ExecutionValidationIssue {
            severity: ExecutionValidationSeverity::Error,
            code: "ambiguous_agent_capability_tool".to_string(),
            path: path.to_string(),
            message: format!(
                "task-local agent capability references {} and {} resolve to ambiguous model-visible tool `orders.refund`",
                reference_labels[0], reference_labels[1]
            ),
        }],
        "compiler must reject {alias_kind:?} ambiguity deterministically"
    );
}

fn installed_connector_tool_name() -> &'static str {
    "conn__0000000000000000000000000000012d__refund"
}

fn installed_connector_source(connection_generation: u64, tool_name: &str) -> CapabilitySource {
    CapabilitySource::InstalledConnectorAction {
        connector_ref: ArtifactRef::connector("billing"),
        connection_id: ConnectorConnectionId(Uuid::from_u128(301)),
        binding_id: Uuid::from_u128(302),
        connection_generation,
        definition_artifact_uid: Uuid::from_u128(303),
        definition_revision_uid: Uuid::from_u128(304),
        action_id: "refund".to_string(),
        contract_hash: "ab".repeat(32),
        governed_contract_revision: "connector-artifact:revision-304:contract-ab".to_string(),
        minimum_effect: ActionPolicyEffect::AdminReview,
        tool_name: tool_name.to_string(),
    }
}

fn installed_connector_capability(
    connection_generation: u64,
    tool_name: &str,
) -> ExecutionCapability {
    let source = installed_connector_source(connection_generation, tool_name);
    let mut capability = capability("action://billing.refund");
    capability.contract_revision = "connector-artifact:revision-304:contract-ab".to_string();
    capability.action_class = ActionClass::ExternalWrite;
    capability.default_effect = ActionPolicyEffect::AdminReview;
    capability.idempotency_class = IdempotencyClass::NonIdempotent;
    capability.execution_class = ExecutionClass::External;
    capability.source = source.clone();
    capability.policy_context = CapabilityPolicyContext::artifact(
        source,
        Some(ArtifactRef::action("billing", "refund")),
        Uuid::from_u128(303),
        Uuid::from_u128(304),
        ActionPolicyEffect::AdminReview,
    );
    capability
}

fn retry(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        initial_backoff_ms: 0,
        max_backoff_ms: 0,
    }
}

fn waiting_replan_task(node_id: &str) -> ExecutionTaskProjection {
    ExecutionTaskProjection {
        task_id: ExecutionTaskId::derive(Uuid::from_u128(77), node_id, "").expect("task id"),
        node_id: node_id.to_string(),
        item_key: String::new(),
        status: ExecutionTaskStatus::WaitingReplan,
        attempt: 1,
        generation: 1,
        input: json!({}),
        outcome: Some(ExecutionTaskOutcome {
            schema_version: 1,
            usage: ExecutionUsage {
                cost_microusd: 0,
                tokens: 0,
                tool_calls: 0,
                retrieved_bytes: 0,
            },
            result: ExecutionTaskResult::NeedsReplan {
                reason: "operation unsupported".to_string(),
                evidence: json!({}),
            },
        }),
    }
}

fn task_projection(node_id: &str, status: ExecutionTaskStatus) -> ExecutionTaskProjection {
    let outcome = (status == ExecutionTaskStatus::Completed).then(|| ExecutionTaskOutcome {
        schema_version: 1,
        usage: ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
        result: ExecutionTaskResult::Completed {
            output: json!({}),
            citations: vec![],
        },
    });
    ExecutionTaskProjection {
        task_id: ExecutionTaskId::derive(Uuid::from_u128(88), node_id, "").expect("task id"),
        node_id: node_id.to_string(),
        item_key: String::new(),
        status,
        attempt: 1,
        generation: 1,
        input: json!({}),
        outcome,
    }
}

fn generous_budget() -> ExecutionBudgetLimit {
    ExecutionBudgetLimit {
        max_cost_microusd: Some(1_000_000),
        max_tokens: Some(1_000_000),
        max_tasks: Some(1_000),
        max_tool_calls: Some(1_000),
        max_retrieved_bytes: Some(1_000_000),
        deadline_at: Some(
            Utc.with_ymd_and_hms(2030, 1, 1, 0, 0, 0)
                .single()
                .expect("time"),
        ),
    }
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 13, 12, 0, 0)
        .single()
        .expect("time")
}
