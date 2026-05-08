//! Configuration loading and persistence.

use std::path::{Path, PathBuf};

use config::{Config, Environment, File};

use crate::error::{MoaError, Result};

use super::{MoaConfig, config_parent_dir};

impl MoaConfig {
    /// Loads configuration from `~/.moa/config.toml` and environment variables.
    pub fn load() -> Result<Self> {
        Self::load_from_path(Self::default_path()?)
    }

    /// Returns the default MOA config file path.
    pub fn default_path() -> Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(MoaError::HomeDirectoryNotFound)?;
        Ok(home.join(".moa").join("config.toml"))
    }

    /// Loads configuration from an explicit TOML file path and environment variables.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let builder = Config::builder()
            .set_default(
                "general.default_provider",
                Self::default().general.default_provider,
            )?
            .set_default("models.main", Self::default().models.main.clone())?
            .set_default("models.auxiliary", Self::default().models.auxiliary.clone())?
            .set_default(
                "general.reasoning_effort",
                Self::default().general.reasoning_effort,
            )?
            .set_default(
                "providers.anthropic.api_key_env",
                Self::default().providers.anthropic.api_key_env,
            )?
            .set_default(
                "providers.openai.api_key_env",
                Self::default().providers.openai.api_key_env,
            )?
            .set_default(
                "providers.google.api_key_env",
                Self::default().providers.google.api_key_env,
            )?
            .set_default("database.url", Self::default().database.url)?
            .set_default("database.admin_url", Self::default().database.admin_url)?
            .set_default(
                "database.max_connections",
                Self::default().database.max_connections as i64,
            )?
            .set_default(
                "database.connect_timeout_seconds",
                Self::default().database.connect_timeout_seconds as i64,
            )?
            .set_default(
                "database.neon.enabled",
                Self::default().database.neon.enabled,
            )?
            .set_default(
                "database.neon.api_key_env",
                Self::default().database.neon.api_key_env,
            )?
            .set_default(
                "database.neon.project_id",
                Self::default().database.neon.project_id,
            )?
            .set_default(
                "database.neon.parent_branch_id",
                Self::default().database.neon.parent_branch_id,
            )?
            .set_default(
                "database.neon.max_checkpoints",
                Self::default().database.neon.max_checkpoints as i64,
            )?
            .set_default(
                "database.neon.checkpoint_ttl_hours",
                Self::default().database.neon.checkpoint_ttl_hours as i64,
            )?
            .set_default("database.neon.pooled", Self::default().database.neon.pooled)?
            .set_default(
                "database.neon.suspend_timeout_seconds",
                Self::default().database.neon.suspend_timeout_seconds as i64,
            )?
            .set_default("local.docker_enabled", Self::default().local.docker_enabled)?
            .set_default("local.sandbox_dir", Self::default().local.sandbox_dir)?
            .set_default("local.memory_dir", Self::default().local.memory_dir)?
            .set_default(
                "memory.auto_bootstrap",
                Self::default().memory.auto_bootstrap,
            )?
            .set_default(
                "memory.embedding_provider",
                Self::default().memory.embedding_provider,
            )?
            .set_default(
                "memory.embedding_model",
                Self::default().memory.embedding_model,
            )?
            .set_default(
                "memory.vector.embedder.name",
                Self::default().memory.vector.embedder.name,
            )?
            .set_default(
                "memory.vector.embedder.output_dim",
                Self::default().memory.vector.embedder.output_dim as i64,
            )?
            .set_default(
                "memory.vector.embedder.cohere.api_key_env",
                Self::default().memory.vector.embedder.cohere.api_key_env,
            )?
            .set_default(
                "memory.vector.embedder.gemini.api_key_env",
                Self::default().memory.vector.embedder.gemini.api_key_env,
            )?
            .set_default(
                "memory.vector.embedder.gemini.default_role",
                Self::default().memory.vector.embedder.gemini.default_role,
            )?
            .set_default("daemon.socket_path", Self::default().daemon.socket_path)?
            .set_default("daemon.pid_file", Self::default().daemon.pid_file)?
            .set_default("daemon.log_file", Self::default().daemon.log_file)?
            .set_default("daemon.auto_connect", Self::default().daemon.auto_connect)?
            .set_default(
                "orchestrator.endpoint",
                Self::default().orchestrator.endpoint,
            )?
            .set_default(
                "orchestrator.health_url",
                Self::default().orchestrator.health_url,
            )?
            .set_default(
                "session.blob_threshold_bytes",
                Self::default().session.blob_threshold_bytes as i64,
            )?
            .set_default("session.blob_dir", Self::default().session.blob_dir)?
            .set_default(
                "observability.enabled",
                Self::default().observability.enabled,
            )?
            .set_default(
                "observability.service_name",
                Self::default().observability.service_name,
            )?
            .set_default(
                "observability.otlp_endpoint",
                Self::default().observability.otlp_endpoint,
            )?
            .set_default(
                "observability.otlp_protocol",
                Self::default().observability.otlp_protocol.as_str(),
            )?
            .set_default(
                "observability.environment",
                Self::default().observability.environment,
            )?
            .set_default(
                "observability.release",
                Self::default().observability.release,
            )?
            .set_default(
                "observability.sample_rate",
                Self::default().observability.sample_rate,
            )?
            .set_default(
                "observability.lineage.enabled",
                Self::default().observability.lineage.enabled,
            )?
            .set_default(
                "observability.lineage.channel_capacity",
                Self::default().observability.lineage.channel_capacity as i64,
            )?
            .set_default(
                "observability.lineage.batch_size",
                Self::default().observability.lineage.batch_size as i64,
            )?
            .set_default(
                "observability.lineage.batch_max_age_secs",
                Self::default().observability.lineage.batch_max_age_secs as i64,
            )?
            .set_default(
                "observability.lineage.journal_path",
                Self::default().observability.lineage.journal_path,
            )?
            .set_default(
                "observability.lineage.sample_pgvector_explain",
                Self::default()
                    .observability
                    .lineage
                    .sample_pgvector_explain,
            )?
            .set_default("metrics.enabled", Self::default().metrics.enabled)?
            .set_default("metrics.listen", Self::default().metrics.listen.clone())?
            .set_default(
                "budgets.daily_workspace_cents",
                Self::default().budgets.daily_workspace_cents as i64,
            )?
            .set_default(
                "session_limits.max_turns",
                Self::default().session_limits.max_turns as i64,
            )?
            .set_default(
                "session_limits.loop_detection_threshold",
                Self::default().session_limits.loop_detection_threshold as i64,
            )?
            .set_default(
                "tool_output.max_replay_chars",
                Self::default().tool_output.max_replay_chars as i64,
            )?
            .set_default(
                "tool_output.max_bash_lines",
                Self::default().tool_output.max_bash_lines as i64,
            )?
            .set_default(
                "tool_output.head_ratio",
                Self::default().tool_output.head_ratio,
            )?
            .set_default(
                "tool_budgets.file_read",
                Self::default().tool_budgets.file_read as i64,
            )?
            .set_default(
                "tool_budgets.bash_stdout",
                Self::default().tool_budgets.bash_stdout as i64,
            )?
            .set_default(
                "tool_budgets.bash_stderr",
                Self::default().tool_budgets.bash_stderr as i64,
            )?
            .set_default(
                "tool_budgets.grep",
                Self::default().tool_budgets.grep as i64,
            )?
            .set_default(
                "tool_budgets.file_search",
                Self::default().tool_budgets.file_search as i64,
            )?
            .set_default(
                "tool_budgets.memory_search",
                Self::default().tool_budgets.memory_search as i64,
            )?
            .set_default(
                "tool_budgets.file_outline",
                Self::default().tool_budgets.file_outline as i64,
            )?
            .set_default(
                "tool_budgets.default",
                Self::default().tool_budgets.default as i64,
            )?
            .set_default(
                "skill_budget.max_manifest_chars",
                Self::default()
                    .skill_budget
                    .max_manifest_chars
                    .map(|value| value as i64),
            )?
            .set_default(
                "skill_budget.max_per_skill_chars",
                Self::default().skill_budget.max_per_skill_chars as i64,
            )?
            .set_default(
                "skill_budget.show_token_estimates",
                Self::default().skill_budget.show_token_estimates,
            )?
            .set_default(
                "query_rewrite.enabled",
                Self::default().query_rewrite.enabled,
            )?
            .set_default(
                "query_rewrite.model",
                Self::default().query_rewrite.model.clone(),
            )?
            .set_default(
                "query_rewrite.timeout_ms",
                Self::default().query_rewrite.timeout_ms as i64,
            )?
            .set_default(
                "query_rewrite.min_query_tokens",
                Self::default().query_rewrite.min_query_tokens as i64,
            )?
            .set_default(
                "query_rewrite.skip_single_turn",
                Self::default().query_rewrite.skip_single_turn,
            )?
            .set_default(
                "query_rewrite.circuit_breaker_threshold",
                Self::default().query_rewrite.circuit_breaker_threshold,
            )?
            .set_default(
                "query_rewrite.circuit_breaker_window_secs",
                Self::default().query_rewrite.circuit_breaker_window_secs as i64,
            )?
            .set_default(
                "query_rewrite.circuit_breaker_cooldown_secs",
                Self::default().query_rewrite.circuit_breaker_cooldown_secs as i64,
            )?
            .set_default("resolution.enabled", Self::default().resolution.enabled)?
            .set_default(
                "resolution.weights.tool",
                Self::default().resolution.weights.tool,
            )?
            .set_default(
                "resolution.weights.verification",
                Self::default().resolution.weights.verification,
            )?
            .set_default(
                "resolution.weights.continuation",
                Self::default().resolution.weights.continuation,
            )?
            .set_default(
                "resolution.weights.self_assessment",
                Self::default().resolution.weights.self_assessment,
            )?
            .set_default(
                "resolution.weights.structural",
                Self::default().resolution.weights.structural,
            )?
            .set_default(
                "resolution.use_llm_self_assessment",
                Self::default().resolution.use_llm_self_assessment,
            )?
            .set_default(
                "resolution.self_assessment_timeout_ms",
                Self::default().resolution.self_assessment_timeout_ms as i64,
            )?
            .set_default(
                "resolution.rephrase_similarity_threshold",
                Self::default().resolution.rephrase_similarity_threshold,
            )?
            .set_default(
                "resolution.structural_min_samples",
                Self::default().resolution.structural_min_samples as i64,
            )?
            .set_default(
                "resolution.idle_timeout_minutes",
                Self::default().resolution.idle_timeout_minutes as i64,
            )?
            .set_default("intents.enabled", Self::default().intents.enabled)?
            .set_default(
                "intents.discovery_interval_hours",
                Self::default().intents.discovery_interval_hours as i64,
            )?
            .set_default(
                "intents.discovery_window_days",
                Self::default().intents.discovery_window_days as i64,
            )?
            .set_default(
                "intents.min_segments_for_discovery",
                Self::default().intents.min_segments_for_discovery as i64,
            )?
            .set_default(
                "intents.min_cluster_size",
                Self::default().intents.min_cluster_size as i64,
            )?
            .set_default(
                "intents.classification_threshold",
                Self::default().intents.classification_threshold,
            )?
            .set_default(
                "intents.retroactive_threshold",
                Self::default().intents.retroactive_threshold,
            )?
            .set_default(
                "intents.medium_confidence_threshold",
                Self::default().intents.medium_confidence_threshold,
            )?
            .set_default(
                "intents.deprecation_after_days",
                Self::default().intents.deprecation_after_days as i64,
            )?
            .set_default(
                "context_snapshot.enabled",
                Self::default().context_snapshot.enabled,
            )?
            .set_default(
                "context_snapshot.max_size_bytes",
                Self::default().context_snapshot.max_size_bytes as i64,
            )?
            .set_default("cloud.enabled", Self::default().cloud.enabled)?
            .set_default("cloud.memory_dir", Self::default().cloud.memory_dir.clone())?
            .set_default(
                "cloud.flyio.api_token_env",
                Self::default()
                    .cloud
                    .flyio
                    .as_ref()
                    .and_then(|config| config.api_token_env.clone()),
            )?
            .set_default(
                "cloud.flyio.app_name",
                Self::default()
                    .cloud
                    .flyio
                    .as_ref()
                    .and_then(|config| config.app_name.clone()),
            )?
            .set_default(
                "cloud.flyio.region",
                Self::default()
                    .cloud
                    .flyio
                    .as_ref()
                    .map(|config| config.region.clone()),
            )?
            .set_default(
                "cloud.flyio.internal_port",
                Self::default()
                    .cloud
                    .flyio
                    .as_ref()
                    .map(|config| config.internal_port as i64),
            )?
            .set_default(
                "cloud.flyio.health_bind",
                Self::default()
                    .cloud
                    .flyio
                    .as_ref()
                    .map(|config| config.health_bind.clone()),
            )?
            .set_default(
                "cloud.flyio.graceful_shutdown_timeout_secs",
                Self::default()
                    .cloud
                    .flyio
                    .as_ref()
                    .map(|config| config.graceful_shutdown_timeout_secs as i64),
            )?
            .set_default(
                "cloud.hands.default_provider",
                Self::default()
                    .cloud
                    .hands
                    .as_ref()
                    .and_then(|config| config.default_provider.clone()),
            )?
            .set_default(
                "cloud.hands.daytona_api_key_env",
                Self::default()
                    .cloud
                    .hands
                    .as_ref()
                    .and_then(|config| config.daytona_api_key_env.clone()),
            )?
            .set_default(
                "cloud.hands.daytona_api_url",
                Self::default()
                    .cloud
                    .hands
                    .as_ref()
                    .and_then(|config| config.daytona_api_url.clone()),
            )?
            .set_default(
                "cloud.hands.daytona_default_image",
                Self::default()
                    .cloud
                    .hands
                    .as_ref()
                    .and_then(|config| config.daytona_default_image.clone()),
            )?
            .set_default(
                "cloud.hands.e2b_api_key_env",
                Self::default()
                    .cloud
                    .hands
                    .as_ref()
                    .and_then(|config| config.e2b_api_key_env.clone()),
            )?
            .set_default(
                "cloud.hands.e2b_api_url",
                Self::default()
                    .cloud
                    .hands
                    .as_ref()
                    .and_then(|config| config.e2b_api_url.clone()),
            )?
            .set_default(
                "cloud.hands.e2b_domain",
                Self::default()
                    .cloud
                    .hands
                    .as_ref()
                    .and_then(|config| config.e2b_domain.clone()),
            )?
            .set_default(
                "cloud.hands.e2b_template",
                Self::default()
                    .cloud
                    .hands
                    .as_ref()
                    .and_then(|config| config.e2b_template.clone()),
            )?
            .set_default(
                "gateway.telegram_token_env",
                Self::default().gateway.telegram_token_env,
            )?
            .set_default(
                "gateway.slack_token_env",
                Self::default().gateway.slack_token_env,
            )?
            .set_default(
                "gateway.slack_app_token_env",
                Self::default().gateway.slack_app_token_env,
            )?
            .set_default(
                "gateway.discord_token_env",
                Self::default().gateway.discord_token_env,
            )?
            .set_default(
                "permissions.default_posture",
                Self::default().permissions.default_posture,
            )?
            .set_default(
                "permissions.auto_approve",
                Self::default().permissions.auto_approve,
            )?
            .set_default(
                "permissions.always_deny",
                Self::default().permissions.always_deny,
            )?
            .set_default("compaction.enabled", Self::default().compaction.enabled)?
            .set_default(
                "compaction.event_threshold",
                Self::default().compaction.event_threshold as i64,
            )?
            .set_default(
                "compaction.token_ratio_threshold",
                Self::default().compaction.token_ratio_threshold,
            )?
            .set_default(
                "compaction.recent_turns_verbatim",
                Self::default().compaction.recent_turns_verbatim as i64,
            )?
            .set_default(
                "compaction.preserve_errors",
                Self::default().compaction.preserve_errors,
            )?
            .set_default(
                "compaction.tier2_trigger_blocks_past_bp4",
                Self::default().compaction.tier2_trigger_blocks_past_bp4 as i64,
            )?
            .set_default(
                "compaction.tier3_trigger_fraction",
                Self::default().compaction.tier3_trigger_fraction,
            )?
            .set_default(
                "compaction.max_input_tokens_per_turn",
                Self::default().compaction.max_input_tokens_per_turn as i64,
            )?
            .add_source(File::from(path).required(false))
            .add_source(Environment::with_prefix("MOA").separator("__"));

        let config: Self = builder.build()?.try_deserialize()?;
        config.validate()?;
        Ok(config)
    }

    /// Persists this config to the default MOA config path.
    ///
    /// This is a synchronous operation. Prefer [`save_async`][Self::save_async] when calling
    /// from an async context to avoid blocking the executor.
    pub fn save(&self) -> Result<()> {
        self.save_to_path(Self::default_path()?)
    }

    /// Persists this config to an explicit TOML file path.
    ///
    /// This is a synchronous operation. Prefer [`save_to_path_async`][Self::save_to_path_async]
    /// when calling from an async context to avoid blocking the executor.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = config_parent_dir(path) {
            std::fs::create_dir_all(parent)?;
        }
        let content = self.serialize_config()?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Persists this config to the default MOA config path using async I/O.
    pub async fn save_async(&self) -> Result<()> {
        self.save_to_path_async(Self::default_path()?).await
    }

    /// Persists this config to an explicit TOML file path using async I/O.
    pub async fn save_to_path_async(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = config_parent_dir(path) {
            tokio::fs::create_dir_all(parent).await?;
        }
        let content = self.serialize_config()?;
        tokio::fs::write(path, content).await?;
        Ok(())
    }
}
