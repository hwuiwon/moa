//! Top-level load-test harness entrypoint.

use crate::*;

/// Runs one load-test scenario and returns the final report.
pub async fn run_loadtest(options: LoadTestOptions) -> Result<LoadTestReport> {
    options.validate()?;
    let config = load_config()?;

    let pool = TenancyPool::generate(options.tenants, options.identities_per_tenant)?;
    let targets = match options.edge_endpoint.as_deref() {
        Some(edge_endpoint) => {
            build_edge_backend_pool(&options, &config, &pool, edge_endpoint).await?
        }
        None => build_backend_pool(&options, &config, &pool).await?,
    };
    let before_step_latency =
        scrape_step_latency_snapshot(options.metrics_endpoint.as_deref()).await?;
    let started = Instant::now();
    let mut report = run_sessions(targets, pool, &options, started).await?;
    let after_step_latency =
        scrape_step_latency_snapshot(options.metrics_endpoint.as_deref()).await?;
    report.step_latency_ms =
        step_latency_delta_reports(before_step_latency.as_ref(), after_step_latency.as_ref());
    Ok(report)
}
