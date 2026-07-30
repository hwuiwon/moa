//! Tests for skill regression suite source generation and score comparison.

#![recursion_limit = "256"]

#[path = "support/common.rs"]
mod support;

use moa_core::events::Event;
use moa_eval_core::assertion::{AssertionOutcome, builtin_registry, evaluate_assertions};
use moa_eval_core::evidence::{ActionKind, ActionOutcome, EvidenceEnvelope, EvidenceSubject};
use moa_eval_core::types::TEST_CASE_SCHEMA_VERSION;
use moa_eval_core::{EvalResult, EvalStatus, TestSuite, TrajectoryStep};
use moa_skills::evidence::SanitizedLearningEvidence;
use moa_skills::format::parse_skill_markdown;
use moa_skills::regression::{
    SkillRegressionSummary, compare_scores, generate_skill_test_suite_source,
};
use support::{SESSION_WITH_8_TOOL_CALLS, experience_input, load_session_fixture, skill_markdown};

#[tokio::test]
async fn generated_suite_source_is_reviewable_without_writing_files() {
    // Pins: proposal generation can attach a regression suite as draft payload text.
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let markdown = skill_markdown(
        "suite-source-skill",
        "Generate suite source for review",
        "Follow the learned task path and verify the final response.",
        "1.0",
    );
    let skill = parse_skill_markdown(&markdown).expect("parse test skill");

    let evidence = fixture_evidence(&loaded).await;
    let generated = generate_skill_test_suite_source(loaded.session.tenant_id, &skill, &evidence)
        .expect("generate suite source");

    assert!(
        generated
            .relative_path
            .ends_with("skills/suite-source-skill/tests/suite.toml")
    );
    assert!(generated.relative_path.starts_with("tenants/"));
    assert!(
        generated
            .source_toml
            .contains("suite-source-skill-regression")
    );
    assert!(generated.source_toml.contains("[[cases]]"));
    assert!(generated.source_toml.contains("auto-generated"));
}

#[test]
fn regression_run_with_score_within_noise_band_commits_new_version_only_if_above_threshold() {
    let previous = summary(0.750, 0);
    let candidate = summary(0.755, 0);

    assert!(
        compare_scores(&previous, &candidate),
        "current regression contract accepts any non-regressing score; there is no separate noise band"
    );
}

fn summary(average_score: f64, failed_runs: usize) -> SkillRegressionSummary {
    SkillRegressionSummary {
        average_score,
        failed_runs,
        total_runs: 1,
        total_cost_dollars: 0.0,
    }
}

/// Implementation-drift guard (harness Task 5): the regression suite the skill
/// machinery generates is only ever *executed* at the proposal-review boundary
/// (orchestrator `skill_acceptance_regression_report`, which needs a live
/// provider). No standing offline lane runs an active skill's generated suite
/// against its own canonical trajectory. This test closes that gap deterministically:
/// generate the suite for a fixture skill, then RUN it through the real
/// `moa-eval` evaluators against the skill's canonical response + tool trajectory
/// and assert it passes — and that a drifted run fails.
#[tokio::test]
async fn generated_regression_suite_runs_green_against_the_skill_canonical_trajectory() {
    // Pins: a generated skill regression suite, executed offline through the eval
    // evaluators, passes for the skill's own canonical behavior and catches drift.
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let markdown = skill_markdown(
        "drift-guard-skill",
        "Run the learned OAuth-refresh workflow",
        "Follow the learned task path and verify the final response.",
        "1.0",
    );
    let skill = parse_skill_markdown(&markdown).expect("parse test skill");

    let evidence = fixture_evidence(&loaded).await;
    let generated = generate_skill_test_suite_source(loaded.session.tenant_id, &skill, &evidence)
        .expect("generate suite source");
    let suite: TestSuite =
        toml::from_str(&generated.source_toml).expect("generated suite source is valid TOML");
    let case = suite
        .cases
        .first()
        .expect("generated suite has one regression case");

    let canonical = canonical_result(case.name.clone(), &loaded.events);
    let canonical_outcomes = run_suite_case(case, &canonical);
    assert!(
        !canonical_outcomes.is_empty(),
        "the generated case must author at least one evaluated assertion"
    );
    assert!(
        !canonical_outcomes
            .iter()
            .any(AssertionOutcome::is_gate_failure),
        "the generated suite must pass against the skill's canonical trajectory, got failing \
         assertions: {:?}",
        canonical_outcomes
            .iter()
            .filter(|outcome| outcome.is_gate_failure())
            .collect::<Vec<_>>()
    );

    // Drift: a run that neither reproduces the response keywords nor the tool
    // trajectory must fail the same generated suite, proving it can catch drift.
    let drifted = EvalResult {
        test_case: case.name.clone(),
        response: Some("unrelated answer with no shared keywords".to_string()),
        trajectory: Vec::new(),
        ..EvalResult::default()
    };
    let drifted_outcomes = run_suite_case(case, &drifted);
    assert!(
        drifted_outcomes
            .iter()
            .any(AssertionOutcome::is_gate_failure),
        "the generated suite must fail against a drifted run, got: {drifted_outcomes:?}"
    );
}

/// Builds the skill's canonical eval result from its recorded session events:
/// the final response text and the ordered successful tool trajectory the
/// generated suite's expectations were derived from.
fn canonical_result(
    test_case: String,
    events: &[moa_core::types::events_stream::EventRecord],
) -> EvalResult {
    let response = events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } => Some(text.clone()),
        _ => None,
    });
    let trajectory: Vec<TrajectoryStep> = events
        .iter()
        .filter_map(|record| match &record.event {
            Event::ToolCall { tool_name, .. } => Some(TrajectoryStep {
                tool_name: tool_name.clone(),
                input_summary: String::new(),
                output_summary: String::new(),
                success: true,
                duration_ms: 1,
            }),
            _ => None,
        })
        .collect();
    // The assertions are evaluated against persisted evidence, so a canonical run
    // has to carry the response text and the ordered action ledger the generator
    // authored its claims about.
    let mut builder = EvidenceEnvelope::builder(EvidenceSubject {
        case: test_case.clone(),
        case_schema_version: TEST_CASE_SCHEMA_VERSION,
        agent_config: "canonical".to_string(),
        run_label: "canonical".to_string(),
    })
    .source("skill_regression_fixture");
    if let Some(text) = &response {
        builder = builder.response(text.clone());
    }
    for step in &trajectory {
        builder = builder.action(
            ActionKind::Invocation,
            step.tool_name.clone(),
            serde_json::Value::Null,
            ActionOutcome::Succeeded,
        );
    }

    EvalResult {
        test_case,
        status: EvalStatus::Passed,
        response,
        trajectory,
        evidence: Some(builder.build()),
        ..EvalResult::default()
    }
}

/// Evaluates the generated case's assertions, which is the path that gates promotion.
///
/// Deliberately not the legacy `OutputMatchEvaluator`/`TrajectoryMatchEvaluator`
/// pair: those now emit `GateEffect::Diagnostic` coverage and path-similarity
/// signals, so a suite scored only through them can never fail and would make this
/// a tautological test of the promotion gate.
fn run_suite_case(case: &moa_eval_core::TestCase, result: &EvalResult) -> Vec<AssertionOutcome> {
    evaluate_assertions(builtin_registry(), case, result.evidence.as_ref())
}

/// Sanitizes a loaded fixture session into the evidence the suite generator takes.
async fn fixture_evidence(loaded: &support::LoadedSession) -> SanitizedLearningEvidence {
    experience_input(loaded, "run the learned workflow")
        .await
        .evidence
}

#[tokio::test]
async fn generated_suite_carries_redaction_placeholders_not_source_identifiers() {
    // Pins: the suite TOML is a durable draft artifact that a reviewer reads and
    // the gate executes, so identifiers present in the source transcript must not
    // survive into it. The case input comes from the caller's message and the
    // expectations from the assistant response, so both carriers are checked.
    const PLANTED_USER_EMAIL: &str = "planted-user@example.com";
    const PLANTED_RESPONSE_EMAIL: &str = "planted-response@example.com";

    let mut loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let mut replaced_user = false;
    let mut replaced_response = false;
    for record in &mut loaded.events {
        match &mut record.event {
            Event::UserMessage { text, .. } if !replaced_user => {
                *text = format!("reset the login for {PLANTED_USER_EMAIL}");
                replaced_user = true;
            }
            Event::BrainResponse { text, .. } => {
                *text = format!("completed the reset and notified {PLANTED_RESPONSE_EMAIL}");
                replaced_response = true;
            }
            _ => {}
        }
    }
    assert!(
        replaced_user && replaced_response,
        "the fixture must carry a user message and an assistant response to plant into"
    );

    let evidence = fixture_evidence(&loaded).await;
    let generated = moa_skills::regression::generate_skill_test_suite_source_for_name(
        loaded.session.tenant_id,
        "redaction-suite-skill",
        &evidence,
    )
    .expect("generate suite source");

    for planted in [PLANTED_USER_EMAIL, PLANTED_RESPONSE_EMAIL] {
        assert!(
            !generated.source_toml.contains(planted),
            "{planted} survived into the generated suite: {}",
            generated.source_toml
        );
    }
    assert!(
        generated.source_toml.contains("[EMAIL_REDACTED]"),
        "the suite should carry the redaction placeholder where the identifier was: {}",
        generated.source_toml
    );
}
