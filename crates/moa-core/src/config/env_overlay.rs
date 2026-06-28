//! Flat single-underscore environment overlay for Kubernetes runtime config.

use std::collections::HashMap;

use serde::Deserialize;

use crate::error::{MoaError, Result};

use super::{
    AsyncAuthzKind, AuthProviderKind, AuthzEngine, MemoryRerankerMode, MoaConfig, OtlpProtocol,
    RuntimeCacheBackend, SessionAttachmentBackend, SessionBlobBackend, TokenVaultKind,
};

/// Optional flat environment overrides for `MoaConfig`.
///
/// envy deserializes `MOA_*` environment variables directly into these typed
/// fields. Only URL validation, header maps, and comma-separated lists need
/// bespoke handling (`validate_urls`, `deserialize_optional_headers`, and
/// `deserialize_optional_list`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
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
    /// `MOA_AUTH_AUTH0_DOMAIN`.
    pub auth_auth0_domain: Option<String>,
    /// `MOA_AUTH_AUTH0_AUDIENCE`.
    pub auth_auth0_audience: Option<String>,
    /// `MOA_AUTH_AUTH0_CLIENT_ID_ENV`.
    pub auth_auth0_client_id_env: Option<String>,
    /// `MOA_AUTH_AUTH0_CLIENT_SECRET_ENV`.
    pub auth_auth0_client_secret_env: Option<String>,
    /// `MOA_AUTH_AUTH0_WEBHOOK_SECRET`.
    pub auth_auth0_webhook_secret: Option<String>,
    /// `MOA_AUTH_OIDC_ISSUER`.
    pub auth_oidc_issuer: Option<String>,
    /// `MOA_AUTH_OIDC_AUDIENCE`.
    pub auth_oidc_audience: Option<String>,
    /// `MOA_AUTH_OIDC_JWKS_URL`.
    pub auth_oidc_jwks_url: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_ISSUER`.
    pub auth_contact_tokens_issuer: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_AUDIENCE`.
    pub auth_contact_tokens_audience: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_KEY_ID`.
    pub auth_contact_tokens_key_id: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM_ENV`.
    pub auth_contact_tokens_private_key_pem_env: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM_ENV`.
    pub auth_contact_tokens_public_key_pem_env: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_CONTACT_POINT_HASH_KEY_ENV`.
    pub auth_contact_tokens_contact_point_hash_key_env: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_UNVERIFIED_TTL_SECONDS`.
    pub auth_contact_tokens_unverified_ttl_seconds: Option<i64>,
    /// `MOA_AUTH_CONTACT_TOKENS_VERIFIED_TTL_SECONDS`.
    pub auth_contact_tokens_verified_ttl_seconds: Option<i64>,
    /// `MOA_AUTH_CONTACT_TOKENS_VERIFICATION_TTL_SECONDS`.
    pub auth_contact_tokens_verification_ttl_seconds: Option<i64>,
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
    /// `MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX`.
    pub privacy_approval_public_key_hex: Option<String>,
    /// `MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX`.
    pub privacy_export_signing_key_hex: Option<String>,
    /// `MOA_PRIVACY_EXPORT_SIGNING_KEY_ID`.
    pub privacy_export_signing_key_id: Option<String>,
    /// `MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX`.
    pub lineage_audit_signing_key_hex: Option<String>,
    /// `MOA_LINEAGE_AUDIT_SIGNING_KEY_ID`.
    pub lineage_audit_signing_key_id: Option<String>,
    /// `MOA_PII_VAULT_SECRET_HEX`.
    pub pii_vault_secret_hex: Option<String>,
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
    /// `MOA_MEMORY_RETRIEVAL_LINEAGE_ENABLED`.
    pub memory_retrieval_lineage_enabled: Option<bool>,
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
    /// `MOA_KNOWLEDGE_PROVIDERS_ENABLED`.
    #[serde(deserialize_with = "deserialize_optional_list")]
    pub knowledge_providers_enabled: Option<Vec<String>>,
    /// `MOA_KNOWLEDGE_PARSERS_ENABLED`.
    #[serde(deserialize_with = "deserialize_optional_list")]
    pub knowledge_parsers_enabled: Option<Vec<String>>,
    /// `MOA_KNOWLEDGE_PARSER_DEFAULT`.
    pub knowledge_parser_default: Option<String>,
    /// `MOA_KNOWLEDGE_EXTERNAL_PARSER_DEFAULT`.
    pub knowledge_external_parser_default: Option<String>,
    /// `MOA_NANGO_API_BASE_URL`.
    pub nango_api_base_url: Option<String>,
    /// `MOA_MERGE_API_BASE_URL`.
    pub merge_api_base_url: Option<String>,
    /// `MOA_LLAMAPARSE_API_URL`.
    pub llamaparse_api_url: Option<String>,
    /// `MOA_LLAMAPARSE_WEBHOOK_HEADER_NAME`.
    pub llamaparse_webhook_header_name: Option<String>,
    /// `MOA_LLAMAPARSE_WEBHOOK_HEADER_VALUE`.
    pub llamaparse_webhook_header_value: Option<String>,
    /// `MOA_LLAMAPARSE_TIER`.
    pub llamaparse_tier: Option<String>,
    /// `MOA_UNSTRUCTURED_API_URL`.
    pub unstructured_api_url: Option<String>,
    /// `MOA_UNSTRUCTURED_STRATEGY`.
    pub unstructured_strategy: Option<String>,
    /// `MOA_UNSTRUCTURED_CHUNKING_STRATEGY`.
    pub unstructured_chunking_strategy: Option<String>,
    /// `MOA_REDUCTO_API_URL`.
    pub reducto_api_url: Option<String>,
    /// `MOA_REDUCTO_WEBHOOK_HEADER_NAME`.
    pub reducto_webhook_header_name: Option<String>,
    /// `MOA_REDUCTO_WEBHOOK_HEADER_VALUE`.
    pub reducto_webhook_header_value: Option<String>,
    /// `MOA_REDUCTO_PARSE_MODE`.
    pub reducto_parse_mode: Option<String>,
    /// `MOA_REDUCTO_ASYNC_ENABLED`.
    pub reducto_async_enabled: Option<bool>,
    /// `MOA_REDUCTO_CHUNK_MODE`.
    pub reducto_chunk_mode: Option<String>,
    /// `MOA_KNOWLEDGE_QUERY_TRACE_ENABLED`.
    pub knowledge_query_trace_enabled: Option<bool>,
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
    /// `MOA_MESSAGING_SLACK_TOKEN_ENV`.
    pub messaging_slack_token_env: Option<String>,
    /// `MOA_MESSAGING_SLACK_APP_TOKEN_ENV`.
    pub messaging_slack_app_token_env: Option<String>,
    /// `MOA_MESSAGING_POSTMARK_BASE_URL`.
    pub messaging_postmark_base_url: Option<String>,
    /// `MOA_MESSAGING_POSTMARK_MESSAGE_STREAM`.
    pub messaging_postmark_message_stream: Option<String>,
    /// `MOA_MESSAGING_EMAIL_FROM`.
    pub messaging_email_from: Option<String>,
    /// `MOA_MESSAGING_EMAIL_REPLY_TO`.
    pub messaging_email_reply_to: Option<String>,
    /// `MOA_MESSAGING_TWILIO_BASE_URL`.
    pub messaging_twilio_base_url: Option<String>,
    /// `MOA_PERMISSIONS_DEFAULT_EFFECT`.
    pub permissions_default_effect: Option<crate::ActionPolicyEffect>,
    /// `MOA_PERMISSIONS_ADMIN_REVIEW`.
    #[serde(deserialize_with = "deserialize_optional_list")]
    pub permissions_admin_review: Option<Vec<String>>,
    /// `MOA_PERMISSIONS_ALWAYS_DENY`.
    #[serde(deserialize_with = "deserialize_optional_list")]
    pub permissions_always_deny: Option<Vec<String>>,
    /// `MOA_SESSION_BLOB_THRESHOLD_BYTES`.
    pub session_blob_threshold_bytes: Option<usize>,
    /// `MOA_SESSION_BLOB_BACKEND`.
    pub session_blob_backend: Option<SessionBlobBackend>,
    /// `MOA_SESSION_BLOB_DIR`.
    pub session_blob_dir: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_BACKEND`.
    pub session_attachment_backend: Option<SessionAttachmentBackend>,
    /// `MOA_SESSION_ATTACHMENT_BUCKET`.
    pub session_attachment_bucket: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_PREFIX`.
    pub session_attachment_prefix: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_REGION`.
    pub session_attachment_region: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_ENDPOINT`.
    pub session_attachment_endpoint: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_ACCESS_KEY_ID`.
    pub session_attachment_access_key_id: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_SECRET_ACCESS_KEY`.
    pub session_attachment_secret_access_key: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_ALLOW_HTTP`.
    pub session_attachment_allow_http: Option<bool>,
    /// `MOA_SESSION_ATTACHMENT_VIRTUAL_HOSTED_STYLE`.
    pub session_attachment_virtual_hosted_style: Option<bool>,
    /// `MOA_SESSION_ATTACHMENT_GCP_SERVICE_ACCOUNT_PATH`.
    pub session_attachment_gcp_service_account_path: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_GCP_SERVICE_ACCOUNT_KEY`.
    pub session_attachment_gcp_service_account_key: Option<String>,
    /// `MOA_SESSION_ATTACHMENT_GCP_APPLICATION_CREDENTIALS_PATH`.
    pub session_attachment_gcp_application_credentials_path: Option<String>,
    /// `MOA_RUNTIME_CACHE_BACKEND`.
    pub runtime_cache_backend: Option<RuntimeCacheBackend>,
    /// `MOA_RUNTIME_CACHE_REDIS_URL`.
    pub runtime_cache_redis_url: Option<String>,
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
    #[serde(deserialize_with = "deserialize_optional_headers")]
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
    /// `MOA_BUDGETS_DAILY_TENANT_CENTS`.
    pub budgets_daily_tenant_cents: Option<u32>,
    /// `MOA_SESSION_LIMITS_MAX_TURNS`.
    pub session_limits_max_turns: Option<u32>,
    /// `MOA_SESSION_LIMITS_SIMPLE_MAX_TURNS`.
    pub session_limits_simple_max_turns: Option<u32>,
    /// `MOA_SESSION_LIMITS_STANDARD_MAX_TURNS`.
    pub session_limits_standard_max_turns: Option<u32>,
    /// `MOA_SESSION_LIMITS_MAX_TOOL_CALLS`.
    pub session_limits_max_tool_calls: Option<u32>,
    /// `MOA_SESSION_LIMITS_LOOP_DETECTION_THRESHOLD`.
    pub session_limits_loop_detection_threshold: Option<u32>,
    /// `MOA_SESSION_LIMITS_PROGRESS_FIRST_DELAY_MS`.
    pub session_limits_progress_first_delay_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_PROGRESS_INTERVAL_MS`.
    pub session_limits_progress_interval_ms: Option<u64>,
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
    /// `MOA_RESOLUTION_REPHRASE_SIMILARITY_THRESHOLD`.
    pub resolution_rephrase_similarity_threshold: Option<f64>,
    /// `MOA_RESOLUTION_STRUCTURAL_MIN_SAMPLES`.
    pub resolution_structural_min_samples: Option<usize>,
    /// `MOA_RESOLUTION_IDLE_TIMEOUT_MINUTES`.
    pub resolution_idle_timeout_minutes: Option<u64>,
    /// `MOA_LEARNING_SKILLS_MIN_TOOL_CALLS`.
    pub learning_skills_min_tool_calls: Option<usize>,
    /// `MOA_CONTEXT_SNAPSHOT_ENABLED`.
    pub context_snapshot_enabled: Option<bool>,
    /// `MOA_CONTEXT_SNAPSHOT_MAX_SIZE_BYTES`.
    pub context_snapshot_max_size_bytes: Option<usize>,
}

impl MoaEnvOverlay {
    /// Loads a flat `MOA_` environment overlay from process environment variables.
    pub fn from_env() -> Result<Self> {
        let overlay: Self = envy::prefixed("MOA_").from_env().map_err(map_env_error)?;
        overlay.validate_urls()?;
        Ok(overlay)
    }

    /// Loads a flat `MOA_` environment overlay from deterministic key-value pairs.
    #[allow(clippy::should_implement_trait)]
    pub fn from_iter(iter: impl IntoIterator<Item = (String, String)>) -> Result<Self> {
        let overlay: Self = envy::prefixed("MOA_")
            .from_iter(iter)
            .map_err(map_env_error)?;
        overlay.validate_urls()?;
        Ok(overlay)
    }

    /// Validates that each URL-shaped overlay value parses as a URL.
    ///
    /// envy/serde populate the string fields without validation, so this mirrors
    /// the previous per-field checks while keeping the canonical `MOA_*` env-var
    /// name in any error message.
    fn validate_urls(&self) -> Result<()> {
        for (env_name, value) in [
            ("MOA_DATABASE_URL", &self.database_url),
            ("MOA_DATABASE_ADMIN_URL", &self.database_admin_url),
            ("MOA_AUTH_OIDC_ISSUER", &self.auth_oidc_issuer),
            ("MOA_AUTH_OIDC_JWKS_URL", &self.auth_oidc_jwks_url),
            ("MOA_AUTHZ_OPENFGA_URL", &self.authz_openfga_url),
            ("MOA_PII_SERVICE_URL", &self.pii_service_url),
            ("MOA_TURBOPUFFER_BASE_URL", &self.turbopuffer_base_url),
            ("MOA_NANGO_API_BASE_URL", &self.nango_api_base_url),
            ("MOA_MERGE_API_BASE_URL", &self.merge_api_base_url),
            ("MOA_LLAMAPARSE_API_URL", &self.llamaparse_api_url),
            ("MOA_UNSTRUCTURED_API_URL", &self.unstructured_api_url),
            ("MOA_REDUCTO_API_URL", &self.reducto_api_url),
            (
                "MOA_CLOUD_HANDS_DAYTONA_API_URL",
                &self.cloud_hands_daytona_api_url,
            ),
            ("MOA_CLOUD_HANDS_E2B_API_URL", &self.cloud_hands_e2b_api_url),
            ("MOA_RESTATE_INGRESS_URL", &self.restate_ingress_url),
            ("MOA_RESTATE_ADMIN_URL", &self.restate_admin_url),
            ("MOA_RESTATE_LLM_GATEWAY_URL", &self.restate_llm_gateway_url),
            ("MOA_ORCHESTRATOR_ENDPOINT", &self.orchestrator_endpoint),
            ("MOA_ORCHESTRATOR_HEALTH_URL", &self.orchestrator_health_url),
            ("MOA_RUNTIME_CACHE_REDIS_URL", &self.runtime_cache_redis_url),
            (
                "MOA_SESSION_ATTACHMENT_ENDPOINT",
                &self.session_attachment_endpoint,
            ),
            (
                "MOA_OBSERVABILITY_OTLP_ENDPOINT",
                &self.observability_otlp_endpoint,
            ),
        ] {
            if let Some(value) = value {
                url::Url::parse(value).map_err(|error| parse_error(env_name, value, error))?;
            }
        }
        Ok(())
    }

    /// Applies this overlay to a typed MOA config.
    pub fn apply_to(&self, config: &mut MoaConfig) -> Result<()> {
        self.apply_provider_overlay(config);
        self.apply_database_overlay(config);
        self.apply_auth_overlay(config)?;
        self.apply_authz_overlay(config)?;
        self.apply_token_vault_overlay(config);
        self.apply_async_authz_overlay(config);
        self.apply_audit_security_overlay(config);
        self.apply_compliance_overlay(config);
        self.apply_local_overlay(config);
        self.apply_memory_overlay(config);
        self.apply_knowledge_overlay(config);
        self.apply_cloud_overlay(config);
        self.apply_messaging_overlay(config);
        self.apply_permissions_overlay(config);
        self.apply_session_overlay(config);
        self.apply_runtime_cache_overlay(config);
        self.apply_compaction_overlay(config);
        self.apply_orchestrator_overlay(config);
        self.apply_observability_overlay(config);
        self.apply_metrics_overlay(config);
        self.apply_context_overlay(config);
        self.apply_learning_overlay(config);
        config.validate()
    }
}

/// Wraps an `envy` deserialization error as a `MoaError`, restoring the `MOA_`
/// prefix that `envy::prefixed` strips from the variable name in value-parse
/// failures so the message still names the canonical env var.
fn map_env_error(error: envy::Error) -> MoaError {
    MoaError::ConfigError(format!(
        "MOA env overlay: {}",
        restore_env_prefix(&error.to_string())
    ))
}

/// Re-attaches the `MOA_` prefix to the variable name in an envy parse-error
/// message.
///
/// envy reports value-parse failures as `... provided by <KEY>` with the prefix
/// already stripped; enum/variant errors carry no key and pass through unchanged.
fn restore_env_prefix(message: &str) -> String {
    match message.rsplit_once("provided by ") {
        Some((head, key)) => format!("{head}provided by MOA_{key}"),
        None => message.to_string(),
    }
}

/// Deserializes a comma-separated env value into a trimmed, non-empty list.
fn deserialize_optional_list<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    Ok(Some(split_list(raw)))
}

/// Deserializes a comma-separated `key=value` env value into a header map.
fn deserialize_optional_headers<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<HashMap<String, String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(deserializer)?;
    parse_headers(&raw)
        .map(Some)
        .map_err(serde::de::Error::custom)
}

/// Parses a comma-separated `key=value` list into a header map.
fn parse_headers(value: &str) -> std::result::Result<HashMap<String, String>, String> {
    let mut headers = HashMap::new();
    if value.trim().is_empty() {
        return Ok(headers);
    }
    for entry in value.split(',') {
        let (key, header_value) = entry
            .split_once('=')
            .ok_or_else(|| format!("header entry `{entry}` must use key=value"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err("header entry contains an empty header name".to_string());
        }
        headers.insert(key.to_string(), header_value.trim().to_string());
    }
    Ok(headers)
}

fn parse_error(env_name: &'static str, value: &str, error: impl std::fmt::Display) -> MoaError {
    MoaError::ConfigError(format!("{env_name} value `{value}` is invalid: {error}"))
}

/// Requires that a partially configured nested section has a non-empty field.
pub(in crate::config) fn require_non_empty(env_name: &'static str, value: &str) -> Result<()> {
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

/// Replaces a string config value when its env overlay value is present.
pub(in crate::config) fn set_if_some(target: &mut String, value: &Option<String>) {
    if let Some(value) = value {
        *target = value.clone();
    }
}

/// Replaces an optional string config value when its env overlay value is present.
pub(in crate::config) fn set_option_if_some(target: &mut Option<String>, value: &Option<String>) {
    if let Some(value) = value {
        *target = Some(value.clone());
    }
}

/// Replaces a vector config value when its env overlay value is present.
pub(in crate::config) fn set_vec_if_some(target: &mut Vec<String>, value: &Option<Vec<String>>) {
    if let Some(value) = value {
        *target = value.clone();
    }
}

/// Replaces a copyable config value when its env overlay value is present.
pub(in crate::config) fn set_copy_if_some<T: Copy>(target: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *target = value;
    }
}

/// Returns whether any field in one nested overlay section was set.
pub(in crate::config) fn any_present(values: &[bool]) -> bool {
    values.iter().any(|value| *value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_iter_applies_flat_single_underscore_env() {
        // Pins: flat MOA env names deserialize through envy and update real nested config fields.
        let approval_key_hex = "01".repeat(32);
        let export_key_hex = "02".repeat(32);
        let lineage_key_hex = "03".repeat(32);
        let pii_vault_secret_hex = "04".repeat(32);
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_DATABASE_URL", "postgres://moa:test@db.example/moa"),
            ("MOA_DATABASE_MAX_CONNECTIONS", "42"),
            ("MOA_AUTH_PROVIDER", "oidc"),
            ("MOA_AUTHZ_ENGINE", "openfga"),
            ("MOA_AUTHZ_OPENFGA_URL", "http://openfga.example"),
            ("MOA_AUTHZ_OPENFGA_PRESHARED_KEY", "shared-key"),
            ("MOA_AUTHZ_OPENFGA_STORE_ID", "store-1"),
            ("MOA_AUTHZ_OPENFGA_MODEL_ID", "model-1"),
            ("MOA_AUTHZ_OPENFGA_TIMEOUT_MS", "2500"),
            ("MOA_AUTH_AUTH0_WEBHOOK_SECRET", "webhook-secret"),
            ("MOA_TOKEN_VAULT_PROVIDER", "auth0"),
            ("MOA_ASYNC_AUTHZ_PROVIDER", "auth0"),
            ("MOA_ASYNC_AUTHZ_DEFAULT_TIMEOUT_SECS", "120"),
            ("MOA_AUDIT_SECURITY_EMIT_AUTHZ_ALLOWS", "true"),
            (
                "MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX",
                approval_key_hex.as_str(),
            ),
            (
                "MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX",
                export_key_hex.as_str(),
            ),
            ("MOA_PRIVACY_EXPORT_SIGNING_KEY_ID", "privacy-key-v2"),
            (
                "MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX",
                lineage_key_hex.as_str(),
            ),
            ("MOA_LINEAGE_AUDIT_SIGNING_KEY_ID", "lineage-key-v2"),
            ("MOA_PII_VAULT_SECRET_HEX", pii_vault_secret_hex.as_str()),
            ("MOA_LOCAL_DOCKER_ENABLED", "false"),
            ("MOA_LOCAL_SANDBOX_DIR", "/tmp/moa-sandbox"),
            ("MOA_PII_SERVICE_URL", "http://pii.example:8080"),
            ("MOA_MEMORY_RETRIEVAL_RERANKER_MODE", "eval_only"),
            ("MOA_MEMORY_RETRIEVAL_LINEAGE_ENABLED", "true"),
            ("MOA_MEMORY_DIGEST_ENABLED", "true"),
            ("MOA_MEMORY_DIGEST_MAX_TOKENS", "384"),
            ("MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS", "12"),
            ("MOA_MEMORY_VECTOR_EMBEDDER_OUTPUT_DIM", "1536"),
            ("MOA_TURBOPUFFER_API_KEY_ENV", "CUSTOM_TURBOPUFFER_KEY"),
            ("MOA_TURBOPUFFER_BASE_URL", "https://tpuf.example"),
            ("MOA_TURBOPUFFER_ENVIRONMENT", "prod"),
            ("MOA_TURBOPUFFER_BAA", "true"),
            ("MOA_MESSAGING_SLACK_TOKEN_ENV", "CUSTOM_SLACK_BOT_TOKEN"),
            (
                "MOA_MESSAGING_SLACK_APP_TOKEN_ENV",
                "CUSTOM_SLACK_APP_TOKEN",
            ),
            (
                "MOA_MESSAGING_POSTMARK_BASE_URL",
                "https://postmark.example",
            ),
            ("MOA_MESSAGING_POSTMARK_MESSAGE_STREAM", "alerts"),
            ("MOA_MESSAGING_EMAIL_FROM", "MOA <moa@example.com>"),
            ("MOA_MESSAGING_EMAIL_REPLY_TO", "support@example.com"),
            ("MOA_MESSAGING_TWILIO_BASE_URL", "https://twilio.example"),
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
            ("MOA_PERMISSIONS_ADMIN_REVIEW", "bash,file_write"),
            ("MOA_PERMISSIONS_DEFAULT_EFFECT", "admin_review"),
        ]))
        .expect("overlay should deserialize");

        let mut config = MoaConfig::default();
        overlay.apply_to(&mut config).expect("overlay should apply");

        assert_eq!(config.database.url, "postgres://moa:test@db.example/moa");
        assert_eq!(config.database.max_connections, 42);
        assert_eq!(config.auth.provider, AuthProviderKind::Oidc);
        assert_eq!(
            config.auth.auth0_webhook_secret.as_deref(),
            Some("webhook-secret")
        );
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
        assert!(config.memory.retrieval.lineage_enabled);
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
        assert_eq!(config.messaging.slack_token_env, "CUSTOM_SLACK_BOT_TOKEN");
        assert_eq!(
            config.messaging.slack_app_token_env,
            "CUSTOM_SLACK_APP_TOKEN"
        );
        assert_eq!(
            config.messaging.postmark_base_url,
            "https://postmark.example"
        );
        assert_eq!(config.messaging.postmark_message_stream, "alerts");
        assert_eq!(config.messaging.email_from, "MOA <moa@example.com>");
        assert_eq!(
            config.messaging.email_reply_to.as_deref(),
            Some("support@example.com")
        );
        assert_eq!(config.messaging.twilio_base_url, "https://twilio.example");
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
        assert_eq!(config.permissions.admin_review, ["bash", "file_write"]);
        assert_eq!(
            config.permissions.default_effect,
            crate::ActionPolicyEffect::AdminReview
        );
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
    fn invalid_enum_reports_offending_value() {
        // Pins: unsupported enum values are rejected. envy/serde deserialize
        // enums directly, so the message names the rejected variant rather than
        // the `MOA_` env var (see `restore_env_prefix`).
        assert_config_error_contains(
            MoaEnvOverlay::from_iter(env_pairs([("MOA_AUTH_PROVIDER", "saml")])),
            "saml",
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
            "auto",
        );
    }

    #[test]
    fn knowledge_overlay_applies_flat_provider_and_parser_settings() {
        // Pins: tenant knowledge overlay updates non-secret runtime config without adding secret indirection knobs.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_KNOWLEDGE_PROVIDERS_ENABLED", "nango"),
            ("MOA_KNOWLEDGE_PARSERS_ENABLED", "native,llamaparse"),
            ("MOA_KNOWLEDGE_PARSER_DEFAULT", "native"),
            ("MOA_KNOWLEDGE_EXTERNAL_PARSER_DEFAULT", "llamaparse"),
            ("MOA_NANGO_API_BASE_URL", "https://nango.example"),
            ("MOA_MERGE_API_BASE_URL", "https://merge.example"),
            ("MOA_LLAMAPARSE_API_URL", "https://llamaparse.example"),
            ("MOA_LLAMAPARSE_WEBHOOK_HEADER_NAME", "x-llama-secret"),
            ("MOA_LLAMAPARSE_WEBHOOK_HEADER_VALUE", "llama-header-secret"),
            ("MOA_LLAMAPARSE_TIER", "agentic"),
            ("MOA_UNSTRUCTURED_API_URL", "https://unstructured.example"),
            ("MOA_UNSTRUCTURED_STRATEGY", "fast"),
            ("MOA_UNSTRUCTURED_CHUNKING_STRATEGY", "basic"),
            ("MOA_REDUCTO_API_URL", "https://reducto.example"),
            ("MOA_REDUCTO_WEBHOOK_HEADER_NAME", "x-reducto-secret"),
            ("MOA_REDUCTO_WEBHOOK_HEADER_VALUE", "reducto-header-secret"),
            ("MOA_REDUCTO_PARSE_MODE", "ocr"),
            ("MOA_REDUCTO_ASYNC_ENABLED", "false"),
            ("MOA_REDUCTO_CHUNK_MODE", "page"),
            ("MOA_KNOWLEDGE_QUERY_TRACE_ENABLED", "true"),
        ]))
        .expect("knowledge overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("knowledge overlay should apply");

        assert_eq!(config.knowledge.providers.enabled, ["nango"]);
        assert_eq!(config.knowledge.parsers.enabled, ["native", "llamaparse"]);
        assert_eq!(config.knowledge.parser.default, "native");
        assert_eq!(config.knowledge.parser.external_default, "llamaparse");
        assert_eq!(config.knowledge.nango.api_base_url, "https://nango.example");
        assert_eq!(config.knowledge.nango.api_key_env, "NANGO_API_KEY");
        assert_eq!(
            config.knowledge.nango.webhook_signing_key_env,
            "NANGO_WEBHOOK_SIGNING_KEY"
        );
        assert_eq!(config.knowledge.merge.api_base_url, "https://merge.example");
        assert_eq!(config.knowledge.merge.api_key_env, "MERGE_API_KEY");
        assert_eq!(
            config.knowledge.merge.webhook_signature_key_env,
            "MERGE_WEBHOOK_SIGNATURE_KEY"
        );
        assert_eq!(
            config.knowledge.llamaparse.api_base_url,
            "https://llamaparse.example"
        );
        assert_eq!(
            config.knowledge.llamaparse.api_key_env,
            "LLAMAPARSE_API_KEY"
        );
        assert_eq!(
            config.knowledge.llamaparse.webhook_signing_key_env,
            "LLAMAPARSE_WEBHOOK_SIGNING_KEY"
        );
        assert_eq!(
            config.knowledge.llamaparse.webhook_header_name.as_deref(),
            Some("x-llama-secret")
        );
        assert_eq!(
            config.knowledge.llamaparse.webhook_header_value.as_deref(),
            Some("llama-header-secret")
        );
        assert_eq!(config.knowledge.llamaparse.tier, "agentic");
        assert_eq!(
            config.knowledge.unstructured.api_base_url,
            "https://unstructured.example"
        );
        assert_eq!(
            config.knowledge.unstructured.api_key_env,
            "UNSTRUCTURED_API_KEY"
        );
        assert_eq!(config.knowledge.unstructured.strategy, "fast");
        assert_eq!(config.knowledge.unstructured.chunking_strategy, "basic");
        assert_eq!(
            config.knowledge.reducto.api_base_url,
            "https://reducto.example"
        );
        assert_eq!(config.knowledge.reducto.api_key_env, "REDUCTO_API_KEY");
        assert_eq!(
            config.knowledge.reducto.webhook_signing_key_env,
            "REDUCTO_WEBHOOK_SIGNING_KEY"
        );
        assert_eq!(
            config.knowledge.reducto.webhook_header_name.as_deref(),
            Some("x-reducto-secret")
        );
        assert_eq!(
            config.knowledge.reducto.webhook_header_value.as_deref(),
            Some("reducto-header-secret")
        );
        assert_eq!(config.knowledge.reducto.parse_mode, "ocr");
        assert!(!config.knowledge.reducto.async_enabled);
        assert_eq!(config.knowledge.reducto.chunk_mode, "page");
        assert!(config.knowledge.observability.query_trace_enabled);
    }

    #[test]
    fn runtime_cache_overlay_applies_backend_and_redis_url() {
        // Pins: runtime cache selection uses flat MOA env names through envy.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_RUNTIME_CACHE_BACKEND", "redis"),
            (
                "MOA_RUNTIME_CACHE_REDIS_URL",
                "redis://cache.example:6379/0",
            ),
        ]))
        .expect("runtime cache overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("runtime cache overlay should apply");

        assert_eq!(config.runtime_cache.backend, RuntimeCacheBackend::Redis);
        assert_eq!(
            config.runtime_cache.redis_url.as_deref(),
            Some("redis://cache.example:6379/0")
        );
    }

    #[test]
    fn session_blob_overlay_applies_backend_and_local_path() {
        // Pins: claim-check blob storage is selected through explicit flat MOA env names.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_SESSION_BLOB_BACKEND", "local"),
            ("MOA_SESSION_BLOB_DIR", "/var/lib/moa/blobs"),
        ]))
        .expect("session blob overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("session blob overlay should apply");

        assert_eq!(config.session.blob_backend, SessionBlobBackend::Local);
        assert_eq!(
            config.session.blob_dir.as_deref(),
            Some("/var/lib/moa/blobs")
        );
    }

    #[test]
    fn session_attachment_overlay_applies_object_store_settings() {
        // Pins: session upload bytes use explicit object storage config rather than Postgres bytes.
        let overlay = MoaEnvOverlay::from_iter(env_pairs([
            ("MOA_SESSION_ATTACHMENT_BACKEND", "gcs"),
            ("MOA_SESSION_ATTACHMENT_BUCKET", "moa-prod-attachments"),
            ("MOA_SESSION_ATTACHMENT_PREFIX", "prod/session-attachments"),
            (
                "MOA_SESSION_ATTACHMENT_GCP_APPLICATION_CREDENTIALS_PATH",
                "/var/run/secrets/gcp/application-default.json",
            ),
            ("MOA_SESSION_ATTACHMENT_ALLOW_HTTP", "false"),
        ]))
        .expect("session attachment overlay should parse");
        let mut config = MoaConfig::default();

        overlay
            .apply_to(&mut config)
            .expect("session attachment overlay should apply");

        assert_eq!(
            config.session.attachments.backend,
            SessionAttachmentBackend::Gcs
        );
        assert_eq!(config.session.attachments.bucket, "moa-prod-attachments");
        assert_eq!(
            config.session.attachments.prefix,
            "prod/session-attachments"
        );
        assert_eq!(
            config
                .session
                .attachments
                .gcp_application_credentials_path
                .as_deref(),
            Some("/var/run/secrets/gcp/application-default.json")
        );
        assert!(!config.session.attachments.allow_http);
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
