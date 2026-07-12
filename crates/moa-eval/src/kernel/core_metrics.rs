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

    /// Returns whether this summary carries no observations at all, so report
    /// serialization can omit metrics a run never measured (`skip_serializing_if`)
    /// while keeping older checked-in baseline reports byte-for-byte round-trippable.
    #[must_use]
    pub fn is_empty(value: &Self) -> bool {
        *value == Self::default()
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

/// Recall summaries split by lexical backend.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PerLexicalBackendRecall {
    /// Recall for facts reached by Postgres `tsvector` lexical search.
    pub postgres_tsvector: MetricSummary,
    /// Recall for facts reached by Turbopuffer BM25 lexical search.
    pub turbopuffer_bm25: MetricSummary,
    /// Recall for facts reached by a mixed BM25 and Postgres lexical route.
    pub mixed: MetricSummary,
}

impl PerLexicalBackendRecall {
    fn is_empty(value: &Self) -> bool {
        *value == Self::default()
    }
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
    /// Recall by lexical backend for candidates with lexical attribution.
    #[serde(default, skip_serializing_if = "PerLexicalBackendRecall::is_empty")]
    pub per_lexical_backend_recall: PerLexicalBackendRecall,
    /// p50 end-to-end retrieval latency in milliseconds.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub p50_retrieval_latency_ms: u64,
    /// p95 end-to-end retrieval latency in milliseconds.
    #[serde(default)]
    pub p95_retrieval_latency_ms: u64,
    /// Number of blocked cross-user facts retrieved.
    pub cross_user_leak_count: usize,
    /// Fraction of update probes whose top candidates leaked a superseded fact.
    ///
    /// This is the staleness rate: over `latest_value_after_update` probes, how
    /// often the closed old value is still retrieved alongside or above its
    /// replacement. Rising values mean memory is going stale in retrieval.
    #[serde(default, skip_serializing_if = "summary_has_no_support")]
    pub staleness_leak_rate: MetricSummary,
    /// Number of PII-redaction probes that returned unredacted material.
    #[serde(default)]
    pub pii_unredacted_count: usize,
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// Returns whether a sliced summary saw no contributing probes.
fn summary_has_no_support(summary: &MetricSummary) -> bool {
    summary.denominator == 0
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
            per_lexical_backend_recall: PerLexicalBackendRecall::default(),
            p50_retrieval_latency_ms: 0,
            p95_retrieval_latency_ms: 0,
            cross_user_leak_count: 0,
            staleness_leak_rate: MetricSummary::default(),
            pii_unredacted_count: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_total_with_zero_denominator_is_zero_not_nan() {
        // Pins: a zero denominator yields a finite 0.0 mean instead of a NaN division.
        let summary = MetricSummary::from_total(7.0, 0);
        assert_eq!(summary.value, 0.0);
        assert!(
            summary.value.is_finite(),
            "zero-denominator mean must be finite"
        );
        assert_eq!(summary.numerator, 7.0);
        assert_eq!(summary.denominator, 0);
    }

    #[test]
    fn from_counts_with_zero_denominator_is_zero_not_nan() {
        // Pins: the count-based constructor inherits the zero-denominator guard.
        let summary = MetricSummary::from_counts(3, 0);
        assert_eq!(summary.value, 0.0);
        assert!(
            summary.value.is_finite(),
            "zero-denominator ratio must be finite"
        );
    }

    #[test]
    fn from_total_divides_when_denominator_is_nonzero() {
        // Pins: a non-zero denominator divides exactly so the guard is not masking real math.
        assert_eq!(MetricSummary::from_total(3.0, 4).value, 0.75);
        assert_eq!(MetricSummary::from_counts(3, 4).value, 0.75);
    }
}
