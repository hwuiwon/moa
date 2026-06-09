//! Configuration for MOA, organized by sub-domain.

mod async_authz;
mod audit_security;
mod auth;
mod authz;
mod context;
mod database;
mod gateway;
mod lineage;
mod loader;
mod memory;
mod orchestrator;
mod providers;
mod sandbox;
mod security;
mod session;
mod telemetry;
mod token_vault;

pub use async_authz::{AsyncAuthzConfig, AsyncAuthzKind};
pub use audit_security::AuditSecurityConfig;
pub use auth::{Auth0AuthConfig, AuthConfig, AuthProviderKind, LocalAuthConfig, OidcAuthConfig};
pub use authz::{AuthzConfig, AuthzEngine, OpenFgaConfig};
pub use context::{
    BudgetConfig, CompactionConfig, ContextSnapshotConfig, QueryRewriteConfig, ResolutionConfig,
    ResolutionWeights, SessionLimitsConfig, SkillBudgetConfig, ToolBudgetConfig, ToolOutputConfig,
};
pub use database::{DatabaseConfig, DatabaseNeonConfig};
pub use gateway::GatewayConfig;
pub use lineage::LineageConfig;
pub use memory::{
    CohereEmbedderConfig, GeminiEmbedderConfig, MemoryConfig, MemoryVectorConfig,
    VectorEmbedderConfig,
};
pub use orchestrator::OrchestratorConfig;
pub use providers::{GeneralConfig, ModelsConfig, ProviderCredentialConfig, ProvidersConfig};
pub use sandbox::{
    CloudConfig, CloudFlyioConfig, CloudHandsConfig, LocalConfig, McpCredentialConfig,
    McpServerConfig, McpTransportConfig,
};
pub use security::PermissionsConfig;
pub use session::SessionConfig;
pub use telemetry::{MetricsConfig, ObservabilityConfig, OtlpProtocol};
pub use token_vault::{TokenVaultConfig, TokenVaultKind};

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
    /// Authorization engine settings.
    pub authz: AuthzConfig,
    /// Authentication provider settings.
    pub auth: AuthConfig,
    /// Token vault provider settings.
    pub token_vault: TokenVaultConfig,
    /// Async authorization provider settings.
    pub async_authz: AsyncAuthzConfig,
    /// OCSF security-event audit settings.
    pub audit_security: AuditSecurityConfig,
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
    /// Restate-backed orchestrator endpoint settings.
    pub orchestrator: OrchestratorConfig,
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
    /// Incremental context snapshot settings.
    pub context_snapshot: ContextSnapshotConfig,
    /// External MCP server connections.
    pub mcp_servers: Vec<McpServerConfig>,
}

impl MoaConfig {
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
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::Mutex;

    use tempfile::NamedTempFile;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const AUTH_ENV_KEYS: &[&str] = &[
        "MOA__AUTH__PROVIDER",
        "MOA__AUTH__AUTH0__DOMAIN",
        "MOA__AUTH__AUTH0__AUDIENCE",
        "MOA__AUTH__AUTH0__CLIENT_ID_ENV",
        "MOA__AUTH__AUTH0__CLIENT_SECRET_ENV",
        "MOA__AUTH__OIDC__ISSUER",
        "MOA__AUTH__OIDC__AUDIENCE",
        "MOA__AUTH__OIDC__JWKS_URL",
    ];

    struct EnvRestore {
        values: Vec<(&'static str, Option<String>)>,
    }

    impl EnvRestore {
        fn clear(keys: &'static [&'static str]) -> Self {
            let values = keys
                .iter()
                .map(|&key| {
                    let value = std::env::var(key).ok();
                    unsafe {
                        std::env::remove_var(key);
                    }
                    (key, value)
                })
                .collect();
            Self { values }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            for (key, value) in &self.values {
                unsafe {
                    if let Some(value) = value {
                        std::env::set_var(*key, value);
                    } else {
                        std::env::remove_var(*key);
                    }
                }
            }
        }
    }

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
    fn auth_provider_disabled_deserializes_from_toml() {
        let toml = r#"
            [auth]
            provider = "disabled"
        "#;

        let config: MoaConfig = toml::from_str(toml).expect("config should deserialize");

        assert_eq!(config.auth.provider, AuthProviderKind::Disabled);
    }

    #[test]
    fn orchestrator_endpoint_overridable_via_env() {
        // Pins: MOA__ORCHESTRATOR__ENDPOINT maps onto the thin-client endpoint.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _auth_env = EnvRestore::clear(AUTH_ENV_KEYS);
        let file = NamedTempFile::new().expect("config temp file");
        unsafe {
            std::env::set_var("MOA__ORCHESTRATOR__ENDPOINT", "http://example:1234");
        }

        let config = MoaConfig::load_from_path(file.path()).expect("load config with env");

        unsafe {
            std::env::remove_var("MOA__ORCHESTRATOR__ENDPOINT");
        }
        assert_eq!(
            config.orchestrator.endpoint.as_deref(),
            Some("http://example:1234")
        );
    }

    #[test]
    fn auth_provider_disabled_loads_from_env() {
        // Pins: MOA__AUTH__PROVIDER can disable credential authentication explicitly.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _auth_env = EnvRestore::clear(AUTH_ENV_KEYS);
        let file = NamedTempFile::new().expect("config temp file");
        unsafe {
            std::env::set_var("MOA__AUTH__PROVIDER", "disabled");
        }

        let config = MoaConfig::load_from_path(file.path()).expect("load config with env");

        unsafe {
            std::env::remove_var("MOA__AUTH__PROVIDER");
        }
        assert_eq!(config.auth.provider, AuthProviderKind::Disabled);
    }

    #[test]
    fn authz_openfga_config_loads_from_env() {
        // Pins: MOA__AUTHZ__OPENFGA__* maps onto the authz config section.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _auth_env = EnvRestore::clear(AUTH_ENV_KEYS);
        let file = NamedTempFile::new().expect("config temp file");
        unsafe {
            std::env::set_var("MOA__AUTHZ__OPENFGA__URL", "http://openfga:8080");
            std::env::set_var("MOA__AUTHZ__OPENFGA__PRESHARED_KEY", "dev-key");
            std::env::set_var("MOA__AUTHZ__OPENFGA__STORE_ID", "store-1");
            std::env::set_var("MOA__AUTHZ__OPENFGA__MODEL_ID", "model-1");
            std::env::set_var("MOA__AUTHZ__OPENFGA__TIMEOUT_MS", "1234");
        }

        let config = MoaConfig::load_from_path(file.path()).expect("load config with env");

        unsafe {
            std::env::remove_var("MOA__AUTHZ__OPENFGA__URL");
            std::env::remove_var("MOA__AUTHZ__OPENFGA__PRESHARED_KEY");
            std::env::remove_var("MOA__AUTHZ__OPENFGA__STORE_ID");
            std::env::remove_var("MOA__AUTHZ__OPENFGA__MODEL_ID");
            std::env::remove_var("MOA__AUTHZ__OPENFGA__TIMEOUT_MS");
        }
        assert_eq!(config.authz.engine, AuthzEngine::Openfga);
        let openfga = config
            .authz
            .openfga
            .expect("openfga env should create config section");
        assert_eq!(openfga.url, "http://openfga:8080");
        assert_eq!(openfga.preshared_key, "dev-key");
        assert_eq!(openfga.store_id, "store-1");
        assert_eq!(openfga.model_id, "model-1");
        assert_eq!(openfga.timeout_ms, 1234);
    }

    #[test]
    fn config_loads_from_file() {
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _auth_env = EnvRestore::clear(AUTH_ENV_KEYS);
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
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _auth_env = EnvRestore::clear(AUTH_ENV_KEYS);
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
