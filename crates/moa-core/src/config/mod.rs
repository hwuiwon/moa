//! Configuration for MOA, organized by sub-domain.

mod context;
mod database;
mod gateway;
mod lineage;
mod loader;
mod memory;
mod providers;
mod sandbox;
mod security;
mod session;
mod telemetry;

pub use context::{
    BudgetConfig, CompactionConfig, ContextSnapshotConfig, IntentConfig, QueryRewriteConfig,
    ResolutionConfig, ResolutionWeights, SessionLimitsConfig, SkillBudgetConfig, ToolBudgetConfig,
    ToolOutputConfig,
};
pub use database::{DatabaseConfig, DatabaseNeonConfig};
pub use gateway::GatewayConfig;
pub use lineage::LineageConfig;
pub use memory::{
    CohereEmbedderConfig, GeminiEmbedderConfig, MemoryConfig, MemoryVectorConfig,
    VectorEmbedderConfig,
};
pub use providers::{GeneralConfig, ModelsConfig, ProviderCredentialConfig, ProvidersConfig};
pub use sandbox::{
    CloudConfig, CloudFlyioConfig, CloudHandsConfig, LocalConfig, McpCredentialConfig,
    McpServerConfig, McpTransportConfig,
};
pub use security::PermissionsConfig;
pub use session::{DaemonConfig, SessionConfig};
pub use telemetry::{MetricsConfig, ObservabilityConfig, OtlpProtocol};

use serde::{Deserialize, Serialize};
use std::path::Path;

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
    /// Skill-manifest prompt budgeting controls for Stage 4 of the context pipeline.
    pub skill_budget: SkillBudgetConfig,
    /// Query-rewriting controls for pre-memory retrieval prompt normalization.
    pub query_rewrite: QueryRewriteConfig,
    /// Automated task-segment resolution scoring controls.
    pub resolution: ResolutionConfig,
    /// Tenant intent discovery and classification controls.
    pub intents: IntentConfig,
    /// Incremental context snapshot settings.
    pub context_snapshot: ContextSnapshotConfig,
    /// External MCP server connections.
    pub mcp_servers: Vec<McpServerConfig>,
}

impl MoaConfig {
    fn serialize_config(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|error| MoaError::ConfigError(error.to_string()))
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

fn config_parent_dir(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
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

    /// Sets the configured main-loop provider/model pair.
    pub fn set_main_model(&mut self, provider: impl Into<String>, model: impl Into<String>) {
        self.general.default_provider = provider.into();
        self.models.main = model.into();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    #[test]
    fn config_loads_from_toml_string() {
        let toml = r#"
            [general]
            default_provider = "openai"
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
        assert_eq!(config.models.main, "claude-sonnet-4-6");
        assert_eq!(config.models.auxiliary.as_deref(), Some("claude-haiku-4-5"));
        assert!(!config.local.docker_enabled);
        assert_eq!(config.database.admin_url(), "postgres://direct.example/moa");
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
        file.write_all(include_bytes!("../../../../docs/sample-config.toml"))
            .unwrap();

        let config = MoaConfig::load_from_path(file.path()).unwrap();
        assert_eq!(config.general.default_provider, "openai");
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
        assert_eq!(config.skill_budget.max_manifest_chars, None);
        assert_eq!(config.skill_budget.max_per_skill_chars, 1_536);
        assert!(config.skill_budget.show_token_estimates);
        assert!(config.query_rewrite.enabled);
        assert_eq!(config.query_rewrite.timeout_ms, 5_000);
        assert!(config.resolution.enabled);
        assert_eq!(config.resolution.structural_min_samples, 20);
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
