//! Numeric summary helpers for load-test reports.

use crate::*;

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
