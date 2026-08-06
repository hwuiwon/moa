//! Inline regressions for execution-service pure helpers and projections.

use super::capability_catalog::{
    build_capability_response, build_skill_regression_compile_authority, single_tool_estimate,
};
use super::handlers::validate_start_source_provenance;
use super::planning_context::{
    PlanningSkillContext, build_planning_skill_context, skill_revision_ref,
};
use super::support::{
    durable_amendment_operation_fingerprints, durable_failure_fingerprint_counts,
    persisted_input_audience, validate_external_wait_payload,
};

use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use moa_artifacts::document::{ArtifactDefinition, ArtifactDocument, ArtifactKind, ArtifactStatus};
use moa_artifacts::execution_plan::{
    ExecutionGoalTemplate, ExecutionNode, ExecutionOperation, ExecutionPlanDefinition,
    ExecutionPlanTemplate, PlanAmendment, PlanAmendmentOperation, RetryPolicy,
};
use moa_artifacts::registry::StoredArtifactRevision;
use moa_artifacts::skill::SkillDefinition;
use moa_core::types::{
    action_policy::ActionPolicyEffect,
    agent::{AgentSkillPolicy, AgentSkillPolicyMode},
    execution_planning::{
        ExecutionPlanningContractError, ExecutionSourceProvenance, PinnedExecutionTemplateRef,
    },
};
use moa_execution::{
    CapabilityCatalogDiagnosticCode, CapabilitySource, ExecutionClass,
    capability::{amendment_hash, amendment_operations_fingerprint},
    replan::{
        ReplanDecision, ReplanLoopEvaluationRequest, ReplanStopReason, evaluate_replan_loop_stop,
        failure_fingerprint,
    },
    state::FailureFingerprintInput,
    wire::PinnedExecutionTemplate,
};
use moa_hands::{McpDiscoveredTool, ToolExecution, ToolRegistry};
use moa_test_support::fixture_capability::{
    REVERSIBLE_FIXTURE_COMPENSATOR_TOOL, REVERSIBLE_FIXTURE_FORWARD_TOOL,
    reversible_fixture_tool_definitions,
};
use serde_json::{Value, json};
use uuid::Uuid;

#[test]
fn tool_estimate_reserves_serialized_output_bytes_from_token_budget() {
    // Pins: a successful non-empty tool result cannot overrun a zero-byte reservation
    // before dependent reducer/output tasks have a chance to run.
    assert_eq!(single_tool_estimate(0).retrieved_bytes, 4);
    assert_eq!(single_tool_estimate(4_000).retrieved_bytes, 64_000);
    assert_eq!(
        single_tool_estimate(u32::MAX).retrieved_bytes,
        u64::from(u32::MAX) * 16
    );
}

#[test]
fn production_catalog_resolves_source_owned_fixture_rollback_to_exact_versions() {
    // Pins: rollback is usable through the real catalog projection; tests do not
    // synthesize CapabilityRollbackContract after the source declarations load.
    let (forward, compensator) = reversible_fixture_tool_definitions();
    let registrations = vec![
        (
            forward,
            ToolExecution::Mcp {
                server_name: "fixture".to_string(),
                remote_tool_name: REVERSIBLE_FIXTURE_FORWARD_TOOL.to_string(),
                schema_hash: "fixture-forward-v1".to_string(),
            },
        ),
        (
            compensator,
            ToolExecution::Mcp {
                server_name: "fixture".to_string(),
                remote_tool_name: REVERSIBLE_FIXTURE_COMPENSATOR_TOOL.to_string(),
                schema_hash: "fixture-compensator-v1".to_string(),
            },
        ),
    ];
    let response = build_capability_response(&registrations, &[], &[])
        .expect("source-declared reversible fixture pair should project");
    let forward = response
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference.name == REVERSIBLE_FIXTURE_FORWARD_TOOL)
        .expect("forward fixture capability should be catalogued");
    let compensator = response
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference.name == REVERSIBLE_FIXTURE_COMPENSATOR_TOOL)
        .expect("compensator fixture capability should be catalogued");
    let rollback = forward
        .rollback
        .as_ref()
        .expect("forward source declaration should resolve to an exact rollback");
    assert_eq!(rollback.compensator, compensator.reference);
    assert_eq!(rollback.input_mapping.bindings.len(), 1);
    assert_eq!(
        rollback.input_mapping.bindings[0].target_pointer,
        "/effect_id"
    );
}

#[test]
fn connector_definitions_require_an_installed_connection_for_capability_projection() {
    // Pins: a reviewed definition alone never becomes executable; only an
    // authorized exact-generation installed action enters the catalog.
    let registry = ToolRegistry::default_local();
    let revisions = vec![revision(
        "runtime-billing",
        62,
        document(
            "runtime-billing",
            ArtifactKind::Connector,
            serde_json::from_value(json!({
                "type": "connector",
                "spec": {
                    "display_name": "Runtime billing",
                    "auth": [{"type": "none"}],
                    "actions": [{
                        "id": "charge",
                        "contract": {
                            "method": "POST",
                            "path_template": "/charges",
                            "max_request_bytes": 1024,
                            "max_response_bytes": 1024,
                            "connect_timeout_ms": 1000,
                            "total_timeout_ms": 2000,
                            "policy": {
                                "input_schema": {"type": "object"},
                                "output_schema": {"type": "object"},
                                "data_classes": ["none"],
                                "idempotency": "non_idempotent"
                            }
                        }
                    }]
                }
            }))
            .expect("runtime connector fixture should decode"),
        ),
    )];

    let response = build_capability_response(&registry.capability_registrations(), &revisions, &[])
        .expect("mixed connector definitions should build the current catalog");
    let names = response
        .catalog
        .capabilities
        .iter()
        .map(|capability| capability.reference.name.as_str())
        .collect::<Vec<_>>();

    assert_eq!(
        names
            .iter()
            .filter(|name| **name == "action://runtime-billing.charge")
            .count(),
        0,
        "a runtime connector requires an installed connection before it can enter a catalog"
    );
}

fn revision(name: &str, revision_uid: u128, document: ArtifactDocument) -> StoredArtifactRevision {
    StoredArtifactRevision {
        artifact_uid: Uuid::from_u128(revision_uid + 100),
        revision_uid: Uuid::from_u128(revision_uid),
        storage_partition_id: None,
        user_id: None,
        scope: "tenant".to_string(),
        kind: document.kind.clone(),
        name: name.to_string(),
        description: document.metadata.description.clone(),
        tags: Vec::new(),
        document,
        canonical_hash: vec![1],
        source_format: "json".to_string(),
        source_text: Vec::new(),
        status: ArtifactStatus::Ready,
        validation_report: json!({}),
        version: 1,
        published_at: Some(Utc::now()),
        valid_to: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn document(name: &str, kind: ArtifactKind, definition: ArtifactDefinition) -> ArtifactDocument {
    serde_json::from_value(json!({
        "api_version": "moa/v1",
        "kind": kind,
        "metadata": {"name": name, "description": format!("{name} description")},
        "status": "published",
        "definition": definition,
        "ui": {},
        "reference_resolutions": []
    }))
    .expect("artifact fixture should decode")
}

fn skill_revision(name: &str, revision_uid: u128) -> StoredArtifactRevision {
    revision(
        name,
        revision_uid,
        document(
            name,
            ArtifactKind::Skill,
            ArtifactDefinition::Skill(SkillDefinition {
                instructions: Default::default(),
                inputs: json!({"type": "object"}),
                outputs: json!({"type": "object"}),
                actions: Vec::new(),
                connectors: Vec::new(),
                allowed_tools: Vec::new(),
                execution_plan: Some(ExecutionPlanTemplate {
                    goal: ExecutionGoalTemplate {
                        requirements: Vec::new(),
                        deliverables: Vec::new(),
                        coverage: Vec::new(),
                        constraints: Vec::new(),
                        completion_checks: Vec::new(),
                    },
                    plan: ExecutionPlanDefinition {
                        schema_version: 2,
                        cancel_policy:
                            moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
                        input_schema: json!({"type": "object"}),
                        output_schema: json!({"type": "object"}),
                        nodes: Vec::new(),
                    },
                }),
                ui: json!({}),
            }),
        ),
    )
}

fn skill_revision_with_status(
    name: &str,
    revision_uid: u128,
    status: ArtifactStatus,
) -> StoredArtifactRevision {
    let mut revision = skill_revision(name, revision_uid);
    revision.status = status.clone();
    revision.document.status = status;
    revision
}

fn selected_skill_refs(context: &PlanningSkillContext) -> Vec<(String, Uuid)> {
    context
        .pinned_instruction_skills
        .iter()
        .map(|skill| (skill.skill_ref.to_string(), skill.revision_uid))
        .collect()
}

fn selected_revision_refs(context: &PlanningSkillContext) -> Vec<(String, Uuid)> {
    context
        .revisions
        .iter()
        .filter(|revision| matches!(revision.document.definition, ArtifactDefinition::Skill(_)))
        .map(|revision| (format!("skill://{}", revision.name), revision.revision_uid))
        .collect()
}

fn selected_template_refs(context: &PlanningSkillContext) -> Vec<(String, Uuid)> {
    context
        .execution_templates
        .iter()
        .map(|template| (template.skill_ref.to_string(), template.revision_uid))
        .collect()
}

fn assert_selected_skill_revisions(context: &PlanningSkillContext, expected: &[(String, Uuid)]) {
    assert_eq!(selected_skill_refs(context), expected);
    assert_eq!(selected_revision_refs(context), expected);
    assert_eq!(selected_template_refs(context), expected);
}

#[test]
fn planning_context_auto_uses_session_locked_revision() {
    // Pins: Auto chooses visible skills, then every matching session lock
    // substitutes the exact revision before authority/templates are derived.
    let policy = AgentSkillPolicy {
        mode: AgentSkillPolicyMode::Auto,
        refs: Vec::new(),
        max_visible: None,
    };
    let context = build_planning_skill_context(
        vec![skill_revision("alpha", 2), skill_revision("beta", 4)],
        vec![skill_revision_with_status(
            "alpha",
            1,
            ArtifactStatus::Superseded,
        )],
        &policy,
        None,
    )
    .expect("Auto selection should accept a matching older session lock");
    assert_selected_skill_revisions(
        &context,
        &[
            ("skill://alpha".to_string(), Uuid::from_u128(1)),
            ("skill://beta".to_string(), Uuid::from_u128(4)),
        ],
    );
}

#[test]
fn planning_context_denylist_substitutes_locks_without_restoring_denied_skills() {
    // Pins: Denylist substitutes locks only for selected non-denied skills;
    // a matching lock never restores a denied skill to planning authority.
    let policy = AgentSkillPolicy {
        mode: AgentSkillPolicyMode::Denylist,
        refs: vec!["skill://beta".to_string()],
        max_visible: None,
    };
    let context = build_planning_skill_context(
        vec![
            skill_revision("alpha", 2),
            skill_revision("beta", 4),
            skill_revision("gamma", 6),
        ],
        vec![
            skill_revision("alpha", 1),
            skill_revision("beta", 3),
            skill_revision("gamma", 5),
        ],
        &policy,
        None,
    )
    .expect("Denylist selection should substitute only non-denied locks");
    assert_selected_skill_revisions(
        &context,
        &[
            ("skill://alpha".to_string(), Uuid::from_u128(1)),
            ("skill://gamma".to_string(), Uuid::from_u128(5)),
        ],
    );
}

#[test]
fn planning_context_lock_substitution_preserves_max_visible_and_order() {
    // Pins: max_visible and reference ordering are resolved from policy
    // selection before matching locks replace revisions deterministically.
    let policy = AgentSkillPolicy {
        mode: AgentSkillPolicyMode::Auto,
        refs: Vec::new(),
        max_visible: Some(2),
    };
    let locks = vec![
        skill_revision("gamma", 5),
        skill_revision("beta", 3),
        skill_revision("alpha", 1),
    ];
    let forward = build_planning_skill_context(
        vec![
            skill_revision("gamma", 6),
            skill_revision("alpha", 2),
            skill_revision("beta", 4),
        ],
        locks.clone(),
        &policy,
        None,
    )
    .expect("forward selection should be valid");
    let reverse = build_planning_skill_context(
        vec![
            skill_revision("beta", 4),
            skill_revision("alpha", 2),
            skill_revision("gamma", 6),
        ],
        locks,
        &policy,
        None,
    )
    .expect("reverse selection should be valid");
    let expected = [
        ("skill://alpha".to_string(), Uuid::from_u128(1)),
        ("skill://beta".to_string(), Uuid::from_u128(3)),
    ];
    assert_selected_skill_revisions(&forward, &expected);
    assert_selected_skill_revisions(&reverse, &expected);
}

#[test]
fn planning_context_skill_policy_allowlist_and_denylist_never_broaden() {
    // Pins: planning authority includes only the session-pinned allowlist and excludes every denylisted skill.
    let revisions = vec![
        skill_revision("gamma", 3),
        skill_revision("alpha", 1),
        skill_revision("beta", 2),
    ];
    let allowlist = AgentSkillPolicy {
        mode: AgentSkillPolicyMode::Allowlist,
        refs: vec!["skill://beta".to_string()],
        max_visible: None,
    };
    let allowed = build_planning_skill_context(revisions.clone(), Vec::new(), &allowlist, None)
        .expect("valid allowlist should select planning skills");
    assert_eq!(
        selected_skill_refs(&allowed),
        vec![("skill://beta".to_string(), Uuid::from_u128(2))]
    );

    let denylist = AgentSkillPolicy {
        mode: AgentSkillPolicyMode::Denylist,
        refs: vec!["skill://beta".to_string()],
        max_visible: None,
    };
    let denied = build_planning_skill_context(revisions, Vec::new(), &denylist, None)
        .expect("valid denylist should select planning skills");
    assert_eq!(
        selected_skill_refs(&denied),
        vec![
            ("skill://alpha".to_string(), Uuid::from_u128(1)),
            ("skill://gamma".to_string(), Uuid::from_u128(3)),
        ]
    );
}

#[test]
fn planning_context_skill_policy_max_visible_is_deterministic_and_pinned_first() {
    // Pins: max_visible selection is input-order independent and reserves capacity for pinned refs.
    let policy = AgentSkillPolicy {
        mode: AgentSkillPolicyMode::Pinned,
        refs: vec!["skill://gamma".to_string()],
        max_visible: Some(2),
    };
    let forward = build_planning_skill_context(
        vec![
            skill_revision("gamma", 3),
            skill_revision("alpha", 1),
            skill_revision("beta", 2),
        ],
        Vec::new(),
        &policy,
        None,
    )
    .expect("pinned policy should select planning skills");
    let reverse = build_planning_skill_context(
        vec![
            skill_revision("beta", 2),
            skill_revision("alpha", 1),
            skill_revision("gamma", 3),
        ],
        Vec::new(),
        &policy,
        None,
    )
    .expect("reordered input should select the same planning skills");
    let expected = vec![
        ("skill://alpha".to_string(), Uuid::from_u128(1)),
        ("skill://gamma".to_string(), Uuid::from_u128(3)),
    ];
    assert_eq!(selected_skill_refs(&forward), expected);
    assert_eq!(selected_skill_refs(&reverse), expected);
}

#[test]
fn planning_context_rejects_explicit_disallowed_template() {
    // Pins: an exact activated template remains unusable when the session allowlist excludes it.
    let policy = AgentSkillPolicy {
        mode: AgentSkillPolicyMode::Allowlist,
        refs: vec!["skill://alpha".to_string()],
        max_visible: None,
    };
    let requested = PinnedExecutionTemplateRef {
        skill_ref: "skill://beta".to_string(),
        revision_uid: Uuid::from_u128(2),
    };
    let error = build_planning_skill_context(
        vec![skill_revision("alpha", 1), skill_revision("beta", 2)],
        Vec::new(),
        &policy,
        Some(&requested),
    )
    .expect_err("disallowed exact template must fail closed");
    assert_eq!(
        error,
        "requested execution template is not an exact permitted pinned activated revision"
    );
}

#[test]
fn planning_context_rejects_non_executable_exact_skill_statuses() {
    // Pins: exact pins may preserve activated history through supersession,
    // but draft, published, and archived revisions never gain execution
    // authority from an agent dependency lock.
    for status in [
        ArtifactStatus::Draft,
        ArtifactStatus::Published,
        ArtifactStatus::Archived,
    ] {
        let revision = skill_revision_with_status("alpha", 1, status.clone());
        let error = skill_revision_ref(&revision)
            .expect_err("non-executable exact skill status must fail closed");
        assert_eq!(
            error,
            format!(
                "planning skill revision {} is {status} and is not executable exact-pinned skill content",
                revision.revision_uid
            )
        );
    }

    for status in [ArtifactStatus::Ready, ArtifactStatus::Superseded] {
        let revision = skill_revision_with_status("alpha", 1, status);
        assert_eq!(
            skill_revision_ref(&revision)
                .expect("activated exact skill status should remain executable"),
            "skill://alpha"
        );
    }
}

#[test]
fn planning_context_uses_locked_revision_and_rejects_duplicate_exact_revision() {
    // Pins: Allowlist and Pinned keep exact locked behavior, while duplicate
    // exact revisions remain ambiguous and fail closed.
    for mode in [
        AgentSkillPolicyMode::Allowlist,
        AgentSkillPolicyMode::Pinned,
    ] {
        let policy = AgentSkillPolicy {
            mode,
            refs: vec!["skill://alpha".to_string()],
            max_visible: None,
        };
        let locked = build_planning_skill_context(
            vec![skill_revision("alpha", 2)],
            vec![skill_revision("alpha", 1)],
            &policy,
            None,
        )
        .expect("locked policy revision should replace the latest activation");
        assert_selected_skill_revisions(
            &locked,
            &[("skill://alpha".to_string(), Uuid::from_u128(1))],
        );
    }

    let policy = AgentSkillPolicy {
        mode: AgentSkillPolicyMode::Allowlist,
        refs: vec!["skill://alpha".to_string()],
        max_visible: None,
    };
    let duplicate = build_planning_skill_context(
        vec![skill_revision("alpha", 1), skill_revision("alpha", 1)],
        Vec::new(),
        &policy,
        None,
    )
    .expect_err("duplicate exact revisions must fail closed");
    assert_eq!(
        duplicate,
        "duplicate exact skill revision: skill://alpha@00000000-0000-0000-0000-000000000001"
    );

    let duplicate_locked = build_planning_skill_context(
        vec![skill_revision("alpha", 2)],
        vec![skill_revision("alpha", 1), skill_revision("alpha", 1)],
        &policy,
        None,
    )
    .expect_err("duplicate exact locked revisions must fail closed");
    assert_eq!(
        duplicate_locked,
        "duplicate exact locked skill revision: skill://alpha@00000000-0000-0000-0000-000000000001"
    );
}

#[test]
fn accepted_turn_requires_skill_template_provenance_from_planning_snapshot() {
    // Pins: Execution/start cannot admit a fabricated template revision as run provenance.
    let skill_ref = "skill://durable-report"
        .parse::<moa_artifacts::reference::ArtifactRef>()
        .expect("canonical skill ref");
    let pinned_revision_uid = Uuid::from_u128(7);
    let templates = vec![PinnedExecutionTemplate {
        skill_ref: skill_ref.clone(),
        revision_uid: pinned_revision_uid,
        skill_input_schema: json!({"type": "object"}),
        execution_plan: ExecutionPlanTemplate {
            goal: ExecutionGoalTemplate {
                requirements: Vec::new(),
                deliverables: Vec::new(),
                coverage: Vec::new(),
                constraints: Vec::new(),
                completion_checks: Vec::new(),
            },
            plan: ExecutionPlanDefinition {
                schema_version: 2,
                cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                nodes: Vec::new(),
            },
        },
    }];
    let committed_plan_hash = "a".repeat(64);
    let exact = ExecutionSourceProvenance::SkillTemplate {
        skill_template_ref: skill_ref.to_string(),
        skill_template_revision_uid: pinned_revision_uid,
    };
    assert_eq!(
        validate_start_source_provenance(&exact, &committed_plan_hash, &templates),
        Ok(())
    );

    let wrong_revision = ExecutionSourceProvenance::SkillTemplate {
        skill_template_ref: skill_ref.to_string(),
        skill_template_revision_uid: Uuid::from_u128(8),
    };
    assert_eq!(
        validate_start_source_provenance(&wrong_revision, &committed_plan_hash, &templates,),
        Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_revision_uid".to_string(),
            message: "must equal one exact template revision in the persisted planning context"
                .to_string(),
        })
    );

    let noncanonical = ExecutionSourceProvenance::SkillTemplate {
        skill_template_ref: "skill://Durable-Report".to_string(),
        skill_template_revision_uid: pinned_revision_uid,
    };
    assert!(matches!(
        validate_start_source_provenance(
            &noncanonical,
            &committed_plan_hash,
            &templates,
        ),
        Err(ExecutionPlanningContractError::InvalidField { field, .. })
            if field == "skill_template_ref"
    ));
}

fn experiment_template_provenance(
    skill_template_ref: String,
    skill_template_revision_uid: Uuid,
) -> ExecutionSourceProvenance {
    ExecutionSourceProvenance::ExperimentTemplate {
        skill_template_ref,
        skill_template_revision_uid,
        experiment_run_uid: Uuid::from_u128(21),
        score_run_id: Uuid::from_u128(22),
        trial_uid: Some(Uuid::from_u128(23)),
    }
}

fn pinned_execution_template(
    skill_ref: moa_artifacts::reference::ArtifactRef,
    revision_uid: Uuid,
) -> PinnedExecutionTemplate {
    PinnedExecutionTemplate {
        skill_ref,
        revision_uid,
        skill_input_schema: json!({"type": "object"}),
        execution_plan: ExecutionPlanTemplate {
            goal: ExecutionGoalTemplate {
                requirements: Vec::new(),
                deliverables: Vec::new(),
                coverage: Vec::new(),
                constraints: Vec::new(),
                completion_checks: Vec::new(),
            },
            plan: ExecutionPlanDefinition {
                schema_version: 2,
                cancel_policy: moa_artifacts::execution_plan::ExecutionCancelPolicy::RetainEffects,
                input_schema: json!({"type": "object"}),
                output_schema: json!({"type": "object"}),
                nodes: Vec::new(),
            },
        },
    }
}

#[test]
fn experiment_template_provenance_rejects_unknown_ref_from_planning_snapshot() {
    // Pins: ExperimentTemplate cannot name a canonical skill absent from the immutable context.
    let pinned_ref = "skill://durable-report"
        .parse::<moa_artifacts::reference::ArtifactRef>()
        .expect("canonical pinned skill ref");
    let templates = vec![pinned_execution_template(pinned_ref, Uuid::from_u128(7))];
    let provenance =
        experiment_template_provenance("skill://other-report".to_string(), Uuid::from_u128(7));

    assert_eq!(
        validate_start_source_provenance(&provenance, &"a".repeat(64), &templates),
        Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_ref".to_string(),
            message:
                "must equal one canonical template reference in the persisted planning context"
                    .to_string(),
        })
    );
}

#[test]
fn experiment_template_provenance_rejects_wrong_revision_from_planning_snapshot() {
    // Pins: ExperimentTemplate cannot substitute a revision absent from the immutable context.
    let pinned_ref = "skill://durable-report"
        .parse::<moa_artifacts::reference::ArtifactRef>()
        .expect("canonical pinned skill ref");
    let templates = vec![pinned_execution_template(
        pinned_ref.clone(),
        Uuid::from_u128(7),
    )];
    let provenance = experiment_template_provenance(pinned_ref.to_string(), Uuid::from_u128(8));

    assert_eq!(
        validate_start_source_provenance(&provenance, &"a".repeat(64), &templates),
        Err(ExecutionPlanningContractError::InvalidField {
            field: "skill_template_revision_uid".to_string(),
            message: "must equal one exact template revision in the persisted planning context"
                .to_string(),
        })
    );
}

#[test]
fn experiment_template_provenance_rejects_noncanonical_ref() {
    // Pins: ExperimentTemplate stores the byte-identical canonical template reference.
    let pinned_ref = "skill://durable-report"
        .parse::<moa_artifacts::reference::ArtifactRef>()
        .expect("canonical pinned skill ref");
    let templates = vec![pinned_execution_template(pinned_ref, Uuid::from_u128(7))];
    let provenance =
        experiment_template_provenance("skill://Durable-Report".to_string(), Uuid::from_u128(7));

    assert!(matches!(
        validate_start_source_provenance(&provenance, &"a".repeat(64), &templates),
        Err(ExecutionPlanningContractError::InvalidField { field, .. })
            if field == "skill_template_ref"
    ));
}

#[test]
fn experiment_template_provenance_accepts_exact_persisted_revision() {
    // Pins: ExperimentTemplate admits the exact canonical ref and revision pinned in context.
    let pinned_ref = "skill://durable-report"
        .parse::<moa_artifacts::reference::ArtifactRef>()
        .expect("canonical pinned skill ref");
    let pinned_revision_uid = Uuid::from_u128(7);
    let templates = vec![pinned_execution_template(
        pinned_ref.clone(),
        pinned_revision_uid,
    )];
    let provenance = experiment_template_provenance(pinned_ref.to_string(), pinned_revision_uid);

    assert_eq!(
        validate_start_source_provenance(&provenance, &"a".repeat(64), &templates),
        Ok(())
    );
}

#[test]
fn artifact_policy_floor_is_inherited_by_skill_action_alias() {
    // Pins: a SkillAction alias cannot weaken the exact Action review floor
    // while reusing the referenced binding's governed backing tool.
    let registry = ToolRegistry::default_local();
    let revisions = vec![
        revision(
            "publish-note",
            10,
            document(
                "publish-note",
                ArtifactKind::Action,
                serde_json::from_value(json!({
                    "type": "action",
                    "spec": {
                        "id": "publish-note",
                        "description": "publish a note",
                        "tool_name": "bash",
                        "input_schema": {"type": "object"},
                        "output_schema": {"type": "object"},
                        "admin_review_required": true
                    }
                }))
                .expect("action definition fixture should decode"),
            ),
        ),
        revision(
            "reviewed-operations",
            30,
            document(
                "reviewed-operations",
                ArtifactKind::Skill,
                serde_json::from_value(json!({
                    "type": "skill",
                    "spec": {
                        "instructions": {"path": "SKILL.md"},
                        "inputs": {"type": "object"},
                        "outputs": {"type": "object"},
                        "actions": [{
                            "id": "publish",
                            "description": "publish through the action alias",
                            "kind": "connector_action",
                            "ref": "action://publish-note",
                            "input_schema": {"type": "object"},
                            "output_schema": {"type": "object"}
                        }]
                    }
                }))
                .expect("skill definition fixture should decode"),
            ),
        ),
    ];

    let response = build_capability_response(&registry.capability_registrations(), &revisions, &[])
        .expect("artifact aliases should build into the production catalog");
    let alias = "skill://reviewed-operations#publish";
    let capability = response
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference.name == alias)
        .unwrap_or_else(|| panic!("production catalog omitted skill alias `{alias}`"));
    assert_eq!(
        capability.policy_context.minimum_effect,
        ActionPolicyEffect::AdminReview,
        "skill alias `{alias}` weakened the referenced artifact policy floor"
    );
    assert_eq!(
        capability
            .policy_context
            .canonical_action_ref
            .as_ref()
            .map(ToString::to_string),
        Some("action://publish-note".to_string())
    );
    assert_eq!(
        capability.policy_context.artifact_uid,
        Some(Uuid::from_u128(130)),
        "skill alias must preserve the skill artifact identity"
    );
    assert_eq!(
        capability.policy_context.revision_uid,
        Some(Uuid::from_u128(30)),
        "skill alias must preserve the skill revision identity"
    );
    assert!(matches!(
        &capability.source,
        CapabilitySource::SkillAction { tool_name, .. } if tool_name == "bash"
    ));
}

#[test]
fn capability_catalog_uses_live_execution_metadata_and_omits_non_invocable_declarations() {
    // Pins: only router-owned tools and artifact wrappers with live backing tools enter the catalog.
    let mut registry = ToolRegistry::default_local();
    registry
        .register_mcp_tool(
            "github",
            McpDiscoveredTool {
                name: "github_issue_create".to_string(),
                description: "create an issue".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {"title": {"type": "string"}},
                    "required": ["title"]
                }),
            },
        )
        .expect("MCP fixture should register");
    let revisions = vec![
        revision(
            "publish-note",
            10,
            document(
                "publish-note",
                ArtifactKind::Action,
                serde_json::from_value(json!({
                    "type": "action",
                    "spec": {
                        "id": "publish-note",
                        "description": "publish a note",
                        "tool_name": "bash",
                        "input_schema": {"type": "object", "required": ["body"]},
                        "output_schema": {"type": "object", "required": ["published"]},
                        "admin_review_required": true,
                        "ui": {}
                    }
                }))
                .expect("action definition fixture should decode"),
            ),
        ),
        revision(
            "missing-action",
            11,
            document(
                "missing-action",
                ArtifactKind::Action,
                serde_json::from_value(json!({
                    "type": "action",
                    "spec": {
                        "id": "missing-action",
                        "description": "not executable",
                        "tool_name": "not_registered",
                        "input_schema": {},
                        "output_schema": {},
                        "ui": {}
                    }
                }))
                .expect("action definition fixture should decode"),
            ),
        ),
        revision(
            "code-skill",
            12,
            document(
                "code-skill",
                ArtifactKind::Skill,
                serde_json::from_value(json!({
                    "type": "skill",
                    "spec": {
                        "instructions": {"path": "SKILL.md"},
                        "inputs": {},
                        "outputs": {},
                        "actions": [{
                            "id": "run-code",
                            "description": "run unowned code",
                            "kind": "code",
                            "runtime": "python",
                            "entrypoint": "main.py",
                            "input_schema": {},
                            "output_schema": {},
                            "ui": {}
                        }],
                        "connectors": [],
                        "allowed_tools": [],
                        "ui": {}
                    }
                }))
                .expect("skill definition fixture should decode"),
            ),
        ),
    ];

    let response = build_capability_response(
        &registry.capability_registrations(),
        &revisions,
        &["connection-123".to_string()],
    )
    .expect("capability catalog should build");

    // The pin this bug needed. Every catalogued capability must resolve, via
    // the SAME function durable execution uses, to a name the router
    // actually knows. This is a whole-catalog property rather than a
    // per-source assertion precisely because the failure it catches is a
    // source variant contributing a name from the wrong namespace — which
    // type-checks, and fails only when a live run dispatches it.
    for capability in &response.catalog.capabilities {
        let Ok(dispatch_name) = crate::workflows::execution_task::capability_tool_name(capability)
        else {
            continue;
        };
        assert!(
            registry.get(&dispatch_name).is_some(),
            "capability {} resolves to `{dispatch_name}`, which the router does not know; \
                 source {:?}",
            capability.reference.name,
            capability.source
        );
    }

    let action = response
        .catalog
        .capabilities
        .iter()
        .find(|entry| entry.reference.name == "action://publish-note")
        .expect("resolved action should be catalogued");
    assert_eq!(action.reference.version, Uuid::from_u128(10).to_string());
    assert_eq!(
        action.input_schema,
        json!({"type": "object", "required": ["body"]})
    );
    assert_eq!(
        action.output_schema,
        json!({"type": "object", "required": ["published"]})
    );
    assert_eq!(
        action.default_effect,
        moa_core::types::action_policy::ActionPolicyEffect::AdminReview
    );
    assert!(matches!(
        &action.source,
        CapabilitySource::ActionArtifact { tool_name, revision_uid, .. }
            if tool_name == "bash" && *revision_uid == Uuid::from_u128(10)
    ));
    assert_eq!(action.estimate.tool_calls, 1);
    assert_eq!(action.estimate.tasks, 1);

    // The catalog reference is server-qualified while the source records the
    // name the connector itself publishes: one identifies the capability
    // unambiguously across connectors, the other keeps provenance answerable
    // in the connector's own terms.
    let mcp = response
        .catalog
        .capabilities
        .iter()
        .find(|entry| {
            entry.reference.name == moa_hands::mcp_tool_reference("github", "github_issue_create")
        })
        .expect("connected MCP tool should be catalogued");
    assert_eq!(mcp.execution_class, ExecutionClass::External);
    assert_eq!(
        mcp.input_schema,
        json!({
            "type": "object",
            "properties": {"title": {"type": "string"}},
            "required": ["title"]
        })
    );
    assert!(matches!(
        &mcp.source,
        CapabilitySource::McpTool { server, tool_name, remote_name }
            if server == "github"
                && tool_name == &moa_hands::mcp_tool_reference("github", "github_issue_create")
                && remote_name == "github_issue_create"
    ));
    assert!(!mcp.contract_revision.is_empty());
    assert!(!mcp.reference.version.is_empty());

    assert!(!response.catalog.capabilities.iter().any(|entry| {
        matches!(
            &entry.source,
            CapabilitySource::SkillCode { .. } | CapabilitySource::Knowledge { .. }
        ) || entry.reference.name == "action://missing-action"
            || entry.reference.name == "connection-123"
    }));
    assert_eq!(
        response
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.code)
            .collect::<Vec<_>>(),
        vec![
            CapabilityCatalogDiagnosticCode::UnresolvedActionTool,
            CapabilityCatalogDiagnosticCode::ConnectionOnlyDataSource,
            CapabilityCatalogDiagnosticCode::UnownedSkillCode,
        ]
    );
}

#[test]
fn skill_regression_authority_uses_governed_catalog_and_exact_skill_allowlist() {
    // Pins: review compilation derives capability and skill authority from the same
    // production catalog builder, including the exact draft without duplicating its
    // stable skill ref when a previous revision is published.
    let registry = ToolRegistry::default_local();
    let published = skill_revision("reviewed-skill", 20);
    let mut draft = skill_revision("reviewed-skill", 21);
    draft.status = ArtifactStatus::Draft;
    draft.published_at = None;
    let authority = build_skill_regression_compile_authority(
        &registry.capability_registrations(),
        &[published, draft, skill_revision("dependency", 22)],
        &[],
    )
    .expect("skill regression authority should resolve");

    assert!(
        authority
            .catalog
            .capabilities
            .iter()
            .any(|capability| capability.reference.name == "file_read")
    );
    assert_eq!(
        authority.authorization.capability_refs,
        authority
            .catalog
            .capabilities
            .iter()
            .map(|capability| capability.reference.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        authority
            .authorization
            .skill_refs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        vec![
            "skill://dependency".to_string(),
            "skill://reviewed-skill".to_string(),
        ]
    );
}

#[test]
fn execution_external_wait_payload_is_validated_against_node_schema() {
    // Pins: review and signal handlers cannot persist caller-supplied output
    // that the active immutable plan would reject.
    let plan = serde_json::from_value(json!({
        "schema_version": 2,
        "cancel_policy": "retain_effects",
        "input_schema": {},
        "output_schema": {},
        "nodes": [{
            "id": "review",
            "requirement_ids": ["approval"],
            "depends_on": [],
            "when": null,
            "input": {},
            "output_schema": {
                "type": "object",
                "required": ["approved"],
                "properties": {"approved": {"type": "boolean"}}
            },
            "operation": {"kind": "review", "prompt": "Approve?"},
            "compensation": null,
            "retry": {"max_attempts": 1, "initial_backoff_ms": 1, "max_backoff_ms": 1},
            "budget": null
        }]
    }))
    .expect("plan fixture should decode");

    validate_external_wait_payload(&plan, "review", &json!({"approved": true}))
        .expect("valid external output should pass");
    assert!(
        validate_external_wait_payload(&plan, "review", &Value::String("bypass".to_string()))
            .is_err(),
        "schema-invalid caller output must be rejected"
    );
}

#[test]
fn replan_failure_counts_include_append_only_superseded_history() {
    // Pins: superseding a NeedsReplan task cannot erase its normalized
    // failure occurrence from the next amendment stop evaluation.
    let failure = FailureFingerprintInput {
        class: moa_artifacts::execution_plan::ExecutionFailureClass::Terminal,
        node_id: "collect".to_string(),
        capability_ref: None,
        message: " Source   Unavailable ".to_string(),
    };
    let fingerprint = failure_fingerprint(&failure).expect("failure should hash");
    let history = vec![
        json!({
            "failure_fingerprint": fingerprint,
            "failure_fingerprint_count": 1
        }),
        json!({
            "failure_fingerprint": fingerprint,
            "failure_fingerprint_count": 2
        }),
    ];
    assert_eq!(
        durable_failure_fingerprint_counts(&history),
        [(fingerprint, 2)].into_iter().collect()
    );
}

#[test]
fn replan_history_detects_duplicate_operations_without_exact_replay() {
    // Pins: the service derives semantic loop identity from persisted amendment values, so a
    // later base revision and changed prose reach DuplicateAmendment without colliding with
    // the repository's full amendment replay hash.
    let operation = PlanAmendmentOperation::AddNode {
        node: ExecutionNode {
            id: "replacement".to_string(),
            requirement_ids: vec!["req_report".to_string()],
            depends_on: vec!["prepared".to_string()],
            when: None,
            input: json!({}),
            output_schema: json!({"type": "object"}),
            operation: ExecutionOperation::Output {
                value: json!({"report": true}),
            },
            compensation: None,
            retry: RetryPolicy {
                max_attempts: 1,
                initial_backoff_ms: 0,
                max_backoff_ms: 0,
            },
            budget: None,
        },
    };
    let first = PlanAmendment {
        schema_version: 2,
        base_plan_revision: 1,
        reason: "first explanation".to_string(),
        evidence: json!({"source": "first"}),
        operations: vec![operation.clone()],
    };
    let proposed = PlanAmendment {
        schema_version: 2,
        base_plan_revision: 2,
        reason: "different explanation".to_string(),
        evidence: json!({"source": "second"}),
        operations: vec![operation],
    };
    assert_ne!(
        amendment_hash(&first).expect("hash first exact amendment"),
        amendment_hash(&proposed).expect("hash proposed exact amendment")
    );
    let seen = durable_amendment_operation_fingerprints(&[json!({"amendment": first})])
        .expect("persisted amendment values should fingerprint");
    let decision = evaluate_replan_loop_stop(ReplanLoopEvaluationRequest {
        proposed_amendment_fingerprint: amendment_operations_fingerprint(&proposed)
            .expect("fingerprint proposed operations"),
        seen_amendment_fingerprints: seen,
        failure_fingerprint_counts: BTreeMap::new(),
        current_failure: None,
        unresolved_requirement_ids: BTreeSet::from(["req_report".to_string()]),
        amendment: proposed,
        config: moa_config::ExecutionConfig::default(),
    });
    assert_eq!(
        decision,
        ReplanDecision::Stop {
            reason: ReplanStopReason::DuplicateAmendment
        }
    );
}

#[test]
fn remove_only_amendment_reaches_no_progress_before_validation_rejection() {
    // Pins: the service can classify structurally invalid remove-only proposals through the
    // shared pure loop policy instead of exposing a compiler-validation error.
    let amendment = PlanAmendment {
        schema_version: 2,
        base_plan_revision: 4,
        reason: "remove failed work".to_string(),
        evidence: json!({}),
        operations: vec![PlanAmendmentOperation::RemovePendingNode {
            node_id: "failed".to_string(),
        }],
    };
    assert_eq!(
        evaluate_replan_loop_stop(ReplanLoopEvaluationRequest {
            proposed_amendment_fingerprint: amendment_operations_fingerprint(&amendment)
                .expect("fingerprint remove-only operations"),
            seen_amendment_fingerprints: BTreeSet::new(),
            failure_fingerprint_counts: BTreeMap::new(),
            current_failure: None,
            unresolved_requirement_ids: BTreeSet::from(["req_report".to_string()]),
            amendment,
            config: moa_config::ExecutionConfig::default(),
        }),
        ReplanDecision::Stop {
            reason: ReplanStopReason::NoProgress
        }
    );
}

#[test]
fn exact_terminalized_input_replay_recovers_audience_from_append_only_audit() {
    // Pins: replacing NeedsInput with a typed terminal admission failure
    // does not make the exact old-generation delivery fail audience checks.
    let needs_input = moa_artifacts::execution_plan::ExecutionTaskOutcome {
        schema_version: 1,
        usage: moa_artifacts::execution_plan::ExecutionUsage {
            cost_microusd: 0,
            tokens: 0,
            tool_calls: 0,
            retrieved_bytes: 0,
        },
        result: moa_artifacts::execution_plan::ExecutionTaskResult::NeedsInput {
            question: "continue?".to_string(),
            audience: moa_artifacts::execution_plan::InputAudience::User,
        },
    };
    let terminal = moa_artifacts::execution_plan::ExecutionTaskOutcome {
        schema_version: 1,
        usage: needs_input.usage.clone(),
        result: moa_artifacts::execution_plan::ExecutionTaskResult::Failed {
            class: moa_artifacts::execution_plan::ExecutionFailureClass::DeadlineExceeded,
            message: "deadline elapsed".to_string(),
        },
    };
    let audit = vec![json!({
        "received_generation": 1,
        "accepted": true,
        "outcome": needs_input
    })];
    assert_eq!(
        persisted_input_audience(2, Some(&terminal), &audit, 1),
        Some(moa_artifacts::execution_plan::InputAudience::User)
    );
    assert_eq!(
        persisted_input_audience(2, Some(&terminal), &audit, 0),
        None
    );
}
