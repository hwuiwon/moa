//! Session, workspace, tool, and cache report commands.

use super::*;

pub(crate) async fn status_report(config: &MoaConfig) -> Result<String> {
    let mut report = String::new();
    let endpoint = orchestrator::orchestrator_endpoint(config);
    let health_url = orchestrator::orchestrator_health_url(config);
    report.push_str(&format!("orchestrator endpoint: {endpoint}\n"));
    match orchestrator::health_check(config).await {
        Ok(()) => report.push_str(&format!("orchestrator: healthy ({health_url})\n")),
        Err(error) => report.push_str(&format!("orchestrator: unavailable ({error})\n")),
    }

    let sessions = match orchestrator::build_client(config) {
        Ok(client) => client
            .list_sessions(SessionFilter::default())
            .await
            .map_err(|error| anyhow::anyhow!(error)),
        Err(error) => Err(error),
    };
    let sessions = match sessions {
        Ok(sessions) => sessions,
        Err(error) => {
            report.push_str(&format!("active session table: unavailable ({error})\n"));
            return Ok(report);
        }
    };
    let active = sessions
        .into_iter()
        .filter(|session| {
            matches!(
                session.status,
                SessionStatus::Created | SessionStatus::Running | SessionStatus::WaitingApproval
            )
        })
        .collect::<Vec<_>>();
    if active.is_empty() {
        report.push_str("active session table: none\n");
    } else {
        report.push_str("active session table:\n");
        for session in active {
            report.push_str(&format!(
                "- {} [{:?}] {} {}\n",
                session.session_id, session.status, session.workspace_id, session.model
            ));
        }
    }

    Ok(report)
}

pub(crate) async fn sessions_report(config: &MoaConfig, workspace: Option<&str>) -> Result<String> {
    let workspace_id = workspace.map(resolve_workspace_arg);
    let sessions = orchestrator::build_client(config)?
        .list_sessions(SessionFilter {
            workspace_id,
            ..SessionFilter::default()
        })
        .await
        .map_err(|error| anyhow::anyhow!(error))?;
    let mut report = String::new();
    for session in sessions {
        report.push_str(&format!(
            "{}\t{:?}\t{}\t{}\n",
            session.session_id, session.status, session.workspace_id, session.model
        ));
    }
    Ok(report)
}

pub(crate) async fn session_stats_report(config: &MoaConfig, id: &str) -> Result<String> {
    let session_id = moa_core::SessionId(
        Uuid::parse_str(id).with_context(|| format!("invalid session id `{id}`"))?,
    );
    let store = load_session_store(config).await?;
    let summary = store.get_session_summary(session_id).await?;

    Ok(format!(
        "session: {}\nworkspace: {}\nuser: {}\nstatus: {:?}\nturns: {}\nevents: {}\ntools: {}\nerrors: {}\nduration_seconds: {:.3}\ntokens: in {} · out {}\ncost: {}\ncache_hit_rate: {:.2}%\n",
        summary.session_id,
        summary.workspace_id,
        summary.user_id,
        summary.status,
        summary.turn_count,
        summary.event_count,
        summary.tool_call_count,
        summary.error_count,
        summary.duration_seconds,
        summary.total_input_tokens,
        summary.total_output_tokens,
        format_cents(summary.total_cost_cents),
        summary.cache_hit_rate * 100.0
    ))
}

pub(crate) async fn workspace_stats_report(
    config: &MoaConfig,
    workspace: Option<&str>,
    days: u32,
) -> Result<String> {
    let workspace_id = workspace
        .map(resolve_workspace_arg)
        .unwrap_or_else(current_workspace_id);
    let store = load_session_store(config).await?;
    store.refresh_analytics_materialized_views().await?;
    let summary = store.get_workspace_stats(&workspace_id, days).await?;

    Ok(format!(
        "workspace: {}\nwindow_days: {}\nsessions: {}\nturns: {}\ntokens: in {} · cache_read {} · out {}\ncost: {}\ncache_hit_rate: {:.2}%\n",
        summary.workspace_id,
        summary.days,
        summary.session_count,
        summary.turn_count,
        summary.total_input_tokens,
        summary.total_cache_read_tokens,
        summary.total_output_tokens,
        format_cents(summary.total_cost_cents),
        summary.cache_hit_rate * 100.0
    ))
}

pub(crate) async fn tool_stats_report(
    config: &MoaConfig,
    workspace: Option<&str>,
) -> Result<String> {
    let workspace_id = workspace.map(resolve_workspace_arg);
    let store = load_session_store(config).await?;
    let rows = store
        .list_tool_call_summaries(workspace_id.as_ref())
        .await?;

    let mut report = String::new();
    if let Some(workspace_id) = workspace_id {
        report.push_str(&format!("workspace: {}\n", workspace_id));
    }
    if rows.is_empty() {
        report.push_str("tool stats: none\n");
        return Ok(report);
    }

    report.push_str("tool\tcalls\tsuccess\tavg_ms\tp50_ms\tp95_ms\n");
    for row in rows {
        report.push_str(&format!(
            "{}\t{}\t{:.2}%\t{:.2}\t{:.2}\t{:.2}\n",
            row.tool_name,
            row.call_count,
            row.success_rate * 100.0,
            row.avg_duration_ms,
            row.p50_ms,
            row.p95_ms
        ));
    }
    Ok(report)
}

pub(crate) async fn cache_stats_report(
    config: &MoaConfig,
    workspace: Option<&str>,
    days: u32,
) -> Result<String> {
    let workspace_id = workspace
        .map(resolve_workspace_arg)
        .unwrap_or_else(current_workspace_id);
    let store = load_session_store(config).await?;
    store.refresh_analytics_materialized_views().await?;
    let summary = store.get_workspace_stats(&workspace_id, days).await?;
    let daily = store.list_cache_daily_metrics(&workspace_id, days).await?;

    let mut report = format!(
        "workspace: {}\nwindow_days: {}\ncache_hit_rate: {:.2}%\ncached_input_tokens: {}\ntotal_input_tokens: {}\nestimated_savings: unavailable (pricing history is not normalized in SQL yet)\n",
        summary.workspace_id,
        summary.days,
        summary.cache_hit_rate * 100.0,
        summary.total_cache_read_tokens,
        summary.total_input_tokens
    );
    if daily.is_empty() {
        report.push_str("daily: none\n");
        return Ok(report);
    }

    report.push_str("day\tcache_hit_rate\tcached_input_tokens\ttotal_input_tokens\tcost\n");
    for row in daily {
        report.push_str(&format!(
            "{}\t{:.2}%\t{}\t{}\t{}\n",
            row.day.format("%Y-%m-%d"),
            row.avg_cache_hit_rate * 100.0,
            row.total_cache_read_tokens,
            row.total_input_tokens,
            format_cents(row.total_cost_cents)
        ));
    }
    Ok(report)
}
