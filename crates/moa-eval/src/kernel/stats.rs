//! Deterministic statistical comparisons for memory retrieval evaluation.

use std::collections::BTreeMap;

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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterObservation {
    /// User cluster identifier used for resampling.
    pub user_id: String,
    /// Probe identifier retained for report auditability.
    pub probe_id: String,
    /// Per-probe metric value.
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

/// Computes a 2.5/97.5 percentile confidence interval by resampling users.
#[must_use]
pub fn cluster_bootstrap_mean_by_user(
    metric_name: impl Into<String>,
    observations: &[ClusterObservation],
    config: BootstrapConfig,
) -> ClusterBootstrapReport {
    let metric_name = metric_name.into();
    let mean = mean(observations.iter().map(|observation| observation.value));
    let clusters = observations_by_user(observations);
    if observations.is_empty() || clusters.is_empty() || config.resamples == 0 {
        return ClusterBootstrapReport {
            metric_name,
            resamples: config.resamples,
            seed: config.seed,
            cluster_count: clusters.len(),
            observation_count: observations.len(),
            mean,
            lower_percentile: 2.5,
            lower: mean,
            upper_percentile: 97.5,
            upper: mean,
        };
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

    ClusterBootstrapReport {
        metric_name,
        resamples: config.resamples,
        seed: config.seed,
        cluster_count: clusters.len(),
        observation_count: observations.len(),
        mean,
        lower_percentile: 2.5,
        lower: percentile(&samples, 2.5),
        upper_percentile: 97.5,
        upper: percentile(&samples, 97.5),
    }
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
    let mut probability = 2.0_f64.powf(-(mismatched_pairs as f64));
    let mut tail = 0.0;
    for successes in 0..=smaller_tail {
        if successes > 0 {
            probability *= (mismatched_pairs - successes + 1) as f64 / successes as f64;
        }
        tail += probability;
    }
    (2.0 * tail).min(1.0)
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
}
