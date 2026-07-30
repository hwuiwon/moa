//! Communication/text assertion evaluator.
//!
//! This is the honest home of the old `ExpectedOutput`: response-text matching,
//! and nothing more. It is a first-class category because "the agent told the
//! user the right thing" is a real requirement — it is just never sufficient
//! evidence that the agent *did* the right thing, which is why a case that only
//! carries this category can no longer claim an environment or action outcome.

use regex::Regex;
use serde_json::{Value, json};

use crate::assertion::{
    AssertionCategory, AssertionEvaluator, AssertionVerdict, EvaluatorDeterminism,
};
use crate::evidence::EvidenceEnvelope;
use crate::types::ExpectedOutput;

/// Registered id of the response-text evaluator.
pub const TEXT_MATCH_EVALUATOR_ID: &str = "text_match";

/// Requires the final response to satisfy every authored text rule.
///
/// Every rule is an all-of requirement. There is no fractional pass: a response
/// matching one of two required fragments fails.
#[derive(Debug, Default, Clone, Copy)]
pub struct TextMatchEvaluator;

impl AssertionEvaluator for TextMatchEvaluator {
    fn id(&self) -> &'static str {
        TEXT_MATCH_EVALUATOR_ID
    }

    fn version(&self) -> u32 {
        1
    }

    fn category(&self) -> AssertionCategory {
        AssertionCategory::Communication
    }

    fn determinism(&self) -> EvaluatorDeterminism {
        EvaluatorDeterminism::Deterministic
    }

    fn evaluate(&self, config: &Value, evidence: &EvidenceEnvelope) -> AssertionVerdict {
        let expected: ExpectedOutput = match serde_json::from_value(config.clone()) {
            Ok(expected) => expected,
            Err(error) => return AssertionVerdict::invalid_config(error),
        };

        let Some(response) = evidence.observations.response.as_deref() else {
            return AssertionVerdict::failed(
                config.clone(),
                Value::Null,
                "no response text was captured for the run",
            );
        };

        let outcome = match match_text(response, &expected) {
            Ok(outcome) => outcome,
            Err(error) => return AssertionVerdict::invalid_config(error),
        };

        let observed = json!({
            "matched_rules": outcome.matched,
            "total_rules": outcome.total,
            "response_bytes": response.len(),
        });
        if outcome.total == 0 {
            return AssertionVerdict::failed(
                config.clone(),
                observed,
                "text_match assertion declares no rules",
            );
        }
        if outcome.failures.is_empty() {
            AssertionVerdict::passed(config.clone(), observed, "every response rule is satisfied")
        } else {
            AssertionVerdict::failed(config.clone(), observed, outcome.failures.join("; "))
        }
    }
}

/// How many text rules matched and which ones did not.
pub(crate) struct TextMatchOutcome {
    /// Number of rules satisfied.
    pub(crate) matched: usize,
    /// Number of rules evaluated.
    pub(crate) total: usize,
    /// Human-readable description of every unmet rule.
    pub(crate) failures: Vec<String>,
}

impl TextMatchOutcome {
    /// Returns the diagnostic coverage fraction, which never gates on its own.
    pub(crate) fn fraction(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.matched as f64 / self.total as f64
        }
    }
}

/// Scores a response against every authored text rule.
pub(crate) fn match_text(
    response: &str,
    expected: &ExpectedOutput,
) -> Result<TextMatchOutcome, regex::Error> {
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

    Ok(TextMatchOutcome {
        matched,
        total,
        failures,
    })
}

#[cfg(test)]
mod tests {
    use super::{TEXT_MATCH_EVALUATOR_ID, TextMatchEvaluator};
    use crate::assertion::AssertionEvaluator;
    use crate::evidence::{EvidenceEnvelope, EvidenceSubject};
    use serde_json::json;

    fn evidence(response: &str) -> EvidenceEnvelope {
        EvidenceEnvelope::builder(EvidenceSubject::default())
            .source("unit_test")
            .response(response)
            .build()
    }

    #[test]
    fn all_required_fragments_present_passes() {
        let verdict = TextMatchEvaluator.evaluate(
            &json!({ "contains": ["deployed", "staging"] }),
            &evidence("App deployed to staging successfully"),
        );

        assert!(verdict.passed, "{}", verdict.diagnostic);
    }

    #[test]
    fn one_of_two_required_fragments_fails() {
        // Pins: text rules are all-of. A half-matching response is a failure,
        // not a 0.5 that squeaks past a global threshold.
        let verdict = TextMatchEvaluator.evaluate(
            &json!({ "contains": ["deployed", "production"] }),
            &evidence("App deployed to staging"),
        );

        assert!(!verdict.passed);
        assert!(verdict.diagnostic.contains("production"));
    }

    #[test]
    fn a_present_exclusion_fails() {
        let verdict = TextMatchEvaluator.evaluate(
            &json!({ "contains": ["deployed"], "not_contains": ["error"] }),
            &evidence("App deployed but hit an error"),
        );

        assert!(!verdict.passed);
    }

    #[test]
    fn an_uncaptured_response_fails_closed() {
        let empty = EvidenceEnvelope::builder(EvidenceSubject::default())
            .source("unit_test")
            .build();

        let verdict = TextMatchEvaluator.evaluate(&json!({ "contains": ["hello"] }), &empty);

        assert!(!verdict.passed);
        assert!(verdict.diagnostic.contains("no response text"));
    }

    #[test]
    fn a_ruleless_config_fails_instead_of_passing_vacuously() {
        let verdict = TextMatchEvaluator.evaluate(&json!({}), &evidence("anything"));

        assert!(!verdict.passed);
    }

    #[test]
    fn the_registered_identity_is_stable() {
        assert_eq!(TextMatchEvaluator.id(), TEXT_MATCH_EVALUATOR_ID);
        assert_eq!(TextMatchEvaluator.version(), 1);
    }
}
