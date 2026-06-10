//! Paired report comparison utilities shared by eval suites.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{
    BinaryProbeOutcome, BootstrapConfig, ClusterBootstrapReport, ClusterObservation,
    PairedComparison, RetrievalCoreMetrics, benjamini_hochberg, cluster_bootstrap_mean_by_user,
    mcnemar_paired_test,
};

const COMPARED_METRICS: &[ComparedMetric] = &[
    ComparedMetric::RecallAt4,
    ComparedMetric::Mrr,
    ComparedMetric::NdcgAt4,
];

/// Error returned by paired report comparison.
#[derive(Debug, thiserror::Error)]
pub enum CompareReportsError {
    /// A report file could not be read.
    #[error("read report {path}: {source}")]
    ReadReport {
        /// Path that failed to read.
        path: String,
        /// I/O source error.
        source: std::io::Error,
    },
    /// A report file could not be parsed.
    #[error("parse report {label}: {source}")]
    ParseReport {
        /// Logical report label.
        label: &'static str,
        /// JSON source error.
        source: serde_json::Error,
    },
    /// Reports describe different corpora.
    #[error("corpus mismatch ({baseline} vs {candidate}); paired comparison refused")]
    CorpusMismatch {
        /// Baseline corpus id.
        baseline: String,
        /// Candidate corpus id.
        candidate: String,
    },
    /// Reports were generated from different seed sets.
    #[error("corpus seed mismatch ({baseline:?} vs {candidate:?}); paired comparison refused")]
    SeedMismatch {
        /// Baseline seed list.
        baseline: Vec<u64>,
        /// Candidate seed list.
        candidate: Vec<u64>,
    },
    /// Reports contain different probe sets.
    #[error(
        "probe id set mismatch; missing in candidate: {missing_in_candidate:?}; missing in baseline: {missing_in_baseline:?}"
    )]
    ProbeSetMismatch {
        /// Probe ids present in baseline but not candidate.
        missing_in_candidate: Vec<String>,
        /// Probe ids present in candidate but not baseline.
        missing_in_baseline: Vec<String>,
    },
    /// Reports use different final cutoffs.
    #[error("final_k mismatch ({baseline} vs {candidate}); paired comparison refused")]
    FinalKMismatch {
        /// Baseline final cutoff.
        baseline: usize,
        /// Candidate final cutoff.
        candidate: usize,
    },
}

impl CompareReportsError {
    /// Returns whether this error should use the paired-comparison exit code.
    #[must_use]
    pub fn is_pairing_error(&self) -> bool {
        matches!(
            self,
            Self::CorpusMismatch { .. }
                | Self::SeedMismatch { .. }
                | Self::ProbeSetMismatch { .. }
                | Self::FinalKMismatch { .. }
        )
    }
}

/// Summary produced by comparing two paired retrieval reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReportComparison {
    /// Corpus id shared by both reports.
    pub corpus_id: String,
    /// Number of probes paired by id.
    pub probes_paired: usize,
    /// Metric-level paired deltas and intervals.
    pub metrics: Vec<MetricComparison>,
    /// McNemar binary comparisons after BH correction.
    pub mcnemar: Vec<PairedComparison>,
}

impl EvalReportComparison {
    /// Renders a fixed-width human-readable comparison table.
    #[must_use]
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "corpus: {} (probes paired: {})\n",
            self.corpus_id, self.probes_paired
        ));
        out.push_str(
            "metric        baseline  candidate  delta    ci95             p_adj   verdict\n",
        );
        for metric in &self.metrics {
            out.push_str(&format!(
                "{:<13} {:>8.3}  {:>9.3}  {:+.3}   [{:+.3},{:+.3}]  {:>6.3}  {}\n",
                metric.metric_name,
                metric.baseline,
                metric.candidate,
                metric.delta,
                metric.ci95_lower,
                metric.ci95_upper,
                metric.adjusted_p_value,
                metric.verdict
            ));
        }
        if let Some(recall) = self
            .mcnemar
            .iter()
            .find(|comparison| comparison.metric_name == ComparedMetric::RecallAt4.name())
        {
            out.push_str(&format!(
                "mcnemar: b={} c={} p={:.3} (adjusted {:.3})\n",
                recall.control_only_successes,
                recall.treatment_only_successes,
                recall.p_value,
                recall.adjusted_p_value
            ));
        }
        out
    }
}

/// One metric row in a paired report comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricComparison {
    /// Metric name.
    pub metric_name: String,
    /// Baseline aggregate value.
    pub baseline: f64,
    /// Candidate aggregate value.
    pub candidate: f64,
    /// Candidate minus baseline.
    pub delta: f64,
    /// Lower cluster-bootstrap confidence interval bound for the paired delta.
    pub ci95_lower: f64,
    /// Upper cluster-bootstrap confidence interval bound for the paired delta.
    pub ci95_upper: f64,
    /// Raw McNemar p-value for this metric's binary success projection.
    pub p_value: f64,
    /// BH-adjusted p-value.
    pub adjusted_p_value: f64,
    /// Ship verdict for this metric row.
    pub verdict: String,
}

/// Compares two report files by paired probe id.
pub fn compare_eval_report_files(
    baseline_path: &Path,
    candidate_path: &Path,
) -> Result<EvalReportComparison, CompareReportsError> {
    let baseline =
        fs::read_to_string(baseline_path).map_err(|source| CompareReportsError::ReadReport {
            path: baseline_path.display().to_string(),
            source,
        })?;
    let candidate =
        fs::read_to_string(candidate_path).map_err(|source| CompareReportsError::ReadReport {
            path: candidate_path.display().to_string(),
            source,
        })?;
    compare_eval_reports(&baseline, &candidate)
}

/// Compares two JSON report bodies using the default bootstrap configuration.
pub fn compare_eval_reports(
    baseline_json: &str,
    candidate_json: &str,
) -> Result<EvalReportComparison, CompareReportsError> {
    compare_eval_reports_with_config(baseline_json, candidate_json, BootstrapConfig::default())
}

/// Compares two JSON report bodies using an explicit bootstrap configuration.
pub fn compare_eval_reports_with_config(
    baseline_json: &str,
    candidate_json: &str,
    bootstrap_config: BootstrapConfig,
) -> Result<EvalReportComparison, CompareReportsError> {
    let baseline: ComparableReport =
        serde_json::from_str(baseline_json).map_err(|source| CompareReportsError::ParseReport {
            label: "baseline",
            source,
        })?;
    let candidate: ComparableReport = serde_json::from_str(candidate_json).map_err(|source| {
        CompareReportsError::ParseReport {
            label: "candidate",
            source,
        }
    })?;
    compare_reports(baseline, candidate, bootstrap_config)
}

fn compare_reports(
    baseline: ComparableReport,
    candidate: ComparableReport,
    bootstrap_config: BootstrapConfig,
) -> Result<EvalReportComparison, CompareReportsError> {
    validate_pairing(&baseline, &candidate)?;
    let baseline_by_probe = baseline.probe_map();
    let candidate_by_probe = candidate.probe_map();
    let mut mcnemar = COMPARED_METRICS
        .iter()
        .map(|metric| {
            mcnemar_paired_test(
                metric.name(),
                &binary_outcomes(&baseline_by_probe, *metric, baseline.final_k),
                &binary_outcomes(&candidate_by_probe, *metric, candidate.final_k),
            )
        })
        .collect::<Vec<_>>();
    mcnemar = benjamini_hochberg(mcnemar, 0.05);
    let p_by_metric = mcnemar
        .iter()
        .map(|comparison| (comparison.metric_name.as_str(), comparison))
        .collect::<BTreeMap<_, _>>();

    let metrics = COMPARED_METRICS
        .iter()
        .map(|metric| {
            let interval = cluster_bootstrap_mean_by_user(
                metric.name(),
                &paired_delta_observations(
                    &baseline_by_probe,
                    &candidate_by_probe,
                    *metric,
                    baseline.final_k,
                    candidate.final_k,
                ),
                bootstrap_config,
            );
            metric_comparison(
                *metric,
                &baseline.metrics,
                &candidate.metrics,
                &interval,
                p_by_metric
                    .get(metric.name())
                    .expect("mcnemar comparison should exist for each metric"),
            )
        })
        .collect();

    Ok(EvalReportComparison {
        corpus_id: baseline.manifest.corpus_id,
        probes_paired: baseline_by_probe.len(),
        metrics,
        mcnemar,
    })
}

fn validate_pairing(
    baseline: &ComparableReport,
    candidate: &ComparableReport,
) -> Result<(), CompareReportsError> {
    if baseline.manifest.corpus_id != candidate.manifest.corpus_id {
        return Err(CompareReportsError::CorpusMismatch {
            baseline: baseline.manifest.corpus_id.clone(),
            candidate: candidate.manifest.corpus_id.clone(),
        });
    }
    if baseline.manifest.seeds != candidate.manifest.seeds {
        return Err(CompareReportsError::SeedMismatch {
            baseline: baseline.manifest.seeds.clone(),
            candidate: candidate.manifest.seeds.clone(),
        });
    }
    if baseline.final_k != candidate.final_k {
        return Err(CompareReportsError::FinalKMismatch {
            baseline: baseline.final_k,
            candidate: candidate.final_k,
        });
    }

    let baseline_probe_ids = baseline.probe_ids();
    let candidate_probe_ids = candidate.probe_ids();
    if baseline_probe_ids != candidate_probe_ids {
        return Err(CompareReportsError::ProbeSetMismatch {
            missing_in_candidate: baseline_probe_ids
                .difference(&candidate_probe_ids)
                .cloned()
                .collect(),
            missing_in_baseline: candidate_probe_ids
                .difference(&baseline_probe_ids)
                .cloned()
                .collect(),
        });
    }
    Ok(())
}

fn metric_comparison(
    metric: ComparedMetric,
    baseline: &RetrievalCoreMetrics,
    candidate: &RetrievalCoreMetrics,
    interval: &ClusterBootstrapReport,
    p_value: &PairedComparison,
) -> MetricComparison {
    let baseline_value = metric.aggregate_value(baseline);
    let candidate_value = metric.aggregate_value(candidate);
    let delta = candidate_value - baseline_value;
    let adjusted_p_value = p_value.adjusted_p_value;
    MetricComparison {
        metric_name: metric.name().to_string(),
        baseline: baseline_value,
        candidate: candidate_value,
        delta,
        ci95_lower: interval.lower,
        ci95_upper: interval.upper,
        p_value: p_value.p_value,
        adjusted_p_value,
        verdict: if delta > 0.0 && interval.lower > 0.0 && adjusted_p_value < 0.05 {
            "SHIP".to_string()
        } else {
            "HOLD".to_string()
        },
    }
}

fn paired_delta_observations(
    baseline_by_probe: &BTreeMap<String, ComparableProbeResult>,
    candidate_by_probe: &BTreeMap<String, ComparableProbeResult>,
    metric: ComparedMetric,
    baseline_final_k: usize,
    candidate_final_k: usize,
) -> Vec<ClusterObservation> {
    baseline_by_probe
        .iter()
        .filter_map(|(probe_id, baseline_probe)| {
            let candidate_probe = candidate_by_probe
                .get(probe_id)
                .expect("probe sets were validated before comparison");
            let baseline_value = metric.probe_value(baseline_probe, baseline_final_k)?;
            let candidate_value = metric.probe_value(candidate_probe, candidate_final_k)?;
            Some(ClusterObservation {
                user_id: baseline_probe.user_id.clone(),
                probe_id: probe_id.clone(),
                value: candidate_value - baseline_value,
            })
        })
        .collect()
}

fn binary_outcomes(
    probes: &BTreeMap<String, ComparableProbeResult>,
    metric: ComparedMetric,
    final_k: usize,
) -> Vec<BinaryProbeOutcome> {
    probes
        .values()
        .filter(|probe| !probe.expected_fact_ids.is_empty())
        .map(|probe| BinaryProbeOutcome {
            probe_id: probe.probe_id.clone(),
            success: metric.binary_success(probe, final_k),
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
struct ComparableReport {
    manifest: ComparableManifest,
    final_k: usize,
    metrics: RetrievalCoreMetrics,
    probe_results: Vec<ComparableProbeResult>,
}

impl ComparableReport {
    fn probe_ids(&self) -> BTreeSet<String> {
        self.probe_results
            .iter()
            .map(|probe| probe.probe_id.clone())
            .collect()
    }

    fn probe_map(&self) -> BTreeMap<String, ComparableProbeResult> {
        self.probe_results
            .iter()
            .map(|probe| (probe.probe_id.clone(), probe.clone()))
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ComparableManifest {
    corpus_id: String,
    #[serde(default)]
    seeds: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct ComparableProbeResult {
    probe_id: String,
    user_id: String,
    #[serde(default)]
    expected_fact_ids: Vec<String>,
    #[serde(default)]
    candidates: Vec<ComparableCandidate>,
    #[serde(default)]
    post_rerank_candidates: Option<Vec<ComparableCandidate>>,
}

impl ComparableProbeResult {
    fn final_candidates(&self) -> &[ComparableCandidate] {
        self.post_rerank_candidates
            .as_deref()
            .unwrap_or(&self.candidates)
    }

    fn expected_fact_set(&self) -> BTreeSet<&str> {
        self.expected_fact_ids.iter().map(String::as_str).collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ComparableCandidate {
    rank: usize,
    fact_id: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum ComparedMetric {
    RecallAt4,
    Mrr,
    NdcgAt4,
}

impl ComparedMetric {
    fn name(self) -> &'static str {
        match self {
            Self::RecallAt4 => "recall_at_4",
            Self::Mrr => "mrr",
            Self::NdcgAt4 => "ndcg_at_4",
        }
    }

    fn aggregate_value(self, metrics: &RetrievalCoreMetrics) -> f64 {
        match self {
            Self::RecallAt4 => metrics.recall_at_4.value,
            Self::Mrr => metrics.mrr.value,
            Self::NdcgAt4 => metrics.ndcg_at_4.value,
        }
    }

    fn probe_value(self, probe: &ComparableProbeResult, final_k: usize) -> Option<f64> {
        match self {
            Self::RecallAt4 => recall_at(probe, final_k),
            Self::Mrr => reciprocal_rank(probe),
            Self::NdcgAt4 => ndcg_at(probe, final_k),
        }
    }

    fn binary_success(self, probe: &ComparableProbeResult, final_k: usize) -> bool {
        match self {
            Self::RecallAt4 => all_expected_found_at_k(probe, final_k),
            Self::Mrr => reciprocal_rank(probe).is_some_and(|value| value > 0.0),
            Self::NdcgAt4 => ndcg_at(probe, final_k).is_some_and(|value| value > 0.0),
        }
    }
}

fn recall_at(probe: &ComparableProbeResult, final_k: usize) -> Option<f64> {
    let expected = probe.expected_fact_set();
    if expected.is_empty() {
        return None;
    }
    let found = retrieved_expected_fact_ids(probe.final_candidates(), final_k, &expected);
    Some(found.len() as f64 / expected.len() as f64)
}

fn reciprocal_rank(probe: &ComparableProbeResult) -> Option<f64> {
    let expected = probe.expected_fact_set();
    if expected.is_empty() {
        return None;
    }
    probe
        .final_candidates()
        .iter()
        .filter(|candidate| candidate.rank > 0)
        .filter_map(|candidate| {
            candidate
                .fact_id
                .as_deref()
                .filter(|fact_id| expected.contains(*fact_id))
                .map(|_| candidate.rank)
        })
        .min()
        .map(|rank| 1.0 / rank as f64)
        .or(Some(0.0))
}

fn ndcg_at(probe: &ComparableProbeResult, final_k: usize) -> Option<f64> {
    let expected = probe.expected_fact_set();
    if expected.is_empty() {
        return None;
    }

    let mut seen = BTreeSet::new();
    let mut dcg = 0.0;
    for candidate in probe
        .final_candidates()
        .iter()
        .filter(|candidate| candidate.rank > 0 && candidate.rank <= final_k)
    {
        let Some(fact_id) = candidate.fact_id.as_deref() else {
            continue;
        };
        if expected.contains(fact_id) && seen.insert(fact_id.to_string()) {
            dcg += discount(candidate.rank);
        }
    }

    let ideal_hits = expected.len().min(final_k);
    let ideal_dcg = (1..=ideal_hits).map(discount).sum::<f64>();
    if ideal_dcg == 0.0 {
        Some(0.0)
    } else {
        Some(dcg / ideal_dcg)
    }
}

fn all_expected_found_at_k(probe: &ComparableProbeResult, final_k: usize) -> bool {
    let expected = probe.expected_fact_set();
    !expected.is_empty()
        && retrieved_expected_fact_ids(probe.final_candidates(), final_k, &expected).len()
            == expected.len()
}

fn retrieved_expected_fact_ids(
    candidates: &[ComparableCandidate],
    final_k: usize,
    expected: &BTreeSet<&str>,
) -> BTreeSet<String> {
    candidates
        .iter()
        .filter(|candidate| candidate.rank > 0 && candidate.rank <= final_k)
        .filter_map(|candidate| {
            candidate
                .fact_id
                .as_deref()
                .filter(|fact_id| expected.contains(*fact_id))
                .map(str::to_string)
        })
        .collect()
}

fn discount(rank: usize) -> f64 {
    1.0 / ((rank + 1) as f64).log2()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn compare_reports_exits_nonzero_on_corpus_mismatch() {
        // Pins: paired comparison refuses different corpora.
        let baseline = report_json("corpus-a", &[1], &["probe-a"], |_| Vec::new());
        let candidate = report_json("corpus-b", &[1], &["probe-a"], |_| Vec::new());

        let err = compare_eval_reports_with_config(&baseline, &candidate, test_bootstrap())
            .expect_err("corpus mismatch should fail");

        assert!(matches!(err, CompareReportsError::CorpusMismatch { .. }));
        assert!(err.is_pairing_error());
    }

    #[test]
    fn compare_reports_errors_on_asymmetric_probe_sets() {
        // Pins: paired comparison refuses reports with unpaired probes.
        let baseline = report_json("corpus-a", &[1], &["probe-a", "probe-b"], |_| Vec::new());
        let candidate = report_json("corpus-a", &[1], &["probe-a", "probe-c"], |_| Vec::new());

        let err = compare_eval_reports_with_config(&baseline, &candidate, test_bootstrap())
            .expect_err("probe set mismatch should fail");

        assert!(matches!(
            err,
            CompareReportsError::ProbeSetMismatch {
                missing_in_candidate,
                missing_in_baseline
            } if missing_in_candidate == ["probe-b"] && missing_in_baseline == ["probe-c"]
        ));
    }

    #[test]
    fn compare_reports_ignores_unknown_suite_fields() {
        // Pins: the kernel reads only the shared report subset.
        let baseline = report_json("corpus-a", &[1], &["probe-a"], |_| Vec::new());
        let mut candidate: serde_json::Value =
            serde_json::from_str(&baseline).expect("fixture should parse");
        candidate["unknown_suite_field"] = json!({"kept": true});
        candidate["probe_results"][0]["suite_probe_extra"] = json!("ignored");

        let comparison = compare_eval_reports_with_config(
            &baseline,
            &serde_json::to_string(&candidate).expect("fixture should serialize"),
            test_bootstrap(),
        )
        .expect("unknown fields should be ignored");

        assert_eq!(comparison.probes_paired, 1);
    }

    #[test]
    fn compare_reports_mcnemar_uses_discordant_pairs_only() {
        // Pins: McNemar counts only probes where the paired binary outcome differs.
        let probes = ["probe-a", "probe-b", "probe-c", "probe-d"];
        let baseline = report_json("corpus-a", &[1], &probes, |index| {
            if index < 2 {
                vec!["fact", "other"]
            } else {
                Vec::new()
            }
        });
        let candidate = report_json("corpus-a", &[1], &probes, |index| {
            if index == 0 || index == 2 {
                vec!["fact", "other"]
            } else {
                Vec::new()
            }
        });

        let comparison = compare_eval_reports_with_config(&baseline, &candidate, test_bootstrap())
            .expect("paired reports should compare");
        let recall = comparison
            .mcnemar
            .iter()
            .find(|metric| metric.metric_name == "recall_at_4")
            .expect("recall mcnemar should exist");

        assert_eq!(recall.both_successes, 1);
        assert_eq!(recall.both_failures, 1);
        assert_eq!(recall.control_only_successes, 1);
        assert_eq!(recall.treatment_only_successes, 1);
    }

    #[test]
    fn compare_reports_applies_benjamini_hochberg_across_metrics() {
        // Pins: p-values are adjusted across all reported metric rows.
        let probes = [
            "probe-a", "probe-b", "probe-c", "probe-d", "probe-e", "probe-f", "probe-g", "probe-h",
        ];
        let baseline = report_json("corpus-a", &[1], &probes, |_| Vec::new());
        let candidate = report_json("corpus-a", &[1], &probes, |index| {
            if index < 4 {
                vec!["fact", "other"]
            } else if index < 6 {
                vec!["fact"]
            } else {
                Vec::new()
            }
        });

        let comparison = compare_eval_reports_with_config(&baseline, &candidate, test_bootstrap())
            .expect("paired reports should compare");

        assert_eq!(comparison.mcnemar.len(), 3);
        assert!(
            comparison
                .mcnemar
                .iter()
                .all(|metric| metric.adjusted_p_value >= metric.p_value)
        );
    }

    fn report_json(
        corpus_id: &str,
        seeds: &[u64],
        probe_ids: &[&str],
        found_fact_ids: impl Fn(usize) -> Vec<&'static str>,
    ) -> String {
        let probe_results = probe_ids
            .iter()
            .enumerate()
            .map(|(index, probe_id)| {
                let candidates = found_fact_ids(index)
                    .into_iter()
                    .enumerate()
                    .map(|(rank, fact_id)| json!({"rank": rank + 1, "fact_id": fact_id}))
                    .collect::<Vec<_>>();
                json!({
                    "probe_id": probe_id,
                    "user_id": format!("user-{}", index % 2),
                    "expected_fact_ids": ["fact", "other"],
                    "candidates": candidates,
                    "post_rerank_candidates": candidates
                })
            })
            .collect::<Vec<_>>();
        let metrics = metrics_for(&probe_results);
        serde_json::to_string(&json!({
            "manifest": {
                "corpus_id": corpus_id,
                "seeds": seeds,
            },
            "final_k": 4,
            "metrics": metrics,
            "probe_results": probe_results,
        }))
        .expect("fixture should serialize")
    }

    fn metrics_for(probes: &[serde_json::Value]) -> serde_json::Value {
        let mut recall = 0.0;
        let mut mrr = 0.0;
        let mut ndcg = 0.0;
        for probe in probes {
            let parsed: ComparableProbeResult =
                serde_json::from_value(probe.clone()).expect("probe fixture should parse");
            recall += recall_at(&parsed, 4).expect("fixture has expected facts");
            mrr += reciprocal_rank(&parsed).expect("fixture has expected facts");
            ndcg += ndcg_at(&parsed, 4).expect("fixture has expected facts");
        }
        let denominator = probes.len();
        json!({
            "recall_at_4": summary(recall, denominator),
            "recall_at_25": summary(recall, denominator),
            "mrr": summary(mrr, denominator),
            "ndcg_at_4": summary(ndcg, denominator),
            "zero_recall_rate": summary(0.0, denominator),
            "per_leg_recall": {
                "graph": summary(0.0, denominator),
                "vector": summary(0.0, denominator),
                "lexical": summary(0.0, denominator)
            },
            "p95_retrieval_latency_ms": 0,
            "cross_user_leak_count": 0,
            "pii_unredacted_count": 0
        })
    }

    fn summary(numerator: f64, denominator: usize) -> serde_json::Value {
        json!({
            "numerator": numerator,
            "denominator": denominator,
            "value": if denominator == 0 { 0.0 } else { numerator / denominator as f64 }
        })
    }

    fn test_bootstrap() -> BootstrapConfig {
        BootstrapConfig {
            resamples: 64,
            seed: 7,
        }
    }
}
