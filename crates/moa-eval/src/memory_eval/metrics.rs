//! Retrieval metric aggregation for memory-evaluation reports.

use std::collections::{BTreeSet, HashMap};
use std::ops::{Deref, DerefMut};

use moa_brain::retrieval::{LegSources, RetrievalHit};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::kernel::{
    BootstrapConfig, ClusterBootstrapReport, ClusterObservation, MetricSummary, PerLegRecall,
    RetrievalCoreMetrics, cluster_bootstrap_mean_by_user,
};

use super::ProbeType;
use super::gold::{GoldResolutionReport, GoldResolutionStatus};

const RECALL_AT_4: usize = 4;
const RECALL_AT_25: usize = 25;

/// Serializable retrieval-leg flags copied from `RetrievalHit.legs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateLegs {
    /// Candidate came from graph traversal.
    pub graph: bool,
    /// Candidate came from vector KNN.
    pub vector: bool,
    /// Candidate came from lexical search.
    pub lexical: bool,
}

impl From<LegSources> for CandidateLegs {
    fn from(value: LegSources) -> Self {
        Self {
            graph: value.graph,
            vector: value.vector,
            lexical: value.lexical,
        }
    }
}

/// One serializable retrieval candidate used for metric scoring.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievedCandidate {
    /// Stable graph node uid.
    pub uid: Uuid,
    /// Rank in the retrieval result list, starting at one.
    pub rank: usize,
    /// Retrieval score assigned by fusion or reranking.
    pub score: f64,
    /// Ledger fact resolved to this candidate, when known.
    pub fact_id: Option<String>,
    /// Retrieval legs that contributed this candidate.
    pub legs: CandidateLegs,
}

/// Per-probe retrieval and answer-evaluation outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Stable probe identifier.
    pub probe_id: String,
    /// User that issued the probe; used as the bootstrap cluster.
    pub user_id: String,
    /// Probe behavior class.
    pub probe_type: ProbeType,
    /// Gold facts that should be retrieved for this probe.
    #[serde(default)]
    pub expected_fact_ids: Vec<String>,
    /// Facts that must not be returned for this probe.
    #[serde(default)]
    pub blocked_fact_ids: Vec<String>,
    /// Ranked retrieval candidates.
    #[serde(default)]
    pub candidates: Vec<RetrievedCandidate>,
    /// Final top-k window after reranking, when the eval collected a post-rerank pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_rerank_candidates: Option<Vec<RetrievedCandidate>>,
    /// End-to-end retrieval latency observed for this probe.
    #[serde(default)]
    pub retrieval_latency_ms: u64,
    /// Whether the answer was faithful to gold evidence, when judged.
    pub answer_faithful: Option<bool>,
    /// Whether abstention behavior was correct, when applicable.
    pub abstention_correct: Option<bool>,
    /// Whether PII-bearing answer material was redacted, when applicable.
    pub pii_redacted: Option<bool>,
    /// Whether the returned answer matched the requested valid-time instant.
    pub temporal_as_of_correct: Option<bool>,
    /// Whether the planner parsed a temporal filter from this probe's query.
    #[serde(default)]
    pub temporal_filter_parsed: Option<bool>,
    /// Whether the parsed temporal filter matched the probe's encoded `as_of` instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal_filter_matches_as_of: Option<bool>,
}

impl ProbeResult {
    /// Computes final-window recall@k for this probe, or `None` when no facts are expected.
    #[must_use]
    pub fn recall_at(&self, k: usize) -> Option<f64> {
        self.post_rerank_recall_at(k)
    }

    /// Computes pre-rerank recall@k for this probe, or `None` when no facts are expected.
    #[must_use]
    pub fn pre_rerank_recall_at(&self, k: usize) -> Option<f64> {
        let expected = self.expected_fact_set();
        if expected.is_empty() {
            return None;
        }
        let retrieved = retrieved_expected_fact_ids(&self.candidates, k, &expected, |_| true);
        Some(retrieved.len() as f64 / expected.len() as f64)
    }

    /// Computes post-rerank recall@k for this probe, or `None` when no facts are expected.
    #[must_use]
    pub fn post_rerank_recall_at(&self, k: usize) -> Option<f64> {
        let expected = self.expected_fact_set();
        if expected.is_empty() {
            return None;
        }
        let retrieved =
            retrieved_expected_fact_ids(self.final_candidates(), k, &expected, |_| true);
        Some(retrieved.len() as f64 / expected.len() as f64)
    }

    /// Computes reciprocal rank for the first expected candidate.
    #[must_use]
    pub fn reciprocal_rank(&self) -> Option<f64> {
        let expected = self.expected_fact_set();
        if expected.is_empty() {
            return None;
        }
        self.final_candidates()
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

    /// Computes binary-relevance nDCG@k for this probe.
    #[must_use]
    pub fn ndcg_at(&self, k: usize) -> Option<f64> {
        let expected = self.expected_fact_set();
        if expected.is_empty() {
            return None;
        }

        let mut seen = BTreeSet::new();
        let mut dcg = 0.0;
        for candidate in self
            .final_candidates()
            .iter()
            .filter(|candidate| candidate.rank > 0 && candidate.rank <= k)
        {
            let Some(fact_id) = candidate.fact_id.as_deref() else {
                continue;
            };
            if expected.contains(fact_id) && seen.insert(fact_id.to_string()) {
                dcg += discount(candidate.rank);
            }
        }

        let ideal_hits = expected.len().min(k);
        let ideal_dcg = (1..=ideal_hits).map(discount).sum::<f64>();
        if ideal_dcg == 0.0 {
            Some(0.0)
        } else {
            Some(dcg / ideal_dcg)
        }
    }

    /// Returns whether this probe has no expected recall within the top-25 candidates.
    #[must_use]
    pub fn zero_recall(&self) -> Option<bool> {
        self.pre_rerank_recall_at(RECALL_AT_25)
            .map(|recall| recall == 0.0)
    }

    /// Returns unique blocked fact identifiers retrieved within the top-25 candidates.
    #[must_use]
    pub fn leaked_blocked_fact_ids(&self) -> Vec<String> {
        let blocked = self
            .blocked_fact_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if blocked.is_empty() {
            return Vec::new();
        }

        let mut leaked = BTreeSet::new();
        for candidate in self
            .candidates
            .iter()
            .filter(|candidate| candidate.rank > 0 && candidate.rank <= RECALL_AT_25)
        {
            if let Some(fact_id) = candidate
                .fact_id
                .as_deref()
                .filter(|fact_id| blocked.contains(*fact_id))
            {
                leaked.insert(fact_id.to_string());
            }
        }
        leaked.into_iter().collect()
    }

    fn expected_fact_set(&self) -> BTreeSet<&str> {
        self.expected_fact_ids.iter().map(String::as_str).collect()
    }

    fn final_candidates(&self) -> &[RetrievedCandidate] {
        self.post_rerank_candidates
            .as_deref()
            .unwrap_or(&self.candidates)
    }
}

/// Aggregated memory-retrieval metrics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalMetrics {
    /// Suite-agnostic retrieval metrics serialized flat for report compatibility.
    #[serde(flatten)]
    pub core: RetrievalCoreMetrics,
    /// Fraction of ledger facts resolved to graph nodes during ingestion.
    pub ingestion_coverage: MetricSummary,
    /// Mean pre-rerank recall@4 over probes with expected facts.
    #[serde(default)]
    pub pre_rerank_recall_at_4: MetricSummary,
    /// Mean pre-rerank recall@25 over probes with expected facts.
    #[serde(default)]
    pub pre_rerank_recall_at_25: MetricSummary,
    /// Mean post-rerank recall@4 over probes with expected facts.
    #[serde(default)]
    pub post_rerank_recall_at_4: MetricSummary,
    /// Fraction of judged answers that were faithful.
    pub answer_faithfulness: MetricSummary,
    /// Fraction of abstention-relevant probes that abstained correctly.
    pub abstention_correctness: MetricSummary,
    /// Fraction of PII-relevant probes with redacted answer material.
    pub pii_redaction_rate: MetricSummary,
    /// Fraction of temporal probes that answered for the requested valid-time instant.
    pub temporal_as_of_accuracy: MetricSummary,
    /// Fraction of temporal probes whose query text produced an absolute temporal filter.
    #[serde(default)]
    pub temporal_parse_rate: MetricSummary,
    /// Number of temporal probes where the parser fired but produced the wrong instant.
    #[serde(default)]
    pub temporal_parse_mismatch_count: usize,
}

impl Deref for RetrievalMetrics {
    type Target = RetrievalCoreMetrics;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl DerefMut for RetrievalMetrics {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

/// Full retrieval-evaluation report suitable for JSON output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalEvalReport {
    /// Aggregated retrieval metrics.
    pub metrics: RetrievalMetrics,
    /// Per-probe retrieval and answer outcomes.
    pub probe_results: Vec<ProbeResult>,
    /// Cluster-bootstrap confidence intervals by metric.
    pub bootstrap: Vec<ClusterBootstrapReport>,
    /// Probe ids that retrieved blocked facts.
    pub cross_user_leak_probe_ids: Vec<String>,
}

/// Converts production retrieval hits into serializable candidates with fact ids.
#[must_use]
pub fn candidates_from_retrieval_hits(
    hits: &[RetrievalHit],
    fact_ids_by_uid: &HashMap<Uuid, String>,
) -> Vec<RetrievedCandidate> {
    hits.iter()
        .enumerate()
        .map(|(index, hit)| RetrievedCandidate {
            uid: hit.uid,
            rank: index + 1,
            score: hit.score,
            fact_id: fact_ids_by_uid.get(&hit.uid).cloned(),
            legs: CandidateLegs::from(hit.legs),
        })
        .collect()
}

/// Aggregates retrieval metrics using gold-resolution ingestion coverage.
#[must_use]
pub fn aggregate_retrieval_eval(
    gold_resolution: &GoldResolutionReport,
    probe_results: Vec<ProbeResult>,
    bootstrap_config: BootstrapConfig,
) -> RetrievalEvalReport {
    let resolved = gold_resolution
        .records
        .iter()
        .filter(|record| record.resolution_status != GoldResolutionStatus::Unresolved)
        .count();
    aggregate_retrieval_eval_from_counts(
        resolved,
        gold_resolution.records.len(),
        probe_results,
        bootstrap_config,
    )
}

/// Aggregates retrieval metrics from explicit ingestion coverage counts.
#[must_use]
pub fn aggregate_retrieval_eval_from_counts(
    resolved_facts: usize,
    total_facts: usize,
    probe_results: Vec<ProbeResult>,
    bootstrap_config: BootstrapConfig,
) -> RetrievalEvalReport {
    let metrics = aggregate_metrics(resolved_facts, total_facts, &probe_results);
    let bootstrap = bootstrap_reports(&probe_results, bootstrap_config);
    let cross_user_leak_probe_ids = cross_user_leak_probe_ids(&probe_results);

    RetrievalEvalReport {
        metrics,
        probe_results,
        bootstrap,
        cross_user_leak_probe_ids,
    }
}

fn aggregate_metrics(
    resolved_facts: usize,
    total_facts: usize,
    probe_results: &[ProbeResult],
) -> RetrievalMetrics {
    let pre_rerank_recall_at_4 = summarize_probe_values(probe_results, |probe| {
        probe.pre_rerank_recall_at(RECALL_AT_4)
    });
    let pre_rerank_recall_at_25 = summarize_probe_values(probe_results, |probe| {
        probe.pre_rerank_recall_at(RECALL_AT_25)
    });
    let post_rerank_recall_at_4 = summarize_probe_values(probe_results, |probe| {
        probe.post_rerank_recall_at(RECALL_AT_4)
    });

    RetrievalMetrics {
        core: RetrievalCoreMetrics {
            recall_at_4: post_rerank_recall_at_4,
            recall_at_25: pre_rerank_recall_at_25,
            mrr: summarize_probe_values(probe_results, ProbeResult::reciprocal_rank),
            ndcg_at_4: summarize_probe_values(probe_results, |probe| probe.ndcg_at(RECALL_AT_4)),
            zero_recall_rate: summarize_probe_values(probe_results, |probe| {
                probe.zero_recall().map(bool_value)
            }),
            per_leg_recall: per_leg_recall(probe_results),
            p50_retrieval_latency_ms: p50_retrieval_latency_ms(probe_results),
            p95_retrieval_latency_ms: p95_retrieval_latency_ms(probe_results),
            cross_user_leak_count: cross_user_leak_count(probe_results),
            pii_unredacted_count: pii_unredacted_count(probe_results),
        },
        ingestion_coverage: MetricSummary::from_counts(resolved_facts, total_facts),
        pre_rerank_recall_at_4,
        pre_rerank_recall_at_25,
        post_rerank_recall_at_4,
        answer_faithfulness: summarize_probe_values(probe_results, |probe| {
            probe.answer_faithful.map(bool_value)
        }),
        abstention_correctness: summarize_probe_values(probe_results, |probe| {
            probe.abstention_correct.map(bool_value)
        }),
        pii_redaction_rate: summarize_probe_values(probe_results, |probe| {
            probe.pii_redacted.map(bool_value)
        }),
        temporal_as_of_accuracy: summarize_probe_values(probe_results, |probe| {
            probe.temporal_as_of_correct.map(bool_value)
        }),
        temporal_parse_rate: temporal_parse_rate(probe_results),
        temporal_parse_mismatch_count: temporal_parse_mismatch_count(probe_results),
    }
}

fn retrieved_expected_fact_ids(
    candidates: &[RetrievedCandidate],
    k: usize,
    expected: &BTreeSet<&str>,
    leg_filter: impl Fn(CandidateLegs) -> bool,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.rank > 0 && candidate.rank <= k)
    {
        if !leg_filter(candidate.legs) {
            continue;
        }
        if let Some(fact_id) = candidate
            .fact_id
            .as_deref()
            .filter(|fact_id| expected.contains(*fact_id))
        {
            found.insert(fact_id.to_string());
        }
    }
    found
}

fn cross_user_leak_count(probe_results: &[ProbeResult]) -> usize {
    probe_results
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::CrossUserIsolation)
        .map(|probe| probe.leaked_blocked_fact_ids().len())
        .sum()
}

fn cross_user_leak_probe_ids(probe_results: &[ProbeResult]) -> Vec<String> {
    probe_results
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::CrossUserIsolation)
        .filter(|probe| !probe.leaked_blocked_fact_ids().is_empty())
        .map(|probe| probe.probe_id.clone())
        .collect()
}

fn pii_unredacted_count(probe_results: &[ProbeResult]) -> usize {
    probe_results
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::PiiRedaction)
        .filter(|probe| probe.pii_redacted == Some(false))
        .count()
}

fn temporal_parse_rate(probe_results: &[ProbeResult]) -> MetricSummary {
    let temporal_probes = probe_results
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::TemporalAsOf)
        .collect::<Vec<_>>();
    let parsed = temporal_probes
        .iter()
        .filter(|probe| probe.temporal_filter_parsed == Some(true))
        .count();
    MetricSummary::from_counts(parsed, temporal_probes.len())
}

fn temporal_parse_mismatch_count(probe_results: &[ProbeResult]) -> usize {
    probe_results
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::TemporalAsOf)
        .filter(|probe| probe.temporal_filter_parsed == Some(true))
        .filter(|probe| probe.temporal_filter_matches_as_of == Some(false))
        .count()
}

fn summarize_probe_values(
    probe_results: &[ProbeResult],
    value_for_probe: impl Fn(&ProbeResult) -> Option<f64>,
) -> MetricSummary {
    let mut total = 0.0;
    let mut count = 0_usize;
    for value in probe_results.iter().filter_map(value_for_probe) {
        total += value;
        count += 1;
    }
    MetricSummary::from_total(total, count)
}

fn per_leg_recall(probe_results: &[ProbeResult]) -> PerLegRecall {
    let mut expected_fact_count = 0_usize;
    let mut graph = 0_usize;
    let mut vector = 0_usize;
    let mut lexical = 0_usize;

    for probe in probe_results {
        let expected = probe.expected_fact_set();
        if expected.is_empty() {
            continue;
        }
        expected_fact_count += expected.len();
        graph += retrieved_expected_fact_ids(&probe.candidates, RECALL_AT_25, &expected, |legs| {
            legs.graph
        })
        .len();
        vector += retrieved_expected_fact_ids(&probe.candidates, RECALL_AT_25, &expected, |legs| {
            legs.vector
        })
        .len();
        lexical +=
            retrieved_expected_fact_ids(&probe.candidates, RECALL_AT_25, &expected, |legs| {
                legs.lexical
            })
            .len();
    }

    PerLegRecall {
        graph: MetricSummary::from_counts(graph, expected_fact_count),
        vector: MetricSummary::from_counts(vector, expected_fact_count),
        lexical: MetricSummary::from_counts(lexical, expected_fact_count),
    }
}

fn bootstrap_reports(
    probe_results: &[ProbeResult],
    bootstrap_config: BootstrapConfig,
) -> Vec<ClusterBootstrapReport> {
    [
        (
            "retrieval.recall_at_4",
            observation_recall_at_4 as fn(&ProbeResult) -> Option<f64>,
        ),
        ("retrieval.recall_at_25", observation_recall_at_25),
        ("retrieval.mrr", observation_mrr),
        ("retrieval.ndcg_at_4", observation_ndcg_at_4),
    ]
    .into_iter()
    .map(|(metric_name, value_for_probe)| {
        let observations = observations_for(probe_results, value_for_probe);
        cluster_bootstrap_mean_by_user(metric_name, &observations, bootstrap_config)
    })
    .collect()
}

fn observations_for(
    probe_results: &[ProbeResult],
    value_for_probe: fn(&ProbeResult) -> Option<f64>,
) -> Vec<ClusterObservation> {
    probe_results
        .iter()
        .filter_map(|probe| {
            value_for_probe(probe).map(|value| ClusterObservation {
                user_id: probe.user_id.clone(),
                probe_id: probe.probe_id.clone(),
                value,
            })
        })
        .collect()
}

fn observation_recall_at_4(probe: &ProbeResult) -> Option<f64> {
    probe.post_rerank_recall_at(RECALL_AT_4)
}

fn observation_recall_at_25(probe: &ProbeResult) -> Option<f64> {
    probe.pre_rerank_recall_at(RECALL_AT_25)
}

fn observation_mrr(probe: &ProbeResult) -> Option<f64> {
    probe.reciprocal_rank()
}

fn observation_ndcg_at_4(probe: &ProbeResult) -> Option<f64> {
    probe.ndcg_at(RECALL_AT_4)
}

fn bool_value(value: bool) -> f64 {
    if value { 1.0 } else { 0.0 }
}

fn discount(rank: usize) -> f64 {
    1.0 / ((rank + 1) as f64).log2()
}

fn p95_retrieval_latency_ms(probe_results: &[ProbeResult]) -> u64 {
    percentile_retrieval_latency_ms(probe_results, 0.95)
}

fn p50_retrieval_latency_ms(probe_results: &[ProbeResult]) -> u64 {
    percentile_retrieval_latency_ms(probe_results, 0.50)
}

fn percentile_retrieval_latency_ms(probe_results: &[ProbeResult], percentile: f64) -> u64 {
    let mut values = probe_results
        .iter()
        .map(|probe| probe.retrieval_latency_ms)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let rank = (values.len() as f64 * percentile).ceil() as usize;
    values[rank.saturating_sub(1).min(values.len() - 1)]
}
