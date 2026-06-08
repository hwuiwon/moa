//! Shared CLI support helpers.

use super::*;

pub(crate) async fn load_session_store(config: &MoaConfig) -> Result<Arc<PostgresSessionStore>> {
    create_session_store(config)
        .await
        .context("opening session store")
}

pub(crate) async fn load_graph_store(config: &MoaConfig) -> Result<AgeGraphStore> {
    let session_store = load_session_store(config).await?;
    let scope = ScopeContext::workspace(current_workspace_id());
    Ok(AgeGraphStore::scoped(session_store.pool().clone(), scope))
}

pub(crate) async fn load_hybrid_retriever(config: &MoaConfig) -> Result<HybridRetriever> {
    let session_store = load_session_store(config).await?;
    let pool = session_store.pool().clone();
    let scope = ScopeContext::workspace(current_workspace_id());
    let vector = Arc::new(PgvectorStore::new(pool.clone(), scope.clone()));
    let graph = AgeGraphStore::scoped(pool.clone(), scope).with_vector_store(vector.clone());
    Ok(HybridRetriever::from_env(pool, Arc::new(graph), vector))
}

pub(crate) async fn load_ingestion_vo(config: &MoaConfig) -> Result<CliIngestionVo> {
    let session_store = load_session_store(config).await?;
    Ok(CliIngestionVo::new(session_store.pool().clone()))
}

pub(crate) struct CliIngestionVo {
    pool: sqlx::PgPool,
}

impl CliIngestionVo {
    pub(crate) fn new(pool: sqlx::PgPool) -> Self {
        Self { pool }
    }

    pub(crate) async fn ingest_turn(&self, turn: SessionTurn) -> Result<IngestApplyReport> {
        let _ = memory_ingest::install_runtime_with_pool(self.pool.clone());
        memory_ingest::ingest_turn_direct(turn)
            .await
            .map_err(|error| anyhow::anyhow!("{error:?}"))
    }
}

pub(crate) fn load_branch_manager(config: &MoaConfig) -> Result<NeonBranchManager> {
    NeonBranchManager::from_config(config).context("opening Neon branch manager")
}

pub(crate) fn resolve_workspace_arg(value: &str) -> WorkspaceId {
    if value == "." {
        return current_workspace_id();
    }

    WorkspaceId::new(value)
}

pub(crate) fn current_workspace_id() -> WorkspaceId {
    let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let name = cwd
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("default");
    WorkspaceId::new(name)
}

pub(crate) fn current_user_id() -> UserId {
    UserId::new(
        env::var("USER")
            .or_else(|_| env::var("USERNAME"))
            .unwrap_or_else(|_| "local-user".to_string()),
    )
}

pub(crate) fn format_cents(cost_cents: u64) -> String {
    format!("${:.2}", cost_cents as f64 / 100.0)
}

pub(crate) fn apply_config_update(config: &mut MoaConfig, key: &str, value: &str) -> Result<()> {
    match key {
        "general.default_provider" => config.general.default_provider = value.to_string(),
        "models.main" => {
            config.models.main = value.to_string();
        }
        "models.auxiliary" => {
            config.models.auxiliary = (!value.trim().is_empty()).then(|| value.to_string());
        }
        "general.reasoning_effort" => config.general.reasoning_effort = value.to_string(),
        "cloud.enabled" => config.cloud.enabled = parse_bool(value)?,
        "cloud.memory_dir" => config.cloud.memory_dir = Some(value.to_string()),
        "local.docker_enabled" => config.local.docker_enabled = parse_bool(value)?,
        "local.sandbox_dir" => config.local.sandbox_dir = value.to_string(),
        "memory.embedding_provider" => config.memory.embedding_provider = value.to_string(),
        "memory.embedding_model" => config.memory.embedding_model = value.to_string(),
        "memory.vector.embedder.name" => config.memory.vector.embedder.name = value.to_string(),
        "memory.vector.embedder.output_dim" => {
            config.memory.vector.embedder.output_dim =
                value.parse().context("expected integer output dimension")?;
        }
        "memory.vector.embedder.cohere.api_key_env" => {
            config.memory.vector.embedder.cohere.api_key_env = value.to_string();
        }
        "memory.vector.embedder.gemini.api_key_env" => {
            config.memory.vector.embedder.gemini.api_key_env = value.to_string();
        }
        "memory.vector.embedder.gemini.default_role" => {
            config.memory.vector.embedder.gemini.default_role = value.to_string();
        }
        "database.url" => config.database.url = value.to_string(),
        "database.admin_url" => config.database.admin_url = Some(value.to_string()),
        "database.max_connections" => {
            config.database.max_connections =
                value.parse().context("expected integer pool size")?;
        }
        "database.connect_timeout_seconds" => {
            config.database.connect_timeout_seconds =
                value.parse().context("expected integer timeout")?;
        }
        "database.neon.enabled" => config.database.neon.enabled = parse_bool(value)?,
        "database.neon.api_key_env" => config.database.neon.api_key_env = value.to_string(),
        "database.neon.project_id" => config.database.neon.project_id = value.to_string(),
        "database.neon.parent_branch_id" => {
            config.database.neon.parent_branch_id = value.to_string();
        }
        "database.neon.max_checkpoints" => {
            config.database.neon.max_checkpoints =
                value.parse().context("expected integer checkpoint count")?;
        }
        "database.neon.checkpoint_ttl_hours" => {
            config.database.neon.checkpoint_ttl_hours = value
                .parse()
                .context("expected integer checkpoint ttl hours")?;
        }
        "database.neon.pooled" => config.database.neon.pooled = parse_bool(value)?,
        "database.neon.suspend_timeout_seconds" => {
            config.database.neon.suspend_timeout_seconds =
                value.parse().context("expected integer suspend timeout")?;
        }
        "local.memory_dir" => config.local.memory_dir = value.to_string(),
        "orchestrator.endpoint" => config.orchestrator.endpoint = Some(value.to_string()),
        "orchestrator.health_url" => {
            config.orchestrator.health_url = (!value.trim().is_empty()).then(|| value.to_string());
        }
        "observability.enabled" => config.observability.enabled = parse_bool(value)?,
        "observability.service_name" => config.observability.service_name = value.to_string(),
        "observability.otlp_endpoint" => {
            config.observability.otlp_endpoint = Some(value.to_string());
        }
        "observability.otlp_protocol" => {
            config.observability.otlp_protocol = parse_otlp_protocol(value)?;
        }
        "observability.environment" => config.observability.environment = Some(value.to_string()),
        "observability.release" => config.observability.release = Some(value.to_string()),
        "observability.sample_rate" => {
            config.observability.sample_rate =
                value.parse().context("expected decimal sample rate")?;
        }
        "metrics.enabled" => config.metrics.enabled = parse_bool(value)?,
        "metrics.listen" => config.metrics.listen = value.to_string(),
        _ => bail!("unsupported config key: {key}"),
    }

    Ok(())
}
pub(crate) fn parse_otlp_protocol(value: &str) -> Result<OtlpProtocol> {
    match value.trim().to_ascii_lowercase().as_str() {
        "grpc" => Ok(OtlpProtocol::Grpc),
        "http" => Ok(OtlpProtocol::Http),
        _ => bail!("expected `grpc` or `http`, got {value}"),
    }
}

pub(crate) fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => bail!("expected boolean value, got {value}"),
    }
}

pub(crate) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(relative);
    }
    PathBuf::from(path)
}
