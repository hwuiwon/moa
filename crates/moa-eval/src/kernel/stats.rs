//! Deterministic statistical comparisons and paired non-inferiority gates.
//!
//! Two layers live here. The lower one is the estimator toolkit that retrieval
//! and execution comparisons already use: a user-cluster percentile bootstrap,
//! an exact McNemar test, and Benjamini-Hochberg correction. The upper one
//! turns a declared [`MetricDefinition`] plus paired observations into the
//! production gate decision: orient, estimate, bound, check support, decide.
//!
//! The gate is deliberately the same code path in production and in
//! simulation. [`simulate_paired_numeric_gate`],
//! [`simulate_paired_binary_gate`], and [`simulate_repeated_live_gate`]
//! generate clustered observations with an injected true utility delta and run
//! [`evaluate_paired_numeric_gate`], [`evaluate_paired_binary_gate`], or
//! [`evaluate_repeated_live_gate`] unchanged, so the reported false-PASS rate,
//! power, coverage, and precision describe the gate that actually ships rather
//! than an idealized model of it.
//!
//! [`assess_design_power`] then judges those measured characteristics against
//! declared targets. This is deliberately a *design*-specific analysis rather
//! than a generic `minimum_detectable_effect(n, variance, ...)`: the answer
//! depends on how many independent clusters or cases exist, how repetitions are
//! nested inside them, how the arms are paired, and which direction counts as
//! better, so none of that can be hidden behind a scalar `n`. A design that
//! misses a target is [`DesignAdequacy::Insufficient`] and must not block a
//! release; the gate it backs returns `INCONCLUSIVE` instead.

use std::collections::{BTreeMap, BTreeSet};

use moa_eval_core::decision::{
    Decision, MetricDecision, SupportSummary, UtilityInterval, decide_metric,
};
use moa_eval_core::metric::{
    ConfidenceMethod, Estimator, MetricClass, MetricDefinition, MetricDefinitionError,
};
use moa_eval_core::reliability::{
    CaseTrials, MAX_REPETITIONS_PER_CASE, ReliabilityEstimate, TrialIndependence, reliability_curve,
};
use serde::{Deserialize, Serialize};

/// Default number of bootstrap resamples for production reports.
pub const DEFAULT_BOOTSTRAP_RESAMPLES: usize = 10_000;

/// Default deterministic bootstrap seed for production reports.
pub const DEFAULT_BOOTSTRAP_SEED: u64 = 0x7a2b_3c4d_5e6f_1021;

/// Bootstrap configuration for cluster-resampled confidence intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapConfig {
    /// Number of bootstrap resamples to draw.
    pub resamples: usize,
    /// Deterministic PRNG seed.
    pub seed: u64,
}

impl Default for BootstrapConfig {
    fn default() -> Self {
        Self {
            resamples: DEFAULT_BOOTSTRAP_RESAMPLES,
            seed: DEFAULT_BOOTSTRAP_SEED,
        }
    }
}

/// One per-probe numeric metric observation owned by a user cluster.
///
/// Report comparisons store candidate-minus-baseline paired deltas in `value`
/// so the bootstrap interval is computed over the numeric metric itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterObservation {
    /// User cluster identifier used for resampling.
    pub user_id: String,
    /// Probe identifier retained for report auditability.
    pub probe_id: String,
    /// Per-probe metric value or paired numeric delta.
    pub value: f64,
}

/// Percentile confidence interval from user-cluster bootstrap resampling.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterBootstrapReport {
    /// Metric name covered by this interval.
    pub metric_name: String,
    /// Number of bootstrap samples drawn.
    pub resamples: usize,
    /// Seed used for deterministic resampling.
    pub seed: u64,
    /// Number of distinct user clusters.
    pub cluster_count: usize,
    /// Number of per-probe observations.
    pub observation_count: usize,
    /// Mean over the original observations.
    pub mean: f64,
    /// Lower percentile used for the confidence interval.
    pub lower_percentile: f64,
    /// Lower confidence interval bound.
    pub lower: f64,
    /// Upper percentile used for the confidence interval.
    pub upper_percentile: f64,
    /// Upper confidence interval bound.
    pub upper: f64,
}

/// One binary per-probe outcome for a paired comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BinaryProbeOutcome {
    /// Stable probe identifier used to pair outcomes.
    pub probe_id: String,
    /// Whether the probe passed for this system variant.
    pub success: bool,
}

/// McNemar paired binary comparison with optional BH-adjusted p-value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedComparison {
    /// Metric or gate name being compared.
    pub metric_name: String,
    /// Number of probes present in both paired outcome sets.
    pub total_pairs: usize,
    /// Probes where both variants succeeded.
    pub both_successes: usize,
    /// Probes where both variants failed.
    pub both_failures: usize,
    /// Probes where only the control variant succeeded.
    pub control_only_successes: usize,
    /// Probes where only the treatment variant succeeded.
    pub treatment_only_successes: usize,
    /// Exact two-sided McNemar p-value.
    pub p_value: f64,
    /// Benjamini-Hochberg adjusted p-value when correction has been applied.
    pub adjusted_p_value: f64,
    /// Whether the comparison is significant at the requested FDR.
    pub significant: bool,
}

/// Default one-sided alpha behind the historical 2.5/97.5 percentile interval.
pub const DEFAULT_ONE_SIDED_ALPHA: f64 = 0.025;

/// Computes a 2.5/97.5 percentile confidence interval by resampling users.
///
/// For paired report decisions, pass one candidate-minus-baseline numeric
/// observation per probe rather than projecting the metric to a binary value.
#[must_use]
pub fn cluster_bootstrap_mean_by_user(
    metric_name: impl Into<String>,
    observations: &[ClusterObservation],
    config: BootstrapConfig,
) -> ClusterBootstrapReport {
    cluster_bootstrap_mean_by_user_at_alpha(
        metric_name,
        observations,
        config,
        DEFAULT_ONE_SIDED_ALPHA,
    )
}

/// Computes a cluster percentile interval whose bounds each sit at `alpha`.
///
/// `alpha` is the one-sided tail probability used by the non-inferiority
/// decision, so the returned interval spans `1 - 2 * alpha`. The default
/// `0.025` reproduces the 2.5/97.5 percentile interval used by the existing
/// retrieval and execution comparisons.
#[must_use]
pub fn cluster_bootstrap_mean_by_user_at_alpha(
    metric_name: impl Into<String>,
    observations: &[ClusterObservation],
    config: BootstrapConfig,
    alpha: f64,
) -> ClusterBootstrapReport {
    let metric_name = metric_name.into();
    let mean = mean(observations.iter().map(|observation| observation.value));
    let clusters = observations_by_user(observations);
    let lower_percentile = alpha * 100.0;
    let upper_percentile = (1.0 - alpha) * 100.0;
    let samples = cluster_bootstrap_mean_samples(&clusters, config);
    let (lower, upper) = if samples.is_empty() {
        (mean, mean)
    } else {
        (
            percentile(&samples, lower_percentile),
            percentile(&samples, upper_percentile),
        )
    };

    ClusterBootstrapReport {
        metric_name,
        resamples: config.resamples,
        seed: config.seed,
        cluster_count: clusters.len(),
        observation_count: observations.len(),
        mean,
        lower_percentile,
        lower,
        upper_percentile,
        upper,
    }
}

/// Draws sorted cluster-bootstrap means for already grouped observations.
fn cluster_bootstrap_mean_samples(
    clusters: &[Vec<&ClusterObservation>],
    config: BootstrapConfig,
) -> Vec<f64> {
    if clusters.is_empty() || config.resamples == 0 {
        return Vec::new();
    }
    let mut rng = DeterministicRng::new(config.seed);
    let mut samples = Vec::with_capacity(config.resamples);
    for _ in 0..config.resamples {
        let mut total = 0.0;
        let mut count = 0_usize;
        for _ in 0..clusters.len() {
            let cluster_index = rng.gen_range(clusters.len());
            for observation in &clusters[cluster_index] {
                total += observation.value;
                count += 1;
            }
        }
        samples.push(if count == 0 {
            0.0
        } else {
            total / count as f64
        });
    }
    samples.sort_by(f64::total_cmp);
    samples
}

/// Computes an exact two-sided McNemar test for paired binary outcomes.
#[must_use]
pub fn mcnemar_paired_test(
    metric_name: impl Into<String>,
    control: &[BinaryProbeOutcome],
    treatment: &[BinaryProbeOutcome],
) -> PairedComparison {
    let treatment_by_probe = treatment
        .iter()
        .map(|outcome| (outcome.probe_id.as_str(), outcome.success))
        .collect::<BTreeMap<_, _>>();

    let mut both_successes = 0_usize;
    let mut both_failures = 0_usize;
    let mut control_only_successes = 0_usize;
    let mut treatment_only_successes = 0_usize;
    for control_outcome in control {
        let Some(treatment_success) = treatment_by_probe.get(control_outcome.probe_id.as_str())
        else {
            continue;
        };
        match (control_outcome.success, *treatment_success) {
            (true, true) => both_successes += 1,
            (false, false) => both_failures += 1,
            (true, false) => control_only_successes += 1,
            (false, true) => treatment_only_successes += 1,
        }
    }

    let p_value = exact_mcnemar_p_value(control_only_successes, treatment_only_successes);
    PairedComparison {
        metric_name: metric_name.into(),
        total_pairs: both_successes
            + both_failures
            + control_only_successes
            + treatment_only_successes,
        both_successes,
        both_failures,
        control_only_successes,
        treatment_only_successes,
        p_value,
        adjusted_p_value: p_value,
        significant: false,
    }
}

/// Applies Benjamini-Hochberg correction to paired comparison p-values.
#[must_use]
pub fn benjamini_hochberg(
    mut comparisons: Vec<PairedComparison>,
    false_discovery_rate: f64,
) -> Vec<PairedComparison> {
    if comparisons.is_empty() {
        return comparisons;
    }

    let mut order = (0..comparisons.len()).collect::<Vec<_>>();
    order.sort_by(|left, right| {
        comparisons[*left]
            .p_value
            .total_cmp(&comparisons[*right].p_value)
            .then_with(|| {
                comparisons[*left]
                    .metric_name
                    .cmp(&comparisons[*right].metric_name)
            })
    });

    let comparison_count = comparisons.len() as f64;
    let mut running_min = 1.0_f64;
    for (rank_zero_based, comparison_index) in order.iter().enumerate().rev() {
        let rank = rank_zero_based + 1;
        let adjusted =
            (comparisons[*comparison_index].p_value * comparison_count / rank as f64).min(1.0);
        running_min = running_min.min(adjusted);
        comparisons[*comparison_index].adjusted_p_value = running_min;
    }

    for comparison in &mut comparisons {
        comparison.significant = comparison.adjusted_p_value <= false_discovery_rate;
    }
    comparisons
}

fn observations_by_user(observations: &[ClusterObservation]) -> Vec<Vec<&ClusterObservation>> {
    let mut by_user = BTreeMap::<&str, Vec<&ClusterObservation>>::new();
    for observation in observations {
        by_user
            .entry(observation.user_id.as_str())
            .or_default()
            .push(observation);
    }
    by_user.into_values().collect()
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let mut total = 0.0;
    let mut count = 0_usize;
    for value in values {
        total += value;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        total / count as f64
    }
}

fn percentile(sorted_samples: &[f64], percentile: f64) -> f64 {
    if sorted_samples.is_empty() {
        return 0.0;
    }
    let index =
        ((percentile / 100.0) * (sorted_samples.len().saturating_sub(1)) as f64).floor() as usize;
    sorted_samples[index.min(sorted_samples.len() - 1)]
}

fn exact_mcnemar_p_value(control_only_successes: usize, treatment_only_successes: usize) -> f64 {
    let mismatched_pairs = control_only_successes + treatment_only_successes;
    if mismatched_pairs == 0 {
        return 1.0;
    }

    let smaller_tail = control_only_successes.min(treatment_only_successes);
    // Start at the largest term in the requested tail. Starting at P(X = 0)
    // makes every later recurrence zero once n exceeds f64's 2^-n range, even
    // when the requested tail is near the mode and the true p-value is large.
    let log_coefficient = (1..=smaller_tail).fold(0.0, |sum, successes| {
        sum + ((mismatched_pairs - successes + 1) as f64).ln() - (successes as f64).ln()
    });
    let log_probability = log_coefficient - (mismatched_pairs as f64) * std::f64::consts::LN_2;
    let mut probability = log_probability.exp();
    let mut tail = probability;
    for successes in (1..=smaller_tail).rev() {
        probability *= successes as f64 / (mismatched_pairs - successes + 1) as f64;
        tail += probability;
    }
    // Extremely small exact probabilities cannot be represented by f64. Keep
    // the result positive instead of conflating "smaller than representable"
    // with a mathematically impossible probability of exactly zero.
    (2.0 * tail).clamp(f64::from_bits(1), 1.0)
}

#[derive(Debug, Clone, Copy)]
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                DEFAULT_BOOTSTRAP_SEED
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut value = self.state;
        value ^= value >> 12;
        value ^= value << 25;
        value ^= value >> 27;
        self.state = value;
        value.wrapping_mul(2_685_821_657_736_338_717)
    }

    fn gen_range(&mut self, upper_bound: usize) -> usize {
        (self.next_u64() as usize) % upper_bound
    }

    fn next_unit_interval(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1_u64 << 53) as f64
    }

    fn next_standard_normal(&mut self) -> f64 {
        let uniform = self.next_unit_interval().max(f64::MIN_POSITIVE);
        let angle = std::f64::consts::TAU * self.next_unit_interval();
        (-2.0 * uniform.ln()).sqrt() * angle.cos()
    }
}

/// One arm's observation before pairing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArmObservation<T> {
    /// Cluster identity, for example the user or logical case.
    pub cluster_id: String,
    /// Pairing identity shared with the other arm.
    pub pair_id: String,
    /// Observed value for this arm.
    pub value: T,
}

/// One paired numeric observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedNumericObservation {
    /// Cluster identity used for resampling.
    pub cluster_id: String,
    /// Pairing identity.
    pub pair_id: String,
    /// Baseline value.
    pub baseline: f64,
    /// Candidate value.
    pub candidate: f64,
}

/// One paired binary outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedBinaryObservation {
    /// Cluster identity used for resampling.
    pub cluster_id: String,
    /// Pairing identity.
    pub pair_id: String,
    /// Baseline outcome.
    pub baseline: bool,
    /// Candidate outcome.
    pub candidate: bool,
}

/// Result of running one metric's production gate over paired observations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedGateReport {
    /// Metric identifier.
    pub metric_id: String,
    /// Three-way decision with every input that produced it.
    pub decision: MetricDecision,
    /// Mean baseline value over the paired observations.
    pub baseline_mean: f64,
    /// Mean candidate value over the paired observations.
    pub candidate_mean: f64,
    /// Exact McNemar zero-difference diagnostic for paired binary metrics.
    ///
    /// Diagnostic only. McNemar tests equality, so it can never establish
    /// non-inferiority at a nonzero margin and never gates a release.
    pub mcnemar_diagnostic: Option<PairedComparison>,
}

impl PairedGateReport {
    /// Returns the three-way decision.
    #[must_use]
    pub fn decision(&self) -> Decision {
        self.decision.decision
    }
}

/// Errors returned when paired observations cannot support a gate decision.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PairedGateError {
    /// The metric declaration was invalid.
    #[error(transparent)]
    Definition(#[from] MetricDefinitionError),
    /// The decision refused the computed interval.
    #[error(transparent)]
    Decision(#[from] moa_eval_core::decision::DecisionError),
    /// No paired observations were supplied.
    #[error("metric {metric_id}: paired gate requires at least one paired observation")]
    EmptyObservations {
        /// Metric identifier.
        metric_id: String,
    },
    /// An observation omitted its cluster key.
    #[error("metric {metric_id}: observation {pair_id} has an empty cluster key")]
    MissingClusterKey {
        /// Metric identifier.
        metric_id: String,
        /// Offending pairing key.
        pair_id: String,
    },
    /// An observation omitted its pairing key.
    #[error("metric {metric_id}: an observation has an empty pairing key")]
    MissingPairKey {
        /// Metric identifier.
        metric_id: String,
    },
    /// The same pairing key appeared twice in one arm.
    #[error("duplicate pairing key {pair_id}")]
    DuplicatePair {
        /// Duplicated pairing key.
        pair_id: String,
    },
    /// Baseline and candidate observed different case sets.
    #[error(
        "paired arms cover different cases; missing in candidate: {missing_in_candidate:?}; missing in baseline: {missing_in_baseline:?}"
    )]
    UnpairedCases {
        /// Pairing keys present in baseline but not candidate.
        missing_in_candidate: Vec<String>,
        /// Pairing keys present in candidate but not baseline.
        missing_in_baseline: Vec<String>,
    },
    /// The two arms disagreed about a case's cluster.
    #[error(
        "pairing key {pair_id} belongs to cluster {baseline_cluster} in baseline and {candidate_cluster} in candidate"
    )]
    ClusterMismatch {
        /// Pairing key with inconsistent clusters.
        pair_id: String,
        /// Cluster observed in the baseline arm.
        baseline_cluster: String,
        /// Cluster observed in the candidate arm.
        candidate_cluster: String,
    },
    /// The declared method cannot decide this metric class.
    #[error("metric {metric_id}: class {class:?} cannot be decided by the declared method")]
    MethodNotApplicable {
        /// Metric identifier.
        metric_id: String,
        /// Declared metric class.
        class: MetricClass,
    },
    /// Clustered binary outcomes were sent to a pair-independent method.
    #[error(
        "metric {metric_id}: {pairs} paired outcomes span only {clusters} clusters; clustered binary outcomes need a cluster-aware method"
    )]
    ClusteredBinaryNeedsClusterAwareMethod {
        /// Metric identifier.
        metric_id: String,
        /// Distinct clusters observed.
        clusters: usize,
        /// Paired outcomes observed.
        pairs: usize,
    },
    /// A simulation input was outside its valid range.
    #[error("metric {metric_id}: invalid simulation design: {reason}")]
    InvalidSimulationDesign {
        /// Metric identifier.
        metric_id: String,
        /// Why the design was refused.
        reason: String,
    },
    /// An inferential detectable-effect estimate was requested for an exact gate.
    #[error(
        "metric {metric_id}: exact fixed-corpus metrics have no minimum detectable effect; simulating power or a smallest detected degradation for one would describe a gate that does not exist"
    )]
    InferentialMdeOnExactMetric {
        /// Metric identifier.
        metric_id: String,
    },
    /// The two arms recorded different paired seeds for one repetition.
    #[error(
        "metric {metric_id}: case {case_id} repetition {repetition} ran under baseline seed {baseline_seed} and candidate seed {candidate_seed}; paired decisions require common randomness"
    )]
    PairedSeedMismatch {
        /// Metric identifier.
        metric_id: String,
        /// Logical case whose repetition is unpaired.
        case_id: String,
        /// Repetition number inside the case.
        repetition: u32,
        /// Seed recorded by the baseline arm.
        baseline_seed: u64,
        /// Seed recorded by the candidate arm.
        candidate_seed: u64,
    },
    /// Branched rollouts were offered to an independent-trial estimator.
    #[error(
        "metric {metric_id}: shared-prefix or branched rollouts are correlated by construction and cannot back a reliability-aware release decision"
    )]
    BranchedRolloutsNotIndependent {
        /// Metric identifier.
        metric_id: String,
    },
    /// Reliability estimation refused the grouped repetitions.
    #[error("metric {metric_id}: {reason}")]
    Reliability {
        /// Metric identifier.
        metric_id: String,
        /// Why reliability estimation refused the input.
        reason: String,
    },
}

/// Pairs two numeric arms by pairing key, refusing any unpaired case.
///
/// # Errors
///
/// Returns [`PairedGateError::UnpairedCases`] when the arms cover different
/// cases, [`PairedGateError::DuplicatePair`] on a repeated key, and
/// [`PairedGateError::ClusterMismatch`] when the arms disagree about a case's
/// cluster.
pub fn pair_numeric_arms(
    baseline: &[ArmObservation<f64>],
    candidate: &[ArmObservation<f64>],
) -> Result<Vec<PairedNumericObservation>, PairedGateError> {
    Ok(pair_arms(baseline, candidate)?
        .into_iter()
        .map(
            |(cluster_id, pair_id, baseline, candidate)| PairedNumericObservation {
                cluster_id,
                pair_id,
                baseline,
                candidate,
            },
        )
        .collect())
}

/// Pairs two binary arms by pairing key, refusing any unpaired case.
///
/// # Errors
///
/// Returns the same pairing errors as [`pair_numeric_arms`].
pub fn pair_binary_arms(
    baseline: &[ArmObservation<bool>],
    candidate: &[ArmObservation<bool>],
) -> Result<Vec<PairedBinaryObservation>, PairedGateError> {
    Ok(pair_arms(baseline, candidate)?
        .into_iter()
        .map(
            |(cluster_id, pair_id, baseline, candidate)| PairedBinaryObservation {
                cluster_id,
                pair_id,
                baseline,
                candidate,
            },
        )
        .collect())
}

fn pair_arms<T: Copy>(
    baseline: &[ArmObservation<T>],
    candidate: &[ArmObservation<T>],
) -> Result<Vec<(String, String, T, T)>, PairedGateError> {
    let baseline_by_pair = arm_by_pair(baseline)?;
    let candidate_by_pair = arm_by_pair(candidate)?;
    let baseline_keys = baseline_by_pair.keys().cloned().collect::<BTreeSet<_>>();
    let candidate_keys = candidate_by_pair.keys().cloned().collect::<BTreeSet<_>>();
    if baseline_keys != candidate_keys {
        return Err(PairedGateError::UnpairedCases {
            missing_in_candidate: baseline_keys.difference(&candidate_keys).cloned().collect(),
            missing_in_baseline: candidate_keys.difference(&baseline_keys).cloned().collect(),
        });
    }

    let mut paired = Vec::with_capacity(baseline_by_pair.len());
    for (pair_id, baseline_observation) in baseline_by_pair {
        let candidate_observation = candidate_by_pair
            .get(&pair_id)
            .expect("pair key sets were compared before pairing");
        if baseline_observation.cluster_id != candidate_observation.cluster_id {
            return Err(PairedGateError::ClusterMismatch {
                pair_id,
                baseline_cluster: baseline_observation.cluster_id.clone(),
                candidate_cluster: candidate_observation.cluster_id.clone(),
            });
        }
        paired.push((
            baseline_observation.cluster_id.clone(),
            pair_id,
            baseline_observation.value,
            candidate_observation.value,
        ));
    }
    Ok(paired)
}

fn arm_by_pair<T: Copy>(
    arm: &[ArmObservation<T>],
) -> Result<BTreeMap<String, &ArmObservation<T>>, PairedGateError> {
    let mut by_pair = BTreeMap::new();
    for observation in arm {
        if by_pair
            .insert(observation.pair_id.clone(), observation)
            .is_some()
        {
            return Err(PairedGateError::DuplicatePair {
                pair_id: observation.pair_id.clone(),
            });
        }
    }
    Ok(by_pair)
}

/// Runs the production non-inferiority gate for a paired numeric metric.
///
/// Handles the paired numeric, stochastic-live, and tail-quantile classes; the
/// interval method comes from the declaration, never from the caller.
///
/// # Errors
///
/// Returns [`PairedGateError`] when the declaration is invalid, the
/// observations are empty or missing cluster/pairing keys, or the declared
/// method cannot decide this class.
pub fn evaluate_paired_numeric_gate(
    definition: &MetricDefinition,
    observations: &[PairedNumericObservation],
) -> Result<PairedGateReport, PairedGateError> {
    definition.validate()?;
    let margin = definition.margin()?;
    if !matches!(
        definition.class(),
        MetricClass::PairedNumeric | MetricClass::StochasticLive | MetricClass::TailLatencyQuantile
    ) {
        return Err(PairedGateError::MethodNotApplicable {
            metric_id: definition.id.clone(),
            class: definition.class(),
        });
    }
    if observations.is_empty() {
        return Err(PairedGateError::EmptyObservations {
            metric_id: definition.id.clone(),
        });
    }
    let mut seen_pairs = BTreeSet::new();
    for observation in observations {
        if observation.cluster_id.trim().is_empty() {
            return Err(PairedGateError::MissingClusterKey {
                metric_id: definition.id.clone(),
                pair_id: observation.pair_id.clone(),
            });
        }
        if observation.pair_id.trim().is_empty() {
            return Err(PairedGateError::MissingPairKey {
                metric_id: definition.id.clone(),
            });
        }
        if !seen_pairs.insert(observation.pair_id.as_str()) {
            return Err(PairedGateError::DuplicatePair {
                pair_id: observation.pair_id.clone(),
            });
        }
    }

    let sign = definition.direction_sign();
    let deltas = observations
        .iter()
        .map(|observation| ClusterObservation {
            user_id: observation.cluster_id.clone(),
            probe_id: observation.pair_id.clone(),
            value: sign * (observation.candidate - observation.baseline),
        })
        .collect::<Vec<_>>();
    let clusters = observations_by_user(&deltas);
    let config = bootstrap_config(definition)?;

    let (point, samples) = match definition.confidence_method {
        ConfidenceMethod::ClusterPairedDeltaBootstrap(_) => (
            mean(deltas.iter().map(|delta| delta.value)),
            cluster_bootstrap_mean_samples(&clusters, config),
        ),
        ConfidenceMethod::HierarchicalCaseBootstrap(_) => {
            hierarchical_case_bootstrap(&clusters, config)
        }
        ConfidenceMethod::ClusterPairedQuantileBootstrap(_) => {
            quantile_bootstrap(definition, observations, config)?
        }
        _ => {
            return Err(PairedGateError::MethodNotApplicable {
                metric_id: definition.id.clone(),
                class: definition.class(),
            });
        }
    };

    let interval = interval_from_samples(point, &samples, definition.alpha);
    let support = SupportSummary {
        independent_units: clusters.len(),
        observations: observations.len(),
        required_independent_units: definition.confidence_method.min_independent_units(),
    };
    let decision = decide_metric(
        definition,
        &interval,
        support,
        bootstrap_regression_p_value(&samples, margin),
    )?;

    Ok(PairedGateReport {
        metric_id: definition.id.clone(),
        decision,
        baseline_mean: mean(observations.iter().map(|observation| observation.baseline)),
        candidate_mean: mean(observations.iter().map(|observation| observation.candidate)),
        mcnemar_diagnostic: None,
    })
}

/// Runs the production non-inferiority gate for a paired binary metric.
///
/// The gate uses a matched risk difference — closed form when every pair is its
/// own independent unit, cluster bootstrap when pairs are nested in clusters.
/// The exact McNemar test is computed alongside as a zero-difference
/// diagnostic; it tests equality and cannot decide a nonzero margin.
///
/// # Errors
///
/// Returns [`PairedGateError`] when the declaration is invalid, the
/// observations are empty or missing cluster/pairing keys, the metric is not
/// paired binary, or clustered outcomes were sent to the pair-independent
/// closed form.
pub fn evaluate_paired_binary_gate(
    definition: &MetricDefinition,
    observations: &[PairedBinaryObservation],
) -> Result<PairedGateReport, PairedGateError> {
    definition.validate()?;
    let margin = definition.margin()?;
    if definition.class() != MetricClass::PairedBinary {
        return Err(PairedGateError::MethodNotApplicable {
            metric_id: definition.id.clone(),
            class: definition.class(),
        });
    }
    if observations.is_empty() {
        return Err(PairedGateError::EmptyObservations {
            metric_id: definition.id.clone(),
        });
    }
    let mut seen_pairs = BTreeSet::new();
    for observation in observations {
        if observation.cluster_id.trim().is_empty() {
            return Err(PairedGateError::MissingClusterKey {
                metric_id: definition.id.clone(),
                pair_id: observation.pair_id.clone(),
            });
        }
        if observation.pair_id.trim().is_empty() {
            return Err(PairedGateError::MissingPairKey {
                metric_id: definition.id.clone(),
            });
        }
        if !seen_pairs.insert(observation.pair_id.as_str()) {
            return Err(PairedGateError::DuplicatePair {
                pair_id: observation.pair_id.clone(),
            });
        }
    }

    let sign = definition.direction_sign();
    let deltas = observations
        .iter()
        .map(|observation| ClusterObservation {
            user_id: observation.cluster_id.clone(),
            probe_id: observation.pair_id.clone(),
            value: sign * (f64::from(observation.candidate) - f64::from(observation.baseline)),
        })
        .collect::<Vec<_>>();
    let clusters = observations_by_user(&deltas);
    let mcnemar = mcnemar_paired_test(
        definition.id.clone(),
        &observations
            .iter()
            .map(|observation| BinaryProbeOutcome {
                probe_id: observation.pair_id.clone(),
                success: observation.baseline,
            })
            .collect::<Vec<_>>(),
        &observations
            .iter()
            .map(|observation| BinaryProbeOutcome {
                probe_id: observation.pair_id.clone(),
                success: observation.candidate,
            })
            .collect::<Vec<_>>(),
    );

    let (interval, support, regression_p_value) = match definition.confidence_method {
        ConfidenceMethod::MatchedRiskDifferenceAdjustedWald {
            pseudo_count,
            min_independent_units,
        } => {
            if clusters.len() < observations.len() {
                return Err(PairedGateError::ClusteredBinaryNeedsClusterAwareMethod {
                    metric_id: definition.id.clone(),
                    clusters: clusters.len(),
                    pairs: observations.len(),
                });
            }
            let (interval, standard_error) =
                matched_risk_difference_interval(&deltas, pseudo_count, definition.alpha);
            let regression_p_value = if standard_error > 0.0 {
                Some(standard_normal_cdf(
                    (interval.point + margin) / standard_error,
                ))
            } else {
                None
            };
            (
                interval,
                SupportSummary {
                    independent_units: observations.len(),
                    observations: observations.len(),
                    required_independent_units: min_independent_units,
                },
                regression_p_value,
            )
        }
        ConfidenceMethod::ClusterMatchedRiskDifferenceBootstrap(_) => {
            let config = bootstrap_config(definition)?;
            let samples = cluster_bootstrap_mean_samples(&clusters, config);
            let point = mean(deltas.iter().map(|delta| delta.value));
            (
                interval_from_samples(point, &samples, definition.alpha),
                SupportSummary {
                    independent_units: clusters.len(),
                    observations: observations.len(),
                    required_independent_units: definition
                        .confidence_method
                        .min_independent_units(),
                },
                bootstrap_regression_p_value(&samples, margin),
            )
        }
        _ => {
            return Err(PairedGateError::MethodNotApplicable {
                metric_id: definition.id.clone(),
                class: definition.class(),
            });
        }
    };

    let decision = decide_metric(definition, &interval, support, regression_p_value)?;
    Ok(PairedGateReport {
        metric_id: definition.id.clone(),
        decision,
        baseline_mean: mean(
            observations
                .iter()
                .map(|observation| f64::from(observation.baseline)),
        ),
        candidate_mean: mean(
            observations
                .iter()
                .map(|observation| f64::from(observation.candidate)),
        ),
        mcnemar_diagnostic: Some(mcnemar),
    })
}

/// One repetition of one logical case, as one arm recorded it.
///
/// The `(case_id, repetition)` pair is the pairing key and `case_id` alone is the
/// cluster key, so repetitions stay nested inside their case and are never
/// pooled across cases. `paired_seed` is the common randomness both arms ran
/// under; it is provenance, and recording it never makes a sampled provider
/// deterministic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepeatedTrialObservation {
    /// Stable logical case identifier, the independent unit.
    pub case_id: String,
    /// One-based repetition number inside the case.
    pub repetition: u32,
    /// Seed both arms ran this repetition under.
    pub paired_seed: u64,
    /// Whether this repetition passed every case-level assertion.
    pub passed: bool,
}

/// A reliability-aware release decision for one stochastic live metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveGateReport {
    /// Three-way gate decision over the paired case-level pass rates.
    pub gate: PairedGateReport,
    /// Independence claim the reliability curves were allowed to rely on.
    pub independence: TrialIndependence,
    /// Baseline reliability, computed per case and then averaged.
    pub baseline_reliability: Vec<ReliabilityEstimate>,
    /// Candidate reliability, computed per case and then averaged.
    pub candidate_reliability: Vec<ReliabilityEstimate>,
}

impl LiveGateReport {
    /// Returns the three-way decision.
    #[must_use]
    pub fn decision(&self) -> Decision {
        self.gate.decision()
    }

    /// Returns the candidate's single-run pass rate, `pass_any_at_k` with `k = 1`.
    #[must_use]
    pub fn candidate_single_run_pass_rate(&self) -> Option<f64> {
        self.candidate_reliability
            .iter()
            .find(|estimate| estimate.k == 1)
            .map(|estimate| estimate.pass_any_at_k)
    }
}

/// Pairs two arms of repeated live trials by `(case, repetition)`.
///
/// Every repetition must appear in both arms under the same paired seed. A
/// missing repetition, a duplicate, or a seed mismatch is refused rather than
/// dropped, because each of those silently turns a paired comparison into an
/// unpaired one.
///
/// # Errors
///
/// Returns [`PairedGateError::UnpairedCases`] when the arms cover different
/// repetitions, [`PairedGateError::DuplicatePair`] on a repeated
/// `(case, repetition)`, and [`PairedGateError::PairedSeedMismatch`] when the
/// arms disagree about the seed a repetition ran under.
pub fn pair_repeated_trials(
    definition: &MetricDefinition,
    baseline: &[RepeatedTrialObservation],
    candidate: &[RepeatedTrialObservation],
) -> Result<Vec<PairedNumericObservation>, PairedGateError> {
    let baseline_arm = repeated_arm(definition, baseline)?;
    let candidate_arm = repeated_arm(definition, candidate)?;
    let baseline_keys = baseline_arm.keys().cloned().collect::<BTreeSet<_>>();
    let candidate_keys = candidate_arm.keys().cloned().collect::<BTreeSet<_>>();
    if baseline_keys != candidate_keys {
        return Err(PairedGateError::UnpairedCases {
            missing_in_candidate: baseline_keys
                .difference(&candidate_keys)
                .map(|(case_id, repetition)| format!("{case_id}#rep-{repetition}"))
                .collect(),
            missing_in_baseline: candidate_keys
                .difference(&baseline_keys)
                .map(|(case_id, repetition)| format!("{case_id}#rep-{repetition}"))
                .collect(),
        });
    }

    let mut paired = Vec::with_capacity(baseline_arm.len());
    for ((case_id, repetition), baseline_trial) in baseline_arm {
        let candidate_trial = candidate_arm
            .get(&(case_id.clone(), repetition))
            .expect("repetition key sets were compared before pairing");
        if baseline_trial.paired_seed != candidate_trial.paired_seed {
            return Err(PairedGateError::PairedSeedMismatch {
                metric_id: definition.id.clone(),
                case_id,
                repetition,
                baseline_seed: baseline_trial.paired_seed,
                candidate_seed: candidate_trial.paired_seed,
            });
        }
        paired.push(PairedNumericObservation {
            pair_id: format!("{case_id}#rep-{repetition}"),
            cluster_id: case_id,
            baseline: f64::from(baseline_trial.passed),
            candidate: f64::from(candidate_trial.passed),
        });
    }
    Ok(paired)
}

fn repeated_arm<'a>(
    definition: &MetricDefinition,
    arm: &'a [RepeatedTrialObservation],
) -> Result<BTreeMap<(String, u32), &'a RepeatedTrialObservation>, PairedGateError> {
    let mut by_key = BTreeMap::new();
    for trial in arm {
        if trial.case_id.trim().is_empty() {
            return Err(PairedGateError::MissingClusterKey {
                metric_id: definition.id.clone(),
                pair_id: format!("rep-{}", trial.repetition),
            });
        }
        if trial.repetition == 0 {
            return Err(PairedGateError::MissingPairKey {
                metric_id: definition.id.clone(),
            });
        }
        if by_key
            .insert((trial.case_id.clone(), trial.repetition), trial)
            .is_some()
        {
            return Err(PairedGateError::DuplicatePair {
                pair_id: format!("{}#rep-{}", trial.case_id, trial.repetition),
            });
        }
    }
    Ok(by_key)
}

/// Runs the reliability-aware release gate for a stochastic live metric.
///
/// The decision is made on the paired per-case pass-rate delta with a
/// hierarchical case bootstrap: cases are the resampling unit and repetitions
/// are nested inside them. The reliability curves are computed per case and then
/// averaged, so no estimate ever pools successes across heterogeneous cases.
///
/// # Errors
///
/// Returns [`PairedGateError::MethodNotApplicable`] unless the metric declares
/// [`MetricClass::StochasticLive`] with a hierarchical case bootstrap,
/// [`PairedGateError::BranchedRolloutsNotIndependent`] for shared-prefix
/// rollouts, and the pairing errors of [`pair_repeated_trials`].
pub fn evaluate_repeated_live_gate(
    definition: &MetricDefinition,
    baseline: &[RepeatedTrialObservation],
    candidate: &[RepeatedTrialObservation],
    independence: TrialIndependence,
) -> Result<LiveGateReport, PairedGateError> {
    definition.validate()?;
    if definition.class() != MetricClass::StochasticLive
        || !matches!(
            definition.confidence_method,
            ConfidenceMethod::HierarchicalCaseBootstrap(_)
        )
    {
        return Err(PairedGateError::MethodNotApplicable {
            metric_id: definition.id.clone(),
            class: definition.class(),
        });
    }
    if independence == TrialIndependence::SharedPrefixBranched {
        return Err(PairedGateError::BranchedRolloutsNotIndependent {
            metric_id: definition.id.clone(),
        });
    }

    let paired = pair_repeated_trials(definition, baseline, candidate)?;
    let gate = evaluate_paired_numeric_gate(definition, &paired)?;
    Ok(LiveGateReport {
        gate,
        independence,
        baseline_reliability: arm_reliability(definition, baseline, independence)?,
        candidate_reliability: arm_reliability(definition, candidate, independence)?,
    })
}

fn arm_reliability(
    definition: &MetricDefinition,
    arm: &[RepeatedTrialObservation],
    independence: TrialIndependence,
) -> Result<Vec<ReliabilityEstimate>, PairedGateError> {
    let mut counts = BTreeMap::<&str, (u32, u32)>::new();
    for trial in arm {
        let entry = counts.entry(trial.case_id.as_str()).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += u32::from(trial.passed);
    }
    let cases = counts
        .into_iter()
        .map(|(case_id, (attempts, successes))| CaseTrials::new(case_id, attempts, successes))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| PairedGateError::Reliability {
            metric_id: definition.id.clone(),
            reason: error.to_string(),
        })?;
    reliability_curve(&cases, independence).map_err(|error| PairedGateError::Reliability {
        metric_id: definition.id.clone(),
        reason: error.to_string(),
    })
}

fn bootstrap_config(definition: &MetricDefinition) -> Result<BootstrapConfig, PairedGateError> {
    let plan = definition
        .confidence_method
        .resampling_plan()
        .ok_or_else(|| PairedGateError::MethodNotApplicable {
            metric_id: definition.id.clone(),
            class: definition.class(),
        })?;
    Ok(BootstrapConfig {
        resamples: plan.resamples,
        seed: plan.seed,
    })
}

/// Resamples cases, then repetitions inside each case, and averages case means.
fn hierarchical_case_bootstrap(
    cases: &[Vec<&ClusterObservation>],
    config: BootstrapConfig,
) -> (f64, Vec<f64>) {
    let point = mean(
        cases
            .iter()
            .map(|case| mean(case.iter().map(|observation| observation.value))),
    );
    if cases.is_empty() || config.resamples == 0 {
        return (point, Vec::new());
    }
    let mut rng = DeterministicRng::new(config.seed);
    let mut samples = Vec::with_capacity(config.resamples);
    for _ in 0..config.resamples {
        let mut total = 0.0;
        for _ in 0..cases.len() {
            let case = &cases[rng.gen_range(cases.len())];
            let mut case_total = 0.0;
            for _ in 0..case.len() {
                case_total += case[rng.gen_range(case.len())].value;
            }
            total += case_total / case.len() as f64;
        }
        samples.push(total / cases.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    (point, samples)
}

/// Bootstraps the paired difference of a quantile by resampling clusters.
fn quantile_bootstrap(
    definition: &MetricDefinition,
    observations: &[PairedNumericObservation],
    config: BootstrapConfig,
) -> Result<(f64, Vec<f64>), PairedGateError> {
    let Estimator::PairedQuantileDelta { quantile } = definition.estimator else {
        return Err(PairedGateError::MethodNotApplicable {
            metric_id: definition.id.clone(),
            class: definition.class(),
        });
    };
    let sign = definition.direction_sign();
    let mut by_cluster = BTreeMap::<&str, Vec<&PairedNumericObservation>>::new();
    for observation in observations {
        by_cluster
            .entry(observation.cluster_id.as_str())
            .or_default()
            .push(observation);
    }
    let clusters = by_cluster.into_values().collect::<Vec<_>>();
    let point = sign
        * (quantile_of(observations.iter().map(|o| o.candidate), quantile)
            - quantile_of(observations.iter().map(|o| o.baseline), quantile));
    if clusters.is_empty() || config.resamples == 0 {
        return Ok((point, Vec::new()));
    }

    let mut rng = DeterministicRng::new(config.seed);
    let mut samples = Vec::with_capacity(config.resamples);
    let mut baseline_values = Vec::with_capacity(observations.len());
    let mut candidate_values = Vec::with_capacity(observations.len());
    for _ in 0..config.resamples {
        baseline_values.clear();
        candidate_values.clear();
        for _ in 0..clusters.len() {
            for observation in &clusters[rng.gen_range(clusters.len())] {
                baseline_values.push(observation.baseline);
                candidate_values.push(observation.candidate);
            }
        }
        samples.push(
            sign * (quantile_of(candidate_values.iter().copied(), quantile)
                - quantile_of(baseline_values.iter().copied(), quantile)),
        );
    }
    samples.sort_by(f64::total_cmp);
    Ok((point, samples))
}

fn quantile_of(values: impl Iterator<Item = f64>, quantile: f64) -> f64 {
    let mut sorted = values.collect::<Vec<_>>();
    sorted.sort_by(f64::total_cmp);
    percentile(&sorted, quantile * 100.0)
}

/// Builds a closed-form matched risk-difference interval with pseudo-counts.
///
/// The deltas are already oriented, so `+1` means the candidate won the pair
/// and `-1` means it lost. Adding a pseudo-count to each of the four matched
/// cells keeps the interval usable when discordant counts are small, where a
/// plain Wald interval collapses to zero width.
fn matched_risk_difference_interval(
    deltas: &[ClusterObservation],
    pseudo_count: f64,
    alpha: f64,
) -> (UtilityInterval, f64) {
    let pairs = deltas.len() as f64;
    let candidate_wins = deltas.iter().filter(|delta| delta.value > 0.0).count() as f64;
    let candidate_losses = deltas.iter().filter(|delta| delta.value < 0.0).count() as f64;
    let point = if pairs == 0.0 {
        0.0
    } else {
        (candidate_wins - candidate_losses) / pairs
    };

    let adjusted_pairs = pairs + 4.0 * pseudo_count;
    let adjusted_wins = candidate_wins + pseudo_count;
    let adjusted_losses = candidate_losses + pseudo_count;
    let adjusted_point = (adjusted_wins - adjusted_losses) / adjusted_pairs;
    let variance = ((adjusted_wins + adjusted_losses)
        - (adjusted_wins - adjusted_losses).powi(2) / adjusted_pairs)
        / (adjusted_pairs * adjusted_pairs);
    let standard_error = variance.max(0.0).sqrt();
    let z = standard_normal_quantile(1.0 - alpha);
    (
        UtilityInterval::new(
            point,
            adjusted_point - z * standard_error,
            adjusted_point + z * standard_error,
        ),
        standard_error,
    )
}

fn interval_from_samples(point: f64, sorted_samples: &[f64], alpha: f64) -> UtilityInterval {
    if sorted_samples.is_empty() {
        return UtilityInterval::new(point, point, point);
    }
    UtilityInterval::new(
        point,
        percentile(sorted_samples, alpha * 100.0),
        percentile(sorted_samples, (1.0 - alpha) * 100.0),
    )
}

/// Returns the one-sided bootstrap p-value for the reverse regression test.
///
/// The reverse hypothesis is `utility_delta >= -margin`; the achieved
/// significance level is the share of resampled statistics that still reach
/// the margin. Small values are regression evidence.
fn bootstrap_regression_p_value(sorted_samples: &[f64], margin: f64) -> Option<f64> {
    if sorted_samples.is_empty() {
        return None;
    }
    let below = sorted_samples.partition_point(|sample| *sample < -margin);
    Some((sorted_samples.len() - below) as f64 / sorted_samples.len() as f64)
}

fn standard_normal_quantile(probability: f64) -> f64 {
    // Acklam's inverse-normal coefficients, transcribed verbatim. Trimming a
    // digit to satisfy the precision lint would silently change the constant.
    #[allow(clippy::excessive_precision)]
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_690e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239e0,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838e0,
        -2.549_732_539_343_734e0,
        4.374_664_141_464_968e0,
        2.938_163_982_698_783e0,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996e0,
        3.754_408_661_907_416e0,
    ];
    const LOW: f64 = 0.024_25;

    if probability <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if probability >= 1.0 {
        return f64::INFINITY;
    }
    if probability < LOW {
        let q = (-2.0 * probability.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if probability <= 1.0 - LOW {
        let q = probability - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        let q = (-2.0 * (1.0 - probability).ln()).sqrt();
        -(((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    }
}

fn standard_normal_cdf(z: f64) -> f64 {
    const P: f64 = 0.231_641_9;
    const B: [f64; 5] = [
        0.319_381_530,
        -0.356_563_782,
        1.781_477_937,
        -1.821_255_978,
        1.330_274_429,
    ];
    if !z.is_finite() {
        return if z > 0.0 { 1.0 } else { 0.0 };
    }
    let magnitude = z.abs();
    let t = 1.0 / (1.0 + P * magnitude);
    let density = (-0.5 * magnitude * magnitude).exp() / (2.0 * std::f64::consts::PI).sqrt();
    let polynomial = t * (B[0] + t * (B[1] + t * (B[2] + t * (B[3] + t * B[4]))));
    let upper_tail = density * polynomial;
    if z < 0.0 {
        upper_tail
    } else {
        1.0 - upper_tail
    }
}

/// Clustered data-generating design used to probe a gate's behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusteredSimulationDesign {
    /// Independent clusters per simulated run.
    pub clusters: usize,
    /// Paired observations inside each cluster.
    pub observations_per_cluster: usize,
    /// Standard deviation of the per-cluster utility shift.
    pub cluster_effect_sd: f64,
    /// Standard deviation of the per-observation numeric noise.
    pub observation_noise_sd: f64,
    /// Baseline level for numeric metrics.
    pub baseline_level: f64,
    /// Probability that a binary pair is discordant.
    pub discordance_rate: f64,
}

/// Inputs for estimating a gate's operating characteristics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateSimulationConfig {
    /// Simulated runs per scenario.
    pub trials: usize,
    /// Deterministic seed for both data generation and resampling.
    pub seed: u64,
    /// Clustered data-generating design.
    pub design: ClusteredSimulationDesign,
    /// Regression-detection power the gate is required to reach.
    pub detection_power_target: f64,
    /// Degradation magnitudes, in utility units, scanned for detectability.
    pub degradation_grid: Vec<f64>,
}

impl GateSimulationConfig {
    fn schedule(&self) -> SimulationSchedule {
        SimulationSchedule {
            trials: self.trials,
            seed: self.seed,
            detection_power_target: self.detection_power_target,
            degradation_grid: self.degradation_grid.clone(),
        }
    }
}

/// Case-and-repetition design used by a stochastic live release gate.
///
/// The independent unit is the logical case; repetitions are nested inside it
/// and are never pooled across cases. Baseline and candidate share one paired
/// seed per repetition, so the two arms differ only by the injected effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplicatedLiveDesign {
    /// Independent logical cases per simulated run.
    pub cases: usize,
    /// Independent repetitions recorded for each case.
    pub repetitions_per_case: u32,
    /// Mean baseline single-run pass rate across cases.
    pub baseline_pass_rate: f64,
    /// Standard deviation of the per-case baseline pass rate.
    pub case_pass_rate_sd: f64,
    /// Share of a repetition's outcome the common paired seed actually pins.
    ///
    /// `1.0` makes the two arms move together perfectly, which is the exact
    /// no-change fixture rather than a realistic live lane. Lower values leave
    /// residual per-arm flakiness — the reason repetitions exist at all — and are
    /// what makes repeat reliability a question worth measuring.
    pub shared_randomness: f64,
}

/// Inputs for estimating a stochastic live gate's operating characteristics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveGateSimulationConfig {
    /// Simulated runs per scenario.
    pub trials: usize,
    /// Deterministic seed for data generation, pairing, and resampling.
    pub seed: u64,
    /// Case-and-repetition design under evaluation.
    pub design: ReplicatedLiveDesign,
    /// Regression-detection power the gate is required to reach.
    pub detection_power_target: f64,
    /// Degradation magnitudes, in utility units, scanned for detectability.
    pub degradation_grid: Vec<f64>,
}

impl LiveGateSimulationConfig {
    fn schedule(&self) -> SimulationSchedule {
        SimulationSchedule {
            trials: self.trials,
            seed: self.seed,
            detection_power_target: self.detection_power_target,
            degradation_grid: self.degradation_grid.clone(),
        }
    }
}

/// Scenario schedule shared by every simulated design.
#[derive(Debug, Clone, PartialEq)]
struct SimulationSchedule {
    trials: usize,
    seed: u64,
    detection_power_target: f64,
    degradation_grid: Vec<f64>,
}

/// Gate behavior at one true utility delta.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioOperatingCharacteristics {
    /// Scenario label.
    pub scenario: String,
    /// True utility delta injected into the generated data.
    pub true_utility_delta: f64,
    /// Simulated runs.
    pub trials: usize,
    /// Share of runs deciding PASS.
    pub pass_rate: f64,
    /// Share of runs deciding REGRESSION.
    pub regression_rate: f64,
    /// Share of runs deciding INCONCLUSIVE.
    pub inconclusive_rate: f64,
    /// Share of runs that were INCONCLUSIVE because cluster support was short.
    ///
    /// Reported separately from [`Self::inconclusive_rate`] because the two have
    /// different remedies: a straddling interval needs a tighter design, while
    /// short support means the design could not make a population claim at all.
    pub insufficient_support_rate: f64,
    /// Share of runs whose interval covered the true utility delta.
    pub coverage: f64,
    /// Mean width of the decided interval, the design's achieved precision.
    pub mean_interval_width: f64,
}

/// Operating characteristics of one metric's production gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateOperatingCharacteristics {
    /// Metric identifier.
    pub metric_id: String,
    /// Simulated runs per scenario.
    pub trials: usize,
    /// Deterministic seed.
    pub seed: u64,
    /// One-sided alpha the gate was run at.
    pub alpha: f64,
    /// Declared non-inferiority margin.
    pub practical_margin: f64,
    /// Independent clusters observed by the gate in each run.
    pub effective_independent_clusters: usize,
    /// Paired observations observed by the gate in each run.
    pub effective_observations: usize,
    /// Behavior at the non-inferiority boundary `utility_delta = -margin`.
    pub boundary: ScenarioOperatingCharacteristics,
    /// Behavior at the declared acceptable alternative.
    pub acceptable_alternative: ScenarioOperatingCharacteristics,
    /// Behavior at the declared unacceptable alternative.
    pub unacceptable_alternative: ScenarioOperatingCharacteristics,
    /// Behavior across the scanned degradation grid.
    pub degradation_scan: Vec<ScenarioOperatingCharacteristics>,
    /// Regression-detection power required from the scan.
    pub detection_power_target: f64,
    /// Smallest scanned degradation detected at the required power.
    pub smallest_detected_degradation: Option<f64>,
}

impl GateOperatingCharacteristics {
    /// Returns the probability of a PASS when the truth sits at `-margin`.
    #[must_use]
    pub fn false_pass_probability_at_margin(&self) -> f64 {
        self.boundary.pass_rate
    }

    /// Returns the PASS power at the declared acceptable alternative.
    #[must_use]
    pub fn pass_power_at_acceptable(&self) -> f64 {
        self.acceptable_alternative.pass_rate
    }

    /// Returns the regression power at the declared unacceptable alternative.
    #[must_use]
    pub fn regression_power_at_unacceptable(&self) -> f64 {
        self.unacceptable_alternative.regression_rate
    }
}

/// Estimates a paired numeric gate's operating characteristics.
///
/// Each simulated run generates clustered paired observations with an injected
/// true utility delta and then calls [`evaluate_paired_numeric_gate`]. Nothing
/// but the resampling seed differs from a production decision.
///
/// # Errors
///
/// Returns [`PairedGateError`] when the declaration or the simulation design is
/// invalid.
pub fn simulate_paired_numeric_gate(
    definition: &MetricDefinition,
    config: &GateSimulationConfig,
) -> Result<GateOperatingCharacteristics, PairedGateError> {
    validate_clustered_design(definition, &config.design)?;
    let design = config.design.clone();
    simulate_gate(
        definition,
        &config.schedule(),
        move |trial_definition, delta, rng| {
            let observations =
                simulated_numeric_observations(trial_definition, &design, delta, rng);
            evaluate_paired_numeric_gate(trial_definition, &observations)
        },
    )
}

/// Estimates a paired binary gate's operating characteristics.
///
/// # Errors
///
/// Returns [`PairedGateError`] when the declaration or the simulation design is
/// invalid.
pub fn simulate_paired_binary_gate(
    definition: &MetricDefinition,
    config: &GateSimulationConfig,
) -> Result<GateOperatingCharacteristics, PairedGateError> {
    validate_clustered_design(definition, &config.design)?;
    let design = config.design.clone();
    simulate_gate(
        definition,
        &config.schedule(),
        move |trial_definition, delta, rng| {
            let observations = simulated_binary_observations(trial_definition, &design, delta, rng);
            evaluate_paired_binary_gate(trial_definition, &observations)
        },
    )
}

/// Estimates a stochastic live gate's operating characteristics.
///
/// Each simulated run generates `cases * repetitions_per_case` paired
/// repetitions under common randomness, pairs them by `(case, repetition)`, and
/// runs [`evaluate_repeated_live_gate`] unchanged. The independent unit is the
/// case, so adding repetitions inside a fixed set of cases buys precision on
/// each case's pass rate but almost no additional population support — which is
/// exactly why a repetition count is not a blocking floor on its own.
///
/// # Errors
///
/// Returns [`PairedGateError`] when the declaration or the simulated design is
/// invalid, including a design whose injected effect cannot be represented as a
/// pass-rate shift.
pub fn simulate_repeated_live_gate(
    definition: &MetricDefinition,
    config: &LiveGateSimulationConfig,
) -> Result<GateOperatingCharacteristics, PairedGateError> {
    validate_live_design(definition, config)?;
    let design = config.design.clone();
    simulate_gate(
        definition,
        &config.schedule(),
        move |trial_definition, delta, rng| {
            let (baseline, candidate) =
                simulated_repeated_trials(trial_definition, &design, delta, rng);
            evaluate_repeated_live_gate(
                trial_definition,
                &baseline,
                &candidate,
                TrialIndependence::IndependentRepetitions,
            )
            .map(|report| report.gate)
        },
    )
}

fn simulate_gate<F>(
    definition: &MetricDefinition,
    schedule: &SimulationSchedule,
    evaluate: F,
) -> Result<GateOperatingCharacteristics, PairedGateError>
where
    F: Fn(
        &MetricDefinition,
        f64,
        &mut DeterministicRng,
    ) -> Result<PairedGateReport, PairedGateError>,
{
    definition.validate()?;
    if definition.is_exact() {
        return Err(PairedGateError::InferentialMdeOnExactMetric {
            metric_id: definition.id.clone(),
        });
    }
    let margin = definition.margin()?;
    let acceptable = definition.acceptable_alternative.ok_or_else(|| {
        MetricDefinitionError::MissingAlternative {
            metric_id: definition.id.clone(),
            field: "acceptable_alternative",
        }
    })?;
    let unacceptable = definition.unacceptable_alternative.ok_or_else(|| {
        MetricDefinitionError::MissingAlternative {
            metric_id: definition.id.clone(),
            field: "unacceptable_alternative",
        }
    })?;
    validate_schedule(definition, schedule)?;

    let mut effective_clusters = usize::MAX;
    let mut effective_observations = usize::MAX;
    let mut run = |scenario: &str,
                   scenario_index: u64,
                   true_utility_delta: f64|
     -> Result<ScenarioOperatingCharacteristics, PairedGateError> {
        let mut pass = 0_usize;
        let mut regression = 0_usize;
        let mut inconclusive = 0_usize;
        let mut insufficient_support = 0_usize;
        let mut covered = 0_usize;
        let mut total_width = 0.0_f64;
        for trial in 0..schedule.trials {
            let mut data_rng =
                DeterministicRng::new(derive_seed(schedule.seed, scenario_index, trial as u64));
            let trial_definition = definition.with_bootstrap_seed(derive_seed(
                schedule.seed ^ 0x5EED_5EED_5EED_5EED,
                scenario_index,
                trial as u64,
            ));
            let report = evaluate(&trial_definition, true_utility_delta, &mut data_rng)?;
            match report.decision() {
                Decision::Pass => pass += 1,
                Decision::Regression => regression += 1,
                Decision::Inconclusive => inconclusive += 1,
            }
            if !report.decision.support.is_sufficient() {
                insufficient_support += 1;
            }
            if report.decision.lower_bound <= true_utility_delta
                && true_utility_delta <= report.decision.upper_bound
            {
                covered += 1;
            }
            total_width += report.decision.upper_bound - report.decision.lower_bound;
            // The reported support is the weakest any simulated run achieved, so
            // an unbalanced design cannot advertise its best trial.
            effective_clusters = effective_clusters.min(report.decision.support.independent_units);
            effective_observations =
                effective_observations.min(report.decision.support.observations);
        }
        let trials = schedule.trials as f64;
        Ok(ScenarioOperatingCharacteristics {
            scenario: scenario.to_string(),
            true_utility_delta,
            trials: schedule.trials,
            pass_rate: pass as f64 / trials,
            regression_rate: regression as f64 / trials,
            inconclusive_rate: inconclusive as f64 / trials,
            insufficient_support_rate: insufficient_support as f64 / trials,
            coverage: covered as f64 / trials,
            mean_interval_width: total_width / trials,
        })
    };

    let boundary = run("non_inferiority_boundary", 1, -margin)?;
    let acceptable_alternative = run("acceptable_alternative", 2, acceptable)?;
    let unacceptable_alternative = run("unacceptable_alternative", 3, unacceptable)?;

    let mut grid = schedule.degradation_grid.clone();
    grid.sort_by(f64::total_cmp);
    let mut degradation_scan = Vec::with_capacity(grid.len());
    let mut smallest_detected_degradation = None;
    for (index, magnitude) in grid.iter().enumerate() {
        let scenario = run(
            &format!("degradation_{magnitude:.4}"),
            10 + index as u64,
            -magnitude,
        )?;
        if smallest_detected_degradation.is_none()
            && scenario.regression_rate >= schedule.detection_power_target
        {
            smallest_detected_degradation = Some(*magnitude);
        }
        degradation_scan.push(scenario);
    }

    Ok(GateOperatingCharacteristics {
        metric_id: definition.id.clone(),
        trials: schedule.trials,
        seed: schedule.seed,
        alpha: definition.alpha,
        practical_margin: margin,
        effective_independent_clusters: effective_clusters,
        effective_observations,
        boundary,
        acceptable_alternative,
        unacceptable_alternative,
        degradation_scan,
        detection_power_target: schedule.detection_power_target,
        smallest_detected_degradation,
    })
}

fn refuse_design(definition: &MetricDefinition, reason: &str) -> PairedGateError {
    PairedGateError::InvalidSimulationDesign {
        metric_id: definition.id.clone(),
        reason: reason.to_string(),
    }
}

fn validate_schedule(
    definition: &MetricDefinition,
    schedule: &SimulationSchedule,
) -> Result<(), PairedGateError> {
    if schedule.trials == 0 {
        return Err(refuse_design(definition, "trials must be positive"));
    }
    if !(0.0..=1.0).contains(&schedule.detection_power_target)
        || schedule.detection_power_target == 0.0
    {
        return Err(refuse_design(
            definition,
            "detection_power_target must be in (0, 1]",
        ));
    }
    if schedule
        .degradation_grid
        .iter()
        .any(|magnitude| !magnitude.is_finite() || *magnitude <= 0.0)
    {
        return Err(refuse_design(
            definition,
            "degradation magnitudes must be positive",
        ));
    }
    Ok(())
}

fn validate_clustered_design(
    definition: &MetricDefinition,
    design: &ClusteredSimulationDesign,
) -> Result<(), PairedGateError> {
    if design.clusters == 0 || design.observations_per_cluster == 0 {
        return Err(refuse_design(
            definition,
            "design must generate at least one observation",
        ));
    }
    if !design.cluster_effect_sd.is_finite()
        || design.cluster_effect_sd < 0.0
        || !design.observation_noise_sd.is_finite()
        || design.observation_noise_sd < 0.0
    {
        return Err(refuse_design(
            definition,
            "noise standard deviations must be finite and non-negative",
        ));
    }
    if !(0.0..=1.0).contains(&design.discordance_rate) {
        return Err(refuse_design(
            definition,
            "discordance_rate must be in [0, 1]",
        ));
    }
    Ok(())
}

fn validate_live_design(
    definition: &MetricDefinition,
    config: &LiveGateSimulationConfig,
) -> Result<(), PairedGateError> {
    let design = &config.design;
    if design.cases == 0 || design.repetitions_per_case == 0 {
        return Err(refuse_design(
            definition,
            "live design must generate at least one repetition of one case",
        ));
    }
    if design.repetitions_per_case > MAX_REPETITIONS_PER_CASE {
        return Err(refuse_design(
            definition,
            "live design exceeds the supported repetitions per case",
        ));
    }
    if !(0.0..=1.0).contains(&design.baseline_pass_rate) {
        return Err(refuse_design(
            definition,
            "baseline_pass_rate must be in [0, 1]",
        ));
    }
    if !design.case_pass_rate_sd.is_finite() || design.case_pass_rate_sd < 0.0 {
        return Err(refuse_design(
            definition,
            "case_pass_rate_sd must be finite and non-negative",
        ));
    }
    if !(0.0..=1.0).contains(&design.shared_randomness) {
        return Err(refuse_design(
            definition,
            "shared_randomness must be in [0, 1]",
        ));
    }
    // Every injected effect has to be representable as a pass-rate shift, or the
    // clamped generator would silently simulate a smaller degradation than the
    // one the report claims to have scanned.
    let margin = definition.margin()?;
    let injected = [
        Some(-margin),
        definition.acceptable_alternative,
        definition.unacceptable_alternative,
    ]
    .into_iter()
    .flatten()
    .chain(config.degradation_grid.iter().map(|value| -*value));
    for delta in injected {
        if delta.abs() > 1.0 {
            return Err(refuse_design(
                definition,
                "a live pass-rate shift cannot exceed one",
            ));
        }
    }
    Ok(())
}

fn simulated_numeric_observations(
    definition: &MetricDefinition,
    design: &ClusteredSimulationDesign,
    true_utility_delta: f64,
    rng: &mut DeterministicRng,
) -> Vec<PairedNumericObservation> {
    let sign = definition.direction_sign();
    let mut observations = Vec::with_capacity(design.clusters * design.observations_per_cluster);
    for cluster in 0..design.clusters {
        let cluster_effect = design.cluster_effect_sd * rng.next_standard_normal();
        for index in 0..design.observations_per_cluster {
            let noise = design.observation_noise_sd * rng.next_standard_normal();
            let utility = true_utility_delta + cluster_effect + noise;
            observations.push(PairedNumericObservation {
                cluster_id: format!("cluster-{cluster:04}"),
                pair_id: format!("cluster-{cluster:04}#case-{index:04}"),
                baseline: design.baseline_level,
                candidate: design.baseline_level + sign * utility,
            });
        }
    }
    observations
}

fn simulated_binary_observations(
    definition: &MetricDefinition,
    design: &ClusteredSimulationDesign,
    true_utility_delta: f64,
    rng: &mut DeterministicRng,
) -> Vec<PairedBinaryObservation> {
    let candidate_wins_raw = definition.direction_sign() > 0.0;
    let mut observations = Vec::with_capacity(design.clusters * design.observations_per_cluster);
    for cluster in 0..design.clusters {
        let cluster_effect = design.cluster_effect_sd * rng.next_standard_normal();
        let tilt = (true_utility_delta + cluster_effect)
            .clamp(-design.discordance_rate, design.discordance_rate);
        let win_probability = (design.discordance_rate + tilt) / 2.0;
        let loss_probability = (design.discordance_rate - tilt) / 2.0;
        for index in 0..design.observations_per_cluster {
            let draw = rng.next_unit_interval();
            let (baseline, candidate) = if draw < win_probability {
                (!candidate_wins_raw, candidate_wins_raw)
            } else if draw < win_probability + loss_probability {
                (candidate_wins_raw, !candidate_wins_raw)
            } else {
                (candidate_wins_raw, candidate_wins_raw)
            };
            observations.push(PairedBinaryObservation {
                cluster_id: format!("cluster-{cluster:04}"),
                pair_id: format!("cluster-{cluster:04}#case-{index:04}"),
                baseline,
                candidate,
            });
        }
    }
    observations
}

fn simulated_repeated_trials(
    definition: &MetricDefinition,
    design: &ReplicatedLiveDesign,
    true_utility_delta: f64,
    rng: &mut DeterministicRng,
) -> (Vec<RepeatedTrialObservation>, Vec<RepeatedTrialObservation>) {
    let raw_shift = definition.direction_sign() * true_utility_delta;
    // Keep the baseline rate far enough from 0/1 that the injected shift is
    // representable, so the true utility delta the report claims is the one the
    // generated arms actually carry.
    let lowest = (-raw_shift).max(0.0);
    let highest = 1.0 - raw_shift.max(0.0);
    let capacity = design.cases * design.repetitions_per_case as usize;
    let mut baseline = Vec::with_capacity(capacity);
    let mut candidate = Vec::with_capacity(capacity);
    for case in 0..design.cases {
        let case_id = format!("case-{case:04}");
        let baseline_rate = (design.baseline_pass_rate
            + design.case_pass_rate_sd * rng.next_standard_normal())
        .clamp(lowest, highest);
        let candidate_rate = baseline_rate + raw_shift;
        for repetition in 1..=design.repetitions_per_case {
            // One paired seed per repetition is the whole point of common
            // randomness: it fully determines both arms, while
            // `shared_randomness` decides how much of the outcome it pins.
            let paired_seed = rng.next_u64();
            let mut paired = DeterministicRng::new(paired_seed);
            let shared_draw = paired.next_unit_interval();
            let reuses_shared_draw = paired.next_unit_interval() < design.shared_randomness;
            let candidate_draw = if reuses_shared_draw {
                shared_draw
            } else {
                paired.next_unit_interval()
            };
            baseline.push(RepeatedTrialObservation {
                case_id: case_id.clone(),
                repetition,
                paired_seed,
                passed: shared_draw < baseline_rate,
            });
            candidate.push(RepeatedTrialObservation {
                case_id: case_id.clone(),
                repetition,
                paired_seed,
                passed: candidate_draw < candidate_rate,
            });
        }
    }
    (baseline, candidate)
}

/// Power and precision targets a blocking design has to meet before it may gate.
///
/// These are declared, not derived: there is no universal sample size, and a
/// repetition count alone says nothing about population support. Each target
/// names the quantity it bounds so a shortfall reads as a design problem rather
/// than as a bad release.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DesignPowerTargets {
    /// Largest tolerated PASS probability when the truth sits at `-margin`.
    pub max_false_pass_at_margin: f64,
    /// Smallest tolerated PASS probability at the acceptable alternative.
    pub min_pass_power_at_acceptable: f64,
    /// Smallest tolerated REGRESSION probability at the unacceptable alternative.
    pub min_regression_power_at_unacceptable: f64,
    /// Smallest tolerated interval coverage of the true utility delta.
    pub min_coverage: f64,
    /// Largest tolerated mean interval width at the acceptable alternative.
    pub max_interval_width_at_acceptable: f64,
}

/// Whether a simulated design may be used as a blocking gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesignAdequacy {
    /// Every declared operating-characteristic target was met.
    Adequate,
    /// At least one target was missed; the design must not block a release.
    Insufficient,
}

impl DesignAdequacy {
    /// Returns whether the design may be used as a blocking gate.
    #[must_use]
    pub fn is_adequate(self) -> bool {
        matches!(self, Self::Adequate)
    }
}

/// A design's measured operating characteristics judged against its targets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DesignPowerAssessment {
    /// Measured operating characteristics of the exact production gate.
    pub characteristics: GateOperatingCharacteristics,
    /// Targets the design was judged against.
    pub targets: DesignPowerTargets,
    /// Whether the design may block a release.
    pub adequacy: DesignAdequacy,
    /// One line per missed target, in declaration order.
    pub shortfalls: Vec<String>,
}

/// Judges measured operating characteristics against declared design targets.
///
/// A design that misses any target is `Insufficient`, which means the gate it
/// backs must return `INCONCLUSIVE` rather than a green decision. Adding
/// repetitions inside a fixed case set cannot rescue an insufficient design,
/// because the independent unit is the case.
#[must_use]
pub fn assess_design_power(
    characteristics: GateOperatingCharacteristics,
    targets: DesignPowerTargets,
) -> DesignPowerAssessment {
    let mut shortfalls = Vec::new();
    let false_pass = characteristics.false_pass_probability_at_margin();
    if false_pass > targets.max_false_pass_at_margin {
        shortfalls.push(format!(
            "false PASS at -margin is {false_pass:.4}, above the declared {:.4}",
            targets.max_false_pass_at_margin
        ));
    }
    let pass_power = characteristics.pass_power_at_acceptable();
    if pass_power < targets.min_pass_power_at_acceptable {
        shortfalls.push(format!(
            "PASS power at the acceptable alternative is {pass_power:.4}, below the declared {:.4}",
            targets.min_pass_power_at_acceptable
        ));
    }
    let regression_power = characteristics.regression_power_at_unacceptable();
    if regression_power < targets.min_regression_power_at_unacceptable {
        shortfalls.push(format!(
            "regression-detection power at the unacceptable alternative is {regression_power:.4}, below the declared {:.4}",
            targets.min_regression_power_at_unacceptable
        ));
    }
    for scenario in [
        &characteristics.boundary,
        &characteristics.acceptable_alternative,
        &characteristics.unacceptable_alternative,
    ] {
        if scenario.coverage < targets.min_coverage {
            shortfalls.push(format!(
                "coverage in scenario {} is {:.4}, below the declared {:.4}",
                scenario.scenario, scenario.coverage, targets.min_coverage
            ));
        }
    }
    let width = characteristics.acceptable_alternative.mean_interval_width;
    if width > targets.max_interval_width_at_acceptable {
        shortfalls.push(format!(
            "mean interval width at the acceptable alternative is {width:.4}, above the declared {:.4}",
            targets.max_interval_width_at_acceptable
        ));
    }

    let adequacy = if shortfalls.is_empty() {
        DesignAdequacy::Adequate
    } else {
        DesignAdequacy::Insufficient
    };
    DesignPowerAssessment {
        characteristics,
        targets,
        adequacy,
        shortfalls,
    }
}

fn derive_seed(base: u64, scenario: u64, trial: u64) -> u64 {
    let mut value = base
        ^ scenario.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ trial.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 30;
    value = value.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    if value == 0 {
        DEFAULT_BOOTSTRAP_SEED
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_mcnemar_p_value_with_no_discordant_pairs_is_one() {
        // Pins: when both arms always agree (b + c == 0) the exact test reports p = 1.0,
        // not a NaN from 2^-0 bookkeeping.
        let p_value = exact_mcnemar_p_value(0, 0);
        assert_eq!(p_value, 1.0);
        assert!(p_value.is_finite(), "no-discordant p-value must be finite");
    }

    #[test]
    fn exact_mcnemar_p_value_matches_two_sided_binomial() {
        // Pins: the exact two-sided p-value equals 2 * sum_{i<=min} C(n,i) * 0.5^n, capped at 1,
        // so the zero guard is anchored against real discordant-pair math.
        // n = 4, min tail = 1: 2 * (C(4,0) + C(4,1)) / 16 = 2 * 5/16 = 0.625.
        assert!((exact_mcnemar_p_value(3, 1) - 0.625).abs() < 1e-12);
        // n = 5, min tail = 0: 2 * C(5,0) / 32 = 2/32 = 0.0625.
        assert!((exact_mcnemar_p_value(5, 0) - 0.0625).abs() < 1e-12);
        // A near-even split saturates at the 1.0 cap rather than exceeding a probability.
        assert_eq!(exact_mcnemar_p_value(2, 2), 1.0);
    }

    #[test]
    fn exact_mcnemar_p_value_stays_stable_beyond_powf_underflow() {
        // Pins: 2^-1075 underflows, but an almost balanced 1,075-pair sample has
        // a large two-sided p-value and must not collapse to zero.
        let balanced = exact_mcnemar_p_value(538, 537);
        assert!(balanced > 0.95, "balanced p-value was {balanced}");
        assert!(balanced <= 1.0);

        // An extreme tail is smaller than f64 can represent, but remains a
        // positive probability for downstream ordering and correction.
        let extreme = exact_mcnemar_p_value(1_075, 0);
        assert!(extreme.is_finite());
        assert!(extreme > 0.0);
    }
}

#[cfg(test)]
mod gate_tests {
    use moa_eval_core::metric::{
        Estimand, GateKind, HypothesisFamily, MetricDirection, MetricUnit, ResamplingPlan,
    };
    use moa_eval_core::reliability::pass_all_at_k;

    use super::*;

    fn numeric_definition(
        id: &str,
        direction: MetricDirection,
        unit: MetricUnit,
        margin: f64,
        min_independent_units: usize,
    ) -> MetricDefinition {
        MetricDefinition {
            id: id.to_string(),
            direction,
            estimand: Estimand {
                class: MetricClass::PairedNumeric,
                summary: format!("mean paired {id} delta"),
                target_population: "corpus users".to_string(),
            },
            unit,
            independent_unit: "user".to_string(),
            cluster_key: Some("user_id".to_string()),
            paired_key: Some("probe_id".to_string()),
            estimator: Estimator::MeanPairedDelta,
            practical_margin: Some(margin),
            alpha: 0.025,
            confidence_method: ConfidenceMethod::ClusterPairedDeltaBootstrap(ResamplingPlan {
                resamples: 400,
                seed: 0x1357_9BDF,
                min_independent_units,
            }),
            acceptable_alternative: Some(0.0),
            unacceptable_alternative: Some(-3.0 * margin),
            gate_kind: GateKind::RequiredNonInferiority,
            hypothesis_family: HypothesisFamily::Primary,
        }
    }

    fn binary_definition(margin: f64, method: ConfidenceMethod) -> MetricDefinition {
        MetricDefinition {
            id: "case_passed".to_string(),
            direction: MetricDirection::HigherIsBetter,
            estimand: Estimand {
                class: MetricClass::PairedBinary,
                summary: "matched risk difference in pass rate".to_string(),
                target_population: "corpus users".to_string(),
            },
            unit: MetricUnit::Proportion,
            independent_unit: "user".to_string(),
            cluster_key: Some("user_id".to_string()),
            paired_key: Some("case_id".to_string()),
            estimator: Estimator::MatchedRiskDifference,
            practical_margin: Some(margin),
            alpha: 0.025,
            confidence_method: method,
            acceptable_alternative: Some(0.0),
            unacceptable_alternative: Some(-3.0 * margin),
            gate_kind: GateKind::RequiredNonInferiority,
            hypothesis_family: HypothesisFamily::Primary,
        }
    }

    fn constant_delta_observations(
        clusters: usize,
        per_cluster: usize,
        baseline: f64,
        delta: f64,
    ) -> Vec<PairedNumericObservation> {
        let mut observations = Vec::new();
        for cluster in 0..clusters {
            for probe in 0..per_cluster {
                observations.push(PairedNumericObservation {
                    cluster_id: format!("user-{cluster:03}"),
                    pair_id: format!("user-{cluster:03}#probe-{probe}"),
                    baseline,
                    candidate: baseline + delta,
                });
            }
        }
        observations
    }

    #[test]
    fn alpha_parameterised_interval_reproduces_the_existing_percentile_bounds() {
        // Pins: the historical 2.5/97.5 user-cluster interval used by the
        // retrieval and execution comparisons is exactly the alpha = 0.025 case,
        // so formalising the gate did not move any existing reported bound.
        let observations = (0..20)
            .map(|index| ClusterObservation {
                user_id: format!("user-{}", index % 5),
                probe_id: format!("probe-{index}"),
                value: f64::from(index) * 0.01 - 0.1,
            })
            .collect::<Vec<_>>();
        let config = BootstrapConfig {
            resamples: 256,
            seed: 4242,
        };

        let legacy = cluster_bootstrap_mean_by_user("recall", &observations, config);
        let parameterised =
            cluster_bootstrap_mean_by_user_at_alpha("recall", &observations, config, 0.025);
        assert_eq!(legacy, parameterised);

        let tighter =
            cluster_bootstrap_mean_by_user_at_alpha("recall", &observations, config, 0.005);
        assert!(tighter.lower <= legacy.lower);
        assert!(tighter.upper >= legacy.upper);
    }

    #[test]
    fn exact_no_change_fixture_passes_and_a_known_paired_regression_regresses() {
        // Pins: the two anchor fixtures. Identical arms decide PASS with a
        // zero-width interval; a uniform paired drop past the margin decides
        // REGRESSION rather than inconclusive.
        let definition = numeric_definition(
            "recall_at_4",
            MetricDirection::HigherIsBetter,
            MetricUnit::Proportion,
            0.02,
            12,
        );

        let unchanged = evaluate_paired_numeric_gate(
            &definition,
            &constant_delta_observations(20, 4, 0.80, 0.0),
        )
        .expect("unchanged arms decide");
        assert_eq!(unchanged.decision(), Decision::Pass);
        assert_eq!(unchanged.decision.utility_delta, 0.0);
        assert_eq!(unchanged.decision.lower_bound, 0.0);
        assert_eq!(unchanged.decision.upper_bound, 0.0);
        // The paired delta is exactly zero because each pair cancels, but the
        // baseline mean is a sum of 80 binary-inexact 0.80s, so it is compared
        // within tolerance rather than for bit equality.
        assert!(
            (unchanged.baseline_mean - 0.80).abs() < 1e-12,
            "baseline mean should recover 0.80, got {}",
            unchanged.baseline_mean
        );

        let regressed = evaluate_paired_numeric_gate(
            &definition,
            &constant_delta_observations(20, 4, 0.80, -0.10),
        )
        .expect("regressed arms decide");
        assert_eq!(regressed.decision(), Decision::Regression);
        assert!((regressed.decision.utility_delta + 0.10).abs() < 1e-12);
        assert_eq!(regressed.decision.regression_p_value, Some(0.0));
    }

    #[test]
    fn orientation_makes_a_slower_candidate_a_regression_and_a_faster_one_a_pass() {
        // Pins: utility_delta = direction_sign * (candidate - baseline). With
        // the sign dropped or flipped, a latency increase would decide PASS.
        let definition = numeric_definition(
            "p95_latency_ms",
            MetricDirection::LowerIsBetter,
            MetricUnit::Milliseconds,
            5.0,
            12,
        );

        let slower = evaluate_paired_numeric_gate(
            &definition,
            &constant_delta_observations(20, 3, 100.0, 25.0),
        )
        .expect("slower candidate decides");
        assert_eq!(slower.decision(), Decision::Regression);
        assert!((slower.decision.utility_delta + 25.0).abs() < 1e-12);

        let faster = evaluate_paired_numeric_gate(
            &definition,
            &constant_delta_observations(20, 3, 100.0, -25.0),
        )
        .expect("faster candidate decides");
        assert_eq!(faster.decision(), Decision::Pass);
        assert!((faster.decision.utility_delta - 25.0).abs() < 1e-12);
    }

    #[test]
    fn insufficient_cluster_support_decides_inconclusive_not_pass() {
        // Pins: five users cannot carry a population claim even when every
        // paired delta is a large improvement.
        let definition = numeric_definition(
            "recall_at_4",
            MetricDirection::HigherIsBetter,
            MetricUnit::Proportion,
            0.02,
            12,
        );
        let report =
            evaluate_paired_numeric_gate(&definition, &constant_delta_observations(5, 8, 0.5, 0.2))
                .expect("underpowered arms decide");

        assert_eq!(report.decision(), Decision::Inconclusive);
        assert_eq!(report.decision.support.independent_units, 5);
        assert_eq!(report.decision.support.observations, 40);
        assert!(report.decision.rationale.contains("insufficient support"));
    }

    #[test]
    fn pairing_and_cluster_keys_are_mandatory_and_case_sets_must_match() {
        // Pins: the gate refuses observations that cannot support a paired,
        // clustered claim: unpaired arms, duplicate keys, arms that disagree
        // about a case's cluster, and blank keys.
        let baseline = vec![
            ArmObservation {
                cluster_id: "user-a".to_string(),
                pair_id: "probe-1".to_string(),
                value: 1.0,
            },
            ArmObservation {
                cluster_id: "user-a".to_string(),
                pair_id: "probe-2".to_string(),
                value: 1.0,
            },
        ];
        let candidate_missing_case = vec![baseline[0].clone()];
        assert!(matches!(
            pair_numeric_arms(&baseline, &candidate_missing_case),
            Err(PairedGateError::UnpairedCases {
                missing_in_candidate,
                missing_in_baseline,
            }) if missing_in_candidate == ["probe-2"] && missing_in_baseline.is_empty()
        ));

        let duplicated = vec![baseline[0].clone(), baseline[0].clone()];
        assert!(matches!(
            pair_numeric_arms(&duplicated, &duplicated),
            Err(PairedGateError::DuplicatePair { pair_id }) if pair_id == "probe-1"
        ));

        let mut moved_cluster = baseline.clone();
        moved_cluster[0].cluster_id = "user-b".to_string();
        assert!(matches!(
            pair_numeric_arms(&baseline, &moved_cluster),
            Err(PairedGateError::ClusterMismatch { pair_id, .. }) if pair_id == "probe-1"
        ));

        let definition = numeric_definition(
            "recall_at_4",
            MetricDirection::HigherIsBetter,
            MetricUnit::Proportion,
            0.02,
            12,
        );
        let mut unkeyed = constant_delta_observations(20, 2, 0.5, 0.0);
        unkeyed[0].cluster_id = "  ".to_string();
        assert!(matches!(
            evaluate_paired_numeric_gate(&definition, &unkeyed),
            Err(PairedGateError::MissingClusterKey { .. })
        ));

        let mut unpaired = constant_delta_observations(20, 2, 0.5, 0.0);
        unpaired[1].pair_id = String::new();
        assert!(matches!(
            evaluate_paired_numeric_gate(&definition, &unpaired),
            Err(PairedGateError::MissingPairKey { .. })
        ));

        assert!(matches!(
            evaluate_paired_numeric_gate(&definition, &[]),
            Err(PairedGateError::EmptyObservations { .. })
        ));
    }

    #[test]
    fn clustered_binary_outcomes_refuse_the_pair_independent_closed_form() {
        // Pins: probes nested inside users must use a cluster-aware matched
        // risk difference; the independent-pair interval is refused rather
        // than quietly understating its width.
        let clustered = (0..24)
            .flat_map(|user| {
                (0..4).map(move |probe| PairedBinaryObservation {
                    cluster_id: format!("user-{user:03}"),
                    pair_id: format!("user-{user:03}#probe-{probe}"),
                    baseline: true,
                    candidate: true,
                })
            })
            .collect::<Vec<_>>();

        let independent_pairs = binary_definition(
            0.03,
            ConfidenceMethod::MatchedRiskDifferenceAdjustedWald {
                pseudo_count: 0.5,
                min_independent_units: 30,
            },
        );
        assert!(matches!(
            evaluate_paired_binary_gate(&independent_pairs, &clustered),
            Err(PairedGateError::ClusteredBinaryNeedsClusterAwareMethod { clusters, pairs, .. })
                if clusters == 24 && pairs == 96
        ));

        let cluster_aware = binary_definition(
            0.03,
            ConfidenceMethod::ClusterMatchedRiskDifferenceBootstrap(ResamplingPlan {
                resamples: 400,
                seed: 99,
                min_independent_units: 12,
            }),
        );
        let report = evaluate_paired_binary_gate(&cluster_aware, &clustered)
            .expect("cluster-aware method decides");
        assert_eq!(report.decision(), Decision::Pass);
        assert_eq!(report.decision.support.independent_units, 24);
    }

    #[test]
    fn mcnemar_equality_never_substitutes_for_the_margin_decision() {
        // Pins: McNemar tests equality. A cohort with perfectly balanced
        // discordance has p = 1.0 while the cluster-aware matched risk
        // difference is far too wide to establish the declared margin, so the
        // gate returns INCONCLUSIVE instead of borrowing McNemar's answer.
        let observations = (0..24)
            .flat_map(|user| {
                let candidate_wins = user % 2 == 0;
                (0..4).map(move |probe| PairedBinaryObservation {
                    cluster_id: format!("user-{user:03}"),
                    pair_id: format!("user-{user:03}#probe-{probe}"),
                    baseline: !candidate_wins,
                    candidate: candidate_wins,
                })
            })
            .collect::<Vec<_>>();
        let definition = binary_definition(
            0.03,
            ConfidenceMethod::ClusterMatchedRiskDifferenceBootstrap(ResamplingPlan {
                resamples: 600,
                seed: 5,
                min_independent_units: 12,
            }),
        );

        let report =
            evaluate_paired_binary_gate(&definition, &observations).expect("balanced arms decide");
        let diagnostic = report
            .mcnemar_diagnostic
            .as_ref()
            .expect("binary gates report the McNemar diagnostic");

        assert_eq!(diagnostic.control_only_successes, 48);
        assert_eq!(diagnostic.treatment_only_successes, 48);
        assert!((diagnostic.p_value - 1.0).abs() < 1e-12);
        assert!(report.decision.utility_delta.abs() < 1e-12);
        assert_eq!(report.decision(), Decision::Inconclusive);
        assert!(report.decision.lower_bound < -0.03);
    }

    #[test]
    fn matched_risk_difference_closed_form_supports_a_nonzero_margin() {
        // Pins: with independent pairs the closed-form matched interval decides
        // the declared margin, which an exact McNemar test of equality cannot.
        let observations = (0..200)
            .map(|case| PairedBinaryObservation {
                cluster_id: format!("case-{case:03}"),
                pair_id: format!("case-{case:03}"),
                baseline: true,
                candidate: case % 25 != 0,
            })
            .collect::<Vec<_>>();
        let definition = binary_definition(
            0.10,
            ConfidenceMethod::MatchedRiskDifferenceAdjustedWald {
                pseudo_count: 0.5,
                min_independent_units: 30,
            },
        );

        let report = evaluate_paired_binary_gate(&definition, &observations)
            .expect("independent pairs decide");
        assert!((report.decision.utility_delta + 0.04).abs() < 1e-12);
        assert_eq!(report.decision(), Decision::Pass);
        assert!(report.decision.lower_bound >= -0.10);
        assert!(report.decision.lower_bound < 0.0);
        let p_value = report
            .decision
            .regression_p_value
            .expect("closed form reports a one-sided regression p-value");
        assert!(p_value > 0.95, "no regression evidence at a 0.10 margin");
    }

    fn live_definition(
        margin: f64,
        min_independent_units: usize,
        resamples: usize,
    ) -> MetricDefinition {
        MetricDefinition {
            id: "scenario_pass_rate".to_string(),
            direction: MetricDirection::HigherIsBetter,
            estimand: Estimand {
                class: MetricClass::StochasticLive,
                summary: "mean paired per-case scenario pass-rate delta".to_string(),
                target_population: "release scenario cohort".to_string(),
            },
            unit: MetricUnit::Proportion,
            independent_unit: "case".to_string(),
            cluster_key: Some("case_id".to_string()),
            paired_key: Some("case_id#repetition".to_string()),
            estimator: Estimator::MeanPairedCaseDelta,
            practical_margin: Some(margin),
            alpha: 0.05,
            confidence_method: ConfidenceMethod::HierarchicalCaseBootstrap(ResamplingPlan {
                resamples,
                seed: 0x2468_ACE0,
                min_independent_units,
            }),
            acceptable_alternative: Some(0.0),
            unacceptable_alternative: Some(-3.0 * margin),
            gate_kind: GateKind::RequiredNonInferiority,
            hypothesis_family: HypothesisFamily::Primary,
        }
    }

    #[test]
    fn standard_normal_helpers_match_published_values() {
        // Pins: the interval half-width and the one-sided regression p-value
        // both depend on these approximations.
        assert!((standard_normal_quantile(0.975) - 1.959_963_985).abs() < 1e-6);
        assert!((standard_normal_quantile(0.5)).abs() < 1e-9);
        assert!((standard_normal_cdf(0.0) - 0.5).abs() < 1e-7);
        assert!((standard_normal_cdf(1.959_963_985) - 0.975).abs() < 1e-6);
        assert!((standard_normal_cdf(-1.959_963_985) - 0.025).abs() < 1e-6);
    }

    /// The declared retrieval design whose operating characteristics are pinned.
    ///
    /// Sixty user clusters of four probes each, per-cluster utility spread 0.03
    /// and per-probe noise 0.06, decided at a 0.02 margin and a one-sided
    /// alpha of 0.025 by a 250-resample user-cluster percentile bootstrap.
    fn declared_numeric_design() -> (MetricDefinition, GateSimulationConfig) {
        let mut definition = numeric_definition(
            "recall_at_4",
            MetricDirection::HigherIsBetter,
            MetricUnit::Proportion,
            0.02,
            20,
        );
        definition.unacceptable_alternative = Some(-0.06);
        definition.confidence_method =
            ConfidenceMethod::ClusterPairedDeltaBootstrap(ResamplingPlan {
                resamples: 250,
                seed: 0x1357_9BDF,
                min_independent_units: 20,
            });
        let config = GateSimulationConfig {
            trials: 200,
            seed: 0xA11C_E5EE,
            design: ClusteredSimulationDesign {
                clusters: 60,
                observations_per_cluster: 4,
                cluster_effect_sd: 0.03,
                observation_noise_sd: 0.06,
                baseline_level: 0.70,
                discordance_rate: 0.0,
            },
            detection_power_target: 0.80,
            degradation_grid: vec![0.02, 0.04, 0.06],
        };
        (definition, config)
    }

    fn declared_numeric_targets() -> DesignPowerTargets {
        DesignPowerTargets {
            max_false_pass_at_margin: 0.025,
            min_pass_power_at_acceptable: 0.90,
            min_regression_power_at_unacceptable: 0.90,
            min_coverage: 0.90,
            max_interval_width_at_acceptable: 0.025,
        }
    }

    #[test]
    fn seeded_boundary_simulation_holds_the_declared_false_pass_rate_at_the_margin() {
        // Pins the measured operating characteristics of the exact production
        // gate. The simulation calls evaluate_paired_numeric_gate itself, so
        // these are properties of the shipped decision rather than of a model
        // of it. Only the resampling seed varies per simulated trial.
        //
        // Measured for this seeded design: false PASS at utility_delta =
        // -margin is 0.0250, exactly the declared one-sided alpha, and the
        // mirror-image false REGRESSION at the boundary is also 0.0250.
        let (definition, config) = declared_numeric_design();
        let measured =
            simulate_paired_numeric_gate(&definition, &config).expect("numeric gate simulates");

        assert_eq!(measured.effective_independent_clusters, 60);
        assert_eq!(measured.effective_observations, 240);
        assert_eq!(measured.trials, 200);
        assert!((measured.boundary.true_utility_delta + 0.02).abs() < 1e-12);

        let false_pass = measured.false_pass_probability_at_margin();
        assert!(
            (false_pass - 0.025).abs() < 1e-12,
            "false PASS at the boundary moved to {false_pass}"
        );
        assert!(
            false_pass <= definition.alpha,
            "false PASS {false_pass} exceeds the declared alpha {}",
            definition.alpha
        );
        assert!((measured.boundary.regression_rate - 0.025).abs() < 1e-12);
        assert!((measured.boundary.inconclusive_rate - 0.95).abs() < 1e-12);
        // Sixty clusters carry the declared support in every simulated trial, so
        // nothing here is inconclusive for want of clusters.
        assert_eq!(measured.boundary.insufficient_support_rate, 0.0);
        assert!(
            (measured.boundary.coverage - 0.95).abs() < 1e-12,
            "boundary coverage moved to {}",
            measured.boundary.coverage
        );
    }

    #[test]
    fn seeded_alternatives_meet_their_separately_declared_pass_and_regression_power() {
        // Pins: the acceptable and unacceptable alternatives are separate
        // declarations on opposite sides of -margin, and each has its own
        // target. Measured for this seeded design: PASS power 0.9550 at
        // utility_delta = 0, regression-detection power 1.0000 at -0.06, and
        // the smallest degradation reaching 0.80 detection power is 0.04.
        let (definition, config) = declared_numeric_design();
        let measured =
            simulate_paired_numeric_gate(&definition, &config).expect("numeric gate simulates");

        assert_eq!(measured.acceptable_alternative.true_utility_delta, 0.0);
        assert!((measured.unacceptable_alternative.true_utility_delta + 0.06).abs() < 1e-12);
        assert!(
            measured.acceptable_alternative.true_utility_delta > -measured.practical_margin
                && measured.unacceptable_alternative.true_utility_delta
                    < -measured.practical_margin,
            "the two alternatives must straddle -margin"
        );

        let pass_power = measured.pass_power_at_acceptable();
        assert!(
            (pass_power - 0.955).abs() < 1e-12,
            "PASS power at the acceptable alternative moved to {pass_power}"
        );
        let regression_power = measured.regression_power_at_unacceptable();
        assert!(
            (regression_power - 1.0).abs() < 1e-12,
            "regression power at the unacceptable alternative moved to {regression_power}"
        );
        assert_eq!(measured.smallest_detected_degradation, Some(0.04));
        assert_eq!(measured.degradation_scan.len(), 3);
        // The scan is ordered smallest-first, so the reported minimum detectable
        // degradation is the first grid point clearing the power target.
        assert!(measured.degradation_scan[0].regression_rate < config.detection_power_target);
        assert!(measured.degradation_scan[1].regression_rate >= config.detection_power_target);

        let assessment = assess_design_power(measured, declared_numeric_targets());
        assert_eq!(
            assessment.adequacy,
            DesignAdequacy::Adequate,
            "{:?}",
            assessment.shortfalls
        );
        assert!(assessment.shortfalls.is_empty());
    }

    #[test]
    fn a_five_cluster_design_simulates_as_insufficient_rather_than_as_a_pass() {
        // Pins: a percentile cluster bootstrap over a handful of clusters is
        // never presented as population inference. Every simulated run returns
        // INCONCLUSIVE for want of support, including the runs where the truth
        // sits at the acceptable alternative and the interval looks fine.
        let (definition, mut config) = declared_numeric_design();
        config.design.clusters = 5;
        config.trials = 40;
        config.degradation_grid = vec![0.06];
        let measured = simulate_paired_numeric_gate(&definition, &config)
            .expect("an underpowered design still simulates");

        assert_eq!(measured.effective_independent_clusters, 5);
        for scenario in [
            &measured.boundary,
            &measured.acceptable_alternative,
            &measured.unacceptable_alternative,
        ] {
            assert_eq!(scenario.pass_rate, 0.0, "{} passed", scenario.scenario);
            assert_eq!(scenario.regression_rate, 0.0);
            assert_eq!(scenario.inconclusive_rate, 1.0);
            assert_eq!(scenario.insufficient_support_rate, 1.0);
        }
        assert_eq!(measured.smallest_detected_degradation, None);

        let assessment = assess_design_power(measured, declared_numeric_targets());
        assert_eq!(assessment.adequacy, DesignAdequacy::Insufficient);
        assert!(
            assessment
                .shortfalls
                .iter()
                .any(|shortfall| shortfall.contains("PASS power")),
            "{:?}",
            assessment.shortfalls
        );
    }

    #[test]
    fn an_exact_metric_cannot_be_simulated_for_a_minimum_detectable_effect() {
        // Pins: a fixed-corpus invariant is decided by counting, so power, a
        // non-inferiority boundary, and a smallest detected degradation are all
        // meaningless for it. Attaching an inferential detectable effect is
        // refused by name rather than quietly simulating a margin of zero.
        let exact = MetricDefinition {
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
        };
        exact.validate().expect("the exact declaration is valid");
        let (_, config) = declared_numeric_design();

        assert!(matches!(
            simulate_paired_numeric_gate(&exact, &config),
            Err(PairedGateError::InferentialMdeOnExactMetric { metric_id })
                if metric_id == "cross_user_leak_count"
        ));
        assert!(matches!(
            simulate_paired_binary_gate(&exact, &config),
            Err(PairedGateError::InferentialMdeOnExactMetric { .. })
        ));
    }

    fn repeated_trials(rows: &[(&str, u32, u64, bool)]) -> Vec<RepeatedTrialObservation> {
        rows.iter()
            .map(
                |(case_id, repetition, paired_seed, passed)| RepeatedTrialObservation {
                    case_id: (*case_id).to_string(),
                    repetition: *repetition,
                    paired_seed: *paired_seed,
                    passed: *passed,
                },
            )
            .collect()
    }

    fn scripted_arm(cases: &[(&str, &[bool])]) -> Vec<RepeatedTrialObservation> {
        cases
            .iter()
            .flat_map(|(case_id, outcomes)| {
                outcomes
                    .iter()
                    .enumerate()
                    .map(move |(index, passed)| RepeatedTrialObservation {
                        case_id: (*case_id).to_string(),
                        repetition: index as u32 + 1,
                        // The paired seed is shared by construction here, which is
                        // exactly the property `pair_repeated_trials` enforces.
                        paired_seed: 0x5EED_0000 + index as u64,
                        passed: *passed,
                    })
            })
            .collect()
    }

    #[test]
    fn repeated_live_trials_pair_by_case_and_repetition_and_refuse_a_seed_mismatch() {
        // Pins: a reliability-aware release decision is only paired if both arms
        // ran the same repetition of the same case under the same seed. A seed
        // mismatch, a missing repetition, and a duplicate repetition are each
        // refused instead of being differenced or dropped.
        let definition = live_definition(0.05, 2, 64);
        let baseline = repeated_trials(&[
            ("case-a", 1, 11, true),
            ("case-a", 2, 12, true),
            ("case-b", 1, 21, true),
            ("case-b", 2, 22, false),
        ]);

        let paired = pair_repeated_trials(&definition, &baseline, &baseline)
            .expect("identical arms are paired");
        assert_eq!(paired.len(), 4);
        assert_eq!(paired[0].cluster_id, "case-a");
        assert_eq!(paired[0].pair_id, "case-a#rep-1");

        let mut reseeded = baseline.clone();
        reseeded[1].paired_seed = 99;
        assert!(matches!(
            pair_repeated_trials(&definition, &baseline, &reseeded),
            Err(PairedGateError::PairedSeedMismatch {
                case_id,
                repetition,
                baseline_seed,
                candidate_seed,
                ..
            }) if case_id == "case-a"
                && repetition == 2
                && baseline_seed == 12
                && candidate_seed == 99
        ));

        let truncated = baseline[..3].to_vec();
        assert!(matches!(
            pair_repeated_trials(&definition, &baseline, &truncated),
            Err(PairedGateError::UnpairedCases { missing_in_candidate, missing_in_baseline })
                if missing_in_candidate == ["case-b#rep-2"] && missing_in_baseline.is_empty()
        ));

        let mut duplicated = baseline.clone();
        duplicated.push(baseline[0].clone());
        assert!(matches!(
            pair_repeated_trials(&definition, &duplicated, &duplicated),
            Err(PairedGateError::DuplicatePair { pair_id }) if pair_id == "case-a#rep-1"
        ));
    }

    #[test]
    fn live_gate_reliability_is_computed_per_case_and_never_pooled() {
        // Pins: reliability is a statement about re-running one case, so it is
        // computed per case and then averaged. Pooling the same successes into
        // one eight-attempt draw answers a different question and must not be
        // what the report shows.
        let definition = live_definition(0.05, 2, 64);
        let baseline = scripted_arm(&[
            ("case-a", &[true, true, true, true]),
            ("case-b", &[true, true, true, true]),
        ]);
        let candidate = scripted_arm(&[
            ("case-a", &[true, true, true, true]),
            ("case-b", &[false, false, false, false]),
        ]);

        let report = evaluate_repeated_live_gate(
            &definition,
            &baseline,
            &candidate,
            TrialIndependence::IndependentRepetitions,
        )
        .expect("scripted live arms decide");

        assert_eq!(report.candidate_reliability.len(), 4);
        assert_eq!(report.candidate_single_run_pass_rate(), Some(0.5));
        let all_at_four = report
            .candidate_reliability
            .iter()
            .find(|estimate| estimate.k == 4)
            .expect("the curve covers k = 4");
        assert_eq!(all_at_four.pass_all_at_k, 0.5);
        assert_eq!(all_at_four.case_count, 2);
        assert_eq!(all_at_four.trial_count, 8);
        let pooled = pass_all_at_k(8, 4, 4).expect("pooled draw");
        assert!(
            (pooled - all_at_four.pass_all_at_k).abs() > 1e-9,
            "the per-case average must differ from the pooled draw"
        );
        assert_eq!(
            report
                .baseline_reliability
                .iter()
                .find(|estimate| estimate.k == 4)
                .map(|estimate| estimate.pass_all_at_k),
            Some(1.0)
        );

        // The observed paired delta is a catastrophic -0.5, and the gate still
        // refuses to call it a regression: resampling two cases produces samples
        // as high as zero, so the interval straddles -margin. Support clears the
        // declared floor here, which is why this is an interval-width
        // INCONCLUSIVE rather than an insufficient-support one — the two are
        // reported separately because they have different remedies.
        assert!((report.gate.decision.utility_delta + 0.5).abs() < 1e-12);
        assert_eq!(report.decision(), Decision::Inconclusive);
        assert!(report.gate.decision.support.is_sufficient());
        assert!(
            report.gate.decision.rationale.contains("straddles"),
            "{}",
            report.gate.decision.rationale
        );
        assert!(report.gate.decision.lower_bound < -0.05);
        assert!(report.gate.decision.upper_bound >= -0.05);
        assert_eq!(report.gate.decision.support.independent_units, 2);
        assert_eq!(report.gate.decision.support.observations, 8);
    }

    #[test]
    fn branched_rollouts_never_back_a_reliability_aware_release_decision() {
        // Pins: shared-prefix branches are correlated by construction, so they
        // are refused here rather than inflating the independent-trial
        // estimators; they belong in a separately labelled failure-discovery
        // diagnostic.
        let definition = live_definition(0.05, 2, 64);
        let arm = scripted_arm(&[
            ("case-a", &[true, false, true, true]),
            ("case-b", &[true, true, false, true]),
        ]);
        assert!(matches!(
            evaluate_repeated_live_gate(
                &definition,
                &arm,
                &arm,
                TrialIndependence::SharedPrefixBranched,
            ),
            Err(PairedGateError::BranchedRolloutsNotIndependent { .. })
        ));
        assert!(
            evaluate_repeated_live_gate(
                &definition,
                &arm,
                &arm,
                TrialIndependence::IndependentRepetitions,
            )
            .is_ok()
        );
    }

    #[test]
    fn a_scripted_flaky_candidate_is_inconclusive_at_insufficient_support_not_green() {
        // Pins: a candidate that looks better on a handful of cases does not get
        // a green decision. Six cases cannot support the declared population
        // claim, so the gate returns INCONCLUSIVE and says why, even though the
        // observed paired delta is positive.
        let definition = live_definition(0.05, 30, 128);
        let baseline = scripted_arm(&[
            ("case-a", &[true, false, false, false]),
            ("case-b", &[false, false, false, false]),
            ("case-c", &[true, false, false, false]),
            ("case-d", &[false, false, false, false]),
            ("case-e", &[true, false, false, false]),
            ("case-f", &[false, false, false, false]),
        ]);
        let candidate = scripted_arm(&[
            ("case-a", &[true, true, false, true]),
            ("case-b", &[false, true, true, false]),
            ("case-c", &[true, true, true, false]),
            ("case-d", &[true, false, true, true]),
            ("case-e", &[true, true, false, true]),
            ("case-f", &[false, true, true, true]),
        ]);

        let report = evaluate_repeated_live_gate(
            &definition,
            &baseline,
            &candidate,
            TrialIndependence::IndependentRepetitions,
        )
        .expect("a flaky candidate still produces a decision");

        assert!(
            report.gate.decision.utility_delta > 0.0,
            "the fixture must look like an improvement, got {}",
            report.gate.decision.utility_delta
        );
        assert_eq!(report.decision(), Decision::Inconclusive);
        assert!(
            report
                .gate
                .decision
                .rationale
                .contains("insufficient support"),
            "{}",
            report.gate.decision.rationale
        );
        assert!(!report.gate.decision.support.is_sufficient());
        assert_eq!(report.gate.decision.support.independent_units, 6);
        assert_eq!(report.gate.decision.support.required_independent_units, 30);
        assert_eq!(report.gate.decision.regression_p_value, None);
        // The candidate really is flakier than a single run suggests: reliability
        // still reports it, but reporting is not gating.
        assert!(
            report
                .candidate_single_run_pass_rate()
                .expect("k = 1 is in the curve")
                > 0.5
        );
    }

    fn declared_live_targets() -> DesignPowerTargets {
        DesignPowerTargets {
            max_false_pass_at_margin: 0.05,
            min_pass_power_at_acceptable: 0.90,
            min_regression_power_at_unacceptable: 0.90,
            min_coverage: 0.90,
            max_interval_width_at_acceptable: 0.08,
        }
    }

    fn live_simulation(cases: usize, repetitions_per_case: u32) -> LiveGateSimulationConfig {
        LiveGateSimulationConfig {
            trials: 120,
            seed: 0xBEEF_1234,
            design: ReplicatedLiveDesign {
                cases,
                repetitions_per_case,
                baseline_pass_rate: 0.70,
                case_pass_rate_sd: 0.15,
                shared_randomness: 0.80,
            },
            detection_power_target: 0.80,
            degradation_grid: vec![0.05, 0.15],
        }
    }

    #[test]
    fn four_repetitions_alone_are_not_a_meaningful_blocking_floor() {
        // Pins the design-specific power analysis for a stochastic live release
        // gate. The independent unit is the case, so repetitions buy precision
        // on each case's pass rate and almost no population support.
        //
        // Four cases repeated forty times and forty cases repeated four times
        // record the *same* 160 observations, and neither may block. Measured
        // over 120 seeded runs per scenario:
        //
        //   4 cases x 40 reps: every run INCONCLUSIVE for want of clusters;
        //                      PASS power 0.0000, regression power 0.0000.
        //   40 cases x 4 reps: PASS power 0.6917, regression power 0.8250,
        //                      mean interval width 0.0907 at the acceptable
        //                      alternative - all three below declared targets.
        //   100 cases x 4 reps: false PASS 0.0333 at -margin, PASS power
        //                      0.9417, regression power 0.9917, width 0.0590,
        //                      smallest detected degradation 0.15.
        //
        // Only the third design is adequate, and it got there by adding cases.
        let definition = live_definition(0.05, 30, 200);
        let targets = declared_live_targets();

        let repetition_heavy = assess_design_power(
            simulate_repeated_live_gate(&definition, &live_simulation(4, 40))
                .expect("the repetition-heavy design simulates"),
            targets,
        );
        assert_eq!(
            repetition_heavy.characteristics.effective_observations, 160,
            "the repetition-heavy design records 160 observations"
        );
        assert_eq!(
            repetition_heavy
                .characteristics
                .effective_independent_clusters,
            4
        );
        assert_eq!(
            repetition_heavy.characteristics.boundary.inconclusive_rate,
            1.0
        );
        assert_eq!(
            repetition_heavy
                .characteristics
                .boundary
                .insufficient_support_rate,
            1.0
        );
        assert_eq!(
            repetition_heavy.characteristics.pass_power_at_acceptable(),
            0.0
        );
        assert_eq!(
            repetition_heavy
                .characteristics
                .regression_power_at_unacceptable(),
            0.0
        );
        assert_eq!(repetition_heavy.adequacy, DesignAdequacy::Insufficient);

        let case_light = assess_design_power(
            simulate_repeated_live_gate(&definition, &live_simulation(40, 4))
                .expect("the forty-case design simulates"),
            targets,
        );
        assert_eq!(
            case_light.characteristics.effective_observations, 160,
            "the same observation count as the repetition-heavy design"
        );
        assert_eq!(
            case_light.characteristics.effective_independent_clusters,
            40
        );
        let case_light_pass_power = case_light.characteristics.pass_power_at_acceptable();
        assert!(
            (case_light_pass_power - 83.0 / 120.0).abs() < 1e-12,
            "PASS power at forty cases moved to {case_light_pass_power}"
        );
        let case_light_regression_power = case_light
            .characteristics
            .regression_power_at_unacceptable();
        assert!(
            (case_light_regression_power - 99.0 / 120.0).abs() < 1e-12,
            "regression power at forty cases moved to {case_light_regression_power}"
        );
        assert_eq!(
            case_light.characteristics.smallest_detected_degradation,
            None
        );
        assert_eq!(case_light.adequacy, DesignAdequacy::Insufficient);
        assert_eq!(
            case_light.shortfalls.len(),
            3,
            "{:?}",
            case_light.shortfalls
        );

        let adequate = assess_design_power(
            simulate_repeated_live_gate(&definition, &live_simulation(100, 4))
                .expect("the hundred-case design simulates"),
            targets,
        );
        assert_eq!(adequate.characteristics.effective_independent_clusters, 100);
        assert_eq!(adequate.characteristics.effective_observations, 400);
        let false_pass = adequate.characteristics.false_pass_probability_at_margin();
        assert!(
            (false_pass - 4.0 / 120.0).abs() < 1e-12,
            "false PASS at a hundred cases moved to {false_pass}"
        );
        assert!(false_pass <= definition.alpha);
        let pass_power = adequate.characteristics.pass_power_at_acceptable();
        assert!(
            (pass_power - 113.0 / 120.0).abs() < 1e-12,
            "PASS power at a hundred cases moved to {pass_power}"
        );
        let regression_power = adequate.characteristics.regression_power_at_unacceptable();
        assert!(
            (regression_power - 119.0 / 120.0).abs() < 1e-12,
            "regression power at a hundred cases moved to {regression_power}"
        );
        assert_eq!(
            adequate.characteristics.smallest_detected_degradation,
            Some(0.15)
        );
        assert_eq!(
            adequate.adequacy,
            DesignAdequacy::Adequate,
            "{:?}",
            adequate.shortfalls
        );

        // Precision improves with cases, not with repetitions inside four cases.
        assert!(
            adequate
                .characteristics
                .acceptable_alternative
                .mean_interval_width
                < case_light
                    .characteristics
                    .acceptable_alternative
                    .mean_interval_width
        );
    }

    #[test]
    fn a_live_simulation_design_is_validated_before_any_run() {
        // Pins: a malformed simulated design is refused by name instead of
        // producing operating characteristics for a design that cannot exist.
        let definition = live_definition(0.05, 30, 64);
        type MutateDesign = fn(&mut LiveGateSimulationConfig);
        let malformed: [(&str, MutateDesign); 6] = [
            ("at least one repetition", |config| {
                config.design.cases = 0;
            }),
            ("supported repetitions", |config| {
                config.design.repetitions_per_case = MAX_REPETITIONS_PER_CASE + 1;
            }),
            ("baseline_pass_rate", |config| {
                config.design.baseline_pass_rate = 1.5;
            }),
            ("shared_randomness", |config| {
                config.design.shared_randomness = -0.1;
            }),
            ("degradation magnitudes", |config| {
                config.degradation_grid = vec![0.0];
            }),
            ("trials must be positive", |config| {
                config.trials = 0;
            }),
        ];
        for (reason, mutate) in malformed {
            let mut config = live_simulation(8, 4);
            mutate(&mut config);
            let error = simulate_repeated_live_gate(&definition, &config)
                .expect_err("a malformed design must be refused");
            assert!(
                error.to_string().contains(reason),
                "expected `{reason}` in `{error}`"
            );
        }
    }
}
