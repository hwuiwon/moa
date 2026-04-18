//! Configuration loading and defaults for MOA.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use config::{Config, Environment, File};
use serde::{Deserialize, Serialize};

use crate::error::{MoaError, Result};

/// Top-level MOA configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MoaConfig {
    /// General runtime settings.
    pub general: GeneralConfig,
    /// Tiered model-routing settings.
    pub models: ModelsConfig,
    /// Provider settings.
    pub providers: ProvidersConfig,
    /// Session database settings.
    pub database: DatabaseConfig,
    /// Local runtime settings.
    pub local: LocalConfig,
    /// Memory bootstrap and maintenance settings.
    pub memory: MemoryConfig,
    /// Cloud runtime settings.
    pub cloud: CloudConfig,
    /// Messaging gateway settings.
    pub gateway: GatewayConfig,
    /// Desktop application settings.
    pub desktop: DesktopConfig,
    /// Permission policy settings.
    pub permissions: PermissionsConfig,
    /// Session storage settings.
    pub session: SessionConfig,
    /// Session-history compaction settings.
    pub compaction: CompactionConfig,
    /// Local daemon settings.
    pub daemon: DaemonConfig,
    /// Observability and OTLP export settings.
    pub observability: ObservabilityConfig,
    /// Prometheus metrics export settings.
    pub metrics: MetricsConfig,
    /// Workspace budget enforcement settings.
    pub budgets: BudgetConfig,
    /// Per-session turn and loop guardrails.
    pub session_limits: SessionLimitsConfig,
    /// Tool-output truncation settings for storage and replay.
    pub tool_output: ToolOutputConfig,
    /// Per-tool router-level output budgets enforced before event persistence.
    pub tool_budgets: ToolBudgetConfig,
    /// Incremental context snapshot settings.
    pub context_snapshot: ContextSnapshotConfig,
    /// External MCP server connections.
    pub mcp_servers: Vec<McpServerConfig>,
}

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
            .set_default(
                "general.default_model",
                Self::default().general.default_model,
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
            .set_default("daemon.socket_path", Self::default().daemon.socket_path)?
            .set_default("daemon.pid_file", Self::default().daemon.pid_file)?
            .set_default("daemon.log_file", Self::default().daemon.log_file)?
            .set_default("daemon.auto_connect", Self::default().daemon.auto_connect)?
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
                "cloud.temporal.address",
                Self::default()
                    .cloud
                    .temporal
                    .as_ref()
                    .and_then(|config| config.address.clone()),
            )?
            .set_default(
                "cloud.temporal.namespace",
                Self::default()
                    .cloud
                    .temporal
                    .as_ref()
                    .and_then(|config| config.namespace.clone()),
            )?
            .set_default(
                "cloud.temporal.task_queue",
                Self::default()
                    .cloud
                    .temporal
                    .as_ref()
                    .map(|config| config.task_queue.clone()),
            )?
            .set_default(
                "cloud.temporal.api_key_env",
                Self::default()
                    .cloud
                    .temporal
                    .as_ref()
                    .and_then(|config| config.api_key_env.clone()),
            )?
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
            .set_default("desktop.theme", Self::default().desktop.theme)?
            .set_default("desktop.sidebar_auto", Self::default().desktop.sidebar_auto)?
            .set_default(
                "desktop.tab_limit",
                Self::default().desktop.tab_limit as i64,
            )?
            .set_default("desktop.diff_style", Self::default().desktop.diff_style)?
            .set_default("desktop.density", Self::default().desktop.density)?
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

        let mut config: Self = builder.build()?.try_deserialize()?;
        config.general.default_model = config.models.main.clone();
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

impl MoaConfig {
    fn serialize_config(&self) -> Result<String> {
        let mut config = self.clone();
        config.models.main = config.general.default_model.clone();
        toml::to_string_pretty(&config).map_err(|error| MoaError::ConfigError(error.to_string()))
    }

    fn validate(&self) -> Result<()> {
        if self.database.url.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "database.url is required and must point to a reachable Postgres instance"
                    .to_string(),
            ));
        }

        if self.database.neon.enabled && self.database.neon.max_checkpoints == 0 {
            return Err(MoaError::ConfigError(
                "database.neon.max_checkpoints must be greater than zero when Neon checkpointing is enabled"
                    .to_string(),
            ));
        }

        Ok(())
    }
}

impl MoaConfig {
    /// Returns the configured model identifier for one routing task.
    #[must_use]
    pub fn model_for_task(&self, task: crate::ModelTask) -> &str {
        match task {
            crate::ModelTask::MainLoop => self.models.main.as_str(),
            crate::ModelTask::Summarization
            | crate::ModelTask::Consolidation
            | crate::ModelTask::SkillDistillation
            | crate::ModelTask::Subagent => self
                .models
                .auxiliary
                .as_deref()
                .unwrap_or(self.models.main.as_str()),
        }
    }

    /// Sets the configured main-loop provider/model pair and mirrors it into routing config.
    pub fn set_main_model(&mut self, provider: impl Into<String>, model: impl Into<String>) {
        let provider = provider.into();
        let model = model.into();
        self.general.default_provider = provider;
        self.general.default_model = model.clone();
        self.models.main = model;
    }
}

fn config_parent_dir(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
}

/// General runtime settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Default provider key.
    pub default_provider: String,
    /// Default model identifier.
    pub default_model: String,
    /// Requested reasoning effort.
    pub reasoning_effort: String,
    /// Whether provider-native web search should be offered to supported models.
    pub web_search_enabled: bool,
    /// Optional workspace-level instructions injected into the prompt.
    pub workspace_instructions: Option<String>,
    /// Optional user-level preferences injected into the prompt.
    pub user_instructions: Option<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".to_string(),
            default_model: "gpt-5.4".to_string(),
            reasoning_effort: "medium".to_string(),
            web_search_enabled: true,
            workspace_instructions: None,
            user_instructions: None,
        }
    }
}

/// Tiered model-routing settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    /// Default model for the primary user-facing agent loop.
    pub main: String,
    /// Optional lower-cost model for auxiliary tasks.
    pub auxiliary: Option<String>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            main: GeneralConfig::default().default_model,
            auxiliary: None,
        }
    }
}

/// Provider credential environment mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderCredentialConfig {
    /// Environment variable containing the API key.
    pub api_key_env: String,
}

impl ProviderCredentialConfig {
    /// Creates a provider credential config with a single environment variable name.
    pub fn new(api_key_env: impl Into<String>) -> Self {
        Self {
            api_key_env: api_key_env.into(),
        }
    }
}

impl Default for ProviderCredentialConfig {
    fn default() -> Self {
        Self::new("")
    }
}

/// Provider-specific configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    /// Anthropic credentials.
    pub anthropic: ProviderCredentialConfig,
    /// `OpenAI` credentials.
    pub openai: ProviderCredentialConfig,
    /// Google Gemini credentials.
    pub google: ProviderCredentialConfig,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            anthropic: ProviderCredentialConfig::new("ANTHROPIC_API_KEY"),
            openai: ProviderCredentialConfig::new("OPENAI_API_KEY"),
            google: ProviderCredentialConfig::new("GOOGLE_API_KEY"),
        }
    }
}

/// Session database configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Runtime Postgres connection URL.
    pub url: String,
    /// Optional direct/admin database URL for migrations and other session-sensitive flows.
    pub admin_url: Option<String>,
    /// Maximum pool size for the shared Postgres client.
    pub max_connections: u32,
    /// Connection timeout in seconds.
    pub connect_timeout_seconds: u64,
    /// Optional Neon branching configuration for ephemeral checkpoints.
    pub neon: DatabaseNeonConfig,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "postgres://moa:moa@localhost:5432/moa".to_string(),
            admin_url: None,
            max_connections: 20,
            connect_timeout_seconds: 10,
            neon: DatabaseNeonConfig::default(),
        }
    }
}

impl DatabaseConfig {
    /// Returns the configured runtime database URL.
    pub fn runtime_url(&self) -> &str {
        &self.url
    }

    /// Returns the direct/admin database URL, falling back to the runtime URL when unset.
    pub fn admin_url(&self) -> &str {
        self.admin_url.as_deref().unwrap_or(&self.url)
    }
}

/// Optional Neon branching configuration for ephemeral database checkpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseNeonConfig {
    /// Whether Neon checkpoint management is enabled.
    pub enabled: bool,
    /// Environment variable containing the Neon API key.
    pub api_key_env: String,
    /// Neon project identifier used for branch management.
    pub project_id: String,
    /// Parent branch name or id used for checkpoint creation.
    pub parent_branch_id: String,
    /// Maximum number of active MOA checkpoint branches.
    pub max_checkpoints: usize,
    /// TTL for automatic checkpoint cleanup, in hours.
    pub checkpoint_ttl_hours: u64,
    /// Whether pooled connection URIs should be requested for checkpoint branches.
    pub pooled: bool,
    /// Auto-suspend timeout in seconds for checkpoint endpoints.
    pub suspend_timeout_seconds: u64,
}

impl Default for DatabaseNeonConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key_env: "NEON_API_KEY".to_string(),
            project_id: String::new(),
            parent_branch_id: "main".to_string(),
            max_checkpoints: 5,
            checkpoint_ttl_hours: 24,
            pooled: true,
            suspend_timeout_seconds: 300,
        }
    }
}

/// Local runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalConfig {
    /// Whether local Docker hands are enabled.
    pub docker_enabled: bool,
    /// Sandbox working directory.
    pub sandbox_dir: String,
    /// Memory root directory.
    pub memory_dir: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            docker_enabled: true,
            sandbox_dir: "~/.moa/sandbox".to_string(),
            memory_dir: "~/.moa/memory".to_string(),
        }
    }
}

/// Memory bootstrap and maintenance configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Automatically bootstrap workspace memory when it is empty.
    pub auto_bootstrap: bool,
    /// Embedding provider used for semantic wiki search. Set to `disabled` to turn it off.
    pub embedding_provider: String,
    /// Embedding model identifier used for semantic wiki search backfills and queries.
    pub embedding_model: String,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            auto_bootstrap: true,
            embedding_provider: "openai".to_string(),
            embedding_model: "text-embedding-3-small".to_string(),
        }
    }
}

/// Session storage configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionConfig {
    /// Offload threshold in bytes for large event payload strings.
    pub blob_threshold_bytes: usize,
    /// Root directory for local blob storage.
    pub blob_dir: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            blob_threshold_bytes: 65_536,
            blob_dir: "~/.moa/blobs".to_string(),
        }
    }
}

/// Local daemon configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Unix socket path used by the daemon control plane.
    pub socket_path: String,
    /// PID file written by the daemon process.
    pub pid_file: String,
    /// Log file written by the daemon process.
    pub log_file: String,
    /// Whether interactive clients should auto-connect when the daemon is running.
    pub auto_connect: bool,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: "~/.moa/daemon/daemon.sock".to_string(),
            pid_file: "~/.moa/daemon/daemon.pid".to_string(),
            log_file: "~/.moa/daemon/daemon.log".to_string(),
            auto_connect: true,
        }
    }
}

/// Observability configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OtlpProtocol {
    /// Export OTLP spans over gRPC.
    #[default]
    Grpc,
    /// Export OTLP spans over HTTP protobuf.
    Http,
}

impl OtlpProtocol {
    /// Returns the serialized config string for this protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Grpc => "grpc",
            Self::Http => "http",
        }
    }
}

/// Observability configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservabilityConfig {
    /// Whether OTLP export is enabled.
    pub enabled: bool,
    /// Logical service name for traces.
    pub service_name: String,
    /// Optional OTLP endpoint override.
    pub otlp_endpoint: Option<String>,
    /// OTLP transport protocol.
    pub otlp_protocol: OtlpProtocol,
    /// Additional OTLP headers for exporter auth and routing.
    pub otlp_headers: HashMap<String, String>,
    /// Deployment environment resource attribute.
    pub environment: Option<String>,
    /// Application release or version resource attribute.
    pub release: Option<String>,
    /// Trace sampling ratio from 0.0 to 1.0.
    pub sample_rate: f64,
}

impl Default for ObservabilityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            service_name: "moa".to_string(),
            otlp_endpoint: None,
            otlp_protocol: OtlpProtocol::Grpc,
            otlp_headers: HashMap::new(),
            environment: None,
            release: None,
            sample_rate: 1.0,
        }
    }
}

/// Prometheus metrics export configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Whether the Prometheus scrape endpoint should be exposed.
    pub enabled: bool,
    /// Listener address for the Prometheus scrape endpoint.
    pub listen: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "0.0.0.0:9090".to_string(),
        }
    }
}

/// Workspace-level cost budget settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetConfig {
    /// Maximum daily spend per workspace in cents. `0` disables budget enforcement.
    pub daily_workspace_cents: u32,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            daily_workspace_cents: 2_000,
        }
    }
}

/// Per-session turn and loop guardrails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionLimitsConfig {
    /// Maximum completed turns per session before pausing. `0` disables the limit.
    pub max_turns: u32,
    /// Number of identical consecutive turn fingerprints that triggers a loop pause. `0` disables detection.
    pub loop_detection_threshold: u32,
}

impl Default for SessionLimitsConfig {
    fn default() -> Self {
        Self {
            max_turns: 50,
            loop_detection_threshold: 3,
        }
    }
}

/// Tool-output truncation settings for storage and history replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolOutputConfig {
    /// Maximum characters for replayed tool output.
    pub max_replay_chars: usize,
    /// Maximum preserved lines for bash output before head+tail truncation.
    pub max_bash_lines: usize,
    /// Fraction of the truncation budget allocated to the head of the output.
    pub head_ratio: f64,
}

impl Default for ToolOutputConfig {
    fn default() -> Self {
        Self {
            max_replay_chars: 20_000,
            max_bash_lines: 200,
            head_ratio: 0.4,
        }
    }
}

/// Per-tool router-level output budgets enforced before event persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolBudgetConfig {
    /// Approximate token budget for `file_read`.
    pub file_read: u32,
    /// Approximate token budget for successful `bash` stdout.
    pub bash_stdout: u32,
    /// Approximate token budget for successful `bash` stderr.
    pub bash_stderr: u32,
    /// Approximate token budget for `grep`.
    pub grep: u32,
    /// Approximate token budget for `file_search`.
    pub file_search: u32,
    /// Approximate token budget for `memory_search`.
    pub memory_search: u32,
    /// Approximate token budget for `file_outline`.
    pub file_outline: u32,
    /// Approximate token budget for tools without a dedicated override, including MCP tools.
    pub default: u32,
}

impl ToolBudgetConfig {
    /// Returns the configured total output budget for one successful tool invocation.
    pub fn for_tool(&self, tool_name: &str) -> u32 {
        match tool_name {
            "bash" => self.bash_stdout,
            "file_read" => self.file_read,
            "grep" => self.grep,
            "file_search" => self.file_search,
            "memory_search" => self.memory_search,
            "file_outline" => self.file_outline,
            _ => self.default,
        }
    }
}

impl Default for ToolBudgetConfig {
    fn default() -> Self {
        Self {
            file_read: 8_000,
            bash_stdout: 4_000,
            bash_stderr: 2_000,
            grep: 4_000,
            file_search: 4_000,
            memory_search: 3_000,
            file_outline: 2_000,
            default: 8_000,
        }
    }
}

/// Incremental context snapshot configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextSnapshotConfig {
    /// Whether compiled context snapshots are enabled.
    pub enabled: bool,
    /// Warn when a serialized snapshot exceeds this size.
    pub max_size_bytes: usize,
}

impl Default for ContextSnapshotConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size_bytes: 5_000_000,
        }
    }
}

/// Cloud runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    /// Whether cloud mode is enabled.
    pub enabled: bool,
    /// Optional alternate memory root for cloud deployments.
    pub memory_dir: Option<String>,
    /// Optional Temporal configuration.
    pub temporal: Option<CloudTemporalConfig>,
    /// Optional Fly.io configuration.
    pub flyio: Option<CloudFlyioConfig>,
    /// Optional hands configuration.
    pub hands: Option<CloudHandsConfig>,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            memory_dir: None,
            temporal: Some(CloudTemporalConfig::default()),
            flyio: Some(CloudFlyioConfig::default()),
            hands: Some(CloudHandsConfig::default()),
        }
    }
}

/// Temporal configuration for cloud mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudTemporalConfig {
    /// Temporal address.
    pub address: Option<String>,
    /// Temporal namespace.
    pub namespace: Option<String>,
    /// Temporal task queue.
    pub task_queue: String,
    /// Environment variable containing the Temporal API key.
    pub api_key_env: Option<String>,
}

impl Default for CloudTemporalConfig {
    fn default() -> Self {
        Self {
            address: None,
            namespace: None,
            task_queue: "moa-brains".to_string(),
            api_key_env: Some("TEMPORAL_API_KEY".to_string()),
        }
    }
}

/// Fly.io configuration for cloud mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudFlyioConfig {
    /// Environment variable containing the Fly.io API token.
    pub api_token_env: Option<String>,
    /// Fly application name.
    pub app_name: Option<String>,
    /// Primary region.
    pub region: String,
    /// Internal HTTP port used for Fly health checks.
    pub internal_port: u16,
    /// Interface used by the cloud health endpoint.
    pub health_bind: String,
    /// Grace period for active turns to complete after SIGTERM.
    pub graceful_shutdown_timeout_secs: u64,
}

impl Default for CloudFlyioConfig {
    fn default() -> Self {
        Self {
            api_token_env: Some("FLY_API_TOKEN".to_string()),
            app_name: None,
            region: "iad".to_string(),
            internal_port: 8080,
            health_bind: "0.0.0.0".to_string(),
            graceful_shutdown_timeout_secs: 30,
        }
    }
}

/// Cloud hand provider configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudHandsConfig {
    /// Default hand provider.
    pub default_provider: Option<String>,
    /// Environment variable containing the Daytona API key.
    pub daytona_api_key_env: Option<String>,
    /// Optional Daytona API base URL override.
    pub daytona_api_url: Option<String>,
    /// Optional default image for Daytona sandboxes.
    pub daytona_default_image: Option<String>,
    /// Environment variable containing the E2B API key.
    pub e2b_api_key_env: Option<String>,
    /// Optional E2B API base URL override.
    pub e2b_api_url: Option<String>,
    /// Optional E2B domain override.
    pub e2b_domain: Option<String>,
    /// Optional default E2B template identifier.
    pub e2b_template: Option<String>,
}

impl Default for CloudHandsConfig {
    fn default() -> Self {
        Self {
            default_provider: Some("daytona".to_string()),
            daytona_api_key_env: Some("DAYTONA_API_KEY".to_string()),
            daytona_api_url: Some("https://app.daytona.io/api".to_string()),
            daytona_default_image: Some("daytonaio/workspace:latest".to_string()),
            e2b_api_key_env: Some("E2B_API_KEY".to_string()),
            e2b_api_url: Some("https://api.e2b.dev".to_string()),
            e2b_domain: Some("e2b.app".to_string()),
            e2b_template: Some("base".to_string()),
        }
    }
}

/// Supported MCP transport configurations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportConfig {
    /// Launch a local MCP server over stdio.
    #[default]
    Stdio,
    /// Connect to a legacy server-sent-event MCP endpoint.
    Sse,
    /// Connect to a Streamable HTTP MCP endpoint.
    Http,
}

/// Credential injection mode for an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpCredentialConfig {
    /// Attach a bearer token from an environment variable.
    Bearer {
        /// Environment variable containing the token.
        token_env: String,
    },
    /// Attach an OAuth access token from an environment variable.
    OAuth {
        /// Environment variable containing the access token.
        token_env: String,
    },
    /// Attach an API key header from an environment variable.
    ApiKey {
        /// Header name expected by the upstream service.
        header: String,
        /// Environment variable containing the header value.
        value_env: String,
    },
}

/// One configured MCP server connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct McpServerConfig {
    /// Stable MCP server name.
    pub name: String,
    /// Selected transport for the server.
    pub transport: McpTransportConfig,
    /// Optional stdio command.
    pub command: Option<String>,
    /// Optional stdio command arguments.
    pub args: Vec<String>,
    /// Optional stdio environment variables.
    pub env: std::collections::HashMap<String, String>,
    /// Optional remote endpoint URL for HTTP/SSE transports.
    pub url: Option<String>,
    /// Optional credential injection configuration.
    pub credentials: Option<McpCredentialConfig>,
}

/// Messaging gateway configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GatewayConfig {
    /// Environment variable containing the Telegram bot token.
    pub telegram_token_env: String,
    /// Environment variable containing the Slack bot token.
    pub slack_token_env: String,
    /// Environment variable containing the Slack app token.
    pub slack_app_token_env: String,
    /// Environment variable containing the Discord bot token.
    pub discord_token_env: String,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            telegram_token_env: "TELEGRAM_BOT_TOKEN".to_string(),
            slack_token_env: "SLACK_BOT_TOKEN".to_string(),
            slack_app_token_env: "SLACK_APP_TOKEN".to_string(),
            discord_token_env: "DISCORD_BOT_TOKEN".to_string(),
        }
    }
}

/// Desktop application configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DesktopConfig {
    /// Theme name.
    pub theme: String,
    /// Whether to auto-show the sidebar.
    pub sidebar_auto: bool,
    /// Maximum number of open tabs.
    pub tab_limit: usize,
    /// Diff rendering mode.
    pub diff_style: String,
    /// UI density: "comfortable" (default) or "compact".
    #[serde(default = "default_density")]
    pub density: String,
}

fn default_density() -> String {
    "comfortable".to_string()
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            theme: "default".to_string(),
            sidebar_auto: true,
            tab_limit: 8,
            diff_style: "auto".to_string(),
            density: default_density(),
        }
    }
}

/// Permission posture configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    /// Default posture for approvals.
    pub default_posture: String,
    /// Tools approved automatically.
    pub auto_approve: Vec<String>,
    /// Tools always denied.
    pub always_deny: Vec<String>,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            default_posture: "approve".to_string(),
            auto_approve: vec!["file_read".to_string(), "file_search".to_string()],
            always_deny: Vec::new(),
        }
    }
}

/// Session-history compaction configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    /// Whether reversible history compaction is enabled.
    pub enabled: bool,
    /// Emit a checkpoint after this many unsummarized events.
    pub event_threshold: usize,
    /// Emit a checkpoint after unsummarized history reaches this fraction of the token budget.
    pub token_ratio_threshold: f64,
    /// Number of most recent user turns to keep verbatim in context.
    pub recent_turns_verbatim: usize,
    /// Whether old error events must stay verbatim in the compiled view.
    pub preserve_errors: bool,
    /// Trigger cache-aware trimming when older history exceeds this many blocks.
    pub tier2_trigger_blocks_past_bp4: usize,
    /// Trigger summarization when the turn approaches this fraction of the model context window.
    pub tier3_trigger_fraction: f64,
    /// Hard ceiling for input tokens per turn after compaction.
    pub max_input_tokens_per_turn: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            event_threshold: 100,
            token_ratio_threshold: 0.7,
            recent_turns_verbatim: 5,
            preserve_errors: true,
            tier2_trigger_blocks_past_bp4: 14,
            tier3_trigger_fraction: 0.9,
            max_input_tokens_per_turn: 160_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn default_config_is_valid() {
        let config = MoaConfig::default();
        assert_eq!(config.general.default_provider, "openai");
        assert_eq!(config.general.default_model, "gpt-5.4");
        assert_eq!(config.models.main, "gpt-5.4");
        assert!(config.models.auxiliary.is_none());
    }

    #[test]
    fn config_loads_from_toml_string() {
        let toml = r#"
            [general]
            default_provider = "openai"
            default_model = "gpt-4o"
            reasoning_effort = "high"

            [models]
            main = "claude-sonnet-4-6"
            auxiliary = "claude-haiku-4-5"

            [database]
            admin_url = "postgres://direct.example/moa"

            [local]
            docker_enabled = false
        "#;
        let config: MoaConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.general.default_provider, "openai");
        assert_eq!(config.general.default_model, "gpt-4o");
        assert_eq!(config.models.main, "claude-sonnet-4-6");
        assert_eq!(config.models.auxiliary.as_deref(), Some("claude-haiku-4-5"));
        assert!(!config.local.docker_enabled);
        assert_eq!(config.database.admin_url(), "postgres://direct.example/moa");
    }

    #[test]
    fn compaction_config_defaults_are_applied() {
        let config = MoaConfig::default();
        assert!(config.compaction.enabled);
        assert_eq!(config.compaction.event_threshold, 100);
        assert_eq!(config.compaction.recent_turns_verbatim, 5);
        assert!(config.compaction.preserve_errors);
        assert_eq!(config.compaction.tier2_trigger_blocks_past_bp4, 14);
        assert!((config.compaction.tier3_trigger_fraction - 0.9_f64).abs() < f64::EPSILON);
        assert_eq!(config.compaction.max_input_tokens_per_turn, 160_000);
    }

    #[test]
    fn session_blob_config_defaults_are_applied() {
        let config = MoaConfig::default();
        assert_eq!(config.session.blob_threshold_bytes, 65_536);
        assert_eq!(config.session.blob_dir, "~/.moa/blobs");
    }

    #[test]
    fn memory_config_defaults_are_applied() {
        let config = MoaConfig::default();
        assert!(config.memory.auto_bootstrap);
    }

    #[test]
    fn budget_config_defaults_are_applied() {
        let config = MoaConfig::default();
        assert_eq!(config.budgets.daily_workspace_cents, 2_000);
    }

    #[test]
    fn session_limits_config_defaults_are_applied() {
        let config = MoaConfig::default();
        assert_eq!(config.session_limits.max_turns, 50);
        assert_eq!(config.session_limits.loop_detection_threshold, 3);
    }

    #[test]
    fn observability_config_defaults_to_grpc() {
        let toml = r#"
            [observability]
            enabled = true
            service_name = "moa"
        "#;
        let config: MoaConfig = toml::from_str(toml).expect("config should deserialize");
        assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Grpc);
        assert!((config.observability.sample_rate - 1.0_f64).abs() < f64::EPSILON);
        assert!(config.observability.otlp_headers.is_empty());
    }

    #[test]
    fn observability_config_http_with_langfuse_headers() {
        let toml = r#"
            [observability]
            enabled = true
            otlp_protocol = "http"
            otlp_endpoint = "http://langfuse:3000/api/public/otel"
            environment = "staging"
            release = "abc123"
            sample_rate = 0.5

            [observability.otlp_headers]
            Authorization = "Basic cGstbGYteHh4eHg6c2stbGYteHh4eHg="
            x-langfuse-ingestion-version = "4"
        "#;
        let config: MoaConfig = toml::from_str(toml).expect("config should deserialize");
        assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Http);
        assert_eq!(config.observability.environment.as_deref(), Some("staging"));
        assert_eq!(config.observability.release.as_deref(), Some("abc123"));
        assert!((config.observability.sample_rate - 0.5_f64).abs() < f64::EPSILON);
        assert_eq!(config.observability.otlp_headers.len(), 2);
    }

    #[test]
    fn observability_config_backward_compat() {
        let toml = r"
            [observability]
            enabled = false
        ";
        let config: MoaConfig = toml::from_str(toml).expect("config should deserialize");
        assert!(!config.observability.enabled);
        assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Grpc);
        assert!((config.observability.sample_rate - 1.0_f64).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_config_defaults_are_applied() {
        let config = MoaConfig::default();
        assert!(!config.metrics.enabled);
        assert_eq!(config.metrics.listen, "0.0.0.0:9090");
    }

    #[test]
    fn metrics_config_deserializes() {
        let toml = r#"
            [metrics]
            enabled = true
            listen = "127.0.0.1:19090"
        "#;
        let config: MoaConfig = toml::from_str(toml).expect("config should deserialize");
        assert!(config.metrics.enabled);
        assert_eq!(config.metrics.listen, "127.0.0.1:19090");
    }

    #[test]
    fn config_loads_mcp_server_configuration() {
        let toml = r#"
            [[mcp_servers]]
            name = "github"
            transport = "stdio"
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-github"]

            [[mcp_servers]]
            name = "custom-api"
            transport = "http"
            url = "https://example.com/mcp"
            credentials = { type = "bearer", token_env = "CUSTOM_TOKEN" }
        "#;

        let config: MoaConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 2);
        assert_eq!(config.mcp_servers[0].name, "github");
        assert_eq!(config.mcp_servers[1].transport, McpTransportConfig::Http);
        assert!(matches!(
            config.mcp_servers[1].credentials,
            Some(McpCredentialConfig::Bearer { .. })
        ));
    }

    #[test]
    fn config_loads_from_file() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(include_bytes!("../../docs/sample-config.toml"))
            .unwrap();

        let config = MoaConfig::load_from_path(file.path()).unwrap();
        assert_eq!(config.general.default_provider, "openai");
        assert_eq!(config.desktop.tab_limit, 8);
        assert_eq!(config.session_limits.max_turns, 50);
        assert_eq!(config.session_limits.loop_detection_threshold, 3);
        assert_eq!(config.tool_output.max_replay_chars, 20_000);
        assert_eq!(config.tool_output.max_bash_lines, 200);
        assert!((config.tool_output.head_ratio - 0.4_f64).abs() < f64::EPSILON);
        assert_eq!(config.tool_budgets.file_read, 8_000);
        assert_eq!(config.tool_budgets.bash_stdout, 4_000);
        assert_eq!(config.tool_budgets.bash_stderr, 2_000);
        assert_eq!(config.tool_budgets.grep, 4_000);
        assert_eq!(config.tool_budgets.file_search, 4_000);
        assert_eq!(config.tool_budgets.memory_search, 3_000);
        assert_eq!(config.tool_budgets.file_outline, 2_000);
        assert_eq!(config.tool_budgets.default, 8_000);
        assert!(!config.metrics.enabled);
        assert_eq!(config.metrics.listen, "0.0.0.0:9090");
    }

    #[test]
    fn config_rejects_zero_neon_checkpoint_limit_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            br#"
                [database]
                backend = "postgres"
                url = "postgres://postgres:postgres@localhost/moa"

                [database.neon]
                enabled = true
                project_id = "project-1"
                max_checkpoints = 0
            "#,
        )
        .unwrap();

        let error = MoaConfig::load_from_path(&path).expect_err("invalid config");
        assert!(
            error
                .to_string()
                .contains("database.neon.max_checkpoints must be greater than zero")
        );
    }
}
