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
    /// Provider settings.
    pub providers: ProvidersConfig,
    /// Session database settings.
    pub database: DatabaseConfig,
    /// Local runtime settings.
    pub local: LocalConfig,
    /// Cloud runtime settings.
    pub cloud: CloudConfig,
    /// Messaging gateway settings.
    pub gateway: GatewayConfig,
    /// Interactive shell settings.
    pub tui: TuiConfig,
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
                "providers.openrouter.api_key_env",
                Self::default().providers.openrouter.api_key_env,
            )?
            .set_default(
                "database.backend",
                Self::default().database.backend.as_str(),
            )?
            .set_default("database.url", Self::default().database.url)?
            .set_default("database.admin_url", Self::default().database.admin_url)?
            .set_default(
                "database.pool_min",
                Self::default().database.pool_min as i64,
            )?
            .set_default(
                "database.pool_max",
                Self::default().database.pool_max as i64,
            )?
            .set_default(
                "database.connect_timeout_secs",
                Self::default().database.connect_timeout_secs as i64,
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
            .set_default("cloud.enabled", Self::default().cloud.enabled)?
            .set_default("cloud.turso_url", Self::default().cloud.turso_url.clone())?
            .set_default(
                "cloud.turso_auth_token_env",
                Self::default().cloud.turso_auth_token_env.clone(),
            )?
            .set_default(
                "cloud.turso_sync_interval_secs",
                Self::default().cloud.turso_sync_interval_secs as i64,
            )?
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
            .set_default("tui.theme", Self::default().tui.theme)?
            .set_default("tui.sidebar_auto", Self::default().tui.sidebar_auto)?
            .set_default("tui.tab_limit", Self::default().tui.tab_limit as i64)?
            .set_default("tui.diff_style", Self::default().tui.diff_style)?
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
            .add_source(File::from(path).required(false))
            .add_source(Environment::with_prefix("MOA").separator("__"));

        let mut config: Self = builder.build()?.try_deserialize()?;
        config.normalize_legacy_database_config();
        config.validate()?;
        Ok(config)
    }

    /// Persists this config to the default MOA config path.
    pub fn save(&self) -> Result<()> {
        self.save_to_path(Self::default_path()?)
    }

    /// Persists this config to an explicit TOML file path.
    pub fn save_to_path(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|error| MoaError::ConfigError(error.to_string()))?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

impl MoaConfig {
    fn normalize_legacy_database_config(&mut self) {
        if self.local.session_db.is_empty() {
            return;
        }

        if self.database.url == DatabaseConfig::default().url {
            self.database.url = self.local.session_db.clone();
        }
    }

    fn validate(&self) -> Result<()> {
        if self.database.neon.enabled && self.database.neon.max_checkpoints == 0 {
            return Err(MoaError::ConfigError(
                "database.neon.max_checkpoints must be greater than zero when Neon checkpointing is enabled"
                    .to_string(),
            ));
        }

        Ok(())
    }
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
    /// OpenAI credentials.
    pub openai: ProviderCredentialConfig,
    /// OpenRouter credentials.
    pub openrouter: ProviderCredentialConfig,
}

impl Default for ProvidersConfig {
    fn default() -> Self {
        Self {
            anthropic: ProviderCredentialConfig::new("ANTHROPIC_API_KEY"),
            openai: ProviderCredentialConfig::new("OPENAI_API_KEY"),
            openrouter: ProviderCredentialConfig::new("OPENROUTER_API_KEY"),
        }
    }
}

/// Supported session database backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseBackend {
    /// SQLite/libSQL/Turso backend.
    #[default]
    Turso,
    /// PostgreSQL backend.
    Postgres,
}

impl DatabaseBackend {
    /// Returns the serialized config string for this backend.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turso => "turso",
            Self::Postgres => "postgres",
        }
    }
}

/// Session database configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Selected database backend.
    pub backend: DatabaseBackend,
    /// Database URL or local file path.
    pub url: String,
    /// Optional direct/admin database URL for migrations and other session-sensitive flows.
    pub admin_url: Option<String>,
    /// Minimum pool size for pooled backends.
    pub pool_min: u32,
    /// Maximum pool size for pooled backends.
    pub pool_max: u32,
    /// Connection timeout in seconds.
    pub connect_timeout_secs: u64,
    /// Optional Neon branching configuration for ephemeral checkpoints.
    pub neon: DatabaseNeonConfig,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            backend: DatabaseBackend::Turso,
            url: "~/.moa/sessions.db".to_string(),
            admin_url: None,
            pool_min: 1,
            pool_max: 5,
            connect_timeout_secs: 10,
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
    /// Legacy session database path alias. New configs should use `database.url`.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_db: String,
    /// Memory root directory.
    pub memory_dir: String,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            docker_enabled: true,
            sandbox_dir: "~/.moa/sandbox".to_string(),
            session_db: String::new(),
            memory_dir: "~/.moa/memory".to_string(),
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

/// Cloud runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    /// Whether cloud mode is enabled.
    pub enabled: bool,
    /// Optional Turso URL.
    pub turso_url: Option<String>,
    /// Environment variable containing the Turso auth token.
    pub turso_auth_token_env: Option<String>,
    /// Background embedded-replica sync cadence in seconds.
    pub turso_sync_interval_secs: u64,
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
            turso_url: None,
            turso_auth_token_env: Some("TURSO_AUTH_TOKEN".to_string()),
            turso_sync_interval_secs: 2,
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

/// Terminal UI configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    /// Theme name.
    pub theme: String,
    /// Whether to auto-show the sidebar.
    pub sidebar_auto: bool,
    /// Maximum number of open tabs.
    pub tab_limit: usize,
    /// Diff rendering mode.
    pub diff_style: String,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: "default".to_string(),
            sidebar_auto: true,
            tab_limit: 8,
            diff_style: "auto".to_string(),
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
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            event_threshold: 100,
            token_ratio_threshold: 0.7,
            recent_turns_verbatim: 5,
            preserve_errors: true,
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
    }

    #[test]
    fn config_loads_from_toml_string() {
        let toml = r#"
            [general]
            default_provider = "openai"
            default_model = "gpt-4o"
            reasoning_effort = "high"

            [database]
            admin_url = "postgres://direct.example/moa"

            [local]
            docker_enabled = false
        "#;
        let config: MoaConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.general.default_provider, "openai");
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
    }

    #[test]
    fn session_blob_config_defaults_are_applied() {
        let config = MoaConfig::default();
        assert_eq!(config.session.blob_threshold_bytes, 65_536);
        assert_eq!(config.session.blob_dir, "~/.moa/blobs");
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
        assert_eq!(config.observability.sample_rate, 1.0);
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
        assert_eq!(config.observability.sample_rate, 0.5);
        assert_eq!(config.observability.otlp_headers.len(), 2);
    }

    #[test]
    fn observability_config_backward_compat() {
        let toml = r#"
            [observability]
            enabled = false
        "#;
        let config: MoaConfig = toml::from_str(toml).expect("config should deserialize");
        assert!(!config.observability.enabled);
        assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Grpc);
        assert_eq!(config.observability.sample_rate, 1.0);
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
        assert_eq!(config.tui.tab_limit, 8);
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
