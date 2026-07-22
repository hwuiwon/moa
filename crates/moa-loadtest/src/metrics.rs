//! Numeric summary helpers for load-test reports.

use std::collections::{BTreeMap, BTreeSet};

use crate::*;
use moa_observability::{SESSION_EVENT_APPEND_PHASE_METRIC, TURN_STEP_DURATION_METRIC};

const METRICS_SCRAPE_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_EVENTS_APPENDED_METRIC: &str = "moa_session_events_appended_total";
const TURN_ADMISSION_LIVE_METRIC: &str = "moa_turn_admission_live";
const PROGRESS_UPDATE_EVENT_TYPE: &str = "ProgressUpdate";
const PROGRESS_NARRATED_EVENT_TYPE: &str = "ProgressNarrated";

#[derive(Debug, Clone, Default)]
pub(crate) struct RuntimeMetricsSnapshot {
    series: BTreeMap<String, HistogramSeries>,
    append_phase_series: BTreeMap<String, HistogramSeries>,
    event_appends: BTreeMap<String, f64>,
    admission_fleet_live: Option<f64>,
}

pub(crate) fn admission_fleet_live(snapshot: Option<&RuntimeMetricsSnapshot>) -> Option<u64> {
    snapshot?
        .admission_fleet_live
        .filter(|value| value.is_finite() && *value >= 0.0)
        .map(|value| value as u64)
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

pub(crate) async fn scrape_runtime_metrics_snapshot(
    endpoint: Option<&str>,
) -> Result<Option<RuntimeMetricsSnapshot>> {
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
    Ok(Some(parse_runtime_metrics_snapshot(&body)))
}

pub(crate) fn step_latency_delta_reports(
    before: Option<&RuntimeMetricsSnapshot>,
    after: Option<&RuntimeMetricsSnapshot>,
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

pub(crate) fn event_append_phase_latency_delta_reports(
    before: Option<&RuntimeMetricsSnapshot>,
    after: Option<&RuntimeMetricsSnapshot>,
) -> Vec<EventAppendPhaseLatencyReport> {
    let Some(after) = after else {
        return Vec::new();
    };
    let mut phases = BTreeSet::new();
    phases.extend(after.append_phase_series.keys().cloned());
    if let Some(before) = before {
        phases.extend(before.append_phase_series.keys().cloned());
    }

    phases
        .into_iter()
        .filter_map(|phase| {
            let after_series = after.append_phase_series.get(&phase)?;
            let before_series =
                before.and_then(|snapshot| snapshot.append_phase_series.get(&phase));
            summarize_histogram_delta(&phase, before_series, after_series).map(|report| {
                EventAppendPhaseLatencyReport {
                    phase: report.step,
                    sample_count: report.sample_count,
                    latency_ms: report.latency_ms,
                }
            })
        })
        .collect()
}

pub(crate) fn resource_bill_delta_report(
    before: Option<&RuntimeMetricsSnapshot>,
    after: Option<&RuntimeMetricsSnapshot>,
    successful_operations: u64,
) -> ResourceBillReport {
    let Some(after) = after else {
        return ResourceBillReport::default();
    };

    let mut event_types = BTreeSet::new();
    event_types.extend(after.event_appends.keys().cloned());
    if let Some(before) = before {
        event_types.extend(before.event_appends.keys().cloned());
    }

    let mut event_rows_by_type = event_types
        .into_iter()
        .filter_map(|event_type| {
            let rows = counter_delta(
                before
                    .and_then(|snapshot| snapshot.event_appends.get(&event_type))
                    .copied()
                    .unwrap_or_default(),
                after
                    .event_appends
                    .get(&event_type)
                    .copied()
                    .unwrap_or_default(),
            );
            (rows > 0).then_some(EventAppendTypeReport { event_type, rows })
        })
        .collect::<Vec<_>>();
    event_rows_by_type.sort_by(|left, right| left.event_type.cmp(&right.event_type));

    let durable_event_rows = event_rows_by_type.iter().map(|item| item.rows).sum();
    let progress_update_rows = event_rows_by_type
        .iter()
        .find(|item| item.event_type == PROGRESS_UPDATE_EVENT_TYPE)
        .map(|item| item.rows)
        .unwrap_or_default();
    let progress_narrated_rows = event_rows_by_type
        .iter()
        .find(|item| item.event_type == PROGRESS_NARRATED_EVENT_TYPE)
        .map(|item| item.rows)
        .unwrap_or_default();
    ResourceBillReport {
        durable_event_rows,
        durable_event_rows_per_successful_operation: per_successful_operation(
            durable_event_rows,
            successful_operations,
        ),
        progress_update_rows,
        progress_update_rows_per_successful_operation: per_successful_operation(
            progress_update_rows,
            successful_operations,
        ),
        progress_narrated_rows,
        progress_narrated_rows_per_successful_operation: per_successful_operation(
            progress_narrated_rows,
            successful_operations,
        ),
        event_rows_by_type,
    }
}

fn parse_runtime_metrics_snapshot(body: &str) -> RuntimeMetricsSnapshot {
    let bucket_name = format!("{TURN_STEP_DURATION_METRIC}_bucket");
    let sum_name = format!("{TURN_STEP_DURATION_METRIC}_sum");
    let count_name = format!("{TURN_STEP_DURATION_METRIC}_count");
    let append_phase_bucket_name = format!("{SESSION_EVENT_APPEND_PHASE_METRIC}_bucket");
    let append_phase_sum_name = format!("{SESSION_EVENT_APPEND_PHASE_METRIC}_sum");
    let append_phase_count_name = format!("{SESSION_EVENT_APPEND_PHASE_METRIC}_count");
    let mut snapshot = RuntimeMetricsSnapshot::default();

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
        } else if line.starts_with(&append_phase_bucket_name) {
            let Some(phase) = prometheus_label_value(line, "phase") else {
                continue;
            };
            let Some(le) = prometheus_label_value(line, "le") else {
                continue;
            };
            if le == "+Inf" {
                continue;
            }
            snapshot
                .append_phase_series
                .entry(phase.to_string())
                .or_default()
                .buckets
                .insert(le.to_string(), value);
        } else if line.starts_with(&append_phase_sum_name) {
            if let Some(phase) = prometheus_label_value(line, "phase") {
                snapshot
                    .append_phase_series
                    .entry(phase.to_string())
                    .or_default()
                    .sum = value;
            }
        } else if line.starts_with(&append_phase_count_name)
            && let Some(phase) = prometheus_label_value(line, "phase")
        {
            snapshot
                .append_phase_series
                .entry(phase.to_string())
                .or_default()
                .count = value;
        } else if line.starts_with(SESSION_EVENTS_APPENDED_METRIC)
            && let Some(event_type) = prometheus_label_value(line, "event_type")
        {
            snapshot.event_appends.insert(event_type.to_string(), value);
        } else if line.starts_with(TURN_ADMISSION_LIVE_METRIC)
            && prometheus_label_value(line, "scope") == Some("fleet")
        {
            snapshot.admission_fleet_live = Some(value);
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

fn counter_delta(before: f64, after: f64) -> u64 {
    metric_delta(before, after).round() as u64
}

fn per_successful_operation(rows: u64, successful_operations: u64) -> f64 {
    if successful_operations == 0 {
        return 0.0;
    }
    rows as f64 / successful_operations as f64
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
        let before = parse_runtime_metrics_snapshot(
            r#"
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.01"} 1
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.05"} 2
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="0.1"} 2
moa_turn_step_duration_seconds_bucket{step="pipeline_compile",le="+Inf"} 2
moa_turn_step_duration_seconds_sum{step="pipeline_compile"} 0.06
moa_turn_step_duration_seconds_count{step="pipeline_compile"} 2
"#,
        );
        let after = parse_runtime_metrics_snapshot(
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
        let before = parse_runtime_metrics_snapshot(
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
    fn capacity_snapshot_reads_only_the_fleet_admission_gauge() {
        // Pins: the composite capacity report consumes the fleet gauge without
        // mistaking per-scope samples for a fleet-wide queue depth.
        let snapshot = parse_runtime_metrics_snapshot(
            r#"
moa_turn_admission_live{scope="tenant"} 19
moa_turn_admission_live{scope="fleet"} 73
"#,
        );

        assert_eq!(admission_fleet_live(Some(&snapshot)), Some(73));
    }

    #[test]
    fn event_append_phase_latency_delta_reports_use_only_new_histogram_samples() {
        // Pins: append phase reports subtract pre-existing Prometheus histogram state.
        let before = parse_runtime_metrics_snapshot(
            r#"
moa_session_event_append_phase_seconds_bucket{phase="lock_session",le="0.01"} 1
moa_session_event_append_phase_seconds_bucket{phase="lock_session",le="0.05"} 2
moa_session_event_append_phase_seconds_bucket{phase="lock_session",le="0.1"} 2
moa_session_event_append_phase_seconds_bucket{phase="lock_session",le="+Inf"} 2
moa_session_event_append_phase_seconds_sum{phase="lock_session"} 0.06
moa_session_event_append_phase_seconds_count{phase="lock_session"} 2
moa_session_event_append_phase_seconds_bucket{phase="acquire_connection",le="0.01"} 2
moa_session_event_append_phase_seconds_bucket{phase="acquire_connection",le="+Inf"} 2
moa_session_event_append_phase_seconds_sum{phase="acquire_connection"} 0.01
moa_session_event_append_phase_seconds_count{phase="acquire_connection"} 2
moa_session_event_append_phase_seconds_bucket{phase="begin_transaction",le="0.01"} 2
moa_session_event_append_phase_seconds_bucket{phase="begin_transaction",le="+Inf"} 2
moa_session_event_append_phase_seconds_sum{phase="begin_transaction"} 0.01
moa_session_event_append_phase_seconds_count{phase="begin_transaction"} 2
"#,
        );
        let after = parse_runtime_metrics_snapshot(
            r#"
moa_session_event_append_phase_seconds_bucket{phase="lock_session",le="0.01"} 1
moa_session_event_append_phase_seconds_bucket{phase="lock_session",le="0.05"} 3
moa_session_event_append_phase_seconds_bucket{phase="lock_session",le="0.1"} 5
moa_session_event_append_phase_seconds_bucket{phase="lock_session",le="+Inf"} 5
moa_session_event_append_phase_seconds_sum{phase="lock_session"} 0.28
moa_session_event_append_phase_seconds_count{phase="lock_session"} 5
moa_session_event_append_phase_seconds_bucket{phase="acquire_connection",le="0.01"} 5
moa_session_event_append_phase_seconds_bucket{phase="acquire_connection",le="+Inf"} 5
moa_session_event_append_phase_seconds_sum{phase="acquire_connection"} 0.025
moa_session_event_append_phase_seconds_count{phase="acquire_connection"} 5
moa_session_event_append_phase_seconds_bucket{phase="begin_transaction",le="0.01"} 5
moa_session_event_append_phase_seconds_bucket{phase="begin_transaction",le="+Inf"} 5
moa_session_event_append_phase_seconds_sum{phase="begin_transaction"} 0.019
moa_session_event_append_phase_seconds_count{phase="begin_transaction"} 5
"#,
        );

        let reports = event_append_phase_latency_delta_reports(Some(&before), Some(&after));

        assert_eq!(
            reports
                .iter()
                .map(|report| report.phase.as_str())
                .collect::<Vec<_>>(),
            vec!["acquire_connection", "begin_transaction", "lock_session"]
        );
        let lock_report = reports
            .iter()
            .find(|report| report.phase == "lock_session")
            .expect("lock_session phase report should be present");
        assert_eq!(lock_report.sample_count, 3);
        assert_eq!(lock_report.latency_ms.mean.round(), 73.0);
        assert_eq!(lock_report.latency_ms.p50, 100.0);
        assert_eq!(lock_report.latency_ms.p95, 100.0);
        assert_eq!(lock_report.latency_ms.p99, 100.0);
    }

    #[test]
    fn resource_bill_uses_successful_operations() {
        // Pins: loadtest resource bills use Prometheus counter deltas, not cumulative totals.
        let before = parse_runtime_metrics_snapshot(
            r#"
moa_session_events_appended_total{event_type="UserMessage"} 10
moa_session_events_appended_total{event_type="BrainResponse"} 10
moa_session_events_appended_total{event_type="ProgressUpdate"} 6
"#,
        );
        let after = parse_runtime_metrics_snapshot(
            r#"
moa_session_events_appended_total{event_type="UserMessage"} 14
moa_session_events_appended_total{event_type="BrainResponse"} 14
moa_session_events_appended_total{event_type="ProgressUpdate"} 6
moa_session_events_appended_total{event_type="ProgressNarrated"} 2
"#,
        );

        let report = resource_bill_delta_report(Some(&before), Some(&after), 4);

        assert_eq!(report.durable_event_rows, 10);
        assert_eq!(report.durable_event_rows_per_successful_operation, 2.5);
        assert_eq!(report.progress_update_rows, 0);
        assert_eq!(report.progress_update_rows_per_successful_operation, 0.0);
        assert_eq!(report.progress_narrated_rows, 2);
        assert_eq!(report.progress_narrated_rows_per_successful_operation, 0.5);
        assert_eq!(report.event_rows_by_type.len(), 3);
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
