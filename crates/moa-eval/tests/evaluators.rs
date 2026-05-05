//! Integration tests for built-in evaluators.

use moa_eval::{
    EvalMetrics, EvalResult, EvalScore, ExpectedOutput, OutputMatchEvaluator, ScoreValue, TestCase,
    ThresholdEvaluator, ToolSuccessEvaluator, TrajectoryMatchEvaluator, TrajectoryStep,
    score_is_failure,
};

#[tokio::test]
async fn trajectory_exact_match_scores_one() {
    let evaluator = TrajectoryMatchEvaluator;
    let case = TestCase {
        expected_trajectory: Some(vec!["bash".to_string(), "file_read".to_string()]),
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

    let scores = moa_eval::Evaluator::evaluate(&evaluator, &case, &result)
        .await
        .expect("score");
    assert_eq!(scores[0].value, ScoreValue::Numeric(1.0));
}

#[tokio::test]
async fn output_match_contains_passes() {
    let evaluator = OutputMatchEvaluator;
    let case = TestCase {
        expected_output: Some(ExpectedOutput {
            contains: vec!["deployed".to_string(), "staging".to_string()],
            ..ExpectedOutput::default()
        }),
        ..TestCase::default()
    };
    let result = EvalResult {
        response: Some("App deployed to staging successfully".to_string()),
        ..EvalResult::default()
    };

    let scores = moa_eval::Evaluator::evaluate(&evaluator, &case, &result)
        .await
        .expect("score");
    assert_eq!(scores[0].value, ScoreValue::Numeric(1.0));
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

    let scores = moa_eval::Evaluator::evaluate(&evaluator, &TestCase::default(), &result)
        .await
        .expect("score");
    assert_eq!(scores[0].value, ScoreValue::Boolean(false));
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

    let scores = moa_eval::Evaluator::evaluate(&evaluator, &TestCase::default(), &result)
        .await
        .expect("score");
    assert_eq!(scores[0].value, ScoreValue::Numeric(0.5));
}

#[test]
fn low_numeric_scores_fail_quality_gate() {
    let failure = EvalScore {
        evaluator: "test".to_string(),
        name: "score".to_string(),
        value: ScoreValue::Numeric(0.3),
        comment: None,
    };
    let success = EvalScore {
        evaluator: "test".to_string(),
        name: "score".to_string(),
        value: ScoreValue::Numeric(0.8),
        comment: None,
    };

    assert!(score_is_failure(&failure));
    assert!(!score_is_failure(&success));
}
