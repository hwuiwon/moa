//! Numeric summary helpers for load-test reports.

use std::collections::{BTreeMap, BTreeSet};

use crate::*;
use moa_observability::TURN_STEP_DURATION_METRIC;

const METRICS_SCRAPE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Default)]
pub(crate) struct StepLatencySnapshot {
    series: BTreeMap<String, HistogramSeries>,
}

#[derive(Debug, Clone, Default)]
struct HistogramSeries {
    buckets: BTreeMap<String, f64>,
    sum: f64,
    count: f64,
}

pub(crate) fn summarize_percentiles(samples: &[f64]) -> PercentileSummary {
    if samples.is_empty() {
        return PercentileSummary {
            min: 0.0,
            mean: 0.0,
            p50: 0.0,
            p95: 0.0,
            p99: 0.0,
            max: 0.0,
        };
    }

    let mut sorted = samples.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    let sum: f64 = sorted.iter().sum();
    PercentileSummary {
        min: *sorted.first().unwrap_or(&0.0),
        mean: sum / sorted.len() as f64,
        p50: percentile(&sorted, 0.50),
        p95: percentile(&sorted, 0.95),
        p99: percentile(&sorted, 0.99),
        max: *sorted.last().unwrap_or(&0.0),
    }
}

pub(crate) fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

pub(crate) async fn scrape_step_latency_snapshot(
    endpoint: Option<&str>,
) -> Result<Option<StepLatencySnapshot>> {
    let Some(endpoint) = endpoint else {
        return Ok(None);
    };

    let client = reqwest::Client::builder()
        .timeout(METRICS_SCRAPE_TIMEOUT)
        .build()
        .map_err(|error| MoaError::ProviderError(format!("metrics HTTP client: {error}")))?;
    let response = client
        .get(endpoint)
        .send()
        .await
        .map_err(|error| MoaError::ProviderError(format!("metrics scrape failed: {error}")))?;
    if !response.status().is_success() {
        return Err(MoaError::ProviderError(format!(
            "metrics scrape returned HTTP {}",
            response.status()
        )));
    }
    let body = response
        .text()
        .await
        .map_err(|error| MoaError::ProviderError(format!("metrics scrape body: {error}")))?;
    Ok(Some(parse_step_latency_snapshot(&body)))
}

pub(crate) fn step_latency_delta_reports(
    before: Option<&StepLatencySnapshot>,
    after: Option<&StepLatencySnapshot>,
) -> Vec<StepLatencyReport> {
    let Some(after) = after else {
        return Vec::new();
    };
    let mut steps = BTreeSet::new();
    steps.extend(after.series.keys().cloned());
    if let Some(before) = before {
        steps.extend(before.series.keys().cloned());
    }

    steps
        .into_iter()
        .filter_map(|step| {
            let after_series = after.series.get(&step)?;
            let before_series = before.and_then(|snapshot| snapshot.series.get(&step));
            summarize_histogram_delta(&step, before_series, after_series)
        })
        .collect()
}

fn parse_step_latency_snapshot(body: &str) -> StepLatencySnapshot {
    let bucket_name = format!("{TURN_STEP_DURATION_METRIC}_bucket");
    let sum_name = format!("{TURN_STEP_DURATION_METRIC}_sum");
    let count_name = format!("{TURN_STEP_DURATION_METRIC}_count");
    let mut snapshot = StepLatencySnapshot::default();

    for line in body.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(value) = metric_value(line) else {
            continue;
        };

        if line.starts_with(&bucket_name) {
            let Some(step) = prometheus_label_value(line, "step") else {
                continue;
            };
            let Some(le) = prometheus_label_value(line, "le") else {
                continue;
            };
            if le == "+Inf" {
                continue;
            }
            snapshot
                .series
                .entry(step.to_string())
                .or_default()
                .buckets
                .insert(le.to_string(), value);
        } else if line.starts_with(&sum_name) {
            if let Some(step) = prometheus_label_value(line, "step") {
                snapshot.series.entry(step.to_string()).or_default().sum = value;
            }
        } else if line.starts_with(&count_name)
            && let Some(step) = prometheus_label_value(line, "step")
        {
            snapshot.series.entry(step.to_string()).or_default().count = value;
        }
    }

    snapshot
}

fn summarize_histogram_delta(
    step: &str,
    before: Option<&HistogramSeries>,
    after: &HistogramSeries,
) -> Option<StepLatencyReport> {
    let count = metric_delta(
        before.map(|series| series.count).unwrap_or_default(),
        after.count,
    );
    if count <= 0.0 {
        return None;
    }
    let sum = metric_delta(
        before.map(|series| series.sum).unwrap_or_default(),
        after.sum,
    );
    let buckets = histogram_delta_buckets(before, after);
    let mean_ms = if count > 0.0 {
        (sum / count) * 1_000.0
    } else {
        0.0
    };

    Some(StepLatencyReport {
        step: step.to_string(),
        sample_count: count.round() as u64,
        latency_ms: PercentileSummary {
            min: histogram_min(&buckets) * 1_000.0,
            mean: mean_ms,
            p50: cumulative_histogram_percentile(&buckets, count, 0.50) * 1_000.0,
            p95: cumulative_histogram_percentile(&buckets, count, 0.95) * 1_000.0,
            p99: cumulative_histogram_percentile(&buckets, count, 0.99) * 1_000.0,
            max: cumulative_histogram_percentile(&buckets, count, 1.0) * 1_000.0,
        },
    })
}

fn histogram_delta_buckets(
    before: Option<&HistogramSeries>,
    after: &HistogramSeries,
) -> Vec<(f64, f64)> {
    let mut bounds = BTreeSet::new();
    bounds.extend(after.buckets.keys().cloned());
    if let Some(before) = before {
        bounds.extend(before.buckets.keys().cloned());
    }

    let mut buckets = bounds
        .into_iter()
        .filter_map(|upper| {
            let upper_bound = upper.parse::<f64>().ok()?;
            let before_count = before
                .and_then(|series| series.buckets.get(&upper))
                .copied()
                .unwrap_or_default();
            let after_count = after.buckets.get(&upper).copied().unwrap_or_default();
            Some((upper_bound, metric_delta(before_count, after_count)))
        })
        .collect::<Vec<_>>();
    buckets.sort_by(|left, right| left.0.total_cmp(&right.0));
    buckets
}

fn histogram_min(buckets: &[(f64, f64)]) -> f64 {
    buckets
        .iter()
        .find_map(|(upper, cumulative)| (*cumulative > 0.0).then_some(*upper))
        .unwrap_or_default()
}

pub(crate) fn cumulative_histogram_percentile(
    buckets: &[(f64, f64)],
    count: f64,
    quantile: f64,
) -> f64 {
    if count <= 0.0 {
        return 0.0;
    }
    let target = count * quantile;
    buckets
        .iter()
        .find_map(|(upper, cumulative)| (*cumulative >= target).then_some(*upper))
        .or_else(|| buckets.last().map(|(upper, _)| *upper))
        .unwrap_or_default()
}

fn metric_delta(before: f64, after: f64) -> f64 {
    (after - before).max(0.0)
}

fn metric_value(line: &str) -> Option<f64> {
    line.split_whitespace().last()?.parse::<f64>().ok()
}

pub(crate) fn prometheus_label_value<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let start = line.find(&format!("{label}=\""))? + label.len() + 2;
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

pub(crate) fn format_millis(value: f64) -> String {
    if value >= 1_000.0 {
        format!("{:.2}s", value / 1_000.0)
    } else {
        format!("{value:.0}ms")
    }
}

pub(crate) fn format_cost(cost_cents: u64) -> String {
    format!("${:.2}", cost_cents as f64 / 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_latency_delta_reports_use_only_new_histogram_samples() {
        // Pins: loadtest step latency reports subtract pre-existing Prometheus histogram state.
        let before = parse_step_latency_snapshot(
            r#"
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.01"} 1
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.05"} 2
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.1"} 2
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="+Inf"} 2
moa_turn_step_duration_seconds_sum{step="pipeline_compile"} 0.06
moa_turn_step_duration_seconds_count{step="pipeline_compile"} 2
"#,
        );
        let after = parse_step_latency_snapshot(
            r#"
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.01"} 1
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.05"} 3
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.1"} 5
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="+Inf"} 5
moa_turn_step_duration_seconds_sum{step="pipeline_compile"} 0.28
moa_turn_step_duration_seconds_count{step="pipeline_compile"} 5
"#,
        );

        let reports = step_latency_delta_reports(Some(&before), Some(&after));

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].step, "pipeline_compile");
        assert_eq!(reports[0].sample_count, 3);
        assert_eq!(reports[0].latency_ms.mean.round(), 73.0);
        assert_eq!(reports[0].latency_ms.p50, 100.0);
        assert_eq!(reports[0].latency_ms.p95, 100.0);
        assert_eq!(reports[0].latency_ms.p99, 100.0);
    }

    #[test]
    fn step_latency_delta_reports_skip_steps_without_new_samples() {
        // Pins: stale histogram series do not appear as step latency reports.
        let before = parse_step_latency_snapshot(
            r#"
moa_turn_step_duration_seconds_bucket{step="llm_call",le="0.1"} 4
moa_turn_step_duration_seconds_sum{step="llm_call"} 0.4
moa_turn_step_duration_seconds_count{step="llm_call"} 4
"#,
        );
        let after = before.clone();

        let reports = step_latency_delta_reports(Some(&before), Some(&after));

        assert!(reports.is_empty());
    }

    #[test]
    fn summarize_percentiles_uses_nearest_rank_over_sorted_sample() {
        // Pins: p50/p95/p99 use the nearest-rank index `((n-1)*q).round()` over the
        // sorted sample, and `summarize_percentiles` sorts its input before ranking.
        // The sample is 1.0..=100.0 supplied in reverse to prove the sort happens; for
        // this sample the value equals (index + 1).
        let samples: Vec<f64> = (1..=100).rev().map(|value| value as f64).collect();

        let summary = summarize_percentiles(&samples);

        assert_eq!(summary.min, 1.0);
        assert_eq!(summary.max, 100.0);
        assert_eq!(summary.mean, 50.5);
        // index = ((100 - 1) * 0.50).round() = 50 -> sorted[50] = 51.0
        assert_eq!(summary.p50, 51.0);
        // index = ((100 - 1) * 0.95).round() = 94 -> sorted[94] = 95.0
        assert_eq!(summary.p95, 95.0);
        // index = ((100 - 1) * 0.99).round() = 98 -> sorted[98] = 99.0
        assert_eq!(summary.p99, 99.0);
    }

    #[test]
    fn summarize_percentiles_on_empty_sample_is_all_zero_not_nan() {
        // Pins: the empty-sample guard returns zeros, never a divide-by-zero NaN mean.
        let summary = summarize_percentiles(&[]);

        assert_eq!(summary.min, 0.0);
        assert_eq!(summary.mean, 0.0);
        assert_eq!(summary.p50, 0.0);
        assert_eq!(summary.p95, 0.0);
        assert_eq!(summary.p99, 0.0);
        assert_eq!(summary.max, 0.0);
        assert!(!summary.mean.is_nan());
    }

    #[test]
    fn percentile_on_empty_slice_returns_zero() {
        // Pins: the percentile() empty-slice guard avoids indexing an empty slice.
        assert_eq!(percentile(&[], 0.95), 0.0);
    }
}
