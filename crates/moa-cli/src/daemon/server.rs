//! Foreground daemon server loop and shutdown helpers.

use super::*;

/// Runs the daemon server in the foreground.
pub async fn run_daemon_server(config: MoaConfig) -> Result<()> {
    let socket_path = daemon_socket_path(&config);
    let pid_path = daemon_pid_path(&config);
    ensure_parent_dir(&socket_path).await?;
    ensure_parent_dir(&pid_path).await?;

    if fs::try_exists(&socket_path).await.unwrap_or(false) {
        fs::remove_file(&socket_path).await.ok();
    }

    let orchestrator: Arc<LocalOrchestrator> =
        Arc::new(LocalOrchestrator::from_config(config.clone()).await?);
    let session_store = orchestrator.session_store();
    let listener = UnixListener::bind(&socket_path)?;
    let info = Arc::new(DaemonInfo {
        pid: std::process::id(),
        socket_path: socket_path.display().to_string(),
        log_path: daemon_log_path(&config).display().to_string(),
        started_at: Utc::now(),
        session_count: 0,
        active_session_count: 0,
    });
    let state = DaemonState {
        orchestrator: orchestrator.clone(),
        session_store,
        info,
        daily_workspace_budget_cents: config.budgets.daily_workspace_cents,
    };

    fs::write(&pid_path, format!("{}\n", std::process::id())).await?;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let signal_task = spawn_signal_listener(shutdown_tx.clone());
    let api_task = spawn_api_server(&config, orchestrator, shutdown_tx.subscribe()).await?;
    let analytics_refresh_task =
        spawn_analytics_refresh_task(state.session_store.clone(), shutdown_tx.subscribe());

    let mut connection_tasks: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_ok() && *shutdown_rx.borrow() {
                    break;
                }
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let state = state.clone();
                let shutdown_tx = shutdown_tx.clone();
                connection_tasks.spawn(async move {
                    let result = std::panic::AssertUnwindSafe(async {
                        handle_connection(state, shutdown_tx, stream).await
                    })
                    .catch_unwind()
                    .await;
                    match result {
                        Err(panic) => tracing::error!(?panic, "daemon connection handler panicked"),
                        Ok(Err(error)) => tracing::error!(%error, "daemon request failed"),
                        Ok(Ok(())) => {}
                    }
                });
            }
            // Reap finished connection tasks to avoid unbounded growth.
            Some(_) = connection_tasks.join_next() => {}
        }
    }

    // Observation streams can outlive the shutdown signal indefinitely. Abort
    // any remaining handlers so drain does not block on long-lived clients.
    connection_tasks.abort_all();

    // Drain in-flight connection handlers before teardown.
    while let Some(result) = connection_tasks.join_next().await {
        if let Err(error) = result {
            tracing::error!(?error, "daemon connection task panicked during drain");
        }
    }

    let shutdown_grace = graceful_shutdown_timeout(&config);
    wait_for_active_turns(state.session_store.as_ref(), shutdown_grace).await?;
    match state.orchestrator.shutdown_lineage_writer().await {
        Ok(Some(stats)) => tracing::info!(
            written = stats.written,
            journal_depth = stats.journal_depth,
            last_flush_unix_ms = stats.last_flush_unix_ms,
            "lineage writer drained"
        ),
        Ok(None) => {}
        Err(error) => tracing::warn!(%error, "lineage writer shutdown failed"),
    }
    signal_task.abort();
    analytics_refresh_task.abort();
    let _ = analytics_refresh_task.await;
    if let Some(task) = api_task {
        task.abort();
        let _ = task.await;
    }
    fs::remove_file(&socket_path).await.ok();
    fs::remove_file(&pid_path).await.ok();
    Ok(())
}

fn spawn_analytics_refresh_task(
    session_store: Arc<PostgresSessionStore>,
    mut shutdown_rx: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(15 * 60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        break;
                    }
                }
                _ = interval.tick() => {
                    if let Err(error) = session_store.refresh_analytics_materialized_views().await {
                        tracing::warn!(%error, "failed to refresh analytics materialized views");
                    }
                    if let Err(error) = session_store.refresh_segment_materialized_views().await {
                        tracing::warn!(%error, "failed to refresh segment materialized views");
                    }
                }
            }
        }
    })
}

fn spawn_signal_listener(shutdown_tx: watch::Sender<bool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        match wait_for_process_signal().await {
            Ok(signal_name) => {
                tracing::warn!(signal = signal_name, "daemon received shutdown signal");
                let _ = shutdown_tx.send(true);
            }
            Err(error) => {
                tracing::error!(error = %error, "daemon signal listener failed");
            }
        }
    })
}

#[cfg(unix)]
async fn wait_for_process_signal() -> Result<&'static str> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).context("registering SIGTERM handler")?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => Ok("SIGINT"),
        _ = terminate.recv() => Ok("SIGTERM"),
    }
}

#[cfg(not(unix))]
async fn wait_for_process_signal() -> Result<&'static str> {
    tokio::signal::ctrl_c()
        .await
        .context("waiting for Ctrl+C")?;
    Ok("SIGINT")
}

async fn spawn_api_server(
    config: &MoaConfig,
    orchestrator: Arc<LocalOrchestrator>,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<Option<JoinHandle<Result<()>>>> {
    if !config.cloud.enabled {
        return Ok(None);
    }

    let fly = config.cloud.flyio.as_ref();
    let bind_host = fly
        .map(|config| config.health_bind.as_str())
        .unwrap_or("0.0.0.0");
    let port = fly.map(|config| config.internal_port).unwrap_or(8080);
    Ok(Some(
        start_api_server(orchestrator, bind_host, port, shutdown_rx).await?,
    ))
}

fn graceful_shutdown_timeout(config: &MoaConfig) -> Duration {
    let seconds = config
        .cloud
        .flyio
        .as_ref()
        .map(|fly| fly.graceful_shutdown_timeout_secs)
        .unwrap_or(30)
        .max(1);
    Duration::from_secs(seconds)
}

async fn wait_for_active_turns(
    session_store: &PostgresSessionStore,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let active = session_store
            .list_sessions(SessionFilter::default())
            .await?
            .into_iter()
            .any(|session| {
                matches!(
                    session.status,
                    SessionStatus::Running | SessionStatus::WaitingApproval
                )
            });
        if !active {
            return Ok(());
        }
        if Instant::now() >= deadline {
            tracing::warn!("graceful shutdown timeout elapsed while active sessions remain");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
