//! Declared measurement contract for every gated evaluation metric.
//!
//! A [`MetricDefinition`] is the predeclaration a sampled gate must publish
//! before it is allowed to make a population claim: what is being estimated and
//! over which population, which unit is independent, how observations are
//! clustered and paired, which estimator and interval method are used, the
//! direction that counts as better, the tolerated regression margin, the alpha
//! and its interval construction, the separate acceptable and unacceptable
//! alternatives used for power analysis, and the gate family the metric belongs
//! to.
//!
//! The classification comes first and the method follows from it
//! ([`MetricClass::decision_method`]). [`MetricDefinition::validate`] refuses a
//! definition whose interval method or estimator does not match its class, and
//! refuses inferential machinery — margins, alternatives, cluster/pairing keys —
//! attached to an exact fixed-corpus metric, where a non-inferiority margin or a
//! minimum detectable effect has no meaning.
//!
//! Deciding a metric lives in [`crate::decision`]; this module only declares
//! what may be decided.

use serde::{Deserialize, Serialize};

/// Version of the metric-definition contract.
pub const METRIC_DEFINITION_VERSION: u32 = 1;

/// Direction of improvement for a metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricDirection {
    /// Larger raw values are better (recall, pass rate).
    HigherIsBetter,
    /// Smaller raw values are better (latency, cost, leak counts).
    LowerIsBetter,
}

impl MetricDirection {
    /// Returns the orientation multiplier that maps a raw delta to utility.
    #[must_use]
    pub fn sign(self) -> f64 {
        match self {
            Self::HigherIsBetter => 1.0,
            Self::LowerIsBetter => -1.0,
        }
    }
}

/// Measurement class of a metric.
///
/// The class is the first decision: it fixes which family of decision methods
/// is admissible before any interval is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricClass {
    /// Fixed-corpus invariant or safety count decided exactly.
    FixedCorpusInvariant,
    /// Paired binary outcome per case, decided on a matched effect.
    PairedBinary,
    /// Paired numeric score per case, decided on the paired delta.
    PairedNumeric,
    /// Stochastic live result with repetitions nested inside each case.
    StochasticLive,
    /// Tail latency quantile such as p95.
    TailLatencyQuantile,
}

impl MetricClass {
    /// Returns the only decision method admissible for this class.
    #[must_use]
    pub fn decision_method(self) -> DecisionMethod {
        match self {
            Self::FixedCorpusInvariant => DecisionMethod::ExactPassFailOrFailureRateBound,
            Self::PairedBinary => DecisionMethod::MatchedPairedEffectInterval,
            Self::PairedNumeric => DecisionMethod::ClusterPairedNumericInterval,
            Self::StochasticLive => DecisionMethod::HierarchicalCaseRepetitionInterval,
            Self::TailLatencyQuantile => DecisionMethod::PairedQuantileInterval,
        }
    }

    /// Returns whether this class is decided exactly rather than inferentially.
    #[must_use]
    pub fn is_exact(self) -> bool {
        matches!(self, Self::FixedCorpusInvariant)
    }
}

/// Decision method implied by a [`MetricClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionMethod {
    /// Exact pass/fail, or an exact upper bound on the failure rate.
    ExactPassFailOrFailureRateBound,
    /// Matched effect interval that supports a nonzero margin.
    MatchedPairedEffectInterval,
    /// Cluster-aware interval on candidate-minus-baseline scores.
    ClusterPairedNumericInterval,
    /// Case-level resampling with repetitions nested inside each case.
    HierarchicalCaseRepetitionInterval,
    /// Quantile-specific paired interval.
    PairedQuantileInterval,
}

/// What the metric estimates and over which population.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimand {
    /// Measurement class that fixes the admissible decision method.
    pub class: MetricClass,
    /// Human-readable statement of the estimated quantity.
    pub summary: String,
    /// Population the estimate is claimed to describe.
    pub target_population: String,
}

/// Unit of the raw metric values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricUnit {
    /// Value in `[0, 1]`.
    Proportion,
    /// Non-negative count.
    Count,
    /// Wall-clock milliseconds.
    Milliseconds,
    /// Cost in micro-USD.
    MicroUsd,
    /// Unitless score.
    Dimensionless,
}

impl MetricUnit {
    /// Returns the largest meaningful non-inferiority margin for this unit.
    #[must_use]
    pub fn max_meaningful_margin(self) -> Option<f64> {
        match self {
            Self::Proportion => Some(1.0),
            Self::Count | Self::Milliseconds | Self::MicroUsd | Self::Dimensionless => None,
        }
    }
}

/// Estimator applied to paired observations.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Estimator {
    /// Exact count of invariant failures over a fixed corpus.
    ExactFailureCount,
    /// Matched risk difference over paired binary outcomes.
    MatchedRiskDifference,
    /// Mean of per-pair candidate-minus-baseline deltas.
    MeanPairedDelta,
    /// Mean of per-case paired deltas with repetitions nested inside a case.
    MeanPairedCaseDelta,
    /// Difference of a paired quantile between candidate and baseline.
    PairedQuantileDelta {
        /// Quantile in `(0, 1)`, for example `0.95`.
        quantile: f64,
    },
}

impl Estimator {
    fn admissible_for(self, class: MetricClass) -> bool {
        match class {
            MetricClass::FixedCorpusInvariant => matches!(self, Self::ExactFailureCount),
            MetricClass::PairedBinary => matches!(self, Self::MatchedRiskDifference),
            MetricClass::PairedNumeric => matches!(self, Self::MeanPairedDelta),
            MetricClass::StochasticLive => matches!(self, Self::MeanPairedCaseDelta),
            MetricClass::TailLatencyQuantile => matches!(self, Self::PairedQuantileDelta { .. }),
        }
    }
}

/// Deterministic resampling parameters for a bootstrap interval method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResamplingPlan {
    /// Number of bootstrap resamples.
    pub resamples: usize,
    /// Deterministic PRNG seed.
    pub seed: u64,
    /// Minimum independent resampling units required for a population claim.
    ///
    /// Support below this count decides `INCONCLUSIVE`. The floor is declared
    /// per metric on purpose: there is no universal sample-size constant, and a
    /// percentile cluster bootstrap over a handful of clusters is not
    /// trustworthy population inference.
    pub min_independent_units: usize,
}

/// How the confidence bounds for the utility delta are constructed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceMethod {
    /// Exact pass/fail against an allowed failure count.
    ExactPassFail,
    /// Exact (Clopper-Pearson) upper bound on a failure rate.
    ExactFailureRateUpperBound,
    /// Closed-form matched risk-difference interval with per-cell pseudo-counts.
    ///
    /// Applies when every pair is its own independent unit. Unlike an exact
    /// McNemar test, this interval supports a nonzero non-inferiority margin.
    MatchedRiskDifferenceAdjustedWald {
        /// Pseudo-count added to each of the four matched cells.
        pseudo_count: f64,
        /// Minimum paired observations required for a population claim.
        min_independent_units: usize,
    },
    /// Cluster bootstrap over matched binary pairs grouped by cluster key.
    ClusterMatchedRiskDifferenceBootstrap(ResamplingPlan),
    /// Cluster percentile bootstrap over paired numeric deltas.
    ClusterPairedDeltaBootstrap(ResamplingPlan),
    /// Two-stage bootstrap resampling cases, then repetitions inside each case.
    HierarchicalCaseBootstrap(ResamplingPlan),
    /// Cluster bootstrap on the difference of a paired quantile.
    ClusterPairedQuantileBootstrap(ResamplingPlan),
}

impl ConfidenceMethod {
    /// Returns whether the method is exact rather than inferential.
    #[must_use]
    pub fn is_exact(self) -> bool {
        matches!(self, Self::ExactPassFail | Self::ExactFailureRateUpperBound)
    }

    /// Returns the minimum independent resampling units this method requires.
    #[must_use]
    pub fn min_independent_units(self) -> usize {
        match self {
            Self::ExactPassFail | Self::ExactFailureRateUpperBound => 0,
            Self::MatchedRiskDifferenceAdjustedWald {
                min_independent_units,
                ..
            } => min_independent_units,
            Self::ClusterMatchedRiskDifferenceBootstrap(plan)
            | Self::ClusterPairedDeltaBootstrap(plan)
            | Self::HierarchicalCaseBootstrap(plan)
            | Self::ClusterPairedQuantileBootstrap(plan) => plan.min_independent_units,
        }
    }

    /// Returns the resampling plan when the method is a bootstrap.
    #[must_use]
    pub fn resampling_plan(self) -> Option<ResamplingPlan> {
        match self {
            Self::ClusterMatchedRiskDifferenceBootstrap(plan)
            | Self::ClusterPairedDeltaBootstrap(plan)
            | Self::HierarchicalCaseBootstrap(plan)
            | Self::ClusterPairedQuantileBootstrap(plan) => Some(plan),
            Self::ExactPassFail
            | Self::ExactFailureRateUpperBound
            | Self::MatchedRiskDifferenceAdjustedWald { .. } => None,
        }
    }

    /// Returns the same method with a different deterministic bootstrap seed.
    ///
    /// Used by operating-characteristic simulation so each simulated trial
    /// resamples independently while running the production gate unchanged.
    #[must_use]
    pub fn with_seed(self, seed: u64) -> Self {
        match self {
            Self::ClusterMatchedRiskDifferenceBootstrap(plan) => {
                Self::ClusterMatchedRiskDifferenceBootstrap(ResamplingPlan { seed, ..plan })
            }
            Self::ClusterPairedDeltaBootstrap(plan) => {
                Self::ClusterPairedDeltaBootstrap(ResamplingPlan { seed, ..plan })
            }
            Self::HierarchicalCaseBootstrap(plan) => {
                Self::HierarchicalCaseBootstrap(ResamplingPlan { seed, ..plan })
            }
            Self::ClusterPairedQuantileBootstrap(plan) => {
                Self::ClusterPairedQuantileBootstrap(ResamplingPlan { seed, ..plan })
            }
            other => other,
        }
    }

    fn admissible_for(self, class: MetricClass) -> bool {
        match class {
            MetricClass::FixedCorpusInvariant => self.is_exact(),
            MetricClass::PairedBinary => matches!(
                self,
                Self::MatchedRiskDifferenceAdjustedWald { .. }
                    | Self::ClusterMatchedRiskDifferenceBootstrap(_)
            ),
            MetricClass::PairedNumeric => matches!(self, Self::ClusterPairedDeltaBootstrap(_)),
            MetricClass::StochasticLive => matches!(self, Self::HierarchicalCaseBootstrap(_)),
            MetricClass::TailLatencyQuantile => {
                matches!(self, Self::ClusterPairedQuantileBootstrap(_))
            }
        }
    }
}

/// Role a metric plays in the release gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateKind {
    /// Exact invariant that fails closed.
    ExactInvariant,
    /// Required non-inferiority metric in the primary gate family.
    RequiredNonInferiority,
    /// Reported only; never blocks a release.
    Diagnostic,
}

impl GateKind {
    /// Returns whether the metric can block a release.
    #[must_use]
    pub fn is_blocking(self) -> bool {
        matches!(self, Self::ExactInvariant | Self::RequiredNonInferiority)
    }
}

/// Predeclared multiplicity family a metric belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HypothesisFamily {
    /// Primary gate family: intersection-union PASS, Holm on reverse
    /// one-sided regression hypotheses.
    Primary,
    /// Exploratory diagnostics: Benjamini-Hochberg only, never gating.
    Exploratory,
}

/// Full predeclaration required before a metric may gate a release.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDefinition {
    /// Stable metric identifier.
    pub id: String,
    /// Direction of improvement.
    pub direction: MetricDirection,
    /// Estimated quantity, class, and target population.
    pub estimand: Estimand,
    /// Unit of the raw values.
    pub unit: MetricUnit,
    /// Name of the independent unit, for example `user` or `case`.
    pub independent_unit: String,
    /// Field carrying the cluster identity; required for sampled metrics.
    pub cluster_key: Option<String>,
    /// Field carrying the pairing identity; required for sampled metrics.
    pub paired_key: Option<String>,
    /// Estimator applied to the paired observations.
    pub estimator: Estimator,
    /// Tolerated regression, expressed in utility units and strictly positive.
    ///
    /// `None` for exact metrics, where a non-inferiority margin is meaningless.
    pub practical_margin: Option<f64>,
    /// One-sided alpha used for each confidence bound.
    pub alpha: f64,
    /// Construction of the confidence bounds.
    pub confidence_method: ConfidenceMethod,
    /// Utility delta that must PASS with high probability, commonly `0.0`.
    pub acceptable_alternative: Option<f64>,
    /// Utility delta below `-margin` that must be detected as a regression.
    pub unacceptable_alternative: Option<f64>,
    /// Role of this metric in the release gate.
    pub gate_kind: GateKind,
    /// Predeclared multiplicity family.
    pub hypothesis_family: HypothesisFamily,
}

impl MetricDefinition {
    /// Returns the measurement class declared by the estimand.
    #[must_use]
    pub fn class(&self) -> MetricClass {
        self.estimand.class
    }

    /// Returns whether the metric is decided exactly.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        self.estimand.class.is_exact()
    }

    /// Returns the orientation multiplier for this metric.
    #[must_use]
    pub fn direction_sign(&self) -> f64 {
        self.direction.sign()
    }

    /// Orients a raw candidate/baseline pair so larger is always better.
    #[must_use]
    pub fn utility_delta(&self, baseline: f64, candidate: f64) -> f64 {
        self.direction_sign() * (candidate - baseline)
    }

    /// Returns the declared non-inferiority margin.
    ///
    /// # Errors
    ///
    /// Returns [`MetricDefinitionError::MissingMargin`] for a sampled metric
    /// that never declared one, and
    /// [`MetricDefinitionError::MarginOnExactMetric`] for an exact metric.
    pub fn margin(&self) -> Result<f64, MetricDefinitionError> {
        if self.is_exact() {
            return Err(MetricDefinitionError::MarginOnExactMetric {
                metric_id: self.id.clone(),
            });
        }
        self.practical_margin
            .ok_or_else(|| MetricDefinitionError::MissingMargin {
                metric_id: self.id.clone(),
            })
    }

    /// Returns a copy whose bootstrap method uses a different seed.
    #[must_use]
    pub fn with_bootstrap_seed(&self, seed: u64) -> Self {
        let mut definition = self.clone();
        definition.confidence_method = definition.confidence_method.with_seed(seed);
        definition
    }

    /// Returns a copy with a different practical margin.
    #[must_use]
    pub fn with_practical_margin(&self, practical_margin: f64) -> Self {
        let mut definition = self.clone();
        definition.practical_margin = Some(practical_margin);
        definition
    }

    /// Validates the declaration before any decision may use it.
    ///
    /// # Errors
    ///
    /// Returns the first violated declaration rule: empty identity, alpha out
    /// of range, a method or estimator that does not match the declared class,
    /// missing cluster/pairing keys, a non-positive or impossible margin,
    /// alternatives that do not straddle the margin, or inferential inputs
    /// attached to an exact metric.
    pub fn validate(&self) -> Result<(), MetricDefinitionError> {
        if self.id.trim().is_empty() {
            return Err(MetricDefinitionError::EmptyField {
                metric_id: self.id.clone(),
                field: "id",
            });
        }
        if self.independent_unit.trim().is_empty() {
            return Err(MetricDefinitionError::EmptyField {
                metric_id: self.id.clone(),
                field: "independent_unit",
            });
        }
        if self.estimand.summary.trim().is_empty() {
            return Err(MetricDefinitionError::EmptyField {
                metric_id: self.id.clone(),
                field: "estimand.summary",
            });
        }
        if self.estimand.target_population.trim().is_empty() {
            return Err(MetricDefinitionError::EmptyField {
                metric_id: self.id.clone(),
                field: "estimand.target_population",
            });
        }

        let class = self.class();
        if !self.confidence_method.admissible_for(class) {
            return Err(MetricDefinitionError::MethodClassMismatch {
                metric_id: self.id.clone(),
                class,
                required: class.decision_method(),
            });
        }
        if !self.estimator.admissible_for(class) {
            return Err(MetricDefinitionError::EstimatorClassMismatch {
                metric_id: self.id.clone(),
                class,
            });
        }
        if let Estimator::PairedQuantileDelta { quantile } = self.estimator
            && (!quantile.is_finite() || quantile <= 0.0 || quantile >= 1.0)
        {
            return Err(MetricDefinitionError::InvalidQuantile {
                metric_id: self.id.clone(),
                quantile,
            });
        }

        if self.is_exact() {
            return self.validate_exact();
        }
        self.validate_sampled()
    }

    fn validate_exact(&self) -> Result<(), MetricDefinitionError> {
        for (field, present) in [
            ("practical_margin", self.practical_margin.is_some()),
            (
                "acceptable_alternative",
                self.acceptable_alternative.is_some(),
            ),
            (
                "unacceptable_alternative",
                self.unacceptable_alternative.is_some(),
            ),
            ("cluster_key", self.cluster_key.is_some()),
            ("paired_key", self.paired_key.is_some()),
        ] {
            if present {
                return Err(MetricDefinitionError::InferentialInputOnExactMetric {
                    metric_id: self.id.clone(),
                    field,
                });
            }
        }
        if self.gate_kind == GateKind::RequiredNonInferiority {
            return Err(MetricDefinitionError::InferentialInputOnExactMetric {
                metric_id: self.id.clone(),
                field: "gate_kind",
            });
        }
        Ok(())
    }

    fn validate_sampled(&self) -> Result<(), MetricDefinitionError> {
        if !self.alpha.is_finite() || self.alpha <= 0.0 || self.alpha > 0.5 {
            return Err(MetricDefinitionError::AlphaOutOfRange {
                metric_id: self.id.clone(),
                alpha: self.alpha,
            });
        }
        match self.cluster_key.as_deref() {
            Some(key) if !key.trim().is_empty() => {}
            _ => {
                return Err(MetricDefinitionError::MissingClusterKey {
                    metric_id: self.id.clone(),
                });
            }
        }
        match self.paired_key.as_deref() {
            Some(key) if !key.trim().is_empty() => {}
            _ => {
                return Err(MetricDefinitionError::MissingPairedKey {
                    metric_id: self.id.clone(),
                });
            }
        }

        let margin = self
            .practical_margin
            .ok_or_else(|| MetricDefinitionError::MissingMargin {
                metric_id: self.id.clone(),
            })?;
        if !margin.is_finite() || margin <= 0.0 {
            return Err(MetricDefinitionError::NonPositiveMargin {
                metric_id: self.id.clone(),
                margin,
            });
        }
        if let Some(max_margin) = self.unit.max_meaningful_margin()
            && margin > max_margin
        {
            return Err(MetricDefinitionError::MarginExceedsUnitRange {
                metric_id: self.id.clone(),
                margin,
                max_margin,
            });
        }

        if let Some(plan) = self.confidence_method.resampling_plan()
            && plan.resamples == 0
        {
            return Err(MetricDefinitionError::ZeroResamples {
                metric_id: self.id.clone(),
            });
        }
        if self.confidence_method.min_independent_units() < MIN_DECLARABLE_INDEPENDENT_UNITS {
            return Err(MetricDefinitionError::SupportFloorTooLow {
                metric_id: self.id.clone(),
                declared: self.confidence_method.min_independent_units(),
                minimum: MIN_DECLARABLE_INDEPENDENT_UNITS,
            });
        }

        let acceptable = self.acceptable_alternative.ok_or_else(|| {
            MetricDefinitionError::MissingAlternative {
                metric_id: self.id.clone(),
                field: "acceptable_alternative",
            }
        })?;
        let unacceptable = self.unacceptable_alternative.ok_or_else(|| {
            MetricDefinitionError::MissingAlternative {
                metric_id: self.id.clone(),
                field: "unacceptable_alternative",
            }
        })?;
        if !acceptable.is_finite() || acceptable <= -margin {
            return Err(MetricDefinitionError::AcceptableAlternativeNotAboveMargin {
                metric_id: self.id.clone(),
                acceptable_alternative: acceptable,
                margin,
            });
        }
        if !unacceptable.is_finite() || unacceptable >= -margin {
            return Err(
                MetricDefinitionError::UnacceptableAlternativeNotBelowMargin {
                    metric_id: self.id.clone(),
                    unacceptable_alternative: unacceptable,
                    margin,
                },
            );
        }
        Ok(())
    }
}

/// Mathematical floor on the declared independent-unit support.
///
/// This is not a power rule: it is the point below which a percentile
/// resampling interval has no meaning at all. Metrics declare their real floor
/// in [`ResamplingPlan::min_independent_units`].
pub const MIN_DECLARABLE_INDEPENDENT_UNITS: usize = 2;

/// Errors returned when a metric declaration is incomplete or inconsistent.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MetricDefinitionError {
    /// A required identity field was empty.
    #[error("metric {metric_id}: field {field} must not be empty")]
    EmptyField {
        /// Metric identifier.
        metric_id: String,
        /// Offending field name.
        field: &'static str,
    },
    /// Alpha was outside `(0, 0.5]`.
    #[error("metric {metric_id}: alpha {alpha} must be in (0, 0.5]")]
    AlphaOutOfRange {
        /// Metric identifier.
        metric_id: String,
        /// Declared alpha.
        alpha: f64,
    },
    /// The interval method does not match the declared metric class.
    #[error("metric {metric_id}: class {class:?} requires decision method {required:?}")]
    MethodClassMismatch {
        /// Metric identifier.
        metric_id: String,
        /// Declared class.
        class: MetricClass,
        /// Method the class requires.
        required: DecisionMethod,
    },
    /// The estimator does not match the declared metric class.
    #[error("metric {metric_id}: estimator is not admissible for class {class:?}")]
    EstimatorClassMismatch {
        /// Metric identifier.
        metric_id: String,
        /// Declared class.
        class: MetricClass,
    },
    /// A quantile estimator declared a quantile outside `(0, 1)`.
    #[error("metric {metric_id}: quantile {quantile} must be in (0, 1)")]
    InvalidQuantile {
        /// Metric identifier.
        metric_id: String,
        /// Declared quantile.
        quantile: f64,
    },
    /// A sampled metric did not declare its cluster key.
    #[error("metric {metric_id}: sampled metrics must declare cluster_key")]
    MissingClusterKey {
        /// Metric identifier.
        metric_id: String,
    },
    /// A sampled metric did not declare its pairing key.
    #[error("metric {metric_id}: sampled metrics must declare paired_key")]
    MissingPairedKey {
        /// Metric identifier.
        metric_id: String,
    },
    /// A sampled metric did not declare a practical margin.
    #[error("metric {metric_id}: sampled metrics must declare practical_margin")]
    MissingMargin {
        /// Metric identifier.
        metric_id: String,
    },
    /// The declared margin was zero or negative.
    #[error("metric {metric_id}: practical_margin {margin} must be positive")]
    NonPositiveMargin {
        /// Metric identifier.
        metric_id: String,
        /// Declared margin.
        margin: f64,
    },
    /// The declared margin exceeded the range of the metric unit.
    #[error("metric {metric_id}: practical_margin {margin} exceeds unit range {max_margin}")]
    MarginExceedsUnitRange {
        /// Metric identifier.
        metric_id: String,
        /// Declared margin.
        margin: f64,
        /// Largest meaningful margin for the unit.
        max_margin: f64,
    },
    /// A bootstrap method declared zero resamples.
    #[error("metric {metric_id}: bootstrap resamples must be positive")]
    ZeroResamples {
        /// Metric identifier.
        metric_id: String,
    },
    /// The declared support floor was below the mathematical minimum.
    #[error("metric {metric_id}: min_independent_units {declared} is below the minimum {minimum}")]
    SupportFloorTooLow {
        /// Metric identifier.
        metric_id: String,
        /// Declared floor.
        declared: usize,
        /// Mathematical minimum.
        minimum: usize,
    },
    /// A sampled metric omitted one of its power alternatives.
    #[error("metric {metric_id}: sampled metrics must declare {field}")]
    MissingAlternative {
        /// Metric identifier.
        metric_id: String,
        /// Missing alternative field.
        field: &'static str,
    },
    /// The acceptable alternative was not above `-margin`.
    #[error(
        "metric {metric_id}: acceptable_alternative {acceptable_alternative} must exceed -{margin}"
    )]
    AcceptableAlternativeNotAboveMargin {
        /// Metric identifier.
        metric_id: String,
        /// Declared acceptable alternative.
        acceptable_alternative: f64,
        /// Declared margin.
        margin: f64,
    },
    /// The unacceptable alternative was not below `-margin`.
    #[error(
        "metric {metric_id}: unacceptable_alternative {unacceptable_alternative} must be below -{margin}"
    )]
    UnacceptableAlternativeNotBelowMargin {
        /// Metric identifier.
        metric_id: String,
        /// Declared unacceptable alternative.
        unacceptable_alternative: f64,
        /// Declared margin.
        margin: f64,
    },
    /// Inferential machinery was attached to an exact metric.
    #[error(
        "metric {metric_id}: exact fixed-corpus metrics must not declare {field}; a non-inferiority margin or minimum detectable effect has no meaning for an exact gate"
    )]
    InferentialInputOnExactMetric {
        /// Metric identifier.
        metric_id: String,
        /// Offending field name.
        field: &'static str,
    },
    /// A margin was requested from an exact metric.
    #[error("metric {metric_id}: exact metrics have no non-inferiority margin")]
    MarginOnExactMetric {
        /// Metric identifier.
        metric_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a valid paired-numeric declaration for mutation of single fields.
    fn paired_numeric() -> MetricDefinition {
        MetricDefinition {
            id: "recall_at_4".to_string(),
            direction: MetricDirection::HigherIsBetter,
            estimand: Estimand {
                class: MetricClass::PairedNumeric,
                summary: "mean paired recall@4 delta".to_string(),
                target_population: "seeded corpus users".to_string(),
            },
            unit: MetricUnit::Proportion,
            independent_unit: "user".to_string(),
            cluster_key: Some("user_id".to_string()),
            paired_key: Some("probe_id".to_string()),
            estimator: Estimator::MeanPairedDelta,
            practical_margin: Some(0.02),
            alpha: 0.025,
            confidence_method: ConfidenceMethod::ClusterPairedDeltaBootstrap(ResamplingPlan {
                resamples: 512,
                seed: 7,
                min_independent_units: 12,
            }),
            acceptable_alternative: Some(0.0),
            unacceptable_alternative: Some(-0.05),
            gate_kind: GateKind::RequiredNonInferiority,
            hypothesis_family: HypothesisFamily::Primary,
        }
    }

    fn exact_invariant() -> MetricDefinition {
        MetricDefinition {
            id: "cross_user_leak_count".to_string(),
            direction: MetricDirection::LowerIsBetter,
            estimand: Estimand {
                class: MetricClass::FixedCorpusInvariant,
                summary: "cross-user leaks over the pinned corpus".to_string(),
                target_population: "pinned corpus".to_string(),
            },
            unit: MetricUnit::Count,
            independent_unit: "corpus".to_string(),
            cluster_key: None,
            paired_key: None,
            estimator: Estimator::ExactFailureCount,
            practical_margin: None,
            alpha: 0.0,
            confidence_method: ConfidenceMethod::ExactPassFail,
            acceptable_alternative: None,
            unacceptable_alternative: None,
            gate_kind: GateKind::ExactInvariant,
            hypothesis_family: HypothesisFamily::Primary,
        }
    }

    #[test]
    fn valid_declarations_pass_validation_in_both_classes() {
        // Pins: the two shipped declaration shapes validate, so later negative
        // cases isolate exactly one violated rule.
        paired_numeric()
            .validate()
            .expect("paired numeric is valid");
        exact_invariant().validate().expect("exact is valid");
    }

    #[test]
    fn orientation_flips_sign_for_lower_is_better_metrics() {
        // Pins: utility_delta = direction_sign * (candidate - baseline); a
        // latency increase is a negative utility delta.
        let mut latency = paired_numeric();
        latency.direction = MetricDirection::LowerIsBetter;
        assert!((paired_numeric().utility_delta(0.80, 0.75) + 0.05).abs() < 1e-12);
        assert!((latency.utility_delta(100.0, 120.0) + 20.0).abs() < 1e-12);
        assert!((latency.utility_delta(100.0, 80.0) - 20.0).abs() < 1e-12);
    }

    #[test]
    fn sampled_metric_without_pairing_or_cluster_key_is_refused() {
        // Pins: a sampled metric cannot gate without declaring how observations
        // are paired and clustered.
        let mut missing_pairing = paired_numeric();
        missing_pairing.paired_key = None;
        assert!(matches!(
            missing_pairing.validate(),
            Err(MetricDefinitionError::MissingPairedKey { .. })
        ));

        let mut blank_cluster = paired_numeric();
        blank_cluster.cluster_key = Some("   ".to_string());
        assert!(matches!(
            blank_cluster.validate(),
            Err(MetricDefinitionError::MissingClusterKey { .. })
        ));
    }

    #[test]
    fn exact_metric_rejects_margin_and_power_alternatives() {
        // Pins: an inferential minimum detectable effect cannot be attached to
        // an exact fixed-corpus gate.
        for mutate in [
            (|definition: &mut MetricDefinition| definition.practical_margin = Some(0.01))
                as fn(&mut MetricDefinition),
            |definition: &mut MetricDefinition| definition.acceptable_alternative = Some(0.0),
            |definition: &mut MetricDefinition| definition.unacceptable_alternative = Some(-0.1),
            |definition: &mut MetricDefinition| definition.cluster_key = Some("user".to_string()),
            |definition: &mut MetricDefinition| definition.paired_key = Some("case".to_string()),
        ] {
            let mut definition = exact_invariant();
            mutate(&mut definition);
            assert!(
                matches!(
                    definition.validate(),
                    Err(MetricDefinitionError::InferentialInputOnExactMetric { .. })
                ),
                "exact metric accepted an inferential input"
            );
        }
        assert!(matches!(
            exact_invariant().margin(),
            Err(MetricDefinitionError::MarginOnExactMetric { .. })
        ));
    }

    #[test]
    fn class_requires_its_decision_method_and_estimator() {
        // Pins: classification happens before method choice; a paired binary
        // metric cannot borrow the numeric cluster-delta interval, and a paired
        // numeric metric cannot claim a matched risk difference.
        let mut binary = paired_numeric();
        binary.estimand.class = MetricClass::PairedBinary;
        binary.estimator = Estimator::MatchedRiskDifference;
        assert!(matches!(
            binary.validate(),
            Err(MetricDefinitionError::MethodClassMismatch { .. })
        ));

        let mut wrong_estimator = paired_numeric();
        wrong_estimator.estimator = Estimator::MatchedRiskDifference;
        assert!(matches!(
            wrong_estimator.validate(),
            Err(MetricDefinitionError::EstimatorClassMismatch { .. })
        ));

        assert_eq!(
            MetricClass::PairedBinary.decision_method(),
            DecisionMethod::MatchedPairedEffectInterval
        );
        assert_eq!(
            MetricClass::TailLatencyQuantile.decision_method(),
            DecisionMethod::PairedQuantileInterval
        );
        assert_eq!(
            MetricClass::StochasticLive.decision_method(),
            DecisionMethod::HierarchicalCaseRepetitionInterval
        );
    }

    #[test]
    fn alternatives_must_straddle_the_declared_margin() {
        // Pins: power analysis needs a separately declared acceptable
        // alternative above -margin and unacceptable alternative below it.
        let mut acceptable_below = paired_numeric();
        acceptable_below.acceptable_alternative = Some(-0.02);
        assert!(matches!(
            acceptable_below.validate(),
            Err(MetricDefinitionError::AcceptableAlternativeNotAboveMargin { .. })
        ));

        let mut unacceptable_above = paired_numeric();
        unacceptable_above.unacceptable_alternative = Some(-0.02);
        assert!(matches!(
            unacceptable_above.validate(),
            Err(MetricDefinitionError::UnacceptableAlternativeNotBelowMargin { .. })
        ));
    }

    #[test]
    fn margin_alpha_and_support_floors_are_bounded() {
        // Pins: a zero margin, an out-of-range alpha, an impossible proportion
        // margin, and a degenerate support floor are all refused.
        let mut zero_margin = paired_numeric();
        zero_margin.practical_margin = Some(0.0);
        assert!(matches!(
            zero_margin.validate(),
            Err(MetricDefinitionError::NonPositiveMargin { .. })
        ));

        let mut wide_alpha = paired_numeric();
        wide_alpha.alpha = 0.6;
        assert!(matches!(
            wide_alpha.validate(),
            Err(MetricDefinitionError::AlphaOutOfRange { .. })
        ));

        let mut impossible_margin = paired_numeric();
        impossible_margin.practical_margin = Some(1.5);
        assert!(matches!(
            impossible_margin.validate(),
            Err(MetricDefinitionError::MarginExceedsUnitRange { .. })
        ));

        let mut low_floor = paired_numeric();
        low_floor.confidence_method =
            ConfidenceMethod::ClusterPairedDeltaBootstrap(ResamplingPlan {
                resamples: 128,
                seed: 1,
                min_independent_units: 1,
            });
        assert!(matches!(
            low_floor.validate(),
            Err(MetricDefinitionError::SupportFloorTooLow { .. })
        ));
    }

    #[test]
    fn bootstrap_seed_override_preserves_every_other_declared_input() {
        // Pins: simulation may vary only the resampling seed, so a simulated
        // trial runs the production gate rather than a relaxed one.
        let definition = paired_numeric();
        let reseeded = definition.with_bootstrap_seed(99);
        assert_eq!(
            reseeded.confidence_method.resampling_plan().map(|p| p.seed),
            Some(99)
        );
        assert_eq!(
            reseeded.confidence_method.min_independent_units(),
            definition.confidence_method.min_independent_units()
        );
        assert_eq!(reseeded.practical_margin, definition.practical_margin);
        assert_eq!(reseeded.alpha, definition.alpha);
        reseeded.validate().expect("reseeded metric stays valid");
    }
}
