//! Configuration for MOA, organized by sub-domain.

mod async_authz;
mod audit_security;
mod auth;
mod authz;
mod compliance;
mod context;
mod database;
mod env_overlay;
mod knowledge;
mod learning;
mod lineage;
mod loader;
mod memory;
mod messaging;
mod orchestrator;
mod providers;
mod runtime_cache;
mod sandbox;
mod security;
mod session;
mod telemetry;
mod token_vault;

pub use async_authz::{AsyncAuthzConfig, AsyncAuthzKind};
pub use audit_security::AuditSecurityConfig;
pub use auth::{
    Auth0AuthConfig, AuthConfig, AuthProviderKind, ContactTokenConfig, LocalAuthConfig,
    OidcAuthConfig,
};
pub use authz::{AuthzConfig, AuthzEngine, OpenFgaConfig};
pub use compliance::{
    ComplianceConfig, LINEAGE_AUDIT_SIGNING_KEY_ID_DEFAULT, PRIVACY_EXPORT_SIGNING_KEY_ID_DEFAULT,
};
pub use context::{
    BudgetConfig, CompactionConfig, ContextSnapshotConfig, QueryRewriteConfig, ResolutionConfig,
    ResolutionWeights, SessionLimitsConfig, SkillBudgetConfig, ToolBudgetConfig, ToolOutputConfig,
};
pub use database::{DatabaseConfig, DatabaseNeonConfig};
pub use env_overlay::MoaEnvOverlay;
pub use knowledge::{
    KnowledgeChunkingConfig, KnowledgeConfig, KnowledgeObservabilityConfig,
    KnowledgeParserDefaultsConfig, KnowledgeParsersConfig, KnowledgeProvidersConfig,
    KnowledgeSyncConfig, LlamaParseKnowledgeParserConfig, MergeKnowledgeProviderConfig,
    NangoKnowledgeProviderConfig, ReductoKnowledgeParserConfig, UnstructuredKnowledgeParserConfig,
};
pub use learning::{LearningConfig, SkillLearningConfig};
pub use lineage::LineageConfig;
pub use memory::{
    CohereEmbedderConfig, GeminiEmbedderConfig, MemoryConfig, MemoryDigestConfig,
    MemoryExtractionConfig, MemoryRankingConfig, MemoryRankingWeights, MemoryRerankerMode,
    MemoryRetrievalConfig, MemoryVectorConfig, TurbopufferVectorConfig, VectorEmbedderConfig,
    ZeroEntropyEmbedderConfig,
};
pub use messaging::MessagingConfig;
pub use orchestrator::OrchestratorConfig;
pub use providers::{GeneralConfig, ModelsConfig, ProviderCredentialConfig, ProvidersConfig};
pub use runtime_cache::{RuntimeCacheBackend, RuntimeCacheConfig};
pub use sandbox::{
    CloudConfig, CloudHandsConfig, LocalConfig, McpCredentialConfig, McpServerConfig,
    McpTransportConfig,
};
pub use security::PermissionsConfig;
pub use session::{
    SessionAttachmentBackend, SessionAttachmentStorageConfig, SessionBlobBackend, SessionConfig,
};
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
    /// Compliance, privacy, and DSAR signing settings.
    pub compliance: ComplianceConfig,
    /// Local runtime settings.
    pub local: LocalConfig,
    /// Memory bootstrap and maintenance settings.
    pub memory: MemoryConfig,
    /// Tenant knowledge-base ingestion settings.
    pub knowledge: KnowledgeConfig,
    /// Cloud runtime settings.
    pub cloud: CloudConfig,
    /// Messaging adapter settings.
    pub messaging: MessagingConfig,
    /// Permission policy settings.
    pub permissions: PermissionsConfig,
    /// Session storage settings.
    pub session: SessionConfig,
    /// Ephemeral runtime cache settings.
    pub runtime_cache: RuntimeCacheConfig,
    /// Session-history compaction settings.
    pub compaction: CompactionConfig,
    /// Restate-backed orchestrator endpoint settings.
    pub orchestrator: OrchestratorConfig,
    /// Observability and OTLP export settings.
    pub observability: ObservabilityConfig,
    /// Prometheus metrics export settings.
    pub metrics: MetricsConfig,
    /// Tenant budget enforcement settings.
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
    /// Automated learning-loop controls.
    pub learning: LearningConfig,
    /// Incremental context snapshot settings.
    pub context_snapshot: ContextSnapshotConfig,
    /// External MCP server connections.
    pub mcp_servers: Vec<McpServerConfig>,
}

/// Returns a trimmed required secret value loaded from direct runtime config.
pub fn required_config_secret(env_name: &'static str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(MoaError::MissingEnvironmentVariable(env_name.to_string()));
    }
    Ok(value.to_string())
}

/// Returns a trimmed optional secret value loaded from direct runtime config.
#[must_use]
pub fn optional_config_secret(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
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

        self.session.validate()?;

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
        "MOA_AUTH_AUTH0_WEBHOOK_SECRET",
        "MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX",
        "MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX",
        "MOA_PRIVACY_EXPORT_SIGNING_KEY_ID",
        "MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX",
        "MOA_LINEAGE_AUDIT_SIGNING_KEY_ID",
        "MOA_PII_VAULT_SECRET_HEX",
        "MOA_ANTHROPIC_API_KEY",
        "MOA_OPENAI_API_KEY",
        "MOA_GOOGLE_API_KEY",
        "MOA_COHERE_API_KEY",
        "MOA_ZEROENTROPY_API_KEY",
        "MOA_DATABASE_NEON_ENABLED",
        "MOA_DATABASE_NEON_PROJECT_ID",
        "MOA_DATABASE_NEON_MAX_CHECKPOINTS",
        "MOA_MEMORY_RETRIEVAL_RERANKER_MODE",
        "MOA_MEMORY_RETRIEVAL_LINEAGE_ENABLED",
        "MOA_MEMORY_DIGEST_ENABLED",
        "MOA_MEMORY_DIGEST_MAX_TOKENS",
        "MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS",
        "MOA_MEMORY_EXTRACTION_ENABLED",
        "MOA_MEMORY_EXTRACTION_MODEL",
        "MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK",
        "MOA_MEMORY_EXTRACTION_TIMEOUT_MS",
        "MOA_LEARNING_SKILLS_MIN_TOOL_CALLS",
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
            std::env::set_var("MOA_AUTH_AUTH0_WEBHOOK_SECRET", "webhook-secret");
        }

        let config = MoaConfig::load_from_env().expect("load config from env");

        assert_eq!(config.database.url, "postgres://env.example/moa");
        assert_eq!(config.auth.provider, AuthProviderKind::Disabled);
        assert_eq!(
            config.auth.auth0_webhook_secret.as_deref(),
            Some("webhook-secret")
        );
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
    fn env_only_loads_memory_extraction_and_embedder_config() {
        // Pins: model-backed memory extraction and vector embedder keys use flat MOA env names.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);
        unsafe {
            std::env::set_var("MOA_MEMORY_EXTRACTION_ENABLED", "true");
            std::env::set_var("MOA_COHERE_API_KEY", "MOA_TEST_COHERE_KEY");
            std::env::set_var("MOA_MEMORY_EXTRACTION_MODEL", "command-test");
            std::env::set_var("MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK", "5");
            std::env::set_var("MOA_MEMORY_EXTRACTION_TIMEOUT_MS", "2500");
            std::env::set_var("MOA_ZEROENTROPY_API_KEY", "MOA_TEST_ZEROENTROPY_KEY");
        }

        let config = MoaConfig::load_from_env().expect("load config from env");

        assert!(config.memory.extraction.enabled);
        assert_eq!(config.providers.cohere.api_key, "MOA_TEST_COHERE_KEY");
        assert_eq!(config.memory.extraction.api_key, "MOA_TEST_COHERE_KEY");
        assert_eq!(config.memory.extraction.model, "command-test");
        assert_eq!(config.memory.extraction.max_facts_per_chunk, 5);
        assert_eq!(config.memory.extraction.timeout_ms, 2500);
        assert_eq!(
            config.providers.zeroentropy.api_key,
            "MOA_TEST_ZEROENTROPY_KEY"
        );
        assert_eq!(
            config.memory.vector.embedder.zeroentropy.api_key,
            "MOA_TEST_ZEROENTROPY_KEY"
        );
    }

    #[test]
    fn env_only_loads_compliance_privacy_and_lineage_config() {
        // Pins: privacy and lineage operational keys use flat MOA env names through envy.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);
        let approval_key_hex = "01".repeat(32);
        let export_key_hex = "02".repeat(32);
        let lineage_key_hex = "03".repeat(32);
        let pii_vault_secret_hex = "04".repeat(32);
        unsafe {
            std::env::set_var("MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX", &approval_key_hex);
            std::env::set_var("MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX", &export_key_hex);
            std::env::set_var("MOA_PRIVACY_EXPORT_SIGNING_KEY_ID", "privacy-key-v2");
            std::env::set_var("MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX", &lineage_key_hex);
            std::env::set_var("MOA_LINEAGE_AUDIT_SIGNING_KEY_ID", "lineage-key-v2");
            std::env::set_var("MOA_PII_VAULT_SECRET_HEX", &pii_vault_secret_hex);
        }

        let config = MoaConfig::load_from_env().expect("load config from env");

        assert_eq!(
            config.compliance.privacy_approval_public_key_hex.as_deref(),
            Some(approval_key_hex.as_str())
        );
        assert_eq!(
            config.compliance.privacy_export_signing_key_hex.as_deref(),
            Some(export_key_hex.as_str())
        );
        assert_eq!(
            config.compliance.privacy_export_signing_key_id,
            "privacy-key-v2"
        );
        assert_eq!(
            config.compliance.lineage_audit_signing_key_hex.as_deref(),
            Some(lineage_key_hex.as_str())
        );
        assert_eq!(
            config.compliance.lineage_audit_signing_key_id,
            "lineage-key-v2"
        );
        assert_eq!(
            config.compliance.pii_vault_secret_hex.as_deref(),
            Some(pii_vault_secret_hex.as_str())
        );
    }

    #[test]
    fn env_only_loads_skill_learning_config() {
        // Pins: post-turn skill learning uses flat MOA env names.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);
        unsafe {
            std::env::set_var("MOA_LEARNING_SKILLS_MIN_TOOL_CALLS", "7");
        }

        let config = MoaConfig::load_from_env().expect("load config from env");

        assert_eq!(config.learning.skills.min_tool_calls, 7);
    }
}
