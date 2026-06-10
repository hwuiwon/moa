//! Suite-agnostic retrieval metrics shared by eval reports.

use serde::{Deserialize, Serialize};

/// Mean metric value with numerator and denominator retained for reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricSummary {
    /// Aggregated numerator before normalization.
    pub numerator: f64,
    /// Number of items in the denominator.
    pub denominator: usize,
    /// Normalized metric value.
    pub value: f64,
}

impl MetricSummary {
    /// Builds a ratio summary from count-like values.
    #[must_use]
    pub fn from_counts(numerator: usize, denominator: usize) -> Self {
        Self::from_total(numerator as f64, denominator)
    }

    /// Builds a mean summary from a total and denominator.
    #[must_use]
    pub fn from_total(numerator: f64, denominator: usize) -> Self {
        Self {
            numerator,
            denominator,
            value: if denominator == 0 {
                0.0
            } else {
                numerator / denominator as f64
            },
        }
    }
}

/// Per-leg recall summaries for graph, vector, and lexical retrieval.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PerLegRecall {
    /// Recall for facts reached by the graph leg.
    pub graph: MetricSummary,
    /// Recall for facts reached by the vector leg.
    pub vector: MetricSummary,
    /// Recall for facts reached by the lexical leg.
    pub lexical: MetricSummary,
}

/// Suite-agnostic metrics every retrieval suite reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalCoreMetrics {
    /// Mean final-window recall@4 over probes with expected facts.
    pub recall_at_4: MetricSummary,
    /// Mean pre-rerank recall@25 over probes with expected facts.
    pub recall_at_25: MetricSummary,
    /// Mean reciprocal rank over probes with expected facts.
    pub mrr: MetricSummary,
    /// Mean binary-relevance nDCG@4 over probes with expected facts.
    pub ndcg_at_4: MetricSummary,
    /// Fraction of probes with expected facts that retrieved none in the top 25.
    pub zero_recall_rate: MetricSummary,
    /// Recall by contributing retrieval leg.
    pub per_leg_recall: PerLegRecall,
    /// p50 end-to-end retrieval latency in milliseconds.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub p50_retrieval_latency_ms: u64,
    /// p95 end-to-end retrieval latency in milliseconds.
    #[serde(default)]
    pub p95_retrieval_latency_ms: u64,
    /// Number of blocked cross-user facts retrieved.
    pub cross_user_leak_count: usize,
    /// Number of PII-redaction probes that returned unredacted material.
    #[serde(default)]
    pub pii_unredacted_count: usize,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

impl Default for RetrievalCoreMetrics {
    fn default() -> Self {
        Self {
            recall_at_4: MetricSummary::default(),
            recall_at_25: MetricSummary::default(),
            mrr: MetricSummary::default(),
            ndcg_at_4: MetricSummary::default(),
            zero_recall_rate: MetricSummary::default(),
            per_leg_recall: PerLegRecall {
                graph: MetricSummary::default(),
                vector: MetricSummary::default(),
                lexical: MetricSummary::default(),
            },
            p50_retrieval_latency_ms: 0,
            p95_retrieval_latency_ms: 0,
            cross_user_leak_count: 0,
            pii_unredacted_count: 0,
        }
    }
}
