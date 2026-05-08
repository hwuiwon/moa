//! Local environment diagnostic command helpers.

use super::*;

pub(crate) async fn doctor_report(config: &MoaConfig, log_path: &Path) -> Result<String> {
    let database_line = doctor_database(config).await;
    let mut lines = vec![
        "MOA doctor".to_string(),
        format!("provider: {}", config.general.default_provider),
        format!("model: {}", config.models.main),
        format!(
            "anthropic_key: {} ({})",
            env_presence(&config.providers.anthropic.api_key_env),
            config.providers.anthropic.api_key_env
        ),
        format!(
            "openai_key: {} ({})",
            env_presence(&config.providers.openai.api_key_env),
            config.providers.openai.api_key_env
        ),
        format!(
            "google_key: {} ({})",
            env_presence(&config.providers.google.api_key_env),
            config.providers.google.api_key_env
        ),
        format!("docker: {}", docker_status().await),
        format!("disk: {}", disk_status(config).await),
        format!("database: {database_line}"),
        format!(
            "orchestrator: {} ({})",
            orchestrator_status(config).await,
            daemon::orchestrator_endpoint(config)
        ),
        format!("graph_memory: {}", graph_memory_status(config).await),
        format!("lineage: {}", lineage_status(config).await),
        format!(
            "log_file: {}{}",
            log_path.display(),
            if cfg!(debug_assertions) || std::env::var_os("RUST_LOG").is_some() {
                " (set via --debug/--log-file or RUST_LOG)"
            } else {
                " (--debug to enable)"
            }
        ),
    ];
    lines.push(doctor_metrics(config).await);

    Ok(lines.join("\n") + "\n")
}

pub(crate) async fn orchestrator_status(config: &MoaConfig) -> String {
    match daemon::health_check(config).await {
        Ok(()) => "healthy".to_string(),
        Err(error) => format!("unavailable ({error})"),
    }
}

pub(crate) async fn doctor_metrics(config: &MoaConfig) -> String {
    if !config.metrics.enabled {
        return "Metrics endpoint: disabled".to_string();
    }

    let Some(url) = metrics_endpoint_url(&config.metrics) else {
        return format!(
            "Metrics endpoint: invalid listen address `{}`",
            config.metrics.listen
        );
    };

    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return format!("Metrics endpoint: {url} - unavailable");
    };

    match client.get(&url).send().await {
        Ok(response) if response.status().is_success() => format!("Metrics endpoint: {url} - OK"),
        Ok(response) => {
            format!(
                "Metrics endpoint: {url} - HTTP {}",
                response.status().as_u16()
            )
        }
        Err(_) => format!("Metrics endpoint: {url} - unavailable"),
    }
}

pub(crate) async fn lineage_status(config: &MoaConfig) -> String {
    if !config.observability.lineage.enabled {
        return "disabled".to_string();
    }

    match load_session_store(config).await {
        Ok(store) => {
            match sqlx::query_scalar::<_, i64>("SELECT count(*) FROM analytics.turn_lineage")
                .fetch_one(store.pool())
                .await
            {
                Ok(count) => format!(
                    "enabled rows={} journal={}",
                    count, config.observability.lineage.journal_path
                ),
                Err(error) => format!("enabled schema_unavailable ({error})"),
            }
        }
        Err(error) => format!("enabled database_unavailable ({error})"),
    }
}

pub(crate) async fn docker_status() -> String {
    match timeout(
        std::time::Duration::from_secs(5),
        Command::new("docker").arg("info").output(),
    )
    .await
    {
        Err(_) => "unavailable (timed out)".to_string(),
        Ok(Ok(output)) if output.status.success() => "available".to_string(),
        Ok(Ok(output)) => format!("unhealthy (exit {})", output.status),
        Ok(Err(_)) => "missing".to_string(),
    }
}

pub(crate) async fn disk_status(config: &MoaConfig) -> String {
    let target = expand_tilde(&config.local.sandbox_dir);
    match Command::new("df").arg("-k").arg(&target).output().await {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.lines()
                .nth(1)
                .map(str::trim)
                .unwrap_or("available")
                .to_string()
        }
        Ok(output) => format!("unhealthy (exit {})", output.status),
        Err(error) => format!("unavailable ({error})"),
    }
}

pub(crate) async fn doctor_database(config: &MoaConfig) -> String {
    match load_session_store(config).await {
        Ok(store) => {
            let version = sqlx::query_scalar::<_, String>("SELECT version()")
                .fetch_one(store.pool())
                .await;
            let pgvector = sqlx::query_scalar::<_, String>(
                "SELECT extversion FROM pg_extension WHERE extname = 'vector'",
            )
            .fetch_optional(store.pool())
            .await;

            match (version, pgvector) {
                (Ok(version), Ok(pgvector)) => format!(
                    "{}; pgvector={}",
                    version.lines().next().unwrap_or("unknown"),
                    pgvector.unwrap_or_else(|| "NOT INSTALLED".to_string())
                ),
                (Err(error), _) => format!("unhealthy ({error})"),
                (_, Err(error)) => format!("pgvector check failed ({error})"),
            }
        }
        Err(error) => format!("unhealthy ({error})"),
    }
}

pub(crate) async fn graph_memory_status(config: &MoaConfig) -> String {
    let workspace_id = current_workspace_id();
    match load_session_store(config).await {
        Ok(store) => {
            let status = sqlx::query_as::<_, (i64, Option<String>)>(
                r#"
                SELECT count(*)::bigint, max(created_at)::text
                FROM moa.node_index
                WHERE workspace_id = $1
                  AND valid_to IS NULL
                "#,
            )
            .bind(workspace_id.as_str())
            .fetch_one(store.pool())
            .await;

            match status {
                Ok((count, Some(last_write))) => {
                    format!("healthy ({count} nodes in current workspace; last_write={last_write})")
                }
                Ok((count, None)) => {
                    format!("healthy ({count} nodes in current workspace; last_write=none)")
                }
                Err(error) => format!("unhealthy ({error})"),
            }
        }
        Err(error) => format!("unhealthy ({error})"),
    }
}
