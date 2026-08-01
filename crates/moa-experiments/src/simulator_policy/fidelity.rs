//! Predeclared per-domain fidelity bounds and the certification decision.
//!
//! There is no universal fidelity threshold in this module, and that is
//! deliberate. The research behind this surface establishes that a simulated user
//! is not a real user and that environment-constrained simulation narrows the
//! gap; it does not establish a single acceptable error rate, and the domain
//! samples it reports have overlapping confidence intervals. So every bound here
//! is supplied per domain by [`DomainFidelityBounds`], which has no `Default`
//! implementation and no constant fallbacks: an operator who wants a domain
//! certified must predeclare that domain's independent unit, minimum support,
//! class bounds, equivalence margin, and interval method first.
//!
//! The decision has three outcomes and their order matters:
//!
//! 1. Support is checked first. Missing measurements or cohorts below the
//!    predeclared minimum yield [`CertificationOutcome::Inconclusive`] — never a
//!    pass and never a fail, because an underpowered study has not measured the
//!    thing the bound is about.
//! 2. Bounds are checked next. Any violated bound yields
//!    [`CertificationOutcome::Failed`], with every violation reported rather than
//!    only the first.
//! 3. Only a study with sufficient support and no violated bound is certified,
//!    and only for the exact policy hash it measured.
//!
//! Every rate, effect, and margin is an exact integer: rates and probabilities in
//! permille, effects and margins in micro-units of the metric. That is not
//! cosmetic. The artifact is hashed with canonical JSON, which forbids
//! floating-point numbers precisely because their text form is not canonical, and
//! a predeclared bound that shifted when it round-tripped through storage would
//! not be a predeclaration. Confidence bounds are *computed* in floating point and
//! then floored to permille, which rounds against the policy under test.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use moa_artifacts::canonical::{canonical_hash, canonical_json_bytes};
use moa_artifacts::release::Digest32;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::simulator_policy::SimulatorPolicyError;
use crate::simulator_policy::registry::{
    CertificationWindow, CohortPin, ScenarioDomain, SimulatorPolicyComponents,
};

/// Parts in one whole, for permille-valued rates and probabilities.
pub const PERMILLE_DENOMINATOR: u32 = 1_000;

/// Micro-units in one whole, for micro-valued effects and margins.
pub const MICRO_DENOMINATOR: i64 = 1_000_000;

/// Supported one-sided confidence levels, in permille.
///
/// A closed set rather than an arbitrary level: the critical values below are
/// exact table values, so a study cannot ask for a confidence this build would
/// have to approximate.
pub const SUPPORTED_CONFIDENCE_PERMILLE: [u32; 4] = [900, 950, 975, 990];

/// One-sided standard-normal critical values for [`SUPPORTED_CONFIDENCE_PERMILLE`].
const CRITICAL_VALUES: [f64; 4] = [
    1.281_551_565_5,
    1.644_853_626_9,
    1.959_963_984_5,
    2.326_347_874_0,
];

/// The independent unit that supplies one observation in a fidelity study.
///
/// Support is counted in these units, not in transcripts or turns: several
/// conversations from one participant are correlated, and counting them
/// separately inflates support without adding evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndependentUnit {
    /// One consented human participant, however many interactions they produced.
    HumanParticipant,
    /// One human interaction, when participants contribute exactly one each.
    HumanInteraction,
    /// One tenant or account, when interactions cluster inside it.
    HumanAccount,
}

impl IndependentUnit {
    /// Returns the persisted representation for this unit.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HumanParticipant => "human_participant",
            Self::HumanInteraction => "human_interaction",
            Self::HumanAccount => "human_account",
        }
    }
}

/// The interval method a study is predeclared to use.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "method")]
pub enum IntervalMethod {
    /// Cluster bootstrap over independent units, percentile interval.
    ClusterBootstrapPercentile {
        /// Number of bootstrap resamples.
        resamples: u32,
        /// Seed the resampling used, so the interval is reproducible.
        seed: u64,
    },
    /// Student-t interval over per-unit means.
    StudentTOnUnitMeans,
}

/// Pin describing the predeclared power analysis behind a minimum support.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PowerAnalysisPin {
    /// Stable identifier of the predeclared analysis document.
    pub analysis_id: String,
    /// Digest over the exact predeclaration.
    pub analysis_hash: Digest32,
    /// Effect size the analysis was powered to detect, in micro-units.
    pub detectable_effect_micro: i64,
    /// Power the analysis targets, in permille.
    pub power_permille: u32,
}

/// Minimum independent support a domain requires before any bound is decided.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MinimumSupport {
    /// Independent units required in the policy-selection cohort.
    pub selection_units: u32,
    /// Independent units required in the untouched certification cohort.
    pub certification_units: u32,
    /// Independent units required per critical outcome class.
    pub per_critical_class_units: u32,
    /// Independent units required in each arm of the treatment-effect comparison.
    pub treatment_effect_units_per_arm: u32,
    /// Independent units required before a disagreement slice is bounded.
    pub per_slice_units: u32,
    /// The predeclared analysis these numbers came from.
    pub power_analysis: PowerAnalysisPin,
}

/// A critical outcome class and the lower bounds it must clear.
///
/// Bounds are on the *lower confidence bound* of the estimate, not on the point
/// estimate: a small cohort that happens to score perfectly does not clear a
/// bound its interval cannot support.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CriticalClassBound {
    /// Outcome class label, exactly as the label protocol emits it.
    pub class: String,
    /// Required lower confidence bound on sensitivity, in permille.
    pub min_sensitivity_lower_bound_permille: u32,
    /// Required lower confidence bound on specificity, in permille.
    pub min_specificity_lower_bound_permille: u32,
}

/// The equivalence bound for simulated-versus-human treatment-effect differences.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectEquivalenceBound {
    /// Largest simulated-minus-human difference treated as equivalent, in micro-units.
    pub margin_micro: i64,
    /// Interval method the study must use for the difference.
    pub method: IntervalMethod,
    /// Confidence level for the difference interval, in permille.
    pub confidence_permille: u32,
}

/// Everything one domain predeclares before a policy can be certified for it.
///
/// No `Default` implementation exists on purpose.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DomainFidelityBounds {
    /// Domain these bounds govern.
    pub domain: ScenarioDomain,
    /// Independent unit support is counted in.
    pub independent_unit: IndependentUnit,
    /// Minimum support from the predeclared power analysis.
    pub minimum_support: MinimumSupport,
    /// One-sided confidence level for class bounds, in permille.
    pub class_confidence_permille: u32,
    /// Critical outcome classes and their required lower bounds.
    pub critical_classes: Vec<CriticalClassBound>,
    /// Equivalence bound for the treatment-effect comparison.
    pub effect_equivalence: EffectEquivalenceBound,
    /// Largest tolerated per-slice disagreement, in permille, when bounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_slice_disagreement_permille: Option<u32>,
    /// How long a passing certification stays in force, in days.
    pub recertification_interval_days: u32,
}

impl DomainFidelityBounds {
    /// Rejects a predeclaration that cannot decide anything.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::InvalidBounds`] when a confidence level is
    /// unsupported, a required probability exceeds [`PERMILLE_DENOMINATOR`], the
    /// equivalence margin is not positive, a minimum support is zero, the critical
    /// class list is empty or repeats a class, or the recertification interval is
    /// zero.
    pub fn validate(&self) -> Result<(), SimulatorPolicyError> {
        critical_value(self.class_confidence_permille)?;
        critical_value(self.effect_equivalence.confidence_permille)?;
        if self.effect_equivalence.margin_micro <= 0 {
            return Err(SimulatorPolicyError::InvalidBounds {
                detail: "equivalence margin must be greater than zero".to_string(),
            });
        }
        if self.critical_classes.is_empty() {
            return Err(SimulatorPolicyError::InvalidBounds {
                detail: format!(
                    "domain `{}` declares no critical outcome class",
                    self.domain
                ),
            });
        }
        let mut seen = BTreeSet::new();
        for class in &self.critical_classes {
            if class.class.is_empty() || class.class.len() > 128 {
                return Err(SimulatorPolicyError::InvalidBounds {
                    detail: "critical class label must be 1..=128 characters".to_string(),
                });
            }
            if !seen.insert(class.class.as_str()) {
                return Err(SimulatorPolicyError::InvalidBounds {
                    detail: format!("critical class `{}` is declared twice", class.class),
                });
            }
            for (label, value) in [
                ("sensitivity", class.min_sensitivity_lower_bound_permille),
                ("specificity", class.min_specificity_lower_bound_permille),
            ] {
                if value > PERMILLE_DENOMINATOR {
                    return Err(SimulatorPolicyError::InvalidBounds {
                        detail: format!(
                            "class `{}` {label} lower bound {value} exceeds {PERMILLE_DENOMINATOR} permille",
                            class.class
                        ),
                    });
                }
            }
        }
        if let Some(slice_bound) = self.max_slice_disagreement_permille
            && slice_bound > PERMILLE_DENOMINATOR
        {
            return Err(SimulatorPolicyError::InvalidBounds {
                detail: format!(
                    "max slice disagreement {slice_bound} exceeds {PERMILLE_DENOMINATOR} permille"
                ),
            });
        }
        let support = &self.minimum_support;
        for (label, value) in [
            ("selection_units", support.selection_units),
            ("certification_units", support.certification_units),
            ("per_critical_class_units", support.per_critical_class_units),
            (
                "treatment_effect_units_per_arm",
                support.treatment_effect_units_per_arm,
            ),
            ("per_slice_units", support.per_slice_units),
        ] {
            if value == 0 {
                return Err(SimulatorPolicyError::InvalidBounds {
                    detail: format!("minimum support `{label}` must be at least 1"),
                });
            }
        }
        if support.power_analysis.analysis_id.is_empty() {
            return Err(SimulatorPolicyError::InvalidBounds {
                detail: "minimum support must name a predeclared power analysis".to_string(),
            });
        }
        if support.power_analysis.detectable_effect_micro <= 0 {
            return Err(SimulatorPolicyError::InvalidBounds {
                detail: "power analysis detectable effect must be positive".to_string(),
            });
        }
        if !(500..1_000).contains(&support.power_analysis.power_permille) {
            return Err(SimulatorPolicyError::InvalidBounds {
                detail: "power analysis power must be within 500..1000 permille".to_string(),
            });
        }
        if self.recertification_interval_days == 0 {
            return Err(SimulatorPolicyError::InvalidBounds {
                detail: "recertification interval must be at least one day".to_string(),
            });
        }
        Ok(())
    }

    /// Returns the bound declared for one class.
    #[must_use]
    pub fn class_bound(&self, class: &str) -> Option<&CriticalClassBound> {
        self.critical_classes
            .iter()
            .find(|bound| bound.class == class)
    }
}

/// Simulator-versus-human agreement for one outcome class.
///
/// Counts are at the independent-unit level declared by the domain bounds: one
/// unit contributes exactly one of the four cells. That is validated, so a study
/// cannot present clustered transcript counts as independent support.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClassAgreement {
    /// Outcome class this row is about.
    pub class: String,
    /// Units the human label and the simulated run both marked positive.
    pub true_positive: u32,
    /// Units the human label marked positive and the simulated run missed.
    pub false_negative: u32,
    /// Units both marked negative.
    pub true_negative: u32,
    /// Units the simulated run marked positive and the human label did not.
    pub false_positive: u32,
    /// Independent units contributing to this row.
    pub independent_units: u32,
}

impl ClassAgreement {
    /// Returns the number of units counted across the four cells.
    #[must_use]
    pub const fn counted_units(&self) -> u32 {
        self.true_positive
            .saturating_add(self.false_negative)
            .saturating_add(self.true_negative)
            .saturating_add(self.false_positive)
    }

    /// Rejects a row whose cells do not account for its declared units.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::InvalidMeasurement`] when the four cells
    /// do not total `independent_units`.
    pub fn validate(&self) -> Result<(), SimulatorPolicyError> {
        if self.counted_units() != self.independent_units {
            return Err(SimulatorPolicyError::InvalidMeasurement {
                detail: format!(
                    "class `{}` counts {} units across its cells but declares {} independent units",
                    self.class,
                    self.counted_units(),
                    self.independent_units
                ),
            });
        }
        Ok(())
    }

    /// Returns the one-sided Wilson lower bound on sensitivity, in permille.
    ///
    /// `None` when no unit carried a positive human label, because sensitivity is
    /// undefined rather than perfect in that case.
    #[must_use]
    pub fn sensitivity_lower_bound_permille(&self, confidence_permille: u32) -> Option<u32> {
        wilson_lower_bound_permille(
            self.true_positive,
            self.true_positive.saturating_add(self.false_negative),
            confidence_permille,
        )
    }

    /// Returns the one-sided Wilson lower bound on specificity, in permille.
    ///
    /// `None` when no unit carried a negative human label.
    #[must_use]
    pub fn specificity_lower_bound_permille(&self, confidence_permille: u32) -> Option<u32> {
        wilson_lower_bound_permille(
            self.true_negative,
            self.true_negative.saturating_add(self.false_positive),
            confidence_permille,
        )
    }
}

/// Simulator-versus-human disagreement on one failure slice.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DisagreementSlice {
    /// Failure slice label.
    pub slice: String,
    /// Rate observed in the simulated runs, in permille.
    pub simulated_rate_permille: u32,
    /// Rate observed in the human interactions, in permille.
    pub human_rate_permille: u32,
    /// Independent units contributing to this slice.
    pub independent_units: u32,
}

impl DisagreementSlice {
    /// Returns the absolute simulated-minus-human disagreement, in permille.
    #[must_use]
    pub const fn disagreement_permille(&self) -> u32 {
        self.simulated_rate_permille
            .abs_diff(self.human_rate_permille)
    }
}

/// A two-sided interval, at a declared confidence and method.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConfidenceInterval {
    /// Lower endpoint, in micro-units.
    pub low_micro: i64,
    /// Upper endpoint, in micro-units.
    pub high_micro: i64,
    /// Confidence level, in permille.
    pub confidence_permille: u32,
    /// Method used to compute the interval.
    pub method: IntervalMethod,
}

/// Whether simulated candidate-minus-baseline effects agree with real-user ones.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TreatmentEffectAgreement {
    /// Candidate-minus-baseline effect measured in simulation, in micro-units.
    pub simulated_effect_micro: i64,
    /// Candidate-minus-baseline effect measured in the real-user A/B, in micro-units.
    pub human_effect_micro: i64,
    /// Interval for `simulated_effect_micro - human_effect_micro`.
    pub difference_interval: ConfidenceInterval,
    /// Independent units in the simulated arm.
    pub simulated_units: u32,
    /// Independent units in the real-user arm.
    pub human_units: u32,
}

impl TreatmentEffectAgreement {
    /// Returns the simulated-minus-human effect difference, in micro-units.
    #[must_use]
    pub const fn difference_micro(&self) -> i64 {
        self.simulated_effect_micro
            .saturating_sub(self.human_effect_micro)
    }

    /// Returns whether both effects point the same way.
    ///
    /// Effects whose magnitude is inside the margin are treated as "no effect", so
    /// two indistinguishable-from-zero effects agree in direction regardless of
    /// their arithmetic sign.
    #[must_use]
    pub const fn agrees_in_direction(&self, margin_micro: i64) -> bool {
        sign_within(self.simulated_effect_micro, margin_micro)
            == sign_within(self.human_effect_micro, margin_micro)
    }

    /// Returns whether the difference interval fits strictly inside the margin.
    #[must_use]
    pub const fn agrees_in_magnitude(&self, margin_micro: i64) -> bool {
        self.difference_interval.low_micro > -margin_micro
            && self.difference_interval.high_micro < margin_micro
    }
}

/// How outcome labels for the human cohort were produced.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelAdjudication {
    /// Independent annotators with disagreements adjudicated by a third.
    IndependentWithAdjudication,
    /// A single annotator per interaction.
    SingleAnnotator,
    /// Deterministic program labels derived from the recorded environment state.
    DeterministicFromEnvironment,
}

/// The exact labelling procedure the study's ground truth came from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LabelProtocolPin {
    /// Stable protocol identifier.
    pub protocol_id: String,
    /// Exact protocol version.
    pub version: u32,
    /// Digest over the exact rubric.
    pub rubric_hash: Digest32,
    /// How labels were adjudicated.
    pub adjudication: LabelAdjudication,
    /// Number of annotators per interaction.
    pub annotators: u32,
    /// Inter-annotator agreement in permille, when more than one annotator ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement_permille: Option<u32>,
}

/// What one fidelity study was allowed to spend and what it did spend.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct FidelityStudyCost {
    /// Explicit budget authorized for the study, in micro-USD.
    pub budget_micro_usd: u64,
    /// Provider spend the study actually incurred, in micro-USD.
    pub spent_micro_usd: u64,
    /// Simulator model calls the study issued.
    pub simulator_calls: u64,
    /// Independent human units the study consumed.
    pub human_units_consumed: u32,
}

/// Authorization to use the human interaction cohorts at all.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HumanDataAuthorization {
    /// Stable authorization record identifier.
    pub authorization_id: String,
    /// Identity that approved the use.
    pub approved_by: String,
    /// When the use was approved.
    pub approved_at: DateTime<Utc>,
    /// Instant after which the authorization no longer permits use.
    pub expires_at: DateTime<Utc>,
}

impl HumanDataAuthorization {
    /// Returns whether the authorization permits use at `now`.
    #[must_use]
    pub fn permits(&self, now: DateTime<Utc>) -> bool {
        now >= self.approved_at && now < self.expires_at
    }
}

/// Current fidelity-study artifact schema version.
pub const FIDELITY_ARTIFACT_VERSION: u16 = 1;

/// The auditable record of one fidelity study.
///
/// Everything a reader needs to re-decide the certification is here: both
/// cohorts, the independent unit and support, the exact simulator components
/// (model, provider, decoding, prompt hash, protocol), the label protocol, the
/// policy hash under study, the predeclared bounds, the measured agreement with
/// its uncertainty, and the cost. Nothing here contains transcript text; cohorts
/// are referenced by content hash.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FidelityStudyArtifact {
    /// Artifact schema version, inside the digest.
    pub artifact_version: u16,
    /// Stable study identifier.
    pub study_uid: Uuid,
    /// Policy under study.
    pub policy_uid: Uuid,
    /// Policy revision under study.
    pub policy_revision: i32,
    /// Exact policy hash under study.
    pub policy_hash: Digest32,
    /// Simulator components as measured, pinning the model and prompt.
    pub simulator_components: SimulatorPolicyComponents,
    /// Domain the study certifies for.
    pub domain: ScenarioDomain,
    /// Predeclared acceptance bounds.
    pub bounds: DomainFidelityBounds,
    /// Cohort used to pick and tune the policy.
    pub selection_cohort: CohortPin,
    /// Untouched cohort the certification decision is made on.
    pub certification_cohort: CohortPin,
    /// Labelling procedure behind the human ground truth.
    pub label_protocol: LabelProtocolPin,
    /// Class-specific agreement measured on the certification cohort.
    pub class_agreement: Vec<ClassAgreement>,
    /// Disagreement by failure slice.
    #[serde(default)]
    pub disagreement_slices: Vec<DisagreementSlice>,
    /// Simulated-versus-human treatment-effect agreement, when measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_agreement: Option<TreatmentEffectAgreement>,
    /// Budget and spend.
    pub cost: FidelityStudyCost,
    /// Human-data authorization the study ran under.
    pub authorization: HumanDataAuthorization,
    /// When the measurements were taken.
    pub observed_at: DateTime<Utc>,
}

impl FidelityStudyArtifact {
    /// Returns the canonical digest over the whole artifact.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::NotCanonicalizable`] when the artifact
    /// cannot be canonically serialized.
    pub fn digest(&self) -> Result<Digest32, SimulatorPolicyError> {
        canonical_hash(self).map(Digest32).map_err(|error| {
            SimulatorPolicyError::NotCanonicalizable {
                detail: error.to_string(),
            }
        })
    }

    /// Returns the canonical JSON bytes the digest is taken over.
    ///
    /// Storage keeps these bytes verbatim rather than a re-serialized value, so a
    /// loaded artifact hashes to the digest it was stored with.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::NotCanonicalizable`] when the artifact
    /// cannot be canonically serialized.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, SimulatorPolicyError> {
        canonical_json_bytes(self).map_err(|error| SimulatorPolicyError::NotCanonicalizable {
            detail: error.to_string(),
        })
    }

    /// Rejects an artifact that is internally inconsistent.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] when the schema version is wrong, the
    /// bounds or cohorts are invalid, the bounds are for a different domain, a
    /// class row does not account for its units, a rate is not a permille
    /// proportion, or an interval is inverted.
    pub fn validate(&self) -> Result<(), SimulatorPolicyError> {
        if self.artifact_version != FIDELITY_ARTIFACT_VERSION {
            return Err(SimulatorPolicyError::InvalidMeasurement {
                detail: format!(
                    "fidelity artifact version {} is not {FIDELITY_ARTIFACT_VERSION}",
                    self.artifact_version
                ),
            });
        }
        self.bounds.validate()?;
        if self.bounds.domain != self.domain {
            return Err(SimulatorPolicyError::InvalidMeasurement {
                detail: format!(
                    "study domain `{}` does not match the bounds domain `{}`",
                    self.domain, self.bounds.domain
                ),
            });
        }
        if self.simulator_components.domain != self.domain {
            return Err(SimulatorPolicyError::InvalidMeasurement {
                detail: format!(
                    "study domain `{}` does not match the simulator components domain `{}`",
                    self.domain, self.simulator_components.domain
                ),
            });
        }
        self.selection_cohort.validate()?;
        self.certification_cohort.validate()?;
        if self.label_protocol.protocol_id.is_empty() || self.label_protocol.annotators == 0 {
            return Err(SimulatorPolicyError::InvalidMeasurement {
                detail: "label protocol must name an id and at least one annotator".to_string(),
            });
        }
        let mut seen = BTreeSet::new();
        for row in &self.class_agreement {
            row.validate()?;
            if !seen.insert(row.class.as_str()) {
                return Err(SimulatorPolicyError::InvalidMeasurement {
                    detail: format!("class `{}` is measured twice", row.class),
                });
            }
        }
        for slice in &self.disagreement_slices {
            for (label, value) in [
                ("simulated", slice.simulated_rate_permille),
                ("human", slice.human_rate_permille),
            ] {
                if value > PERMILLE_DENOMINATOR {
                    return Err(SimulatorPolicyError::InvalidMeasurement {
                        detail: format!(
                            "slice `{}` {label} rate {value} exceeds {PERMILLE_DENOMINATOR} permille",
                            slice.slice
                        ),
                    });
                }
            }
        }
        if let Some(effect) = &self.effect_agreement
            && effect.difference_interval.low_micro > effect.difference_interval.high_micro
        {
            return Err(SimulatorPolicyError::InvalidMeasurement {
                detail: "treatment-effect difference interval is inverted".to_string(),
            });
        }
        Ok(())
    }

    /// Returns the measured uncertainty the artifact pins.
    ///
    /// This is what makes "the artifact pins uncertainty" checkable: the
    /// per-class lower confidence bounds derived from the unit-level counts, plus
    /// the treatment-effect difference interval, at the predeclared confidence
    /// levels and interval method.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::InvalidBounds`] when the declared
    /// confidence level is not one this build has an exact critical value for.
    pub fn uncertainty(&self) -> Result<FidelityUncertainty, SimulatorPolicyError> {
        critical_value(self.bounds.class_confidence_permille)?;
        let class_bounds = self
            .class_agreement
            .iter()
            .map(|row| ClassUncertainty {
                class: row.class.clone(),
                independent_units: row.independent_units,
                sensitivity_lower_bound_permille: row
                    .sensitivity_lower_bound_permille(self.bounds.class_confidence_permille),
                specificity_lower_bound_permille: row
                    .specificity_lower_bound_permille(self.bounds.class_confidence_permille),
            })
            .collect();
        Ok(FidelityUncertainty {
            class_confidence_permille: self.bounds.class_confidence_permille,
            class_bounds,
            effect_difference_interval: self
                .effect_agreement
                .as_ref()
                .map(|effect| effect.difference_interval),
        })
    }

    /// Decides certification for the policy this study measured.
    ///
    /// Support first, then bounds. See the module documentation for why that
    /// order is load-bearing.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError`] when the artifact itself is invalid, so a
    /// malformed study cannot be reported as a clean `Failed` or `Inconclusive`.
    pub fn certify(
        &self,
        now: DateTime<Utc>,
    ) -> Result<CertificationOutcome, SimulatorPolicyError> {
        self.validate()?;
        let gaps = self.support_gaps();
        if !gaps.is_empty() {
            return Ok(CertificationOutcome::Inconclusive { gaps });
        }
        let violations = self.bound_violations(now)?;
        if !violations.is_empty() {
            return Ok(CertificationOutcome::Failed { violations });
        }
        let interval_days = i64::from(self.bounds.recertification_interval_days);
        let certified_until = now
            .checked_add_signed(Duration::days(interval_days))
            .ok_or_else(|| SimulatorPolicyError::InvalidBounds {
                detail: "recertification interval overflows the certification window".to_string(),
            })?;
        Ok(CertificationOutcome::Certified {
            window: CertificationWindow {
                study_uid: self.study_uid,
                study_artifact_hash: self.digest()?,
                certified_policy_hash: self.policy_hash,
                certified_from: now,
                certified_until,
            },
            uncertainty: self.uncertainty()?,
        })
    }

    /// Returns every predeclared support requirement the study did not meet.
    #[must_use]
    pub fn support_gaps(&self) -> Vec<SupportGap> {
        let support = &self.bounds.minimum_support;
        let mut gaps = Vec::new();
        if self.selection_cohort.independent_units < support.selection_units {
            gaps.push(SupportGap::SelectionCohortUnderpowered {
                observed: self.selection_cohort.independent_units,
                required: support.selection_units,
            });
        }
        if self.certification_cohort.independent_units < support.certification_units {
            gaps.push(SupportGap::CertificationCohortUnderpowered {
                observed: self.certification_cohort.independent_units,
                required: support.certification_units,
            });
        }
        for declared in &self.bounds.critical_classes {
            match self
                .class_agreement
                .iter()
                .find(|row| row.class == declared.class)
            {
                None => gaps.push(SupportGap::CriticalClassUnmeasured {
                    class: declared.class.clone(),
                }),
                Some(row) => {
                    if row.independent_units < support.per_critical_class_units {
                        gaps.push(SupportGap::CriticalClassUnderpowered {
                            class: declared.class.clone(),
                            observed: row.independent_units,
                            required: support.per_critical_class_units,
                        });
                    }
                    if row.true_positive.saturating_add(row.false_negative) == 0 {
                        gaps.push(SupportGap::CriticalClassHasNoPositiveLabels {
                            class: declared.class.clone(),
                        });
                    }
                    if row.true_negative.saturating_add(row.false_positive) == 0 {
                        gaps.push(SupportGap::CriticalClassHasNoNegativeLabels {
                            class: declared.class.clone(),
                        });
                    }
                }
            }
        }
        match &self.effect_agreement {
            None => gaps.push(SupportGap::TreatmentEffectUnmeasured),
            Some(effect) => {
                for (arm, observed) in [
                    ("simulated", effect.simulated_units),
                    ("human", effect.human_units),
                ] {
                    if observed < support.treatment_effect_units_per_arm {
                        gaps.push(SupportGap::TreatmentEffectArmUnderpowered {
                            arm: arm.to_string(),
                            observed,
                            required: support.treatment_effect_units_per_arm,
                        });
                    }
                }
            }
        }
        if self.bounds.max_slice_disagreement_permille.is_some() {
            for slice in &self.disagreement_slices {
                if slice.independent_units < support.per_slice_units {
                    gaps.push(SupportGap::SliceUnderpowered {
                        slice: slice.slice.clone(),
                        observed: slice.independent_units,
                        required: support.per_slice_units,
                    });
                }
            }
        }
        gaps
    }

    /// Returns every certification bound the study violated.
    ///
    /// # Errors
    ///
    /// Returns [`SimulatorPolicyError::InvalidBounds`] when a declared confidence
    /// level has no exact critical value in this build.
    pub fn bound_violations(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<BoundViolation>, SimulatorPolicyError> {
        let confidence = self.bounds.class_confidence_permille;
        critical_value(confidence)?;
        let mut violations = Vec::new();

        if self.selection_cohort.cohort_id == self.certification_cohort.cohort_id
            || self.selection_cohort.content_hash == self.certification_cohort.content_hash
        {
            violations.push(BoundViolation::CohortsNotIndependent {
                cohort_id: self.certification_cohort.cohort_id.clone(),
            });
        }
        if self.cost.spent_micro_usd > self.cost.budget_micro_usd {
            violations.push(BoundViolation::BudgetExceeded {
                spent_micro_usd: self.cost.spent_micro_usd,
                budget_micro_usd: self.cost.budget_micro_usd,
            });
        }
        if !self.authorization.permits(self.observed_at) {
            violations.push(BoundViolation::HumanDataAuthorizationInvalid {
                authorization_id: self.authorization.authorization_id.clone(),
            });
        }
        if !self.simulator_components.validity.contains(now) {
            violations.push(BoundViolation::PolicyOutsideValidityWindow {
                valid_until: self.simulator_components.validity.valid_until,
            });
        }

        for declared in &self.bounds.critical_classes {
            let Some(row) = self
                .class_agreement
                .iter()
                .find(|row| row.class == declared.class)
            else {
                continue;
            };
            if let Some(observed) = row.sensitivity_lower_bound_permille(confidence)
                && observed < declared.min_sensitivity_lower_bound_permille
            {
                violations.push(BoundViolation::SensitivityBelowBound {
                    class: declared.class.clone(),
                    observed_lower_bound_permille: observed,
                    required_lower_bound_permille: declared.min_sensitivity_lower_bound_permille,
                });
            }
            if let Some(observed) = row.specificity_lower_bound_permille(confidence)
                && observed < declared.min_specificity_lower_bound_permille
            {
                violations.push(BoundViolation::SpecificityBelowBound {
                    class: declared.class.clone(),
                    observed_lower_bound_permille: observed,
                    required_lower_bound_permille: declared.min_specificity_lower_bound_permille,
                });
            }
        }

        if let Some(limit) = self.bounds.max_slice_disagreement_permille {
            for slice in &self.disagreement_slices {
                if slice.disagreement_permille() > limit {
                    violations.push(BoundViolation::SliceDisagreementAboveBound {
                        slice: slice.slice.clone(),
                        observed_permille: slice.disagreement_permille(),
                        limit_permille: limit,
                    });
                }
            }
        }

        if let Some(effect) = &self.effect_agreement {
            let equivalence = self.bounds.effect_equivalence;
            if effect.difference_interval.confidence_permille != equivalence.confidence_permille
                || effect.difference_interval.method != equivalence.method
            {
                violations.push(BoundViolation::IntervalMethodNotPredeclared {
                    declared: equivalence.method,
                    declared_confidence_permille: equivalence.confidence_permille,
                    observed: effect.difference_interval.method,
                    observed_confidence_permille: effect.difference_interval.confidence_permille,
                });
            }
            if !effect.agrees_in_direction(equivalence.margin_micro) {
                violations.push(BoundViolation::EffectDirectionDisagrees {
                    simulated_effect_micro: effect.simulated_effect_micro,
                    human_effect_micro: effect.human_effect_micro,
                });
            }
            if !effect.agrees_in_magnitude(equivalence.margin_micro) {
                violations.push(BoundViolation::EffectNotEquivalent {
                    interval_low_micro: effect.difference_interval.low_micro,
                    interval_high_micro: effect.difference_interval.high_micro,
                    margin_micro: equivalence.margin_micro,
                });
            }
        }

        Ok(violations)
    }
}

/// Uncertainty for one measured class.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClassUncertainty {
    /// Outcome class.
    pub class: String,
    /// Independent units behind the estimate.
    pub independent_units: u32,
    /// Lower confidence bound on sensitivity in permille, when positives exist.
    pub sensitivity_lower_bound_permille: Option<u32>,
    /// Lower confidence bound on specificity in permille, when negatives exist.
    pub specificity_lower_bound_permille: Option<u32>,
}

/// The uncertainty a fidelity artifact pins.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FidelityUncertainty {
    /// Confidence level the class bounds are computed at, in permille.
    pub class_confidence_permille: u32,
    /// Per-class lower confidence bounds.
    pub class_bounds: Vec<ClassUncertainty>,
    /// Interval for the simulated-minus-human effect difference, when measured.
    pub effect_difference_interval: Option<ConfidenceInterval>,
}

/// A predeclared support requirement the study did not meet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "gap", rename_all = "snake_case")]
pub enum SupportGap {
    /// The policy-selection cohort is below the predeclared minimum.
    SelectionCohortUnderpowered {
        /// Independent units observed.
        observed: u32,
        /// Independent units required.
        required: u32,
    },
    /// The certification cohort is below the predeclared minimum.
    CertificationCohortUnderpowered {
        /// Independent units observed.
        observed: u32,
        /// Independent units required.
        required: u32,
    },
    /// A declared critical class was never measured.
    CriticalClassUnmeasured {
        /// Class the bounds declare.
        class: String,
    },
    /// A critical class has fewer independent units than required.
    CriticalClassUnderpowered {
        /// Class the bounds declare.
        class: String,
        /// Independent units observed.
        observed: u32,
        /// Independent units required.
        required: u32,
    },
    /// A critical class has no positively labelled unit, so sensitivity is undefined.
    CriticalClassHasNoPositiveLabels {
        /// Class the bounds declare.
        class: String,
    },
    /// A critical class has no negatively labelled unit, so specificity is undefined.
    CriticalClassHasNoNegativeLabels {
        /// Class the bounds declare.
        class: String,
    },
    /// The simulated-versus-human treatment-effect comparison is missing.
    TreatmentEffectUnmeasured,
    /// One arm of the treatment-effect comparison is underpowered.
    TreatmentEffectArmUnderpowered {
        /// Arm name.
        arm: String,
        /// Independent units observed.
        observed: u32,
        /// Independent units required.
        required: u32,
    },
    /// A bounded disagreement slice has too few units to bound.
    SliceUnderpowered {
        /// Failure slice label.
        slice: String,
        /// Independent units observed.
        observed: u32,
        /// Independent units required.
        required: u32,
    },
}

/// A certification bound the study violated.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "violation", rename_all = "snake_case")]
pub enum BoundViolation {
    /// The certification cohort is the selection cohort, so it is not untouched.
    CohortsNotIndependent {
        /// Cohort identifier that repeated.
        cohort_id: String,
    },
    /// The study spent more than its authorized budget.
    BudgetExceeded {
        /// Micro-USD spent.
        spent_micro_usd: u64,
        /// Micro-USD authorized.
        budget_micro_usd: u64,
    },
    /// The human-data authorization did not permit use when the study ran.
    HumanDataAuthorizationInvalid {
        /// Authorization identifier.
        authorization_id: String,
    },
    /// The policy's own validity window had lapsed at decision time.
    PolicyOutsideValidityWindow {
        /// End of the policy validity window.
        valid_until: DateTime<Utc>,
    },
    /// A critical class missed its sensitivity lower bound.
    SensitivityBelowBound {
        /// Outcome class.
        class: String,
        /// Observed lower confidence bound, in permille.
        observed_lower_bound_permille: u32,
        /// Required lower confidence bound, in permille.
        required_lower_bound_permille: u32,
    },
    /// A critical class missed its specificity lower bound.
    SpecificityBelowBound {
        /// Outcome class.
        class: String,
        /// Observed lower confidence bound, in permille.
        observed_lower_bound_permille: u32,
        /// Required lower confidence bound, in permille.
        required_lower_bound_permille: u32,
    },
    /// A failure slice disagreed more than the domain tolerates.
    SliceDisagreementAboveBound {
        /// Failure slice label.
        slice: String,
        /// Observed absolute disagreement, in permille.
        observed_permille: u32,
        /// Declared limit, in permille.
        limit_permille: u32,
    },
    /// The reported interval was not computed the predeclared way.
    IntervalMethodNotPredeclared {
        /// Predeclared method.
        declared: IntervalMethod,
        /// Predeclared confidence, in permille.
        declared_confidence_permille: u32,
        /// Method the study reported.
        observed: IntervalMethod,
        /// Confidence the study reported, in permille.
        observed_confidence_permille: u32,
    },
    /// Simulated and real-user effects point in different directions.
    EffectDirectionDisagrees {
        /// Simulated candidate-minus-baseline effect, in micro-units.
        simulated_effect_micro: i64,
        /// Real-user candidate-minus-baseline effect, in micro-units.
        human_effect_micro: i64,
    },
    /// The simulated-minus-human difference interval leaves the equivalence margin.
    EffectNotEquivalent {
        /// Interval lower endpoint, in micro-units.
        interval_low_micro: i64,
        /// Interval upper endpoint, in micro-units.
        interval_high_micro: i64,
        /// Declared equivalence margin, in micro-units.
        margin_micro: i64,
    },
}

/// The decision a fidelity study produces.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CertificationOutcome {
    /// Every bound met on sufficient support; the window is in force.
    Certified {
        /// Window opened for the exact measured policy hash.
        window: CertificationWindow,
        /// Uncertainty behind the decision.
        uncertainty: FidelityUncertainty,
    },
    /// Support was sufficient and at least one bound was violated.
    Failed {
        /// Every violated bound, not only the first.
        violations: Vec<BoundViolation>,
    },
    /// Support was insufficient, so no bound could be decided.
    Inconclusive {
        /// Every unmet support requirement.
        gaps: Vec<SupportGap>,
    },
}

impl CertificationOutcome {
    /// Returns the persisted verdict discriminator.
    #[must_use]
    pub const fn verdict(&self) -> &'static str {
        match self {
            Self::Certified { .. } => "certified",
            Self::Failed { .. } => "failed",
            Self::Inconclusive { .. } => "inconclusive",
        }
    }

    /// Returns the certification window when the study passed.
    #[must_use]
    pub const fn window(&self) -> Option<&CertificationWindow> {
        match self {
            Self::Certified { window, .. } => Some(window),
            Self::Failed { .. } | Self::Inconclusive { .. } => None,
        }
    }
}

/// Returns the exact one-sided critical value for a supported confidence level.
///
/// # Errors
///
/// Returns [`SimulatorPolicyError::InvalidBounds`] for any level this build has
/// no exact critical value for.
pub fn critical_value(confidence_permille: u32) -> Result<f64, SimulatorPolicyError> {
    SUPPORTED_CONFIDENCE_PERMILLE
        .iter()
        .position(|supported| *supported == confidence_permille)
        .and_then(|index| CRITICAL_VALUES.get(index).copied())
        .ok_or(SimulatorPolicyError::InvalidBounds {
            detail: format!(
                "confidence {confidence_permille} permille is not one of {SUPPORTED_CONFIDENCE_PERMILLE:?}"
            ),
        })
}

/// Returns the one-sided Wilson score lower bound, floored to permille.
///
/// Flooring rounds against the policy under test, so a bound is never cleared by
/// rounding. `None` when there are no trials: a proportion over an empty
/// denominator is undefined, and reporting it as zero would turn missing evidence
/// into a bound violation instead of insufficient support.
#[must_use]
pub fn wilson_lower_bound_permille(
    successes: u32,
    trials: u32,
    confidence_permille: u32,
) -> Option<u32> {
    if trials == 0 || successes > trials {
        return None;
    }
    let critical = critical_value(confidence_permille).ok()?;
    let n = f64::from(trials);
    let observed = f64::from(successes) / n;
    let z_squared = critical * critical;
    let denominator = 1.0 + z_squared / n;
    let center = observed + z_squared / (2.0 * n);
    let spread = critical * (observed * (1.0 - observed) / n + z_squared / (4.0 * n * n)).sqrt();
    let lower = ((center - spread) / denominator).clamp(0.0, 1.0);
    // `lower` is clamped to 0.0..=1.0, so the scaled value fits a `u32`.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let permille = (lower * f64::from(PERMILLE_DENOMINATOR)).floor() as u32;
    Some(permille)
}

/// Returns the sign of an effect, treating magnitudes inside the margin as zero.
const fn sign_within(effect_micro: i64, margin_micro: i64) -> i8 {
    if effect_micro.saturating_abs() <= margin_micro {
        0
    } else if effect_micro > 0 {
        1
    } else {
        -1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulator_policy::test_support::{
        at, bounds, certification_cohort, passing_artifact,
    };

    // Pins: the Wilson lower bound sits below the point estimate, tightens with
    // support, loosens with confidence, and is undefined rather than perfect on an
    // empty denominator.
    #[test]
    fn wilson_lower_bound_properties_offline() {
        assert_eq!(wilson_lower_bound_permille(0, 0, 950), None);
        assert_eq!(wilson_lower_bound_permille(5, 4, 950), None);

        let small = wilson_lower_bound_permille(10, 10, 950).expect("10/10 has a bound");
        let large = wilson_lower_bound_permille(200, 200, 950).expect("200/200 has a bound");
        assert!(small < 1_000, "a perfect small sample cannot reach 1.0");
        assert!(
            large > small,
            "more independent support must tighten the bound: {large} vs {small}"
        );

        let bound = wilson_lower_bound_permille(90, 100, 950).expect("90/100 has a bound");
        assert!(
            bound < 900,
            "the lower bound must sit below the 900 permille estimate, got {bound}"
        );

        let tighter = wilson_lower_bound_permille(90, 100, 900).expect("90% confidence");
        let looser = wilson_lower_bound_permille(90, 100, 990).expect("99% confidence");
        assert!(
            tighter > looser,
            "a higher confidence level must produce a lower bound"
        );

        let zero = wilson_lower_bound_permille(0, 50, 950).expect("zero successes has a bound");
        assert!(zero < 50, "zero successes bound {zero}");
    }

    // Pins: an unsupported confidence level is refused rather than approximated.
    #[test]
    fn unsupported_confidence_level_is_refused_offline() {
        assert!(critical_value(950).is_ok());
        assert!(critical_value(960).is_err());
        assert!(critical_value(0).is_err());
    }

    // Pins: a complete, adequately powered, in-bounds study certifies the exact
    // policy hash it measured and nothing else.
    #[test]
    fn passing_study_certifies_the_measured_policy_hash_offline() {
        let artifact = passing_artifact();
        let outcome = artifact
            .certify(at(1_500_000))
            .expect("a valid artifact must produce an outcome");
        let window = match &outcome {
            CertificationOutcome::Certified { window, .. } => window,
            other => panic!("expected certification, got {other:?}"),
        };
        assert_eq!(window.certified_policy_hash, artifact.policy_hash);
        assert_eq!(window.study_uid, artifact.study_uid);
        assert_eq!(window.certified_from, at(1_500_000));
        assert_eq!(
            window.certified_until - window.certified_from,
            Duration::days(i64::from(artifact.bounds.recertification_interval_days))
        );
        assert_eq!(outcome.verdict(), "certified");
    }

    // Pins: insufficient support is INCONCLUSIVE, never a pass and never a fail,
    // even when the measured point estimates are perfect.
    #[test]
    fn insufficient_support_is_inconclusive_not_failed_offline() {
        let mut artifact = passing_artifact();
        artifact.certification_cohort.independent_units = 10;
        let row = artifact
            .class_agreement
            .first_mut()
            .expect("one measured class");
        row.true_positive = 5;
        row.false_negative = 0;
        row.true_negative = 5;
        row.false_positive = 0;
        row.independent_units = 10;

        let outcome = artifact.certify(at(1_500_000)).expect("outcome");
        let gaps = match &outcome {
            CertificationOutcome::Inconclusive { gaps } => gaps,
            other => panic!("expected inconclusive, got {other:?}"),
        };
        assert!(
            gaps.contains(&SupportGap::CertificationCohortUnderpowered {
                observed: 10,
                required: artifact.bounds.minimum_support.certification_units,
            }),
            "gaps {gaps:?}"
        );
        assert_eq!(outcome.verdict(), "inconclusive");
        assert!(outcome.window().is_none());
    }

    // Pins: a missing treatment-effect comparison is insufficient support rather
    // than a silent pass, so a study that never compared against real users
    // cannot certify.
    #[test]
    fn missing_treatment_effect_comparison_is_inconclusive_offline() {
        let mut artifact = passing_artifact();
        artifact.effect_agreement = None;
        let outcome = artifact.certify(at(1_500_000)).expect("outcome");
        match outcome {
            CertificationOutcome::Inconclusive { gaps } => {
                assert!(
                    gaps.contains(&SupportGap::TreatmentEffectUnmeasured),
                    "{gaps:?}"
                );
            }
            other => panic!("expected inconclusive, got {other:?}"),
        }
    }

    // Pins: a sensitivity lower bound below the domain's declared floor fails
    // certification and names the observed and required values.
    #[test]
    fn sensitivity_below_bound_fails_certification_offline() {
        let mut artifact = passing_artifact();
        let row = artifact
            .class_agreement
            .first_mut()
            .expect("one measured class");
        // Keep the unit accounting intact: the same 116 positively labelled units,
        // most of them now missed by the simulator.
        row.true_positive = 60;
        row.false_negative = 56;

        let outcome = artifact.certify(at(1_500_000)).expect("outcome");
        let violations = match &outcome {
            CertificationOutcome::Failed { violations } => violations,
            other => panic!("expected failure, got {other:?}"),
        };
        assert!(
            violations.iter().any(|violation| matches!(
                violation,
                BoundViolation::SensitivityBelowBound { class, .. } if class == "handoff_required"
            )),
            "violations {violations:?}"
        );
        assert_eq!(outcome.verdict(), "failed");
    }

    // Pins: reusing the selection cohort as the certification cohort fails, so a
    // policy cannot be certified on the data it was tuned on. Support is held
    // constant so the failure cannot be confused with an underpowered study.
    #[test]
    fn reusing_the_selection_cohort_fails_certification_offline() {
        let mut same_id = passing_artifact();
        same_id.certification_cohort.cohort_id = same_id.selection_cohort.cohort_id.clone();
        match same_id.certify(at(1_500_000)).expect("outcome") {
            CertificationOutcome::Failed { violations } => assert!(
                violations.iter().any(|violation| matches!(
                    violation,
                    BoundViolation::CohortsNotIndependent { .. }
                )),
                "{violations:?}"
            ),
            other => panic!("expected failure, got {other:?}"),
        }

        // Same content under a different id is still the same data.
        let mut same_content = passing_artifact();
        same_content.certification_cohort.content_hash = same_content.selection_cohort.content_hash;
        match same_content.certify(at(1_500_000)).expect("outcome") {
            CertificationOutcome::Failed { violations } => assert!(
                violations.iter().any(|violation| matches!(
                    violation,
                    BoundViolation::CohortsNotIndependent { .. }
                )),
                "{violations:?}"
            ),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    // Pins: an effect that agrees in direction but not in magnitude still fails,
    // and an effect that points the other way fails on direction.
    #[test]
    fn effect_agreement_requires_direction_and_magnitude_offline() {
        let mut wide = passing_artifact();
        if let Some(effect) = wide.effect_agreement.as_mut() {
            effect.difference_interval.low_micro = -90_000;
            effect.difference_interval.high_micro = 90_000;
        }
        match wide.certify(at(1_500_000)).expect("outcome") {
            CertificationOutcome::Failed { violations } => assert!(
                violations.iter().any(|violation| matches!(
                    violation,
                    BoundViolation::EffectNotEquivalent { .. }
                )),
                "{violations:?}"
            ),
            other => panic!("expected failure, got {other:?}"),
        }

        let mut flipped = passing_artifact();
        if let Some(effect) = flipped.effect_agreement.as_mut() {
            effect.simulated_effect_micro = 200_000;
            effect.human_effect_micro = -200_000;
            effect.difference_interval.low_micro = 390_000;
            effect.difference_interval.high_micro = 410_000;
        }
        match flipped.certify(at(1_500_000)).expect("outcome") {
            CertificationOutcome::Failed { violations } => {
                assert!(
                    violations.iter().any(|violation| matches!(
                        violation,
                        BoundViolation::EffectDirectionDisagrees { .. }
                    )),
                    "{violations:?}"
                );
            }
            other => panic!("expected failure, got {other:?}"),
        }
    }

    // Pins: two effects both inside the margin agree in direction even when their
    // arithmetic signs differ, so "no effect either way" is not a disagreement.
    #[test]
    fn effects_inside_the_margin_agree_in_direction_offline() {
        let mut artifact = passing_artifact();
        if let Some(effect) = artifact.effect_agreement.as_mut() {
            effect.simulated_effect_micro = 10_000;
            effect.human_effect_micro = -10_000;
            effect.difference_interval.low_micro = 15_000;
            effect.difference_interval.high_micro = 25_000;
        }
        let outcome = artifact.certify(at(1_500_000)).expect("outcome");
        assert_eq!(
            outcome.verdict(),
            "certified",
            "two null effects must not read as a direction disagreement: {outcome:?}"
        );
    }

    // Pins: an interval computed a different way than predeclared fails, so a
    // study cannot swap in a narrower method after seeing the data.
    #[test]
    fn interval_method_must_match_the_predeclaration_offline() {
        let mut artifact = passing_artifact();
        if let Some(effect) = artifact.effect_agreement.as_mut() {
            effect.difference_interval.method = IntervalMethod::StudentTOnUnitMeans;
        }
        match artifact.certify(at(1_500_000)).expect("outcome") {
            CertificationOutcome::Failed { violations } => assert!(
                violations.iter().any(|violation| matches!(
                    violation,
                    BoundViolation::IntervalMethodNotPredeclared { .. }
                )),
                "{violations:?}"
            ),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    // Pins: spending past the authorized study budget fails certification, so a
    // fidelity study cannot buy its way to a pass.
    #[test]
    fn spending_past_the_authorized_budget_fails_certification_offline() {
        let mut artifact = passing_artifact();
        artifact.cost.spent_micro_usd = artifact.cost.budget_micro_usd + 1;
        match artifact.certify(at(1_500_000)).expect("outcome") {
            CertificationOutcome::Failed { violations } => assert!(
                violations
                    .iter()
                    .any(|violation| matches!(violation, BoundViolation::BudgetExceeded { .. })),
                "{violations:?}"
            ),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    // Pins: a study run outside its human-data authorization fails certification.
    #[test]
    fn unauthorized_human_data_use_fails_certification_offline() {
        let mut artifact = passing_artifact();
        artifact.authorization.expires_at = artifact.observed_at;
        match artifact.certify(at(1_500_000)).expect("outcome") {
            CertificationOutcome::Failed { violations } => assert!(
                violations.iter().any(|violation| matches!(
                    violation,
                    BoundViolation::HumanDataAuthorizationInvalid { .. }
                )),
                "{violations:?}"
            ),
            other => panic!("expected failure, got {other:?}"),
        }
    }

    // Pins: a class row whose cells do not account for its declared independent
    // units is refused outright, so clustered transcript counts cannot be passed
    // off as independent support.
    #[test]
    fn class_counts_must_account_for_declared_units_offline() {
        let mut artifact = passing_artifact();
        artifact
            .class_agreement
            .first_mut()
            .expect("one class")
            .independent_units += 1;
        assert!(matches!(
            artifact.certify(at(1_500_000)),
            Err(SimulatorPolicyError::InvalidMeasurement { .. })
        ));
    }

    // Pins: the artifact pins every input the verify bullet lists, so changing any
    // of them changes the artifact digest.
    #[test]
    fn artifact_digest_covers_every_pinned_input_offline() {
        let baseline = passing_artifact();
        let baseline_digest = baseline.digest().expect("baseline digest");

        let mut mutations: Vec<(&str, FidelityStudyArtifact)> = Vec::new();

        let mut selection = baseline.clone();
        selection.selection_cohort.cohort_id = "other-selection".to_string();
        mutations.push(("selection cohort", selection));

        let mut certification = baseline.clone();
        certification.certification_cohort.content_hash = Digest32([42_u8; 32]);
        mutations.push(("certification cohort", certification));

        let mut support = baseline.clone();
        support.bounds.minimum_support.certification_units += 1;
        mutations.push(("independent support", support));

        let mut unit = baseline.clone();
        unit.bounds.independent_unit = IndependentUnit::HumanAccount;
        mutations.push(("independent unit", unit));

        let mut model = baseline.clone();
        model.simulator_components.model =
            moa_core::types::identifiers::ModelId::new("other-model");
        mutations.push(("model", model));

        let mut prompt = baseline.clone();
        prompt
            .simulator_components
            .system_prompt
            .push_str(" changed");
        mutations.push(("prompt", prompt));

        let mut labels = baseline.clone();
        labels.label_protocol.rubric_hash = Digest32([12_u8; 32]);
        mutations.push(("labels", labels));

        let mut policy = baseline.clone();
        policy.policy_hash = Digest32([13_u8; 32]);
        mutations.push(("policy", policy));

        let mut acceptance = baseline.clone();
        acceptance.bounds.effect_equivalence.margin_micro = 70_000;
        mutations.push(("acceptance bounds", acceptance));

        let mut cost = baseline.clone();
        cost.cost.spent_micro_usd += 1;
        mutations.push(("cost", cost));

        let mut uncertainty = baseline.clone();
        if let Some(effect) = uncertainty.effect_agreement.as_mut() {
            effect.difference_interval.high_micro += 1_000;
        }
        mutations.push(("uncertainty", uncertainty));

        let mut authorization = baseline.clone();
        authorization.authorization.authorization_id = "other-authorization".to_string();
        mutations.push(("authorization", authorization));

        for (label, mutated) in mutations {
            assert_ne!(
                mutated.digest().expect("mutated digest"),
                baseline_digest,
                "changing `{label}` must change the fidelity artifact digest"
            );
        }
    }

    // Pins: the canonical artifact bytes round-trip to the same value and the same
    // digest, which is what lets storage keep the bytes and re-verify them.
    #[test]
    fn canonical_bytes_round_trip_to_the_same_digest_offline() {
        let artifact = passing_artifact();
        let bytes = artifact.canonical_bytes().expect("canonical bytes");
        let decoded: FidelityStudyArtifact =
            serde_json::from_slice(&bytes).expect("canonical bytes decode");
        assert_eq!(decoded, artifact);
        assert_eq!(
            decoded.digest().expect("decoded digest"),
            artifact.digest().expect("artifact digest")
        );
    }

    // Pins: the pinned uncertainty reports a real interval per measured class at
    // the predeclared confidence level.
    #[test]
    fn artifact_uncertainty_reports_per_class_intervals_offline() {
        let artifact = passing_artifact();
        let uncertainty = artifact.uncertainty().expect("uncertainty");
        assert_eq!(
            uncertainty.class_confidence_permille,
            artifact.bounds.class_confidence_permille
        );
        let class = uncertainty
            .class_bounds
            .first()
            .expect("one measured class bound");
        let sensitivity = class
            .sensitivity_lower_bound_permille
            .expect("positives exist, so sensitivity is defined");
        assert!((1..PERMILLE_DENOMINATOR).contains(&sensitivity));
        assert!(uncertainty.effect_difference_interval.is_some());
    }

    // Pins: a domain predeclaration with an impossible bound is refused before it
    // can decide anything.
    #[test]
    fn invalid_domain_bounds_are_refused_offline() {
        let mut zero_margin = bounds();
        zero_margin.effect_equivalence.margin_micro = 0;
        assert!(zero_margin.validate().is_err());

        let mut no_classes = bounds();
        no_classes.critical_classes.clear();
        assert!(no_classes.validate().is_err());

        let mut zero_support = bounds();
        zero_support.minimum_support.certification_units = 0;
        assert!(zero_support.validate().is_err());

        let mut bad_confidence = bounds();
        bad_confidence.class_confidence_permille = 933;
        assert!(bad_confidence.validate().is_err());

        let mut no_recert = bounds();
        no_recert.recertification_interval_days = 0;
        assert!(no_recert.validate().is_err());

        let mut impossible_class_bound = bounds();
        impossible_class_bound
            .critical_classes
            .first_mut()
            .expect("one class")
            .min_sensitivity_lower_bound_permille = PERMILLE_DENOMINATOR + 1;
        assert!(impossible_class_bound.validate().is_err());

        let mut duplicate_class = bounds();
        let first = duplicate_class
            .critical_classes
            .first()
            .expect("one class")
            .clone();
        duplicate_class.critical_classes.push(first);
        assert!(duplicate_class.validate().is_err());
    }

    // Pins: a cohort pin declaring no independent units is refused, which is what
    // keeps a study from claiming support it does not have.
    #[test]
    fn cohort_without_independent_units_is_refused_offline() {
        let mut cohort = certification_cohort();
        cohort.independent_units = 0;
        assert!(cohort.validate().is_err());
    }
}
