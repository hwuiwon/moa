//! Three-way gate decisions over declared metrics.
//!
//! Every sampled metric is oriented so larger is better, and the decision is
//! made on the paired utility delta against the declared non-inferiority
//! margin:
//!
//! ```text
//! utility_delta = direction_sign * (candidate - baseline)
//! PASS         when lower_bound(utility_delta) >= -margin
//! REGRESSION   when upper_bound(utility_delta) <  -margin
//! INCONCLUSIVE otherwise
//! ```
//!
//! `INCONCLUSIVE` is also returned when the evidence cannot support a
//! population claim at all — fewer independent clusters than the metric
//! declared. Underpowered evidence never produces a green decision.
//!
//! Multiplicity is predeclared, not chosen after the fact:
//!
//! * PASS for a release uses [`intersection_union_gate`]: every required metric
//!   must pass on its own one-sided test, which controls the false PASS rate
//!   without a blanket multiplicity adjustment;
//! * declaring an overall REGRESSION because *any* metric regressed is a
//!   different, reverse family, so [`holm_regression_family`] applies Holm to
//!   the reverse one-sided regression p-values;
//! * [`benjamini_hochberg_adjusted_p_values`] is for exploratory diagnostics
//!   only and never gates.

use serde::{Deserialize, Serialize};

use crate::metric::{GateKind, HypothesisFamily, MetricDefinition, MetricDefinitionError};

/// Version of the decision contract.
pub const DECISION_CONTRACT_VERSION: u32 = 1;

/// Three-way outcome of a gate decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    /// Non-inferiority is established at the declared margin and alpha.
    Pass,
    /// A regression beyond the declared margin is established.
    Regression,
    /// The evidence supports neither conclusion.
    Inconclusive,
}

impl Decision {
    /// Returns whether the decision permits shipping.
    #[must_use]
    pub fn is_pass(self) -> bool {
        matches!(self, Self::Pass)
    }

    /// Returns whether the decision blocks shipping.
    #[must_use]
    pub fn is_blocking(self) -> bool {
        matches!(self, Self::Regression | Self::Inconclusive)
    }
}

/// Confidence bounds on the oriented utility delta.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct UtilityInterval {
    /// Point estimate of the utility delta.
    pub point: f64,
    /// Lower confidence bound at the declared one-sided alpha.
    pub lower: f64,
    /// Upper confidence bound at the declared one-sided alpha.
    pub upper: f64,
}

impl UtilityInterval {
    /// Builds an interval from a point estimate and its bounds.
    #[must_use]
    pub fn new(point: f64, lower: f64, upper: f64) -> Self {
        Self {
            point,
            lower,
            upper,
        }
    }

    /// Returns whether the interval covers a value.
    #[must_use]
    pub fn covers(&self, value: f64) -> bool {
        self.lower <= value && value <= self.upper
    }
}

/// Independent support behind an interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupportSummary {
    /// Distinct independent units (clusters or cases) actually observed.
    pub independent_units: usize,
    /// Paired observations actually observed.
    pub observations: usize,
    /// Independent units the metric declared it needs.
    pub required_independent_units: usize,
}

impl SupportSummary {
    /// Returns whether the observed support meets the declared requirement.
    #[must_use]
    pub fn is_sufficient(&self) -> bool {
        self.required_independent_units > 0
            && self.independent_units >= self.required_independent_units
            && self.observations > 0
    }
}

/// Decision for one metric, carrying every input that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDecision {
    /// Metric identifier.
    pub metric_id: String,
    /// Three-way decision.
    pub decision: Decision,
    /// Point estimate of the oriented utility delta.
    pub utility_delta: f64,
    /// Lower confidence bound on the utility delta.
    pub lower_bound: f64,
    /// Upper confidence bound on the utility delta.
    pub upper_bound: f64,
    /// Tolerated regression margin, `0.0` for exact metrics.
    pub practical_margin: f64,
    /// One-sided alpha, `0.0` for exact metrics.
    pub alpha: f64,
    /// Role of this metric in the release gate.
    pub gate_kind: GateKind,
    /// Predeclared multiplicity family.
    pub hypothesis_family: HypothesisFamily,
    /// Independent support behind the interval.
    pub support: SupportSummary,
    /// One-sided p-value for the reverse regression hypothesis, when available.
    pub regression_p_value: Option<f64>,
    /// Human-readable statement of why this decision was reached.
    pub rationale: String,
}

/// Applies the three-way rule to an interval on the utility delta.
///
/// The caller is responsible for orientation: `interval` must already be in
/// utility units, where larger is better.
#[must_use]
pub fn decide_utility_interval(margin: f64, interval: &UtilityInterval) -> Decision {
    if interval.lower >= -margin {
        Decision::Pass
    } else if interval.upper < -margin {
        Decision::Regression
    } else {
        Decision::Inconclusive
    }
}

/// Decides one sampled metric from its interval and observed support.
///
/// # Errors
///
/// Returns an error when the declaration is invalid, when the metric is exact
/// (use [`decide_exact_metric`]), or when the interval is non-finite or has its
/// bounds inverted.
pub fn decide_metric(
    definition: &MetricDefinition,
    interval: &UtilityInterval,
    support: SupportSummary,
    regression_p_value: Option<f64>,
) -> Result<MetricDecision, DecisionError> {
    definition.validate()?;
    if definition.is_exact() {
        return Err(DecisionError::ExactMetricNeedsExactDecision {
            metric_id: definition.id.clone(),
        });
    }
    let margin = definition.margin()?;
    if !interval.point.is_finite() || !interval.lower.is_finite() || !interval.upper.is_finite() {
        return Err(DecisionError::NonFiniteInterval {
            metric_id: definition.id.clone(),
        });
    }
    if interval.lower > interval.upper {
        return Err(DecisionError::InvertedInterval {
            metric_id: definition.id.clone(),
            lower: interval.lower,
            upper: interval.upper,
        });
    }

    let (decision, rationale) = if support.is_sufficient() {
        let decision = decide_utility_interval(margin, interval);
        let rationale = match decision {
            Decision::Pass => {
                format!(
                    "lower_bound {:.6} >= -margin {:.6}",
                    interval.lower, -margin
                )
            }
            Decision::Regression => {
                format!("upper_bound {:.6} < -margin {:.6}", interval.upper, -margin)
            }
            Decision::Inconclusive => format!(
                "interval [{:.6}, {:.6}] straddles -margin {:.6}",
                interval.lower, interval.upper, -margin
            ),
        };
        (decision, rationale)
    } else {
        (
            Decision::Inconclusive,
            format!(
                "insufficient support: {} independent {} observed, {} required",
                support.independent_units,
                definition.independent_unit,
                support.required_independent_units
            ),
        )
    };

    Ok(MetricDecision {
        metric_id: definition.id.clone(),
        decision,
        utility_delta: interval.point,
        lower_bound: interval.lower,
        upper_bound: interval.upper,
        practical_margin: margin,
        alpha: definition.alpha,
        gate_kind: definition.gate_kind,
        hypothesis_family: definition.hypothesis_family,
        support,
        regression_p_value: regression_p_value.filter(|_| support.is_sufficient()),
        rationale,
    })
}

/// Decides one exact fixed-corpus metric against its allowed failure count.
///
/// Exact metrics never return `INCONCLUSIVE`: the corpus is fixed and fully
/// observed, so the count either clears the allowance or it does not.
///
/// # Errors
///
/// Returns an error when the declaration is invalid or when the metric is
/// sampled rather than exact.
pub fn decide_exact_metric(
    definition: &MetricDefinition,
    observed_failures: u64,
    allowed_failures: u64,
) -> Result<MetricDecision, DecisionError> {
    definition.validate()?;
    if !definition.is_exact() {
        return Err(DecisionError::SampledMetricNeedsInterval {
            metric_id: definition.id.clone(),
        });
    }
    let utility_delta = allowed_failures as f64 - observed_failures as f64;
    let decision = if observed_failures <= allowed_failures {
        Decision::Pass
    } else {
        Decision::Regression
    };
    Ok(MetricDecision {
        metric_id: definition.id.clone(),
        decision,
        utility_delta,
        lower_bound: utility_delta,
        upper_bound: utility_delta,
        practical_margin: 0.0,
        alpha: 0.0,
        gate_kind: definition.gate_kind,
        hypothesis_family: definition.hypothesis_family,
        support: SupportSummary {
            independent_units: 1,
            observations: 1,
            required_independent_units: 1,
        },
        regression_p_value: None,
        rationale: format!(
            "exact gate: {observed_failures} failures observed, {allowed_failures} allowed"
        ),
    })
}

/// Predeclared gate family used to combine metric decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateFamily {
    /// Every required metric must pass its own one-sided test.
    IntersectionUnionNonInferiority,
}

/// Combined release decision over a set of metric decisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateOutcome {
    /// Predeclared family used to combine the metrics.
    pub family: GateFamily,
    /// Combined three-way decision.
    pub decision: Decision,
    /// Required metric identifiers that passed.
    pub passing: Vec<String>,
    /// Required metric identifiers that regressed.
    pub regressed: Vec<String>,
    /// Required metric identifiers whose evidence was inconclusive.
    pub inconclusive: Vec<String>,
    /// Human-readable statement of why this decision was reached.
    pub rationale: String,
}

/// Combines required metric decisions with an intersection-union rule.
///
/// Only blocking metrics ([`GateKind::is_blocking`]) in the
/// [`HypothesisFamily::Primary`] family participate. Diagnostics and
/// exploratory metrics are ignored here by construction.
#[must_use]
pub fn intersection_union_gate(decisions: &[MetricDecision]) -> GateOutcome {
    let mut passing = Vec::new();
    let mut regressed = Vec::new();
    let mut inconclusive = Vec::new();
    for decision in decisions {
        if !decision.gate_kind.is_blocking()
            || decision.hypothesis_family != HypothesisFamily::Primary
        {
            continue;
        }
        match decision.decision {
            Decision::Pass => passing.push(decision.metric_id.clone()),
            Decision::Regression => regressed.push(decision.metric_id.clone()),
            Decision::Inconclusive => inconclusive.push(decision.metric_id.clone()),
        }
    }

    let (decision, rationale) = if !regressed.is_empty() {
        (
            Decision::Regression,
            format!("required metrics regressed: {}", regressed.join(", ")),
        )
    } else if !inconclusive.is_empty() {
        (
            Decision::Inconclusive,
            format!(
                "required metrics lack sufficient evidence: {}",
                inconclusive.join(", ")
            ),
        )
    } else if passing.is_empty() {
        (
            Decision::Inconclusive,
            "no required metric produced a decision".to_string(),
        )
    } else {
        (
            Decision::Pass,
            format!(
                "all {} required metrics passed their non-inferiority tests",
                passing.len()
            ),
        )
    };

    GateOutcome {
        family: GateFamily::IntersectionUnionNonInferiority,
        decision,
        passing,
        regressed,
        inconclusive,
        rationale,
    }
}

/// One declared regression hypothesis after family-wise correction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegressionDeclaration {
    /// Metric identifier.
    pub metric_id: String,
    /// One-sided p-value for the reverse regression hypothesis.
    pub raw_p_value: f64,
    /// Holm-adjusted p-value.
    pub adjusted_p_value: f64,
    /// Whether a regression is declared at the family alpha.
    pub declared: bool,
}

/// Applies Holm's step-down correction to the reverse regression hypotheses.
///
/// Metrics without a regression p-value are skipped: no p-value means no
/// hypothesis in this family, and silently substituting one would change the
/// family size and therefore every adjustment.
#[must_use]
pub fn holm_regression_family(
    decisions: &[MetricDecision],
    family_alpha: f64,
) -> Vec<RegressionDeclaration> {
    let entries = decisions
        .iter()
        .filter(|decision| {
            decision.gate_kind.is_blocking()
                && decision.hypothesis_family == HypothesisFamily::Primary
        })
        .filter_map(|decision| {
            decision
                .regression_p_value
                .map(|p_value| (decision.metric_id.clone(), p_value))
        })
        .collect::<Vec<_>>();
    let adjusted = holm_adjusted_p_values(&entries.iter().map(|(_, p)| *p).collect::<Vec<_>>());
    entries
        .into_iter()
        .zip(adjusted)
        .map(
            |((metric_id, raw_p_value), adjusted_p_value)| RegressionDeclaration {
                metric_id,
                raw_p_value,
                adjusted_p_value,
                declared: adjusted_p_value <= family_alpha,
            },
        )
        .collect()
}

/// Returns Holm step-down adjusted p-values in the input order.
#[must_use]
pub fn holm_adjusted_p_values(p_values: &[f64]) -> Vec<f64> {
    let order = ascending_order(p_values);
    let count = p_values.len();
    let mut adjusted = vec![0.0; count];
    let mut running_max = 0.0_f64;
    for (rank_zero_based, index) in order.into_iter().enumerate() {
        let scaled = (p_values[index] * (count - rank_zero_based) as f64).min(1.0);
        running_max = running_max.max(scaled);
        adjusted[index] = running_max;
    }
    adjusted
}

/// Returns Benjamini-Hochberg adjusted p-values in the input order.
///
/// Exploratory diagnostics only. A BH-controlled discovery is not a gate.
#[must_use]
pub fn benjamini_hochberg_adjusted_p_values(p_values: &[f64]) -> Vec<f64> {
    let order = ascending_order(p_values);
    let count = p_values.len();
    let mut adjusted = vec![0.0; count];
    let mut running_min = 1.0_f64;
    for (rank_zero_based, index) in order.into_iter().enumerate().rev() {
        let rank = rank_zero_based + 1;
        let scaled = (p_values[index] * count as f64 / rank as f64).min(1.0);
        running_min = running_min.min(scaled);
        adjusted[index] = running_min;
    }
    adjusted
}

fn ascending_order(p_values: &[f64]) -> Vec<usize> {
    let mut order = (0..p_values.len()).collect::<Vec<_>>();
    order.sort_by(|left, right| p_values[*left].total_cmp(&p_values[*right]));
    order
}

/// Errors returned when a decision cannot be made from the given inputs.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DecisionError {
    /// The metric declaration was invalid.
    #[error(transparent)]
    Definition(#[from] MetricDefinitionError),
    /// An exact metric was decided through an inferential interval.
    #[error("metric {metric_id}: exact metrics are decided by count, not by an interval")]
    ExactMetricNeedsExactDecision {
        /// Metric identifier.
        metric_id: String,
    },
    /// A sampled metric was decided as an exact count.
    #[error("metric {metric_id}: sampled metrics require an interval decision")]
    SampledMetricNeedsInterval {
        /// Metric identifier.
        metric_id: String,
    },
    /// The interval contained a non-finite bound.
    #[error("metric {metric_id}: interval bounds must be finite")]
    NonFiniteInterval {
        /// Metric identifier.
        metric_id: String,
    },
    /// The interval bounds were inverted.
    #[error("metric {metric_id}: interval lower {lower} exceeds upper {upper}")]
    InvertedInterval {
        /// Metric identifier.
        metric_id: String,
        /// Lower bound.
        lower: f64,
        /// Upper bound.
        upper: f64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metric::{
        ConfidenceMethod, Estimand, Estimator, MetricClass, MetricDirection, MetricUnit,
        ResamplingPlan,
    };

    fn sampled_metric(id: &str) -> MetricDefinition {
        MetricDefinition {
            id: id.to_string(),
            direction: MetricDirection::HigherIsBetter,
            estimand: Estimand {
                class: MetricClass::PairedNumeric,
                summary: "mean paired delta".to_string(),
                target_population: "corpus users".to_string(),
            },
            unit: MetricUnit::Proportion,
            independent_unit: "user".to_string(),
            cluster_key: Some("user_id".to_string()),
            paired_key: Some("probe_id".to_string()),
            estimator: Estimator::MeanPairedDelta,
            practical_margin: Some(0.02),
            alpha: 0.025,
            confidence_method: ConfidenceMethod::ClusterPairedDeltaBootstrap(ResamplingPlan {
                resamples: 256,
                seed: 11,
                min_independent_units: 12,
            }),
            acceptable_alternative: Some(0.0),
            unacceptable_alternative: Some(-0.05),
            gate_kind: GateKind::RequiredNonInferiority,
            hypothesis_family: HypothesisFamily::Primary,
        }
    }

    fn exact_metric(id: &str) -> MetricDefinition {
        MetricDefinition {
            id: id.to_string(),
            direction: MetricDirection::LowerIsBetter,
            estimand: Estimand {
                class: MetricClass::FixedCorpusInvariant,
                summary: "cross-user leaks".to_string(),
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

    fn sufficient_support() -> SupportSummary {
        SupportSummary {
            independent_units: 20,
            observations: 80,
            required_independent_units: 12,
        }
    }

    #[test]
    fn three_way_rule_is_exact_at_both_margin_boundaries() {
        // Pins: PASS at lower == -margin, REGRESSION only strictly below
        // -margin, INCONCLUSIVE in between. A flipped comparison or a
        // strict/non-strict swap changes at least one of these.
        let margin = 0.02;
        assert_eq!(
            decide_utility_interval(margin, &UtilityInterval::new(-0.01, -0.02, 0.00)),
            Decision::Pass
        );
        assert_eq!(
            decide_utility_interval(margin, &UtilityInterval::new(-0.03, -0.05, -0.020_000_1)),
            Decision::Regression
        );
        assert_eq!(
            decide_utility_interval(margin, &UtilityInterval::new(-0.03, -0.05, -0.02)),
            Decision::Inconclusive
        );
        assert_eq!(
            decide_utility_interval(margin, &UtilityInterval::new(-0.02, -0.020_000_1, 0.01)),
            Decision::Inconclusive
        );
    }

    #[test]
    fn exact_no_change_and_known_regression_fixtures_decide_as_expected() {
        // Pins: an exactly zero paired delta passes, and an interval entirely
        // below -margin is a regression rather than an inconclusive result.
        let definition = sampled_metric("recall_at_4");
        let no_change = decide_metric(
            &definition,
            &UtilityInterval::new(0.0, 0.0, 0.0),
            sufficient_support(),
            Some(1.0),
        )
        .expect("no-change decision");
        assert_eq!(no_change.decision, Decision::Pass);
        assert_eq!(no_change.utility_delta, 0.0);

        let regression = decide_metric(
            &definition,
            &UtilityInterval::new(-0.09, -0.14, -0.05),
            sufficient_support(),
            Some(0.001),
        )
        .expect("regression decision");
        assert_eq!(regression.decision, Decision::Regression);
        assert!(regression.rationale.contains("upper_bound"));
    }

    #[test]
    fn insufficient_cluster_support_is_inconclusive_even_with_a_positive_interval() {
        // Pins: a percentile cluster bootstrap over a handful of clusters is
        // never presented as a population claim, however good it looks.
        let definition = sampled_metric("recall_at_4");
        let decision = decide_metric(
            &definition,
            &UtilityInterval::new(0.10, 0.05, 0.15),
            SupportSummary {
                independent_units: 5,
                observations: 40,
                required_independent_units: 12,
            },
            Some(0.99),
        )
        .expect("underpowered decision");

        assert_eq!(decision.decision, Decision::Inconclusive);
        assert!(decision.rationale.contains("insufficient support"));
        assert_eq!(decision.regression_p_value, None);
    }

    #[test]
    fn exact_and_sampled_decisions_refuse_each_others_inputs() {
        // Pins: exact metrics cannot be decided through an interval and
        // sampled metrics cannot be decided by a bare count.
        let exact = exact_metric("cross_user_leak_count");
        assert!(matches!(
            decide_metric(
                &exact,
                &UtilityInterval::new(0.0, 0.0, 0.0),
                sufficient_support(),
                None
            ),
            Err(DecisionError::ExactMetricNeedsExactDecision { .. })
        ));
        assert!(matches!(
            decide_exact_metric(&sampled_metric("recall_at_4"), 0, 0),
            Err(DecisionError::SampledMetricNeedsInterval { .. })
        ));

        assert_eq!(
            decide_exact_metric(&exact, 0, 0)
                .expect("exact pass")
                .decision,
            Decision::Pass
        );
        assert_eq!(
            decide_exact_metric(&exact, 1, 0)
                .expect("exact fail")
                .decision,
            Decision::Regression
        );
    }

    #[test]
    fn malformed_intervals_are_errors_rather_than_silent_passes() {
        // Pins: a NaN bound or inverted interval fails loudly instead of
        // sliding into the PASS branch.
        let definition = sampled_metric("recall_at_4");
        assert!(matches!(
            decide_metric(
                &definition,
                &UtilityInterval::new(f64::NAN, 0.0, 0.0),
                sufficient_support(),
                None
            ),
            Err(DecisionError::NonFiniteInterval { .. })
        ));
        assert!(matches!(
            decide_metric(
                &definition,
                &UtilityInterval::new(0.0, 0.1, -0.1),
                sufficient_support(),
                None
            ),
            Err(DecisionError::InvertedInterval { .. })
        ));
    }

    #[test]
    fn intersection_union_gate_requires_every_required_metric_to_pass() {
        // Pins: one regression fails the gate, one inconclusive metric blocks
        // it, and diagnostics never influence it.
        let pass = decide_metric(
            &sampled_metric("recall_at_4"),
            &UtilityInterval::new(0.01, -0.01, 0.03),
            sufficient_support(),
            Some(1.0),
        )
        .expect("pass");
        let regression = decide_metric(
            &sampled_metric("mrr"),
            &UtilityInterval::new(-0.09, -0.14, -0.05),
            sufficient_support(),
            Some(0.001),
        )
        .expect("regression");
        let underpowered = decide_metric(
            &sampled_metric("ndcg_at_4"),
            &UtilityInterval::new(0.01, -0.01, 0.03),
            SupportSummary {
                independent_units: 3,
                observations: 9,
                required_independent_units: 12,
            },
            None,
        )
        .expect("inconclusive");
        let mut diagnostic_definition = sampled_metric("pre_rerank_recall");
        diagnostic_definition.gate_kind = GateKind::Diagnostic;
        diagnostic_definition.hypothesis_family = HypothesisFamily::Exploratory;
        let diagnostic = decide_metric(
            &diagnostic_definition,
            &UtilityInterval::new(-0.30, -0.40, -0.20),
            sufficient_support(),
            Some(0.0001),
        )
        .expect("diagnostic");

        assert_eq!(
            intersection_union_gate(&[pass.clone(), diagnostic.clone()]).decision,
            Decision::Pass
        );
        assert_eq!(
            intersection_union_gate(&[pass.clone(), underpowered.clone()]).decision,
            Decision::Inconclusive
        );
        let regressed_gate =
            intersection_union_gate(&[pass.clone(), underpowered, regression, diagnostic]);
        assert_eq!(regressed_gate.decision, Decision::Regression);
        assert_eq!(regressed_gate.regressed, vec!["mrr".to_string()]);
        assert_eq!(regressed_gate.passing, vec!["recall_at_4".to_string()]);
        assert_eq!(
            intersection_union_gate(&[]).decision,
            Decision::Inconclusive
        );
    }

    #[test]
    fn holm_controls_the_reverse_regression_family_more_strictly_than_bh() {
        // Pins: Holm is step-down with (n - rank + 1) scaling, BH is step-up
        // with n / rank, and the exploratory correction is never the stricter
        // of the two.
        let p_values = [0.01, 0.02, 0.30];
        let holm = holm_adjusted_p_values(&p_values);
        assert!((holm[0] - 0.03).abs() < 1e-12);
        assert!((holm[1] - 0.04).abs() < 1e-12);
        assert!((holm[2] - 0.30).abs() < 1e-12);

        let bh = benjamini_hochberg_adjusted_p_values(&p_values);
        assert!((bh[0] - 0.03).abs() < 1e-12);
        assert!((bh[1] - 0.03).abs() < 1e-12);
        assert!((bh[2] - 0.30).abs() < 1e-12);
        for (holm_value, bh_value) in holm.iter().zip(bh.iter()) {
            assert!(holm_value >= bh_value, "Holm must not be looser than BH");
        }

        assert_eq!(holm_adjusted_p_values(&[]), Vec::<f64>::new());
        assert!((holm_adjusted_p_values(&[0.6])[0] - 0.6).abs() < 1e-12);
    }

    #[test]
    fn regression_family_skips_metrics_without_a_regression_p_value() {
        // Pins: a metric with no reverse-hypothesis p-value is excluded from
        // the family rather than padding it and weakening the correction.
        let with_p = decide_metric(
            &sampled_metric("mrr"),
            &UtilityInterval::new(-0.09, -0.14, -0.05),
            sufficient_support(),
            Some(0.004),
        )
        .expect("regression");
        let without_p = decide_metric(
            &sampled_metric("ndcg_at_4"),
            &UtilityInterval::new(0.0, -0.01, 0.01),
            sufficient_support(),
            None,
        )
        .expect("pass");

        let family = holm_regression_family(&[with_p, without_p], 0.05);
        assert_eq!(family.len(), 1);
        assert_eq!(family[0].metric_id, "mrr");
        assert!((family[0].adjusted_p_value - 0.004).abs() < 1e-12);
        assert!(family[0].declared);
    }
}
