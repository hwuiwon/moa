//! Typed, data-only assertions evaluated against captured run evidence.
//!
//! An authored case never carries an executable oracle. It carries an
//! [`AssertionSpec`], which names a *server-registered* evaluator by id and
//! version and hands it a pure JSON parameter block. Anything the registry does
//! not know about fails closed, so a suite cannot smuggle behavior in through
//! its fixture.
//!
//! Four categories exist, matching what a case can actually claim:
//!
//! - [`AssertionCategory::EnvironmentState`] — the world ended in a state;
//! - [`AssertionCategory::Communication`] — the response said something;
//! - [`AssertionCategory::SemanticHistory`] — the conversation or its lineage
//!   carried something;
//! - [`AssertionCategory::Action`] — required, prohibited, ordered, or
//!   approval-gated effects.
//!
//! Evaluation is synchronous and pure over an already-captured
//! [`EvidenceEnvelope`], which is what makes frozen-observation scorer tests and
//! mutation slices possible without a runtime.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::evidence::EvidenceEnvelope;
use crate::results::EvalResult;
use crate::types::{TEST_CASE_SCHEMA_VERSION, TestCase};

/// Whether an evaluator returns the same verdict for the same evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorDeterminism {
    /// Same evidence always produces the same verdict.
    #[default]
    Deterministic,
    /// The verdict may vary across calls (model judges, sampling).
    Stochastic,
}

/// Reference to one registered evaluator.
///
/// The version is part of the identity: a registry whose evaluator moved to
/// version 2 refuses a spec still pinned to version 1 rather than silently
/// re-scoring it under new semantics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct EvaluatorRef {
    /// Registered evaluator id.
    pub id: String,
    /// Registered evaluator version.
    pub version: u32,
    /// Determinism class the author expects.
    pub determinism: EvaluatorDeterminism,
}

impl EvaluatorRef {
    /// Creates a deterministic evaluator reference.
    #[must_use]
    pub fn deterministic(id: impl Into<String>, version: u32) -> Self {
        Self {
            id: id.into(),
            version,
            determinism: EvaluatorDeterminism::Deterministic,
        }
    }
}

/// What a case is claiming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssertionCategory {
    /// The observable world ended in an exact state.
    #[default]
    EnvironmentState,
    /// The final response communicated something.
    Communication,
    /// Conversation history or lineage carried something.
    SemanticHistory,
    /// Required, prohibited, ordered, or approval-gated effects.
    Action,
}

/// Whether a failed assertion fails the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GateEffect {
    /// A failure downgrades the run to `Failed`.
    #[default]
    Blocking,
    /// A failure is reported but never gates. Path similarity lives here.
    Diagnostic,
}

impl GateEffect {
    /// Returns whether this effect gates the run.
    #[must_use]
    pub const fn is_blocking(self) -> bool {
        matches!(self, Self::Blocking)
    }
}

/// One authored, data-only assertion.
///
/// Field order is deliberate: scalars precede the two nested tables so the spec
/// serializes cleanly back into TOML for generated suites.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AssertionSpec {
    /// Stable, case-unique assertion identity used by reports and reviews.
    pub id: String,
    /// Category the assertion claims.
    pub category: AssertionCategory,
    /// Whether a failure gates the run.
    pub gate_effect: GateEffect,
    /// Registered evaluator this assertion selects.
    pub evaluator: EvaluatorRef,
    /// Pure JSON parameters handed to the evaluator.
    pub config: Value,
}

/// Verdict returned by a registered evaluator.
#[derive(Debug, Clone, PartialEq)]
pub struct AssertionVerdict {
    /// Whether the claim held.
    pub passed: bool,
    /// Bounded structured description of what was required.
    pub expected: Value,
    /// Bounded structured description of what was observed.
    pub observed: Value,
    /// Short deterministic diagnostic.
    pub diagnostic: String,
}

impl AssertionVerdict {
    /// Builds a passing verdict.
    #[must_use]
    pub fn passed(expected: Value, observed: Value, diagnostic: impl Into<String>) -> Self {
        Self {
            passed: true,
            expected,
            observed,
            diagnostic: diagnostic.into(),
        }
    }

    /// Builds a failing verdict.
    #[must_use]
    pub fn failed(expected: Value, observed: Value, diagnostic: impl Into<String>) -> Self {
        Self {
            passed: false,
            expected,
            observed,
            diagnostic: diagnostic.into(),
        }
    }

    /// Builds the fail-closed verdict used when a config cannot be parsed.
    #[must_use]
    pub fn invalid_config(error: impl std::fmt::Display) -> Self {
        Self::failed(
            Value::Null,
            Value::Null,
            format!("assertion config is not valid for this evaluator: {error}"),
        )
    }
}

/// Result of evaluating one [`AssertionSpec`].
///
/// This mirrors the domain-specific execution invariant result shape
/// (`invariant_id`, `passed`, `expected`, `observed`, `diagnostic`) so typed
/// domain suites can adapt into the shared result identity without giving up
/// their own authority over what an invariant means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionOutcome {
    /// Case-unique assertion identity.
    pub assertion_id: String,
    /// Category the assertion claimed.
    pub category: AssertionCategory,
    /// Evaluator that produced the verdict.
    pub evaluator: EvaluatorRef,
    /// Whether a failure gates the run.
    pub gate_effect: GateEffect,
    /// Whether the claim held.
    pub passed: bool,
    /// Bounded structured description of what was required.
    pub expected: Value,
    /// Bounded structured description of what was observed.
    pub observed: Value,
    /// Short deterministic diagnostic.
    pub diagnostic: String,
}

impl AssertionOutcome {
    /// Returns whether this outcome must downgrade the run to `Failed`.
    #[must_use]
    pub const fn is_gate_failure(&self) -> bool {
        self.gate_effect.is_blocking() && !self.passed
    }
}

/// One registered, data-driven assertion evaluator.
///
/// Implementations are pure functions of `(config, evidence)`. They never see a
/// live environment, which is what makes an assertion reproducible from a
/// persisted envelope.
pub trait AssertionEvaluator: Send + Sync + std::fmt::Debug {
    /// Registered evaluator id.
    fn id(&self) -> &'static str;

    /// Registered evaluator version.
    fn version(&self) -> u32;

    /// Category this evaluator serves.
    fn category(&self) -> AssertionCategory;

    /// Determinism class of this evaluator.
    fn determinism(&self) -> EvaluatorDeterminism;

    /// Scores one assertion config against captured evidence.
    fn evaluate(&self, config: &Value, evidence: &EvidenceEnvelope) -> AssertionVerdict;
}

/// Registry of evaluators an authored assertion is allowed to select.
#[derive(Debug, Default, Clone)]
pub struct AssertionRegistry {
    evaluators: BTreeMap<&'static str, Arc<dyn AssertionEvaluator>>,
}

impl AssertionRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a registry holding every built-in evaluator.
    #[must_use]
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        for evaluator in crate::evaluators::builtin_assertion_evaluators() {
            registry
                .register(evaluator)
                .expect("built-in assertion evaluator ids are unique");
        }
        registry
    }

    /// Registers one evaluator, rejecting a duplicate id.
    pub fn register(&mut self, evaluator: Arc<dyn AssertionEvaluator>) -> Result<()> {
        let id = evaluator.id();
        if self.evaluators.contains_key(id) {
            return Err(Error::InvalidConfig(format!(
                "assertion evaluator '{id}' is already registered"
            )));
        }
        self.evaluators.insert(id, evaluator);
        Ok(())
    }

    /// Returns the evaluator registered under an id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Arc<dyn AssertionEvaluator>> {
        self.evaluators.get(id)
    }

    /// Returns every registered id, sorted.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.evaluators.keys().copied().collect()
    }

    /// Rejects a spec the registry cannot honor exactly.
    ///
    /// Called at load time so an unusable suite is refused before it burns a
    /// single provider call, and again at evaluation time so a programmatically
    /// built case cannot bypass the check.
    pub fn check_spec(&self, spec: &AssertionSpec) -> Result<()> {
        if spec.id.trim().is_empty() {
            return Err(Error::InvalidConfig(
                "assertion is missing a stable id".to_string(),
            ));
        }
        let Some(evaluator) = self.get(&spec.evaluator.id) else {
            return Err(Error::InvalidConfig(format!(
                "assertion '{}' selects unregistered evaluator '{}'; registered: [{}]",
                spec.id,
                spec.evaluator.id,
                self.ids().join(", ")
            )));
        };
        if evaluator.version() != spec.evaluator.version {
            return Err(Error::InvalidConfig(format!(
                "assertion '{}' pins evaluator '{}' version {} but the registry serves version {}",
                spec.id,
                spec.evaluator.id,
                spec.evaluator.version,
                evaluator.version()
            )));
        }
        if evaluator.category() != spec.category {
            return Err(Error::InvalidConfig(format!(
                "assertion '{}' declares category {:?} but evaluator '{}' serves {:?}",
                spec.id,
                spec.category,
                spec.evaluator.id,
                evaluator.category()
            )));
        }
        if evaluator.determinism() != spec.evaluator.determinism {
            return Err(Error::InvalidConfig(format!(
                "assertion '{}' declares {:?} but evaluator '{}' is {:?}",
                spec.id,
                spec.evaluator.determinism,
                spec.evaluator.id,
                evaluator.determinism()
            )));
        }
        Ok(())
    }

    /// Rejects a case whose assertion set the registry cannot honor.
    pub fn check_case(&self, case: &TestCase) -> Result<()> {
        if case.schema_version != TEST_CASE_SCHEMA_VERSION {
            return Err(Error::InvalidConfig(format!(
                "test case '{}' declares schema version {} but this build requires {}",
                case.name, case.schema_version, TEST_CASE_SCHEMA_VERSION
            )));
        }
        let mut seen = BTreeSet::new();
        for spec in &case.assertions {
            self.check_spec(spec)?;
            if !seen.insert(spec.id.as_str()) {
                return Err(Error::InvalidConfig(format!(
                    "test case '{}' repeats assertion id '{}'",
                    case.name, spec.id
                )));
            }
        }
        Ok(())
    }
}

/// Returns the process-wide built-in registry.
#[must_use]
pub fn builtin_registry() -> &'static AssertionRegistry {
    static REGISTRY: OnceLock<AssertionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(AssertionRegistry::with_builtins)
}

/// Evaluates a case's assertions against captured evidence.
///
/// Every failure mode is closed rather than skipped: a wrong case version, an
/// absent envelope, an envelope defect, an unregistered evaluator, or a version
/// or category mismatch all produce failing outcomes carrying the reason. There
/// is no path on which a blocking assertion silently disappears.
#[must_use]
pub fn evaluate_assertions(
    registry: &AssertionRegistry,
    case: &TestCase,
    evidence: Option<&EvidenceEnvelope>,
) -> Vec<AssertionOutcome> {
    if case.assertions.is_empty() {
        return Vec::new();
    }

    if let Some(reason) = blocking_defect(case, evidence) {
        return case
            .assertions
            .iter()
            .map(|spec| fail_closed(spec, &reason))
            .collect();
    }

    let envelope = evidence.expect("blocking_defect returns a reason when evidence is absent");
    case.assertions
        .iter()
        .map(|spec| match registry.check_spec(spec) {
            Err(error) => fail_closed(spec, &error.to_string()),
            Ok(()) => {
                let evaluator = registry
                    .get(&spec.evaluator.id)
                    .expect("check_spec proved the evaluator is registered");
                let verdict = evaluator.evaluate(&spec.config, envelope);
                AssertionOutcome {
                    assertion_id: spec.id.clone(),
                    category: spec.category,
                    evaluator: spec.evaluator.clone(),
                    gate_effect: spec.gate_effect,
                    passed: verdict.passed,
                    expected: verdict.expected,
                    observed: verdict.observed,
                    diagnostic: verdict.diagnostic,
                }
            }
        })
        .collect()
}

/// Evaluates a case's assertions against a completed result using the built-in
/// registry.
#[must_use]
pub fn evaluate_case_assertions(case: &TestCase, result: &EvalResult) -> Vec<AssertionOutcome> {
    evaluate_assertions(builtin_registry(), case, result.evidence.as_ref())
}

/// Returns the reason every assertion must fail closed, when one applies.
fn blocking_defect(case: &TestCase, evidence: Option<&EvidenceEnvelope>) -> Option<String> {
    if case.schema_version != TEST_CASE_SCHEMA_VERSION {
        return Some(format!(
            "test case '{}' declares schema version {} but this build requires {}",
            case.name, case.schema_version, TEST_CASE_SCHEMA_VERSION
        ));
    }
    let Some(envelope) = evidence else {
        return Some(format!(
            "no evidence envelope was captured for case '{}'",
            case.name
        ));
    };
    if let Err(defect) = envelope.validate() {
        return Some(defect.to_string());
    }
    if envelope.subject.case_schema_version != case.schema_version {
        return Some(format!(
            "evidence was captured for case schema version {} but the case declares {}",
            envelope.subject.case_schema_version, case.schema_version
        ));
    }
    if !envelope.subject.case.is_empty() && envelope.subject.case != case.name {
        return Some(format!(
            "evidence subject '{}' does not match case '{}'",
            envelope.subject.case, case.name
        ));
    }
    None
}

fn fail_closed(spec: &AssertionSpec, reason: &str) -> AssertionOutcome {
    AssertionOutcome {
        assertion_id: spec.id.clone(),
        category: spec.category,
        evaluator: spec.evaluator.clone(),
        gate_effect: spec.gate_effect,
        passed: false,
        expected: spec.config.clone(),
        observed: Value::Null,
        diagnostic: format!("assertion failed closed: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AssertionCategory, AssertionRegistry, AssertionSpec, EvaluatorDeterminism, EvaluatorRef,
        GateEffect, builtin_registry, evaluate_assertions,
    };
    use crate::evidence::{EvidenceEnvelope, EvidenceSubject};
    use crate::types::{TEST_CASE_SCHEMA_VERSION, TestCase};
    use serde_json::json;

    fn evidence(case: &str) -> EvidenceEnvelope {
        EvidenceEnvelope::builder(EvidenceSubject {
            case: case.to_string(),
            case_schema_version: TEST_CASE_SCHEMA_VERSION,
            agent_config: "config".to_string(),
            run_label: "run".to_string(),
        })
        .source("unit_test")
        .state("deploy.production", json!("2.1"))
        .build()
    }

    fn case_with(spec: AssertionSpec) -> TestCase {
        TestCase {
            name: "case".to_string(),
            assertions: vec![spec],
            ..TestCase::default()
        }
    }

    fn state_spec() -> AssertionSpec {
        AssertionSpec {
            id: "prod-version".to_string(),
            category: AssertionCategory::EnvironmentState,
            gate_effect: GateEffect::Blocking,
            evaluator: EvaluatorRef::deterministic("environment_state", 1),
            config: json!({ "expect": { "deploy.production": "2.1" } }),
        }
    }

    #[test]
    fn every_builtin_category_is_served_by_the_registry() {
        // Pins: the four server-registered categories the plan requires all
        // resolve, so a suite can express each claim without a custom oracle.
        let registry = builtin_registry();
        let categories = registry
            .ids()
            .into_iter()
            .filter_map(|id| registry.get(id).map(|evaluator| evaluator.category()))
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(
            categories,
            [
                AssertionCategory::EnvironmentState,
                AssertionCategory::Communication,
                AssertionCategory::SemanticHistory,
                AssertionCategory::Action,
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
        );
    }

    #[test]
    fn an_unregistered_evaluator_fails_closed() {
        // Pins: an assertion cannot select behavior the server does not serve.
        let mut spec = state_spec();
        spec.evaluator = EvaluatorRef::deterministic("shell_script", 1);
        let case = case_with(spec);

        let outcomes = evaluate_assertions(builtin_registry(), &case, Some(&evidence("case")));

        assert_eq!(outcomes.len(), 1);
        assert!(!outcomes[0].passed);
        assert!(outcomes[0].is_gate_failure());
        assert!(
            outcomes[0].diagnostic.contains("unregistered evaluator"),
            "{}",
            outcomes[0].diagnostic
        );
    }

    #[test]
    fn a_pinned_version_mismatch_fails_closed() {
        let mut spec = state_spec();
        spec.evaluator.version = 99;
        let case = case_with(spec);

        let outcomes = evaluate_assertions(builtin_registry(), &case, Some(&evidence("case")));

        assert!(!outcomes[0].passed);
        assert!(outcomes[0].diagnostic.contains("version"));
    }

    #[test]
    fn a_declared_determinism_mismatch_fails_closed() {
        // Pins: a case cannot relabel a deterministic evaluator as stochastic to
        // dodge a reproducibility requirement.
        let mut spec = state_spec();
        spec.evaluator.determinism = EvaluatorDeterminism::Stochastic;
        let case = case_with(spec);

        let outcomes = evaluate_assertions(builtin_registry(), &case, Some(&evidence("case")));

        assert!(!outcomes[0].passed);
    }

    #[test]
    fn missing_evidence_fails_every_assertion_closed() {
        let case = case_with(state_spec());

        let outcomes = evaluate_assertions(builtin_registry(), &case, None);

        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].is_gate_failure());
        assert!(outcomes[0].diagnostic.contains("no evidence envelope"));
    }

    #[test]
    fn a_wrong_case_schema_version_fails_every_assertion_closed() {
        let mut case = case_with(state_spec());
        case.schema_version = TEST_CASE_SCHEMA_VERSION - 1;

        let outcomes = evaluate_assertions(builtin_registry(), &case, Some(&evidence("case")));

        assert!(outcomes[0].is_gate_failure());
        assert!(outcomes[0].diagnostic.contains("schema version"));
    }

    #[test]
    fn evidence_captured_for_a_different_case_fails_closed() {
        // Pins: an envelope from another case cannot be substituted to certify
        // this one.
        let case = case_with(state_spec());

        let outcomes =
            evaluate_assertions(builtin_registry(), &case, Some(&evidence("other-case")));

        assert!(outcomes[0].is_gate_failure());
        assert!(outcomes[0].diagnostic.contains("does not match case"));
    }

    #[test]
    fn a_spec_round_trips_through_json_and_carries_no_executable_field() {
        let spec = state_spec();
        let encoded = serde_json::to_value(&spec).expect("serialize");
        let object = encoded.as_object().expect("spec is an object");

        assert_eq!(
            object.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["id", "category", "gate_effect", "evaluator", "config"],
            "an assertion spec is exactly identity, category, gate, evaluator ref, and data"
        );
        let decoded: AssertionSpec = serde_json::from_value(encoded).expect("deserialize");
        assert_eq!(decoded, spec);
    }

    #[test]
    fn duplicate_assertion_ids_are_rejected() {
        let registry = AssertionRegistry::with_builtins();
        let case = TestCase {
            name: "case".to_string(),
            assertions: vec![state_spec(), state_spec()],
            ..TestCase::default()
        };

        let error = registry.check_case(&case).expect_err("duplicate ids");
        assert!(error.to_string().contains("repeats assertion id"));
    }
}
