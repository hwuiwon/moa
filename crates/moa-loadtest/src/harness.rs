//! Top-level load-test harness entrypoint.

use crate::*;

/// Runs one load-test scenario and returns the final report.
pub async fn run_loadtest(options: LoadTestOptions) -> Result<LoadTestReport> {
    options.validate()?;
    let config = MoaConfig::load()?;

    let pool = TenancyPool::generate(options.tenants, options.identities_per_tenant)?;
    let targets = match options.edge_endpoint.as_deref() {
        Some(edge_endpoint) => {
            build_edge_backend_pool(&options, &config, &pool, edge_endpoint).await?
        }
        None => build_backend_pool(&options, &config, &pool).await?,
    };
    let before_metrics =
        scrape_runtime_metrics_snapshot(options.metrics_endpoint.as_deref()).await?;
    let started = Instant::now();
    let mut report = run_sessions(targets, pool, &options, started).await?;
    let after_metrics =
        scrape_runtime_metrics_snapshot(options.metrics_endpoint.as_deref()).await?;
    report.step_latency_ms =
        step_latency_delta_reports(before_metrics.as_ref(), after_metrics.as_ref());
    report.event_append_phase_latency_ms =
        event_append_phase_latency_delta_reports(before_metrics.as_ref(), after_metrics.as_ref());
    report.resource_bill = resource_bill_delta_report(
        before_metrics.as_ref(),
        after_metrics.as_ref(),
        report.successful_operations,
    );
    Ok(report)
}
