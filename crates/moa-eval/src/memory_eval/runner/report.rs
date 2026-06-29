//! Memory retrieval eval report schema and assembly helpers.

use std::collections::BTreeMap;

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
    ExtractionPrecisionCounts, GoldResolutionReport, ProbeResult, RetrievalMetrics,
};

/// JSON report written by `run-memory-retrieval-eval`.
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
    /// Query rewrite policy used by this run.
    #[serde(default)]
    pub query_rewrite_policy: QueryRewritePolicy,
    /// Eval-only graph expansion policy used by this run.
    #[serde(default)]
    pub graph_expansion_policy: GraphExpansionEvalPolicy,
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
    /// Aggregated retrieval metrics.
    pub metrics: RetrievalMetrics,
    /// Per-probe retrieval results with candidate attribution.
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

pub(super) struct ReportBuildInput {
    pub(super) manifest: CorpusManifest,
    pub(super) gold_resolution: GoldResolutionReport,
    pub(super) probe_results: Vec<ProbeResult>,
    pub(super) bootstrap_config: BootstrapConfig,
    pub(super) extraction_precision: ExtractionPrecisionCounts,
    pub(super) entity_fragmentation: EntityFragmentationCounts,
    pub(super) reranker_enabled: bool,
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
    MemoryRetrievalEvalReport {
        manifest: input.manifest,
        candidate_k: RETRIEVAL_EVAL_CANDIDATE_K,
        final_k: RETRIEVAL_EVAL_FINAL_K,
        reranker_enabled: input.reranker_enabled,
        query_rewrite_policy: input.rewrite_summary.policy,
        graph_expansion_policy: input.graph_expansion_policy,
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
