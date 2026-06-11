//! Flat single-underscore environment overlay for Kubernetes runtime config.

use std::collections::HashMap;
use std::str::FromStr;

use serde::Deserialize;

use crate::error::{MoaError, Result};

use super::{
    AsyncAuthzKind, Auth0AuthConfig, AuthHeaderTrustKind, AuthProviderKind, AuthzEngine,
    MemoryRerankerMode, OidcAuthConfig, OpenFgaConfig, OtlpProtocol, TokenVaultKind,
};
use super::{CloudFlyioConfig, CloudHandsConfig, MoaConfig};

const OPENFGA_DEFAULT_TIMEOUT_MS: u64 = 5000;

/// Optional flat environment overrides for `MoaConfig`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MoaEnvOverlay {
    /// `MOA_GENERAL_DEFAULT_PROVIDER`.
    pub general_default_provider: Option<String>,
    /// `MOA_GENERAL_REASONING_EFFORT`.
    pub general_reasoning_effort: Option<String>,
    /// `MOA_GENERAL_WEB_SEARCH_ENABLED`.
    pub general_web_search_enabled: Option<bool>,
    /// `MOA_GENERAL_WORKSPACE_INSTRUCTIONS`.
    pub general_workspace_instructions: Option<String>,
    /// `MOA_GENERAL_USER_INSTRUCTIONS`.
    pub general_user_instructions: Option<String>,
    /// `MOA_MODELS_MAIN`.
    pub models_main: Option<String>,
    /// `MOA_MODELS_AUXILIARY`.
    pub models_auxiliary: Option<String>,
    /// `MOA_PROVIDERS_ANTHROPIC_API_KEY_ENV`.
    pub providers_anthropic_api_key_env: Option<String>,
    /// `MOA_PROVIDERS_OPENAI_API_KEY_ENV`.
    pub providers_openai_api_key_env: Option<String>,
    /// `MOA_PROVIDERS_GOOGLE_API_KEY_ENV`.
    pub providers_google_api_key_env: Option<String>,
    /// `MOA_DATABASE_URL`.
    pub database_url: Option<String>,
    /// `MOA_DATABASE_ADMIN_URL`.
    pub database_admin_url: Option<String>,
    /// `MOA_DATABASE_SCHEMA`.
    pub database_schema: Option<String>,
    /// `MOA_DATABASE_MAX_CONNECTIONS`.
    pub database_max_connections: Option<u32>,
    /// `MOA_DATABASE_CONNECT_TIMEOUT_SECONDS`.
    pub database_connect_timeout_seconds: Option<u64>,
    /// `MOA_DATABASE_NEON_ENABLED`.
    pub database_neon_enabled: Option<bool>,
    /// `MOA_DATABASE_NEON_API_KEY_ENV`.
    pub database_neon_api_key_env: Option<String>,
    /// `MOA_DATABASE_NEON_PROJECT_ID`.
    pub database_neon_project_id: Option<String>,
    /// `MOA_DATABASE_NEON_PARENT_BRANCH_ID`.
    pub database_neon_parent_branch_id: Option<String>,
    /// `MOA_DATABASE_NEON_MAX_CHECKPOINTS`.
    pub database_neon_max_checkpoints: Option<usize>,
    /// `MOA_DATABASE_NEON_CHECKPOINT_TTL_HOURS`.
    pub database_neon_checkpoint_ttl_hours: Option<u64>,
    /// `MOA_DATABASE_NEON_POOLED`.
    pub database_neon_pooled: Option<bool>,
    /// `MOA_DATABASE_NEON_SUSPEND_TIMEOUT_SECONDS`.
    pub database_neon_suspend_timeout_seconds: Option<u64>,
    /// `MOA_AUTH_PROVIDER`.
    pub auth_provider: Option<AuthProviderKind>,
    /// `MOA_AUTH_HEADER_TRUST`.
    pub auth_header_trust: Option<AuthHeaderTrustKind>,
    /// `MOA_AUTH_AUTH0_DOMAIN`.
    pub auth_auth0_domain: Option<String>,
    /// `MOA_AUTH_AUTH0_AUDIENCE`.
    pub auth_auth0_audience: Option<String>,
    /// `MOA_AUTH_AUTH0_CLIENT_ID_ENV`.
    pub auth_auth0_client_id_env: Option<String>,
    /// `MOA_AUTH_AUTH0_CLIENT_SECRET_ENV`.
    pub auth_auth0_client_secret_env: Option<String>,
    /// `MOA_AUTH_OIDC_ISSUER`.
    pub auth_oidc_issuer: Option<String>,
    /// `MOA_AUTH_OIDC_AUDIENCE`.
    pub auth_oidc_audience: Option<String>,
    /// `MOA_AUTH_OIDC_JWKS_URL`.
    pub auth_oidc_jwks_url: Option<String>,
    /// `MOA_AUTHZ_ENGINE`.
    pub authz_engine: Option<AuthzEngine>,
    /// `MOA_AUTHZ_OPENFGA_URL`.
    pub authz_openfga_url: Option<String>,
    /// `MOA_AUTHZ_OPENFGA_PRESHARED_KEY`.
    pub authz_openfga_preshared_key: Option<String>,
    /// `MOA_AUTHZ_OPENFGA_STORE_ID`.
    pub authz_openfga_store_id: Option<String>,
    /// `MOA_AUTHZ_OPENFGA_MODEL_ID`.
    pub authz_openfga_model_id: Option<String>,
    /// `MOA_AUTHZ_OPENFGA_TIMEOUT_MS`.
    pub authz_openfga_timeout_ms: Option<u64>,
    /// `MOA_TOKEN_VAULT_PROVIDER`.
    pub token_vault_provider: Option<TokenVaultKind>,
    /// `MOA_ASYNC_AUTHZ_PROVIDER`.
    pub async_authz_provider: Option<AsyncAuthzKind>,
    /// `MOA_ASYNC_AUTHZ_DEFAULT_TIMEOUT_SECS`.
    pub async_authz_default_timeout_secs: Option<u64>,
    /// `MOA_AUDIT_SECURITY_EMIT_AUTHZ_ALLOWS`.
    pub audit_security_emit_authz_allows: Option<bool>,
    /// `MOA_LOCAL_DOCKER_ENABLED`.
    pub local_docker_enabled: Option<bool>,
    /// `MOA_LOCAL_SANDBOX_DIR`.
    pub local_sandbox_dir: Option<String>,
    /// `MOA_LOCAL_MEMORY_DIR`.
    pub local_memory_dir: Option<String>,
    /// `MOA_MEMORY_AUTO_BOOTSTRAP`.
    pub memory_auto_bootstrap: Option<bool>,
    /// `MOA_MEMORY_EMBEDDING_PROVIDER`.
    pub memory_embedding_provider: Option<String>,
    /// `MOA_MEMORY_EMBEDDING_MODEL`.
    pub memory_embedding_model: Option<String>,
    /// `MOA_MEMORY_RETRIEVAL_RERANKER_MODE`.
    pub memory_retrieval_reranker_mode: Option<MemoryRerankerMode>,
    /// `MOA_MEMORY_DIGEST_ENABLED`.
    pub memory_digest_enabled: Option<bool>,
    /// `MOA_MEMORY_DIGEST_MAX_TOKENS`.
    pub memory_digest_max_tokens: Option<usize>,
    /// `MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS`.
    pub memory_digest_rebuild_min_interval_hours: Option<i64>,
    /// `MOA_MEMORY_EXTRACTION_ENABLED`.
    pub memory_extraction_enabled: Option<bool>,
    /// `MOA_MEMORY_EXTRACTION_API_KEY_ENV`.
    pub memory_extraction_api_key_env: Option<String>,
    /// `MOA_MEMORY_EXTRACTION_MODEL`.
    pub memory_extraction_model: Option<String>,
    /// `MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK`.
    pub memory_extraction_max_facts_per_chunk: Option<usize>,
    /// `MOA_MEMORY_EXTRACTION_TIMEOUT_MS`.
    pub memory_extraction_timeout_ms: Option<u64>,
    /// `MOA_MEMORY_VECTOR_EMBEDDER_NAME`.
    pub memory_vector_embedder_name: Option<String>,
    /// `MOA_MEMORY_VECTOR_EMBEDDER_OUTPUT_DIM`.
    pub memory_vector_embedder_output_dim: Option<usize>,
    /// `MOA_MEMORY_VECTOR_EMBEDDER_COHERE_API_KEY_ENV`.
    pub memory_vector_embedder_cohere_api_key_env: Option<String>,
    /// `MOA_MEMORY_VECTOR_EMBEDDER_GEMINI_API_KEY_ENV`.
    pub memory_vector_embedder_gemini_api_key_env: Option<String>,
    /// `MOA_MEMORY_VECTOR_EMBEDDER_GEMINI_DEFAULT_ROLE`.
    pub memory_vector_embedder_gemini_default_role: Option<String>,
    /// `MOA_PII_SERVICE_URL`.
    pub pii_service_url: Option<String>,
    /// `MOA_TURBOPUFFER_API_KEY_ENV`.
    pub turbopuffer_api_key_env: Option<String>,
    /// `MOA_TURBOPUFFER_BASE_URL`.
    pub turbopuffer_base_url: Option<String>,
    /// `MOA_TURBOPUFFER_ENVIRONMENT`.
    pub turbopuffer_environment: Option<String>,
    /// `MOA_TURBOPUFFER_BAA`.
    pub turbopuffer_baa: Option<bool>,
    /// `MOA_CLOUD_ENABLED`.
    pub cloud_enabled: Option<bool>,
    /// `MOA_CLOUD_MEMORY_DIR`.
    pub cloud_memory_dir: Option<String>,
    /// `MOA_CLOUD_FLYIO_API_TOKEN_ENV`.
    pub cloud_flyio_api_token_env: Option<String>,
    /// `MOA_CLOUD_FLYIO_APP_NAME`.
    pub cloud_flyio_app_name: Option<String>,
    /// `MOA_CLOUD_FLYIO_REGION`.
    pub cloud_flyio_region: Option<String>,
    /// `MOA_CLOUD_FLYIO_INTERNAL_PORT`.
    pub cloud_flyio_internal_port: Option<u16>,
    /// `MOA_CLOUD_FLYIO_HEALTH_BIND`.
    pub cloud_flyio_health_bind: Option<String>,
    /// `MOA_CLOUD_FLYIO_GRACEFUL_SHUTDOWN_TIMEOUT_SECS`.
    pub cloud_flyio_graceful_shutdown_timeout_secs: Option<u64>,
    /// `MOA_CLOUD_HANDS_DEFAULT_PROVIDER`.
    pub cloud_hands_default_provider: Option<String>,
    /// `MOA_CLOUD_HANDS_DAYTONA_API_KEY_ENV`.
    pub cloud_hands_daytona_api_key_env: Option<String>,
    /// `MOA_CLOUD_HANDS_DAYTONA_API_URL`.
    pub cloud_hands_daytona_api_url: Option<String>,
    /// `MOA_CLOUD_HANDS_DAYTONA_DEFAULT_IMAGE`.
    pub cloud_hands_daytona_default_image: Option<String>,
    /// `MOA_CLOUD_HANDS_E2B_API_KEY_ENV`.
    pub cloud_hands_e2b_api_key_env: Option<String>,
    /// `MOA_CLOUD_HANDS_E2B_API_URL`.
    pub cloud_hands_e2b_api_url: Option<String>,
    /// `MOA_CLOUD_HANDS_E2B_DOMAIN`.
    pub cloud_hands_e2b_domain: Option<String>,
    /// `MOA_CLOUD_HANDS_E2B_TEMPLATE`.
    pub cloud_hands_e2b_template: Option<String>,
    /// `MOA_GATEWAY_TELEGRAM_TOKEN_ENV`.
    pub gateway_telegram_token_env: Option<String>,
    /// `MOA_GATEWAY_SLACK_TOKEN_ENV`.
    pub gateway_slack_token_env: Option<String>,
    /// `MOA_GATEWAY_SLACK_APP_TOKEN_ENV`.
    pub gateway_slack_app_token_env: Option<String>,
    /// `MOA_GATEWAY_DISCORD_TOKEN_ENV`.
    pub gateway_discord_token_env: Option<String>,
    /// `MOA_PERMISSIONS_DEFAULT_POSTURE`.
    pub permissions_default_posture: Option<String>,
    /// `MOA_PERMISSIONS_AUTO_APPROVE`.
    pub permissions_auto_approve: Option<Vec<String>>,
    /// `MOA_PERMISSIONS_ALWAYS_DENY`.
    pub permissions_always_deny: Option<Vec<String>>,
    /// `MOA_SESSION_BLOB_THRESHOLD_BYTES`.
    pub session_blob_threshold_bytes: Option<usize>,
    /// `MOA_SESSION_BLOB_DIR`.
    pub session_blob_dir: Option<String>,
    /// `MOA_COMPACTION_ENABLED`.
    pub compaction_enabled: Option<bool>,
    /// `MOA_COMPACTION_EVENT_THRESHOLD`.
    pub compaction_event_threshold: Option<usize>,
    /// `MOA_COMPACTION_TOKEN_RATIO_THRESHOLD`.
    pub compaction_token_ratio_threshold: Option<f64>,
    /// `MOA_COMPACTION_RECENT_TURNS_VERBATIM`.
    pub compaction_recent_turns_verbatim: Option<usize>,
    /// `MOA_COMPACTION_PRESERVE_ERRORS`.
    pub compaction_preserve_errors: Option<bool>,
    /// `MOA_COMPACTION_TIER2_TRIGGER_BLOCKS_PAST_BP4`.
    pub compaction_tier2_trigger_blocks_past_bp4: Option<usize>,
    /// `MOA_COMPACTION_TIER3_TRIGGER_FRACTION`.
    pub compaction_tier3_trigger_fraction: Option<f64>,
    /// `MOA_COMPACTION_MAX_INPUT_TOKENS_PER_TURN`.
    pub compaction_max_input_tokens_per_turn: Option<usize>,
    /// `MOA_RESTATE_INGRESS_URL`.
    pub restate_ingress_url: Option<String>,
    /// `MOA_RESTATE_ADMIN_URL`.
    pub restate_admin_url: Option<String>,
    /// `MOA_RESTATE_LLM_GATEWAY_URL`.
    pub restate_llm_gateway_url: Option<String>,
    /// `MOA_ORCHESTRATOR_ENDPOINT`.
    pub orchestrator_endpoint: Option<String>,
    /// `MOA_ORCHESTRATOR_HEALTH_URL`.
    pub orchestrator_health_url: Option<String>,
    /// `MOA_OBSERVABILITY_ENABLED`.
    pub observability_enabled: Option<bool>,
    /// `MOA_OBSERVABILITY_SERVICE_NAME`.
    pub observability_service_name: Option<String>,
    /// `MOA_OBSERVABILITY_OTLP_ENDPOINT`.
    pub observability_otlp_endpoint: Option<String>,
    /// `MOA_OBSERVABILITY_OTLP_PROTOCOL`.
    pub observability_otlp_protocol: Option<OtlpProtocol>,
    /// `MOA_OBSERVABILITY_OTLP_HEADERS`.
    pub observability_otlp_headers: Option<HashMap<String, String>>,
    /// `MOA_OBSERVABILITY_ENVIRONMENT`.
    pub observability_environment: Option<String>,
    /// `MOA_OBSERVABILITY_RELEASE`.
    pub observability_release: Option<String>,
    /// `MOA_OBSERVABILITY_SAMPLE_RATE`.
    pub observability_sample_rate: Option<f64>,
    /// `MOA_OBSERVABILITY_LINEAGE_ENABLED`.
    pub observability_lineage_enabled: Option<bool>,
    /// `MOA_OBSERVABILITY_LINEAGE_CHANNEL_CAPACITY`.
    pub observability_lineage_channel_capacity: Option<usize>,
    /// `MOA_OBSERVABILITY_LINEAGE_BATCH_SIZE`.
    pub observability_lineage_batch_size: Option<usize>,
    /// `MOA_OBSERVABILITY_LINEAGE_BATCH_MAX_AGE_SECS`.
    pub observability_lineage_batch_max_age_secs: Option<u64>,
    /// `MOA_OBSERVABILITY_LINEAGE_JOURNAL_PATH`.
    pub observability_lineage_journal_path: Option<String>,
    /// `MOA_OBSERVABILITY_LINEAGE_SAMPLE_PGVECTOR_EXPLAIN`.
    pub observability_lineage_sample_pgvector_explain: Option<f64>,
    /// `MOA_METRICS_ENABLED`.
    pub metrics_enabled: Option<bool>,
    /// `MOA_METRICS_LISTEN`.
    pub metrics_listen: Option<String>,
    /// `MOA_BUDGETS_DAILY_WORKSPACE_CENTS`.
    pub budgets_daily_workspace_cents: Option<u32>,
    /// `MOA_SESSION_LIMITS_MAX_TURNS`.
    pub session_limits_max_turns: Option<u32>,
    /// `MOA_SESSION_LIMITS_LOOP_DETECTION_THRESHOLD`.
    pub session_limits_loop_detection_threshold: Option<u32>,
    /// `MOA_TOOL_OUTPUT_MAX_REPLAY_CHARS`.
    pub tool_output_max_replay_chars: Option<usize>,
    /// `MOA_TOOL_OUTPUT_MAX_BASH_LINES`.
    pub tool_output_max_bash_lines: Option<usize>,
    /// `MOA_TOOL_OUTPUT_HEAD_RATIO`.
    pub tool_output_head_ratio: Option<f64>,
    /// `MOA_TOOL_BUDGETS_FILE_READ`.
    pub tool_budgets_file_read: Option<u32>,
    /// `MOA_TOOL_BUDGETS_BASH_STDOUT`.
    pub tool_budgets_bash_stdout: Option<u32>,
    /// `MOA_TOOL_BUDGETS_BASH_STDERR`.
    pub tool_budgets_bash_stderr: Option<u32>,
    /// `MOA_TOOL_BUDGETS_GREP`.
    pub tool_budgets_grep: Option<u32>,
    /// `MOA_TOOL_BUDGETS_FILE_SEARCH`.
    pub tool_budgets_file_search: Option<u32>,
    /// `MOA_TOOL_BUDGETS_MEMORY_SEARCH`.
    pub tool_budgets_memory_search: Option<u32>,
    /// `MOA_TOOL_BUDGETS_FILE_OUTLINE`.
    pub tool_budgets_file_outline: Option<u32>,
    /// `MOA_TOOL_BUDGETS_DEFAULT`.
    pub tool_budgets_default: Option<u32>,
    /// `MOA_SKILL_BUDGET_MAX_MANIFEST_CHARS`.
    pub skill_budget_max_manifest_chars: Option<usize>,
    /// `MOA_SKILL_BUDGET_MAX_PER_SKILL_CHARS`.
    pub skill_budget_max_per_skill_chars: Option<usize>,
    /// `MOA_SKILL_BUDGET_SHOW_TOKEN_ESTIMATES`.
    pub skill_budget_show_token_estimates: Option<bool>,
    /// `MOA_QUERY_REWRITE_ENABLED`.
    pub query_rewrite_enabled: Option<bool>,
    /// `MOA_QUERY_REWRITE_MODEL`.
    pub query_rewrite_model: Option<String>,
    /// `MOA_QUERY_REWRITE_TIMEOUT_MS`.
    pub query_rewrite_timeout_ms: Option<u64>,
    /// `MOA_QUERY_REWRITE_MIN_QUERY_TOKENS`.
    pub query_rewrite_min_query_tokens: Option<usize>,
    /// `MOA_QUERY_REWRITE_SKIP_SINGLE_TURN`.
    pub query_rewrite_skip_single_turn: Option<bool>,
    /// `MOA_QUERY_REWRITE_CIRCUIT_BREAKER_THRESHOLD`.
    pub query_rewrite_circuit_breaker_threshold: Option<f64>,
    /// `MOA_QUERY_REWRITE_CIRCUIT_BREAKER_WINDOW_SECS`.
    pub query_rewrite_circuit_breaker_window_secs: Option<u64>,
    /// `MOA_QUERY_REWRITE_CIRCUIT_BREAKER_COOLDOWN_SECS`.
    pub query_rewrite_circuit_breaker_cooldown_secs: Option<u64>,
    /// `MOA_RESOLUTION_ENABLED`.
    pub resolution_enabled: Option<bool>,
    /// `MOA_RESOLUTION_WEIGHTS_TOOL`.
    pub resolution_weights_tool: Option<f64>,
    /// `MOA_RESOLUTION_WEIGHTS_VERIFICATION`.
    pub resolution_weights_verification: Option<f64>,
    /// `MOA_RESOLUTION_WEIGHTS_CONTINUATION`.
    pub resolution_weights_continuation: Option<f64>,
    /// `MOA_RESOLUTION_WEIGHTS_SELF_ASSESSMENT`.
    pub resolution_weights_self_assessment: Option<f64>,
    /// `MOA_RESOLUTION_WEIGHTS_STRUCTURAL`.
    pub resolution_weights_structural: Option<f64>,
    /// `MOA_RESOLUTION_USE_LLM_SELF_ASSESSMENT`.
    pub resolution_use_llm_self_assessment: Option<bool>,
    /// `MOA_RESOLUTION_SELF_ASSESSMENT_TIMEOUT_MS`.
    pub resolution_self_assessment_timeout_ms: Option<u64>,
    /// `MOA_RESOLUTION_REPHRASE_SIMILARITY_THRESHOLD`.
    pub resolution_rephrase_similarity_threshold: Option<f64>,
    /// `MOA_RESOLUTION_STRUCTURAL_MIN_SAMPLES`.
    pub resolution_structural_min_samples: Option<usize>,
    /// `MOA_RESOLUTION_IDLE_TIMEOUT_MINUTES`.
    pub resolution_idle_timeout_minutes: Option<u64>,
    /// `MOA_CONTEXT_SNAPSHOT_ENABLED`.
    pub context_snapshot_enabled: Option<bool>,
    /// `MOA_CONTEXT_SNAPSHOT_MAX_SIZE_BYTES`.
    pub context_snapshot_max_size_bytes: Option<usize>,
}

impl MoaEnvOverlay {
    /// Loads a flat `MOA_` environment overlay from process environment variables.
    pub fn from_env() -> Result<Self> {
        RawMoaEnvOverlay::from_env()?.try_into()
    }

    /// Loads a flat `MOA_` environment overlay from deterministic key-value pairs.
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter(iter: impl IntoIterator<Item = (String, String)>) -> Result<Self> {
        RawMoaEnvOverlay::from_iter(iter)?.try_into()
    }

    /// Applies this overlay to a typed MOA config.
    pub fn apply_to(&self, config: &mut MoaConfig) -> Result<()> {
        set_if_some(
            &mut config.general.default_provider,
            &self.general_default_provider,
        );
        set_if_some(
            &mut config.general.reasoning_effort,
            &self.general_reasoning_effort,
        );
        set_copy_if_some(
            &mut config.general.web_search_enabled,
            self.general_web_search_enabled,
        );
        set_option_if_some(
            &mut config.general.workspace_instructions,
            &self.general_workspace_instructions,
        );
        set_option_if_some(
            &mut config.general.user_instructions,
            &self.general_user_instructions,
        );
        set_if_some(&mut config.models.main, &self.models_main);
        set_option_if_some(&mut config.models.auxiliary, &self.models_auxiliary);
        set_if_some(
            &mut config.providers.anthropic.api_key_env,
            &self.providers_anthropic_api_key_env,
        );
        set_if_some(
            &mut config.providers.openai.api_key_env,
            &self.providers_openai_api_key_env,
        );
        set_if_some(
            &mut config.providers.google.api_key_env,
            &self.providers_google_api_key_env,
        );
        set_if_some(&mut config.database.url, &self.database_url);
        set_option_if_some(&mut config.database.admin_url, &self.database_admin_url);
        set_option_if_some(&mut config.database.schema, &self.database_schema);
        set_copy_if_some(
            &mut config.database.max_connections,
            self.database_max_connections,
        );
        set_copy_if_some(
            &mut config.database.connect_timeout_seconds,
            self.database_connect_timeout_seconds,
        );
        set_copy_if_some(
            &mut config.database.neon.enabled,
            self.database_neon_enabled,
        );
        set_if_some(
            &mut config.database.neon.api_key_env,
            &self.database_neon_api_key_env,
        );
        set_if_some(
            &mut config.database.neon.project_id,
            &self.database_neon_project_id,
        );
        set_if_some(
            &mut config.database.neon.parent_branch_id,
            &self.database_neon_parent_branch_id,
        );
        set_copy_if_some(
            &mut config.database.neon.max_checkpoints,
            self.database_neon_max_checkpoints,
        );
        set_copy_if_some(
            &mut config.database.neon.checkpoint_ttl_hours,
            self.database_neon_checkpoint_ttl_hours,
        );
        set_copy_if_some(&mut config.database.neon.pooled, self.database_neon_pooled);
        set_copy_if_some(
            &mut config.database.neon.suspend_timeout_seconds,
            self.database_neon_suspend_timeout_seconds,
        );
        set_copy_if_some(&mut config.auth.provider, self.auth_provider);
        set_copy_if_some(&mut config.auth.header_trust, self.auth_header_trust);
        self.apply_auth0(config)?;
        self.apply_oidc(config)?;
        set_copy_if_some(&mut config.authz.engine, self.authz_engine);
        self.apply_openfga(config)?;
        set_copy_if_some(&mut config.token_vault.provider, self.token_vault_provider);
        set_copy_if_some(&mut config.async_authz.provider, self.async_authz_provider);
        set_copy_if_some(
            &mut config.async_authz.default_timeout_secs,
            self.async_authz_default_timeout_secs,
        );
        set_copy_if_some(
            &mut config.audit_security.emit_authz_allows,
            self.audit_security_emit_authz_allows,
        );
        set_copy_if_some(&mut config.local.docker_enabled, self.local_docker_enabled);
        set_if_some(&mut config.local.sandbox_dir, &self.local_sandbox_dir);
        set_if_some(&mut config.local.memory_dir, &self.local_memory_dir);
        set_copy_if_some(
            &mut config.memory.auto_bootstrap,
            self.memory_auto_bootstrap,
        );
        set_if_some(
            &mut config.memory.embedding_provider,
            &self.memory_embedding_provider,
        );
        set_if_some(
            &mut config.memory.embedding_model,
            &self.memory_embedding_model,
        );
        set_copy_if_some(
            &mut config.memory.retrieval.reranker_mode,
            self.memory_retrieval_reranker_mode,
        );
        set_copy_if_some(
            &mut config.memory.digest.enabled,
            self.memory_digest_enabled,
        );
        set_copy_if_some(
            &mut config.memory.digest.max_tokens,
            self.memory_digest_max_tokens,
        );
        set_copy_if_some(
            &mut config.memory.digest.rebuild_min_interval_hours,
            self.memory_digest_rebuild_min_interval_hours,
        );
        set_copy_if_some(
            &mut config.memory.extraction.enabled,
            self.memory_extraction_enabled,
        );
        set_if_some(
            &mut config.memory.extraction.api_key_env,
            &self.memory_extraction_api_key_env,
        );
        set_if_some(
            &mut config.memory.extraction.model,
            &self.memory_extraction_model,
        );
        set_copy_if_some(
            &mut config.memory.extraction.max_facts_per_chunk,
            self.memory_extraction_max_facts_per_chunk,
        );
        set_copy_if_some(
            &mut config.memory.extraction.timeout_ms,
            self.memory_extraction_timeout_ms,
        );
        set_if_some(
            &mut config.memory.vector.embedder.name,
            &self.memory_vector_embedder_name,
        );
        set_copy_if_some(
            &mut config.memory.vector.embedder.output_dim,
            self.memory_vector_embedder_output_dim,
        );
        set_if_some(
            &mut config.memory.vector.embedder.cohere.api_key_env,
            &self.memory_vector_embedder_cohere_api_key_env,
        );
        set_if_some(
            &mut config.memory.vector.embedder.gemini.api_key_env,
            &self.memory_vector_embedder_gemini_api_key_env,
        );
        set_if_some(
            &mut config.memory.vector.embedder.gemini.default_role,
            &self.memory_vector_embedder_gemini_default_role,
        );
        set_option_if_some(&mut config.memory.pii_service_url, &self.pii_service_url);
        set_if_some(
            &mut config.memory.vector.turbopuffer.api_key_env,
            &self.turbopuffer_api_key_env,
        );
        set_option_if_some(
            &mut config.memory.vector.turbopuffer.base_url,
            &self.turbopuffer_base_url,
        );
        set_option_if_some(
            &mut config.memory.vector.turbopuffer.environment,
            &self.turbopuffer_environment,
        );
        set_copy_if_some(
            &mut config.memory.vector.turbopuffer.baa_enabled,
            self.turbopuffer_baa,
        );
        set_copy_if_some(&mut config.cloud.enabled, self.cloud_enabled);
        set_option_if_some(&mut config.cloud.memory_dir, &self.cloud_memory_dir);
        self.apply_cloud(config);
        set_if_some(
            &mut config.gateway.telegram_token_env,
            &self.gateway_telegram_token_env,
        );
        set_if_some(
            &mut config.gateway.slack_token_env,
            &self.gateway_slack_token_env,
        );
        set_if_some(
            &mut config.gateway.slack_app_token_env,
            &self.gateway_slack_app_token_env,
        );
        set_if_some(
            &mut config.gateway.discord_token_env,
            &self.gateway_discord_token_env,
        );
        set_if_some(
            &mut config.permissions.default_posture,
            &self.permissions_default_posture,
        );
        set_vec_if_some(
            &mut config.permissions.auto_approve,
            &self.permissions_auto_approve,
        );
        set_vec_if_some(
            &mut config.permissions.always_deny,
            &self.permissions_always_deny,
        );
        set_copy_if_some(
            &mut config.session.blob_threshold_bytes,
            self.session_blob_threshold_bytes,
        );
        set_if_some(&mut config.session.blob_dir, &self.session_blob_dir);
        self.apply_compaction(config);
        if let Some(restate_ingress_url) = &self.restate_ingress_url {
            config.orchestrator.restate_ingress_url = Some(restate_ingress_url.clone());
            config.orchestrator.endpoint = Some(restate_ingress_url.clone());
        }
        if let Some(endpoint) = &self.orchestrator_endpoint {
            config.orchestrator.endpoint = Some(endpoint.clone());
        }
        set_option_if_some(
            &mut config.orchestrator.restate_admin_url,
            &self.restate_admin_url,
        );
        set_option_if_some(
            &mut config.orchestrator.llm_gateway_url,
            &self.restate_llm_gateway_url,
        );
        set_option_if_some(
            &mut config.orchestrator.health_url,
            &self.orchestrator_health_url,
        );
        self.apply_observability(config);
        set_copy_if_some(&mut config.metrics.enabled, self.metrics_enabled);
        set_if_some(&mut config.metrics.listen, &self.metrics_listen);
        set_copy_if_some(
            &mut config.budgets.daily_workspace_cents,
            self.budgets_daily_workspace_cents,
        );
        set_copy_if_some(
            &mut config.session_limits.max_turns,
            self.session_limits_max_turns,
        );
        set_copy_if_some(
            &mut config.session_limits.loop_detection_threshold,
            self.session_limits_loop_detection_threshold,
        );
        self.apply_tooling(config);
        self.apply_query_rewrite(config);
        self.apply_resolution(config);
        set_copy_if_some(
            &mut config.context_snapshot.enabled,
            self.context_snapshot_enabled,
        );
        set_copy_if_some(
            &mut config.context_snapshot.max_size_bytes,
            self.context_snapshot_max_size_bytes,
        );
        config.validate()
    }

    fn apply_auth0(&self, config: &mut MoaConfig) -> Result<()> {
        if !any_present(&[
            self.auth_auth0_domain.is_some(),
            self.auth_auth0_audience.is_some(),
            self.auth_auth0_client_id_env.is_some(),
            self.auth_auth0_client_secret_env.is_some(),
        ]) {
            return Ok(());
        }

        let mut auth0 = config
            .auth
            .auth0
            .clone()
            .unwrap_or_else(|| Auth0AuthConfig {
                domain: String::new(),
                audience: String::new(),
                client_id_env: String::new(),
                client_secret_env: String::new(),
            });
        set_if_some(&mut auth0.domain, &self.auth_auth0_domain);
        set_if_some(&mut auth0.audience, &self.auth_auth0_audience);
        set_if_some(&mut auth0.client_id_env, &self.auth_auth0_client_id_env);
        set_if_some(
            &mut auth0.client_secret_env,
            &self.auth_auth0_client_secret_env,
        );
        require_non_empty("MOA_AUTH_AUTH0_DOMAIN", &auth0.domain)?;
        require_non_empty("MOA_AUTH_AUTH0_AUDIENCE", &auth0.audience)?;
        require_non_empty("MOA_AUTH_AUTH0_CLIENT_ID_ENV", &auth0.client_id_env)?;
        require_non_empty("MOA_AUTH_AUTH0_CLIENT_SECRET_ENV", &auth0.client_secret_env)?;
        config.auth.auth0 = Some(auth0);
        Ok(())
    }

    fn apply_oidc(&self, config: &mut MoaConfig) -> Result<()> {
        if !any_present(&[
            self.auth_oidc_issuer.is_some(),
            self.auth_oidc_audience.is_some(),
            self.auth_oidc_jwks_url.is_some(),
        ]) {
            return Ok(());
        }

        let mut oidc = config.auth.oidc.clone().unwrap_or_else(|| OidcAuthConfig {
            issuer: String::new(),
            audience: String::new(),
            jwks_url: String::new(),
        });
        set_if_some(&mut oidc.issuer, &self.auth_oidc_issuer);
        set_if_some(&mut oidc.audience, &self.auth_oidc_audience);
        set_if_some(&mut oidc.jwks_url, &self.auth_oidc_jwks_url);
        require_non_empty("MOA_AUTH_OIDC_ISSUER", &oidc.issuer)?;
        require_non_empty("MOA_AUTH_OIDC_AUDIENCE", &oidc.audience)?;
        require_non_empty("MOA_AUTH_OIDC_JWKS_URL", &oidc.jwks_url)?;
        config.auth.oidc = Some(oidc);
        Ok(())
    }

    fn apply_openfga(&self, config: &mut MoaConfig) -> Result<()> {
        if !any_present(&[
            self.authz_openfga_url.is_some(),
            self.authz_openfga_preshared_key.is_some(),
            self.authz_openfga_store_id.is_some(),
            self.authz_openfga_model_id.is_some(),
            self.authz_openfga_timeout_ms.is_some(),
        ]) {
            return Ok(());
        }

        let mut openfga = config
            .authz
            .openfga
            .clone()
            .unwrap_or_else(|| OpenFgaConfig {
                url: String::new(),
                preshared_key: String::new(),
                store_id: String::new(),
                model_id: String::new(),
                timeout_ms: OPENFGA_DEFAULT_TIMEOUT_MS,
            });
        set_if_some(&mut openfga.url, &self.authz_openfga_url);
        set_if_some(
            &mut openfga.preshared_key,
            &self.authz_openfga_preshared_key,
        );
        set_if_some(&mut openfga.store_id, &self.authz_openfga_store_id);
        set_if_some(&mut openfga.model_id, &self.authz_openfga_model_id);
        set_copy_if_some(&mut openfga.timeout_ms, self.authz_openfga_timeout_ms);
        require_non_empty("MOA_AUTHZ_OPENFGA_URL", &openfga.url)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_PRESHARED_KEY", &openfga.preshared_key)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_STORE_ID", &openfga.store_id)?;
        require_non_empty("MOA_AUTHZ_OPENFGA_MODEL_ID", &openfga.model_id)?;
        config.authz.openfga = Some(openfga);
        Ok(())
    }

    fn apply_cloud(&self, config: &mut MoaConfig) {
        if any_present(&[
            self.cloud_flyio_api_token_env.is_some(),
            self.cloud_flyio_app_name.is_some(),
            self.cloud_flyio_region.is_some(),
            self.cloud_flyio_internal_port.is_some(),
            self.cloud_flyio_health_bind.is_some(),
            self.cloud_flyio_graceful_shutdown_timeout_secs.is_some(),
        ]) {
            let flyio = config
                .cloud
                .flyio
                .get_or_insert_with(CloudFlyioConfig::default);
            set_option_if_some(&mut flyio.api_token_env, &self.cloud_flyio_api_token_env);
            set_option_if_some(&mut flyio.app_name, &self.cloud_flyio_app_name);
            set_if_some(&mut flyio.region, &self.cloud_flyio_region);
            set_copy_if_some(&mut flyio.internal_port, self.cloud_flyio_internal_port);
            set_if_some(&mut flyio.health_bind, &self.cloud_flyio_health_bind);
            set_copy_if_some(
                &mut flyio.graceful_shutdown_timeout_secs,
                self.cloud_flyio_graceful_shutdown_timeout_secs,
            );
        }

        if any_present(&[
            self.cloud_hands_default_provider.is_some(),
            self.cloud_hands_daytona_api_key_env.is_some(),
            self.cloud_hands_daytona_api_url.is_some(),
            self.cloud_hands_daytona_default_image.is_some(),
            self.cloud_hands_e2b_api_key_env.is_some(),
            self.cloud_hands_e2b_api_url.is_some(),
            self.cloud_hands_e2b_domain.is_some(),
            self.cloud_hands_e2b_template.is_some(),
        ]) {
            let hands = config
                .cloud
                .hands
                .get_or_insert_with(CloudHandsConfig::default);
            set_option_if_some(
                &mut hands.default_provider,
                &self.cloud_hands_default_provider,
            );
            set_option_if_some(
                &mut hands.daytona_api_key_env,
                &self.cloud_hands_daytona_api_key_env,
            );
            set_option_if_some(
                &mut hands.daytona_api_url,
                &self.cloud_hands_daytona_api_url,
            );
            set_option_if_some(
                &mut hands.daytona_default_image,
                &self.cloud_hands_daytona_default_image,
            );
            set_option_if_some(
                &mut hands.e2b_api_key_env,
                &self.cloud_hands_e2b_api_key_env,
            );
            set_option_if_some(&mut hands.e2b_api_url, &self.cloud_hands_e2b_api_url);
            set_option_if_some(&mut hands.e2b_domain, &self.cloud_hands_e2b_domain);
            set_option_if_some(&mut hands.e2b_template, &self.cloud_hands_e2b_template);
        }
    }

    fn apply_compaction(&self, config: &mut MoaConfig) {
        set_copy_if_some(&mut config.compaction.enabled, self.compaction_enabled);
        set_copy_if_some(
            &mut config.compaction.event_threshold,
            self.compaction_event_threshold,
        );
        set_copy_if_some(
            &mut config.compaction.token_ratio_threshold,
            self.compaction_token_ratio_threshold,
        );
        set_copy_if_some(
            &mut config.compaction.recent_turns_verbatim,
            self.compaction_recent_turns_verbatim,
        );
        set_copy_if_some(
            &mut config.compaction.preserve_errors,
            self.compaction_preserve_errors,
        );
        set_copy_if_some(
            &mut config.compaction.tier2_trigger_blocks_past_bp4,
            self.compaction_tier2_trigger_blocks_past_bp4,
        );
        set_copy_if_some(
            &mut config.compaction.tier3_trigger_fraction,
            self.compaction_tier3_trigger_fraction,
        );
        set_copy_if_some(
            &mut config.compaction.max_input_tokens_per_turn,
            self.compaction_max_input_tokens_per_turn,
        );
    }

    fn apply_observability(&self, config: &mut MoaConfig) {
        set_copy_if_some(
            &mut config.observability.enabled,
            self.observability_enabled,
        );
        set_if_some(
            &mut config.observability.service_name,
            &self.observability_service_name,
        );
        set_option_if_some(
            &mut config.observability.otlp_endpoint,
            &self.observability_otlp_endpoint,
        );
        set_copy_if_some(
            &mut config.observability.otlp_protocol,
            self.observability_otlp_protocol,
        );
        if let Some(headers) = &self.observability_otlp_headers {
            config.observability.otlp_headers = headers.clone();
        }
        set_option_if_some(
            &mut config.observability.environment,
            &self.observability_environment,
        );
        set_option_if_some(
            &mut config.observability.release,
            &self.observability_release,
        );
        set_copy_if_some(
            &mut config.observability.sample_rate,
            self.observability_sample_rate,
        );
        set_copy_if_some(
            &mut config.observability.lineage.enabled,
            self.observability_lineage_enabled,
        );
        set_copy_if_some(
            &mut config.observability.lineage.channel_capacity,
            self.observability_lineage_channel_capacity,
        );
        set_copy_if_some(
            &mut config.observability.lineage.batch_size,
            self.observability_lineage_batch_size,
        );
        set_copy_if_some(
            &mut config.observability.lineage.batch_max_age_secs,
            self.observability_lineage_batch_max_age_secs,
        );
        set_if_some(
            &mut config.observability.lineage.journal_path,
            &self.observability_lineage_journal_path,
        );
        set_copy_if_some(
            &mut config.observability.lineage.sample_pgvector_explain,
            self.observability_lineage_sample_pgvector_explain,
        );
    }

    fn apply_tooling(&self, config: &mut MoaConfig) {
        set_copy_if_some(
            &mut config.tool_output.max_replay_chars,
            self.tool_output_max_replay_chars,
        );
        set_copy_if_some(
            &mut config.tool_output.max_bash_lines,
            self.tool_output_max_bash_lines,
        );
        set_copy_if_some(
            &mut config.tool_output.head_ratio,
            self.tool_output_head_ratio,
        );
        set_copy_if_some(
            &mut config.tool_budgets.file_read,
            self.tool_budgets_file_read,
        );
        set_copy_if_some(
            &mut config.tool_budgets.bash_stdout,
            self.tool_budgets_bash_stdout,
        );
        set_copy_if_some(
            &mut config.tool_budgets.bash_stderr,
            self.tool_budgets_bash_stderr,
        );
        set_copy_if_some(&mut config.tool_budgets.grep, self.tool_budgets_grep);
        set_copy_if_some(
            &mut config.tool_budgets.file_search,
            self.tool_budgets_file_search,
        );
        set_copy_if_some(
            &mut config.tool_budgets.memory_search,
            self.tool_budgets_memory_search,
        );
        set_copy_if_some(
            &mut config.tool_budgets.file_outline,
            self.tool_budgets_file_outline,
        );
        set_copy_if_some(&mut config.tool_budgets.default, self.tool_budgets_default);
        if let Some(max_manifest_chars) = self.skill_budget_max_manifest_chars {
            config.skill_budget.max_manifest_chars = Some(max_manifest_chars);
        }
        set_copy_if_some(
            &mut config.skill_budget.max_per_skill_chars,
            self.skill_budget_max_per_skill_chars,
        );
        set_copy_if_some(
            &mut config.skill_budget.show_token_estimates,
            self.skill_budget_show_token_estimates,
        );
    }

    fn apply_query_rewrite(&self, config: &mut MoaConfig) {
        set_copy_if_some(
            &mut config.query_rewrite.enabled,
            self.query_rewrite_enabled,
        );
        set_option_if_some(&mut config.query_rewrite.model, &self.query_rewrite_model);
        set_copy_if_some(
            &mut config.query_rewrite.timeout_ms,
            self.query_rewrite_timeout_ms,
        );
        set_copy_if_some(
            &mut config.query_rewrite.min_query_tokens,
            self.query_rewrite_min_query_tokens,
        );
        set_copy_if_some(
            &mut config.query_rewrite.skip_single_turn,
            self.query_rewrite_skip_single_turn,
        );
        set_copy_if_some(
            &mut config.query_rewrite.circuit_breaker_threshold,
            self.query_rewrite_circuit_breaker_threshold,
        );
        set_copy_if_some(
            &mut config.query_rewrite.circuit_breaker_window_secs,
            self.query_rewrite_circuit_breaker_window_secs,
        );
        set_copy_if_some(
            &mut config.query_rewrite.circuit_breaker_cooldown_secs,
            self.query_rewrite_circuit_breaker_cooldown_secs,
        );
    }

    fn apply_resolution(&self, config: &mut MoaConfig) {
        set_copy_if_some(&mut config.resolution.enabled, self.resolution_enabled);
        set_copy_if_some(
            &mut config.resolution.weights.tool,
            self.resolution_weights_tool,
        );
        set_copy_if_some(
            &mut config.resolution.weights.verification,
            self.resolution_weights_verification,
        );
        set_copy_if_some(
            &mut config.resolution.weights.continuation,
            self.resolution_weights_continuation,
        );
        set_copy_if_some(
            &mut config.resolution.weights.self_assessment,
            self.resolution_weights_self_assessment,
        );
        set_copy_if_some(
            &mut config.resolution.weights.structural,
            self.resolution_weights_structural,
        );
        set_copy_if_some(
            &mut config.resolution.use_llm_self_assessment,
            self.resolution_use_llm_self_assessment,
        );
        set_copy_if_some(
            &mut config.resolution.self_assessment_timeout_ms,
            self.resolution_self_assessment_timeout_ms,
        );
        set_copy_if_some(
            &mut config.resolution.rephrase_similarity_threshold,
            self.resolution_rephrase_similarity_threshold,
        );
        set_copy_if_some(
            &mut config.resolution.structural_min_samples,
            self.resolution_structural_min_samples,
        );
        set_copy_if_some(
            &mut config.resolution.idle_timeout_minutes,
            self.resolution_idle_timeout_minutes,
        );
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RawMoaEnvOverlay {
    general_default_provider: Option<String>,
    general_reasoning_effort: Option<String>,
    general_web_search_enabled: Option<String>,
    general_workspace_instructions: Option<String>,
    general_user_instructions: Option<String>,
    models_main: Option<String>,
    models_auxiliary: Option<String>,
    providers_anthropic_api_key_env: Option<String>,
    providers_openai_api_key_env: Option<String>,
    providers_google_api_key_env: Option<String>,
    database_url: Option<String>,
    database_admin_url: Option<String>,
    database_schema: Option<String>,
    database_max_connections: Option<String>,
    database_connect_timeout_seconds: Option<String>,
    database_neon_enabled: Option<String>,
    database_neon_api_key_env: Option<String>,
    database_neon_project_id: Option<String>,
    database_neon_parent_branch_id: Option<String>,
    database_neon_max_checkpoints: Option<String>,
    database_neon_checkpoint_ttl_hours: Option<String>,
    database_neon_pooled: Option<String>,
    database_neon_suspend_timeout_seconds: Option<String>,
    auth_provider: Option<String>,
    auth_header_trust: Option<String>,
    auth_auth0_domain: Option<String>,
    auth_auth0_audience: Option<String>,
    auth_auth0_client_id_env: Option<String>,
    auth_auth0_client_secret_env: Option<String>,
    auth_oidc_issuer: Option<String>,
    auth_oidc_audience: Option<String>,
    auth_oidc_jwks_url: Option<String>,
    authz_engine: Option<String>,
    authz_openfga_url: Option<String>,
    authz_openfga_preshared_key: Option<String>,
    authz_openfga_store_id: Option<String>,
    authz_openfga_model_id: Option<String>,
    authz_openfga_timeout_ms: Option<String>,
    token_vault_provider: Option<String>,
    async_authz_provider: Option<String>,
    async_authz_default_timeout_secs: Option<String>,
    audit_security_emit_authz_allows: Option<String>,
    local_docker_enabled: Option<String>,
    local_sandbox_dir: Option<String>,
    local_memory_dir: Option<String>,
    memory_auto_bootstrap: Option<String>,
    memory_embedding_provider: Option<String>,
    memory_embedding_model: Option<String>,
    memory_retrieval_reranker_mode: Option<String>,
    memory_digest_enabled: Option<String>,
    memory_digest_max_tokens: Option<String>,
    memory_digest_rebuild_min_interval_hours: Option<String>,
    memory_extraction_enabled: Option<String>,
    memory_extraction_api_key_env: Option<String>,
    memory_extraction_model: Option<String>,
    memory_extraction_max_facts_per_chunk: Option<String>,
    memory_extraction_timeout_ms: Option<String>,
    memory_vector_embedder_name: Option<String>,
    memory_vector_embedder_output_dim: Option<String>,
    memory_vector_embedder_cohere_api_key_env: Option<String>,
    memory_vector_embedder_gemini_api_key_env: Option<String>,
    memory_vector_embedder_gemini_default_role: Option<String>,
    pii_service_url: Option<String>,
    turbopuffer_api_key_env: Option<String>,
    turbopuffer_base_url: Option<String>,
    turbopuffer_environment: Option<String>,
    turbopuffer_baa: Option<String>,
    cloud_enabled: Option<String>,
    cloud_memory_dir: Option<String>,
    cloud_flyio_api_token_env: Option<String>,
    cloud_flyio_app_name: Option<String>,
    cloud_flyio_region: Option<String>,
    cloud_flyio_internal_port: Option<String>,
    cloud_flyio_health_bind: Option<String>,
    cloud_flyio_graceful_shutdown_timeout_secs: Option<String>,
    cloud_hands_default_provider: Option<String>,
    cloud_hands_daytona_api_key_env: Option<String>,
    cloud_hands_daytona_api_url: Option<String>,
    cloud_hands_daytona_default_image: Option<String>,
    cloud_hands_e2b_api_key_env: Option<String>,
    cloud_hands_e2b_api_url: Option<String>,
    cloud_hands_e2b_domain: Option<String>,
    cloud_hands_e2b_template: Option<String>,
    gateway_telegram_token_env: Option<String>,
    gateway_slack_token_env: Option<String>,
    gateway_slack_app_token_env: Option<String>,
    gateway_discord_token_env: Option<String>,
    permissions_default_posture: Option<String>,
    permissions_auto_approve: Option<String>,
    permissions_always_deny: Option<String>,
    session_blob_threshold_bytes: Option<String>,
    session_blob_dir: Option<String>,
    compaction_enabled: Option<String>,
    compaction_event_threshold: Option<String>,
    compaction_token_ratio_threshold: Option<String>,
    compaction_recent_turns_verbatim: Option<String>,
    compaction_preserve_errors: Option<String>,
    compaction_tier2_trigger_blocks_past_bp4: Option<String>,
    compaction_tier3_trigger_fraction: Option<String>,
    compaction_max_input_tokens_per_turn: Option<String>,
    restate_ingress_url: Option<String>,
    restate_admin_url: Option<String>,
    restate_llm_gateway_url: Option<String>,
    orchestrator_endpoint: Option<String>,
    orchestrator_health_url: Option<String>,
    observability_enabled: Option<String>,
    observability_service_name: Option<String>,
    observability_otlp_endpoint: Option<String>,
    observability_otlp_protocol: Option<String>,
    observability_otlp_headers: Option<String>,
    observability_environment: Option<String>,
    observability_release: Option<String>,
    observability_sample_rate: Option<String>,
    observability_lineage_enabled: Option<String>,
    observability_lineage_channel_capacity: Option<String>,
    observability_lineage_batch_size: Option<String>,
    observability_lineage_batch_max_age_secs: Option<String>,
    observability_lineage_journal_path: Option<String>,
    observability_lineage_sample_pgvector_explain: Option<String>,
    metrics_enabled: Option<String>,
    metrics_listen: Option<String>,
    budgets_daily_workspace_cents: Option<String>,
    session_limits_max_turns: Option<String>,
    session_limits_loop_detection_threshold: Option<String>,
    tool_output_max_replay_chars: Option<String>,
    tool_output_max_bash_lines: Option<String>,
    tool_output_head_ratio: Option<String>,
    tool_budgets_file_read: Option<String>,
    tool_budgets_bash_stdout: Option<String>,
    tool_budgets_bash_stderr: Option<String>,
    tool_budgets_grep: Option<String>,
    tool_budgets_file_search: Option<String>,
    tool_budgets_memory_search: Option<String>,
    tool_budgets_file_outline: Option<String>,
    tool_budgets_default: Option<String>,
    skill_budget_max_manifest_chars: Option<String>,
    skill_budget_max_per_skill_chars: Option<String>,
    skill_budget_show_token_estimates: Option<String>,
    query_rewrite_enabled: Option<String>,
    query_rewrite_model: Option<String>,
    query_rewrite_timeout_ms: Option<String>,
    query_rewrite_min_query_tokens: Option<String>,
    query_rewrite_skip_single_turn: Option<String>,
    query_rewrite_circuit_breaker_threshold: Option<String>,
    query_rewrite_circuit_breaker_window_secs: Option<String>,
    query_rewrite_circuit_breaker_cooldown_secs: Option<String>,
    resolution_enabled: Option<String>,
    resolution_weights_tool: Option<String>,
    resolution_weights_verification: Option<String>,
    resolution_weights_continuation: Option<String>,
    resolution_weights_self_assessment: Option<String>,
    resolution_weights_structural: Option<String>,
    resolution_use_llm_self_assessment: Option<String>,
    resolution_self_assessment_timeout_ms: Option<String>,
    resolution_rephrase_similarity_threshold: Option<String>,
    resolution_structural_min_samples: Option<String>,
    resolution_idle_timeout_minutes: Option<String>,
    context_snapshot_enabled: Option<String>,
    context_snapshot_max_size_bytes: Option<String>,
}

impl RawMoaEnvOverlay {
    fn from_env() -> Result<Self> {
        envy::prefixed("MOA_")
            .from_env()
            .map_err(|error| MoaError::ConfigError(format!("MOA env overlay: {error}")))
    }

    fn from_iter(iter: impl IntoIterator<Item = (String, String)>) -> Result<Self> {
        envy::prefixed("MOA_")
            .from_iter(iter)
            .map_err(|error| MoaError::ConfigError(format!("MOA env overlay: {error}")))
    }
}

impl TryFrom<RawMoaEnvOverlay> for MoaEnvOverlay {
    type Error = MoaError;

    fn try_from(raw: RawMoaEnvOverlay) -> Result<Self> {
        Ok(Self {
            general_default_provider: raw.general_default_provider,
            general_reasoning_effort: raw.general_reasoning_effort,
            general_web_search_enabled: parse_optional(
                "MOA_GENERAL_WEB_SEARCH_ENABLED",
                raw.general_web_search_enabled,
            )?,
            general_workspace_instructions: raw.general_workspace_instructions,
            general_user_instructions: raw.general_user_instructions,
            models_main: raw.models_main,
            models_auxiliary: raw.models_auxiliary,
            providers_anthropic_api_key_env: raw.providers_anthropic_api_key_env,
            providers_openai_api_key_env: raw.providers_openai_api_key_env,
            providers_google_api_key_env: raw.providers_google_api_key_env,
            database_url: parse_optional_url("MOA_DATABASE_URL", raw.database_url)?,
            database_admin_url: parse_optional_url(
                "MOA_DATABASE_ADMIN_URL",
                raw.database_admin_url,
            )?,
            database_schema: raw.database_schema,
            database_max_connections: parse_optional(
                "MOA_DATABASE_MAX_CONNECTIONS",
                raw.database_max_connections,
            )?,
            database_connect_timeout_seconds: parse_optional(
                "MOA_DATABASE_CONNECT_TIMEOUT_SECONDS",
                raw.database_connect_timeout_seconds,
            )?,
            database_neon_enabled: parse_optional(
                "MOA_DATABASE_NEON_ENABLED",
                raw.database_neon_enabled,
            )?,
            database_neon_api_key_env: raw.database_neon_api_key_env,
            database_neon_project_id: raw.database_neon_project_id,
            database_neon_parent_branch_id: raw.database_neon_parent_branch_id,
            database_neon_max_checkpoints: parse_optional(
                "MOA_DATABASE_NEON_MAX_CHECKPOINTS",
                raw.database_neon_max_checkpoints,
            )?,
            database_neon_checkpoint_ttl_hours: parse_optional(
                "MOA_DATABASE_NEON_CHECKPOINT_TTL_HOURS",
                raw.database_neon_checkpoint_ttl_hours,
            )?,
            database_neon_pooled: parse_optional(
                "MOA_DATABASE_NEON_POOLED",
                raw.database_neon_pooled,
            )?,
            database_neon_suspend_timeout_seconds: parse_optional(
                "MOA_DATABASE_NEON_SUSPEND_TIMEOUT_SECONDS",
                raw.database_neon_suspend_timeout_seconds,
            )?,
            auth_provider: parse_optional_enum(
                "MOA_AUTH_PROVIDER",
                raw.auth_provider,
                parse_auth_provider,
            )?,
            auth_header_trust: parse_optional_enum(
                "MOA_AUTH_HEADER_TRUST",
                raw.auth_header_trust,
                parse_auth_header_trust,
            )?,
            auth_auth0_domain: raw.auth_auth0_domain,
            auth_auth0_audience: raw.auth_auth0_audience,
            auth_auth0_client_id_env: raw.auth_auth0_client_id_env,
            auth_auth0_client_secret_env: raw.auth_auth0_client_secret_env,
            auth_oidc_issuer: parse_optional_url("MOA_AUTH_OIDC_ISSUER", raw.auth_oidc_issuer)?,
            auth_oidc_audience: raw.auth_oidc_audience,
            auth_oidc_jwks_url: parse_optional_url(
                "MOA_AUTH_OIDC_JWKS_URL",
                raw.auth_oidc_jwks_url,
            )?,
            authz_engine: parse_optional_enum(
                "MOA_AUTHZ_ENGINE",
                raw.authz_engine,
                parse_authz_engine,
            )?,
            authz_openfga_url: parse_optional_url("MOA_AUTHZ_OPENFGA_URL", raw.authz_openfga_url)?,
            authz_openfga_preshared_key: raw.authz_openfga_preshared_key,
            authz_openfga_store_id: raw.authz_openfga_store_id,
            authz_openfga_model_id: raw.authz_openfga_model_id,
            authz_openfga_timeout_ms: parse_optional(
                "MOA_AUTHZ_OPENFGA_TIMEOUT_MS",
                raw.authz_openfga_timeout_ms,
            )?,
            token_vault_provider: parse_optional_enum(
                "MOA_TOKEN_VAULT_PROVIDER",
                raw.token_vault_provider,
                parse_token_vault_provider,
            )?,
            async_authz_provider: parse_optional_enum(
                "MOA_ASYNC_AUTHZ_PROVIDER",
                raw.async_authz_provider,
                parse_async_authz_provider,
            )?,
            async_authz_default_timeout_secs: parse_optional(
                "MOA_ASYNC_AUTHZ_DEFAULT_TIMEOUT_SECS",
                raw.async_authz_default_timeout_secs,
            )?,
            audit_security_emit_authz_allows: parse_optional(
                "MOA_AUDIT_SECURITY_EMIT_AUTHZ_ALLOWS",
                raw.audit_security_emit_authz_allows,
            )?,
            local_docker_enabled: parse_optional(
                "MOA_LOCAL_DOCKER_ENABLED",
                raw.local_docker_enabled,
            )?,
            local_sandbox_dir: raw.local_sandbox_dir,
            local_memory_dir: raw.local_memory_dir,
            memory_auto_bootstrap: parse_optional(
                "MOA_MEMORY_AUTO_BOOTSTRAP",
                raw.memory_auto_bootstrap,
            )?,
            memory_embedding_provider: raw.memory_embedding_provider,
            memory_embedding_model: raw.memory_embedding_model,
            memory_retrieval_reranker_mode: parse_optional(
                "MOA_MEMORY_RETRIEVAL_RERANKER_MODE",
                raw.memory_retrieval_reranker_mode,
            )?,
            memory_digest_enabled: parse_optional(
                "MOA_MEMORY_DIGEST_ENABLED",
                raw.memory_digest_enabled,
            )?,
            memory_digest_max_tokens: parse_optional(
                "MOA_MEMORY_DIGEST_MAX_TOKENS",
                raw.memory_digest_max_tokens,
            )?,
            memory_digest_rebuild_min_interval_hours: parse_optional(
                "MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS",
                raw.memory_digest_rebuild_min_interval_hours,
            )?,
            memory_extraction_enabled: parse_optional(
                "MOA_MEMORY_EXTRACTION_ENABLED",
                raw.memory_extraction_enabled,
            )?,
            memory_extraction_api_key_env: raw.memory_extraction_api_key_env,
            memory_extraction_model: raw.memory_extraction_model,
            memory_extraction_max_facts_per_chunk: parse_optional(
                "MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK",
                raw.memory_extraction_max_facts_per_chunk,
            )?,
            memory_extraction_timeout_ms: parse_optional(
                "MOA_MEMORY_EXTRACTION_TIMEOUT_MS",
                raw.memory_extraction_timeout_ms,
            )?,
            memory_vector_embedder_name: raw.memory_vector_embedder_name,
            memory_vector_embedder_output_dim: parse_optional(
                "MOA_MEMORY_VECTOR_EMBEDDER_OUTPUT_DIM",
                raw.memory_vector_embedder_output_dim,
            )?,
            memory_vector_embedder_cohere_api_key_env: raw
                .memory_vector_embedder_cohere_api_key_env,
            memory_vector_embedder_gemini_api_key_env: raw
                .memory_vector_embedder_gemini_api_key_env,
            memory_vector_embedder_gemini_default_role: raw
                .memory_vector_embedder_gemini_default_role,
            pii_service_url: parse_optional_url("MOA_PII_SERVICE_URL", raw.pii_service_url)?,
            turbopuffer_api_key_env: raw.turbopuffer_api_key_env,
            turbopuffer_base_url: parse_optional_url(
                "MOA_TURBOPUFFER_BASE_URL",
                raw.turbopuffer_base_url,
            )?,
            turbopuffer_environment: raw.turbopuffer_environment,
            turbopuffer_baa: parse_optional("MOA_TURBOPUFFER_BAA", raw.turbopuffer_baa)?,
            cloud_enabled: parse_optional("MOA_CLOUD_ENABLED", raw.cloud_enabled)?,
            cloud_memory_dir: raw.cloud_memory_dir,
            cloud_flyio_api_token_env: raw.cloud_flyio_api_token_env,
            cloud_flyio_app_name: raw.cloud_flyio_app_name,
            cloud_flyio_region: raw.cloud_flyio_region,
            cloud_flyio_internal_port: parse_optional(
                "MOA_CLOUD_FLYIO_INTERNAL_PORT",
                raw.cloud_flyio_internal_port,
            )?,
            cloud_flyio_health_bind: raw.cloud_flyio_health_bind,
            cloud_flyio_graceful_shutdown_timeout_secs: parse_optional(
                "MOA_CLOUD_FLYIO_GRACEFUL_SHUTDOWN_TIMEOUT_SECS",
                raw.cloud_flyio_graceful_shutdown_timeout_secs,
            )?,
            cloud_hands_default_provider: raw.cloud_hands_default_provider,
            cloud_hands_daytona_api_key_env: raw.cloud_hands_daytona_api_key_env,
            cloud_hands_daytona_api_url: parse_optional_url(
                "MOA_CLOUD_HANDS_DAYTONA_API_URL",
                raw.cloud_hands_daytona_api_url,
            )?,
            cloud_hands_daytona_default_image: raw.cloud_hands_daytona_default_image,
            cloud_hands_e2b_api_key_env: raw.cloud_hands_e2b_api_key_env,
            cloud_hands_e2b_api_url: parse_optional_url(
                "MOA_CLOUD_HANDS_E2B_API_URL",
                raw.cloud_hands_e2b_api_url,
            )?,
            cloud_hands_e2b_domain: raw.cloud_hands_e2b_domain,
            cloud_hands_e2b_template: raw.cloud_hands_e2b_template,
            gateway_telegram_token_env: raw.gateway_telegram_token_env,
            gateway_slack_token_env: raw.gateway_slack_token_env,
            gateway_slack_app_token_env: raw.gateway_slack_app_token_env,
            gateway_discord_token_env: raw.gateway_discord_token_env,
            permissions_default_posture: raw.permissions_default_posture,
            permissions_auto_approve: raw.permissions_auto_approve.map(split_list),
            permissions_always_deny: raw.permissions_always_deny.map(split_list),
            session_blob_threshold_bytes: parse_optional(
                "MOA_SESSION_BLOB_THRESHOLD_BYTES",
                raw.session_blob_threshold_bytes,
            )?,
            session_blob_dir: raw.session_blob_dir,
            compaction_enabled: parse_optional("MOA_COMPACTION_ENABLED", raw.compaction_enabled)?,
            compaction_event_threshold: parse_optional(
                "MOA_COMPACTION_EVENT_THRESHOLD",
                raw.compaction_event_threshold,
            )?,
            compaction_token_ratio_threshold: parse_optional(
                "MOA_COMPACTION_TOKEN_RATIO_THRESHOLD",
                raw.compaction_token_ratio_threshold,
            )?,
            compaction_recent_turns_verbatim: parse_optional(
                "MOA_COMPACTION_RECENT_TURNS_VERBATIM",
                raw.compaction_recent_turns_verbatim,
            )?,
            compaction_preserve_errors: parse_optional(
                "MOA_COMPACTION_PRESERVE_ERRORS",
                raw.compaction_preserve_errors,
            )?,
            compaction_tier2_trigger_blocks_past_bp4: parse_optional(
                "MOA_COMPACTION_TIER2_TRIGGER_BLOCKS_PAST_BP4",
                raw.compaction_tier2_trigger_blocks_past_bp4,
            )?,
            compaction_tier3_trigger_fraction: parse_optional(
                "MOA_COMPACTION_TIER3_TRIGGER_FRACTION",
                raw.compaction_tier3_trigger_fraction,
            )?,
            compaction_max_input_tokens_per_turn: parse_optional(
                "MOA_COMPACTION_MAX_INPUT_TOKENS_PER_TURN",
                raw.compaction_max_input_tokens_per_turn,
            )?,
            restate_ingress_url: parse_optional_url(
                "MOA_RESTATE_INGRESS_URL",
                raw.restate_ingress_url,
            )?,
            restate_admin_url: parse_optional_url("MOA_RESTATE_ADMIN_URL", raw.restate_admin_url)?,
            restate_llm_gateway_url: parse_optional_url(
                "MOA_RESTATE_LLM_GATEWAY_URL",
                raw.restate_llm_gateway_url,
            )?,
            orchestrator_endpoint: parse_optional_url(
                "MOA_ORCHESTRATOR_ENDPOINT",
                raw.orchestrator_endpoint,
            )?,
            orchestrator_health_url: parse_optional_url(
                "MOA_ORCHESTRATOR_HEALTH_URL",
                raw.orchestrator_health_url,
            )?,
            observability_enabled: parse_optional(
                "MOA_OBSERVABILITY_ENABLED",
                raw.observability_enabled,
            )?,
            observability_service_name: raw.observability_service_name,
            observability_otlp_endpoint: parse_optional_url(
                "MOA_OBSERVABILITY_OTLP_ENDPOINT",
                raw.observability_otlp_endpoint,
            )?,
            observability_otlp_protocol: parse_optional_enum(
                "MOA_OBSERVABILITY_OTLP_PROTOCOL",
                raw.observability_otlp_protocol,
                parse_otlp_protocol,
            )?,
            observability_otlp_headers: raw
                .observability_otlp_headers
                .map(|value| parse_headers("MOA_OBSERVABILITY_OTLP_HEADERS", &value))
                .transpose()?,
            observability_environment: raw.observability_environment,
            observability_release: raw.observability_release,
            observability_sample_rate: parse_optional(
                "MOA_OBSERVABILITY_SAMPLE_RATE",
                raw.observability_sample_rate,
            )?,
            observability_lineage_enabled: parse_optional(
                "MOA_OBSERVABILITY_LINEAGE_ENABLED",
                raw.observability_lineage_enabled,
            )?,
            observability_lineage_channel_capacity: parse_optional(
                "MOA_OBSERVABILITY_LINEAGE_CHANNEL_CAPACITY",
                raw.observability_lineage_channel_capacity,
            )?,
            observability_lineage_batch_size: parse_optional(
                "MOA_OBSERVABILITY_LINEAGE_BATCH_SIZE",
                raw.observability_lineage_batch_size,
            )?,
            observability_lineage_batch_max_age_secs: parse_optional(
                "MOA_OBSERVABILITY_LINEAGE_BATCH_MAX_AGE_SECS",
                raw.observability_lineage_batch_max_age_secs,
            )?,
            observability_lineage_journal_path: raw.observability_lineage_journal_path,
            observability_lineage_sample_pgvector_explain: parse_optional(
                "MOA_OBSERVABILITY_LINEAGE_SAMPLE_PGVECTOR_EXPLAIN",
                raw.observability_lineage_sample_pgvector_explain,
            )?,
            metrics_enabled: parse_optional("MOA_METRICS_ENABLED", raw.metrics_enabled)?,
            metrics_listen: raw.metrics_listen,
            budgets_daily_workspace_cents: parse_optional(
                "MOA_BUDGETS_DAILY_WORKSPACE_CENTS",
                raw.budgets_daily_workspace_cents,
            )?,
            session_limits_max_turns: parse_optional(
                "MOA_SESSION_LIMITS_MAX_TURNS",
                raw.session_limits_max_turns,
            )?,
            session_limits_loop_detection_threshold: parse_optional(
                "MOA_SESSION_LIMITS_LOOP_DETECTION_THRESHOLD",
                raw.session_limits_loop_detection_threshold,
            )?,
            tool_output_max_replay_chars: parse_optional(
                "MOA_TOOL_OUTPUT_MAX_REPLAY_CHARS",
                raw.tool_output_max_replay_chars,
            )?,
            tool_output_max_bash_lines: parse_optional(
                "MOA_TOOL_OUTPUT_MAX_BASH_LINES",
                raw.tool_output_max_bash_lines,
            )?,
            tool_output_head_ratio: parse_optional(
                "MOA_TOOL_OUTPUT_HEAD_RATIO",
                raw.tool_output_head_ratio,
            )?,
            tool_budgets_file_read: parse_optional(
                "MOA_TOOL_BUDGETS_FILE_READ",
                raw.tool_budgets_file_read,
            )?,
            tool_budgets_bash_stdout: parse_optional(
                "MOA_TOOL_BUDGETS_BASH_STDOUT",
                raw.tool_budgets_bash_stdout,
            )?,
            tool_budgets_bash_stderr: parse_optional(
                "MOA_TOOL_BUDGETS_BASH_STDERR",
                raw.tool_budgets_bash_stderr,
            )?,
            tool_budgets_grep: parse_optional("MOA_TOOL_BUDGETS_GREP", raw.tool_budgets_grep)?,
            tool_budgets_file_search: parse_optional(
                "MOA_TOOL_BUDGETS_FILE_SEARCH",
                raw.tool_budgets_file_search,
            )?,
            tool_budgets_memory_search: parse_optional(
                "MOA_TOOL_BUDGETS_MEMORY_SEARCH",
                raw.tool_budgets_memory_search,
            )?,
            tool_budgets_file_outline: parse_optional(
                "MOA_TOOL_BUDGETS_FILE_OUTLINE",
                raw.tool_budgets_file_outline,
            )?,
            tool_budgets_default: parse_optional(
                "MOA_TOOL_BUDGETS_DEFAULT",
                raw.tool_budgets_default,
            )?,
            skill_budget_max_manifest_chars: parse_optional(
                "MOA_SKILL_BUDGET_MAX_MANIFEST_CHARS",
                raw.skill_budget_max_manifest_chars,
            )?,
            skill_budget_max_per_skill_chars: parse_optional(
                "MOA_SKILL_BUDGET_MAX_PER_SKILL_CHARS",
                raw.skill_budget_max_per_skill_chars,
            )?,
            skill_budget_show_token_estimates: parse_optional(
                "MOA_SKILL_BUDGET_SHOW_TOKEN_ESTIMATES",
                raw.skill_budget_show_token_estimates,
            )?,
            query_rewrite_enabled: parse_optional(
                "MOA_QUERY_REWRITE_ENABLED",
                raw.query_rewrite_enabled,
            )?,
            query_rewrite_model: raw.query_rewrite_model,
            query_rewrite_timeout_ms: parse_optional(
                "MOA_QUERY_REWRITE_TIMEOUT_MS",
                raw.query_rewrite_timeout_ms,
            )?,
            query_rewrite_min_query_tokens: parse_optional(
                "MOA_QUERY_REWRITE_MIN_QUERY_TOKENS",
                raw.query_rewrite_min_query_tokens,
            )?,
            query_rewrite_skip_single_turn: parse_optional(
                "MOA_QUERY_REWRITE_SKIP_SINGLE_TURN",
                raw.query_rewrite_skip_single_turn,
            )?,
            query_rewrite_circuit_breaker_threshold: parse_optional(
                "MOA_QUERY_REWRITE_CIRCUIT_BREAKER_THRESHOLD",
                raw.query_rewrite_circuit_breaker_threshold,
            )?,
            query_rewrite_circuit_breaker_window_secs: parse_optional(
                "MOA_QUERY_REWRITE_CIRCUIT_BREAKER_WINDOW_SECS",
                raw.query_rewrite_circuit_breaker_window_secs,
            )?,
            query_rewrite_circuit_breaker_cooldown_secs: parse_optional(
                "MOA_QUERY_REWRITE_CIRCUIT_BREAKER_COOLDOWN_SECS",
                raw.query_rewrite_circuit_breaker_cooldown_secs,
            )?,
            resolution_enabled: parse_optional("MOA_RESOLUTION_ENABLED", raw.resolution_enabled)?,
            resolution_weights_tool: parse_optional(
                "MOA_RESOLUTION_WEIGHTS_TOOL",
                raw.resolution_weights_tool,
            )?,
            resolution_weights_verification: parse_optional(
                "MOA_RESOLUTION_WEIGHTS_VERIFICATION",
                raw.resolution_weights_verification,
            )?,
            resolution_weights_continuation: parse_optional(
                "MOA_RESOLUTION_WEIGHTS_CONTINUATION",
                raw.resolution_weights_continuation,
            )?,
            resolution_weights_self_assessment: parse_optional(
                "MOA_RESOLUTION_WEIGHTS_SELF_ASSESSMENT",
                raw.resolution_weights_self_assessment,
            )?,
            resolution_weights_structural: parse_optional(
                "MOA_RESOLUTION_WEIGHTS_STRUCTURAL",
                raw.resolution_weights_structural,
            )?,
            resolution_use_llm_self_assessment: parse_optional(
                "MOA_RESOLUTION_USE_LLM_SELF_ASSESSMENT",
                raw.resolution_use_llm_self_assessment,
            )?,
            resolution_self_assessment_timeout_ms: parse_optional(
                "MOA_RESOLUTION_SELF_ASSESSMENT_TIMEOUT_MS",
                raw.resolution_self_assessment_timeout_ms,
            )?,
            resolution_rephrase_similarity_threshold: parse_optional(
                "MOA_RESOLUTION_REPHRASE_SIMILARITY_THRESHOLD",
                raw.resolution_rephrase_similarity_threshold,
            )?,
            resolution_structural_min_samples: parse_optional(
                "MOA_RESOLUTION_STRUCTURAL_MIN_SAMPLES",
                raw.resolution_structural_min_samples,
            )?,
            resolution_idle_timeout_minutes: parse_optional(
                "MOA_RESOLUTION_IDLE_TIMEOUT_MINUTES",
                raw.resolution_idle_timeout_minutes,
            )?,
            context_snapshot_enabled: parse_optional(
                "MOA_CONTEXT_SNAPSHOT_ENABLED",
                raw.context_snapshot_enabled,
            )?,
            context_snapshot_max_size_bytes: parse_optional(
                "MOA_CONTEXT_SNAPSHOT_MAX_SIZE_BYTES",
                raw.context_snapshot_max_size_bytes,
            )?,
        })
    }
}

fn parse_auth_header_trust(value: &str) -> Option<AuthHeaderTrustKind> {
    match value {
        "strict" => Some(AuthHeaderTrustKind::Strict),
        "lenient" => Some(AuthHeaderTrustKind::Lenient),
        _ => None,
    }
}

fn parse_auth_provider(value: &str) -> Option<AuthProviderKind> {
    match value {
        "local" => Some(AuthProviderKind::Local),
        "disabled" | "none" => Some(AuthProviderKind::Disabled),
        "auth0" => Some(AuthProviderKind::Auth0),
        "oidc" => Some(AuthProviderKind::Oidc),
        _ => None,
    }
}

fn parse_authz_engine(value: &str) -> Option<AuthzEngine> {
    match value {
        "openfga" => Some(AuthzEngine::Openfga),
        "auth0_fga" => Some(AuthzEngine::Auth0Fga),
        _ => None,
    }
}

fn parse_token_vault_provider(value: &str) -> Option<TokenVaultKind> {
    match value {
        "none" => Some(TokenVaultKind::None),
        "auth0" => Some(TokenVaultKind::Auth0),
        _ => None,
    }
}

fn parse_async_authz_provider(value: &str) -> Option<AsyncAuthzKind> {
    match value {
        "builtin" => Some(AsyncAuthzKind::Builtin),
        "auth0" => Some(AsyncAuthzKind::Auth0),
        _ => None,
    }
}

fn parse_otlp_protocol(value: &str) -> Option<OtlpProtocol> {
    match value {
        "grpc" => Some(OtlpProtocol::Grpc),
        "http" => Some(OtlpProtocol::Http),
        _ => None,
    }
}

fn parse_optional<T>(env_name: &'static str, raw: Option<String>) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    raw.map(|value| parse_value(env_name, &value)).transpose()
}

fn parse_value<T>(env_name: &'static str, value: &str) -> Result<T>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse::<T>()
        .map_err(|error| parse_error(env_name, value, error))
}

fn parse_optional_enum<T>(
    env_name: &'static str,
    raw: Option<String>,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>> {
    raw.map(|value| {
        parse(&value).ok_or_else(|| {
            MoaError::ConfigError(format!("{env_name} has unsupported value `{value}`"))
        })
    })
    .transpose()
}

fn parse_optional_url(env_name: &'static str, raw: Option<String>) -> Result<Option<String>> {
    raw.map(|value| {
        reqwest::Url::parse(&value).map_err(|error| parse_error(env_name, &value, error))?;
        Ok(value)
    })
    .transpose()
}

fn parse_headers(env_name: &'static str, value: &str) -> Result<HashMap<String, String>> {
    let mut headers = HashMap::new();
    if value.trim().is_empty() {
        return Ok(headers);
    }
    for entry in value.split(',') {
        let (key, header_value) = entry.split_once('=').ok_or_else(|| {
            MoaError::ConfigError(format!("{env_name} entry `{entry}` must use key=value"))
        })?;
        let key = key.trim();
        if key.is_empty() {
            return Err(MoaError::ConfigError(format!(
                "{env_name} contains an empty header name"
            )));
        }
        headers.insert(key.to_string(), header_value.trim().to_string());
    }
    Ok(headers)
}

fn parse_error(env_name: &'static str, value: &str, error: impl std::fmt::Display) -> MoaError {
    MoaError::ConfigError(format!("{env_name} value `{value}` is invalid: {error}"))
}

fn require_non_empty(env_name: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(MoaError::ConfigError(format!(
            "{env_name} is required when configuring this section"
        )));
    }
    Ok(())
}

fn split_list(value: String) -> Vec<String> {
    value
        .split(',')
        .filter_map(|item| {
            let item = item.trim();
            (!item.is_empty()).then(|| item.to_string())
        })
        .collect()
}

fn set_if_some(target: &mut String, value: &Option<String>) {
    if let Some(value) = value {
        *target = value.clone();
    }
}

fn set_option_if_some(target: &mut Option<String>, value: &Option<String>) {
    if let Some(value) = value {
        *target = Some(value.clone());
    }
}

fn set_vec_if_some(target: &mut Vec<String>, value: &Option<Vec<String>>) {
    if let Some(value) = value {
        *target = value.clone();
    }
}

fn set_copy_if_some<T: Copy>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

fn any_present(values: &[bool]) -> bool {
    values.iter().any(|value| *value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_iter_applies_flat_single_underscore_env() {
        // Pins: flat MOA env names deserialize through envy and update real nested config fields.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_DATABASE_MAX_CONNECTIONS", "42"),
            ("MOA_AUTH_PROVIDER", "oidc"),
            ("MOA_AUTH_HEADER_TRUST", "lenient"),
            ("MOA_AUTHZ_ENGINE", "openfga"),
            ("MOA_AUTHZ_OPENFGA_URL", "http://openfga.example"),
            ("MOA_AUTHZ_OPENFGA_PRESHARED_KEY", "shared-key"),
            ("MOA_AUTHZ_OPENFGA_STORE_ID", "store-1"),
            ("MOA_AUTHZ_OPENFGA_MODEL_ID", "model-1"),
            ("MOA_AUTHZ_OPENFGA_TIMEOUT_MS", "2500"),
            ("MOA_TOKEN_VAULT_PROVIDER", "auth0"),
            ("MOA_ASYNC_AUTHZ_PROVIDER", "auth0"),
            ("MOA_ASYNC_AUTHZ_DEFAULT_TIMEOUT_SECS", "120"),
            ("MOA_AUDIT_SECURITY_EMIT_AUTHZ_ALLOWS", "true"),
            ("MOA_LOCAL_DOCKER_ENABLED", "false"),
            ("MOA_LOCAL_SANDBOX_DIR", "/tmp/moa-sandbox"),
            ("MOA_PII_SERVICE_URL", "http://pii.example:8080"),
            ("MOA_MEMORY_RETRIEVAL_RERANKER_MODE", "eval_only"),
            ("MOA_MEMORY_DIGEST_ENABLED", "true"),
            ("MOA_MEMORY_DIGEST_MAX_TOKENS", "384"),
            ("MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS", "12"),
            ("MOA_MEMORY_VECTOR_EMBEDDER_OUTPUT_DIM", "1536"),
            ("MOA_TURBOPUFFER_API_KEY_ENV", "CUSTOM_TURBOPUFFER_KEY"),
            ("MOA_TURBOPUFFER_BASE_URL", "https://tpuf.example"),
            ("MOA_TURBOPUFFER_ENVIRONMENT", "prod"),
            ("MOA_TURBOPUFFER_BAA", "true"),
            ("MOA_PROVIDERS_OPENAI_API_KEY_ENV", "CUSTOM_OPENAI_KEY"),
            ("MOA_RESTATE_INGRESS_URL", "http://restate.example:8080"),
            ("MOA_RESTATE_ADMIN_URL", "http://restate.example:9070"),
            (
                "MOA_RESTATE_LLM_GATEWAY_URL",
                "http://llm-gateway.example:10020",
            ),
            ("MOA_OBSERVABILITY_ENABLED", "true"),
            (
                "MOA_OBSERVABILITY_OTLP_ENDPOINT",
                "http://otel.example:4317",
            ),
            ("MOA_OBSERVABILITY_OTLP_PROTOCOL", "http"),
            (
                "MOA_OBSERVABILITY_OTLP_HEADERS",
                "tenant=moa,token=redacted",
            ),
            ("MOA_METRICS_ENABLED", "true"),
            ("MOA_METRICS_LISTEN", "127.0.0.1:9091"),
            ("MOA_PERMISSIONS_AUTO_APPROVE", "file_read,grep"),
        ]))
        .expect("overlay should deserialize");

        let mut config = MoaConfig::default();
        overlay.apply_to(&mut config).expect("overlay should apply");

        assert_eq!(config.database.url, "postgres://moa:test@db.example/moa");
        assert_eq!(config.database.max_connections, 42);
        assert_eq!(config.auth.provider, AuthProviderKind::Oidc);
        assert_eq!(config.auth.header_trust, AuthHeaderTrustKind::Lenient);
        assert_eq!(config.authz.engine, AuthzEngine::Openfga);
        let openfga = config.authz.openfga.expect("openfga config");
        assert_eq!(openfga.url, "http://openfga.example");
        assert_eq!(openfga.preshared_key, "shared-key");
        assert_eq!(openfga.store_id, "store-1");
        assert_eq!(openfga.model_id, "model-1");
        assert_eq!(openfga.timeout_ms, 2500);
        assert_eq!(config.token_vault.provider, TokenVaultKind::Auth0);
        assert_eq!(config.async_authz.provider, AsyncAuthzKind::Auth0);
        assert_eq!(config.async_authz.default_timeout_secs, 120);
        assert!(config.audit_security.emit_authz_allows);
        assert!(!config.local.docker_enabled);
        assert_eq!(config.local.sandbox_dir, "/tmp/moa-sandbox");
        assert_eq!(
            config.memory.pii_service_url.as_deref(),
            Some("http://pii.example:8080")
        );
        assert_eq!(
            config.memory.retrieval.reranker_mode,
            MemoryRerankerMode::EvalOnly
        );
        assert!(config.memory.digest.enabled);
        assert_eq!(config.memory.digest.max_tokens, 384);
        assert_eq!(config.memory.digest.rebuild_min_interval_hours, 12);
        assert_eq!(
            config.memory.vector.turbopuffer.api_key_env,
            "CUSTOM_TURBOPUFFER_KEY"
        );
        assert_eq!(config.memory.vector.embedder.output_dim, 1536);
        assert_eq!(
            config.memory.vector.turbopuffer.base_url.as_deref(),
            Some("https://tpuf.example")
        );
        assert_eq!(
            config.memory.vector.turbopuffer.environment.as_deref(),
            Some("prod")
        );
        assert!(config.memory.vector.turbopuffer.baa_enabled);
        assert_eq!(config.providers.openai.api_key_env, "CUSTOM_OPENAI_KEY");
        assert_eq!(
            config.orchestrator.endpoint.as_deref(),
            Some("http://restate.example:8080")
        );
        assert_eq!(
            config.orchestrator.restate_ingress_url.as_deref(),
            Some("http://restate.example:8080")
        );
        assert_eq!(
            config.orchestrator.restate_admin_url.as_deref(),
            Some("http://restate.example:9070")
        );
        assert_eq!(
            config.orchestrator.llm_gateway_url.as_deref(),
            Some("http://llm-gateway.example:10020")
        );
        assert!(config.observability.enabled);
        assert_eq!(
            config.observability.otlp_endpoint.as_deref(),
            Some("http://otel.example:4317")
        );
        assert_eq!(config.observability.otlp_protocol, OtlpProtocol::Http);
        assert_eq!(
            config
                .observability
                .otlp_headers
                .get("tenant")
                .map(String::as_str),
            Some("moa")
        );
        assert_eq!(
            config
                .observability
                .otlp_headers
                .get("token")
                .map(String::as_str),
            Some("redacted")
        );
        assert!(config.metrics.enabled);
        assert_eq!(config.metrics.listen, "127.0.0.1:9091");
        assert_eq!(config.permissions.auto_approve, ["file_read", "grep"]);
    }

    #[test]
    fn invalid_bool_reports_env_name() {
        // Pins: boolean parse failures name the canonical env var.
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([("MOA_LOCAL_DOCKER_ENABLED", "sometimes")])),
            "MOA_LOCAL_DOCKER_ENABLED",
        );
    }

    #[test]
    fn invalid_integer_reports_env_name() {
        // Pins: integer parse failures name the canonical env var.
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([("MOA_DATABASE_MAX_CONNECTIONS", "many")])),
            "MOA_DATABASE_MAX_CONNECTIONS",
        );
    }

    #[test]
    fn invalid_enum_reports_env_name() {
        // Pins: enum parse failures name the canonical env var.
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([("MOA_AUTH_PROVIDER", "saml")])),
            "MOA_AUTH_PROVIDER",
        );
    }

    #[test]
    fn memory_retrieval_reranker_mode_overlay_applies_and_rejects_unknown_values() {
        // Pins: MOA_MEMORY_RETRIEVAL_RERANKER_MODE accepts off/eval_only/on and rejects unsupported modes.
        let overlay =
            MoaEnvOverlay::from_iter(env_pairs([("MOA_MEMORY_RETRIEVAL_RERANKER_MODE", "on")]))
                .expect("reranker mode overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("reranker mode overlay should apply");

        assert_eq!(
            config.memory.retrieval.reranker_mode,
            MemoryRerankerMode::On
        );
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([("MOA_MEMORY_RETRIEVAL_RERANKER_MODE", "auto")])),
            "MOA_MEMORY_RETRIEVAL_RERANKER_MODE",
        );
    }

    #[test]
    fn invalid_url_reports_env_name() {
        // Pins: URL-shaped parse failures name the canonical env var.
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([("MOA_AUTHZ_OPENFGA_URL", "openfga.internal")])),
            "MOA_AUTHZ_OPENFGA_URL",
        );
    }

    #[test]
    fn partial_openfga_overlay_reports_missing_env_name() {
        // Pins: OpenFGA overlay cannot synthesize a partial nested config.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([(
            "MOA_AUTHZ_OPENFGA_URL",
            "http://openfga.example",
        )]))
        .expect("overlay should parse");
        let mut config = MoaConfig::default();

        assert_config_error_contains(
            overlay.apply_to(&mut config),
            "MOA_AUTHZ_OPENFGA_PRESHARED_KEY",
        );
    }

    fn env_pairs<const N: usize>(pairs: [(&str, &str); N]) -> Vec<(String, String)> {
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn assert_config_error_contains<T: std::fmt::Debug>(result: Result<T>, expected: &str) {
        let error = result.expect_err("expected config error");
        match error {
            MoaError::ConfigError(message) => assert!(
                message.contains(expected),
                "expected `{message}` to contain `{expected}`"
            ),
            other => panic!("expected config error, got {other:?}"),
        }
    }
}
