//! Environment-state assertion evaluator.
//!
//! This is the oracle a response-text expectation can never be: it reads the
//! final world state captured before teardown and requires exact values at
//! named keys. Two agents that took entirely different valid paths to the same
//! state both satisfy it; an agent that narrated the right answer without
//! changing the world does not.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::assertion::{
    AssertionCategory, AssertionEvaluator, AssertionVerdict, EvaluatorDeterminism,
};
use crate::evidence::EvidenceEnvelope;

/// Registered id of the environment-state evaluator.
pub const ENVIRONMENT_STATE_EVALUATOR_ID: &str = "environment_state";

/// Parameters for [`EnvironmentStateEvaluator`].
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct EnvironmentStateConfig {
    /// State keys that must hold exactly these values.
    pub expect: BTreeMap<String, Value>,
    /// State keys that must not be present at all.
    pub absent: Vec<String>,
}

/// Requires exact final-state values at named keys.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvironmentStateEvaluator;

impl AssertionEvaluator for EnvironmentStateEvaluator {
    fn id(&self) -> &'static str {
        ENVIRONMENT_STATE_EVALUATOR_ID
    }

    fn version(&self) -> u32 {
        1
    }

    fn category(&self) -> AssertionCategory {
        AssertionCategory::EnvironmentState
    }

    fn determinism(&self) -> EvaluatorDeterminism {
        EvaluatorDeterminism::Deterministic
    }

    fn evaluate(&self, config: &Value, evidence: &EvidenceEnvelope) -> AssertionVerdict {
        let config: EnvironmentStateConfig = match serde_json::from_value(config.clone()) {
            Ok(config) => config,
            Err(error) => return AssertionVerdict::invalid_config(error),
        };

        if config.expect.is_empty() && config.absent.is_empty() {
            return AssertionVerdict::failed(
                json!({}),
                json!({}),
                "environment_state assertion declares no expected or absent keys",
            );
        }

        let state = &evidence.observations.final_state;
        let mut failures = Vec::new();
        let mut observed = serde_json::Map::new();

        for (key, expected) in &config.expect {
            match state.get(key) {
                Some(actual) => {
                    observed.insert(key.clone(), actual.clone());
                    if actual != expected {
                        failures.push(format!("{key} is {actual} but must be {expected}"));
                    }
                }
                None => {
                    observed.insert(key.clone(), Value::Null);
                    failures.push(format!("{key} is absent but must be {expected}"));
                }
            }
        }

        for key in &config.absent {
            if let Some(actual) = state.get(key) {
                observed.insert(key.clone(), actual.clone());
                failures.push(format!("{key} must be absent but is {actual}"));
            }
        }

        let expected = json!({
            "expect": config
                .expect
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<serde_json::Map<String, Value>>(),
            "absent": config.absent,
        });
        let observed = Value::Object(observed);
        if failures.is_empty() {
            AssertionVerdict::passed(expected, observed, "final state matches every expected key")
        } else {
            AssertionVerdict::failed(expected, observed, failures.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ENVIRONMENT_STATE_EVALUATOR_ID, EnvironmentStateEvaluator};
    use crate::assertion::AssertionEvaluator;
    use crate::evidence::{EvidenceEnvelope, EvidenceSubject};
    use serde_json::json;

    fn evidence_with_state() -> EvidenceEnvelope {
        EvidenceEnvelope::builder(EvidenceSubject::default())
            .source("unit_test")
            .state("deploy.production", json!("2.1"))
            .state("ticket.TCK-1", json!("closed"))
            .build()
    }

    #[test]
    fn exact_state_matches() {
        let verdict = EnvironmentStateEvaluator.evaluate(
            &json!({ "expect": { "deploy.production": "2.1", "ticket.TCK-1": "closed" } }),
            &evidence_with_state(),
        );

        assert!(verdict.passed, "{}", verdict.diagnostic);
    }

    #[test]
    fn a_wrong_value_fails_and_names_the_key() {
        let verdict = EnvironmentStateEvaluator.evaluate(
            &json!({ "expect": { "deploy.production": "2.0" } }),
            &evidence_with_state(),
        );

        assert!(!verdict.passed);
        assert!(
            verdict.diagnostic.contains("deploy.production"),
            "{}",
            verdict.diagnostic
        );
    }

    #[test]
    fn an_absent_key_fails_rather_than_passing_vacuously() {
        // Pins: a harness with no environment oracle captures no state, and an
        // environment assertion over it must fail rather than find nothing to
        // contradict.
        let empty = EvidenceEnvelope::builder(EvidenceSubject::default())
            .source("unit_test")
            .build();

        let verdict = EnvironmentStateEvaluator
            .evaluate(&json!({ "expect": { "deploy.production": "2.1" } }), &empty);

        assert!(!verdict.passed);
        assert!(verdict.diagnostic.contains("absent"));
    }

    #[test]
    fn an_absent_requirement_catches_a_key_that_should_not_exist() {
        let verdict = EnvironmentStateEvaluator.evaluate(
            &json!({ "absent": ["ticket.TCK-1"] }),
            &evidence_with_state(),
        );

        assert!(!verdict.passed);
        assert!(verdict.diagnostic.contains("must be absent"));
    }

    #[test]
    fn an_empty_config_fails_instead_of_passing_vacuously() {
        let verdict = EnvironmentStateEvaluator.evaluate(&json!({}), &evidence_with_state());

        assert!(!verdict.passed);
    }

    #[test]
    fn an_unknown_config_key_fails_closed() {
        // Pins: a typo in an authored assertion is a failure, not a silently
        // weakened claim.
        let verdict = EnvironmentStateEvaluator
            .evaluate(&json!({ "expects": { "a": 1 } }), &evidence_with_state());

        assert!(!verdict.passed);
        assert!(verdict.diagnostic.contains("not valid"));
    }

    #[test]
    fn the_registered_identity_is_stable() {
        assert_eq!(
            EnvironmentStateEvaluator.id(),
            ENVIRONMENT_STATE_EVALUATOR_ID
        );
        assert_eq!(EnvironmentStateEvaluator.version(), 1);
    }
}
