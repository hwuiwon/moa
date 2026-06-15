//! Configuration for MOA, organized by sub-domain.

mod async_authz;
mod audit_security;
mod auth;
mod authz;
mod context;
mod database;
mod env_overlay;
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
pub use auth::{
    Auth0AuthConfig, AuthConfig, AuthHeaderTrustKind, AuthProviderKind, LocalAuthConfig,
    OidcAuthConfig,
};
pub use authz::{AuthzConfig, AuthzEngine, OpenFgaConfig};
pub use context::{
    BudgetConfig, CompactionConfig, ContextSnapshotConfig, QueryRewriteConfig, ResolutionConfig,
    ResolutionWeights, SessionLimitsConfig, SkillBudgetConfig, ToolBudgetConfig, ToolOutputConfig,
};
pub use database::{DatabaseConfig, DatabaseNeonConfig};
pub use env_overlay::MoaEnvOverlay;
pub use gateway::GatewayConfig;
pub use lineage::LineageConfig;
pub use memory::{
    CohereEmbedderConfig, GeminiEmbedderConfig, MemoryConfig, MemoryDigestConfig,
    MemoryExtractionConfig, MemoryRankingConfig, MemoryRankingMode, MemoryRankingWeights,
    MemoryRerankerMode, MemoryRetrievalConfig, MemoryVectorConfig, TurbopufferVectorConfig,
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
    /// Automated task-segment assessment controls.
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
    use std::sync::Mutex;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const CONFIG_ENV_KEYS: &[&str] = &[
        "HOME",
        "MOA_DATABASE_URL",
        "MOA_AUTH_PROVIDER",
        "MOA_AUTHZ_OPENFGA_URL",
        "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
        "MOA_AUTHZ_OPENFGA_STORE_ID",
        "MOA_AUTHZ_OPENFGA_MODEL_ID",
        "MOA_AUTHZ_OPENFGA_TIMEOUT_MS",
        "MOA_DATABASE_NEON_ENABLED",
        "MOA_DATABASE_NEON_PROJECT_ID",
        "MOA_DATABASE_NEON_MAX_CHECKPOINTS",
        "MOA_MEMORY_RETRIEVAL_RERANKER_MODE",
        "MOA_MEMORY_RETRIEVAL_LINEAGE_ENABLED",
        "MOA_MEMORY_DIGEST_ENABLED",
        "MOA_MEMORY_DIGEST_MAX_TOKENS",
        "MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS",
        "MOA_MEMORY_EXTRACTION_ENABLED",
        "MOA_MEMORY_EXTRACTION_API_KEY_ENV",
        "MOA_MEMORY_EXTRACTION_MODEL",
        "MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK",
        "MOA_MEMORY_EXTRACTION_TIMEOUT_MS",
        "MOA_ORCHESTRATOR_ENDPOINT",
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
    fn load_does_not_require_home() {
        // Pins: the default runtime loader does not require a home directory.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);

        let config = MoaConfig::load().expect("load config from env-only defaults");

        assert_eq!(config.database.url, MoaConfig::default().database.url);
    }

    #[test]
    fn env_only_loads_database_auth_and_openfga_config() {
        // Pins: canonical single-underscore env names populate runtime config.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);
        unsafe {
            std::env::set_var("MOA_DATABASE_URL", "postgres://env.example/moa");
            std::env::set_var("MOA_AUTH_PROVIDER", "disabled");
            std::env::set_var("MOA_AUTHZ_OPENFGA_URL", "http://openfga:8080");
            std::env::set_var("MOA_AUTHZ_OPENFGA_PRESHARED_KEY", "dev-key");
            std::env::set_var("MOA_AUTHZ_OPENFGA_STORE_ID", "store-1");
            std::env::set_var("MOA_AUTHZ_OPENFGA_MODEL_ID", "model-1");
            std::env::set_var("MOA_AUTHZ_OPENFGA_TIMEOUT_MS", "1234");
        }

        let config = MoaConfig::load_from_env().expect("load config from env");

        assert_eq!(config.database.url, "postgres://env.example/moa");
        assert_eq!(config.auth.provider, AuthProviderKind::Disabled);
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
    fn env_only_loader_rejects_zero_neon_checkpoint_limit_when_enabled() {
        // Pins: env-only config loading still validates nested config invariants.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);
        unsafe {
            std::env::set_var("MOA_DATABASE_URL", "postgres://env.example/moa");
            std::env::set_var("MOA_DATABASE_NEON_ENABLED", "true");
            std::env::set_var("MOA_DATABASE_NEON_PROJECT_ID", "project-1");
            std::env::set_var("MOA_DATABASE_NEON_MAX_CHECKPOINTS", "0");
        }

        let error = MoaConfig::load_from_env().expect_err("invalid config");
        assert!(
            error
                .to_string()
                .contains("database.neon.max_checkpoints must be greater than zero")
        );
    }

    #[test]
    fn env_only_loads_memory_extraction_config() {
        // Pins: model-backed memory extraction uses flat MOA env names.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);
        unsafe {
            std::env::set_var("MOA_MEMORY_EXTRACTION_ENABLED", "true");
            std::env::set_var("MOA_MEMORY_EXTRACTION_API_KEY_ENV", "MOA_TEST_COHERE_KEY");
            std::env::set_var("MOA_MEMORY_EXTRACTION_MODEL", "command-test");
            std::env::set_var("MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK", "5");
            std::env::set_var("MOA_MEMORY_EXTRACTION_TIMEOUT_MS", "2500");
        }

        let config = MoaConfig::load_from_env().expect("load config from env");

        assert!(config.memory.extraction.enabled);
        assert_eq!(config.memory.extraction.api_key_env, "MOA_TEST_COHERE_KEY");
        assert_eq!(config.memory.extraction.model, "command-test");
        assert_eq!(config.memory.extraction.max_facts_per_chunk, 5);
        assert_eq!(config.memory.extraction.timeout_ms, 2500);
    }
}
