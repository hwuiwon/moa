//! Memory retrieval eval report schema and assembly helpers.

use std::collections::BTreeMap;

use moa_retrieval::retrieval::{GraphCandidateCounts, GraphRetrievalPolicy};
use moa_memory_lifecycle::ConsolidationOutcome;
use serde::{Deserialize, Serialize};

use super::rewrite::QueryRewriteSummary;
use super::{
    GraphExpansionEvalPolicy, QueryRewritePolicy, RETRIEVAL_EVAL_CANDIDATE_K,
    RETRIEVAL_EVAL_FINAL_K,
};
use crate::kernel::{CostLedger, ProviderProvenance};
use crate::memory_eval::{
    BootstrapConfig, ClusterBootstrapReport, CorpusManifest, EntityFragmentationCounts,
    ExtractionPrecisionCounts, GoldResolutionReport, GraphImpact, ProbeResult, RetrievalMetrics,
};

/// Retrieval-only JSON report written by `run-memory-retrieval-eval`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryRetrievalEvalReport {
    /// Corpus manifest loaded for this run.
    pub manifest: CorpusManifest,
    /// Number of candidates requested from production retrieval for metric scoring.
    pub candidate_k: usize,
    /// Final answer-context cutoff used by recall@4 and nDCG@4 metrics.
    pub final_k: usize,
    /// Whether the eval collected a post-rerank top-4 retrieval pass.
    #[serde(default)]
    pub reranker_enabled: bool,
    /// Whether probes retrieved through the production stage-7 evidence seam
    /// (router, admission, cross-scope merge, and evidence-budget packing).
    #[serde(default, skip_serializing_if = "is_false")]
    pub parity: bool,
    /// Query rewrite policy used by this run.
    #[serde(default)]
    pub query_rewrite_policy: QueryRewritePolicy,
    /// Eval-only graph expansion policy used by this run.
    #[serde(default)]
    pub graph_expansion_policy: GraphExpansionEvalPolicy,
    /// Effective graph retrieval policy selected by the run.
    #[serde(default, skip_serializing_if = "is_default_graph_retrieval_policy")]
    pub graph_retrieval_policy: GraphRetrievalPolicy,
    /// Cheap graph diagnostics derived from eval retrieval candidates.
    #[serde(default, skip_serializing_if = "is_default_memory_graph_diagnostics")]
    pub graph_diagnostics: MemoryGraphDiagnostics,
    /// Number of probes whose retrieval query came from a rewrite fixture.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub query_rewrite_call_count: usize,
    /// Number of probes that used the original query.
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub query_rewrite_skip_count: usize,
    /// Fraction of probes that used a rewrite fixture.
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub query_rewrite_call_rate: f64,
    /// PR-lane deterministic p50 rewrite latency.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub query_rewrite_p50_latency_ms: u64,
    /// PR-lane deterministic p95 rewrite latency.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub query_rewrite_p95_latency_ms: u64,
    /// Estimated rewrite input tokens.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub query_rewrite_input_tokens: u64,
    /// Estimated rewrite output tokens.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub query_rewrite_output_tokens: u64,
    /// Estimated rewrite cost in USD.
    #[serde(default, skip_serializing_if = "is_zero_f64")]
    pub query_rewrite_est_usd: f64,
    /// p95 latency with rewrite latency included.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub retrieval_plus_rewrite_p95_latency_ms: u64,
    /// Query rewrite accounting grouped by deterministic query class.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query_rewrite_by_class: BTreeMap<String, QueryRewriteClassMetrics>,
    /// Whether the runner stopped after crossing the configured live-lane budget.
    #[serde(default, skip_serializing_if = "is_false")]
    pub aborted_over_budget: bool,
    /// Optional provider cost ledger for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<CostLedger>,
    /// Optional provider provenance for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<ProviderProvenance>,
    /// Aggregated retrieval and storage metrics; generated-answer quality is out of scope.
    pub metrics: RetrievalMetrics,
    /// Per-probe observed retrieval/storage results with candidate attribution.
    pub probe_results: Vec<ProbeResult>,
    /// Cluster-bootstrap confidence intervals by user.
    pub bootstrap: Vec<ClusterBootstrapReport>,
    /// Probe ids that retrieved blocked facts.
    pub cross_user_leak_probe_ids: Vec<String>,
    /// Gold-resolution ingestion and fact-to-node mapping details.
    pub gold_resolution: GoldResolutionReport,
    /// Optional consolidation outcome collected after gold resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidation: Option<ConsolidationOutcome>,
}

/// Query rewrite accounting for one deterministic query class.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryRewriteClassMetrics {
    /// Total probes in this class.
    pub total_count: usize,
    /// Probes rewritten in this class.
    pub call_count: usize,
    /// Probes skipped in this class.
    pub skip_count: usize,
    /// Fraction rewritten in this class.
    pub call_rate: f64,
}

/// Graph diagnostics included in memory-retrieval eval reports.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryGraphDiagnostics {
    /// Candidate counts before optional reranking.
    pub pre_rerank_candidate_counts: GraphCandidateCounts,
    /// Candidate counts after optional reranking or top-k truncation.
    pub post_rerank_candidate_counts: GraphCandidateCounts,
    /// Probes whose pre-rerank candidate set included at least one graph candidate.
    pub probes_with_graph_candidates: usize,
    /// Raw graph path count returned across primary probe retrievals.
    pub raw_path_count: usize,
    /// Edge-label histogram across primary probe graph paths.
    pub path_label_histogram: BTreeMap<String, usize>,
    /// Hop-count histogram across primary probe graph paths.
    pub hop_histogram: BTreeMap<u8, usize>,
    /// Probes with graph-on/off comparison results.
    pub compared_probe_count: usize,
    /// Probes where graph worsened first relevant rank.
    pub graph_hurt_count: usize,
    /// Probes where graph improved first relevant rank.
    pub graph_rescue_count: usize,
    /// Probes where graph left first relevant rank unchanged.
    pub graph_neutral_count: usize,
}

impl MemoryGraphDiagnostics {
    /// Aggregates graph diagnostics from per-probe memory eval results.
    #[must_use]
    pub fn from_probe_results(probe_results: &[ProbeResult]) -> Self {
        let mut diagnostics = Self::default();
        for probe in probe_results {
            let pre = graph_candidate_counts_from_candidates(&probe.candidates);
            if pre.graph_only + pre.vector_graph + pre.lexical_graph + pre.all_legs > 0 {
                diagnostics.probes_with_graph_candidates += 1;
            }
            diagnostics.pre_rerank_candidate_counts.add(pre);
            let post_candidates = probe
                .post_rerank_candidates
                .as_deref()
                .unwrap_or(probe.candidates.as_slice());
            diagnostics
                .post_rerank_candidate_counts
                .add(graph_candidate_counts_from_candidates(post_candidates));
            if let Some(graph) = &probe.graph_diagnostics {
                diagnostics.raw_path_count += graph.raw_path_count;
                for (label, count) in &graph.path_label_histogram {
                    *diagnostics
                        .path_label_histogram
                        .entry(label.clone())
                        .or_default() += count;
                }
                for (hop, count) in &graph.hop_histogram {
                    *diagnostics.hop_histogram.entry(*hop).or_default() += count;
                }
            }
            if let Some(comparison) = &probe.graph_comparison {
                diagnostics.compared_probe_count += 1;
                match comparison.impact {
                    GraphImpact::Hurt => diagnostics.graph_hurt_count += 1,
                    GraphImpact::Rescue => diagnostics.graph_rescue_count += 1,
                    GraphImpact::Neutral => diagnostics.graph_neutral_count += 1,
                }
            }
        }
        diagnostics
    }
}

pub(super) struct ReportBuildInput {
    pub(super) manifest: CorpusManifest,
    pub(super) gold_resolution: GoldResolutionReport,
    pub(super) probe_results: Vec<ProbeResult>,
    pub(super) bootstrap_config: BootstrapConfig,
    pub(super) extraction_precision: ExtractionPrecisionCounts,
    pub(super) entity_fragmentation: EntityFragmentationCounts,
    pub(super) reranker_enabled: bool,
    pub(super) parity: bool,
    pub(super) rewrite_summary: QueryRewriteSummary,
    pub(super) graph_expansion_policy: GraphExpansionEvalPolicy,
    pub(super) aborted_over_budget: bool,
    pub(super) cost: Option<CostLedger>,
    pub(super) providers: Option<ProviderProvenance>,
    pub(super) consolidation: Option<ConsolidationOutcome>,
}

pub(super) fn build_eval_report(input: ReportBuildInput) -> MemoryRetrievalEvalReport {
    let rewrite_p50_latency_ms = deterministic_rewrite_latency_ms(input.rewrite_summary.call_count);
    let rewrite_p95_latency_ms = deterministic_rewrite_latency_ms(input.rewrite_summary.call_count);
    let retrieval = crate::memory_eval::aggregate_retrieval_eval_with_diagnostics(
        &input.gold_resolution,
        input.probe_results,
        input.bootstrap_config,
        input.extraction_precision,
        input.entity_fragmentation,
    );
    let retrieval_plus_rewrite_p95_latency_ms = retrieval
        .metrics
        .p95_retrieval_latency_ms
        .saturating_add(rewrite_p95_latency_ms);
    let graph_diagnostics = MemoryGraphDiagnostics::from_probe_results(&retrieval.probe_results);
    MemoryRetrievalEvalReport {
        manifest: input.manifest,
        candidate_k: RETRIEVAL_EVAL_CANDIDATE_K,
        final_k: RETRIEVAL_EVAL_FINAL_K,
        reranker_enabled: input.reranker_enabled,
        parity: input.parity,
        query_rewrite_policy: input.rewrite_summary.policy,
        graph_expansion_policy: input.graph_expansion_policy,
        graph_retrieval_policy: input.graph_expansion_policy.graph_retrieval_policy(),
        graph_diagnostics,
        query_rewrite_call_count: input.rewrite_summary.call_count,
        query_rewrite_skip_count: input.rewrite_summary.skip_count,
        query_rewrite_call_rate: input.rewrite_summary.call_rate(),
        query_rewrite_p50_latency_ms: rewrite_p50_latency_ms,
        query_rewrite_p95_latency_ms: rewrite_p95_latency_ms,
        query_rewrite_input_tokens: input.rewrite_summary.input_tokens,
        query_rewrite_output_tokens: input.rewrite_summary.output_tokens,
        query_rewrite_est_usd: 0.0,
        retrieval_plus_rewrite_p95_latency_ms,
        query_rewrite_by_class: input.rewrite_summary.by_class,
        aborted_over_budget: input.aborted_over_budget,
        cost: input.cost,
        providers: input.providers,
        metrics: retrieval.metrics,
        probe_results: retrieval.probe_results,
        bootstrap: retrieval.bootstrap,
        cross_user_leak_probe_ids: retrieval.cross_user_leak_probe_ids,
        gold_resolution: input.gold_resolution,
        consolidation: input.consolidation,
    }
}

fn graph_candidate_counts_from_candidates(
    candidates: &[crate::memory_eval::RetrievedCandidate],
) -> GraphCandidateCounts {
    let mut counts = GraphCandidateCounts::default();
    for candidate in candidates {
        match (
            candidate.legs.graph,
            candidate.legs.vector,
            candidate.legs.lexical,
        ) {
            (true, false, false) => counts.graph_only += 1,
            (true, true, false) => counts.vector_graph += 1,
            (true, false, true) => counts.lexical_graph += 1,
            (true, true, true) => counts.all_legs += 1,
            _ => {}
        }
    }
    counts
}

fn deterministic_rewrite_latency_ms(call_count: usize) -> u64 {
    if call_count == 0 { 0 } else { 1 }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_zero_f64(value: &f64) -> bool {
    *value == 0.0
}

fn is_default_graph_retrieval_policy(value: &GraphRetrievalPolicy) -> bool {
    *value == GraphRetrievalPolicy::default()
}

fn is_default_memory_graph_diagnostics(value: &MemoryGraphDiagnostics) -> bool {
    *value == MemoryGraphDiagnostics::default()
}
