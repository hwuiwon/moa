//! Output-content evaluator covering containment, exclusion, regex, and exact matching.

use regex::Regex;

use crate::{EvalResult, EvalScore, EvalScoreValue, Evaluator, ExpectedOutput, Result, TestCase};

/// Scores how well the final response matches the test case output expectations.
pub struct OutputMatchEvaluator;

#[async_trait::async_trait]
impl Evaluator for OutputMatchEvaluator {
    fn name(&self) -> &str {
        "output_match"
    }

    async fn evaluate(&self, case: &TestCase, result: &EvalResult) -> Result<Vec<EvalScore>> {
        let Some(expected) = &case.expected_output else {
            return Ok(Vec::new());
        };

        let response = result.response.as_deref().unwrap_or("");
        let outcome = evaluate_output(response, expected)?;
        // Diagnostic fractional score: how many expectation rules matched. This is
        // reporting only and must NOT be used as the pass/fail gate on its own,
        // because a partial match above the global 0.5 threshold would otherwise
        // certify a response that is missing a required fragment.
        let mut scores = vec![EvalScore {
            evaluator: self.name().to_string(),
            name: "output_match".to_string(),
            value: EvalScoreValue::Numeric(outcome.fraction),
            comment: outcome.comment.clone(),
        }];
        // Hard all-of gate: every expectation rule (contains/not_contains/facts/
        // regex/exact) is required, so any missing required fragment or present
        // exclusion fails the scenario regardless of the averaged fraction.
        if outcome.rule_count > 0 {
            scores.push(EvalScore {
                evaluator: self.name().to_string(),
                name: "output_match_required".to_string(),
                value: EvalScoreValue::Boolean(outcome.required_satisfied),
                comment: if outcome.required_satisfied {
                    None
                } else {
                    outcome.comment
                },
            });
        }
        Ok(scores)
    }
}

/// Outcome of scoring a response against its expectation rules.
struct OutputMatchOutcome {
    /// Fraction of all expectation rules satisfied. Diagnostic reporting only.
    fraction: f64,
    /// Whether every required rule is satisfied. This is the hard pass/fail gate.
    required_satisfied: bool,
    /// Number of expectation rules evaluated.
    rule_count: usize,
    /// Human-readable description of every unmet rule, when any failed.
    comment: Option<String>,
}

fn evaluate_output(response: &str, expected: &ExpectedOutput) -> Result<OutputMatchOutcome> {
    let response_lower = response.to_lowercase();
    let mut matched = 0usize;
    let mut total = 0usize;
    let mut failures = Vec::new();

    for phrase in &expected.contains {
        total += 1;
        if response_lower.contains(&phrase.to_lowercase()) {
            matched += 1;
        } else {
            failures.push(format!("missing '{phrase}'"));
        }
    }

    for phrase in &expected.not_contains {
        total += 1;
        if response_lower.contains(&phrase.to_lowercase()) {
            failures.push(format!("unexpected '{phrase}'"));
        } else {
            matched += 1;
        }
    }

    for fact in &expected.facts {
        total += 1;
        if response_lower.contains(&fact.to_lowercase()) {
            matched += 1;
        } else {
            failures.push(format!("missing fact '{fact}'"));
        }
    }

    if let Some(pattern) = &expected.regex {
        total += 1;
        if Regex::new(pattern)?.is_match(response) {
            matched += 1;
        } else {
            failures.push(format!("regex mismatch '{pattern}'"));
        }
    }

    if let Some(exact) = &expected.exact {
        total += 1;
        if response.trim() == exact.trim() {
            matched += 1;
        } else {
            failures.push("exact match failed".to_string());
        }
    }

    let fraction = if total == 0 {
        1.0
    } else {
        matched as f64 / total as f64
    };
    // Every expectation rule is required, so a satisfied outcome is exactly the
    // absence of any recorded failure. The fraction stays as a diagnostic signal.
    let required_satisfied = failures.is_empty();
    let comment = if failures.is_empty() {
        None
    } else {
        Some(failures.join("; "))
    };

    Ok(OutputMatchOutcome {
        fraction,
        required_satisfied,
        rule_count: total,
        comment,
    })
}

#[cfg(test)]
mod tests {
    use super::OutputMatchEvaluator;
    use crate::evaluators::score_is_failure;
    use crate::{EvalResult, EvalScore, EvalScoreValue, Evaluator, ExpectedOutput, TestCase};

    fn required_gate(scores: &[EvalScore]) -> &EvalScore {
        scores
            .iter()
            .find(|score| score.name == "output_match_required")
            .expect("evaluator must emit the hard output_match_required gate")
    }

    fn fraction(scores: &[EvalScore]) -> f64 {
        match scores
            .iter()
            .find(|score| score.name == "output_match")
            .expect("evaluator must emit the diagnostic output_match score")
            .value
        {
            EvalScoreValue::Numeric(value) => value,
            _ => panic!("output_match must be numeric"),
        }
    }

    #[tokio::test]
    async fn contains_rules_pass_when_all_terms_match() {
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

        let scores = evaluator.evaluate(&case, &result).await.expect("score");
        assert_eq!(fraction(&scores), 1.0);
        assert_eq!(
            required_gate(&scores).value,
            EvalScoreValue::Boolean(true),
            "all required fragments present must pass the hard gate"
        );
        assert!(
            !score_is_failure(required_gate(&scores)),
            "a satisfied gate must not downgrade the scenario"
        );
    }

    #[tokio::test]
    async fn one_of_two_required_contains_fails_scenario() {
        // Pins (F15): a response matching only 1 of 2 required `contains` fragments
        // scores 0.5 diagnostically but FAILS the hard gate, so the harness downgrades
        // the scenario. Previously the 0.5 average passed the global threshold.
        let evaluator = OutputMatchEvaluator;
        let case = TestCase {
            expected_output: Some(ExpectedOutput {
                contains: vec!["deployed".to_string(), "production".to_string()],
                ..ExpectedOutput::default()
            }),
            ..TestCase::default()
        };
        let result = EvalResult {
            response: Some("App deployed to staging".to_string()),
            ..EvalResult::default()
        };

        let scores = evaluator.evaluate(&case, &result).await.expect("score");
        assert_eq!(
            fraction(&scores),
            0.5,
            "diagnostic fraction is still reported"
        );
        assert_eq!(
            required_gate(&scores).value,
            EvalScoreValue::Boolean(false),
            "a missing required fragment must fail the hard gate"
        );
        assert!(
            score_is_failure(required_gate(&scores)),
            "the failed gate must downgrade the scenario to Failed"
        );
    }

    #[tokio::test]
    async fn present_exclusion_fails_scenario() {
        // Pins (F15): a `not_contains` exclusion that appears in the response fails the
        // hard gate — exclusions are safety requirements, not fractional signals.
        let evaluator = OutputMatchEvaluator;
        let case = TestCase {
            expected_output: Some(ExpectedOutput {
                contains: vec!["deployed".to_string()],
                not_contains: vec!["error".to_string()],
                ..ExpectedOutput::default()
            }),
            ..TestCase::default()
        };
        let result = EvalResult {
            response: Some("App deployed but hit an error".to_string()),
            ..EvalResult::default()
        };

        let scores = evaluator.evaluate(&case, &result).await.expect("score");
        assert_eq!(
            required_gate(&scores).value,
            EvalScoreValue::Boolean(false),
            "a present exclusion must fail the hard gate"
        );
    }

    #[tokio::test]
    async fn no_expectation_rules_emit_only_diagnostic_score() {
        // Pins: an empty expectation block has no required rules, so no hard gate is
        // emitted and the diagnostic fraction is a vacuous 1.0.
        let evaluator = OutputMatchEvaluator;
        let case = TestCase {
            expected_output: Some(ExpectedOutput::default()),
            ..TestCase::default()
        };
        let result = EvalResult {
            response: Some("anything".to_string()),
            ..EvalResult::default()
        };

        let scores = evaluator.evaluate(&case, &result).await.expect("score");
        assert_eq!(fraction(&scores), 1.0);
        assert!(
            !scores
                .iter()
                .any(|score| score.name == "output_match_required"),
            "no rules means no hard gate score"
        );
    }
}
