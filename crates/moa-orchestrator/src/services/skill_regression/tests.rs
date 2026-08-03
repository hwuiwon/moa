//! Unit tests for skill-regression suite pooling, scoring, and template compilation.

use moa_artifacts::execution_plan::ExecutionPlanTemplate;
use moa_config::MoaConfig;
use moa_core::types::{
    execution_planning::{
        ExecutionAuditReport, ExecutionCompileOutcome, ExecutionCompileSource,
        ExecutionPlanningAuditPayload,
    },
    identifiers::TenantId,
};
use moa_eval_core::{EvalResult, EvalScore, EvalScoreValue, EvalStatus};
use moa_execution::{ExecutionAuthorizationEnvelope, ExecutionCapabilityCatalog};
use moa_hands::ToolRegistry;
use serde_json::json;

use crate::services::execution::capability_catalog::build_capability_response;

use moa_artifacts::registry::{StoredSuiteContribution, SuiteContributionKind};

use super::{
    compilation::{
        SkillTemplateCompile, SkillTemplateCompileRequest, compile_skill_execution_template,
    },
    runner::result_score,
    suite::{
        RegressionExecutionInput, collect_held_out_pool, generated_suite_contribution,
        resolve_regression_execution_input,
    },
};

fn contribution(
    kind: SuiteContributionKind,
    suite_name: &str,
    suite_source: &str,
) -> StoredSuiteContribution {
    StoredSuiteContribution {
        kind,
        suite_name: suite_name.to_string(),
        suite_source: suite_source.to_string(),
        source_session_id: Some(uuid::Uuid::now_v7()),
        source_experience_id: Some(uuid::Uuid::now_v7()),
    }
}

#[test]
fn diagnostic_scores_cannot_change_skill_regression_acceptance_score() {
    // Pins: reporting-only scores never raise or lower the score used for
    // skill acceptance. With no numeric/boolean blocking score, terminal
    // run status remains the source of truth.
    let passed_with_diagnostic_failure = EvalResult {
        status: EvalStatus::Passed,
        scores: vec![EvalScore::diagnostic(
            "ordered_actions",
            "path_similarity",
            EvalScoreValue::Numeric(0.0),
            None,
        )],
        ..EvalResult::default()
    };
    let failed_with_diagnostic_success = EvalResult {
        status: EvalStatus::Failed,
        scores: vec![EvalScore::diagnostic(
            "ordered_actions",
            "path_similarity",
            EvalScoreValue::Boolean(true),
            None,
        )],
        ..EvalResult::default()
    };
    let blocking_success_with_diagnostic_failure = EvalResult {
        status: EvalStatus::Passed,
        scores: vec![
            EvalScore::gating(
                "required_actions",
                "recorded-tools-were-used",
                EvalScoreValue::Boolean(true),
                None,
            ),
            EvalScore::diagnostic(
                "ordered_actions",
                "recorded-tool-order",
                EvalScoreValue::Boolean(false),
                None,
            ),
        ],
        ..EvalResult::default()
    };

    assert_eq!(result_score(&passed_with_diagnostic_failure), 1.0);
    assert_eq!(result_score(&failed_with_diagnostic_success), 0.0);
    assert_eq!(result_score(&blocking_success_with_diagnostic_failure), 1.0);
}

#[test]
fn held_out_pool_merges_sibling_suites_with_prefixed_case_names() {
    // Pins: accumulated sibling suites merge into one pool suite with source-prefixed
    // case names, and unreadable entries are skipped with a recorded reason instead
    // of rejecting the candidate.
    let contributions = [
        contribution(
            SuiteContributionKind::Accumulated,
            "sibling/a",
            "[suite]\nname = \"s0\"\ndefault_timeout_seconds = 90\n\n[[cases]]\nname = \"smoke\"\ninput = \"run\"\n",
        ),
        contribution(
            SuiteContributionKind::Accumulated,
            "sibling/b",
            "this is [not toml",
        ),
    ];

    let pool = collect_held_out_pool(None, &contributions);

    assert_eq!(pool.source_count, 1);
    assert_eq!(
        pool.skipped.len(),
        1,
        "unreadable sibling is recorded, not fatal"
    );
    let suite = pool.suite.expect("readable sibling contributes cases");
    assert_eq!(suite.cases.len(), 1);
    assert_eq!(suite.cases[0].name, "sib0-smoke");
}

#[test]
fn held_out_pool_excludes_the_candidates_own_generated_suite() {
    // Pins: the pool is HELD-OUT material. The candidate's own generated suite lives
    // in the same contribution table as its siblings, so a collector that took every
    // row would grade the draft on the very cases it was derived from and report a
    // passing held-out split that never held anything out.
    let contributions = [
        contribution(
            SuiteContributionKind::Generated,
            "tests/regression-suite.toml",
            "[suite]\nname = \"own\"\ndefault_timeout_seconds = 90\n\n[[cases]]\nname = \"own\"\ninput = \"run\"\n",
        ),
        contribution(
            SuiteContributionKind::Accumulated,
            "sibling/a",
            "[suite]\nname = \"s0\"\ndefault_timeout_seconds = 90\n\n[[cases]]\nname = \"smoke\"\ninput = \"run\"\n",
        ),
    ];

    let pool = collect_held_out_pool(None, &contributions);

    assert_eq!(
        pool.source_count, 1,
        "only the sibling is held-out material"
    );
    let suite = pool.suite.expect("sibling contributes cases");
    assert_eq!(suite.cases.len(), 1);
    assert_eq!(suite.cases[0].name, "sib0-smoke");
}

#[test]
fn the_generated_suite_is_selected_by_kind_not_by_position() {
    // Pins: the candidate's own suite is identified by its stored kind. Rows come back
    // ordered by `suite_kind` first, so a positional read would silently pick an
    // accumulated sibling as the candidate's own suite whenever the generated row was
    // erased — grading a draft against another session's cases.
    let contributions = [
        contribution(SuiteContributionKind::Accumulated, "sibling/a", "x"),
        contribution(
            SuiteContributionKind::Generated,
            "tests/regression-suite.toml",
            "y",
        ),
    ];

    let generated = generated_suite_contribution(&contributions).expect("generated suite is found");
    assert_eq!(generated.suite_name, "tests/regression-suite.toml");
    assert!(generated_suite_contribution(&contributions[..1]).is_none());
}

#[test]
fn held_out_pool_is_empty_without_material() {
    // Pins: a first revision of a novel task has no held-out material and the report
    // base says so instead of implying a split ran.
    let pool = collect_held_out_pool(None, &[]);

    assert_eq!(pool.source_count, 0);
    assert!(pool.suite.is_none());
    assert_eq!(pool.report_base()["decision"], "no_material");
}

#[test]
fn regression_template_input_comes_only_from_explicit_structured_case_metadata() {
    // Pins: free-form case input is never parsed as template JSON; one explicit
    // structured metadata value is preserved as compiler input, while no value stays missing.
    let exact = json!({"ticket": "INC-42", "options": {"notify": true}});
    let suite = moa_eval_core::TestSuite {
        cases: vec![
            moa_eval_core::TestCase {
                input: r#"{"ticket":"fabricated-from-prose"}"#.to_string(),
                ..moa_eval_core::TestCase::default()
            },
            moa_eval_core::TestCase {
                metadata: std::collections::HashMap::from([(
                    "execution_input".to_string(),
                    exact.clone(),
                )]),
                ..moa_eval_core::TestCase::default()
            },
        ],
        ..moa_eval_core::TestSuite::default()
    };

    assert_eq!(
        resolve_regression_execution_input(&suite).expect("one structured input should resolve"),
        RegressionExecutionInput::Resolved(exact)
    );
    assert_eq!(
        resolve_regression_execution_input(&moa_eval_core::TestSuite {
            cases: vec![moa_eval_core::TestCase {
                input: r#"{"ticket":"still-prose"}"#.to_string(),
                ..moa_eval_core::TestCase::default()
            }],
            ..moa_eval_core::TestSuite::default()
        })
        .expect("free-form input is ignored"),
        RegressionExecutionInput::Missing
    );
}

#[test]
fn canonical_identical_regression_execution_inputs_resolve_once() {
    // Pins: semantically identical explicit objects deduplicate through artifact
    // canonical JSON even when their source key insertion order differs.
    let first: serde_json::Value =
        serde_json::from_str(r#"{"ticket":"INC-42","options":{"notify":true,"limit":2}}"#)
            .expect("first structured input parses");
    let reordered: serde_json::Value =
        serde_json::from_str(r#"{"options":{"limit":2,"notify":true},"ticket":"INC-42"}"#)
            .expect("reordered structured input parses");
    let suite = moa_eval_core::TestSuite {
        cases: vec![
            moa_eval_core::TestCase {
                metadata: std::collections::HashMap::from([(
                    "execution_input".to_string(),
                    first.clone(),
                )]),
                ..moa_eval_core::TestCase::default()
            },
            moa_eval_core::TestCase {
                metadata: std::collections::HashMap::from([(
                    "execution_input".to_string(),
                    reordered,
                )]),
                ..moa_eval_core::TestCase::default()
            },
        ],
        ..moa_eval_core::TestSuite::default()
    };

    assert_eq!(
        resolve_regression_execution_input(&suite)
            .expect("canonical-identical inputs should resolve"),
        RegressionExecutionInput::Resolved(first)
    );
}

#[test]
fn distinct_regression_execution_inputs_need_input_and_cannot_be_accepted() {
    // Pins: a suite with conflicting explicit template inputs is ambiguous and must
    // produce a typed NeedsInput audit instead of silently compiling the first case.
    let suite = moa_eval_core::TestSuite {
        cases: vec![
            moa_eval_core::TestCase {
                metadata: std::collections::HashMap::from([(
                    "execution_input".to_string(),
                    json!({"ticket": "INC-42"}),
                )]),
                ..moa_eval_core::TestCase::default()
            },
            moa_eval_core::TestCase {
                metadata: std::collections::HashMap::from([(
                    "execution_input".to_string(),
                    json!({"ticket": "INC-43"}),
                )]),
                ..moa_eval_core::TestCase::default()
            },
        ],
        ..moa_eval_core::TestSuite::default()
    };
    let mut template = output_only_template();
    template.plan.input_schema = json!({
        "type": "object",
        "required": ["ticket"],
        "properties": {"ticket": {"type": "string"}},
        "additionalProperties": false
    });
    let skill_input_schema = template.plan.input_schema.clone();
    let run_input = resolve_regression_execution_input(&suite)
        .expect("conflicting structured inputs produce a typed resolution");
    assert_eq!(run_input, RegressionExecutionInput::Ambiguous);
    let (catalog, authorization) = empty_authority("input-template");
    let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
        config: &MoaConfig::default(),
        tenant_id: TenantId::new(),
        skill_name: "input-template",
        skill_input_schema: &skill_input_schema,
        template: &template,
        run_input: &run_input,
        catalog: &catalog,
        authorization: &authorization,
        operation_key: &format!(
            "skill_regression:{}:{}",
            uuid::Uuid::now_v7(),
            "f".repeat(64)
        ),
    })
    .expect("input ambiguity is captured in a strict compile audit");

    assert!(!compiled.accepted, "ambiguous input must never be accepted");
    assert_eq!(
        compile_outcome_and_candidate_hash(&compiled).0,
        ExecutionCompileOutcome::NeedsInput
    );
    let ExecutionPlanningAuditPayload::Compile {
        validation_report,
        final_plan_hash,
        ..
    } = &compiled.audit.payload
    else {
        panic!("input ambiguity must emit a compile audit");
    };
    assert_eq!(final_plan_hash, &None);
    let ExecutionAuditReport::Compiler { violations, .. } =
        serde_json::from_str(validation_report).expect("strict compiler report parses")
    else {
        panic!("input ambiguity must emit a compiler report");
    };
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].code, "ambiguous_run_input");
    assert_eq!(violations[0].path, "run_input");
}

fn output_only_template() -> ExecutionPlanTemplate {
    serde_json::from_value(json!({
        "goal": {
            "requirements": [{
                "id": "regression_result",
                "description": "Return the deterministic regression result."
            }],
            "deliverables": [],
            "coverage": [],
            "constraints": [],
            "completion_checks": [{
                "id": "output_schema",
                "description": "Validate the regression output.",
                "requirement_ids": ["regression_result"],
                "constraint_ids": [],
                "kind": {"kind": "output_schema"}
            }]
        },
        "plan": {
            "schema_version": 1,
            "input_schema": {
                "type": "object",
                "additionalProperties": false
            },
            "output_schema": {"type": "object"},
            "nodes": [{
                "id": "result",
                "requirement_ids": ["regression_result"],
                "depends_on": [],
                "when": null,
                "input": {},
                "output_schema": {"type": "object"},
                "operation": {
                    "kind": "output",
                    "value": {"status": "validated"}
                },
                "retry": {
                    "max_attempts": 1,
                    "initial_backoff_ms": 0,
                    "max_backoff_ms": 0
                },
                "budget": null
            }]
        }
    }))
    .expect("execution-plan template parses")
}

fn empty_authority(
    skill_name: &str,
) -> (ExecutionCapabilityCatalog, ExecutionAuthorizationEnvelope) {
    let catalog =
        ExecutionCapabilityCatalog::build(Vec::new()).expect("empty governed catalog should build");
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: vec![moa_artifacts::reference::ArtifactRef::artifact(
            moa_artifacts::document::ArtifactKind::Skill,
            skill_name,
        )],
    };
    (catalog, authorization)
}

fn compile_outcome_and_candidate_hash(
    compiled: &SkillTemplateCompile,
) -> (ExecutionCompileOutcome, String) {
    let ExecutionPlanningAuditPayload::Compile {
        outcome,
        candidate_hash,
        ..
    } = &compiled.audit.payload
    else {
        panic!("template compilation must emit a compile audit");
    };
    (*outcome, candidate_hash.clone())
}

fn governed_capability_template(
    reference: &moa_artifacts::execution_plan::CapabilityReference,
    output_schema: &serde_json::Value,
) -> ExecutionPlanTemplate {
    serde_json::from_value(json!({
        "goal": {
            "requirements": [{
                "id": "regression_result",
                "description": "Read the governed regression fixture."
            }],
            "deliverables": [],
            "coverage": [],
            "constraints": [],
            "completion_checks": [{
                "id": "output_schema",
                "description": "Validate the regression output.",
                "requirement_ids": ["regression_result"],
                "constraint_ids": [],
                "kind": {"kind": "output_schema"}
            }]
        },
        "plan": {
            "schema_version": 1,
            "input_schema": {
                "type": "object",
                "additionalProperties": false
            },
            "output_schema": output_schema,
            "nodes": [
                {
                    "id": "read_fixture",
                    "requirement_ids": ["regression_result"],
                    "depends_on": [],
                    "when": null,
                    "input": {"path": "SKILL.md"},
                    "output_schema": output_schema,
                    "operation": {
                        "kind": "capability",
                        "reference": reference
                    },
                    "retry": {
                        "max_attempts": 1,
                        "initial_backoff_ms": 0,
                        "max_backoff_ms": 0
                    },
                    "budget": null
                },
                {
                    "id": "result",
                    "requirement_ids": ["regression_result"],
                    "depends_on": ["read_fixture"],
                    "when": null,
                    "input": {},
                    "output_schema": output_schema,
                    "operation": {
                        "kind": "output",
                        "value": {"$ref": "$.nodes.read_fixture.output"}
                    },
                    "retry": {
                        "max_attempts": 1,
                        "initial_backoff_ms": 0,
                        "max_backoff_ms": 0
                    },
                    "budget": null
                }
            ]
        }
    }))
    .expect("governed capability template parses")
}

#[test]
fn execution_template_uses_the_execution_compiler_and_emits_strict_audit() {
    // Pins: a template-bearing draft is validated by the shared execution compiler and
    // produces the sessionless skill-regression audit consumed by the learning append.
    let config = MoaConfig::default();
    let tenant_id = TenantId::new();
    let operation_key = format!(
        "skill_regression:{}:{}",
        uuid::Uuid::now_v7(),
        "a".repeat(64)
    );
    let run_input = json!({});
    let run_input = RegressionExecutionInput::Resolved(run_input);
    let (catalog, authorization) = empty_authority("regression-template");
    let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
        config: &config,
        tenant_id,
        skill_name: "regression-template",
        skill_input_schema: &json!({
            "type": "object",
            "additionalProperties": false
        }),
        template: &output_only_template(),
        run_input: &run_input,
        catalog: &catalog,
        authorization: &authorization,
        operation_key: &operation_key,
    })
    .expect("valid output-only template compiles");

    assert!(compiled.accepted);
    assert_eq!(compiled.audit.tenant_id, tenant_id);
    assert_eq!(compiled.audit.contact_id, None);
    assert_eq!(compiled.audit.session_id, None);
    assert_eq!(compiled.audit.originating_sequence, None);
    let ExecutionPlanningAuditPayload::Compile {
        source,
        operation_key: persisted_key,
        outcome,
        candidate_hash,
        final_plan_hash,
        validation_report,
        ..
    } = compiled.audit.payload
    else {
        panic!("template compilation must emit a compile audit");
    };
    assert_eq!(source, ExecutionCompileSource::SkillRegression);
    assert_eq!(persisted_key, operation_key);
    assert_eq!(outcome, ExecutionCompileOutcome::Accepted);
    assert_eq!(candidate_hash.len(), 64);
    assert!(final_plan_hash.is_some());
    assert!(matches!(
        serde_json::from_str::<ExecutionAuditReport>(&validation_report)
            .expect("compiler report stays strict"),
        ExecutionAuditReport::Compiler { .. }
    ));
}

#[test]
fn execution_template_compiles_with_exact_supplied_structured_input() {
    // Pins: the regression compiler consumes the explicit structured input instead of
    // fabricating an empty object; changing only that input changes the audited candidate.
    let mut template = output_only_template();
    template.plan.input_schema = json!({
        "type": "object",
        "required": ["ticket"],
        "properties": {"ticket": {"type": "string"}},
        "additionalProperties": false
    });
    let skill_input_schema = json!({
        "type": "object",
        "required": ["ticket"],
        "properties": {"ticket": {"type": "string"}},
        "additionalProperties": false
    });
    let first_input = json!({"ticket": "INC-42"});
    let second_input = json!({"ticket": "INC-43"});
    let (catalog, authorization) = empty_authority("input-template");
    let compile_with_input = |run_input: &serde_json::Value| {
        let run_input = RegressionExecutionInput::Resolved(run_input.clone());
        compile_skill_execution_template(SkillTemplateCompileRequest {
            config: &MoaConfig::default(),
            tenant_id: TenantId::new(),
            skill_name: "input-template",
            skill_input_schema: &skill_input_schema,
            template: &template,
            run_input: &run_input,
            catalog: &catalog,
            authorization: &authorization,
            operation_key: &format!(
                "skill_regression:{}:{}",
                uuid::Uuid::now_v7(),
                "b".repeat(64)
            ),
        })
        .expect("valid structured regression input compiles")
    };

    let first = compile_with_input(&first_input);
    let second = compile_with_input(&second_input);
    let (first_outcome, first_hash) = compile_outcome_and_candidate_hash(&first);
    let (second_outcome, second_hash) = compile_outcome_and_candidate_hash(&second);

    assert!(first.accepted);
    assert!(second.accepted);
    assert_eq!(first_outcome, ExecutionCompileOutcome::Accepted);
    assert_eq!(second_outcome, ExecutionCompileOutcome::Accepted);
    assert_ne!(
        first_hash, second_hash,
        "candidate hash must bind exact input"
    );
}

#[test]
fn execution_template_with_missing_or_invalid_structured_input_needs_input() {
    // Pins: absence and schema-invalid structured input remain typed input failures.
    let mut template = output_only_template();
    template.plan.input_schema = json!({
        "type": "object",
        "required": ["ticket"],
        "properties": {"ticket": {"type": "string"}},
        "additionalProperties": false
    });
    let skill_input_schema = template.plan.input_schema.clone();
    let invalid_input = json!({"ticket": 42});
    let (catalog, authorization) = empty_authority("input-template");
    for run_input in [
        RegressionExecutionInput::Missing,
        RegressionExecutionInput::Resolved(invalid_input.clone()),
    ] {
        let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
            config: &MoaConfig::default(),
            tenant_id: TenantId::new(),
            skill_name: "input-template",
            skill_input_schema: &skill_input_schema,
            template: &template,
            run_input: &run_input,
            catalog: &catalog,
            authorization: &authorization,
            operation_key: &format!(
                "skill_regression:{}:{}",
                uuid::Uuid::now_v7(),
                "c".repeat(64)
            ),
        })
        .expect("typed compiler rejection is still audited");

        assert!(!compiled.accepted);
        assert_eq!(
            compile_outcome_and_candidate_hash(&compiled).0,
            ExecutionCompileOutcome::NeedsInput
        );
    }
}

#[test]
fn execution_template_compiles_with_real_governed_capability_authority() {
    // Pins: skill regression uses the same live governed catalog builder and exact
    // capability authorization as production execution planning.
    let registry = ToolRegistry::default_local();
    let response = build_capability_response(&registry.capability_registrations(), &[], &[])
        .expect("production capability catalog should build");
    let capability = response
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference.name == "file_read")
        .expect("default governed file_read capability exists");
    let template = governed_capability_template(&capability.reference, &capability.output_schema);
    let run_input = json!({});
    let run_input = RegressionExecutionInput::Resolved(run_input);
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: response
            .catalog
            .capabilities
            .iter()
            .map(|entry| entry.reference.clone())
            .collect(),
        skill_refs: vec![moa_artifacts::reference::ArtifactRef::artifact(
            moa_artifacts::document::ArtifactKind::Skill,
            "governed-template",
        )],
    };

    let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
        config: &MoaConfig::default(),
        tenant_id: TenantId::new(),
        skill_name: "governed-template",
        skill_input_schema: &json!({"type": "object"}),
        template: &template,
        run_input: &run_input,
        catalog: &response.catalog,
        authorization: &authorization,
        operation_key: &format!(
            "skill_regression:{}:{}",
            uuid::Uuid::now_v7(),
            "d".repeat(64)
        ),
    })
    .expect("authorized governed capability compiles");

    assert!(compiled.accepted);
    assert_eq!(
        compile_outcome_and_candidate_hash(&compiled).0,
        ExecutionCompileOutcome::Accepted
    );
}

#[test]
fn execution_template_rejects_unauthorized_governed_capability_as_unsupported() {
    // Pins: catalog presence alone never authorizes a governed capability.
    let registry = ToolRegistry::default_local();
    let response = build_capability_response(&registry.capability_registrations(), &[], &[])
        .expect("production capability catalog should build");
    let capability = response
        .catalog
        .capabilities
        .iter()
        .find(|capability| capability.reference.name == "file_read")
        .expect("default governed file_read capability exists");
    let template = governed_capability_template(&capability.reference, &capability.output_schema);
    let run_input = json!({});
    let run_input = RegressionExecutionInput::Resolved(run_input);
    let authorization = ExecutionAuthorizationEnvelope {
        capability_refs: Vec::new(),
        skill_refs: vec![moa_artifacts::reference::ArtifactRef::artifact(
            moa_artifacts::document::ArtifactKind::Skill,
            "governed-template",
        )],
    };
    let compiled = compile_skill_execution_template(SkillTemplateCompileRequest {
        config: &MoaConfig::default(),
        tenant_id: TenantId::new(),
        skill_name: "governed-template",
        skill_input_schema: &json!({"type": "object"}),
        template: &template,
        run_input: &run_input,
        catalog: &response.catalog,
        authorization: &authorization,
        operation_key: &format!(
            "skill_regression:{}:{}",
            uuid::Uuid::now_v7(),
            "e".repeat(64)
        ),
    })
    .expect("unauthorized compiler outcome is still audited");

    assert!(!compiled.accepted);
    assert_eq!(
        compile_outcome_and_candidate_hash(&compiled).0,
        ExecutionCompileOutcome::Unsupported
    );
}
