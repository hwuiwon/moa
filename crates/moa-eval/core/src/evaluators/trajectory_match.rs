//! Non-blocking path-similarity diagnostic.
//!
//! Longest-common-subsequence similarity between an authored tool order and the
//! observed one used to be a gate. It cannot be: two different valid paths to
//! the same correct state score arbitrarily low against each other, and a run
//! that reproduced the path exactly while leaving the world wrong scores 1.0.
//! Ordering claims now belong to the `ordered_actions` assertion, which is
//! explicit about which relative order is actually required.
//!
//! The similarity number survives only as triage: it answers "how far did this
//! run drift from the authored path", and [`GateEffect::Diagnostic`] guarantees
//! it can never fail a run on its own.

use crate::assertion::{AssertionCategory, GateEffect};
use crate::evaluators::action_assertions::ORDERED_ACTIONS_EVALUATOR_ID;
use crate::{EvalResult, EvalScore, EvalScoreValue, Evaluator, Result, TestCase};

/// Reports LCS similarity between an authored order and the observed one.
pub struct TrajectoryMatchEvaluator;

#[async_trait::async_trait]
impl Evaluator for TrajectoryMatchEvaluator {
    fn name(&self) -> &str {
        "trajectory_match"
    }

    async fn evaluate(&self, case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        let actual: Vec<&str> = result
            .trajectory
            .iter()
            .map(|step| step.tool_name.as_str())
            .collect();

        let mut scores = Vec::new();
        for spec in case
            .assertions
            .iter()
            .filter(|spec| spec.category == AssertionCategory::Action)
            .filter(|spec| spec.evaluator.id == ORDERED_ACTIONS_EVALUATOR_ID)
        {
            let Some(sequence) = spec
                .config
                .get("sequence")
                .and_then(|value| value.as_array())
                .map(|values| {
                    values
                        .iter()
                        .filter_map(|value| value.as_str())
                        .collect::<Vec<_>>()
                })
            else {
                continue;
            };

            let max_len = sequence.len().max(actual.len());
            let similarity = if max_len == 0 {
                1.0
            } else {
                lcs_len(&sequence, &actual) as f64 / max_len as f64
            };
            scores.push(EvalScore {
                evaluator: self.name().to_string(),
                name: format!("path_similarity:{}", spec.id),
                value: EvalScoreValue::Numeric(similarity),
                gate: GateEffect::Diagnostic,
                comment: Some(format!(
                    "authored order [{}], observed path [{}]",
                    sequence.join(", "),
                    actual.join(", ")
                )),
            });
        }
        Ok(scores)
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
    use crate::assertion::{AssertionCategory, AssertionSpec, EvaluatorRef, GateEffect};
    use crate::evaluators::score_is_failure;
    use crate::{EvalResult, EvalScoreValue, Evaluator, TestCase, TrajectoryStep};
    use serde_json::json;

    fn ordered_case() -> TestCase {
        TestCase {
            name: "case".to_string(),
            assertions: vec![AssertionSpec {
                id: "read-then-deploy".to_string(),
                category: AssertionCategory::Action,
                gate_effect: GateEffect::Blocking,
                evaluator: EvaluatorRef::deterministic("ordered_actions", 1),
                config: json!({ "sequence": ["bash", "file_read"] }),
            }],
            ..TestCase::default()
        }
    }

    fn step(tool_name: &str) -> TrajectoryStep {
        TrajectoryStep {
            tool_name: tool_name.to_string(),
            ..TrajectoryStep::default()
        }
    }

    #[tokio::test]
    async fn a_divergent_path_is_reported_without_failing_the_run() {
        // Pins: this is the whole point of the migration. A run that took a
        // different route scores below 1.0 and stays non-blocking.
        let result = EvalResult {
            trajectory: vec![step("bash"), step("web_search"), step("file_read")],
            ..EvalResult::default()
        };

        let scores = TrajectoryMatchEvaluator
            .evaluate(&ordered_case(), &result)
            .await
            .expect("score");

        assert_eq!(scores.len(), 1);
        match scores[0].value {
            EvalScoreValue::Numeric(value) => assert!(value > 0.0 && value < 1.0),
            ref other => panic!("unexpected score: {other:?}"),
        }
        assert!(
            !score_is_failure(&scores[0]),
            "path similarity must never gate"
        );
    }

    #[tokio::test]
    async fn a_case_without_an_ordering_assertion_scores_nothing() {
        let result = EvalResult {
            trajectory: vec![step("bash")],
            ..EvalResult::default()
        };

        let scores = TrajectoryMatchEvaluator
            .evaluate(&TestCase::default(), &result)
            .await
            .expect("score");

        assert!(
            scores.is_empty(),
            "there is no implicit trajectory expectation left to score"
        );
    }
}
