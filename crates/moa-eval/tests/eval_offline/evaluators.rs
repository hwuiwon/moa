//! Integration tests for built-in evaluators.

use moa_eval_core::assertion::{AssertionCategory, AssertionSpec, EvaluatorRef, GateEffect};
use moa_eval_core::evaluators::action_assertions::ORDERED_ACTIONS_EVALUATOR_ID;
use moa_eval_core::evaluators::communication::TEXT_MATCH_EVALUATOR_ID;
use moa_eval_core::{
    EvalMetrics, EvalResult, EvalScore, EvalScoreValue, Evaluator, ExpectedOutput,
    OutputMatchEvaluator, TestCase, ThresholdEvaluator, ToolSuccessEvaluator,
    TrajectoryMatchEvaluator, TrajectoryStep, score_is_failure,
};
use serde_json::json;

/// Builds one authored assertion selecting a registered deterministic evaluator.
fn assertion(
    id: &str,
    category: AssertionCategory,
    evaluator_id: &str,
    config: serde_json::Value,
) -> AssertionSpec {
    AssertionSpec {
        id: id.to_string(),
        category,
        // Path similarity and text coverage are diagnostics, so the specs these
        // tests author say so rather than relying on the `Blocking` default.
        gate_effect: GateEffect::Diagnostic,
        evaluator: EvaluatorRef::deterministic(evaluator_id, 1),
        config,
    }
}

#[tokio::test]
async fn ordered_action_assertion_scores_an_exact_path_at_one_offline() {
    // Pins: an ordered-action assertion whose sequence matches the trajectory exactly
    // scores 1.0, and it is reached through `case.assertions` rather than a removed
    // `expected_trajectory` field.
    let evaluator = TrajectoryMatchEvaluator;
    let case = TestCase {
        assertions: vec![assertion(
            "path",
            AssertionCategory::Action,
            ORDERED_ACTIONS_EVALUATOR_ID,
            json!({ "sequence": ["bash", "file_read"] }),
        )],
        ..TestCase::default()
    };
    let result = EvalResult {
        trajectory: vec![
            TrajectoryStep {
                tool_name: "bash".to_string(),
                ..TrajectoryStep::default()
            },
            TrajectoryStep {
                tool_name: "file_read".to_string(),
                ..TrajectoryStep::default()
            },
        ],
        ..EvalResult::default()
    };

    let scores = Evaluator::evaluate(&evaluator, &case, &result)
        .await
        .expect("score");
    assert_eq!(scores[0].value, EvalScoreValue::Numeric(1.0));
    assert_eq!(
        scores[0].gate,
        GateEffect::Diagnostic,
        "path similarity must never gate a run on its own"
    );
}

#[tokio::test]
async fn text_match_assertion_covers_every_required_fragment_offline() {
    // Pins: a communication assertion carrying an `ExpectedOutput` config reports full
    // coverage when the response contains every required fragment.
    let evaluator = OutputMatchEvaluator;
    let expected = ExpectedOutput {
        contains: vec!["deployed".to_string(), "staging".to_string()],
        ..ExpectedOutput::default()
    };
    let case = TestCase {
        assertions: vec![assertion(
            "text",
            AssertionCategory::Communication,
            TEXT_MATCH_EVALUATOR_ID,
            serde_json::to_value(&expected).expect("expected output serializes"),
        )],
        ..TestCase::default()
    };
    let result = EvalResult {
        response: Some("App deployed to staging successfully".to_string()),
        ..EvalResult::default()
    };

    let scores = Evaluator::evaluate(&evaluator, &case, &result)
        .await
        .expect("score");
    assert_eq!(scores[0].value, EvalScoreValue::Numeric(1.0));
}

#[tokio::test]
async fn threshold_cost_over_budget_fails() {
    let evaluator = ThresholdEvaluator {
        max_cost_dollars: Some(0.01),
        ..ThresholdEvaluator::default()
    };
    let result = EvalResult {
        metrics: EvalMetrics {
            cost_dollars: 0.05,
            ..EvalMetrics::default()
        },
        ..EvalResult::default()
    };

    let scores = Evaluator::evaluate(&evaluator, &TestCase::default(), &result)
        .await
        .expect("score");
    assert_eq!(scores[0].value, EvalScoreValue::Boolean(false));
}

#[tokio::test]
async fn tool_success_reports_rate() {
    let evaluator = ToolSuccessEvaluator;
    let result = EvalResult {
        trajectory: vec![
            TrajectoryStep {
                tool_name: "bash".to_string(),
                success: true,
                ..TrajectoryStep::default()
            },
            TrajectoryStep {
                tool_name: "file_read".to_string(),
                success: false,
                ..TrajectoryStep::default()
            },
        ],
        ..EvalResult::default()
    };

    let scores = Evaluator::evaluate(&evaluator, &TestCase::default(), &result)
        .await
        .expect("score");
    assert_eq!(scores[0].value, EvalScoreValue::Numeric(0.5));
}

#[test]
fn low_numeric_scores_fail_quality_gate() {
    let failure = EvalScore {
        evaluator: "test".to_string(),
        name: "score".to_string(),
        value: EvalScoreValue::Numeric(0.3),
        gate: GateEffect::Blocking,
        comment: None,
    };
    let success = EvalScore {
        evaluator: "test".to_string(),
        name: "score".to_string(),
        value: EvalScoreValue::Numeric(0.8),
        gate: GateEffect::Blocking,
        comment: None,
    };

    assert!(score_is_failure(&failure));
    assert!(!score_is_failure(&success));
}
