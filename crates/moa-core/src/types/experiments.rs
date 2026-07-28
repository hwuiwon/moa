//! Typed Behavior Lab scorecard definitions shared by plan artifacts, the
//! experiment store, and the trial workflow finalizer.
//!
//! A scorecard is the list of evaluator results a Behavior Lab trial must
//! produce before its evidence is considered complete. Every requirement names
//! the evaluator that produces it, the exact version of that evaluator, the
//! exact score name and value type it emits, the deterministic configuration it
//! runs under, and whether a missing or failing result blocks eligibility.
//!
//! This module owns only the structural contract. Evaluator existence, version
//! validity, configuration schemas, and determinism are owned by the evaluator
//! registry in `moa-experiments`, which is the crate that actually runs them.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use thiserror::Error;

/// Value type an evaluator writes into its score row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScorecardValueType {
    /// A floating-point measurement.
    Numeric,
    /// A pass/fail assertion.
    Boolean,
    /// A closed-vocabulary label.
    Categorical,
}

impl ScorecardValueType {
    /// Returns the `analytics.scores.value_type` representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Numeric => "numeric",
            Self::Boolean => "boolean",
            Self::Categorical => "categorical",
        }
    }

    /// Parses a value type loaded from durable storage.
    #[must_use]
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "numeric" => Some(Self::Numeric),
            "boolean" => Some(Self::Boolean),
            "categorical" => Some(Self::Categorical),
            _ => None,
        }
    }
}

/// Whether a requirement participates in the eligibility gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScorecardEffect {
    /// A missing or failing result makes the scorecard ineligible.
    Blocking,
    /// The result is recorded but never gates eligibility.
    Informational,
}

impl ScorecardEffect {
    /// Returns true when this effect participates in the eligibility gate.
    #[must_use]
    pub const fn is_blocking(self) -> bool {
        matches!(self, Self::Blocking)
    }

    /// Returns the persisted representation for this effect.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Blocking => "blocking",
            Self::Informational => "informational",
        }
    }
}

/// One evaluator result a trial must produce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScorecardRequirement {
    /// Stable evaluator identifier, resolved against the evaluator registry.
    pub evaluator_id: String,
    /// Exact evaluator version. Part of score identity, so a version bump
    /// produces a different score row rather than overwriting the old one.
    pub evaluator_version: String,
    /// Exact score name this evaluator writes.
    pub score_name: String,
    /// Exact score value type this evaluator writes.
    pub value_type: ScorecardValueType,
    /// Deterministic evaluator configuration, validated by the registry.
    #[serde(default)]
    pub config: Value,
    /// Whether a missing or failing result blocks eligibility.
    pub effect: ScorecardEffect,
}

/// Reasons a scorecard is structurally invalid.
///
/// Registry-dependent rejections (unknown evaluator, unknown version, invalid
/// threshold, stochastic evaluator marked blocking) are raised by
/// `moa-experiments`, not here.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScorecardError {
    /// A scorecard with no requirements can never prove anything.
    #[error("experiment scorecard must declare at least one requirement")]
    Empty,
    /// Two requirements claim the same score name.
    #[error("experiment scorecard declares duplicate score name `{score_name}`")]
    DuplicateScoreName {
        /// Score name declared more than once.
        score_name: String,
    },
    /// A required identity field is blank.
    #[error("experiment scorecard requirement `{field}` must not be blank")]
    BlankField {
        /// Blank identity field.
        field: &'static str,
    },
    /// No requirement blocks, so the scorecard cannot gate anything.
    #[error("experiment scorecard must declare at least one blocking requirement")]
    NoBlockingRequirement,
}

/// Wire and storage shape for a scorecard before validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExperimentScorecardRepr {
    requirements: Vec<ScorecardRequirement>,
}

/// The set of evaluator results one Behavior Lab experiment requires.
///
/// Construction always validates, and deserialization goes through the same
/// validation, so an `ExperimentScorecard` value is never structurally invalid
/// regardless of where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ExperimentScorecardRepr", into = "ExperimentScorecardRepr")]
pub struct ExperimentScorecard {
    requirements: Vec<ScorecardRequirement>,
}

impl ExperimentScorecard {
    /// Validates and builds a scorecard from its requirements.
    ///
    /// # Errors
    ///
    /// Returns [`ScorecardError`] when the requirement set is empty, declares a
    /// duplicate score name, leaves an identity field blank, or declares no
    /// blocking requirement.
    pub fn new(requirements: Vec<ScorecardRequirement>) -> Result<Self, ScorecardError> {
        if requirements.is_empty() {
            return Err(ScorecardError::Empty);
        }
        let mut seen = BTreeSet::new();
        for requirement in &requirements {
            for (field, value) in [
                ("evaluator_id", requirement.evaluator_id.as_str()),
                ("evaluator_version", requirement.evaluator_version.as_str()),
                ("score_name", requirement.score_name.as_str()),
            ] {
                if value.trim().is_empty() {
                    return Err(ScorecardError::BlankField { field });
                }
            }
            if !seen.insert(requirement.score_name.as_str()) {
                return Err(ScorecardError::DuplicateScoreName {
                    score_name: requirement.score_name.clone(),
                });
            }
        }
        if !requirements
            .iter()
            .any(|requirement| requirement.effect.is_blocking())
        {
            return Err(ScorecardError::NoBlockingRequirement);
        }
        Ok(Self { requirements })
    }

    /// Returns every declared requirement in declaration order.
    #[must_use]
    pub fn requirements(&self) -> &[ScorecardRequirement] {
        &self.requirements
    }

    /// Returns only the requirements that gate eligibility.
    pub fn blocking_requirements(&self) -> impl Iterator<Item = &ScorecardRequirement> {
        self.requirements
            .iter()
            .filter(|requirement| requirement.effect.is_blocking())
    }

    /// Returns the requirement that declares `score_name`, when one exists.
    #[must_use]
    pub fn requirement_for(&self, score_name: &str) -> Option<&ScorecardRequirement> {
        self.requirements
            .iter()
            .find(|requirement| requirement.score_name == score_name)
    }
}

impl TryFrom<ExperimentScorecardRepr> for ExperimentScorecard {
    type Error = ScorecardError;

    fn try_from(value: ExperimentScorecardRepr) -> Result<Self, Self::Error> {
        Self::new(value.requirements)
    }
}

impl From<ExperimentScorecard> for ExperimentScorecardRepr {
    fn from(value: ExperimentScorecard) -> Self {
        Self {
            requirements: value.requirements,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn requirement(score_name: &str, effect: ScorecardEffect) -> ScorecardRequirement {
        ScorecardRequirement {
            evaluator_id: "target_completed".to_string(),
            evaluator_version: "v1".to_string(),
            score_name: score_name.to_string(),
            value_type: ScorecardValueType::Boolean,
            config: json!({}),
            effect,
        }
    }

    #[test]
    fn empty_scorecard_is_rejected_offline() {
        // Pins: a scorecard with no requirements cannot be constructed, so an
        // experiment can never claim complete evidence by requiring nothing.
        assert_eq!(
            ExperimentScorecard::new(Vec::new()).expect_err("empty must be rejected"),
            ScorecardError::Empty
        );
    }

    #[test]
    fn duplicate_score_names_are_rejected_offline() {
        // Pins: exactly one requirement owns each score name, which is what makes
        // "exactly one row per blocking requirement" a decidable gate.
        let error = ExperimentScorecard::new(vec![
            requirement("target_completed", ScorecardEffect::Blocking),
            requirement("target_completed", ScorecardEffect::Informational),
        ])
        .expect_err("duplicate score name must be rejected");

        assert_eq!(
            error,
            ScorecardError::DuplicateScoreName {
                score_name: "target_completed".to_string(),
            }
        );
    }

    #[test]
    fn scorecard_without_a_blocking_requirement_is_rejected_offline() {
        // Pins: an all-informational scorecard cannot gate anything, so it is
        // refused at construction rather than silently passing every trial.
        assert_eq!(
            ExperimentScorecard::new(vec![requirement(
                "target_completed",
                ScorecardEffect::Informational
            )])
            .expect_err("informational-only must be rejected"),
            ScorecardError::NoBlockingRequirement
        );
    }

    #[test]
    fn blank_identity_fields_are_rejected_offline() {
        // Pins: score identity is derived from evaluator id/version/name, so a
        // blank component would collapse two distinct scores onto one row.
        for (field, mutate) in [
            (
                "evaluator_id",
                (|requirement: &mut ScorecardRequirement| {
                    requirement.evaluator_id = "  ".to_string();
                }) as fn(&mut ScorecardRequirement),
            ),
            ("evaluator_version", |requirement| {
                requirement.evaluator_version = String::new();
            }),
            ("score_name", |requirement| {
                requirement.score_name = "\t".to_string();
            }),
        ] {
            let mut candidate = requirement("target_completed", ScorecardEffect::Blocking);
            mutate(&mut candidate);
            assert_eq!(
                ExperimentScorecard::new(vec![candidate])
                    .expect_err("blank identity field must be rejected"),
                ScorecardError::BlankField { field }
            );
        }
    }

    #[test]
    fn deserialization_runs_the_same_validation_as_construction_offline() {
        // Pins: a scorecard loaded from storage or the wire cannot bypass the
        // constructor's rules, so no invalid scorecard can enter through serde.
        let invalid = json!({ "requirements": [] });
        let error = serde_json::from_value::<ExperimentScorecard>(invalid)
            .expect_err("empty requirement set must not deserialize");
        assert!(
            error.to_string().contains("at least one requirement"),
            "unexpected deserialization error: {error}"
        );

        let valid = json!({
            "requirements": [{
                "evaluator_id": "target_completed",
                "evaluator_version": "v1",
                "score_name": "target_completed",
                "value_type": "boolean",
                "config": {},
                "effect": "blocking"
            }]
        });
        let scorecard = serde_json::from_value::<ExperimentScorecard>(valid)
            .expect("valid scorecard deserializes");
        assert_eq!(scorecard.requirements().len(), 1);
        assert_eq!(scorecard.blocking_requirements().count(), 1);
        assert_eq!(
            serde_json::to_value(&scorecard).expect("scorecard serializes"),
            json!({
                "requirements": [{
                    "evaluator_id": "target_completed",
                    "evaluator_version": "v1",
                    "score_name": "target_completed",
                    "value_type": "boolean",
                    "config": {},
                    "effect": "blocking"
                }]
            })
        );
    }
}
