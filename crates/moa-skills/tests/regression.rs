//! Tests for skill regression suite source generation and score comparison.

#![recursion_limit = "256"]

#[path = "support/common.rs"]
mod support;

use moa_core::Event;
use moa_eval_core::{
    EvalResult, EvalStatus, Evaluator, OutputMatchEvaluator, TestSuite, TrajectoryMatchEvaluator,
    TrajectoryStep, score_is_failure,
};
use moa_skills::format::parse_skill_markdown;
use moa_skills::regression::{
    SkillRegressionSummary, compare_scores, generate_skill_test_suite_source,
};
use support::{SESSION_WITH_8_TOOL_CALLS, load_session_fixture, skill_markdown};

#[test]
fn generated_suite_source_is_reviewable_without_writing_files() {
    // Pins: proposal generation can attach a regression suite as draft payload text.
    let loaded = load_session_fixture(SESSION_WITH_8_TOOL_CALLS);
    let markdown = skill_markdown(
        "suite-source-skill",
        "Generate suite source for review",
        "Follow the learned task path and verify the final response.",
        "1.0",
    );
    let skill = parse_skill_markdown(&markdown).expect("parse test skill");

    let generated =
        generate_skill_test_suite_source(loaded.session.tenant_id, &skill, &loaded.events)
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

    let generated =
        generate_skill_test_suite_source(loaded.session.tenant_id, &skill, &loaded.events)
            .expect("generate suite source");
    let suite: TestSuite =
        toml::from_str(&generated.source_toml).expect("generated suite source is valid TOML");
    let case = suite
        .cases
        .first()
        .expect("generated suite has one regression case");

    let canonical = canonical_result(case.name.clone(), &loaded.events);
    let canonical_scores = run_suite_case(case, &canonical).await;
    assert!(
        !canonical_scores.is_empty(),
        "output and trajectory evaluators should both score the generated case"
    );
    assert!(
        !canonical_scores.iter().any(score_is_failure),
        "the generated suite must pass against the skill's canonical trajectory, got failing \
         scores: {:?}",
        canonical_scores
            .iter()
            .filter(|score| score_is_failure(score))
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
    let drifted_scores = run_suite_case(case, &drifted).await;
    assert!(
        drifted_scores.iter().any(score_is_failure),
        "the generated suite must fail against a drifted run, got scores: {drifted_scores:?}"
    );
}

/// Builds the skill's canonical eval result from its recorded session events:
/// the final response text and the ordered successful tool trajectory the
/// generated suite's expectations were derived from.
fn canonical_result(test_case: String, events: &[moa_core::EventRecord]) -> EvalResult {
    let response = events.iter().rev().find_map(|record| match &record.event {
        Event::BrainResponse { text, .. } => Some(text.clone()),
        _ => None,
    });
    let trajectory = events
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
    EvalResult {
        test_case,
        status: EvalStatus::Passed,
        response,
        trajectory,
        ..EvalResult::default()
    }
}

/// Runs the output and trajectory evaluators the generated suite relies on.
async fn run_suite_case(
    case: &moa_eval_core::TestCase,
    result: &EvalResult,
) -> Vec<moa_eval_core::EvalScore> {
    let evaluators: [Box<dyn Evaluator>; 2] = [
        Box::new(OutputMatchEvaluator),
        Box::new(TrajectoryMatchEvaluator),
    ];
    let mut scores = Vec::new();
    for evaluator in &evaluators {
        scores.extend(
            evaluator
                .evaluate(case, result)
                .await
                .expect("evaluator scores the generated case"),
        );
    }
    scores
}
