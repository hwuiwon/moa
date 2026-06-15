//! Output-content evaluator covering containment, exclusion, regex, and exact matching.

use regex::Regex;

use crate::{EvalResult, EvalScore, Evaluator, ExpectedOutput, Result, ScoreValue, TestCase};

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
        let score = evaluate_output(response, expected)?;
        Ok(vec![EvalScore {
            evaluator: self.name().to_string(),
            name: "output_match".to_string(),
            value: ScoreValue::Numeric(score.0),
            comment: score.1,
        }])
    }
}

fn evaluate_output(response: &str, expected: &ExpectedOutput) -> Result<(f64, Option<String>)> {
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

    let score = if total == 0 {
        1.0
    } else {
        matched as f64 / total as f64
    };
    let comment = if failures.is_empty() {
        None
    } else {
        Some(failures.join("; "))
    };

    Ok((score, comment))
}

#[cfg(test)]
mod tests {
    use super::OutputMatchEvaluator;
    use crate::{EvalResult, Evaluator, ExpectedOutput, ScoreValue, TestCase};

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
        assert_eq!(scores[0].value, ScoreValue::Numeric(1.0));
    }

    #[tokio::test]
    async fn missing_contains_term_reduces_score() {
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
        assert_eq!(scores[0].value, ScoreValue::Numeric(0.5));
    }
}
