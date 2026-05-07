//! Top-level load-test harness entrypoint.

use crate::*;

/// Runs one load-test scenario and returns the final report.
pub async fn run_loadtest(options: LoadTestOptions) -> Result<LoadTestReport> {
    options.validate()?;
    let mut config = load_config(options.config_path.as_deref())?;
    config.observability.enabled = false;
    config.metrics.enabled = false;
    config.memory.auto_bootstrap = false;
    if matches!(options.mode, LoadMode::Mock) {
        config.compaction.enabled = false;
        config.session_limits.max_turns = 0;
        config.session_limits.loop_detection_threshold = 0;
    }

    let workspace_root = match options.target {
        LoadTarget::Local => Some(resolve_workspace_root(options.workspace_root.as_deref())?),
        LoadTarget::Daemon => None,
    };
    let inspection_files = inspectable_files(workspace_root.as_deref()).await?;
    let plans = build_session_plans(options.sessions, options.profile, &inspection_files);
    let backend = build_backend(&options, &mut config, workspace_root.clone()).await?;
    let started = Instant::now();
    let run_result = run_sessions(
        backend.clone(),
        &options,
        plans,
        workspace_root.clone(),
        started,
    )
    .await;
    let cleanup_result = backend.cleanup().await;

    match (run_result, cleanup_result) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(_cleanup_error)) => Err(error),
    }
}
