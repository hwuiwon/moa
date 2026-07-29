//! Configuration for MOA, organized by sub-domain.
//!
//! This crate owns the `MoaConfig` tree, its per-domain sub-configs, and the
//! flat `EnvOverlay` used to apply Kubernetes-style environment overrides. It
//! depends on `moa-core` for shared domain types and the `MoaError`/`Result`
//! error surface, and is kept separate so config-knob changes do not force a
//! rebuild of crates that never touch configuration.

mod analytics;
mod async_authz;
mod audit_security;
mod auth;
mod authz;
mod clickhouse;
mod compliance;
mod context;
mod database;
mod env_overlay;
mod execution;
mod kms;
mod knowledge;
mod learning;
mod lineage;
mod llm_dlp;
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

pub use analytics::AnalyticsConfig;
pub use async_authz::{AsyncAuthzConfig, AsyncAuthzKind};
pub use audit_security::AuditSecurityConfig;
pub use auth::{
    Auth0AuthConfig, AuthConfig, AuthProviderKind, ContactTokenConfig, LocalAuthConfig,
    OAuthClientConfig, OAuthClientType, OAuthServerConfig, OidcAuthConfig,
};
pub use authz::{AuthzConfig, AuthzEngine, OpenFgaConfig};
pub use clickhouse::ClickHouseConfig;
pub use compliance::{
    ComplianceConfig, LINEAGE_AUDIT_SIGNING_KEY_ID_DEFAULT, PRIVACY_EXPORT_SIGNING_KEY_ID_DEFAULT,
};
pub use context::{
    BudgetConfig, CompactionConfig, ContextSnapshotConfig, QueryRewriteConfig, ResolutionConfig,
    ResolutionWeights, SessionLimitsConfig, SkillBudgetConfig, ToolBudgetConfig, ToolOutputConfig,
};
pub use database::{DatabaseConfig, DatabaseNeonConfig};
pub use env_overlay::EnvOverlay;
pub use execution::ExecutionConfig;
pub use kms::{KmsConfig, KmsProviderKind};
pub use knowledge::{
    KnowledgeChunkingConfig, KnowledgeConfig, KnowledgeObservabilityConfig,
    KnowledgeParserDefaultsConfig, KnowledgeParsersConfig, KnowledgeProvidersConfig,
    KnowledgeSyncConfig, LlamaParseKnowledgeParserConfig, MergeKnowledgeProviderConfig,
    NangoKnowledgeProviderConfig, ReductoKnowledgeParserConfig, UnstructuredKnowledgeParserConfig,
};
pub use learning::{
    EmbeddingBackfillConfig, LearningConfig, RecurrenceConfig, RegressionMonitorConfig,
    SegmentBoundaryConfig, SkillLearningConfig,
};
pub use lineage::LineageConfig;
pub use llm_dlp::LlmDlpConfig;
pub use memory::{
    MemoryConfig, MemoryDigestConfig, MemoryExtractionConfig, MemoryRankingConfig,
    MemoryRankingWeights, MemoryRetrievalConfig, MemoryVectorConfig, TurbopufferVectorConfig,
    TurbopufferVectorType, VectorEmbedderConfig,
};
pub use messaging::MessagingConfig;
pub use orchestrator::OrchestratorConfig;
pub use providers::{
    ConcurrencyScope, CoordinationFailurePolicy, DeploymentProviderPolicyConfig, GeneralConfig,
    ModelsConfig, ProviderCapabilitiesConfig, ProviderConcurrencyConfig, ProviderCredentialConfig,
    ProviderPacingConfig, ProviderStreamTimeoutConfig, ProvidersConfig,
};
pub use runtime_cache::{RuntimeCacheBackend, RuntimeCacheConfig};
pub use sandbox::{
    CloudConfig, CloudHandsConfig, LOCAL_DEVELOPMENT_SANDBOX_REVISION, LocalConfig,
    McpCredentialConfig, McpDiscoveryMode, McpServerConfig, SandboxPolicyConfig,
    SandboxProfileConfig,
};
pub use security::{PermissionsConfig, SecurityProfile};
pub use session::{
    SessionAttachmentBackend, SessionAttachmentStorageConfig, SessionBlobBackend, SessionConfig,
};
pub use telemetry::{
    MetricsConfig, MetricsExporter, ObservabilityConfig, OtlpProtocol, OtlpSignal,
    otlp_signal_endpoint,
};
pub use token_vault::{OAuthRefreshConfig, TokenVaultConfig, TokenVaultKind};

use serde::{Deserialize, Serialize};

use moa_core::error::{MoaError, Result};

/// Top-level MOA configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MoaConfig {
    /// Deployment security posture that decides whether local hands and
    /// permissive permission defaults are permitted at all.
    pub security_profile: SecurityProfile,
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
    /// Envelope-encryption key-management settings.
    pub kms: KmsConfig,
    /// Async authorization provider settings.
    pub async_authz: AsyncAuthzConfig,
    /// OCSF security-event audit settings.
    pub audit_security: AuditSecurityConfig,
    /// Compliance, privacy, and DSAR signing settings.
    pub compliance: ComplianceConfig,
    /// DLP governance applied on the outbound LLM egress boundary.
    pub llm_dlp: LlmDlpConfig,
    /// Local runtime settings.
    pub local: LocalConfig,
    /// Deployment-level sandbox resource and egress policy, the outermost of
    /// the four layers intersected into every sandbox's effective profile.
    pub sandbox_policy: SandboxPolicyConfig,
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
    /// Optional ClickHouse analytics store; when present, high-volume
    /// append-only analytics rows are stored in ClickHouse instead of Postgres.
    pub clickhouse: Option<ClickHouseConfig>,
    /// Per-query analytics budgets (Postgres statement timeout, ClickHouse limits).
    pub analytics: AnalyticsConfig,
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
    /// Execution planning, failure-loop, and resource defaults.
    pub execution: ExecutionConfig,
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

        if self.database.max_connections == 0 {
            return Err(MoaError::ConfigError(
                "database.max_connections must be greater than zero".to_string(),
            ));
        }
        if self.database.background_max_connections == 0 {
            return Err(MoaError::ConfigError(
                "database.background_max_connections must be greater than zero".to_string(),
            ));
        }

        if self.session_limits.turn_admission_fleet_limit == 0
            || self.session_limits.turn_admission_tenant_limit == 0
        {
            return Err(MoaError::ConfigError(
                "session_limits turn admission fleet and tenant limits must be greater than zero"
                    .to_string(),
            ));
        }
        if self.session_limits.turn_admission_lease_ttl_ms == 0
            || self.session_limits.turn_admission_retry_after_ms == 0
        {
            return Err(MoaError::ConfigError(
                "session_limits turn admission lease TTL and retry delay must be greater than zero"
                    .to_string(),
            ));
        }

        if self.database.uses_builtin_dev_url() {
            // Fails safe (the default targets localhost), so warn rather than reject:
            // a real deployment should configure database.url / MOA_DATABASE_URL.
            tracing::warn!(
                "using built-in development database credentials; set database.url \
                 (MOA_DATABASE_URL) for any non-development deployment"
            );
        }

        if self.general.default_provider.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "general.default_provider is required and must be a non-empty provider key"
                    .to_string(),
            ));
        }

        if self.models.main.trim().is_empty() {
            return Err(MoaError::ConfigError(
                "models.main is required and must be a non-empty model identifier".to_string(),
            ));
        }

        if self.database.neon.enabled && self.database.neon.max_checkpoints == 0 {
            return Err(MoaError::ConfigError(
                "database.neon.max_checkpoints must be greater than zero when Neon checkpointing is enabled"
                    .to_string(),
            ));
        }

        self.session.validate()?;
        self.providers.validate()?;
        self.metrics.validate()?;
        self.observability.lineage.validate()?;
        self.token_vault.validate()?;

        if self.kms.provider == KmsProviderKind::Postgres {
            if self.kms.root_key_dir.as_os_str().is_empty() {
                return Err(MoaError::ConfigError(
                    "kms.root_key_dir must be set for the postgres provider".to_string(),
                ));
            }
            if self.kms.required_generation.trim().is_empty() {
                return Err(MoaError::ConfigError(
                    "kms.required_generation must be non-empty for the postgres provider"
                        .to_string(),
                ));
            }
        }

        if let Some(clickhouse) = &self.clickhouse {
            clickhouse.validate()?;
        }
        self.analytics.validate()?;

        Ok(())
    }
}

impl MoaConfig {
    /// Returns the configured model identifier for one routing task.
    #[must_use]
    pub fn model_for_task(&self, task: moa_core::types::provider::ModelTask) -> &str {
        match task {
            moa_core::types::provider::ModelTask::MainLoop => self.models.main.as_str(),
            moa_core::types::provider::ModelTask::Summarization
            | moa_core::types::provider::ModelTask::Consolidation
            | moa_core::types::provider::ModelTask::SkillDistillation
            | moa_core::types::provider::ModelTask::Worker => self
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
        "MOA_LINEAGE_AUDIT_ROOT_SEED_HEX",
        "MOA_PII_VAULT_SECRET_HEX",
        "MOA_ANTHROPIC_API_KEY",
        "MOA_OPENAI_API_KEY",
        "MOA_GOOGLE_API_KEY",
        "MOA_COHERE_API_KEY",
        "MOA_ZEROENTROPY_API_KEY",
        "MOA_ANTHROPIC_MAX_REQUESTS_PER_MIN",
        "MOA_ANTHROPIC_MAX_INPUTS_PER_MIN",
        "MOA_ANTHROPIC_MAX_CONCURRENT_REQUESTS",
        "MOA_OPENAI_MAX_REQUESTS_PER_MIN",
        "MOA_OPENAI_MAX_INPUTS_PER_MIN",
        "MOA_OPENAI_MAX_CONCURRENT_REQUESTS",
        "MOA_GOOGLE_MAX_REQUESTS_PER_MIN",
        "MOA_GOOGLE_MAX_INPUTS_PER_MIN",
        "MOA_GOOGLE_MAX_CONCURRENT_REQUESTS",
        "MOA_COHERE_MAX_REQUESTS_PER_MIN",
        "MOA_COHERE_MAX_INPUTS_PER_MIN",
        "MOA_COHERE_MAX_CONCURRENT_REQUESTS",
        "MOA_ZEROENTROPY_MAX_REQUESTS_PER_MIN",
        "MOA_ZEROENTROPY_MAX_INPUTS_PER_MIN",
        "MOA_ZEROENTROPY_MAX_CONCURRENT_REQUESTS",
        "MOA_DATABASE_NEON_ENABLED",
        "MOA_DATABASE_NEON_PROJECT_ID",
        "MOA_DATABASE_NEON_MAX_CHECKPOINTS",
        "MOA_MEMORY_EMBEDDING_MODEL",
        "MOA_MEMORY_RETRIEVAL_RERANKER_MODEL",
        "MOA_MEMORY_RETRIEVAL_RERANKER_LATENCY",
        "MOA_MEMORY_RETRIEVAL_LINEAGE_ENABLED",
        "MOA_MEMORY_DIGEST_ENABLED",
        "MOA_MEMORY_DIGEST_MAX_TOKENS",
        "MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS",
        "MOA_MEMORY_EXTRACTION_ENABLED",
        "MOA_MEMORY_EXTRACTION_MODEL",
        "MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK",
        "MOA_MEMORY_EXTRACTION_TIMEOUT_MS",
        "MOA_LEARNING_SKILLS_MIN_TOOL_CALLS",
        "MOA_LEARNING_SEGMENTS_IDLE_GAP_MINUTES",
        "MOA_EXECUTION_PLANNER_REPAIR_ATTEMPTS",
        "MOA_EXECUTION_REPEATED_FAILURE_LIMIT",
        "MOA_EXECUTION_MAX_TASKS",
        "MOA_EXECUTION_MAX_TOKENS",
        "MOA_EXECUTION_MAX_TOOL_CALLS",
        "MOA_EXECUTION_MAX_RETRIEVED_BYTES",
        "MOA_EXECUTION_MAX_COST_MICROUSD",
        "MOA_EXECUTION_UNATTENDED_MAX_COST_MICROUSD",
        "MOA_EXECUTION_AGENT_TURN_COST_MICROUSD",
        "MOA_EXECUTION_AGENT_TURN_TOKENS",
        "MOA_EXECUTION_AGENT_TURN_TOOL_CALLS",
        "MOA_EXECUTION_AGENT_TURN_RETRIEVED_BYTES",
        "MOA_EXECUTION_VERIFIER_TURN_COST_MICROUSD",
        "MOA_EXECUTION_VERIFIER_TURN_TOKENS",
        "MOA_EXECUTION_VERIFIER_TURN_TOOL_CALLS",
        "MOA_EXECUTION_VERIFIER_TURN_RETRIEVED_BYTES",
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
    fn env_only_loads_memory_extraction_and_provider_config() {
        // Pins: model-backed memory extraction and provider keys use flat MOA env names.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);
        unsafe {
            std::env::set_var("MOA_MEMORY_EXTRACTION_ENABLED", "true");
            std::env::set_var("MOA_OPENAI_API_KEY", "MOA_TEST_OPENAI_KEY");
            std::env::set_var("MOA_MEMORY_EXTRACTION_MODEL", "gpt-5.4-mini");
            std::env::set_var("MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK", "5");
            std::env::set_var("MOA_MEMORY_EXTRACTION_TIMEOUT_MS", "2500");
            std::env::set_var("MOA_ZEROENTROPY_API_KEY", "MOA_TEST_ZEROENTROPY_KEY");
        }

        let config = MoaConfig::load_from_env().expect("load config from env");

        assert!(config.memory.extraction.enabled);
        assert_eq!(config.providers.openai.api_key, "MOA_TEST_OPENAI_KEY");
        assert_eq!(config.memory.extraction.model, "gpt-5.4-mini");
        assert_eq!(config.memory.extraction.max_facts_per_chunk, 5);
        assert_eq!(config.memory.extraction.timeout_ms, 2500);
        assert_eq!(
            config.providers.zeroentropy.api_key,
            "MOA_TEST_ZEROENTROPY_KEY"
        );
    }

    #[test]
    fn env_only_loads_provider_rate_and_concurrency_caps() {
        // Pins: per-provider rate/concurrency caps use the flat MOA_<PROVIDER>_MAX_*
        // env names (the env-driven trial-key flow), unset caps stay None, and an
        // env override sets the provider's in-flight/rate ceiling.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);
        unsafe {
            std::env::set_var("MOA_COHERE_API_KEY", "MOA_TEST_COHERE_KEY");
            std::env::set_var("MOA_COHERE_MAX_REQUESTS_PER_MIN", "10");
            std::env::set_var("MOA_COHERE_MAX_INPUTS_PER_MIN", "2000");
            std::env::set_var("MOA_COHERE_MAX_CONCURRENT_REQUESTS", "2");
            std::env::set_var("MOA_ZEROENTROPY_MAX_REQUESTS_PER_MIN", "5");
            std::env::set_var("MOA_OPENAI_MAX_CONCURRENT_REQUESTS", "16");
        }

        let config = MoaConfig::load_from_env().expect("load config from env");

        assert_eq!(config.providers.cohere.max_requests_per_min, Some(10));
        assert_eq!(config.providers.cohere.max_inputs_per_min, Some(2000));
        assert_eq!(config.providers.cohere.max_concurrent_requests, Some(2));
        assert_eq!(config.providers.zeroentropy.max_requests_per_min, Some(5));
        assert_eq!(config.providers.openai.max_concurrent_requests, Some(16));
        // Unset caps remain None (provider built-in defaults apply).
        assert_eq!(config.providers.cohere.max_concurrent_requests, Some(2));
        assert_eq!(config.providers.openai.max_inputs_per_min, None);
        assert_eq!(config.providers.anthropic.max_concurrent_requests, None);
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

    #[test]
    fn env_only_overrides_segment_boundary_idle_gap() {
        // Pins: the deterministic segment-boundary idle-gap threshold is
        // overridable via a flat MOA env name, defaulting to 30 minutes.
        let _guard = ENV_LOCK.lock().expect("env test lock");
        let _env = EnvRestore::clear(CONFIG_ENV_KEYS);

        let default_config = MoaConfig::load_from_env().expect("load default config from env");
        assert_eq!(default_config.learning.segments.idle_gap_minutes, 30);

        unsafe {
            std::env::set_var("MOA_LEARNING_SEGMENTS_IDLE_GAP_MINUTES", "45");
        }
        let config = MoaConfig::load_from_env().expect("load config from env");

        assert_eq!(config.learning.segments.idle_gap_minutes, 45);
    }

    #[test]
    fn model_for_task_falls_back_to_main_when_auxiliary_is_unset() {
        // Pins: auxiliary tasks route to models.auxiliary when set and otherwise
        // fall back to models.main; the main loop always uses models.main.
        let auxiliary_tasks = [
            moa_core::types::provider::ModelTask::Summarization,
            moa_core::types::provider::ModelTask::Consolidation,
            moa_core::types::provider::ModelTask::SkillDistillation,
            moa_core::types::provider::ModelTask::Worker,
        ];

        let mut config = MoaConfig::default();
        config.models.main = "main-model".to_string();
        config.models.auxiliary = None;

        assert_eq!(
            config.model_for_task(moa_core::types::provider::ModelTask::MainLoop),
            "main-model"
        );
        for task in auxiliary_tasks {
            assert_eq!(
                config.model_for_task(task),
                "main-model",
                "auxiliary task {task:?} must fall back to models.main"
            );
        }

        config.models.auxiliary = Some("aux-model".to_string());

        assert_eq!(
            config.model_for_task(moa_core::types::provider::ModelTask::MainLoop),
            "main-model",
            "main loop must keep using models.main even when auxiliary is set"
        );
        for task in auxiliary_tasks {
            assert_eq!(
                config.model_for_task(task),
                "aux-model",
                "auxiliary task {task:?} must use models.auxiliary when set"
            );
        }
    }
}
