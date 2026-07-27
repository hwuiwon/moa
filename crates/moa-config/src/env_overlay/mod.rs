//! Flat single-underscore environment overlay for Kubernetes runtime config.

mod database;
mod messaging;
mod observability;
mod providers;
mod security;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use moa_core::error::{MoaError, Result};
use moa_core::types::provider::ProviderId;

use observability::{deserialize_optional_headers, deserialize_optional_nonempty};
use providers::{deserialize_optional_list, deserialize_optional_provider_ids};

use super::{
    AsyncAuthzKind, AuthProviderKind, AuthzEngine, KmsProviderKind, McpServerConfig, MoaConfig,
    OAuthClientConfig, OAuthRefreshConfig, OtlpProtocol, RuntimeCacheBackend, SecurityProfile,
    SessionAttachmentBackend, SessionBlobBackend, TokenVaultKind,
};

/// Optional flat environment overrides for `MoaConfig`.
///
/// envy deserializes `MOA_*` environment variables directly into these typed
/// fields. Only URL validation, header maps, comma-separated lists, and the MCP
/// server JSON array need bespoke handling.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct EnvOverlay {
    /// `MOA_SECURITY_PROFILE`.
    pub security_profile: Option<SecurityProfile>,
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
    /// `MOA_MCP_SERVERS_JSON`.
    #[serde(deserialize_with = "deserialize_optional_mcp_servers")]
    pub mcp_servers_json: Option<Vec<McpServerConfig>>,
    /// `MOA_LLM_DLP_TOKENIZE_ENABLED`.
    pub llm_dlp_tokenize_enabled: Option<bool>,
    /// `MOA_MODELS_MAIN`.
    pub models_main: Option<String>,
    /// `MOA_MODELS_AUXILIARY`.
    pub models_auxiliary: Option<String>,
    /// `MOA_MODELS_FALLBACK_MODELS`.
    #[serde(deserialize_with = "deserialize_optional_list")]
    pub models_fallback_models: Option<Vec<String>>,
    /// `MOA_ANTHROPIC_API_KEY`.
    pub anthropic_api_key: Option<String>,
    /// `MOA_OPENAI_API_KEY`.
    pub openai_api_key: Option<String>,
    /// `MOA_GOOGLE_API_KEY`.
    pub google_api_key: Option<String>,
    /// `MOA_COHERE_API_KEY`.
    pub cohere_api_key: Option<String>,
    /// `MOA_ZEROENTROPY_API_KEY`.
    pub zeroentropy_api_key: Option<String>,
    /// `MOA_ANTHROPIC_MAX_REQUESTS_PER_MIN`.
    pub anthropic_max_requests_per_min: Option<u32>,
    /// `MOA_ANTHROPIC_MAX_INPUTS_PER_MIN`.
    pub anthropic_max_inputs_per_min: Option<u32>,
    /// `MOA_ANTHROPIC_MAX_CONCURRENT_REQUESTS`.
    pub anthropic_max_concurrent_requests: Option<u32>,
    /// `MOA_OPENAI_MAX_REQUESTS_PER_MIN`.
    pub openai_max_requests_per_min: Option<u32>,
    /// `MOA_OPENAI_MAX_INPUTS_PER_MIN`.
    pub openai_max_inputs_per_min: Option<u32>,
    /// `MOA_OPENAI_MAX_CONCURRENT_REQUESTS`.
    pub openai_max_concurrent_requests: Option<u32>,
    /// `MOA_GOOGLE_MAX_REQUESTS_PER_MIN`.
    pub google_max_requests_per_min: Option<u32>,
    /// `MOA_GOOGLE_MAX_INPUTS_PER_MIN`.
    pub google_max_inputs_per_min: Option<u32>,
    /// `MOA_GOOGLE_MAX_CONCURRENT_REQUESTS`.
    pub google_max_concurrent_requests: Option<u32>,
    /// `MOA_COHERE_MAX_REQUESTS_PER_MIN`.
    pub cohere_max_requests_per_min: Option<u32>,
    /// `MOA_COHERE_MAX_INPUTS_PER_MIN`.
    pub cohere_max_inputs_per_min: Option<u32>,
    /// `MOA_COHERE_MAX_CONCURRENT_REQUESTS`.
    pub cohere_max_concurrent_requests: Option<u32>,
    /// `MOA_ZEROENTROPY_MAX_REQUESTS_PER_MIN`.
    pub zeroentropy_max_requests_per_min: Option<u32>,
    /// `MOA_ZEROENTROPY_MAX_INPUTS_PER_MIN`.
    pub zeroentropy_max_inputs_per_min: Option<u32>,
    /// `MOA_ZEROENTROPY_MAX_CONCURRENT_REQUESTS`.
    pub zeroentropy_max_concurrent_requests: Option<u32>,
    /// `MOA_ANTHROPIC_CAPABILITIES_ZERO_RETENTION`.
    pub anthropic_capabilities_zero_retention: Option<bool>,
    /// `MOA_ANTHROPIC_CAPABILITIES_PRIVATE_DEPLOYMENT`.
    pub anthropic_capabilities_private_deployment: Option<bool>,
    /// `MOA_ANTHROPIC_CAPABILITIES_DATA_RESIDENCY`.
    pub anthropic_capabilities_data_residency: Option<String>,
    /// `MOA_OPENAI_CAPABILITIES_ZERO_RETENTION`.
    pub openai_capabilities_zero_retention: Option<bool>,
    /// `MOA_OPENAI_CAPABILITIES_PRIVATE_DEPLOYMENT`.
    pub openai_capabilities_private_deployment: Option<bool>,
    /// `MOA_OPENAI_CAPABILITIES_DATA_RESIDENCY`.
    pub openai_capabilities_data_residency: Option<String>,
    /// `MOA_GOOGLE_CAPABILITIES_ZERO_RETENTION`.
    pub google_capabilities_zero_retention: Option<bool>,
    /// `MOA_GOOGLE_CAPABILITIES_PRIVATE_DEPLOYMENT`.
    pub google_capabilities_private_deployment: Option<bool>,
    /// `MOA_GOOGLE_CAPABILITIES_DATA_RESIDENCY`.
    pub google_capabilities_data_residency: Option<String>,
    /// `MOA_PROVIDERS_ROUTING_POLICY_REQUIRE_ZERO_RETENTION`.
    pub providers_routing_policy_require_zero_retention: Option<bool>,
    /// `MOA_PROVIDERS_ROUTING_POLICY_REQUIRE_PRIVATE_DEPLOYMENT`.
    pub providers_routing_policy_require_private_deployment: Option<bool>,
    /// `MOA_PROVIDERS_ROUTING_POLICY_ALLOWED_PROVIDERS`.
    #[serde(deserialize_with = "deserialize_optional_provider_ids")]
    pub providers_routing_policy_allowed_providers: Option<Vec<ProviderId>>,
    /// `MOA_PROVIDERS_ROUTING_POLICY_DENIED_PROVIDERS`.
    #[serde(deserialize_with = "deserialize_optional_provider_ids")]
    pub providers_routing_policy_denied_providers: Option<Vec<ProviderId>>,
    /// `MOA_PROVIDERS_ROUTING_POLICY_REQUIRED_RESIDENCY`.
    pub providers_routing_policy_required_residency: Option<String>,
    /// `MOA_PROVIDERS_CONCURRENCY_SCOPE` (`local` | `global`).
    pub providers_concurrency_scope: Option<String>,
    /// `MOA_PROVIDERS_CONCURRENCY_DEFAULT_MAX_IN_FLIGHT`.
    pub providers_concurrency_default_max_in_flight: Option<u32>,
    /// `MOA_PROVIDERS_CONCURRENCY_BLOCK_THRESHOLD_MS`.
    pub providers_concurrency_block_threshold_ms: Option<u64>,
    /// `MOA_PROVIDERS_CONCURRENCY_LEASE_TTL_MS`.
    pub providers_concurrency_lease_ttl_ms: Option<u64>,
    /// Provider first-byte stream timeout in milliseconds.
    pub providers_stream_timeouts_first_byte_ms: Option<u64>,
    /// Provider stream idle timeout in milliseconds.
    pub providers_stream_timeouts_idle_ms: Option<u64>,
    /// Provider total stream timeout in milliseconds.
    pub providers_stream_timeouts_total_ms: Option<u64>,
    /// `MOA_DATABASE_URL`.
    pub database_url: Option<String>,
    /// `MOA_DATABASE_ADMIN_URL`.
    pub database_admin_url: Option<String>,
    /// `MOA_DATABASE_SCHEMA`.
    pub database_schema: Option<String>,
    /// `MOA_DATABASE_MAX_CONNECTIONS`.
    pub database_max_connections: Option<u32>,
    /// `MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS`.
    pub database_background_max_connections: Option<u32>,
    /// `MOA_DATABASE_CONNECT_TIMEOUT_SECONDS`.
    pub database_connect_timeout_seconds: Option<u64>,
    /// `MOA_DATABASE_NEON_ENABLED`.
    pub database_neon_enabled: Option<bool>,
    /// `MOA_DATABASE_NEON_API_KEY`.
    pub database_neon_api_key: Option<String>,
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
    /// `MOA_AUTH_AUTH0_CLIENT_ID`.
    pub auth_auth0_client_id: Option<String>,
    /// `MOA_AUTH_AUTH0_CLIENT_SECRET`.
    pub auth_auth0_client_secret: Option<String>,
    /// `MOA_AUTH_AUTH0_WEBHOOK_SECRET`.
    pub auth_auth0_webhook_secret: Option<String>,
    /// `MOA_AUTH_OIDC_ISSUER`.
    pub auth_oidc_issuer: Option<String>,
    /// `MOA_AUTH_OIDC_AUDIENCE`.
    pub auth_oidc_audience: Option<String>,
    /// `MOA_AUTH_OIDC_JWKS_URL`.
    pub auth_oidc_jwks_url: Option<String>,
    /// `MOA_AUTH_OAUTH_ISSUER`.
    pub auth_oauth_issuer: Option<String>,
    /// `MOA_AUTH_OAUTH_RESOURCE`.
    pub auth_oauth_resource: Option<String>,
    /// `MOA_AUTH_OAUTH_AUTHORIZATION_REQUEST_TTL_SECONDS`.
    pub auth_oauth_authorization_request_ttl_seconds: Option<i64>,
    /// `MOA_AUTH_OAUTH_AUTHORIZATION_CODE_TTL_SECONDS`.
    pub auth_oauth_authorization_code_ttl_seconds: Option<i64>,
    /// `MOA_AUTH_OAUTH_ACCESS_TOKEN_TTL_SECONDS`.
    pub auth_oauth_access_token_ttl_seconds: Option<i64>,
    /// `MOA_AUTH_OAUTH_REFRESH_TOKEN_TTL_SECONDS`.
    pub auth_oauth_refresh_token_ttl_seconds: Option<i64>,
    /// `MOA_AUTH_OAUTH_CLIENTS_JSON`.
    #[serde(deserialize_with = "deserialize_optional_oauth_clients")]
    pub auth_oauth_clients_json: Option<Vec<OAuthClientConfig>>,
    /// `MOA_AUTH_CONTACT_TOKENS_ISSUER`.
    pub auth_contact_tokens_issuer: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_AUDIENCE`.
    pub auth_contact_tokens_audience: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_KEY_ID`.
    pub auth_contact_tokens_key_id: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM`.
    pub auth_contact_tokens_private_key_pem: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM`.
    pub auth_contact_tokens_public_key_pem: Option<String>,
    /// `MOA_AUTH_CONTACT_TOKENS_CONTACT_POINT_HASH_KEY_HEX`.
    pub auth_contact_tokens_contact_point_hash_key_hex: Option<String>,
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
    /// `MOA_TOKEN_VAULT_REFRESH_JSON`.
    #[serde(deserialize_with = "deserialize_optional_token_vault_refresh")]
    pub token_vault_refresh_json: Option<BTreeMap<String, OAuthRefreshConfig>>,
    /// `MOA_KMS_PROVIDER`.
    pub kms_provider: Option<KmsProviderKind>,
    /// `MOA_KMS_ROOT_KEY_DIR`.
    pub kms_root_key_dir: Option<PathBuf>,
    /// `MOA_KMS_REQUIRED_GENERATION`.
    pub kms_required_generation: Option<String>,
    /// `MOA_KMS_ALLOW_EPHEMERAL`.
    pub kms_allow_ephemeral: Option<bool>,
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
    /// `MOA_LINEAGE_AUDIT_ROOT_SEED_HEX`.
    pub lineage_audit_root_seed_hex: Option<String>,
    /// `MOA_PII_VAULT_SECRET_HEX`.
    pub pii_vault_secret_hex: Option<String>,
    /// `MOA_REQUIRE_DUAL_CONTROL_FOR_ERASURE`.
    pub require_dual_control_for_erasure: Option<bool>,
    /// `MOA_LOCAL_DOCKER_ENABLED`.
    pub local_docker_enabled: Option<bool>,
    /// `MOA_LOCAL_SANDBOX_DIR`.
    pub local_sandbox_dir: Option<String>,
    /// `MOA_LOCAL_MEMORY_DIR`.
    pub local_memory_dir: Option<String>,
    /// `MOA_MEMORY_EMBEDDING_MODEL`.
    pub memory_embedding_model: Option<String>,
    /// `MOA_MEMORY_RETRIEVAL_RERANKER_MODEL`.
    pub memory_retrieval_reranker_model: Option<String>,
    /// `MOA_MEMORY_RETRIEVAL_RERANKER_LATENCY`.
    pub memory_retrieval_reranker_latency: Option<String>,
    /// `MOA_MEMORY_RETRIEVAL_LINEAGE_ENABLED`.
    pub memory_retrieval_lineage_enabled: Option<bool>,
    /// `MOA_MEMORY_RETRIEVAL_LINEAGE_SAMPLE_RATE`.
    pub memory_retrieval_lineage_sample_rate: Option<f64>,
    /// `MOA_MEMORY_DIGEST_ENABLED`.
    pub memory_digest_enabled: Option<bool>,
    /// `MOA_MEMORY_DIGEST_MAX_TOKENS`.
    pub memory_digest_max_tokens: Option<usize>,
    /// `MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS`.
    pub memory_digest_rebuild_min_interval_hours: Option<i64>,
    /// `MOA_MEMORY_EXTRACTION_ENABLED`.
    pub memory_extraction_enabled: Option<bool>,
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
    /// `MOA_NANGO_API_KEY`.
    pub nango_api_key: Option<String>,
    /// `MOA_NANGO_WEBHOOK_SIGNING_KEY`.
    pub nango_webhook_signing_key: Option<String>,
    /// `MOA_MERGE_API_BASE_URL`.
    pub merge_api_base_url: Option<String>,
    /// `MOA_MERGE_API_KEY`.
    pub merge_api_key: Option<String>,
    /// `MOA_MERGE_WEBHOOK_SIGNATURE_KEY`.
    pub merge_webhook_signature_key: Option<String>,
    /// `MOA_LLAMAPARSE_API_URL`.
    pub llamaparse_api_url: Option<String>,
    /// `MOA_LLAMAPARSE_API_KEY`.
    pub llamaparse_api_key: Option<String>,
    /// `MOA_LLAMAPARSE_WEBHOOK_SIGNING_KEY`.
    pub llamaparse_webhook_signing_key: Option<String>,
    /// `MOA_LLAMAPARSE_WEBHOOK_HEADER_NAME`.
    pub llamaparse_webhook_header_name: Option<String>,
    /// `MOA_LLAMAPARSE_WEBHOOK_HEADER_VALUE`.
    pub llamaparse_webhook_header_value: Option<String>,
    /// `MOA_LLAMAPARSE_TIER`.
    pub llamaparse_tier: Option<String>,
    /// `MOA_UNSTRUCTURED_API_URL`.
    pub unstructured_api_url: Option<String>,
    /// `MOA_UNSTRUCTURED_API_KEY`.
    pub unstructured_api_key: Option<String>,
    /// `MOA_UNSTRUCTURED_STRATEGY`.
    pub unstructured_strategy: Option<String>,
    /// `MOA_UNSTRUCTURED_CHUNKING_STRATEGY`.
    pub unstructured_chunking_strategy: Option<String>,
    /// `MOA_REDUCTO_API_URL`.
    pub reducto_api_url: Option<String>,
    /// `MOA_REDUCTO_API_KEY`.
    pub reducto_api_key: Option<String>,
    /// `MOA_REDUCTO_WEBHOOK_SIGNING_KEY`.
    pub reducto_webhook_signing_key: Option<String>,
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
    /// `MOA_PII_SERVICE_URL`; empty means unset.
    #[serde(deserialize_with = "deserialize_optional_nonempty")]
    pub pii_service_url: Option<String>,
    /// `MOA_TURBOPUFFER_API_KEY`.
    pub turbopuffer_api_key: Option<String>,
    /// `MOA_TURBOPUFFER_BASE_URL`.
    pub turbopuffer_base_url: Option<String>,
    /// `MOA_TURBOPUFFER_ENVIRONMENT`.
    pub turbopuffer_environment: Option<String>,
    /// `MOA_TURBOPUFFER_BAA`.
    pub turbopuffer_baa: Option<bool>,
    /// `MOA_TURBOPUFFER_VECTOR_TYPE`.
    pub turbopuffer_vector_type: Option<String>,
    /// `MOA_CLOUD_MEMORY_DIR`.
    pub cloud_memory_dir: Option<String>,
    /// `MOA_CLOUD_HANDS_DEFAULT_PROVIDER`.
    pub cloud_hands_default_provider: Option<String>,
    /// `MOA_CLOUD_HANDS_FALLBACK_PROVIDERS`.
    #[serde(deserialize_with = "deserialize_optional_list")]
    pub cloud_hands_fallback_providers: Option<Vec<String>>,
    /// `MOA_CLOUD_HANDS_DAYTONA_API_KEY`.
    pub cloud_hands_daytona_api_key: Option<String>,
    /// `MOA_CLOUD_HANDS_DAYTONA_API_URL`.
    pub cloud_hands_daytona_api_url: Option<String>,
    /// `MOA_CLOUD_HANDS_DAYTONA_DEFAULT_IMAGE`.
    pub cloud_hands_daytona_default_image: Option<String>,
    /// `MOA_CLOUD_HANDS_E2B_API_KEY`.
    pub cloud_hands_e2b_api_key: Option<String>,
    /// `MOA_CLOUD_HANDS_E2B_API_URL`.
    pub cloud_hands_e2b_api_url: Option<String>,
    /// `MOA_CLOUD_HANDS_E2B_DOMAIN`.
    pub cloud_hands_e2b_domain: Option<String>,
    /// `MOA_CLOUD_HANDS_E2B_TEMPLATE`.
    pub cloud_hands_e2b_template: Option<String>,
    /// `MOA_MESSAGING_SLACK_TOKEN`.
    pub messaging_slack_token: Option<String>,
    /// `MOA_MESSAGING_SLACK_APP_TOKEN`.
    pub messaging_slack_app_token: Option<String>,
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
    pub permissions_default_effect: Option<moa_core::types::action_policy::ActionPolicyEffect>,
    /// `MOA_PERMISSIONS_ADMIN_REVIEW`.
    #[serde(deserialize_with = "deserialize_optional_list")]
    pub permissions_admin_review: Option<Vec<String>>,
    /// `MOA_PERMISSIONS_ALWAYS_DENY`.
    #[serde(deserialize_with = "deserialize_optional_list")]
    pub permissions_always_deny: Option<Vec<String>>,
    /// `MOA_SESSION_BLOB_THRESHOLD_BYTES`.
    pub session_blob_threshold_bytes: Option<usize>,
    /// `MOA_SESSION_DIRECT_TURN_EVENT_APPEND`.
    pub session_direct_turn_event_append: Option<bool>,
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
    /// `MOA_RESTATE_INGRESS_URL`.
    pub restate_ingress_url: Option<String>,
    /// `MOA_RESTATE_ADMIN_URL`.
    pub restate_admin_url: Option<String>,
    /// `MOA_RESTATE_LLM_GATEWAY_URL`.
    pub restate_llm_gateway_url: Option<String>,
    /// `MOA_ORCHESTRATOR_ENDPOINT`.
    pub orchestrator_endpoint: Option<String>,
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
    /// `MOA_CLICKHOUSE_URL`; empty means unset.
    #[serde(deserialize_with = "deserialize_optional_nonempty")]
    pub clickhouse_url: Option<String>,
    /// `MOA_CLICKHOUSE_DATABASE`; empty means unset.
    #[serde(deserialize_with = "deserialize_optional_nonempty")]
    pub clickhouse_database: Option<String>,
    /// `MOA_CLICKHOUSE_USER`; empty means unset.
    #[serde(deserialize_with = "deserialize_optional_nonempty")]
    pub clickhouse_user: Option<String>,
    /// `MOA_CLICKHOUSE_PASSWORD`; empty means unset.
    #[serde(deserialize_with = "deserialize_optional_nonempty")]
    pub clickhouse_password: Option<String>,
    /// `MOA_CLICKHOUSE_LINEAGE_TTL_DAYS`.
    pub clickhouse_lineage_ttl_days: Option<u32>,
    /// `MOA_CLICKHOUSE_EXPORT_POLL_SECS`.
    pub clickhouse_export_poll_secs: Option<u64>,
    /// `MOA_CLICKHOUSE_EXPORT_BATCH_ROWS`.
    pub clickhouse_export_batch_rows: Option<usize>,
    /// `MOA_METRICS_ENABLED`.
    pub metrics_enabled: Option<bool>,
    /// `MOA_METRICS_LISTEN`.
    pub metrics_listen: Option<String>,
    /// `MOA_BUDGETS_DAILY_TENANT_CENTS`.
    pub budgets_daily_tenant_cents: Option<u32>,
    /// `MOA_SESSION_LIMITS_TURN_ADMISSION_FLEET_LIMIT`.
    pub session_limits_turn_admission_fleet_limit: Option<u32>,
    /// `MOA_SESSION_LIMITS_TURN_ADMISSION_TENANT_LIMIT`.
    pub session_limits_turn_admission_tenant_limit: Option<u32>,
    /// `MOA_SESSION_LIMITS_TURN_ADMISSION_LEASE_TTL_MS`.
    pub session_limits_turn_admission_lease_ttl_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_TURN_ADMISSION_RETRY_AFTER_MS`.
    pub session_limits_turn_admission_retry_after_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_MAX_PENDING_MESSAGES`.
    pub session_limits_max_pending_messages: Option<u32>,
    /// `MOA_SESSION_LIMITS_MAX_TURNS`.
    pub session_limits_max_turns: Option<u32>,
    /// `MOA_SESSION_LIMITS_SIMPLE_MAX_TURNS`.
    pub session_limits_simple_max_turns: Option<u32>,
    /// `MOA_SESSION_LIMITS_STANDARD_MAX_TURNS`.
    pub session_limits_standard_max_turns: Option<u32>,
    /// `MOA_SESSION_LIMITS_MAX_MODEL_TURNS_DELEGATION`.
    pub session_limits_max_model_turns_delegation: Option<u32>,
    /// `MOA_SESSION_LIMITS_MAX_TOOL_CALLS`.
    pub session_limits_max_tool_calls: Option<u32>,
    /// `MOA_SESSION_LIMITS_LOOP_DETECTION_THRESHOLD`.
    pub session_limits_loop_detection_threshold: Option<u32>,
    /// `MOA_SESSION_LIMITS_PROGRESS_FIRST_DELAY_MS`.
    pub session_limits_progress_first_delay_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_PROGRESS_INTERVAL_MS`.
    pub session_limits_progress_interval_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_PROGRESS_NARRATION_ENABLED`.
    pub session_limits_progress_narration_enabled: Option<bool>,
    /// `MOA_SESSION_LIMITS_PROGRESS_NARRATION_MODEL`.
    pub session_limits_progress_narration_model: Option<String>,
    /// `MOA_SESSION_LIMITS_PROGRESS_NARRATION_INTERVAL_MS`.
    pub session_limits_progress_narration_interval_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_PROGRESS_NARRATION_MAX_PER_WINDOW`.
    pub session_limits_progress_narration_max_per_window: Option<u32>,
    /// `MOA_SESSION_LIMITS_PROGRESS_NARRATION_MAX_TOKENS`.
    pub session_limits_progress_narration_max_tokens: Option<u32>,
    /// `MOA_SESSION_LIMITS_WORKER_CLEANUP_GRACE_MS`.
    pub session_limits_worker_cleanup_grace_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_WORKER_RESUME_MAX_PER_WINDOW`.
    pub session_limits_worker_resume_max_per_window: Option<u32>,
    /// `MOA_SESSION_LIMITS_WORKER_RESUME_WINDOW_MS`.
    pub session_limits_worker_resume_window_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_WORKER_INPUT_TIMEOUT_MS`.
    pub session_limits_worker_input_timeout_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_WORKER_HEARTBEAT_INTERVAL_MS`.
    pub session_limits_worker_heartbeat_interval_ms: Option<u64>,
    /// `MOA_SESSION_LIMITS_WORKER_HEARTBEAT_STALE_MS`.
    pub session_limits_worker_heartbeat_stale_ms: Option<u64>,
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
    /// `MOA_LEARNING_SKILLS_IMPROVE_ROUTE_SIMILARITY`.
    pub learning_skills_improve_route_similarity: Option<f64>,
    /// `MOA_LEARNING_SKILLS_PROPOSAL_DEDUP_SIMILARITY`.
    pub learning_skills_proposal_dedup_similarity: Option<f64>,
    /// `MOA_LEARNING_SEGMENTS_IDLE_GAP_MINUTES`.
    pub learning_segments_idle_gap_minutes: Option<u64>,
    /// `MOA_LEARNING_EMBEDDINGS_EXPERIENCE_BATCH_SIZE`.
    pub learning_embeddings_experience_batch_size: Option<usize>,
    /// `MOA_LEARNING_EMBEDDINGS_EXPERIENCE_LOOKBACK_DAYS`.
    pub learning_embeddings_experience_lookback_days: Option<i64>,
    /// `MOA_LEARNING_EMBEDDINGS_SKILL_BATCH_SIZE`.
    pub learning_embeddings_skill_batch_size: Option<usize>,
    /// `MOA_LEARNING_RECURRENCE_MIN_OCCURRENCES`.
    pub learning_recurrence_min_occurrences: Option<usize>,
    /// `MOA_LEARNING_RECURRENCE_LOOKBACK_DAYS`.
    pub learning_recurrence_lookback_days: Option<i64>,
    /// `MOA_LEARNING_RECURRENCE_RELAXED_MIN_TOOL_CALLS`.
    pub learning_recurrence_relaxed_min_tool_calls: Option<usize>,
    /// `MOA_LEARNING_RECURRENCE_REJECTION_COOLDOWN_DAYS`.
    pub learning_recurrence_rejection_cooldown_days: Option<i64>,
    /// `MOA_LEARNING_RECURRENCE_CLUSTER_SIMILARITY`.
    pub learning_recurrence_cluster_similarity: Option<f64>,
    /// `MOA_LEARNING_RECURRENCE_MAX_CANDIDATE_GROUPS`.
    pub learning_recurrence_max_candidate_groups: Option<usize>,
    /// `MOA_EXECUTION_PLANNER_REPAIR_ATTEMPTS`.
    pub execution_planner_repair_attempts: Option<u32>,
    /// `MOA_EXECUTION_REPEATED_FAILURE_LIMIT`.
    pub execution_repeated_failure_limit: Option<u32>,
    /// `MOA_EXECUTION_MAX_TASKS`.
    pub execution_max_tasks: Option<u64>,
    /// `MOA_EXECUTION_MAX_TOKENS`.
    pub execution_max_tokens: Option<u64>,
    /// `MOA_EXECUTION_MAX_TOOL_CALLS`.
    pub execution_max_tool_calls: Option<u64>,
    /// `MOA_EXECUTION_MAX_RETRIEVED_BYTES`.
    pub execution_max_retrieved_bytes: Option<u64>,
    /// `MOA_EXECUTION_MAX_COST_MICROUSD`.
    pub execution_max_cost_microusd: Option<u64>,
    /// `MOA_EXECUTION_UNATTENDED_MAX_COST_MICROUSD`.
    pub execution_unattended_max_cost_microusd: Option<u64>,
    /// `MOA_EXECUTION_AGENT_TURN_COST_MICROUSD`.
    pub execution_agent_turn_cost_microusd: Option<u64>,
    /// `MOA_EXECUTION_AGENT_TURN_TOKENS`.
    pub execution_agent_turn_tokens: Option<u64>,
    /// `MOA_EXECUTION_AGENT_TURN_TOOL_CALLS`.
    pub execution_agent_turn_tool_calls: Option<u64>,
    /// `MOA_EXECUTION_AGENT_TURN_RETRIEVED_BYTES`.
    pub execution_agent_turn_retrieved_bytes: Option<u64>,
    /// `MOA_EXECUTION_VERIFIER_TURN_COST_MICROUSD`.
    pub execution_verifier_turn_cost_microusd: Option<u64>,
    /// `MOA_EXECUTION_VERIFIER_TURN_TOKENS`.
    pub execution_verifier_turn_tokens: Option<u64>,
    /// `MOA_EXECUTION_VERIFIER_TURN_TOOL_CALLS`.
    pub execution_verifier_turn_tool_calls: Option<u64>,
    /// `MOA_EXECUTION_VERIFIER_TURN_RETRIEVED_BYTES`.
    pub execution_verifier_turn_retrieved_bytes: Option<u64>,
    /// `MOA_CONTEXT_SNAPSHOT_ENABLED`.
    pub context_snapshot_enabled: Option<bool>,
    /// `MOA_CONTEXT_SNAPSHOT_MAX_SIZE_BYTES`.
    pub context_snapshot_max_size_bytes: Option<usize>,
}

impl EnvOverlay {
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
        database::validate_urls(self)?;
        security::validate_urls(self)?;
        providers::validate_urls(self)?;
        messaging::validate_urls(self)?;
        observability::validate_urls(self)
    }

    /// Applies this overlay to a typed MOA config.
    pub fn apply_to(&self, config: &mut MoaConfig) -> Result<()> {
        let mut config_value = serde_json::to_value(&*config).map_err(config_codec_error)?;
        let mut schema_value = config_value.clone();
        seed_optional_sections(&mut schema_value)?;

        for (field, value) in self.present_values()? {
            let path = overlay_path(&field, &schema_value)?;
            insert_overlay_value(&mut config_value, &path, value)?;
        }

        let mut next: MoaConfig =
            serde_json::from_value(config_value).map_err(config_codec_error)?;
        self.finalize_intentional_fanout(&mut next);
        self.validate_required_sections(&next)?;
        next.validate()?;
        *config = next;
        Ok(())
    }

    fn present_values(&self) -> Result<Vec<(String, Value)>> {
        let Value::Object(values) = serde_json::to_value(self).map_err(config_codec_error)? else {
            return Err(MoaError::ConfigError(
                "MOA env overlay did not serialize to an object".to_string(),
            ));
        };

        Ok(values
            .into_iter()
            .filter(|(_, value)| !value.is_null())
            .collect())
    }
}

fn config_codec_error(error: serde_json::Error) -> MoaError {
    MoaError::ConfigError(format!("MOA env overlay could not update config: {error}"))
}

fn overlay_path(field: &str, schema: &Value) -> Result<Vec<String>> {
    if let Some(path) = exact_overlay_path(field) {
        return Ok(path);
    }

    for (prefix, path_prefix) in PREFIX_ALIASES {
        let Some(remainder) = field.strip_prefix(prefix) else {
            continue;
        };
        let Some(remainder) = remainder.strip_prefix('_') else {
            continue;
        };
        let base = value_at_path(schema, path_prefix).ok_or_else(|| {
            MoaError::ConfigError(format!(
                "MOA env overlay mapping `{field}` references missing config path `{}`",
                path_prefix.join(".")
            ))
        })?;
        let mut path = strings(path_prefix);
        match resolve_path(&split_segments(remainder), base) {
            Some(resolved) => path.extend(resolved),
            None => path.push(remainder.to_string()),
        }
        return Ok(path);
    }

    resolve_path(&split_segments(field), schema).ok_or_else(|| {
        MoaError::ConfigError(format!(
            "MOA env overlay field `{field}` does not map to MoaConfig"
        ))
    })
}

fn exact_overlay_path(field: &str) -> Option<Vec<String>> {
    if field == "mcp_servers_json" {
        return Some(vec!["mcp_servers".to_string()]);
    }
    providers::exact_overlay_path(field)
        .or_else(|| security::exact_overlay_path(field))
        .or_else(|| messaging::exact_overlay_path(field))
}

fn deserialize_optional_oauth_clients<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<OAuthClientConfig>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_json(deserializer, "MOA_AUTH_OAUTH_CLIENTS_JSON")
}

fn deserialize_optional_token_vault_refresh<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<BTreeMap<String, OAuthRefreshConfig>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_json(deserializer, "MOA_TOKEN_VAULT_REFRESH_JSON")
}

fn deserialize_optional_mcp_servers<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<Vec<McpServerConfig>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_json(deserializer, "MOA_MCP_SERVERS_JSON")
}

fn deserialize_optional_json<'de, D, T>(
    deserializer: D,
    env_name: &str,
) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Option::<String>::deserialize(deserializer)?;
    value
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| {
                serde::de::Error::custom(format!("{env_name} contains invalid JSON: {error}"))
            })
        })
        .transpose()
}
const PREFIX_ALIASES: &[(&str, &[&str])] = &[
    ("session_attachment", &["session", "attachments"]),
    ("cloud_hands", &["cloud", "hands"]),
    ("turbopuffer", &["memory", "vector", "turbopuffer"]),
    ("anthropic", &["providers", "anthropic"]),
    ("openai", &["providers", "openai"]),
    ("google", &["providers", "google"]),
    ("cohere", &["providers", "cohere"]),
    ("zeroentropy", &["providers", "zeroentropy"]),
    ("nango", &["knowledge", "nango"]),
    ("merge", &["knowledge", "merge"]),
    ("llamaparse", &["knowledge", "llamaparse"]),
    ("unstructured", &["knowledge", "unstructured"]),
    ("reducto", &["knowledge", "reducto"]),
];

fn resolve_path(segments: &[&str], value: &Value) -> Option<Vec<String>> {
    let object = value.as_object()?;
    for split_at in (1..=segments.len()).rev() {
        let key = segments[..split_at].join("_");
        let Some(child) = object.get(&key) else {
            continue;
        };
        if split_at == segments.len() {
            return Some(vec![key]);
        }
        if child.is_object() {
            let mut path = vec![key];
            path.extend(resolve_path(&segments[split_at..], child)?);
            return Some(path);
        }
    }
    None
}

fn insert_overlay_value(target: &mut Value, path: &[String], value: Value) -> Result<()> {
    let mut current = target;
    let mut traversed = Vec::new();
    for segment in &path[..path.len().saturating_sub(1)] {
        traversed.push(segment.as_str());
        let object = current.as_object_mut().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "MOA env overlay target `{}` is not an object",
                traversed.join(".")
            ))
        })?;
        let entry = object.entry(segment.clone()).or_insert(Value::Null);
        if entry.is_null() {
            *entry = optional_section_seed(&traversed).unwrap_or_else(|| Value::Object(Map::new()));
        }
        current = entry;
    }

    let leaf = path.last().ok_or_else(|| {
        MoaError::ConfigError("MOA env overlay resolved an empty config path".to_string())
    })?;
    let object = current.as_object_mut().ok_or_else(|| {
        MoaError::ConfigError(format!(
            "MOA env overlay target `{}` is not an object",
            path[..path.len().saturating_sub(1)].join(".")
        ))
    })?;
    object.insert(leaf.clone(), value);
    Ok(())
}

fn seed_optional_sections(config: &mut Value) -> Result<()> {
    for path in [
        &["auth", "auth0"][..],
        &["auth", "oidc"],
        &["authz", "openfga"],
        &["clickhouse"],
        &["cloud", "hands"],
    ] {
        insert_seed(config, path)?;
    }
    Ok(())
}

fn insert_seed(target: &mut Value, path: &[&str]) -> Result<()> {
    let mut current = target;
    for (index, segment) in path.iter().enumerate() {
        let object = current.as_object_mut().ok_or_else(|| {
            MoaError::ConfigError(format!(
                "MOA env overlay schema path `{}` is not an object",
                path[..index].join(".")
            ))
        })?;
        let entry = object.entry((*segment).to_string()).or_insert(Value::Null);
        if index == path.len() - 1 {
            if entry.is_null() {
                *entry = optional_section_seed(path).ok_or_else(|| {
                    MoaError::ConfigError(format!(
                        "MOA env overlay schema path `{}` has no seed",
                        path.join(".")
                    ))
                })?;
            }
            return Ok(());
        }
        if entry.is_null() {
            *entry = Value::Object(Map::new());
        }
        current = entry;
    }
    Ok(())
}

fn optional_section_seed(path: &[&str]) -> Option<Value> {
    security::optional_section_seed(path)
        .or_else(|| observability::optional_section_seed(path))
        .or_else(|| providers::optional_section_seed(path))
}
fn value_at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for segment in path {
        current = current.as_object()?.get(*segment)?;
    }
    Some(current)
}

fn split_segments(field: &str) -> Vec<&str> {
    field.split('_').collect()
}

fn strings(path: &[&str]) -> Vec<String> {
    path.iter().map(|segment| (*segment).to_string()).collect()
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

/// Environment variable that switches the unknown-`MOA_*` audit from warn to
/// fail. Set to a truthy value (`1`, `true`, `yes`, `on`) in production to fail
/// startup on a misspelled overlay variable instead of silently ignoring it.
pub const CONFIG_ENV_STRICT_VAR: &str = "MOA_CONFIG_ENV_STRICT";

/// Maximum edit distance for a "did you mean" suggestion against a known key.
const SUGGESTION_MAX_DISTANCE: usize = 3;

/// Approved `MOA_*` variable-name prefixes that are read directly by tooling,
/// deploy scripts, live-test gates, or provider credential lookups rather than
/// through the typed overlay. Each is verified to share no prefix with a real
/// overlay field, so allowlisting the namespace cannot mask an overlay typo.
///
/// Evidence: `grep -roE 'var(_os)?\("MOA_..."'` across `crates/`, plus `MOA_*`
/// names in `scripts/`, `Makefile`, `docker-compose*.yml`, and `.env*`.
const ALLOWLIST_PREFIXES: &[&str] = &[
    "MOA_RUN_",                // live/docker/chaos test gates (MOA_RUN_LIVE_*, ...)
    "MOA_TEST_",               // test-only config (Auth0 test tenant, ...)
    "MOA_FIXTURE_",            // integration-test fixtures (fixture OpenFGA, ...)
    "MOA_PENTEST_",            // cross-tenant pentest harness knobs
    "MOA_LOADTEST_",           // k6/loadtest harness knobs
    "MOA_CLEAN_E2E_",          // run-clean-e2e.sh harness knobs
    "MOA_RUSTFS_",             // local compose object-store ports
    "MOA_FGA_",                // OpenFGA bootstrap script knobs
    "MOA_BOOTSTRAP_",          // tenant/user bootstrap script knobs
    "MOA_EVAL_",               // eval harness knobs
    "MOA_TRACE_",              // tracing/debug sampling toggles
    "MOA_TWILIO_",             // Twilio live-messaging credentials
    "MOA_DAYTONA_",            // Daytona sandbox credentials (compose)
    "MOA_NEON_",               // Neon branching credentials (deploy/tests)
    "MOA_OPENFGA_",            // OpenFGA compose/bootstrap vars (not MOA_AUTHZ_OPENFGA_*)
    "MOA_POSTMARK_",           // Postmark live-email credentials
    "MOA_E2B_",                // E2B sandbox credentials
    "MOA_OPENROUTER_",         // OpenRouter credentials (deploy)
    "MOA_EDGE_",               // edge binary bind/upstream (not an overlay field)
    "MOA_RESTATE_DEPLOYMENT_", // Restate deploy-registration vars (not overlay)
];

/// Approved `MOA_*` variables that live in a namespace shared with real overlay
/// fields (so a prefix would be unsafe) or that stand alone. Kept exact.
const ALLOWLIST_EXACT: &[&str] = &[
    "MOA_CONFIG_ENV_STRICT", // this audit's own strictness switch
    "MOA_AUDIT_BUCKET",      // audit bucket bootstrap (MOA_AUDIT_ collides with overlay)
    "MOA_AUDIT_OBJECT_LOCK_MODE",
    "MOA_AUDIT_RETENTION_YEARS",
    "MOA_AUTH_HEADER_TRUST",
    "MOA_AUTH0_CLIENT_ID",
    "MOA_AUTHZ_DECISION_CACHE_TTL_MS", // MOA_AUTHZ_ collides with overlay
    "MOA_AUTHZ_OPENFGA_STORE_NAME",
    "MOA_DEREGISTER_ON_SHUTDOWN",
    "MOA_DOCKER_SECCOMP_PROFILE",
    "MOA_LINEAGE_SINK",
    "MOA_MEMORY_AUTO_BOOTSTRAP", // MOA_MEMORY_ collides with overlay
    "MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK",
    "MOA_MEMORY_EXTRACTION_MODEL",
    "MOA_MEMORY_EXTRACTION_TIMEOUT_MS",
    "MOA_ORCHESTRATOR_BIN", // MOA_ORCHESTRATOR_ collides with overlay
    "MOA_ORCHESTRATOR_FEATURES",
    "MOA_PERSIST_TURN_METRICS",
    "MOA_PROVIDERS_OVERRIDE",
    "MOA_REQUIRE_RESTATE_REGISTRATION_FOR_READINESS",
    "MOA_SCIM_BASE_URL",
    "MOA_SKIP_FGA",
    "MOA_TOXIPROXY_URL",
    "MOA_TURBOPUFFER_LIVE_NEWS_FACTS", // MOA_TURBOPUFFER_ collides with overlay
    "MOA_VENDOR_NAME",
];

/// Set of `MOA_*` environment variable names recognized by the typed overlay,
/// derived at runtime from the overlay struct itself (serde is the source of
/// truth, so a renamed field updates this set automatically).
fn known_overlay_env_keys() -> std::collections::BTreeSet<String> {
    match serde_json::to_value(EnvOverlay::default()) {
        Ok(Value::Object(map)) => map
            .keys()
            .map(|field| format!("MOA_{}", field.to_uppercase()))
            .collect(),
        _ => std::collections::BTreeSet::new(),
    }
}

/// Returns true when `name` matches an approved allowlist prefix or exact entry.
fn is_allowlisted(name: &str) -> bool {
    ALLOWLIST_EXACT.contains(&name)
        || ALLOWLIST_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix))
}

/// Returns the sorted, de-duplicated `MOA_*` variable names in `names` that are
/// neither a known overlay field nor allowlisted.
fn unknown_moa_env_vars<I>(names: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    let known = known_overlay_env_keys();
    let mut unknown = names
        .into_iter()
        .map(|name| name.to_uppercase())
        .filter(|name| name.starts_with("MOA_"))
        .filter(|name| !known.contains(name) && !is_allowlisted(name))
        .collect::<Vec<_>>();
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Returns the closest known overlay key to `name` within
/// [`SUGGESTION_MAX_DISTANCE`], for a "did you mean" hint.
fn nearest_known_key(name: &str) -> Option<String> {
    known_overlay_env_keys()
        .into_iter()
        .map(|candidate| {
            let distance = levenshtein(name, &candidate);
            (distance, candidate)
        })
        .filter(|(distance, _)| *distance <= SUGGESTION_MAX_DISTANCE)
        .min()
        .map(|(_, candidate)| candidate)
}

/// Iterative Levenshtein edit distance (in-tree; no dependency).
fn levenshtein(left: &str, right: &str) -> usize {
    let left = left.as_bytes();
    let right = right.as_bytes();
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0usize; right.len() + 1];
    for (i, &lchar) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, &rchar) in right.iter().enumerate() {
            let cost = usize::from(lchar != rchar);
            current[j + 1] = (previous[j] + cost)
                .min(previous[j + 1] + 1)
                .min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

impl EnvOverlay {
    /// Audits process `MOA_*` environment variables against the overlay registry.
    ///
    /// envy silently ignores unrecognized prefixed variables, so a typo like
    /// `MOA_MODELS_MIAN` falls back to defaults with no signal. This flags any
    /// `MOA_*` name that is neither a typed overlay field nor an approved special
    /// key: in strict mode it fails startup listing the offenders (with a
    /// nearest-match suggestion); otherwise it logs a warning.
    pub(crate) fn audit_env_registry<I>(names: I, strict: bool) -> Result<()>
    where
        I: IntoIterator<Item = String>,
    {
        let unknown = unknown_moa_env_vars(names);
        if unknown.is_empty() {
            return Ok(());
        }
        let report = unknown
            .iter()
            .map(|name| match nearest_known_key(name) {
                Some(suggestion) => format!("{name} (did you mean {suggestion}?)"),
                None => name.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        if strict {
            Err(MoaError::ConfigError(format!(
                "unrecognized MOA_* environment variable(s): {report}"
            )))
        } else {
            tracing::warn!(
                unrecognized = %report,
                "ignoring unrecognized MOA_* environment variable(s) (typo?); \
                 set MOA_CONFIG_ENV_STRICT=1 to fail startup instead"
            );
            Ok(())
        }
    }

    /// Reads [`CONFIG_ENV_STRICT_VAR`] as a boolean strictness flag.
    #[must_use]
    pub(crate) fn env_registry_strict_from_env() -> bool {
        std::env::var(CONFIG_ENV_STRICT_VAR)
            .ok()
            .is_some_and(|value| {
                matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
    }
}

/// Deserializes a comma-separated env value into a trimmed, non-empty list.
fn parse_error(env_name: &'static str, value: &str, error: impl std::fmt::Display) -> MoaError {
    MoaError::ConfigError(format!("{env_name} value `{value}` is invalid: {error}"))
}

fn validate_url(env_name: &'static str, value: &Option<String>) -> Result<()> {
    if let Some(value) = value {
        url::Url::parse(value).map_err(|error| parse_error(env_name, value, error))?;
    }
    Ok(())
}

/// Requires that a partially configured nested section has a non-empty field.
pub(crate) fn require_non_empty(env_name: &'static str, value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(MoaError::ConfigError(format!(
            "{env_name} is required when configuring this section"
        )));
    }
    Ok(())
}

/// Returns whether any field in one nested overlay section was set.
pub(crate) fn any_present(values: &[bool]) -> bool {
    values.iter().any(|value| *value)
}

#[cfg(test)]
fn env_pairs<const N: usize>(pairs: [(&str, &str); N]) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[cfg(test)]
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

#[cfg(test)]
mod tests;
