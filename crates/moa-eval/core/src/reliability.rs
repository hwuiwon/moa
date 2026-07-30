//! Shared repeat-reliability estimators for stochastic evaluation lanes.
//!
//! A lane that re-runs the same logical case several times can report how often a
//! single attempt succeeds and how often every attempt succeeds. Both quantities are
//! computed with the exact combinatorial estimators
//!
//! ```text
//! pass_any_at_k = 1 - C(n - c, k) / C(n, k)
//! pass_all_at_k =     C(c, k)     / C(n, k)
//! ```
//!
//! where `n` is the number of independent repetitions recorded for one case and `c`
//! is how many of them passed. The plug-in forms `(c / n)^k` and `1 - (1 - c/n)^k`
//! are deliberately not used: they are biased for small `n` and circulate only in
//! secondary summaries.
//!
//! Two rules make these numbers mean what they claim:
//!
//! 1. Every value is computed **per logical case** and only then averaged across
//!    cases. Successes are never pooled over different cases, because pooling
//!    silently answers a different question ("pick `k` runs from the whole suite")
//!    than the one a reliability report is read as ("re-run this case `k` times").
//! 2. Trials must be independent. Shared-prefix or branched rollouts break that
//!    assumption and are refused here; see [`ConditionalFailureDiscovery`] for the
//!    separately labeled diagnostic those rollouts support instead.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Schema version for [`CaseReliabilityReport`].
pub const RELIABILITY_SCHEMA_VERSION: u8 = 1;

/// Largest repetition count these estimators accept for one case.
///
/// Reliability lanes repeat a case a handful of times; the bound keeps binomial
/// arithmetic inside `u128` and rejects obviously malformed inputs early.
pub const MAX_REPETITIONS_PER_CASE: u32 = 125;

/// Resampling unit a reliability estimate is defined over.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReliabilityResamplingUnit {
    /// The logical evaluation case, with its repetitions nested inside it.
    Case,
}

/// How the repeated trials of one case were produced.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TrialIndependence {
    /// Every repetition re-ran the case from an independent start.
    IndependentRepetitions,
    /// Repetitions branched from a shared prefix or a common partial rollout.
    ///
    /// These are refused by [`estimate_reliability`] because they are correlated by
    /// construction.
    SharedPrefixBranched,
}

/// Independent repeated-trial counts for one logical evaluation case.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseTrials {
    /// Stable logical case identifier; repetitions of one case share it.
    pub case_id: String,
    /// Number of independent repetitions recorded for the case (`n`).
    pub attempts: u32,
    /// Number of those repetitions that passed (`c`).
    pub successes: u32,
}

impl CaseTrials {
    /// Builds validated per-case trial counts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] when the case identifier is blank, when
    /// `attempts` is zero or above [`MAX_REPETITIONS_PER_CASE`], or when
    /// `successes` exceeds `attempts`.
    pub fn new(case_id: impl Into<String>, attempts: u32, successes: u32) -> Result<Self> {
        let trials = Self {
            case_id: case_id.into(),
            attempts,
            successes,
        };
        trials.validate()?;
        Ok(trials)
    }

    /// Validates the case identity and trial counts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] when the identity is blank or the counts are
    /// out of range.
    pub fn validate(&self) -> Result<()> {
        if self.case_id.trim().is_empty() {
            return Err(invalid("reliability case ID cannot be empty".to_string()));
        }
        if self.attempts == 0 || self.attempts > MAX_REPETITIONS_PER_CASE {
            return Err(invalid(format!(
                "reliability case `{}` must record 1..={MAX_REPETITIONS_PER_CASE} attempts, got {}",
                self.case_id, self.attempts
            )));
        }
        if self.successes > self.attempts {
            return Err(invalid(format!(
                "reliability case `{}` records {} successes over {} attempts",
                self.case_id, self.successes, self.attempts
            )));
        }
        Ok(())
    }
}

/// Exact reliability of one logical case at a given draw size `k`.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseReliability {
    /// Stable logical case identifier.
    pub case_id: String,
    /// Independent repetitions recorded for the case (`n`).
    pub attempts: u32,
    /// Passing repetitions (`c`).
    pub successes: u32,
    /// Draw size the estimate is defined over (`k`).
    pub k: u32,
    /// Probability that a uniformly drawn `k`-subset contains at least one success.
    pub pass_any_at_k: f64,
    /// Probability that a uniformly drawn `k`-subset contains only successes.
    pub pass_all_at_k: f64,
}

/// Cross-case reliability aggregate at one draw size.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReliabilityEstimate {
    /// Draw size the estimate is defined over (`k`).
    pub k: u32,
    /// Number of logical cases averaged, the resampling unit.
    pub case_count: u64,
    /// Total repetitions across those cases, reported only as provenance.
    pub trial_count: u64,
    /// Mean of the per-case `pass_any_at_k` values.
    pub pass_any_at_k: f64,
    /// Mean of the per-case `pass_all_at_k` values.
    pub pass_all_at_k: f64,
    /// Whether every case shared one repetition count, allowing exact integer means.
    pub balanced_design: bool,
}

/// Repetition bookkeeping for one grouped reliability input.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepetitionIntegrity {
    /// Repetitions each case was supposed to record.
    pub expected_repetitions: u32,
    /// Cases the lane declared, whether or not any row arrived for them.
    pub expected_cases: u64,
    /// Rows supplied to grouping, including rejected ones.
    pub observed_rows: u64,
    /// Rows accepted as distinct `(case, repetition)` trials.
    pub counted_trials: u64,
    /// Expected trials that never produced a row.
    pub missing_repetitions: u64,
    /// Rows repeating a `(case, repetition)` identity already accepted.
    pub duplicate_repetitions: u64,
    /// Rows whose repetition number fell outside `1..=expected_repetitions`.
    pub out_of_range_repetitions: u64,
    /// Rows naming a case the lane did not declare.
    pub unknown_case_rows: u64,
}

impl RepetitionIntegrity {
    /// Reports whether every declared trial arrived exactly once.
    pub const fn is_complete(&self) -> bool {
        self.missing_repetitions == 0
            && self.duplicate_repetitions == 0
            && self.out_of_range_repetitions == 0
            && self.unknown_case_rows == 0
    }
}

/// One independent repetition outcome, before grouping into per-case counts.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepetitionObservation {
    /// Stable logical case identifier this repetition belongs to.
    pub case_id: String,
    /// One-based repetition number within the case.
    pub repetition: u32,
    /// Whether this repetition passed every case-level gate.
    pub passed: bool,
}

/// Provenance required to interpret a reliability report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReliabilityProvenance {
    /// Stable provider identifier that served the repetitions.
    pub provider: String,
    /// Stable model identifier.
    pub model: String,
    /// Stable prompt or cassette version.
    pub prompt_version: String,
    /// Corpus seeds recorded so case selection and ordering can be reproduced.
    ///
    /// A seed is provenance only. It fixes the fixture, not the provider: recording
    /// it never makes a sampled provider deterministic, which is exactly why the
    /// repetitions in this report are needed.
    pub seeds: Vec<u64>,
    /// Summed reconciled cost across every counted trial.
    pub cost_microusd: u64,
    /// Whether every counted trial contributed a reconciled cost.
    pub cost_complete: bool,
}

/// Case-level repeat-reliability report for one stochastic lane run.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseReliabilityReport {
    /// Report schema version, fixed at [`RELIABILITY_SCHEMA_VERSION`].
    pub schema_version: u8,
    /// Resampling unit the curve is defined over; always the logical case.
    pub resampling_unit: ReliabilityResamplingUnit,
    /// Independence claim the estimators were allowed to rely on.
    pub independence: TrialIndependence,
    /// Repetition bookkeeping behind the per-case counts.
    pub integrity: RepetitionIntegrity,
    /// Seed, model, and cost provenance.
    pub provenance: ReliabilityProvenance,
    /// Per-case trial counts, the primary unit with repetitions nested inside.
    pub cases: Vec<CaseTrials>,
    /// Aggregate estimate for every draw size the design supports.
    pub curve: Vec<ReliabilityEstimate>,
    /// Population variance of pooled repetition outcomes, `p * (1 - p)`.
    pub pooled_outcome_variance: Option<f64>,
}

impl CaseReliabilityReport {
    /// Builds a report and its full `k` curve from grouped per-case trial counts.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] when `cases` is empty, contains a duplicate
    /// or invalid case, or declares [`TrialIndependence::SharedPrefixBranched`].
    pub fn new(
        cases: Vec<CaseTrials>,
        independence: TrialIndependence,
        integrity: RepetitionIntegrity,
        provenance: ReliabilityProvenance,
    ) -> Result<Self> {
        let curve = reliability_curve(&cases, independence)?;
        Ok(Self {
            schema_version: RELIABILITY_SCHEMA_VERSION,
            resampling_unit: ReliabilityResamplingUnit::Case,
            independence,
            integrity,
            provenance,
            pooled_outcome_variance: pooled_outcome_variance(&cases),
            cases,
            curve,
        })
    }

    /// Returns the aggregate estimate for one draw size, if the curve covers it.
    pub fn at_k(&self, k: u32) -> Option<&ReliabilityEstimate> {
        self.curve.iter().find(|estimate| estimate.k == k)
    }
}

/// Conditional failure-discovery diagnostic for shared-prefix or branched rollouts.
///
/// Branches that share a prefix are correlated by construction, so they cannot be
/// treated as independent draws and never feed [`pass_any_at_k`] or
/// [`pass_all_at_k`]. This type exists so such rollouts can still be surfaced, under
/// their own name, as a statement about failures discovered *conditional on* the
/// shared prefix.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConditionalFailureDiscovery {
    /// Stable logical case identifier the branches were taken from.
    pub case_id: String,
    /// Branches explored from the shared prefix.
    pub branch_count: u32,
    /// Branches that failed.
    pub failing_branches: u32,
    /// Failing branches over all branches, conditional on the shared prefix.
    pub conditional_failure_rate: f64,
}

impl ConditionalFailureDiscovery {
    /// Builds a branched-rollout diagnostic.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidConfig`] when the case identifier is blank, when no
    /// branches were explored, or when failures exceed branches.
    pub fn new(
        case_id: impl Into<String>,
        branch_count: u32,
        failing_branches: u32,
    ) -> Result<Self> {
        let case_id = case_id.into();
        if case_id.trim().is_empty() || branch_count == 0 || failing_branches > branch_count {
            return Err(invalid(
                "branched failure-discovery diagnostic requires a case ID and 0..=branch failures"
                    .to_string(),
            ));
        }
        Ok(Self {
            conditional_failure_rate: f64::from(failing_branches) / f64::from(branch_count),
            case_id,
            branch_count,
            failing_branches,
        })
    }
}

/// Computes `1 - C(n - c, k) / C(n, k)` for one case.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when `k` is zero, when `k > attempts`, when
/// `successes > attempts`, or when the binomial arithmetic overflows.
pub fn pass_any_at_k(attempts: u32, successes: u32, k: u32) -> Result<f64> {
    let (numerator, denominator) = pass_any_ratio(attempts, successes, k)?;
    Ok(numerator as f64 / denominator as f64)
}

/// Computes `C(c, k) / C(n, k)` for one case.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when `k` is zero, when `k > attempts`, when
/// `successes > attempts`, or when the binomial arithmetic overflows.
pub fn pass_all_at_k(attempts: u32, successes: u32, k: u32) -> Result<f64> {
    let (numerator, denominator) = pass_all_ratio(attempts, successes, k)?;
    Ok(numerator as f64 / denominator as f64)
}

/// Computes both estimators for one case at one draw size.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when the trial counts are invalid or `k` is
/// outside `1..=attempts`.
pub fn case_reliability(trials: &CaseTrials, k: u32) -> Result<CaseReliability> {
    trials.validate()?;
    Ok(CaseReliability {
        case_id: trials.case_id.clone(),
        attempts: trials.attempts,
        successes: trials.successes,
        k,
        pass_any_at_k: pass_any_at_k(trials.attempts, trials.successes, k)?,
        pass_all_at_k: pass_all_at_k(trials.attempts, trials.successes, k)?,
    })
}

/// Averages the per-case estimators across every case at one draw size.
///
/// Each case contributes exactly one `pass_any_at_k` and one `pass_all_at_k`;
/// successes are never pooled across cases. When every case shares one repetition
/// count the means are accumulated as exact integers over the shared denominator,
/// so a balanced design yields the same `f64` a single ratio would.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when `cases` is empty, holds a duplicate or
/// invalid case, declares [`TrialIndependence::SharedPrefixBranched`], or when any
/// case recorded fewer than `k` repetitions.
pub fn estimate_reliability(
    cases: &[CaseTrials],
    k: u32,
    independence: TrialIndependence,
) -> Result<ReliabilityEstimate> {
    if independence == TrialIndependence::SharedPrefixBranched {
        return Err(invalid(
            "pass_any_at_k and pass_all_at_k require independent repetitions; report \
             shared-prefix or branched rollouts as conditional failure discovery instead"
                .to_string(),
        ));
    }
    if cases.is_empty() {
        return Err(invalid(
            "reliability estimation requires at least one case".to_string(),
        ));
    }
    if k == 0 {
        return Err(invalid(
            "reliability draw size k must be positive".to_string(),
        ));
    }
    validate_case_identities(cases)?;

    let case_count = u64::try_from(cases.len())
        .map_err(|_| invalid("reliability case count exceeds u64".to_string()))?;
    let mut trial_count = 0_u64;
    for case in cases {
        trial_count = trial_count
            .checked_add(u64::from(case.attempts))
            .ok_or_else(|| invalid("reliability trial count overflowed u64".to_string()))?;
    }

    let shared_denominator = cases
        .iter()
        .all(|case| case.attempts == cases[0].attempts)
        .then(|| binomial(cases[0].attempts, k))
        .transpose()?;

    let (pass_any, pass_all) = match shared_denominator {
        Some(denominator) => {
            let mut any_numerator = 0_u128;
            let mut all_numerator = 0_u128;
            for case in cases {
                let (case_any, _) = pass_any_ratio(case.attempts, case.successes, k)?;
                let (case_all, _) = pass_all_ratio(case.attempts, case.successes, k)?;
                any_numerator = checked_add(any_numerator, case_any)?;
                all_numerator = checked_add(all_numerator, case_all)?;
            }
            let total = denominator
                .checked_mul(u128::from(case_count))
                .ok_or_else(|| invalid("reliability denominator overflowed u128".to_string()))?;
            (
                any_numerator as f64 / total as f64,
                all_numerator as f64 / total as f64,
            )
        }
        None => {
            let mut any_sum = 0.0_f64;
            let mut all_sum = 0.0_f64;
            for case in cases {
                any_sum += pass_any_at_k(case.attempts, case.successes, k)?;
                all_sum += pass_all_at_k(case.attempts, case.successes, k)?;
            }
            let cases_f64 = case_count as f64;
            (
                (any_sum / cases_f64).clamp(0.0, 1.0),
                (all_sum / cases_f64).clamp(0.0, 1.0),
            )
        }
    };

    Ok(ReliabilityEstimate {
        k,
        case_count,
        trial_count,
        pass_any_at_k: pass_any,
        pass_all_at_k: pass_all,
        balanced_design: shared_denominator.is_some(),
    })
}

/// Estimates reliability for every draw size the design supports, `1..=min(n)`.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] under the same conditions as
/// [`estimate_reliability`].
pub fn reliability_curve(
    cases: &[CaseTrials],
    independence: TrialIndependence,
) -> Result<Vec<ReliabilityEstimate>> {
    let max_k = cases
        .iter()
        .map(|case| case.attempts)
        .min()
        .ok_or_else(|| invalid("reliability curve requires at least one case".to_string()))?;
    (1..=max_k)
        .map(|k| estimate_reliability(cases, k, independence))
        .collect()
}

/// Returns `p * (1 - p)` for the pooled repetition outcomes.
///
/// This deliberately pools every repetition row, so it describes the spread of
/// individual outcomes and must never be read as uncertainty of `pass_any_at_k`
/// across cases; the per-case curve is the case-level view.
pub fn pooled_outcome_variance(cases: &[CaseTrials]) -> Option<f64> {
    let attempts = cases
        .iter()
        .map(|case| u64::from(case.attempts))
        .try_fold(0_u64, u64::checked_add)?;
    let successes = cases
        .iter()
        .map(|case| u64::from(case.successes))
        .try_fold(0_u64, u64::checked_add)?;
    if attempts == 0 {
        return None;
    }
    let rate = successes as f64 / attempts as f64;
    Some(rate * (1.0 - rate))
}

/// Groups repetition rows into per-case trial counts and reports their integrity.
///
/// The first row for a `(case, repetition)` identity wins; later rows are counted as
/// duplicates and dropped so a repeated run cannot inflate `n`. Rows outside
/// `1..=expected_repetitions`, and rows naming an undeclared case, are counted and
/// dropped the same way.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when `expected_repetitions` is zero or above
/// [`MAX_REPETITIONS_PER_CASE`], when `case_ids` is empty, or when it repeats an
/// identifier.
pub fn group_repetitions<S: AsRef<str>>(
    case_ids: &[S],
    expected_repetitions: u32,
    observations: &[RepetitionObservation],
) -> Result<(Vec<CaseTrials>, RepetitionIntegrity)> {
    if expected_repetitions == 0 || expected_repetitions > MAX_REPETITIONS_PER_CASE {
        return Err(invalid(format!(
            "reliability grouping requires 1..={MAX_REPETITIONS_PER_CASE} expected repetitions"
        )));
    }
    if case_ids.is_empty() {
        return Err(invalid(
            "reliability grouping requires at least one declared case".to_string(),
        ));
    }

    let mut seen = std::collections::BTreeSet::new();
    let mut order = Vec::with_capacity(case_ids.len());
    for case_id in case_ids {
        let case_id = case_id.as_ref();
        if case_id.trim().is_empty() || !seen.insert(case_id) {
            return Err(invalid(
                "reliability grouping case IDs must be unique and non-empty".to_string(),
            ));
        }
        order.push(case_id);
    }

    let mut counted = std::collections::BTreeMap::<&str, (u32, u32)>::new();
    let mut identities = std::collections::BTreeSet::<(&str, u32)>::new();
    let mut integrity = RepetitionIntegrity {
        expected_repetitions,
        expected_cases: u64::try_from(order.len())
            .map_err(|_| invalid("reliability case count exceeds u64".to_string()))?,
        observed_rows: u64::try_from(observations.len())
            .map_err(|_| invalid("reliability observation count exceeds u64".to_string()))?,
        ..RepetitionIntegrity::default()
    };

    for observation in observations {
        let Some(case_id) = seen.get(observation.case_id.as_str()).copied() else {
            integrity.unknown_case_rows += 1;
            continue;
        };
        if observation.repetition == 0 || observation.repetition > expected_repetitions {
            integrity.out_of_range_repetitions += 1;
            continue;
        }
        if !identities.insert((case_id, observation.repetition)) {
            integrity.duplicate_repetitions += 1;
            continue;
        }
        let entry = counted.entry(case_id).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += u32::from(observation.passed);
        integrity.counted_trials += 1;
    }

    let expected_trials = integrity
        .expected_cases
        .checked_mul(u64::from(expected_repetitions))
        .ok_or_else(|| invalid("reliability expected-trial count overflowed u64".to_string()))?;
    integrity.missing_repetitions = expected_trials.saturating_sub(integrity.counted_trials);

    let trials = order
        .into_iter()
        .filter_map(|case_id| {
            counted
                .get(case_id)
                .map(|(attempts, successes)| CaseTrials::new(case_id, *attempts, *successes))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((trials, integrity))
}

fn validate_case_identities(cases: &[CaseTrials]) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    for case in cases {
        case.validate()?;
        if !ids.insert(case.case_id.as_str()) {
            return Err(invalid(format!(
                "reliability cases repeat identity `{}`; repetitions of one case must be \
                 grouped into a single row",
                case.case_id
            )));
        }
    }
    Ok(())
}

fn pass_any_ratio(attempts: u32, successes: u32, k: u32) -> Result<(u128, u128)> {
    let denominator = checked_draw(attempts, successes, k)?;
    let failures = attempts - successes;
    let all_failing = binomial(failures, k)?;
    Ok((denominator - all_failing, denominator))
}

fn pass_all_ratio(attempts: u32, successes: u32, k: u32) -> Result<(u128, u128)> {
    let denominator = checked_draw(attempts, successes, k)?;
    Ok((binomial(successes, k)?, denominator))
}

fn checked_draw(attempts: u32, successes: u32, k: u32) -> Result<u128> {
    if attempts == 0 || attempts > MAX_REPETITIONS_PER_CASE {
        return Err(invalid(format!(
            "reliability estimation requires 1..={MAX_REPETITIONS_PER_CASE} attempts, got {attempts}"
        )));
    }
    if successes > attempts {
        return Err(invalid(format!(
            "reliability estimation received {successes} successes over {attempts} attempts"
        )));
    }
    if k == 0 {
        return Err(invalid(
            "reliability draw size k must be positive".to_string(),
        ));
    }
    if k > attempts {
        return Err(invalid(format!(
            "reliability draw size k={k} exceeds the {attempts} repetitions recorded for the case"
        )));
    }
    binomial(attempts, k)
}

fn binomial(n: u32, k: u32) -> Result<u128> {
    if k > n {
        return Ok(0);
    }
    let k = k.min(n - k);
    let mut value = 1_u128;
    for step in 0..k {
        value = value.checked_mul(u128::from(n - step)).ok_or_else(|| {
            invalid("reliability binomial coefficient overflowed u128".to_string())
        })?;
        value /= u128::from(step + 1);
    }
    Ok(value)
}

fn checked_add(left: u128, right: u128) -> Result<u128> {
    left.checked_add(right)
        .ok_or_else(|| invalid("reliability numerator overflowed u128".to_string()))
}

fn invalid(message: String) -> Error {
    Error::InvalidConfig(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trials(rows: &[(&str, u32, u32)]) -> Vec<CaseTrials> {
        rows.iter()
            .map(|(id, attempts, successes)| {
                CaseTrials::new(*id, *attempts, *successes).expect("valid trial counts")
            })
            .collect()
    }

    fn estimate(rows: &[(&str, u32, u32)], k: u32) -> ReliabilityEstimate {
        estimate_reliability(&trials(rows), k, TrialIndependence::IndependentRepetitions)
            .expect("valid reliability input")
    }

    #[test]
    fn pass_at_k_matches_closed_form_at_k_one() {
        // Pins: k=1 collapses to the observed per-case success rate for both estimators.
        for successes in 0..=5 {
            assert_eq!(
                pass_any_at_k(5, successes, 1).expect("valid draw"),
                f64::from(successes) / 5.0
            );
            assert_eq!(
                pass_all_at_k(5, successes, 1).expect("valid draw"),
                f64::from(successes) / 5.0
            );
        }
    }

    #[test]
    fn repetition_limit_matches_checked_binomial_algorithm_boundary() {
        // Pins: the largest accepted design completes its full curve, while the
        // first n whose intermediate multiply can overflow is rejected up front.
        let cases = trials(&[("boundary", MAX_REPETITIONS_PER_CASE, 62)]);
        let curve = reliability_curve(&cases, TrialIndependence::IndependentRepetitions)
            .expect("the declared repetition maximum must be computable");
        assert_eq!(curve.len(), MAX_REPETITIONS_PER_CASE as usize);
        assert_eq!(
            curve.last().map(|estimate| estimate.k),
            Some(MAX_REPETITIONS_PER_CASE)
        );

        assert!(
            CaseTrials::new("overflow", MAX_REPETITIONS_PER_CASE + 1, 63).is_err(),
            "the first unsafe design must be rejected before estimation"
        );
    }

    #[test]
    fn pass_at_k_matches_closed_form_at_k_equals_n() {
        // Pins: k=n is the all-or-nothing indicator, not a smoothed rate.
        assert_eq!(pass_all_at_k(5, 5, 5).expect("valid draw"), 1.0);
        assert_eq!(pass_all_at_k(5, 4, 5).expect("valid draw"), 0.0);
        assert_eq!(pass_any_at_k(5, 1, 5).expect("valid draw"), 1.0);
        assert_eq!(pass_any_at_k(5, 0, 5).expect("valid draw"), 0.0);
    }

    #[test]
    fn pass_at_k_handles_zero_and_full_success_counts() {
        // Pins: c=0 and c=n stay exactly at the 0/1 boundaries for every k.
        for k in 1..=4 {
            assert_eq!(pass_any_at_k(4, 0, k).expect("valid draw"), 0.0);
            assert_eq!(pass_all_at_k(4, 0, k).expect("valid draw"), 0.0);
            assert_eq!(pass_any_at_k(4, 4, k).expect("valid draw"), 1.0);
            assert_eq!(pass_all_at_k(4, 4, k).expect("valid draw"), 1.0);
        }
    }

    #[test]
    fn pass_at_k_uses_combinatorial_not_plug_in_estimator() {
        // Pins: the hypergeometric form, which differs from (c/n)^k and 1-(1-c/n)^k.
        // n=5, c=2, k=2: C(2,2)/C(5,2) = 1/10 and 1 - C(3,2)/C(5,2) = 1 - 3/10.
        assert_eq!(pass_all_at_k(5, 2, 2).expect("valid draw"), 0.1);
        assert_eq!(pass_any_at_k(5, 2, 2).expect("valid draw"), 0.7);
        let plug_in_all = (2.0_f64 / 5.0).powi(2);
        let plug_in_any = 1.0 - (1.0 - 2.0_f64 / 5.0).powi(2);
        assert!((plug_in_all - 0.1).abs() > 1e-9);
        assert!((plug_in_any - 0.7).abs() > 1e-9);
    }

    #[test]
    fn pass_at_k_is_monotone_in_k() {
        // Pins: more draws never lower pass-any and never raise pass-all.
        let rows = [("a", 8, 3), ("b", 8, 5), ("c", 8, 8), ("d", 8, 0)];
        for (id, attempts, successes) in rows {
            let mut previous_any = 0.0_f64;
            let mut previous_all = 1.0_f64;
            for k in 1..=attempts {
                let any = pass_any_at_k(attempts, successes, k).expect("valid draw");
                let all = pass_all_at_k(attempts, successes, k).expect("valid draw");
                assert!(any >= previous_any, "{id} pass_any decreased at k={k}");
                assert!(all <= previous_all, "{id} pass_all increased at k={k}");
                previous_any = any;
                previous_all = all;
            }
        }
    }

    #[test]
    fn aggregation_never_pools_successes_across_cases() {
        // Pins: per-case-then-average, which differs from drawing k runs suite-wide.
        let rows = [("a", 2, 2), ("b", 2, 0)];
        let estimate = estimate(&rows, 2);
        assert_eq!(estimate.pass_all_at_k, 0.5);
        assert_eq!(estimate.pass_any_at_k, 0.5);
        assert_eq!(estimate.case_count, 2);
        assert_eq!(estimate.trial_count, 4);

        // Pooling the same successes into one 4-attempt draw answers a different
        // question and must not be what the aggregate reports.
        let pooled_all = pass_all_at_k(4, 2, 2).expect("valid draw");
        let pooled_any = pass_any_at_k(4, 2, 2).expect("valid draw");
        assert!((pooled_all - estimate.pass_all_at_k).abs() > 1e-9);
        assert!((pooled_any - estimate.pass_any_at_k).abs() > 1e-9);
    }

    #[test]
    fn aggregation_rejects_repeated_case_identity() {
        // Pins: two rows for one case are a grouping bug, not two cases.
        let cases = trials(&[("a", 5, 3), ("a", 5, 2)]);
        let error = estimate_reliability(&cases, 1, TrialIndependence::IndependentRepetitions)
            .expect_err("repeated case identity must be refused");
        assert!(error.to_string().contains("repeat identity"));
    }

    #[test]
    fn balanced_design_matches_a_single_pooled_ratio_bit_for_bit() {
        // Pins: the extraction reproduces the previous count-ratio arithmetic exactly.
        let rows = [("a", 5, 3), ("b", 5, 5), ("c", 5, 0), ("d", 5, 4)];
        let at_one = estimate(&rows, 1);
        assert!(at_one.balanced_design);
        let successes = f64::from(rows.iter().map(|(_, _, passed)| passed).sum::<u32>());
        assert_eq!(at_one.pass_any_at_k.to_bits(), (successes / 20.0).to_bits());

        let at_five = estimate(&rows, 5);
        assert_eq!(at_five.pass_all_at_k.to_bits(), (1.0_f64 / 4.0).to_bits());
    }

    #[test]
    fn unbalanced_design_is_flagged_and_still_averaged_per_case() {
        // Pins: mixed repetition counts stay case-averaged and are marked unbalanced.
        let rows = [("a", 4, 2), ("b", 2, 1)];
        let estimate = estimate(&rows, 2);
        assert!(!estimate.balanced_design);
        // C(2,2)/C(4,2) = 1/6 for `a`, C(1,2)/C(2,2) = 0 for `b`.
        assert!((estimate.pass_all_at_k - (1.0 / 6.0) / 2.0).abs() < 1e-12);
    }

    #[test]
    fn estimation_rejects_draw_size_above_recorded_repetitions() {
        // Pins: k>n is refused instead of silently clamping or extrapolating.
        let cases = trials(&[("a", 3, 2)]);
        let error = estimate_reliability(&cases, 4, TrialIndependence::IndependentRepetitions)
            .expect_err("k above n must be refused");
        assert!(error.to_string().contains("exceeds the 3 repetitions"));
        assert!(pass_any_at_k(3, 2, 4).is_err());
        assert!(pass_all_at_k(3, 2, 0).is_err());
    }

    #[test]
    fn estimation_rejects_shared_prefix_branched_rollouts() {
        // Pins: correlated branches never reach the independent-trial estimators.
        let cases = trials(&[("a", 4, 2)]);
        let error = estimate_reliability(&cases, 2, TrialIndependence::SharedPrefixBranched)
            .expect_err("branched rollouts must be refused");
        assert!(error.to_string().contains("independent repetitions"));

        let diagnostic =
            ConditionalFailureDiscovery::new("a", 4, 1).expect("valid branch diagnostic");
        assert_eq!(diagnostic.conditional_failure_rate, 0.25);
    }

    #[test]
    fn grouping_counts_missing_duplicate_and_unknown_repetitions() {
        // Pins: repetition bookkeeping is reported, and duplicates never inflate n.
        let observations = vec![
            RepetitionObservation {
                case_id: "a".to_string(),
                repetition: 1,
                passed: true,
            },
            RepetitionObservation {
                case_id: "a".to_string(),
                repetition: 1,
                passed: false,
            },
            RepetitionObservation {
                case_id: "a".to_string(),
                repetition: 2,
                passed: false,
            },
            RepetitionObservation {
                case_id: "a".to_string(),
                repetition: 9,
                passed: true,
            },
            RepetitionObservation {
                case_id: "ghost".to_string(),
                repetition: 1,
                passed: true,
            },
        ];
        let (cases, integrity) = group_repetitions(&["a", "b"], 3, &observations)
            .expect("grouping should tolerate malformed rows by counting them");

        assert_eq!(cases, trials(&[("a", 2, 1)]));
        assert_eq!(integrity.expected_cases, 2);
        assert_eq!(integrity.observed_rows, 5);
        assert_eq!(integrity.counted_trials, 2);
        assert_eq!(integrity.duplicate_repetitions, 1);
        assert_eq!(integrity.out_of_range_repetitions, 1);
        assert_eq!(integrity.unknown_case_rows, 1);
        assert_eq!(integrity.missing_repetitions, 4);
        assert!(!integrity.is_complete());
    }

    #[test]
    fn grouping_reports_a_complete_balanced_design() {
        // Pins: a complete design reports zero repetition defects.
        let observations = (0..2)
            .flat_map(|case| {
                (1..=3).map(move |repetition| RepetitionObservation {
                    case_id: format!("case-{case}"),
                    repetition,
                    passed: repetition != 2,
                })
            })
            .collect::<Vec<_>>();
        let (cases, integrity) =
            group_repetitions(&["case-0", "case-1"], 3, &observations).expect("complete grouping");
        assert_eq!(cases, trials(&[("case-0", 3, 2), ("case-1", 3, 2)]));
        assert!(integrity.is_complete());
        assert_eq!(integrity.missing_repetitions, 0);
    }

    #[test]
    fn pooled_outcome_variance_is_the_pooled_bernoulli_spread() {
        // Pins: the migrated variance stays p*(1-p) over pooled repetition rows.
        let cases = trials(&[("a", 5, 5), ("b", 5, 5)]);
        assert_eq!(pooled_outcome_variance(&cases), Some(0.0));
        let mixed = trials(&[("a", 2, 1), ("b", 2, 1)]);
        assert_eq!(pooled_outcome_variance(&mixed), Some(0.25));
    }

    #[test]
    fn report_curve_covers_every_supported_draw_size() {
        // Pins: the emitted curve spans k=1..=n with the case as the resampling unit.
        let cases = trials(&[("a", 5, 5), ("b", 5, 3), ("c", 5, 0)]);
        let report = CaseReliabilityReport::new(
            cases,
            TrialIndependence::IndependentRepetitions,
            RepetitionIntegrity {
                expected_repetitions: 5,
                expected_cases: 3,
                observed_rows: 15,
                counted_trials: 15,
                ..RepetitionIntegrity::default()
            },
            ReliabilityProvenance {
                provider: "live".to_string(),
                model: "configured".to_string(),
                prompt_version: "v1".to_string(),
                seeds: vec![1, 2, 3],
                cost_microusd: 1_500,
                cost_complete: true,
            },
        )
        .expect("valid reliability report");

        assert_eq!(report.schema_version, RELIABILITY_SCHEMA_VERSION);
        assert_eq!(report.resampling_unit, ReliabilityResamplingUnit::Case);
        assert_eq!(report.curve.len(), 5);
        assert_eq!(
            report.at_k(1).map(|estimate| estimate.pass_any_at_k),
            Some(8.0 / 15.0)
        );
        assert_eq!(
            report.at_k(5).map(|estimate| estimate.pass_all_at_k),
            Some(1.0 / 3.0)
        );
        assert!(report.at_k(6).is_none());
    }
}
