//! Deterministic product evaluators for Behavior Lab trials.
//!
//! These evaluators belong to the product, not to the regression-eval system and
//! not to the orchestrator: they decide whether one trial produced the evidence a
//! Behavior Lab scorecard requires. Every evaluator in the initial registry is
//! deterministic — it reads the typed terminal evidence a trial already produced
//! and returns the same answer on every replay. Scenario-quality judging is
//! declared here as stochastic so the registry can refuse to let it block.

use moa_core::types::experiments::{
    ExperimentScorecard, ScorecardEffect, ScorecardRequirement, ScorecardValueType,
};
use moa_core::types::security::SensitivityClass;
use moa_memory_pii::classify_heuristic;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::evidence::{TrialScoreTarget, TrialTerminalEvidence};

/// Namespace for deterministic score-id derivation.
const SCORE_ID_NAMESPACE: Uuid = Uuid::from_u128(0x8f3d_41b6_9a27_5c04_b1e8_60f5_2d97_a41c);

/// Domain separator for deterministic score-id derivation.
const SCORE_ID_DOMAIN: &str = "moa.experiment.score-id";

/// Whether an evaluator returns the same answer for the same evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorDeterminism {
    /// Replay-stable: same evidence, same result, no model call.
    Deterministic,
    /// Model-backed judging. Informational only; it can never gate eligibility.
    Stochastic,
}

impl EvaluatorDeterminism {
    /// Returns true when this evaluator may be declared blocking.
    #[must_use]
    pub const fn permits_blocking(self) -> bool {
        matches!(self, Self::Deterministic)
    }
}

/// What an evaluator reads from the evidence and how it is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvaluatorKind {
    /// Target reached a terminal state with no provider or runtime error.
    TargetCompleted,
    /// Target produced a non-empty visible result.
    ResultProduced,
    /// Total observed tokens stayed within `max_tokens`.
    TokenBudget,
    /// Total observed cost stayed within `max_cost_cents`.
    CostBudget,
    /// Driven turns stayed within `max_turns`.
    TurnBudget,
    /// Visible output carries no sensitive content above the configured class.
    PrivacySafeOutput,
    /// Model-judged scenario quality. Never evaluated on the trial path.
    ScenarioQuality,
}

/// One registered product evaluator.
#[derive(Debug, Clone, Copy)]
pub struct EvaluatorDescriptor {
    /// Stable evaluator identifier.
    pub id: &'static str,
    /// Exact evaluator version. Part of score identity.
    pub version: &'static str,
    /// Exact score name this evaluator writes.
    pub score_name: &'static str,
    /// Exact score value type this evaluator writes.
    pub value_type: ScorecardValueType,
    /// Whether this evaluator is replay-stable.
    pub determinism: EvaluatorDeterminism,
    kind: EvaluatorKind,
}

/// Every evaluator this build can run or validate against.
pub const EVALUATORS: &[EvaluatorDescriptor] = &[
    EvaluatorDescriptor {
        id: "target_completed",
        version: "v1",
        score_name: "target_completed",
        value_type: ScorecardValueType::Boolean,
        determinism: EvaluatorDeterminism::Deterministic,
        kind: EvaluatorKind::TargetCompleted,
    },
    EvaluatorDescriptor {
        id: "result_produced",
        version: "v1",
        score_name: "result_produced",
        value_type: ScorecardValueType::Boolean,
        determinism: EvaluatorDeterminism::Deterministic,
        kind: EvaluatorKind::ResultProduced,
    },
    EvaluatorDescriptor {
        id: "token_budget",
        version: "v1",
        score_name: "token_budget_respected",
        value_type: ScorecardValueType::Boolean,
        determinism: EvaluatorDeterminism::Deterministic,
        kind: EvaluatorKind::TokenBudget,
    },
    EvaluatorDescriptor {
        id: "cost_budget",
        version: "v1",
        score_name: "cost_budget_respected",
        value_type: ScorecardValueType::Boolean,
        determinism: EvaluatorDeterminism::Deterministic,
        kind: EvaluatorKind::CostBudget,
    },
    EvaluatorDescriptor {
        id: "turn_budget",
        version: "v1",
        score_name: "turn_budget_respected",
        value_type: ScorecardValueType::Boolean,
        determinism: EvaluatorDeterminism::Deterministic,
        kind: EvaluatorKind::TurnBudget,
    },
    EvaluatorDescriptor {
        id: "privacy_safe_output",
        version: "v1",
        score_name: "privacy_safe_output",
        value_type: ScorecardValueType::Boolean,
        determinism: EvaluatorDeterminism::Deterministic,
        kind: EvaluatorKind::PrivacySafeOutput,
    },
    EvaluatorDescriptor {
        id: "scenario_quality",
        version: "v1",
        score_name: "scenario_quality",
        value_type: ScorecardValueType::Numeric,
        determinism: EvaluatorDeterminism::Stochastic,
        kind: EvaluatorKind::ScenarioQuality,
    },
];

/// Reasons a scorecard cannot be run against this registry.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EvaluatorError {
    /// The scorecard names an evaluator this build does not have.
    #[error("unknown evaluator `{evaluator_id}`")]
    UnknownEvaluator {
        /// Requested evaluator ID.
        evaluator_id: String,
    },
    /// The scorecard names a version this build does not have.
    #[error("evaluator `{evaluator_id}` has no version `{version}` in this build")]
    UnknownEvaluatorVersion {
        /// Requested evaluator ID.
        evaluator_id: String,
        /// Requested evaluator version.
        version: String,
    },
    /// A stochastic evaluator was marked blocking.
    #[error("evaluator `{evaluator_id}` is stochastic and cannot be a blocking requirement")]
    StochasticBlocking {
        /// Requested evaluator ID.
        evaluator_id: String,
    },
    /// The evaluator configuration is missing a required threshold.
    #[error("evaluator `{evaluator_id}` requires config key `{key}`")]
    MissingConfigKey {
        /// Requested evaluator ID.
        evaluator_id: String,
        /// Missing config key.
        key: &'static str,
    },
    /// The evaluator configuration carries an unusable threshold.
    #[error("evaluator `{evaluator_id}` config key `{key}` must be {expectation}")]
    InvalidConfigValue {
        /// Requested evaluator ID.
        evaluator_id: String,
        /// Offending config key.
        key: &'static str,
        /// What a usable value looks like.
        expectation: &'static str,
    },
    /// A stochastic evaluator was reached on the deterministic trial path.
    #[error("evaluator `{evaluator_id}` is stochastic and is not evaluated on the trial path")]
    StochasticOnTrialPath {
        /// Requested evaluator ID.
        evaluator_id: String,
    },
}

/// Value one evaluator produced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "v", rename_all = "snake_case")]
pub enum EvaluatedValue {
    /// A numeric measurement.
    Numeric(f64),
    /// A pass/fail assertion.
    Boolean(bool),
    /// A closed-vocabulary label.
    Categorical(String),
}

impl EvaluatedValue {
    /// Returns the value type this value occupies.
    #[must_use]
    pub const fn value_type(&self) -> ScorecardValueType {
        match self {
            Self::Numeric(_) => ScorecardValueType::Numeric,
            Self::Boolean(_) => ScorecardValueType::Boolean,
            Self::Categorical(_) => ScorecardValueType::Categorical,
        }
    }
}

/// One evaluator result ready to be written as a score row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluatedScore {
    /// Deterministic score identity.
    pub score_id: Uuid,
    /// Evaluator that produced the result.
    pub evaluator_id: String,
    /// Exact evaluator version that produced the result.
    pub evaluator_version: String,
    /// Exact score name.
    pub score_name: String,
    /// Produced value.
    pub value: EvaluatedValue,
    /// Whether this result blocks eligibility.
    pub effect: ScorecardEffect,
    /// Whether the result satisfies its requirement.
    pub passed: bool,
}

/// Looks up a registered evaluator by ID and version.
///
/// # Errors
///
/// Returns [`EvaluatorError::UnknownEvaluator`] when no evaluator has that ID and
/// [`EvaluatorError::UnknownEvaluatorVersion`] when the ID exists at another version.
pub fn descriptor(
    evaluator_id: &str,
    version: &str,
) -> Result<&'static EvaluatorDescriptor, EvaluatorError> {
    let mut id_exists = false;
    for candidate in EVALUATORS {
        if candidate.id != evaluator_id {
            continue;
        }
        id_exists = true;
        if candidate.version == version {
            return Ok(candidate);
        }
    }
    if id_exists {
        return Err(EvaluatorError::UnknownEvaluatorVersion {
            evaluator_id: evaluator_id.to_string(),
            version: version.to_string(),
        });
    }
    Err(EvaluatorError::UnknownEvaluator {
        evaluator_id: evaluator_id.to_string(),
    })
}

/// Validates every requirement in a scorecard against this build's registry.
///
/// # Errors
///
/// Returns the first [`EvaluatorError`] raised by a requirement.
pub fn validate_scorecard(scorecard: &ExperimentScorecard) -> Result<(), EvaluatorError> {
    for requirement in scorecard.requirements() {
        validate_requirement(requirement)?;
    }
    Ok(())
}

/// Validates one requirement against this build's registry.
///
/// # Errors
///
/// Returns [`EvaluatorError`] when the evaluator is unknown, a stochastic
/// evaluator is marked blocking, or the deterministic configuration is missing
/// or unusable.
pub fn validate_requirement(requirement: &ScorecardRequirement) -> Result<(), EvaluatorError> {
    let descriptor = descriptor(&requirement.evaluator_id, &requirement.evaluator_version)?;
    if requirement.effect.is_blocking() && !descriptor.determinism.permits_blocking() {
        return Err(EvaluatorError::StochasticBlocking {
            evaluator_id: requirement.evaluator_id.clone(),
        });
    }
    validate_config(descriptor, &requirement.config)
}

fn validate_config(descriptor: &EvaluatorDescriptor, config: &Value) -> Result<(), EvaluatorError> {
    match descriptor.kind {
        EvaluatorKind::TokenBudget => positive_u64(descriptor, config, "max_tokens").map(|_| ()),
        EvaluatorKind::CostBudget => positive_u64(descriptor, config, "max_cost_cents").map(|_| ()),
        EvaluatorKind::TurnBudget => positive_u64(descriptor, config, "max_turns").map(|_| ()),
        EvaluatorKind::PrivacySafeOutput => max_sensitivity(descriptor, config).map(|_| ()),
        EvaluatorKind::TargetCompleted
        | EvaluatorKind::ResultProduced
        | EvaluatorKind::ScenarioQuality => Ok(()),
    }
}

fn positive_u64(
    descriptor: &EvaluatorDescriptor,
    config: &Value,
    key: &'static str,
) -> Result<u64, EvaluatorError> {
    let value = config
        .get(key)
        .ok_or_else(|| EvaluatorError::MissingConfigKey {
            evaluator_id: descriptor.id.to_string(),
            key,
        })?;
    value
        .as_u64()
        .filter(|value| *value > 0)
        .ok_or_else(|| EvaluatorError::InvalidConfigValue {
            evaluator_id: descriptor.id.to_string(),
            key,
            expectation: "a positive integer",
        })
}

fn max_sensitivity(
    descriptor: &EvaluatorDescriptor,
    config: &Value,
) -> Result<SensitivityClass, EvaluatorError> {
    let Some(value) = config.get("max_sensitivity") else {
        return Ok(SensitivityClass::None);
    };
    match value.as_str() {
        Some("none") => Ok(SensitivityClass::None),
        Some("pii") => Ok(SensitivityClass::Pii),
        _ => Err(EvaluatorError::InvalidConfigValue {
            evaluator_id: descriptor.id.to_string(),
            key: "max_sensitivity",
            expectation: "one of `none` or `pii`",
        }),
    }
}

/// Derives the deterministic score identity for one evaluator result.
///
/// Identity covers the score run, the evaluator and its exact version, the score
/// name, and the exact target. Two builds of the same evaluator at different
/// versions therefore write different rows instead of overwriting each other.
#[must_use]
pub fn derive_score_id(
    score_run_id: Uuid,
    evaluator_id: &str,
    evaluator_version: &str,
    score_name: &str,
    target: TrialScoreTarget,
) -> Uuid {
    let name = format!(
        "{SCORE_ID_DOMAIN}|{score_run_id}|{evaluator_id}|{evaluator_version}|{score_name}|{}",
        target.identity_fragment()
    );
    Uuid::new_v5(&SCORE_ID_NAMESPACE, name.as_bytes())
}

/// Runs every deterministic requirement in a scorecard against one trial's evidence.
///
/// # Errors
///
/// Returns [`EvaluatorError`] when a requirement fails registry validation or
/// when a stochastic evaluator is reached on the deterministic trial path.
pub fn evaluate_trial(
    scorecard: &ExperimentScorecard,
    score_run_id: Uuid,
    evidence: &TrialTerminalEvidence,
) -> Result<Vec<EvaluatedScore>, EvaluatorError> {
    let mut scores = Vec::with_capacity(scorecard.requirements().len());
    for requirement in scorecard.requirements() {
        validate_requirement(requirement)?;
        let descriptor = descriptor(&requirement.evaluator_id, &requirement.evaluator_version)?;
        if matches!(descriptor.determinism, EvaluatorDeterminism::Stochastic) {
            if requirement.effect.is_blocking() {
                return Err(EvaluatorError::StochasticBlocking {
                    evaluator_id: requirement.evaluator_id.clone(),
                });
            }
            // Informational stochastic judging runs nightly, outside the trial
            // path, so the trial finalizer emits nothing for it rather than
            // fabricating a deterministic stand-in.
            continue;
        }
        let passed = deterministic_result(descriptor, &requirement.config, evidence)?;
        scores.push(EvaluatedScore {
            score_id: derive_score_id(
                score_run_id,
                &requirement.evaluator_id,
                &requirement.evaluator_version,
                descriptor.score_name,
                evidence.target,
            ),
            evaluator_id: requirement.evaluator_id.clone(),
            evaluator_version: requirement.evaluator_version.clone(),
            score_name: descriptor.score_name.to_string(),
            value: EvaluatedValue::Boolean(passed),
            effect: requirement.effect,
            passed,
        });
    }
    Ok(scores)
}

fn deterministic_result(
    descriptor: &EvaluatorDescriptor,
    config: &Value,
    evidence: &TrialTerminalEvidence,
) -> Result<bool, EvaluatorError> {
    Ok(match descriptor.kind {
        EvaluatorKind::TargetCompleted => evidence.outcome.is_clean_completion(),
        EvaluatorKind::ResultProduced => evidence.produced_result(),
        EvaluatorKind::TokenBudget => {
            evidence.total_tokens <= positive_u64(descriptor, config, "max_tokens")?
        }
        EvaluatorKind::CostBudget => {
            evidence.total_cost_cents <= positive_u64(descriptor, config, "max_cost_cents")?
        }
        EvaluatorKind::TurnBudget => {
            u64::from(evidence.turn_count) <= positive_u64(descriptor, config, "max_turns")?
        }
        EvaluatorKind::PrivacySafeOutput => {
            privacy_safe(evidence, max_sensitivity(descriptor, config)?)
        }
        EvaluatorKind::ScenarioQuality => {
            return Err(EvaluatorError::StochasticOnTrialPath {
                evaluator_id: descriptor.id.to_string(),
            });
        }
    })
}

fn privacy_safe(evidence: &TrialTerminalEvidence, max: SensitivityClass) -> bool {
    let Some(output) = evidence.visible_output.as_deref() else {
        return true;
    };
    let result = classify_heuristic(output);
    // The heuristic classifier never abstains, but an abstention would mean the
    // classifier reached no conclusion, and "no conclusion" must not read as
    // "safe" on a privacy blocker.
    if result.abstained {
        return false;
    }
    sensitivity_rank(result.class) <= sensitivity_rank(max)
}

const fn sensitivity_rank(class: SensitivityClass) -> u8 {
    match class {
        SensitivityClass::None => 0,
        SensitivityClass::Pii => 1,
        SensitivityClass::Phi => 2,
        SensitivityClass::Restricted => 3,
    }
}

/// Returns the version of the deterministic PII classifier this build evaluates with.
#[must_use]
pub fn privacy_classifier_version() -> String {
    classify_heuristic("").model_version
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::TrialTerminalOutcome;
    use crate::model::ExperimentTrialStopReason;
    use moa_core::types::identifiers::SessionId;
    use serde_json::json;

    fn requirement(
        evaluator_id: &str,
        config: Value,
        effect: ScorecardEffect,
    ) -> ScorecardRequirement {
        ScorecardRequirement {
            evaluator_id: evaluator_id.to_string(),
            evaluator_version: "v1".to_string(),
            config,
            effect,
        }
    }

    fn blocking(evaluator_id: &str, config: Value) -> ScorecardRequirement {
        requirement(evaluator_id, config, ScorecardEffect::Blocking)
    }

    fn evidence() -> TrialTerminalEvidence {
        TrialTerminalEvidence {
            target: TrialScoreTarget::Session {
                session_id: SessionId(Uuid::from_u128(11)),
            },
            session_id: SessionId(Uuid::from_u128(11)),
            outcome: TrialTerminalOutcome::Completed,
            stop_reason: ExperimentTrialStopReason::SimulatorDone,
            turn_count: 4,
            total_tokens: 900,
            total_cost_cents: 3,
            latest_sequence_num: 20,
            visible_output: Some("your refund is on the way".to_string()),
            failure_code: None,
        }
    }

    #[test]
    fn every_registered_evaluator_has_a_unique_id_and_version_pair_offline() {
        // Pins: score identity includes evaluator id + version, so two registry
        // entries sharing that pair would make identity ambiguous.
        let mut seen = std::collections::BTreeSet::new();
        for descriptor in EVALUATORS {
            assert!(
                seen.insert((descriptor.id, descriptor.version)),
                "duplicate registry entry for {}@{}",
                descriptor.id,
                descriptor.version
            );
        }
    }

    #[test]
    fn unknown_evaluator_and_unknown_version_are_distinguished_offline() {
        // Pins: a typo'd evaluator and a real evaluator pinned at a version this
        // build cannot run are different operator problems and report differently.
        assert_eq!(
            descriptor("no_such_evaluator", "v1").expect_err("unknown evaluator"),
            EvaluatorError::UnknownEvaluator {
                evaluator_id: "no_such_evaluator".to_string(),
            }
        );
        assert_eq!(
            descriptor("target_completed", "v99").expect_err("unknown version"),
            EvaluatorError::UnknownEvaluatorVersion {
                evaluator_id: "target_completed".to_string(),
                version: "v99".to_string(),
            }
        );
    }

    #[test]
    fn stochastic_evaluator_cannot_be_declared_blocking_offline() {
        // Pins: scenario-quality judging stays nightly/informational. Marking it
        // blocking must be refused at validation, not silently gate deployments
        // on a model's mood.
        let scorecard = ExperimentScorecard::new(vec![requirement(
            "scenario_quality",
            json!({}),
            ScorecardEffect::Blocking,
        )])
        .expect("structurally valid");

        assert_eq!(
            validate_scorecard(&scorecard).expect_err("stochastic blocking must be refused"),
            EvaluatorError::StochasticBlocking {
                evaluator_id: "scenario_quality".to_string(),
            }
        );
    }

    #[test]
    fn budget_evaluators_reject_missing_and_unusable_thresholds_offline() {
        // Pins: a budget evaluator with no threshold, a zero threshold, or a
        // non-integer threshold is refused rather than silently passing every trial.
        assert_eq!(
            validate_requirement(&blocking("token_budget", json!({})))
                .expect_err("missing threshold"),
            EvaluatorError::MissingConfigKey {
                evaluator_id: "token_budget".to_string(),
                key: "max_tokens",
            }
        );
        for bad in [json!({"max_tokens": 0}), json!({"max_tokens": "lots"})] {
            assert_eq!(
                validate_requirement(&blocking("token_budget", bad))
                    .expect_err("unusable threshold"),
                EvaluatorError::InvalidConfigValue {
                    evaluator_id: "token_budget".to_string(),
                    key: "max_tokens",
                    expectation: "a positive integer",
                }
            );
        }
        assert_eq!(
            validate_requirement(&blocking(
                "privacy_safe_output",
                json!({"max_sensitivity": "whatever"})
            ))
            .expect_err("unusable sensitivity ceiling"),
            EvaluatorError::InvalidConfigValue {
                evaluator_id: "privacy_safe_output".to_string(),
                key: "max_sensitivity",
                expectation: "one of `none` or `pii`",
            }
        );
    }

    #[test]
    fn deterministic_evaluators_read_exact_terminal_evidence_offline() {
        // Pins: each initial evaluator reads the observation it claims to read —
        // completion, result production, the three budgets, and privacy — so a
        // trial that blew its token budget cannot pass the token requirement.
        let scorecard = ExperimentScorecard::new(vec![
            blocking("target_completed", json!({})),
            blocking("result_produced", json!({})),
            blocking("token_budget", json!({"max_tokens": 1000})),
            blocking("cost_budget", json!({"max_cost_cents": 5})),
            blocking("turn_budget", json!({"max_turns": 8})),
            blocking("privacy_safe_output", json!({})),
        ])
        .expect("structurally valid");
        let score_run_id = Uuid::from_u128(31);

        let passing = evaluate_trial(&scorecard, score_run_id, &evidence()).expect("evaluates");
        assert_eq!(passing.len(), 6);
        assert!(
            passing.iter().all(|score| score.passed),
            "clean evidence must satisfy every requirement: {passing:?}"
        );

        let mut blown = evidence();
        blown.outcome = TrialTerminalOutcome::ProviderFailure;
        blown.total_tokens = 1001;
        blown.total_cost_cents = 6;
        blown.turn_count = 9;
        blown.visible_output = Some("reach me at agent@example.com".to_string());
        let failing = evaluate_trial(&scorecard, score_run_id, &blown).expect("evaluates");
        let failed = failing
            .iter()
            .filter(|score| !score.passed)
            .map(|score| score.score_name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            failed,
            vec![
                "target_completed",
                "token_budget_respected",
                "cost_budget_respected",
                "turn_budget_respected",
                "privacy_safe_output",
            ]
        );
    }

    #[test]
    fn informational_stochastic_requirement_emits_no_trial_score_offline() {
        // Pins: nightly judging is not faked on the trial path. An informational
        // stochastic requirement is accepted by validation and produces nothing.
        let scorecard = ExperimentScorecard::new(vec![
            blocking("target_completed", json!({})),
            requirement(
                "scenario_quality",
                json!({}),
                ScorecardEffect::Informational,
            ),
        ])
        .expect("structurally valid");

        let scores =
            evaluate_trial(&scorecard, Uuid::from_u128(41), &evidence()).expect("evaluates");

        assert_eq!(scores.len(), 1);
        assert_eq!(scores[0].score_name, "target_completed");
    }

    #[test]
    fn score_identity_covers_run_evaluator_version_name_and_exact_target_offline() {
        // Pins: changing any identity component yields a different score row, so a
        // version bump cannot overwrite the previous version's result and a score
        // cannot silently attach to another target.
        let base = derive_score_id(
            Uuid::from_u128(1),
            "target_completed",
            "v1",
            "target_completed",
            TrialScoreTarget::Session {
                session_id: SessionId(Uuid::from_u128(2)),
            },
        );
        assert_eq!(
            base,
            derive_score_id(
                Uuid::from_u128(1),
                "target_completed",
                "v1",
                "target_completed",
                TrialScoreTarget::Session {
                    session_id: SessionId(Uuid::from_u128(2)),
                },
            ),
            "identity must be replay-stable"
        );
        for other in [
            derive_score_id(
                Uuid::from_u128(99),
                "target_completed",
                "v1",
                "target_completed",
                TrialScoreTarget::Session {
                    session_id: SessionId(Uuid::from_u128(2)),
                },
            ),
            derive_score_id(
                Uuid::from_u128(1),
                "result_produced",
                "v1",
                "target_completed",
                TrialScoreTarget::Session {
                    session_id: SessionId(Uuid::from_u128(2)),
                },
            ),
            derive_score_id(
                Uuid::from_u128(1),
                "target_completed",
                "v2",
                "target_completed",
                TrialScoreTarget::Session {
                    session_id: SessionId(Uuid::from_u128(2)),
                },
            ),
            derive_score_id(
                Uuid::from_u128(1),
                "target_completed",
                "v1",
                "result_produced",
                TrialScoreTarget::Session {
                    session_id: SessionId(Uuid::from_u128(2)),
                },
            ),
            derive_score_id(
                Uuid::from_u128(1),
                "target_completed",
                "v1",
                "target_completed",
                TrialScoreTarget::Session {
                    session_id: SessionId(Uuid::from_u128(3)),
                },
            ),
            derive_score_id(
                Uuid::from_u128(1),
                "target_completed",
                "v1",
                "target_completed",
                TrialScoreTarget::ExecutionRun {
                    execution_run_uid: Uuid::from_u128(2),
                },
            ),
        ] {
            assert_ne!(base, other, "identity component was not load-bearing");
        }
    }

    #[test]
    fn privacy_evaluator_uses_the_pinned_deterministic_classifier_version_offline() {
        // Pins: the privacy blocker runs MOA's deterministic heuristic classifier,
        // and a silent swap to a different classifier version is visible here.
        assert_eq!(privacy_classifier_version(), "moa-heuristic:v1");

        let mut secret = evidence();
        secret.visible_output = Some("token sk-live-abcdefghijklmnop".to_string());
        let scorecard = ExperimentScorecard::new(vec![blocking("privacy_safe_output", json!({}))])
            .expect("structurally valid");

        let scores = evaluate_trial(&scorecard, Uuid::from_u128(51), &secret).expect("evaluates");

        assert!(!scores[0].passed, "restricted output must fail the blocker");
    }
}
