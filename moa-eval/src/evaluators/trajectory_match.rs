//! Trajectory-matching evaluator based on longest common subsequence.

use crate::{EvalResult, EvalScore, Evaluator, Result, ScoreValue, TestCase};

/// Scores how closely the actual tool-call sequence matches the expected trajectory.
pub struct TrajectoryMatchEvaluator;

#[async_trait::async_trait]
impl Evaluator for TrajectoryMatchEvaluator {
    fn name(&self) -> &str {
        "trajectory_match"
    }

    async fn evaluate(&self, case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        let Some(expected) = &case.expected_trajectory else {
            return Ok(Vec::new());
        };

        let actual: Vec<&str> = result
            .trajectory
            .iter()
            .map(|step| step.tool_name.as_str())
            .collect();
        let expected_refs: Vec<&str> = expected.iter().map(String::as_str).collect();
        let max_len = expected_refs.len().max(actual.len());
        let score = if max_len == 0 {
            1.0
        } else {
            lcs_len(&expected_refs, &actual) as f64 / max_len as f64
        };
        let comment = if (score - 1.0).abs() < f64::EPSILON {
            Some("exact match".to_string())
        } else {
            Some(format!(
                "expected [{}], actual [{}]",
                expected.join(", "),
                actual.join(", ")
            ))
        };

        Ok(vec![EvalScore {
            evaluator: self.name().to_string(),
            name: "trajectory_match".to_string(),
            value: ScoreValue::Numeric(score),
            comment,
        }])
    }
}

fn lcs_len(expected: &[&str], actual: &[&str]) -> usize {
    let mut prev = vec![0usize; actual.len() + 1];
    let mut curr = vec![0usize; actual.len() + 1];

    for expected_item in expected {
        for (index, actual_item) in actual.iter().enumerate() {
            curr[index + 1] = if expected_item == actual_item {
                prev[index] + 1
            } else {
                prev[index + 1].max(curr[index])
            };
        }
        prev.clone_from(&curr);
        curr.fill(0);
    }

    prev[actual.len()]
}

#[cfg(test)]
mod tests {
    use super::TrajectoryMatchEvaluator;
    use crate::{EvalResult, Evaluator, ScoreValue, TestCase, TrajectoryStep};

    #[tokio::test]
    async fn exact_match_scores_one() {
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

        let scores = evaluator.evaluate(&case, &result).await.expect("score");
        assert_eq!(scores[0].value, ScoreValue::Numeric(1.0));
    }

    #[tokio::test]
    async fn partial_match_scores_below_one() {
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
                    tool_name: "web_search".to_string(),
                    ..TrajectoryStep::default()
                },
                TrajectoryStep {
                    tool_name: "file_read".to_string(),
                    ..TrajectoryStep::default()
                },
            ],
            ..EvalResult::default()
        };

        let scores = evaluator.evaluate(&case, &result).await.expect("score");
        match &scores[0].value {
            ScoreValue::Numeric(score) => assert!(*score > 0.0 && *score < 1.0),
            other => panic!("unexpected score: {other:?}"),
        }
    }
}
