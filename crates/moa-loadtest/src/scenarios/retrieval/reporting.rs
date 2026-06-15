//! Perf-gate reporting, Prometheus rendering, and histogram helpers.

use crate::{cumulative_histogram_percentile, prometheus_label_value};

use super::*;

pub(super) fn enforce_gates(
    cfg: &PerfGateConfig,
    report: &LoadReport,
    leaks: &LeakReport,
) -> Result<()> {
    let mut breaches = Vec::new();
    if report.failed_requests > 0 {
        breaches.push(format!(
            "{} retrieval requests failed",
            report.failed_requests
        ));
    }
    if report.p95_ms > cfg.p95_budget_ms as f64 {
        breaches.push(format!(
            "P95 {:.1} ms > budget {} ms",
            report.p95_ms, cfg.p95_budget_ms
        ));
    }
    if report.cache_hit_rate < cfg.cache_hit_floor {
        breaches.push(format!(
            "cache hit {:.2} < floor {:.2}",
            report.cache_hit_rate, cfg.cache_hit_floor
        ));
    }
    if leaks.count > 0 {
        breaches.push(format!("RLS leaks observed: {}", leaks.count));
    }
    for (leg, p95, ceiling) in report.leg_breaches() {
        breaches.push(format!("leg {leg} P95 {p95:.1} ms > {ceiling:.1} ms"));
    }

    if report.p99_ms > cfg.p99_soft_target_ms as f64 {
        write_stderr(&format!(
            "P99 {:.1} ms exceeds soft target {} ms (warning, not failure)\n",
            report.p99_ms, cfg.p99_soft_target_ms
        ))?;
    }

    if breaches.is_empty() {
        write_stderr("all gates green\n")?;
        Ok(())
    } else {
        for breach in &breaches {
            write_stderr(&format!("{breach}\n"))?;
        }
        std::process::exit(2);
    }
}

pub(super) fn print_summary_table(report: &LoadReport, leaks: &LeakReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "perf_gate summary");
    let _ = writeln!(out, "| Metric | Value |");
    let _ = writeln!(out, "| --- | ---: |");
    let _ = writeln!(out, "| Requests | {} |", report.total_requests);
    let _ = writeln!(out, "| Successful requests | {} |", report.ok_requests);
    let _ = writeln!(out, "| Failed requests | {} |", report.failed_requests);
    let _ = writeln!(out, "| Total P50 | {:.1} ms |", report.p50_ms);
    let _ = writeln!(out, "| Total P95 | {:.1} ms |", report.p95_ms);
    let _ = writeln!(out, "| Total P99 | {:.1} ms |", report.p99_ms);
    let _ = writeln!(out, "| Cache hit rate | {:.3} |", report.cache_hit_rate);
    let _ = writeln!(out, "| RLS attack attempts | {} |", leaks.attempts);
    let _ = writeln!(out, "| RLS leaks | {} |", leaks.count);
    let _ = writeln!(out, "| Cache hit P95 | {:.1} ms |", report.cache_hit_p95_ms);
    let _ = writeln!(out, "| Embedder P95 | {:.1} ms |", report.embedder_p95_ms);
    let _ = writeln!(out, "| Graph leg P95 | {:.1} ms |", report.graph_p95_ms);
    let _ = writeln!(out, "| Vector leg P95 | {:.1} ms |", report.vector_p95_ms);
    let _ = writeln!(out, "| Lexical leg P95 | {:.1} ms |", report.lexical_p95_ms);
    let _ = writeln!(
        out,
        "| RRF + rerank P95 | {:.1} ms |",
        report.rrf_rerank_p95_ms
    );
    if !leaks.failures.is_empty() {
        let _ = writeln!(out, "\nRLS failures:");
        for failure in &leaks.failures {
            let _ = writeln!(out, "- {failure}");
        }
    }
    out
}

pub(super) fn render_prometheus(
    handle: &PrometheusHandle,
    report: &LoadReport,
    leaks: &LeakReport,
) -> String {
    let mut snapshot = handle.render();
    let _ = writeln!(snapshot, "# TYPE perf_gate_total_p95_ms gauge");
    let _ = writeln!(snapshot, "perf_gate_total_p95_ms {}", report.p95_ms);
    let _ = writeln!(snapshot, "# TYPE perf_gate_total_p99_ms gauge");
    let _ = writeln!(snapshot, "perf_gate_total_p99_ms {}", report.p99_ms);
    let _ = writeln!(snapshot, "# TYPE perf_gate_cache_hit_rate gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_cache_hit_rate {}",
        report.cache_hit_rate
    );
    let _ = writeln!(snapshot, "# TYPE perf_gate_rls_leaks gauge");
    let _ = writeln!(snapshot, "perf_gate_rls_leaks {}", leaks.count);
    let _ = writeln!(snapshot, "# TYPE perf_gate_requests_total gauge");
    let _ = writeln!(
        snapshot,
        "perf_gate_requests_total {}",
        report.total_requests
    );
    snapshot
}

pub(super) async fn write_snapshot(path: &PathBuf, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "failed to create perf snapshot directory {}",
                parent.display()
            )
        })?;
    }
    tokio::fs::write(path, body)
        .await
        .with_context(|| format!("failed to write perf snapshot {}", path.display()))
}

pub(super) fn write_stdout(message: &str) -> Result<()> {
    use std::io::Write as _;

    std::io::stdout()
        .write_all(message.as_bytes())
        .context("failed to write perf summary")
}

pub(super) fn write_stderr(message: &str) -> Result<()> {
    use std::io::Write as _;

    std::io::stderr()
        .write_all(message.as_bytes())
        .context("failed to write perf gate status")
}

pub(super) fn sanitize_prom_comment(value: &str) -> String {
    value.replace('\n', " ")
}

pub(super) fn prom_counter(snapshot: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
    snapshot
        .lines()
        .find_map(|line| {
            if !line.starts_with(metric) || line.contains("_bucket") {
                return None;
            }
            if !labels
                .iter()
                .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            {
                return None;
            }
            line.split_whitespace().last()?.parse::<f64>().ok()
        })
        .unwrap_or(0.0)
}

pub(super) fn prom_histogram_p95_ms(snapshot: &str, metric: &str, labels: &[(&str, &str)]) -> f64 {
    prometheus_histogram_percentile(snapshot, metric, labels, 0.95) * 1000.0
}

pub(super) fn prometheus_histogram_percentile(
    snapshot: &str,
    metric: &str,
    labels: &[(&str, &str)],
    quantile: f64,
) -> f64 {
    let bucket_prefix = format!("{metric}_bucket");
    let mut buckets = snapshot
        .lines()
        .filter_map(|line| {
            if !line.starts_with(&bucket_prefix) {
                return None;
            }
            if !labels.iter().all(|(key, value)| {
                prometheus_label_value(line, key)
                    .map(|actual| actual == *value)
                    .unwrap_or(false)
            }) {
                return None;
            }
            let le = prometheus_label_value(line, "le")?;
            if le == "+Inf" {
                return None;
            }
            let upper = le.parse::<f64>().ok()?;
            let count = line.split_whitespace().last()?.parse::<f64>().ok()?;
            Some((upper, count))
        })
        .collect::<Vec<_>>();
    buckets.sort_by(|left, right| left.0.total_cmp(&right.0));
    let total = buckets.last().map(|(_, count)| *count).unwrap_or(0.0);
    cumulative_histogram_percentile(&buckets, total, quantile)
}

/// Returns the percentile bucket upper bound for non-cumulative histogram buckets.
#[must_use]
pub fn histogram_percentile(buckets: &[f64], counts: &[u64], quantile: f64) -> f64 {
    let total = counts.iter().sum::<u64>();
    if total == 0 || buckets.is_empty() || counts.is_empty() {
        return 0.0;
    }
    let target = (total as f64 * quantile).ceil() as u64;
    let mut cumulative = 0_u64;
    for (bucket, count) in buckets.iter().zip(counts) {
        cumulative += count;
        if cumulative >= target {
            return *bucket;
        }
    }
    *buckets.last().unwrap_or(&0.0)
}

#[cfg(test)]
mod tests {
    use super::histogram_percentile;

    #[test]
    fn histogram_math_percentile_is_monotonic_and_within_bucket() {
        let buckets = vec![5.0, 10.0, 20.0, 40.0, 80.0, 160.0, 320.0, 640.0];
        let counts = vec![10, 20, 30, 25, 10, 3, 1, 1];
        let p50 = histogram_percentile(&buckets, &counts, 0.50);
        let p95 = histogram_percentile(&buckets, &counts, 0.95);
        let p99 = histogram_percentile(&buckets, &counts, 0.99);
        assert!(p50 <= p95 && p95 <= p99);
        assert!((40.0..=80.0).contains(&p95));
    }
}
