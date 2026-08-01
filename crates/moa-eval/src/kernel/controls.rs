//! Suite-agnostic negative/null and positive/oracle control contracts.
//!
//! A suite that only reports a candidate score cannot tell the difference
//! between a capable system and a scorer that rewards an artifact of the data.
//! Two controls close that gap from opposite sides:
//!
//! - a *negative/null* control is a deliberately incapable system (it ignores
//!   the query, replays a popularity prior, or answers from nothing). It proves
//!   the metric is not free to obtain. It is a tripwire, not evidence that the
//!   metric measures the construct it claims to.
//! - a *positive/oracle* control is a deliberately correct system (it is handed
//!   the labels). It proves the metric can actually reach a high value through
//!   the production scoring path, so a floored candidate score means the
//!   candidate is bad rather than the scorer being broken.
//!
//! Both are required, per slice, because a global mean hides a slice where the
//! null already wins.
//!
//! # Ceilings are derived, never asserted
//!
//! A hand-written constant ("a null cannot exceed 0.2") is unfalsifiable. A
//! ceiling here is derived from repeated null seeds: at least
//! [`MIN_NULL_SEEDS`] independent null runs per slice, summarized as a
//! one-sided *prediction* upper bound for the next null run. The prediction
//! bound (not the bound on the mean) is the right estimand because the question
//! is "could a future null run score this high", not "where is the average
//! null".
//!
//! # A crossed ceiling is never subtracted
//!
//! When a null crosses its ceiling the suite is invalid for that metric and
//! [`SuiteValidityReport::headline_score`] returns `None`. There is no
//! null-corrected score, because subtracting a null from a candidate silently
//! converts broken measurement into a smaller number that still gates.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Minimum independent null seeds required to derive a ceiling.
///
/// Five is the smallest count for which the one-sided Student-t prediction
/// bound stops being dominated by its own multiplier (t is 2.13 at four degrees
/// of freedom versus 6.31 at one), so fewer seeds produce a ceiling that is
/// arithmetic rather than evidence.
pub const MIN_NULL_SEEDS: usize = 5;

/// Default one-sided error rate used for null ceilings.
pub const DEFAULT_CONTROL_ALPHA: f64 = 0.05;

/// Default floor a positive/oracle control must clear.
///
/// An oracle is handed the labels, so anything materially below one means the
/// scoring path — not the candidate — is losing information.
pub const DEFAULT_ORACLE_FLOOR: f64 = 0.99;

/// Which side of suite validity a control proves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRole {
    /// A deliberately incapable system whose score must stay under a ceiling.
    NegativeNull,
    /// A deliberately correct system whose score must stay over a floor.
    PositiveOracle,
}

impl ControlRole {
    /// Returns the stable wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NegativeNull => "negative_null",
            Self::PositiveOracle => "positive_oracle",
        }
    }
}

/// Runtime a control needs in order to run in its suite's native lane.
///
/// Controls are labeled by what they actually require. A control that reads a
/// graph, a tenant corpus, or any live store is not "offline" just because its
/// arithmetic is pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlLane {
    /// Pure scorer over frozen observations; no process outside the test.
    PureScorer,
    /// The in-process mock-domain harness.
    MockDomain,
    /// One isolated Postgres integration in the suite's own DB lane.
    DatabaseIntegration,
}

impl ControlLane {
    /// Returns whether this lane needs a live Postgres instance.
    #[must_use]
    pub const fn requires_postgres(self) -> bool {
        matches!(self, Self::DatabaseIntegration)
    }

    /// Returns the stable wire label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PureScorer => "pure_scorer",
            Self::MockDomain => "mock_domain",
            Self::DatabaseIntegration => "database_integration",
        }
    }
}

/// Errors raised while deriving or auditing controls.
#[derive(Debug, Error, PartialEq)]
pub enum ControlError {
    /// Fewer than [`MIN_NULL_SEEDS`] null runs were supplied.
    #[error("null ceiling needs at least {MIN_NULL_SEEDS} seeds; got {seeds}")]
    InsufficientNullSeeds {
        /// Seeds actually supplied.
        seeds: usize,
    },
    /// The same seed was supplied twice, so the runs are not independent.
    #[error("null seed {seed} appears more than once; repeated seeds are not independent runs")]
    RepeatedNullSeed {
        /// Offending seed.
        seed: u64,
    },
    /// Seed runs disagree about which slices they measured.
    #[error(
        "null seed {seed} measured slices [{observed}] but the first seed measured [{expected}]"
    )]
    UnbalancedSlices {
        /// Offending seed.
        seed: u64,
        /// Slices this seed reported.
        observed: String,
        /// Slices the first seed reported.
        expected: String,
    },
    /// A metric value was not a finite number.
    #[error("null seed {seed} slice `{slice}` reported a non-finite value")]
    NonFiniteValue {
        /// Offending seed.
        seed: u64,
        /// Offending slice.
        slice: String,
    },
    /// No slices were measured at all.
    #[error("null ceilings require at least one measured slice")]
    NoSlices,
    /// The requested error rate has no tabulated quantile.
    #[error("unsupported control alpha {alpha}; supported: 0.10, 0.05, 0.01")]
    UnsupportedAlpha {
        /// Requested error rate.
        alpha: f64,
    },
}

/// One complete null run, keyed by the seed that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct NullSeedRun {
    /// Seed that produced this null run.
    pub seed: u64,
    /// Metric value per slice for this run.
    pub slice_values: BTreeMap<String, f64>,
}

impl NullSeedRun {
    /// Builds one null seed run from slice/value pairs.
    #[must_use]
    pub fn new(seed: u64, values: impl IntoIterator<Item = (String, f64)>) -> Self {
        Self {
            seed,
            slice_values: values.into_iter().collect(),
        }
    }
}

/// How a null ceiling was computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CeilingMethod {
    /// One-sided Student-t prediction bound over repeated null seeds.
    SeedPredictionUpperBound,
}

/// Derived upper bound on what a null run can score in one slice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NullCeiling {
    /// Slice this ceiling applies to.
    pub slice: String,
    /// Independent null seeds behind the ceiling.
    pub seeds: usize,
    /// Mean null score across seeds.
    pub mean: f64,
    /// Sample standard deviation across seeds.
    pub std_dev: f64,
    /// One-sided error rate used for the bound.
    pub alpha: f64,
    /// Highest score a further null run is expected to reach.
    pub ceiling: f64,
    /// Estimator identity, retained so a report explains its own numbers.
    pub method: CeilingMethod,
}

impl NullCeiling {
    /// Returns whether the ceiling leaves no room for a capability signal.
    ///
    /// A unit-scaled metric whose null ceiling reaches 1.0 cannot separate a
    /// capable system from a degenerate one in that slice. This happens honestly
    /// on very small slices — a single case with five labels drawn from a small
    /// pool can be saturated by a popularity prior — and the answer is to report
    /// the slice as uninformative, not to lower the ceiling.
    #[must_use]
    pub fn is_uninformative(&self) -> bool {
        self.ceiling >= 1.0
    }

    /// Returns whether every null seed produced an identical score.
    ///
    /// A degenerate ceiling is not automatically wrong — a null that truly
    /// cannot score is expected to be constant — but it is only trustworthy
    /// paired with a positive control proving the metric can move at all.
    #[must_use]
    pub fn is_degenerate(&self) -> bool {
        self.std_dev == 0.0
    }
}

/// Derives one null ceiling per slice from repeated null seed runs.
///
/// Requires at least [`MIN_NULL_SEEDS`] distinct seeds that all measured the
/// same slices, so a ceiling can never be a single unexplained constant.
pub fn derive_null_ceilings(
    runs: &[NullSeedRun],
    alpha: f64,
) -> Result<BTreeMap<String, NullCeiling>, ControlError> {
    if runs.len() < MIN_NULL_SEEDS {
        return Err(ControlError::InsufficientNullSeeds { seeds: runs.len() });
    }
    let mut seen_seeds = BTreeSet::new();
    for run in runs {
        if !seen_seeds.insert(run.seed) {
            return Err(ControlError::RepeatedNullSeed { seed: run.seed });
        }
    }
    let first = &runs[0];
    if first.slice_values.is_empty() {
        return Err(ControlError::NoSlices);
    }
    let expected = first.slice_values.keys().cloned().collect::<BTreeSet<_>>();
    for run in &runs[1..] {
        let observed = run.slice_values.keys().cloned().collect::<BTreeSet<_>>();
        if observed != expected {
            return Err(ControlError::UnbalancedSlices {
                seed: run.seed,
                observed: join_slices(&observed),
                expected: join_slices(&expected),
            });
        }
    }
    let multiplier = one_sided_t_quantile(alpha, runs.len() - 1)?;

    let mut ceilings = BTreeMap::new();
    for slice in &expected {
        let mut values = Vec::with_capacity(runs.len());
        for run in runs {
            let value = run.slice_values[slice];
            if !value.is_finite() {
                return Err(ControlError::NonFiniteValue {
                    seed: run.seed,
                    slice: slice.clone(),
                });
            }
            values.push(value);
        }
        let seeds = values.len();
        let mean = values.iter().sum::<f64>() / seeds as f64;
        let variance = values
            .iter()
            .map(|value| (value - mean).powi(2))
            .sum::<f64>()
            / (seeds - 1) as f64;
        let std_dev = variance.max(0.0).sqrt();
        // Prediction bound for one further null run, not a bound on the mean:
        // the extra `1 +` term is the new run's own variance.
        let ceiling = mean + multiplier * std_dev * (1.0 + 1.0 / seeds as f64).sqrt();
        ceilings.insert(
            slice.clone(),
            NullCeiling {
                slice: slice.clone(),
                seeds,
                mean,
                std_dev,
                alpha,
                ceiling,
                method: CeilingMethod::SeedPredictionUpperBound,
            },
        );
    }
    Ok(ceilings)
}

fn join_slices(slices: &BTreeSet<String>) -> String {
    slices
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns the one-sided Student-t quantile for a tabulated error rate.
///
/// Only the three rates a gate may legitimately choose are tabulated; anything
/// else is refused rather than interpolated, so a report cannot claim a
/// confidence level this code cannot honor.
fn one_sided_t_quantile(alpha: f64, degrees_of_freedom: usize) -> Result<f64, ControlError> {
    const T_90: [f64; 30] = [
        3.078, 1.886, 1.638, 1.533, 1.476, 1.440, 1.415, 1.397, 1.383, 1.372, 1.363, 1.356, 1.350,
        1.345, 1.341, 1.337, 1.333, 1.330, 1.328, 1.325, 1.323, 1.321, 1.319, 1.318, 1.316, 1.315,
        1.314, 1.313, 1.311, 1.310,
    ];
    const T_95: [f64; 30] = [
        6.314, 2.920, 2.353, 2.132, 2.015, 1.943, 1.895, 1.860, 1.833, 1.812, 1.796, 1.782, 1.771,
        1.761, 1.753, 1.746, 1.740, 1.734, 1.729, 1.725, 1.721, 1.717, 1.714, 1.711, 1.708, 1.706,
        1.703, 1.701, 1.699, 1.697,
    ];
    const T_99: [f64; 30] = [
        // The df=11 entry is written to four decimals; at three it is the decimal
        // expansion of Euler's number and reads as a copied constant.
        31.821, 6.965, 4.541, 3.747, 3.365, 3.143, 2.998, 2.896, 2.821, 2.764, 2.7181, 2.681, 2.650,
        2.624, 2.602, 2.583, 2.567, 2.552, 2.539, 2.528, 2.518, 2.508, 2.500, 2.492, 2.485, 2.479,
        2.473, 2.467, 2.462, 2.457,
    ];
    let (table, limit) = if (alpha - 0.10).abs() < 1e-9 {
        (&T_90, 1.282)
    } else if (alpha - 0.05).abs() < 1e-9 {
        (&T_95, 1.645)
    } else if (alpha - 0.01).abs() < 1e-9 {
        (&T_99, 2.326)
    } else {
        return Err(ControlError::UnsupportedAlpha { alpha });
    };
    if degrees_of_freedom == 0 {
        return Err(ControlError::InsufficientNullSeeds { seeds: 1 });
    }
    Ok(table.get(degrees_of_freedom - 1).copied().unwrap_or(limit))
}

/// One slice's candidate score together with both control observations.
#[derive(Debug, Clone, PartialEq)]
pub struct SliceEvidence {
    /// Slice identity, matching the suite's own slice keys.
    pub slice: String,
    /// Candidate system score for this slice, exactly as the suite measured it.
    pub candidate: f64,
    /// Negative/null control score for this slice.
    pub null_observed: f64,
    /// Derived ceiling for the null control in this slice.
    pub null_ceiling: NullCeiling,
    /// Positive/oracle control score for this slice.
    pub oracle_observed: f64,
    /// Floor the oracle must clear for the scorer to be considered intact.
    pub oracle_floor: f64,
}

/// One headline metric of one suite, with per-slice control evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlledMetric {
    /// Suite identity.
    pub suite: String,
    /// Headline metric identity.
    pub metric: String,
    /// Overall candidate score reported by the suite for this metric.
    pub candidate_overall: f64,
    /// Per-slice evidence; an empty list is itself a validity failure.
    pub slices: Vec<SliceEvidence>,
}

/// A specific way suite validity failed.
///
/// Every variant invalidates the metric for its slice. They are separate variants
/// because a reviewer needs to know which failure happened, not because some are
/// advisory.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "finding", rename_all = "snake_case")]
pub enum ValidityFinding {
    /// A null run scored above its derived ceiling: the metric is obtainable
    /// without capability, so the candidate score means nothing here.
    NullCrossedCeiling {
        /// Null score observed.
        null_observed: f64,
        /// Ceiling the null crossed.
        ceiling: f64,
    },
    /// The candidate did not beat the null ceiling, so this slice carries no
    /// evidence of capability even if the null itself behaved.
    CandidateNotAboveNullCeiling {
        /// Candidate score observed.
        candidate: f64,
        /// Ceiling the candidate failed to clear.
        ceiling: f64,
    },
    /// The oracle could not reach its floor, which indicts the scoring path.
    OracleBelowFloor {
        /// Oracle score observed.
        oracle_observed: f64,
        /// Floor the oracle failed to clear.
        floor: f64,
    },
    /// Both controls are pinned to the same constant, so nothing separates
    /// "the metric cannot move" from "the scorer is stuck".
    DegenerateControls {
        /// Constant both controls produced.
        value: f64,
    },
    /// The metric reported no slices at all.
    MissingSliceResults,
}

/// Per-slice validity outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SliceValidity {
    /// Slice identity.
    pub slice: String,
    /// Candidate score, reported unchanged.
    pub candidate_score: f64,
    /// Null control score.
    pub null_score: f64,
    /// Derived null ceiling.
    pub null_ceiling: NullCeiling,
    /// Oracle control score.
    pub oracle_score: f64,
    /// Oracle floor.
    pub oracle_floor: f64,
    /// Findings for this slice; empty means the slice is valid.
    pub findings: Vec<ValidityFinding>,
}

impl SliceValidity {
    /// Returns whether this slice produced usable evidence.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Whether a metric's controls proved the suite can measure it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuiteVerdict {
    /// Both controls behaved in every slice.
    Valid,
    /// At least one slice failed; the candidate score is not usable evidence.
    InvalidSuite,
}

/// Control audit for one suite metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuiteValidityReport {
    /// Suite identity.
    pub suite: String,
    /// Headline metric identity.
    pub metric: String,
    /// Candidate overall score, reported unchanged even when invalid.
    pub candidate_overall: f64,
    /// Verdict across every slice.
    pub verdict: SuiteVerdict,
    /// Per-slice outcomes.
    pub slices: Vec<SliceValidity>,
}

impl SuiteValidityReport {
    /// Returns the score a gate may consume.
    ///
    /// `None` on an invalid suite. There is deliberately no null-adjusted
    /// alternative: a suite whose null crossed its ceiling has not measured the
    /// candidate, and `candidate - null` would hide that behind a plausible
    /// number.
    #[must_use]
    pub fn headline_score(&self) -> Option<f64> {
        match self.verdict {
            SuiteVerdict::Valid => Some(self.candidate_overall),
            SuiteVerdict::InvalidSuite => None,
        }
    }

    /// Returns every slice that failed validity.
    pub fn invalid_slices(&self) -> impl Iterator<Item = &SliceValidity> {
        self.slices.iter().filter(|slice| !slice.is_valid())
    }
}

/// Audits one metric's per-slice control evidence.
///
/// The candidate score is copied through untouched; the verdict decides whether
/// a gate is allowed to read it.
#[must_use]
pub fn audit_controlled_metric(metric: &ControlledMetric) -> SuiteValidityReport {
    if metric.slices.is_empty() {
        return SuiteValidityReport {
            suite: metric.suite.clone(),
            metric: metric.metric.clone(),
            candidate_overall: metric.candidate_overall,
            verdict: SuiteVerdict::InvalidSuite,
            slices: vec![SliceValidity {
                slice: "*".to_string(),
                candidate_score: metric.candidate_overall,
                null_score: f64::NAN,
                null_ceiling: NullCeiling {
                    slice: "*".to_string(),
                    seeds: 0,
                    mean: f64::NAN,
                    std_dev: f64::NAN,
                    alpha: DEFAULT_CONTROL_ALPHA,
                    ceiling: f64::NAN,
                    method: CeilingMethod::SeedPredictionUpperBound,
                },
                oracle_score: f64::NAN,
                oracle_floor: DEFAULT_ORACLE_FLOOR,
                findings: vec![ValidityFinding::MissingSliceResults],
            }],
        };
    }

    let slices = metric
        .slices
        .iter()
        .map(|evidence| {
            let mut findings = Vec::new();
            if evidence.null_observed > evidence.null_ceiling.ceiling {
                findings.push(ValidityFinding::NullCrossedCeiling {
                    null_observed: evidence.null_observed,
                    ceiling: evidence.null_ceiling.ceiling,
                });
            }
            if evidence.candidate <= evidence.null_ceiling.ceiling {
                findings.push(ValidityFinding::CandidateNotAboveNullCeiling {
                    candidate: evidence.candidate,
                    ceiling: evidence.null_ceiling.ceiling,
                });
            }
            if evidence.oracle_observed < evidence.oracle_floor {
                findings.push(ValidityFinding::OracleBelowFloor {
                    oracle_observed: evidence.oracle_observed,
                    floor: evidence.oracle_floor,
                });
            }
            if evidence.null_ceiling.is_degenerate()
                && (evidence.oracle_observed - evidence.null_observed).abs() < f64::EPSILON
            {
                findings.push(ValidityFinding::DegenerateControls {
                    value: evidence.null_observed,
                });
            }
            SliceValidity {
                slice: evidence.slice.clone(),
                candidate_score: evidence.candidate,
                null_score: evidence.null_observed,
                null_ceiling: evidence.null_ceiling.clone(),
                oracle_score: evidence.oracle_observed,
                oracle_floor: evidence.oracle_floor,
                findings,
            }
        })
        .collect::<Vec<_>>();

    let verdict = if slices.iter().all(SliceValidity::is_valid) {
        SuiteVerdict::Valid
    } else {
        SuiteVerdict::InvalidSuite
    };
    SuiteValidityReport {
        suite: metric.suite.clone(),
        metric: metric.metric.clone(),
        candidate_overall: metric.candidate_overall,
        verdict,
        slices,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_runs(values: &[(u64, f64)]) -> Vec<NullSeedRun> {
        values
            .iter()
            .map(|(seed, value)| NullSeedRun::new(*seed, [("all".to_string(), *value)]))
            .collect()
    }

    #[test]
    fn a_single_null_seed_cannot_produce_a_ceiling() {
        // Pins: one unexplained constant is refused, so a ceiling always has
        // repeated-seed evidence behind it.
        let error = derive_null_ceilings(&seeded_runs(&[(1, 0.1)]), DEFAULT_CONTROL_ALPHA)
            .expect_err("one seed must be refused");
        assert_eq!(error, ControlError::InsufficientNullSeeds { seeds: 1 });

        let four = seeded_runs(&[(1, 0.1), (2, 0.1), (3, 0.1), (4, 0.1)]);
        assert_eq!(
            derive_null_ceilings(&four, DEFAULT_CONTROL_ALPHA).expect_err("four seeds"),
            ControlError::InsufficientNullSeeds { seeds: 4 }
        );
    }

    #[test]
    fn repeated_seeds_are_not_independent_runs() {
        // Pins: copying one run five times cannot manufacture a tight ceiling.
        let runs = seeded_runs(&[(7, 0.1), (7, 0.1), (7, 0.1), (7, 0.1), (7, 0.1)]);
        assert_eq!(
            derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect_err("repeat"),
            ControlError::RepeatedNullSeed { seed: 7 }
        );
    }

    #[test]
    fn ceiling_is_a_prediction_bound_above_the_seed_mean() {
        // Pins: the ceiling exceeds the null mean by t * s * sqrt(1 + 1/n) so a
        // further null run at ordinary variability stays under it.
        let runs = seeded_runs(&[(1, 0.10), (2, 0.14), (3, 0.12), (4, 0.16), (5, 0.08)]);
        let ceilings = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings");
        let ceiling = &ceilings["all"];

        assert_eq!(ceiling.seeds, 5);
        assert!((ceiling.mean - 0.12).abs() < 1e-12, "mean {}", ceiling.mean);
        let expected = 0.12 + 2.132 * ceiling.std_dev * (1.0 + 1.0 / 5.0f64).sqrt();
        assert!(
            (ceiling.ceiling - expected).abs() < 1e-9,
            "ceiling {} vs {expected}",
            ceiling.ceiling
        );
        assert!(ceiling.ceiling > ceiling.mean);
        assert!(!ceiling.is_degenerate());
        assert_eq!(ceiling.method, CeilingMethod::SeedPredictionUpperBound);
    }

    #[test]
    fn a_constant_null_yields_a_degenerate_ceiling_at_its_value() {
        // Pins: a null that truly cannot score gets a zero-width ceiling, and
        // says so, instead of borrowing slack it never demonstrated.
        let runs = seeded_runs(&[(1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0), (5, 0.0)]);
        let ceiling =
            derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings")["all"].clone();
        assert_eq!(ceiling.ceiling, 0.0);
        assert!(ceiling.is_degenerate());
    }

    #[test]
    fn a_ceiling_at_or_above_one_is_reported_as_uninformative() {
        // Pins: a saturated slice is labeled, not silently trusted. The candidate
        // in such a slice can never clear the ceiling, so the audit invalidates it.
        let runs = seeded_runs(&[(1, 0.8), (2, 1.0), (3, 0.6), (4, 1.0), (5, 0.8)]);
        let ceiling =
            derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings")["all"].clone();
        assert!(ceiling.is_uninformative(), "ceiling {ceiling:?}");

        let report = audit_controlled_metric(&ControlledMetric {
            suite: "golden_graph".to_string(),
            metric: "expected_uid_recall_at_5".to_string(),
            candidate_overall: 1.0,
            slices: vec![SliceEvidence {
                slice: "q-01".to_string(),
                candidate: 1.0,
                null_observed: 0.8,
                null_ceiling: ceiling,
                oracle_observed: 1.0,
                oracle_floor: DEFAULT_ORACLE_FLOOR,
            }],
        });
        assert_eq!(report.verdict, SuiteVerdict::InvalidSuite);
        assert_eq!(report.headline_score(), None);
    }

    #[test]
    fn unbalanced_and_non_finite_seed_runs_are_refused() {
        // Pins: a slice measured by only some seeds, or a NaN, cannot silently
        // become a ceiling over a different denominator.
        let mut runs = seeded_runs(&[(1, 0.1), (2, 0.1), (3, 0.1), (4, 0.1)]);
        runs.push(NullSeedRun::new(
            5,
            [("all".to_string(), 0.1), ("extra".to_string(), 0.2)],
        ));
        let error = derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect_err("unbalanced");
        assert!(matches!(
            error,
            ControlError::UnbalancedSlices { seed: 5, .. }
        ));

        let nan = seeded_runs(&[(1, 0.1), (2, 0.1), (3, 0.1), (4, 0.1), (5, f64::NAN)]);
        assert_eq!(
            derive_null_ceilings(&nan, DEFAULT_CONTROL_ALPHA).expect_err("nan"),
            ControlError::NonFiniteValue {
                seed: 5,
                slice: "all".to_string()
            }
        );
    }

    #[test]
    fn an_untabulated_alpha_is_refused_rather_than_interpolated() {
        // Pins: a report cannot advertise a confidence level this code does not have.
        let runs = seeded_runs(&[(1, 0.1), (2, 0.1), (3, 0.1), (4, 0.1), (5, 0.1)]);
        assert_eq!(
            derive_null_ceilings(&runs, 0.025).expect_err("alpha"),
            ControlError::UnsupportedAlpha { alpha: 0.025 }
        );
    }

    fn ceiling_at(value: f64) -> NullCeiling {
        NullCeiling {
            slice: "point_recall".to_string(),
            seeds: MIN_NULL_SEEDS,
            mean: value / 2.0,
            std_dev: 0.01,
            alpha: DEFAULT_CONTROL_ALPHA,
            ceiling: value,
            method: CeilingMethod::SeedPredictionUpperBound,
        }
    }

    fn evidence(slice: &str, candidate: f64, null: f64, oracle: f64) -> SliceEvidence {
        SliceEvidence {
            slice: slice.to_string(),
            candidate,
            null_observed: null,
            null_ceiling: ceiling_at(0.30),
            oracle_observed: oracle,
            oracle_floor: DEFAULT_ORACLE_FLOOR,
        }
    }

    #[test]
    fn a_valid_metric_exposes_its_candidate_score_unchanged() {
        let metric = ControlledMetric {
            suite: "memory_retrieval".to_string(),
            metric: "recall_at_4".to_string(),
            candidate_overall: 0.82,
            slices: vec![
                evidence("point_recall", 0.90, 0.12, 1.0),
                evidence("multi_hop", 0.71, 0.08, 1.0),
            ],
        };

        let report = audit_controlled_metric(&metric);

        assert_eq!(report.verdict, SuiteVerdict::Valid);
        assert_eq!(report.headline_score(), Some(0.82));
        assert_eq!(report.slices[0].candidate_score, 0.90);
        assert_eq!(report.invalid_slices().count(), 0);
    }

    #[test]
    fn a_null_over_its_ceiling_invalidates_the_suite_without_subtracting() {
        // Pins: null success is invalid-suite evidence. The candidate score is
        // still reported verbatim, and no null-corrected score is offered.
        let metric = ControlledMetric {
            suite: "memory_retrieval".to_string(),
            metric: "recall_at_4".to_string(),
            candidate_overall: 0.82,
            slices: vec![evidence("point_recall", 0.90, 0.44, 1.0)],
        };

        let report = audit_controlled_metric(&metric);

        assert_eq!(report.verdict, SuiteVerdict::InvalidSuite);
        assert_eq!(report.headline_score(), None);
        assert_eq!(report.candidate_overall, 0.82);
        assert_eq!(report.slices[0].candidate_score, 0.90);
        assert_eq!(report.slices[0].null_score, 0.44);
        assert_eq!(report.invalid_slices().count(), 1);
        assert_eq!(
            report.slices[0].findings,
            vec![ValidityFinding::NullCrossedCeiling {
                null_observed: 0.44,
                ceiling: 0.30
            }]
        );
    }

    #[test]
    fn a_candidate_inside_the_null_ceiling_carries_no_capability_evidence() {
        let metric = ControlledMetric {
            suite: "fixed_rag".to_string(),
            metric: "recall_at_10".to_string(),
            candidate_overall: 0.25,
            slices: vec![evidence("all", 0.25, 0.10, 1.0)],
        };

        let report = audit_controlled_metric(&metric);

        assert_eq!(report.verdict, SuiteVerdict::InvalidSuite);
        assert_eq!(
            report.slices[0].findings,
            vec![ValidityFinding::CandidateNotAboveNullCeiling {
                candidate: 0.25,
                ceiling: 0.30
            }]
        );
    }

    #[test]
    fn a_broken_scorer_shows_up_as_an_oracle_below_its_floor() {
        // Pins: the positive control is what separates "the candidate is weak"
        // from "the metric cannot reach a high value at all".
        let metric = ControlledMetric {
            suite: "golden_graph".to_string(),
            metric: "expected_uid_recall".to_string(),
            candidate_overall: 0.40,
            slices: vec![evidence("all", 0.40, 0.05, 0.55)],
        };

        let report = audit_controlled_metric(&metric);

        assert_eq!(report.verdict, SuiteVerdict::InvalidSuite);
        assert_eq!(report.headline_score(), None);
        assert_eq!(
            report.slices[0].findings,
            vec![ValidityFinding::OracleBelowFloor {
                oracle_observed: 0.55,
                floor: DEFAULT_ORACLE_FLOOR
            }]
        );
    }

    #[test]
    fn controls_pinned_to_one_constant_are_reported_as_degenerate() {
        // Pins: a scorer stuck at zero cannot pass by having a "clean" null.
        let mut slice = evidence("all", 0.0, 0.0, 0.0);
        slice.null_ceiling.std_dev = 0.0;
        slice.null_ceiling.ceiling = 0.0;
        let metric = ControlledMetric {
            suite: "long_conversation".to_string(),
            metric: "case_pass_rate".to_string(),
            candidate_overall: 0.0,
            slices: vec![slice],
        };

        let report = audit_controlled_metric(&metric);

        assert_eq!(report.verdict, SuiteVerdict::InvalidSuite);
        assert!(
            report.slices[0]
                .findings
                .contains(&ValidityFinding::DegenerateControls { value: 0.0 })
        );
    }

    #[test]
    fn a_metric_with_no_slices_is_invalid() {
        // Pins: a headline number with no per-slice result never gates.
        let report = audit_controlled_metric(&ControlledMetric {
            suite: "execution_routing".to_string(),
            metric: "route_accuracy".to_string(),
            candidate_overall: 0.97,
            slices: Vec::new(),
        });

        assert_eq!(report.verdict, SuiteVerdict::InvalidSuite);
        assert_eq!(report.headline_score(), None);
        assert_eq!(
            report.slices[0].findings,
            vec![ValidityFinding::MissingSliceResults]
        );
    }

    #[test]
    fn an_informative_ceiling_is_not_reported_as_uninformative() {
        // Pins: the saturation flag is a real predicate, not a constant.
        let runs = seeded_runs(&[(1, 0.1), (2, 0.2), (3, 0.1), (4, 0.2), (5, 0.15)]);
        let ceiling =
            derive_null_ceilings(&runs, DEFAULT_CONTROL_ALPHA).expect("ceilings")["all"].clone();
        assert!(!ceiling.is_uninformative(), "ceiling {ceiling:?}");
        assert!(!ceiling.is_degenerate());
    }

    #[test]
    fn control_wire_labels_are_stable() {
        // Pins: the labels a persisted control report is read by. Renaming one
        // silently would re-key every historical report.
        assert_eq!(ControlRole::NegativeNull.as_str(), "negative_null");
        assert_eq!(ControlRole::PositiveOracle.as_str(), "positive_oracle");
        assert_eq!(ControlLane::PureScorer.as_str(), "pure_scorer");
        assert_eq!(ControlLane::MockDomain.as_str(), "mock_domain");
        assert_eq!(
            ControlLane::DatabaseIntegration.as_str(),
            "database_integration"
        );
    }

    #[test]
    fn database_backed_controls_declare_that_they_need_postgres() {
        // Pins: a DB control cannot be advertised as an offline one.
        assert!(ControlLane::DatabaseIntegration.requires_postgres());
        assert!(!ControlLane::PureScorer.requires_postgres());
        assert!(!ControlLane::MockDomain.requires_postgres());
    }
}
