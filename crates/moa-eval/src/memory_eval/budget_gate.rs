//! Budget gate for memory-retrieval eval reports.
//!
//! Owns the gate logic behind `cargo xtask check-eval-budgets --suite
//! memory_retrieval`, so tests and tooling can apply the gate in-process
//! instead of shelling out to a nested `cargo run` (which serializes on the
//! target-dir lock and thrashes feature unification for the whole workspace).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use moa_eval_core::{Error, Result};
use serde_json::Value;

use super::{
    CorpusProfile, MemoryRetrievalEvalReport, ProbeResult, ProbeType, QueryRewritePolicy,
    TranscriptStyle,
};

const MEMORY_PR_ZERO_RECALL_RATE_MAX: f64 = 0.10;
const MEMORY_RERANKER_RECALL_REGRESSION_MAX: f64 = 0.03;
/// A reranker's job is precision; one that reduces final-window precision by
/// more than noise is strictly worse than `noop` and must not ship silently.
const MEMORY_RERANKER_PRECISION_REGRESSION_MAX: f64 = 0.03;
const MEMORY_RERANKER_RECALL_GAIN_MIN_FOR_LATENCY: f64 = 0.03;
const MEMORY_RERANKER_P95_LATENCY_MS_MAX: u64 = 2_000;
const MEMORY_REWRITE_P95_LATENCY_MS_MAX: u64 = 2_000;

/// Minimum-value floor for one dotted metric path in the report JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct MinMetricFloor {
    /// Dotted path under the report `metrics` object, such as
    /// `per_leg_recall.graph`.
    pub name: String,
    /// Smallest acceptable value for the metric.
    pub floor: f64,
}

impl FromStr for MinMetricFloor {
    type Err = Error;

    /// Parses a `name=value` floor as accepted by `--min-metric`.
    fn from_str(raw: &str) -> Result<Self> {
        let (name, value) = raw.split_once('=').ok_or_else(|| {
            Error::InvalidConfig(format!("min-metric value `{raw}` must use name=value"))
        })?;
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::InvalidConfig(format!(
                "min-metric value `{raw}` has an empty metric name"
            )));
        }
        let floor = value.trim().parse::<f64>().map_err(|error| {
            Error::InvalidConfig(format!(
                "parse min-metric floor `{value}` for `{name}`: {error}"
            ))
        })?;
        Ok(Self {
            name: name.to_string(),
            floor,
        })
    }
}

/// One budget violation surfaced by the gate.
#[derive(Debug)]
pub struct BudgetViolation {
    /// Metric identifier, such as `retrieval.recall_at_4`.
    pub metric: String,
    /// Human-readable bound the metric had to satisfy.
    pub expected: String,
    /// Human-readable observed value.
    pub actual: String,
    /// Probe IDs implicated in the violation, when probe-scoped.
    pub affected_probe_ids: Vec<String>,
}

impl BudgetViolation {
    fn new(
        metric: impl Into<String>,
        expected: impl Into<String>,
        actual: impl Into<String>,
    ) -> Self {
        Self {
            metric: metric.into(),
            expected: expected.into(),
            actual: actual.into(),
            affected_probe_ids: Vec::new(),
        }
    }

    fn with_probe_ids(mut self, probe_ids: Vec<String>) -> Self {
        self.affected_probe_ids = probe_ids;
        self
    }
}

/// Inputs to the memory-retrieval budget gate.
#[derive(Debug)]
pub struct MemoryBudgetGateOptions {
    /// Report produced by the memory-retrieval eval runner.
    pub report_path: PathBuf,
    /// Optional baseline report for regression comparison.
    pub previous_report_path: Option<PathBuf>,
    /// Maximum tolerated regression, in percent, against the baseline.
    pub max_regression_pct: f64,
    /// Suite-agnostic metric floors to enforce on the raw report JSON.
    pub min_metric_floors: Vec<MinMetricFloor>,
}

/// Result of applying the memory-retrieval budget gate to one report.
#[derive(Debug)]
pub struct MemoryBudgetGateOutcome {
    /// Violations found; empty when the gate passes.
    pub violations: Vec<BudgetViolation>,
    /// Number of regression baselines that were compared (0 or 1).
    pub regression_baselines_compared: usize,
    /// Human-readable gate output, matching the `check-eval-budgets` CLI.
    pub rendered: String,
}

impl MemoryBudgetGateOutcome {
    /// Returns true when no budget was violated.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Applies the memory-retrieval budget gate to the report at
/// `options.report_path`.
///
/// Returns an error only when a report cannot be read or parsed; budget
/// violations are reported through the outcome so callers decide how to
/// surface them.
pub async fn run_memory_retrieval_budget_gate(
    options: &MemoryBudgetGateOptions,
) -> Result<MemoryBudgetGateOutcome> {
    let raw_report = load_json_value(&options.report_path).await?;
    let report = load_memory_retrieval_report(&options.report_path).await?;

    let mut violations = memory_retrieval_gate_violations(&report);
    violations.extend(min_metric_violations(
        &raw_report,
        &options.min_metric_floors,
    ));

    let mut regression_baselines_compared = 0_usize;
    if let Some(previous_path) = options.previous_report_path.as_deref() {
        let previous = load_memory_retrieval_report(previous_path).await?;
        regression_baselines_compared += 1;
        violations.extend(compare_memory_regression(
            &report,
            &previous,
            options.max_regression_pct,
        ));
    }

    let rendered = render_outcome(
        &violations,
        regression_baselines_compared,
        &options.min_metric_floors,
    );
    Ok(MemoryBudgetGateOutcome {
        violations,
        regression_baselines_compared,
        rendered,
    })
}

/// Renders the gate outcome exactly as the `check-eval-budgets` CLI prints it.
fn render_outcome(
    violations: &[BudgetViolation],
    regression_baselines_compared: usize,
    floors: &[MinMetricFloor],
) -> String {
    if violations.is_empty() {
        let floors = if floors.is_empty() {
            "none".to_string()
        } else {
            floors
                .iter()
                .map(|floor| floor.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return format!(
            "Memory-retrieval budgets passed: 1 report checked, {regression_baselines_compared} regression baseline(s) compared, floors met: {floors}.\n"
        );
    }

    let mut rendered = String::from("Budget violations:\n  scenario: memory_retrieval\n");
    for violation in violations {
        if violation.affected_probe_ids.is_empty() {
            let _ = writeln!(
                rendered,
                "    {}: expected {}, actual {}",
                violation.metric, violation.expected, violation.actual
            );
        } else {
            let _ = writeln!(
                rendered,
                "    {}: expected {}, actual {} (affected probe IDs: {})",
                violation.metric,
                violation.expected,
                violation.actual,
                violation.affected_probe_ids.join(", ")
            );
        }
    }
    let _ = writeln!(
        rendered,
        "\nTotal: 1 scenario(s) failed, {} metric violation(s).",
        violations.len()
    );
    rendered
}

async fn load_json_value(path: &Path) -> Result<Value> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    serde_json::from_str(&raw).map_err(|source| Error::ParseJson {
        path: path.to_path_buf(),
        source,
    })
}

async fn load_memory_retrieval_report(path: &Path) -> Result<MemoryRetrievalEvalReport> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
    serde_json::from_str(&raw).map_err(|source| Error::ParseJson {
        path: path.to_path_buf(),
        source,
    })
}

fn memory_retrieval_gate_violations(report: &MemoryRetrievalEvalReport) -> Vec<BudgetViolation> {
    let mut violations = Vec::new();
    if report.aborted_over_budget {
        let actual = report
            .cost
            .as_ref()
            .map(|cost| {
                format!(
                    "estimated ${:.4} over budget ${:.4}",
                    cost.est_usd, cost.budget_usd
                )
            })
            .unwrap_or_else(|| "true".to_string());
        violations.push(BudgetViolation::new("aborted_over_budget", "false", actual));
    }

    let cross_user_leak_probe_ids = cross_user_leak_probe_ids(&report.probe_results);
    let cross_user_leak_count = cross_user_leak_count(&report.probe_results);
    if cross_user_leak_count != 0 {
        violations.push(
            BudgetViolation::new(
                "cross_user_leak_count",
                "0",
                cross_user_leak_count.to_string(),
            )
            .with_probe_ids(cross_user_leak_probe_ids),
        );
    }

    let pii_unredacted_probe_ids = pii_unredacted_probe_ids(&report.probe_results);
    if !pii_unredacted_probe_ids.is_empty() {
        violations.push(
            BudgetViolation::new(
                "pii_unredacted_count",
                "0",
                pii_unredacted_probe_ids.len().to_string(),
            )
            .with_probe_ids(pii_unredacted_probe_ids),
        );
    }

    if report.manifest.profile == CorpusProfile::Pr
        && report.manifest.transcript_style == TranscriptStyle::Marked
        && report.metrics.zero_recall_rate.value > MEMORY_PR_ZERO_RECALL_RATE_MAX
    {
        violations.push(BudgetViolation::new(
            "zero_recall_rate",
            format!("<= {MEMORY_PR_ZERO_RECALL_RATE_MAX:.4}"),
            format!("{:.4}", report.metrics.zero_recall_rate.value),
        ));
    }

    if report.reranker_enabled {
        let pre_recall_at_4 = report.metrics.pre_rerank_recall_at_4.value;
        let post_recall_at_4 = report.metrics.post_rerank_recall_at_4.value;
        let recall_delta = post_recall_at_4 - pre_recall_at_4;
        let recall_regression = pre_recall_at_4 - post_recall_at_4;
        if recall_regression > MEMORY_RERANKER_RECALL_REGRESSION_MAX {
            violations.push(BudgetViolation::new(
                "retrieval.reranker_recall_at_4_regression",
                format!("<= {MEMORY_RERANKER_RECALL_REGRESSION_MAX:.2}"),
                format!(
                    "{recall_regression:.4} (pre {pre_recall_at_4:.4}, post {post_recall_at_4:.4})"
                ),
            ));
        }
        let pre_precision_at_4 = report.metrics.pre_rerank_precision_at_4.value;
        let post_precision_at_4 = report.metrics.precision_at_4.value;
        let precision_regression = pre_precision_at_4 - post_precision_at_4;
        if precision_regression > MEMORY_RERANKER_PRECISION_REGRESSION_MAX {
            violations.push(BudgetViolation::new(
                "retrieval.reranker_precision_at_4_regression",
                format!("<= {MEMORY_RERANKER_PRECISION_REGRESSION_MAX:.2}"),
                format!(
                    "{precision_regression:.4} (pre {pre_precision_at_4:.4}, post {post_precision_at_4:.4})"
                ),
            ));
        }
        if report.metrics.p95_retrieval_latency_ms > MEMORY_RERANKER_P95_LATENCY_MS_MAX
            && recall_delta < MEMORY_RERANKER_RECALL_GAIN_MIN_FOR_LATENCY
        {
            violations.push(BudgetViolation::new(
                "retrieval.p95_retrieval_latency_ms",
                format!(
                    "<= {MEMORY_RERANKER_P95_LATENCY_MS_MAX} unless recall@4 gain >= {MEMORY_RERANKER_RECALL_GAIN_MIN_FOR_LATENCY:.2}"
                ),
                format!(
                    "{} (recall@4 gain {recall_delta:.4})",
                    report.metrics.p95_retrieval_latency_ms
                ),
            ));
        }
    }

    if report.query_rewrite_policy == QueryRewritePolicy::Gated {
        match report.query_rewrite_by_class.get("exact_identifier") {
            Some(metrics) if metrics.total_count > 0 && metrics.call_count == 0 => {}
            Some(metrics) if metrics.total_count > 0 => violations.push(BudgetViolation::new(
                "query_rewrite.exact_identifier_call_count",
                "0",
                metrics.call_count.to_string(),
            )),
            _ => violations.push(BudgetViolation::new(
                "query_rewrite.exact_identifier_controls",
                "present with at least 1 probe",
                "missing".to_string(),
            )),
        }
    }

    violations
}

fn min_metric_violations(report: &Value, floors: &[MinMetricFloor]) -> Vec<BudgetViolation> {
    floors
        .iter()
        .filter_map(|floor| match resolve_metric_number(report, &floor.name) {
            Ok(actual) if actual < floor.floor => Some(BudgetViolation::new(
                floor.name.clone(),
                format!(">= {:.4}", floor.floor),
                format!("{actual:.4}"),
            )),
            Ok(_) => None,
            Err(error) => Some(BudgetViolation::new(
                floor.name.clone(),
                format!(">= {:.4}", floor.floor),
                error.to_string(),
            )),
        })
        .collect()
}

fn resolve_metric_number(report: &Value, name: &str) -> Result<f64> {
    let mut current = report
        .get("metrics")
        .ok_or_else(|| Error::InvalidConfig("report is missing metrics object".to_string()))?;
    for part in name.split('.') {
        if part.is_empty() {
            return Err(Error::InvalidConfig(format!(
                "metric path `{name}` contains an empty segment"
            )));
        }
        current = current.get(part).ok_or_else(|| {
            Error::InvalidConfig(format!("metric `{name}` is missing path segment `{part}`"))
        })?;
    }
    if let Some(value) = current.as_f64() {
        return Ok(value);
    }
    if let Some(value) = current.get("value").and_then(Value::as_f64) {
        return Ok(value);
    }
    Err(Error::InvalidConfig(format!(
        "metric `{name}` did not resolve to a numeric value"
    )))
}

fn compare_memory_regression(
    current: &MemoryRetrievalEvalReport,
    previous: &MemoryRetrievalEvalReport,
    max_regression_pct: f64,
) -> Vec<BudgetViolation> {
    let mut violations = [
        (
            "retrieval.recall_at_4",
            current.metrics.recall_at_4.value,
            previous.metrics.recall_at_4.value,
        ),
        (
            "retrieval.recall_at_25",
            current.metrics.recall_at_25.value,
            previous.metrics.recall_at_25.value,
        ),
        (
            "retrieval.mrr",
            current.metrics.mrr.value,
            previous.metrics.mrr.value,
        ),
        (
            "retrieval.ndcg_at_4",
            current.metrics.ndcg_at_4.value,
            previous.metrics.ndcg_at_4.value,
        ),
        // Precision is the counterweight to the recall metrics above: without
        // it, a change can pass this gate by widening the window with noise.
        // Baselines recorded before the metric existed deserialize to 0.0 and
        // are skipped by `regression_pct`.
        (
            "retrieval.precision_at_4",
            current.metrics.precision_at_4.value,
            previous.metrics.precision_at_4.value,
        ),
    ]
    .into_iter()
    .filter_map(|(metric, current_value, previous_value)| {
        regression_pct(current_value, previous_value)
            .filter(|regression| *regression > max_regression_pct)
            .map(|regression| {
                BudgetViolation::new(
                    metric,
                    format!("regression <= {max_regression_pct:.2}%"),
                    format!(
                        "{current_value:.4} (regression: {regression:+.2}% vs baseline {previous_value:.4})"
                    ),
                )
            })
    })
    .collect::<Vec<_>>();

    violations.extend(compare_query_rewrite_regression(current, previous));
    violations
}

fn compare_query_rewrite_regression(
    current: &MemoryRetrievalEvalReport,
    previous: &MemoryRetrievalEvalReport,
) -> Vec<BudgetViolation> {
    let mut violations = Vec::new();
    if current.query_rewrite_policy != QueryRewritePolicy::Gated
        || previous.query_rewrite_policy != QueryRewritePolicy::Always
    {
        return violations;
    }

    if previous.query_rewrite_call_count > 0
        && current.query_rewrite_call_count.saturating_mul(2) > previous.query_rewrite_call_count
    {
        let reduction = 1.0
            - (current.query_rewrite_call_count as f64 / previous.query_rewrite_call_count as f64);
        violations.push(BudgetViolation::new(
            "query_rewrite.call_count_reduction",
            ">= 50.00% fewer calls than always",
            format!(
                "{:.2}% fewer ({} gated calls vs {} always calls)",
                reduction * 100.0,
                current.query_rewrite_call_count,
                previous.query_rewrite_call_count
            ),
        ));
    }

    let current_p95 = current.retrieval_plus_rewrite_p95_latency_ms;
    let previous_p95 = previous.retrieval_plus_rewrite_p95_latency_ms;
    if current_p95 > MEMORY_REWRITE_P95_LATENCY_MS_MAX
        && (previous_p95 == 0 || current_p95 > previous_p95)
    {
        violations.push(BudgetViolation::new(
            "query_rewrite.retrieval_plus_rewrite_p95_latency_ms",
            format!("<= {MEMORY_REWRITE_P95_LATENCY_MS_MAX} or <= always baseline {previous_p95}"),
            current_p95.to_string(),
        ));
    }

    violations
}

/// Regression of a higher-is-better metric, in percent, when it regressed.
fn regression_pct(current: f64, previous: f64) -> Option<f64> {
    if previous.abs() < f64::EPSILON {
        return None;
    }
    let delta = previous - current;
    (delta > 0.0).then(|| (delta / previous.abs()) * 100.0)
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
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn pii_unredacted_probe_ids(probe_results: &[ProbeResult]) -> Vec<String> {
    probe_results
        .iter()
        .filter(|probe| probe.probe_type == ProbeType::PiiRedaction)
        .filter(|probe| probe.stored_pii_redacted == Some(false))
        .map(|probe| probe.probe_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::kernel::{PerLexicalBackendRecall, RetrievalCoreMetrics};
    use crate::memory_eval::runner::QueryRewriteClassMetrics;
    use crate::memory_eval::{
        CorpusManifest, GoldResolutionReport, GraphExpansionEvalPolicy, MetricSummary,
        PerLegRecall, RetrievalMetrics,
    };

    #[test]
    fn check_eval_budgets_min_metric_fails_below_floor() {
        // Pins: --min-metric compares MetricSummary.value from raw report JSON.
        let report = serde_json::json!({
            "metrics": {
                "ingestion_coverage": {
                    "numerator": 8.0,
                    "denominator": 10,
                    "value": 0.80
                }
            }
        });
        let floors = vec![
            "ingestion_coverage=0.85"
                .parse::<MinMetricFloor>()
                .expect("parse floor"),
        ];

        let violations = min_metric_violations(&report, &floors);

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].metric, "ingestion_coverage");
        assert_eq!(violations[0].expected, ">= 0.8500");
        assert_eq!(violations[0].actual, "0.8000");
    }

    #[test]
    fn check_eval_budgets_min_metric_resolves_nested_per_leg_names() {
        // Pins: suite-agnostic metric floors walk dotted JSON paths and MetricSummary.value leaves.
        let report = serde_json::json!({
            "metrics": {
                "per_leg_recall": {
                    "graph": {
                        "numerator": 9.0,
                        "denominator": 10,
                        "value": 0.90
                    }
                }
            }
        });

        let actual =
            resolve_metric_number(&report, "per_leg_recall.graph").expect("resolve nested metric");

        assert_eq!(actual, 0.90);
        assert!(
            min_metric_violations(
                &report,
                &["per_leg_recall.graph=0.90"
                    .parse::<MinMetricFloor>()
                    .expect("parse floor")]
            )
            .is_empty()
        );
    }

    #[test]
    fn check_eval_budgets_min_metric_treats_absent_metric_as_violation() {
        // Pins: a --min-metric floor on a metric absent from the report fails the gate
        // (surfacing the resolution error) rather than silently passing.
        let report = serde_json::json!({
            "metrics": {
                "ingestion_coverage": { "value": 0.95 }
            }
        });
        let floors = vec![
            "multi_hop_recall=0.80"
                .parse::<MinMetricFloor>()
                .expect("parse floor"),
        ];

        let violations = min_metric_violations(&report, &floors);

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].metric, "multi_hop_recall");
        assert_eq!(violations[0].expected, ">= 0.8000");
        assert!(
            violations[0]
                .actual
                .contains("missing path segment `multi_hop_recall`"),
            "absent metric should surface the resolution error, got {:?}",
            violations[0].actual
        );
    }

    #[test]
    fn memory_regression_checks_recall25_and_rewrite_budget() {
        // Pins: gated-vs-always comparison enforces recall@25, call reduction, and rewrite p95.
        let previous = memory_report(QueryRewritePolicy::Always, 147, 0, 2_100, 0, 0, 0.96);
        let current = memory_report(QueryRewritePolicy::Gated, 84, 63, 2_200, 5, 0, 0.80);

        let violations = compare_memory_regression(&current, &previous, 5.0);
        let metrics = violations
            .iter()
            .map(|violation| violation.metric.as_str())
            .collect::<BTreeSet<_>>();

        assert!(metrics.contains("retrieval.recall_at_25"));
        assert!(metrics.contains("query_rewrite.call_count_reduction"));
        assert!(metrics.contains("query_rewrite.retrieval_plus_rewrite_p95_latency_ms"));
    }

    #[test]
    fn memory_gate_requires_gated_exact_identifier_controls() {
        // Pins: gated reports prove exact-anchor controls exist and do not invoke rewriting.
        let missing = memory_report(QueryRewritePolicy::Gated, 10, 10, 100, 0, 0, 1.0);
        let missing_violations = memory_retrieval_gate_violations(&missing);
        assert!(
            missing_violations
                .iter()
                .any(|violation| violation.metric == "query_rewrite.exact_identifier_controls")
        );

        let rewritten = memory_report(QueryRewritePolicy::Gated, 10, 10, 100, 3, 1, 1.0);
        let rewritten_violations = memory_retrieval_gate_violations(&rewritten);
        assert!(rewritten_violations.iter().any(|violation| {
            violation.metric == "query_rewrite.exact_identifier_call_count"
                && violation.actual == "1"
        }));

        let skipped = memory_report(QueryRewritePolicy::Gated, 10, 10, 100, 3, 0, 1.0);
        assert!(
            memory_retrieval_gate_violations(&skipped)
                .iter()
                .all(|violation| !violation
                    .metric
                    .starts_with("query_rewrite.exact_identifier"))
        );
    }

    fn memory_report(
        policy: QueryRewritePolicy,
        call_count: usize,
        skip_count: usize,
        rewrite_p95_ms: u64,
        exact_total: usize,
        exact_calls: usize,
        recall_at_25: f64,
    ) -> MemoryRetrievalEvalReport {
        let mut by_class = BTreeMap::new();
        if exact_total > 0 {
            by_class.insert(
                "exact_identifier".to_string(),
                QueryRewriteClassMetrics {
                    total_count: exact_total,
                    call_count: exact_calls,
                    skip_count: exact_total.saturating_sub(exact_calls),
                    call_rate: exact_calls as f64 / exact_total as f64,
                },
            );
        }

        MemoryRetrievalEvalReport {
            manifest: CorpusManifest {
                version: 1,
                corpus_id: "test-corpus".to_string(),
                profile: CorpusProfile::Pr,
                description: "test corpus".to_string(),
                seeds: vec![1, 2, 3],
                transcript_style: TranscriptStyle::Marked,
            },
            candidate_k: 25,
            final_k: 4,
            reranker_enabled: false,
            parity: false,
            query_rewrite_policy: policy,
            graph_expansion_policy: GraphExpansionEvalPolicy::Current,
            graph_retrieval_policy: GraphExpansionEvalPolicy::Current.graph_retrieval_policy(),
            graph_diagnostics: Default::default(),
            query_rewrite_call_count: call_count,
            query_rewrite_skip_count: skip_count,
            query_rewrite_call_rate: if call_count + skip_count == 0 {
                0.0
            } else {
                call_count as f64 / (call_count + skip_count) as f64
            },
            query_rewrite_p50_latency_ms: 0,
            query_rewrite_p95_latency_ms: 0,
            query_rewrite_input_tokens: 0,
            query_rewrite_output_tokens: 0,
            query_rewrite_est_usd: 0.0,
            retrieval_plus_rewrite_p95_latency_ms: rewrite_p95_ms,
            query_rewrite_by_class: by_class,
            aborted_over_budget: false,
            cost: None,
            providers: None,
            metrics: retrieval_metrics(recall_at_25),
            probe_results: Vec::new(),
            bootstrap: Vec::new(),
            cross_user_leak_probe_ids: Vec::new(),
            gold_resolution: GoldResolutionReport {
                ingest_reports: Vec::new(),
                records: Vec::new(),
            },
            consolidation: None,
        }
    }

    fn retrieval_metrics(recall_at_25: f64) -> RetrievalMetrics {
        RetrievalMetrics {
            core: RetrievalCoreMetrics {
                recall_at_4: metric(0.90),
                recall_at_25: metric(recall_at_25),
                mrr: metric(0.90),
                ndcg_at_4: metric(0.90),
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
            },
            ingestion_coverage: MetricSummary::default(),
            scope_match_rate: MetricSummary::default(),
            scope_match_rate_contact: MetricSummary::default(),
            scope_match_rate_tenant: MetricSummary::default(),
            extraction_precision: MetricSummary::default(),
            entity_fragmentation: MetricSummary::default(),
            pre_rerank_recall_at_4: MetricSummary::default(),
            pre_rerank_recall_at_25: MetricSummary::default(),
            post_rerank_recall_at_4: MetricSummary::default(),
            precision_at_4: MetricSummary::default(),
            pre_rerank_precision_at_4: MetricSummary::default(),
            graded_precision_at_4: MetricSummary::default(),
            rendered_context_precision: MetricSummary::default(),
            abstention_false_positive_rate: MetricSummary::default(),
            all_expected_found_at_4: MetricSummary::default(),
            forbidden_fact_absent_at_4: MetricSummary::default(),
            stored_pii_redacted: MetricSummary::default(),
            retrieval_temporal_as_of_correct: MetricSummary::default(),
            temporal_parse_rate: MetricSummary::default(),
            temporal_parse_mismatch_count: 0,
            preference_context_rate: MetricSummary::default(),
            graded_ndcg_at_10: MetricSummary::default(),
            per_probe_type: std::collections::BTreeMap::new(),
        }
    }

    fn metric(value: f64) -> MetricSummary {
        MetricSummary {
            numerator: value,
            denominator: 1,
            value,
        }
    }
}
