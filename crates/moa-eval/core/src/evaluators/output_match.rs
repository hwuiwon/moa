//! Diagnostic response-coverage reporter over a case's text assertions.
//!
//! The hard text gate now lives in the `text_match` assertion evaluator, which
//! reads captured evidence and is all-of. This run evaluator exists only to
//! report *how much* of the authored text a response covered, which is useful
//! when triaging a near-miss and is explicitly non-blocking so a partial match
//! can never certify anything.

use crate::assertion::{AssertionCategory, GateEffect};
use crate::evaluators::communication::{TEXT_MATCH_EVALUATOR_ID, match_text};
use crate::types::ExpectedOutput;
use crate::{EvalResult, EvalScore, EvalScoreValue, Evaluator, Result, TestCase};

/// Reports the fraction of a case's authored text rules the response covered.
pub struct OutputMatchEvaluator;

#[async_trait::async_trait]
impl Evaluator for OutputMatchEvaluator {
    fn name(&self) -> &str {
        "output_match"
    }

    async fn evaluate(&self, case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        let response = result.response.as_deref().unwrap_or("");
        let mut scores = Vec::new();
        for spec in case
            .assertions
            .iter()
            .filter(|spec| spec.category == AssertionCategory::Communication)
            .filter(|spec| spec.evaluator.id == TEXT_MATCH_EVALUATOR_ID)
        {
            let Ok(expected) = serde_json::from_value::<ExpectedOutput>(spec.config.clone()) else {
                continue;
            };
            let outcome = match_text(response, &expected)?;
            scores.push(EvalScore {
                evaluator: self.name().to_string(),
                name: format!("output_coverage:{}", spec.id),
                value: EvalScoreValue::Numeric(outcome.fraction()),
                gate: GateEffect::Diagnostic,
                comment: if outcome.failures.is_empty() {
                    None
                } else {
                    Some(outcome.failures.join("; "))
                },
            });
        }
        Ok(scores)
    }
}

#[cfg(test)]
mod tests {
    use super::OutputMatchEvaluator;
    use crate::assertion::{AssertionCategory, AssertionSpec, EvaluatorRef, GateEffect};
    use crate::evaluators::score_is_failure;
    use crate::{EvalResult, EvalScoreValue, Evaluator, TestCase};
    use serde_json::json;

    fn case_with_text_assertion() -> TestCase {
        TestCase {
            name: "case".to_string(),
            assertions: vec![AssertionSpec {
                id: "response-mentions-both".to_string(),
                category: AssertionCategory::Communication,
                gate_effect: GateEffect::Blocking,
                evaluator: EvaluatorRef::deterministic("text_match", 1),
                config: json!({ "contains": ["deployed", "production"] }),
            }],
            ..TestCase::default()
        }
    }

    #[tokio::test]
    async fn partial_coverage_is_reported_but_never_gates() {
        // Pins: the coverage number is triage. A 0.5 coverage on a case whose
        // text assertion failed must not be what decides the run — the assertion
        // outcome already did.
        let result = EvalResult {
            response: Some("App deployed to staging".to_string()),
            ..EvalResult::default()
        };

        let scores = OutputMatchEvaluator
            .evaluate(&case_with_text_assertion(), &result)
            .await
            .expect("score");

        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].value, EvalScoreValue::Numeric(0.5));
        assert!(
            !score_is_failure(&scores[0]),
            "a diagnostic coverage score must never downgrade a run"
        );
    }

    #[tokio::test]
    async fn a_case_without_text_assertions_scores_nothing() {
        let scores = OutputMatchEvaluator
            .evaluate(&TestCase::default(), &EvalResult::default())
            .await
            .expect("score");

        assert!(scores.is_empty());
    }
}
