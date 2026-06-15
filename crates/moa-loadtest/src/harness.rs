//! Top-level load-test harness entrypoint.

use crate::*;

/// Runs one load-test scenario and returns the final report.
pub async fn run_loadtest(options: LoadTestOptions) -> Result<LoadTestReport> {
    options.validate()?;
    let mut config = load_config()?;
    config.observability.enabled = false;
    config.metrics.enabled = false;
    config.memory.auto_bootstrap = false;
    if matches!(options.mode, LoadMode::Mock) {
        config.compaction.enabled = false;
        config.session_limits.max_turns = 0;
        config.session_limits.loop_detection_threshold = 0;
    }

    let inspection_files = inspectable_files(None).await?;
    let plans = build_session_plans(options.sessions, options.profile, &inspection_files);
    let backend = build_backend(&options, &config).await?;
    let before_step_latency =
        scrape_step_latency_snapshot(options.metrics_endpoint.as_deref()).await?;
    let started = Instant::now();
    let run_result = run_sessions(backend.clone(), &options, plans, started).await;
    let run_result = match run_result {
        Ok(mut report) => {
            match scrape_step_latency_snapshot(options.metrics_endpoint.as_deref()).await {
                Ok(after_step_latency) => {
                    report.step_latency_ms = step_latency_delta_reports(
                        before_step_latency.as_ref(),
                        after_step_latency.as_ref(),
                    );
                    Ok(report)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    };
    let cleanup_result = backend.cleanup().await;

    match (run_result, cleanup_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(_cleanup_error)) => Err(error),
    }
}
