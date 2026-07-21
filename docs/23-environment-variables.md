# Environment Variables

This is the reference for MOA's runtime configuration through environment
variables. The variable tables below are **generated from source** (the
`EnvOverlay` field set and `MoaConfig::default()`), so they stay accurate as
config changes — see [Regenerating the tables](#regenerating-the-tables).

## How configuration works

MOA is configured from two layers, highest precedence first:

1. **`MOA_*` environment variables** — a flat overlay applied on top of the file
   config. In cloud/Kubernetes this is the primary mechanism.
2. **Built-in defaults** — every field has a default from its Rust `Default`
   impl. A field left unset keeps its default.

The environment overlay always wins over the defaults. The typed defaults are
what you get with no `MOA_*` variables set at all.

### Variable naming

Each overlay variable is `MOA_` + the overlay field name, uppercased. The field
maps to a nested config path, shown in the **Config path** column:

- `MOA_MODELS_MAIN` → `models.main`
- `MOA_PROVIDERS_CONCURRENCY_SCOPE` → `providers.concurrency.scope`
- `MOA_ANTHROPIC_API_KEY` → `providers.anthropic.api_key`

There is **no** double-underscore nested form for the application — every
variable uses single underscores as shown above.

### Unknown-variable check (typo protection)

`envy` silently ignores an unrecognized `MOA_*` variable, so a typo like
`MOA_MODELS_MIAN` would quietly fall back to the default. On startup MOA audits
every `MOA_*` variable in the environment against the known overlay fields plus
an allowlist of approved special variables (see
[Special variables](#special-non-overlay-variables)):

- **Default (warn):** unrecognized names are logged as a warning and ignored,
  with a "did you mean `MOA_MODELS_MAIN`?" suggestion.
- **Strict:** set `MOA_CONFIG_ENV_STRICT=1` (or `true`/`yes`/`on`) to **fail
  startup** listing the offending names. Recommended in production.

### Secrets

Variables marked **(secret)** carry credentials (API keys, tokens, private keys,
HMAC/hex secrets). In production, source them from a secret store (Kubernetes
Secret, Vault) rather than a checked-in file or plain env block.

## Overlay variable reference

Grouped by top-level config section. `_unset_`/`_none_` means the field is
`None`/absent by default; `_empty_` means an empty string default.

### `general`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_GENERAL_DEFAULT_PROVIDER` | `general.default_provider` | openai | Default provider key |
| `MOA_GENERAL_REASONING_EFFORT` | `general.reasoning_effort` | medium | Requested reasoning effort |
| `MOA_GENERAL_USER_INSTRUCTIONS` | `general.user_instructions` | _none_ | Optional user-level preferences injected into the prompt |
| `MOA_GENERAL_WEB_SEARCH_ENABLED` | `general.web_search_enabled` | true | Whether provider-native web search should be offered to supported models |
| `MOA_GENERAL_WORKSPACE_INSTRUCTIONS` | `general.workspace_instructions` | _none_ | Optional repository-workspace instructions injected into the prompt |

### `mcp_servers`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_MCP_SERVERS_JSON` | `mcp_servers` | [] | JSON array of complete MCP server objects; replaces file-backed server configuration. `trust_tool_annotations` defaults to `false` and must be enabled per server before a negotiated standard `idempotentHint` can permit retries. |

### `execution`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_EXECUTION_AGENT_TURN_COST_MICROUSD` | `execution.agent_turn_cost_microusd` | 100000 | Worst-case integer micro-USD estimate for one agent turn |
| `MOA_EXECUTION_AGENT_TURN_RETRIEVED_BYTES` | `execution.agent_turn_retrieved_bytes` | 10000000 | Worst-case retrieved-byte estimate for one agent turn |
| `MOA_EXECUTION_AGENT_TURN_TOKENS` | `execution.agent_turn_tokens` | 8000 | Worst-case token estimate for one agent turn |
| `MOA_EXECUTION_AGENT_TURN_TOOL_CALLS` | `execution.agent_turn_tool_calls` | 8 | Worst-case governed tool-call estimate for one agent turn |
| `MOA_EXECUTION_MAX_COST_MICROUSD` | `execution.max_cost_microusd` | 100000000 | Default run cost limit in integer micro-USD |
| `MOA_EXECUTION_MAX_RETRIEVED_BYTES` | `execution.max_retrieved_bytes` | 10000000000 | Default run retrieved-byte limit |
| `MOA_EXECUTION_MAX_TASKS` | `execution.max_tasks` | 10000 | Default logical-task limit; this is not an active-worker cap |
| `MOA_EXECUTION_MAX_TOKENS` | `execution.max_tokens` | 10000000 | Default run token limit |
| `MOA_EXECUTION_MAX_TOOL_CALLS` | `execution.max_tool_calls` | 100000 | Default governed tool-call limit |
| `MOA_EXECUTION_PLANNER_REPAIR_ATTEMPTS` | `execution.planner_repair_attempts` | 1 | Maximum repair attempts for an invalid initial planner response |
| `MOA_EXECUTION_REPEATED_FAILURE_LIMIT` | `execution.repeated_failure_limit` | 3 | Repeated normalized failure count that stops replanning |
| `MOA_EXECUTION_UNATTENDED_MAX_COST_MICROUSD` | `execution.unattended_max_cost_microusd` | 5000000 | Cost threshold above which a compiled run requires owning-user confirmation |
| `MOA_EXECUTION_VERIFIER_TURN_COST_MICROUSD` | `execution.verifier_turn_cost_microusd` | 200000 | Worst-case integer micro-USD estimate for one completion-verifier turn |
| `MOA_EXECUTION_VERIFIER_TURN_RETRIEVED_BYTES` | `execution.verifier_turn_retrieved_bytes` | 1000000 | Worst-case retrieved-byte estimate for one completion-verifier turn |
| `MOA_EXECUTION_VERIFIER_TURN_TOKENS` | `execution.verifier_turn_tokens` | 16000 | Worst-case token estimate for one completion-verifier turn |
| `MOA_EXECUTION_VERIFIER_TURN_TOOL_CALLS` | `execution.verifier_turn_tool_calls` | 4 | Worst-case governed tool-call estimate for one completion-verifier turn |

### `models`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_MODELS_AUXILIARY` | `models.auxiliary` | _none_ | Optional lower-cost model for auxiliary tasks |
| `MOA_MODELS_FALLBACK_MODELS` | `models.fallback_models` | _unset_ | Ordered fallback chain for the main-loop model, each `provider:model` or a bare model id |
| `MOA_MODELS_MAIN` | `models.main` | gpt-5.4 | Default model for the primary user-facing agent loop |

### `providers`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_ANTHROPIC_API_KEY` | `providers.anthropic.api_key` | _empty_ | API key value loaded from runtime configuration **(secret)** |
| `MOA_ANTHROPIC_CAPABILITIES_DATA_RESIDENCY` | `providers.anthropic.capabilities.data_residency` | _unset_ | Contractual data-residency region, or unset when none is asserted |
| `MOA_ANTHROPIC_CAPABILITIES_PRIVATE_DEPLOYMENT` | `providers.anthropic.capabilities.private_deployment` | _unset_ | Whether the endpoint is a private or self-hosted deployment |
| `MOA_ANTHROPIC_CAPABILITIES_ZERO_RETENTION` | `providers.anthropic.capabilities.zero_retention` | _unset_ | Whether the endpoint guarantees no training on or retention of requests |
| `MOA_ANTHROPIC_MAX_CONCURRENT_REQUESTS` | `providers.anthropic.max_concurrent_requests` | _unset_ | In-flight concurrency ceiling for this provider account — the natural place to express the credential's tier |
| `MOA_ANTHROPIC_MAX_INPUTS_PER_MIN` | `providers.anthropic.max_inputs_per_min` | _unset_ | Optional per-minute input-rate cap; `None` keeps the provider default |
| `MOA_ANTHROPIC_MAX_REQUESTS_PER_MIN` | `providers.anthropic.max_requests_per_min` | _unset_ | Optional per-minute request-rate cap; `None` keeps the provider default |
| `MOA_COHERE_API_KEY` | `providers.cohere.api_key` | _empty_ | API key value loaded from runtime configuration **(secret)** |
| `MOA_COHERE_MAX_CONCURRENT_REQUESTS` | `providers.cohere.max_concurrent_requests` | _unset_ | In-flight concurrency ceiling for this provider account — the natural place to express the credential's tier |
| `MOA_COHERE_MAX_INPUTS_PER_MIN` | `providers.cohere.max_inputs_per_min` | _unset_ | Optional per-minute input-rate cap; `None` keeps the provider default |
| `MOA_COHERE_MAX_REQUESTS_PER_MIN` | `providers.cohere.max_requests_per_min` | _unset_ | Optional per-minute request-rate cap; `None` keeps the provider default |
| `MOA_GOOGLE_API_KEY` | `providers.google.api_key` | _empty_ | API key value loaded from runtime configuration **(secret)** |
| `MOA_GOOGLE_CAPABILITIES_DATA_RESIDENCY` | `providers.google.capabilities.data_residency` | _unset_ | Contractual data-residency region, or unset when none is asserted |
| `MOA_GOOGLE_CAPABILITIES_PRIVATE_DEPLOYMENT` | `providers.google.capabilities.private_deployment` | _unset_ | Whether the endpoint is a private or self-hosted deployment |
| `MOA_GOOGLE_CAPABILITIES_ZERO_RETENTION` | `providers.google.capabilities.zero_retention` | _unset_ | Whether the endpoint guarantees no training on or retention of requests |
| `MOA_GOOGLE_MAX_CONCURRENT_REQUESTS` | `providers.google.max_concurrent_requests` | _unset_ | In-flight concurrency ceiling for this provider account — the natural place to express the credential's tier |
| `MOA_GOOGLE_MAX_INPUTS_PER_MIN` | `providers.google.max_inputs_per_min` | _unset_ | Optional per-minute input-rate cap; `None` keeps the provider default |
| `MOA_GOOGLE_MAX_REQUESTS_PER_MIN` | `providers.google.max_requests_per_min` | _unset_ | Optional per-minute request-rate cap; `None` keeps the provider default |
| `MOA_OPENAI_API_KEY` | `providers.openai.api_key` | _empty_ | API key value loaded from runtime configuration **(secret)** |
| `MOA_OPENAI_CAPABILITIES_DATA_RESIDENCY` | `providers.openai.capabilities.data_residency` | _unset_ | Contractual data-residency region, or unset when none is asserted |
| `MOA_OPENAI_CAPABILITIES_PRIVATE_DEPLOYMENT` | `providers.openai.capabilities.private_deployment` | _unset_ | Whether the endpoint is a private or self-hosted deployment |
| `MOA_OPENAI_CAPABILITIES_ZERO_RETENTION` | `providers.openai.capabilities.zero_retention` | _unset_ | Whether the endpoint guarantees no training on or retention of requests |
| `MOA_OPENAI_MAX_CONCURRENT_REQUESTS` | `providers.openai.max_concurrent_requests` | _unset_ | In-flight concurrency ceiling for this provider account — the natural place to express the credential's tier |
| `MOA_OPENAI_MAX_INPUTS_PER_MIN` | `providers.openai.max_inputs_per_min` | _unset_ | Optional per-minute input-rate cap; `None` keeps the provider default |
| `MOA_OPENAI_MAX_REQUESTS_PER_MIN` | `providers.openai.max_requests_per_min` | _unset_ | Optional per-minute request-rate cap; `None` keeps the provider default |
| `MOA_PROVIDERS_CONCURRENCY_BLOCK_THRESHOLD_MS` | `providers.concurrency.block_threshold_ms` | 2000 | How long a caller waits for a slot before reporting "saturated", in ms |
| `MOA_PROVIDERS_CONCURRENCY_DEFAULT_MAX_IN_FLIGHT` | `providers.concurrency.default_max_in_flight` | 16 | Fallback in-flight ceiling for any provider that sets no `max_concurrent_requests` of its own (`0` = unbounded) |
| `MOA_PROVIDERS_CONCURRENCY_LEASE_TTL_MS` | `providers.concurrency.lease_ttl_ms` | 600000 | Global-scope lease time-to-live, in ms: the crash backstop for a held slot |
| `MOA_PROVIDERS_CONCURRENCY_SCOPE` | `providers.concurrency.scope` | local | Whether the ceiling is enforced per process or shared across replicas |
| `MOA_PROVIDERS_ROUTING_POLICY_ALLOWED_PROVIDERS` | `providers.routing_policy.allowed_providers` | _unset_ | Allowlist of provider ids; empty means no allowlist, non-empty restricts routing to exactly these providers |
| `MOA_PROVIDERS_ROUTING_POLICY_DENIED_PROVIDERS` | `providers.routing_policy.denied_providers` | _unset_ | Denylist of provider ids that must never serve this deployment |
| `MOA_PROVIDERS_ROUTING_POLICY_REQUIRE_PRIVATE_DEPLOYMENT` | `providers.routing_policy.require_private_deployment` | _unset_ | Require the selected provider to assert a private/self-hosted deployment |
| `MOA_PROVIDERS_ROUTING_POLICY_REQUIRE_ZERO_RETENTION` | `providers.routing_policy.require_zero_retention` | _unset_ | Require the selected provider to assert zero request retention |
| `MOA_PROVIDERS_ROUTING_POLICY_REQUIRED_RESIDENCY` | `providers.routing_policy.required_residency` | _unset_ | Require the selected provider to assert this data-residency class |
| `MOA_PROVIDERS_STREAM_TIMEOUTS_FIRST_BYTE_MS` | `providers.stream_timeouts.first_byte_ms` | 30000 | Maximum wait for the first server-sent event, in milliseconds |
| `MOA_PROVIDERS_STREAM_TIMEOUTS_IDLE_MS` | `providers.stream_timeouts.idle_ms` | 60000 | Maximum idle gap between server-sent events, in milliseconds |
| `MOA_PROVIDERS_STREAM_TIMEOUTS_TOTAL_MS` | `providers.stream_timeouts.total_ms` | 300000 | Maximum wall-clock duration of the complete stream, in milliseconds |
| `MOA_ZEROENTROPY_API_KEY` | `providers.zeroentropy.api_key` | _empty_ | API key value loaded from runtime configuration **(secret)** |
| `MOA_ZEROENTROPY_MAX_CONCURRENT_REQUESTS` | `providers.zeroentropy.max_concurrent_requests` | _unset_ | In-flight concurrency ceiling for this provider account — the natural place to express the credential's tier |
| `MOA_ZEROENTROPY_MAX_INPUTS_PER_MIN` | `providers.zeroentropy.max_inputs_per_min` | _unset_ | Optional per-minute input-rate cap; `None` keeps the provider default |
| `MOA_ZEROENTROPY_MAX_REQUESTS_PER_MIN` | `providers.zeroentropy.max_requests_per_min` | _unset_ | Optional per-minute request-rate cap; `None` keeps the provider default |

### `database`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_DATABASE_ADMIN_URL` | `database.admin_url` | _none_ | Optional direct/admin database URL for migrations and other session-sensitive flows |
| `MOA_DATABASE_BACKGROUND_MAX_CONNECTIONS` | `database.background_max_connections` | 2 | Maximum pool size reserved for process-owned background workers, separate from the foreground Restate handler pool |
| `MOA_DATABASE_CONNECT_TIMEOUT_SECONDS` | `database.connect_timeout_seconds` | 10 | Pool acquire timeout, in seconds |
| `MOA_DATABASE_MAX_CONNECTIONS` | `database.max_connections` | 20 | Maximum pool size for the shared Postgres client |
| `MOA_DATABASE_NEON_API_KEY` | `database.neon.api_key` | _empty_ | Neon API key value loaded from runtime configuration **(secret)** |
| `MOA_DATABASE_NEON_CHECKPOINT_TTL_HOURS` | `database.neon.checkpoint_ttl_hours` | 24 | TTL for automatic checkpoint cleanup, in hours |
| `MOA_DATABASE_NEON_ENABLED` | `database.neon.enabled` | false | Whether Neon checkpoint management is enabled |
| `MOA_DATABASE_NEON_MAX_CHECKPOINTS` | `database.neon.max_checkpoints` | 5 | Maximum number of active MOA checkpoint branches |
| `MOA_DATABASE_NEON_PARENT_BRANCH_ID` | `database.neon.parent_branch_id` | main | Parent branch name or id used for checkpoint creation |
| `MOA_DATABASE_NEON_POOLED` | `database.neon.pooled` | true | Whether pooled connection URIs should be requested for checkpoint branches |
| `MOA_DATABASE_NEON_PROJECT_ID` | `database.neon.project_id` | _empty_ | Neon project identifier used for branch management |
| `MOA_DATABASE_NEON_SUSPEND_TIMEOUT_SECONDS` | `database.neon.suspend_timeout_seconds` | 300 | Auto-suspend timeout in seconds for checkpoint endpoints |
| `MOA_DATABASE_SCHEMA` | `database.schema` | _none_ | Optional already-provisioned Postgres schema to bind runtime queries; setting it disables automatic migrations |
| `MOA_DATABASE_URL` | `database.url` | postgres://moa_owner:dev@localhost:10040/moa | Runtime Postgres connection URL |

### `memory`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_MEMORY_DIGEST_ENABLED` | `memory.digest.enabled` | false | Whether the brain context pipeline injects stored digest rows |
| `MOA_MEMORY_DIGEST_MAX_TOKENS` | `memory.digest.max_tokens` | 600 | Maximum rendered digest size using the rough chars/4 token estimate |
| `MOA_MEMORY_DIGEST_REBUILD_MIN_INTERVAL_HOURS` | `memory.digest.rebuild_min_interval_hours` | 6 | Minimum interval between digest row rebuilds during consolidation |
| `MOA_MEMORY_EMBEDDING_MODEL` | `memory.embedding_model` | openai:text-embedding-3-small | Embedding model selector used for graph memory embedding backfills and queries |
| `MOA_MEMORY_EXTRACTION_ENABLED` | `memory.extraction.enabled` | false | Whether model-backed fact extraction is enabled |
| `MOA_MEMORY_EXTRACTION_MAX_FACTS_PER_CHUNK` | `memory.extraction.max_facts_per_chunk` | 12 | Maximum facts accepted from one chunk |
| `MOA_MEMORY_EXTRACTION_MODEL` | `memory.extraction.model` | gpt-5.4-mini | Provider model selector used for extraction and memory-ingest judging |
| `MOA_MEMORY_EXTRACTION_TIMEOUT_MS` | `memory.extraction.timeout_ms` | 10000 | Provider request timeout in milliseconds |
| `MOA_MEMORY_RETRIEVAL_LINEAGE_ENABLED` | `memory.retrieval.lineage_enabled` | true | Whether retrieval writes narrow quality-scoring lineage rows |
| `MOA_MEMORY_RETRIEVAL_LINEAGE_SAMPLE_RATE` | `memory.retrieval.lineage_sample_rate` | 1.0 | Fraction of turns that write lineage rows when lineage is enabled |
| `MOA_MEMORY_RETRIEVAL_RERANKER_LATENCY` | `memory.retrieval.reranker_latency` | _none_ | Optional provider-specific reranker latency mode |
| `MOA_MEMORY_RETRIEVAL_RERANKER_MODEL` | `memory.retrieval.reranker_model` | noop | Reranker model selector |
| `MOA_MEMORY_VECTOR_EMBEDDER_NAME` | `memory.vector.embedder.name` | gemini:gemini-embedding-2 | Embedder model name |
| `MOA_MEMORY_VECTOR_EMBEDDER_OUTPUT_DIM` | `memory.vector.embedder.output_dim` | 1024 | Requested output dimensionality |
| `MOA_PII_SERVICE_URL` | `memory.pii_service_url` | _none_ | Optional HTTP base URL for the PII classification sidecar |
| `MOA_TURBOPUFFER_API_KEY` | `memory.vector.turbopuffer.api_key` | _empty_ | Turbopuffer API key value loaded from runtime configuration **(secret)** |
| `MOA_TURBOPUFFER_BAA` | `memory.vector.turbopuffer.baa_enabled` | false | Whether the configured Turbopuffer account has a BAA for restricted data |
| `MOA_TURBOPUFFER_BASE_URL` | `memory.vector.turbopuffer.base_url` | _none_ | Optional Turbopuffer API base URL override |
| `MOA_TURBOPUFFER_ENVIRONMENT` | `memory.vector.turbopuffer.environment` | _none_ | Optional namespace environment segment |
| `MOA_TURBOPUFFER_VECTOR_TYPE` | `memory.vector.turbopuffer.vector_type` | f16 | Vector element type used for the Turbopuffer projection namespace |

### `knowledge`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_KNOWLEDGE_EXTERNAL_PARSER_DEFAULT` | `knowledge.parser.external_default` | llamaparse | Default parser for external synced records and files |
| `MOA_KNOWLEDGE_PARSERS_ENABLED` | `knowledge.parsers.enabled` | ["native","llamaparse","unstructured","reducto"] | Parser identifiers allowed for request-time or sync-run selection |
| `MOA_KNOWLEDGE_PARSER_DEFAULT` | `knowledge.parser.default` | native | Default parser for local or already-normalized content |
| `MOA_KNOWLEDGE_PROVIDERS_ENABLED` | `knowledge.providers.enabled` | ["nango","merge"] | Provider identifiers allowed for link and sync runs |
| `MOA_LLAMAPARSE_API_KEY` | `knowledge.llamaparse.api_key` | _empty_ | LlamaParse API key loaded from runtime configuration **(secret)** |
| `MOA_LLAMAPARSE_API_URL` | `knowledge.llamaparse.api_base_url` | https://api.cloud.llamaindex.ai | LlamaParse API base URL |
| `MOA_LLAMAPARSE_TIER` | `knowledge.llamaparse.tier` | agentic | LlamaParse plan or routing tier |
| `MOA_LLAMAPARSE_WEBHOOK_HEADER_NAME` | `knowledge.llamaparse.webhook_header_name` | _unset_ | Optional custom header name required on LlamaParse webhooks |
| `MOA_LLAMAPARSE_WEBHOOK_HEADER_VALUE` | `knowledge.llamaparse.webhook_header_value` | _unset_ | Optional custom header value required on LlamaParse webhooks |
| `MOA_LLAMAPARSE_WEBHOOK_SIGNING_KEY` | `knowledge.llamaparse.webhook_signing_key` | _empty_ | LlamaParse webhook signing key loaded from runtime configuration **(secret)** |
| `MOA_MERGE_API_BASE_URL` | `knowledge.merge.api_base_url` | https://api.merge.dev | Merge API base URL |
| `MOA_MERGE_API_KEY` | `knowledge.merge.api_key` | _empty_ | Merge API key loaded from runtime configuration **(secret)** |
| `MOA_MERGE_WEBHOOK_SIGNATURE_KEY` | `knowledge.merge.webhook_signature_key` | _empty_ | Merge webhook signature key loaded from runtime configuration **(secret)** |
| `MOA_NANGO_API_BASE_URL` | `knowledge.nango.api_base_url` | https://api.nango.dev | Nango API base URL |
| `MOA_NANGO_API_KEY` | `knowledge.nango.api_key` | _empty_ | Nango API key loaded from runtime configuration **(secret)** |
| `MOA_NANGO_WEBHOOK_SIGNING_KEY` | `knowledge.nango.webhook_signing_key` | _empty_ | Nango webhook signing key loaded from runtime configuration **(secret)** |
| `MOA_REDUCTO_API_KEY` | `knowledge.reducto.api_key` | _empty_ | Reducto API key loaded from runtime configuration **(secret)** |
| `MOA_REDUCTO_API_URL` | `knowledge.reducto.api_base_url` | https://platform.reducto.ai | Reducto API base URL |
| `MOA_REDUCTO_ASYNC_ENABLED` | `knowledge.reducto.async_enabled` | true | Whether Reducto asynchronous parsing is enabled |
| `MOA_REDUCTO_CHUNK_MODE` | `knowledge.reducto.chunk_mode` | variable | Reducto chunk mode |
| `MOA_REDUCTO_PARSE_MODE` | `knowledge.reducto.parse_mode` | standard | Reducto parse mode |
| `MOA_REDUCTO_WEBHOOK_HEADER_NAME` | `knowledge.reducto.webhook_header_name` | _unset_ | Optional custom header name required on Reducto webhooks |
| `MOA_REDUCTO_WEBHOOK_HEADER_VALUE` | `knowledge.reducto.webhook_header_value` | _unset_ | Optional custom header value required on Reducto webhooks |
| `MOA_REDUCTO_WEBHOOK_SIGNING_KEY` | `knowledge.reducto.webhook_signing_key` | _empty_ | Reducto webhook signing key loaded from runtime configuration **(secret)** |
| `MOA_UNSTRUCTURED_API_KEY` | `knowledge.unstructured.api_key` | _empty_ | Unstructured API key loaded from runtime configuration **(secret)** |
| `MOA_UNSTRUCTURED_API_URL` | `knowledge.unstructured.api_base_url` | https://api.unstructuredapp.io | Unstructured API base URL |
| `MOA_UNSTRUCTURED_CHUNKING_STRATEGY` | `knowledge.unstructured.chunking_strategy` | by_title | Unstructured chunking strategy |
| `MOA_UNSTRUCTURED_STRATEGY` | `knowledge.unstructured.strategy` | auto | Unstructured partition strategy |

### `session`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_SESSION_ATTACHMENT_ACCESS_KEY_ID` | `session.attachments.access_key_id` | _none_ | Optional explicit S3 access key |
| `MOA_SESSION_ATTACHMENT_ALLOW_HTTP` | `session.attachments.allow_http` | false | Allows HTTP endpoints for local S3-compatible development |
| `MOA_SESSION_ATTACHMENT_BACKEND` | `session.attachments.backend` | s3 | Object store backend |
| `MOA_SESSION_ATTACHMENT_BUCKET` | `session.attachments.bucket` | moa-session-attachments | Bucket that stores attachment objects |
| `MOA_SESSION_ATTACHMENT_ENDPOINT` | `session.attachments.endpoint` | _none_ | Optional S3-compatible endpoint |
| `MOA_SESSION_ATTACHMENT_GCP_APPLICATION_CREDENTIALS_PATH` | `session.attachments.gcp_application_credentials_path` | _none_ | Optional GCS application credentials file path |
| `MOA_SESSION_ATTACHMENT_GCP_SERVICE_ACCOUNT_KEY` | `session.attachments.gcp_service_account_key` | _none_ | Optional inline GCS service account JSON **(secret)** |
| `MOA_SESSION_ATTACHMENT_GCP_SERVICE_ACCOUNT_PATH` | `session.attachments.gcp_service_account_path` | _none_ | Optional GCS service account file path |
| `MOA_SESSION_ATTACHMENT_PREFIX` | `session.attachments.prefix` | session-attachments | Prefix used for all MOA attachment objects in the bucket |
| `MOA_SESSION_ATTACHMENT_REGION` | `session.attachments.region` | us-east-1 | AWS/S3-compatible region |
| `MOA_SESSION_ATTACHMENT_SECRET_ACCESS_KEY` | `session.attachments.secret_access_key` | _none_ | Optional explicit S3 secret key **(secret)** |
| `MOA_SESSION_ATTACHMENT_VIRTUAL_HOSTED_STYLE` | `session.attachments.virtual_hosted_style` | false | Uses virtual-hosted-style S3 requests when true |
| `MOA_SESSION_BLOB_BACKEND` | `session.blob_backend` | postgres | Backend used for claim-check blob payloads |
| `MOA_SESSION_BLOB_DIR` | `session.blob_dir` | _none_ | Root directory for local blob storage |
| `MOA_SESSION_BLOB_THRESHOLD_BYTES` | `session.blob_threshold_bytes` | 65536 | Offload threshold in bytes for large event payload strings |

### `session_limits`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_SESSION_LIMITS_LOOP_DETECTION_THRESHOLD` | `session_limits.loop_detection_threshold` | 3 | Number of identical consecutive turn fingerprints that triggers a loop pause |
| `MOA_SESSION_LIMITS_MAX_MODEL_TURNS_DELEGATION` | `session_limits.max_model_turns_delegation` | 12 | Maximum model loop iterations once a standard turn has delegated to at least one worker; replaces the base cap for the rest of that turn |
| `MOA_SESSION_LIMITS_MAX_TOOL_CALLS` | `session_limits.max_tool_calls` | 30 | Maximum tool calls allowed within one turn |
| `MOA_SESSION_LIMITS_MAX_TURNS` | `session_limits.max_turns` | 50 | Maximum completed turns per session before pausing |
| `MOA_SESSION_LIMITS_PROGRESS_FIRST_DELAY_MS` | `session_limits.progress_first_delay_ms` | 8000 | Delay before the first durable progress update is eligible, in milliseconds |
| `MOA_SESSION_LIMITS_PROGRESS_INTERVAL_MS` | `session_limits.progress_interval_ms` | 8000 | Minimum interval between durable progress updates, in milliseconds |
| `MOA_SESSION_LIMITS_PROGRESS_NARRATION_ENABLED` | `session_limits.progress_narration_enabled` | true | Whether default-on natural-language progress narration is enabled |
| `MOA_SESSION_LIMITS_PROGRESS_NARRATION_INTERVAL_MS` | `session_limits.progress_narration_interval_ms` | 20000 | Minimum interval between progress narrations, in milliseconds |
| `MOA_SESSION_LIMITS_PROGRESS_NARRATION_MAX_PER_WINDOW` | `session_limits.progress_narration_max_per_window` | 30 | Maximum number of narrations per rolling window before the narrator backs off |
| `MOA_SESSION_LIMITS_PROGRESS_NARRATION_MAX_TOKENS` | `session_limits.progress_narration_max_tokens` | 120 | Maximum output tokens for one progress-narration completion |
| `MOA_SESSION_LIMITS_PROGRESS_NARRATION_MODEL` | `session_limits.progress_narration_model` | _none_ | Optional model id override for progress narration |
| `MOA_SESSION_LIMITS_SIMPLE_MAX_TURNS` | `session_limits.simple_max_turns` | 1 | Maximum model loop iterations for requests classified as simple |
| `MOA_SESSION_LIMITS_STANDARD_MAX_TURNS` | `session_limits.standard_max_turns` | 6 | Maximum model loop iterations for requests classified as standard |
| `MOA_SESSION_LIMITS_WORKER_CLEANUP_GRACE_MS` | `session_limits.worker_cleanup_grace_ms` | 60000 | Grace window before a terminal worker self-cleans (removes itself from the parent fan-out and clears its VO state) after reporting its result |
| `MOA_SESSION_LIMITS_WORKER_HEARTBEAT_INTERVAL_MS` | `session_limits.worker_heartbeat_interval_ms` | 15000 | Target cadence, in milliseconds, at which an active child refreshes its telemetry-plane heartbeat while running |
| `MOA_SESSION_LIMITS_WORKER_HEARTBEAT_STALE_MS` | `session_limits.worker_heartbeat_stale_ms` | 60000 | Age, in milliseconds, beyond which an active child's last heartbeat is treated as stale by the per-child liveness watchdog |
| `MOA_SESSION_LIMITS_WORKER_INPUT_TIMEOUT_MS` | `session_limits.worker_input_timeout_ms` | 1800000 | Maximum time a child `request_input` round-trip blocks on its awakeable before returning a "no input received" result so the child can proceed or abort |
| `MOA_SESSION_LIMITS_WORKER_RESUME_MAX_PER_WINDOW` | `session_limits.worker_resume_max_per_window` | 6 | Maximum guarded coordinator auto-resumes dispatched per rolling window before the resume path backs off |
| `MOA_SESSION_LIMITS_WORKER_RESUME_WINDOW_MS` | `session_limits.worker_resume_window_ms` | 600000 | Rolling-window length, in milliseconds, for the guarded parent-resume budget |

### `compaction`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_COMPACTION_ENABLED` | `compaction.enabled` | true | Whether reversible history compaction is enabled |
| `MOA_COMPACTION_EVENT_THRESHOLD` | `compaction.event_threshold` | 100 | Emit a checkpoint after this many unsummarized events |
| `MOA_COMPACTION_PRESERVE_ERRORS` | `compaction.preserve_errors` | true | Whether old error events must stay verbatim in the compiled view |
| `MOA_COMPACTION_RECENT_TURNS_VERBATIM` | `compaction.recent_turns_verbatim` | 5 | Number of most recent user turns to keep verbatim in context |
| `MOA_COMPACTION_TOKEN_RATIO_THRESHOLD` | `compaction.token_ratio_threshold` | 0.7 | Emit a checkpoint after unsummarized history reaches this fraction of the token budget |

### `context_snapshot`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_CONTEXT_SNAPSHOT_ENABLED` | `context_snapshot.enabled` | true | Whether compiled context snapshots are enabled |
| `MOA_CONTEXT_SNAPSHOT_MAX_SIZE_BYTES` | `context_snapshot.max_size_bytes` | 5000000 | Warn when a serialized snapshot exceeds this size |

### `orchestrator`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_ORCHESTRATOR_ENDPOINT` | `orchestrator.endpoint` | http://localhost:10010 | Restate ingress URL fronting the `moa-orchestrator` deployment |
| `MOA_RESTATE_ADMIN_URL` | `orchestrator.restate_admin_url` | http://localhost:10011 | Restate admin API base URL used for deployment registration and probes |
| `MOA_RESTATE_INGRESS_URL` | `orchestrator.restate_ingress_url` | http://localhost:10010 | Restate ingress URL used by hosted runtime clients and tests |
| `MOA_RESTATE_LLM_GATEWAY_URL` | `orchestrator.llm_gateway_url` | _none_ | Optional LLM gateway URL for direct service calls |

### `runtime_cache`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_RUNTIME_CACHE_BACKEND` | `runtime_cache.backend` | auto | Backend used for runtime cache operations |
| `MOA_RUNTIME_CACHE_REDIS_URL` | `runtime_cache.redis_url` | _none_ | Redis URL used when the Redis backend is selected |

### `auth`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_AUTH_AUTH0_AUDIENCE` | `auth.auth0.audience` | _empty_ | Expected API audience |
| `MOA_AUTH_AUTH0_CLIENT_ID` | `auth.auth0.client_id` | _empty_ | Auth0 client id loaded from runtime configuration |
| `MOA_AUTH_AUTH0_CLIENT_SECRET` | `auth.auth0.client_secret` | _empty_ | Auth0 client secret loaded from runtime configuration **(secret)** |
| `MOA_AUTH_AUTH0_DOMAIN` | `auth.auth0.domain` | _empty_ | Auth0 tenant domain |
| `MOA_AUTH_AUTH0_WEBHOOK_SECRET` | `auth.auth0_webhook_secret` | _none_ | Shared secret used to verify Auth0 connection-linked webhooks **(secret)** |
| `MOA_AUTH_CONTACT_TOKENS_AUDIENCE` | `auth.contact_tokens.audience` | moa-agent-contact | Expected audience for contact JWTs |
| `MOA_AUTH_CONTACT_TOKENS_CONTACT_POINT_HASH_KEY_HEX` | `auth.contact_tokens.contact_point_hash_key_hex` | _empty_ | 32-byte hex key used for contact point lookup hashes **(secret)** |
| `MOA_AUTH_CONTACT_TOKENS_ISSUER` | `auth.contact_tokens.issuer` | https://moa.local/contacts | Expected issuer for contact JWTs |
| `MOA_AUTH_CONTACT_TOKENS_KEY_ID` | `auth.contact_tokens.key_id` | moa-contact-rs256 | JWT key id placed in the token header |
| `MOA_AUTH_CONTACT_TOKENS_PRIVATE_KEY_PEM` | `auth.contact_tokens.private_key_pem` | _empty_ | RSA private key PEM for issuance **(secret)** |
| `MOA_AUTH_CONTACT_TOKENS_PUBLIC_KEY_PEM` | `auth.contact_tokens.public_key_pem` | _empty_ | RSA public key PEM for verification |
| `MOA_AUTH_CONTACT_TOKENS_UNVERIFIED_TTL_SECONDS` | `auth.contact_tokens.unverified_ttl_seconds` | 3600 | TTL for unverified contact tokens |
| `MOA_AUTH_CONTACT_TOKENS_VERIFICATION_TTL_SECONDS` | `auth.contact_tokens.verification_ttl_seconds` | 600 | TTL for one-time verification challenges |
| `MOA_AUTH_CONTACT_TOKENS_VERIFIED_TTL_SECONDS` | `auth.contact_tokens.verified_ttl_seconds` | 7200 | TTL for verified contact tokens, in seconds |
| `MOA_AUTH_OAUTH_ACCESS_TOKEN_TTL_SECONDS` | `auth.oauth.access_token_ttl_seconds` | 3600 | Lifetime of an issued access token, in seconds |
| `MOA_AUTH_OAUTH_AUTHORIZATION_CODE_TTL_SECONDS` | `auth.oauth.authorization_code_ttl_seconds` | 60 | Lifetime of a single-use authorization code, in seconds |
| `MOA_AUTH_OAUTH_AUTHORIZATION_REQUEST_TTL_SECONDS` | `auth.oauth.authorization_request_ttl_seconds` | 300 | Lifetime of an unapproved authorization transaction, in seconds |
| `MOA_AUTH_OAUTH_CLIENTS_JSON` | `auth.oauth.clients` | [] | JSON array of statically registered OAuth clients, validated and converged into Postgres at startup |
| `MOA_AUTH_OAUTH_ISSUER` | `auth.oauth.issuer` | https://moa.local | Canonical authorization-server issuer URL |
| `MOA_AUTH_OAUTH_REFRESH_TOKEN_TTL_SECONDS` | `auth.oauth.refresh_token_ttl_seconds` | 1209600 | Lifetime of an issued refresh token, in seconds |
| `MOA_AUTH_OAUTH_RESOURCE` | `auth.oauth.resource` | https://moa.local/mcp | Exact RFC 8707 protected resource accepted by this server |
| `MOA_AUTH_OIDC_AUDIENCE` | `auth.oidc.audience` | _empty_ | Expected token audience |
| `MOA_AUTH_OIDC_ISSUER` | `auth.oidc.issuer` | _empty_ | OIDC issuer URL |
| `MOA_AUTH_OIDC_JWKS_URL` | `auth.oidc.jwks_url` | _empty_ | JWKS endpoint URL |
| `MOA_AUTH_PROVIDER` | `auth.provider` | local | Selected authentication provider |

### `authz`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_AUTHZ_ENGINE` | `authz.engine` | openfga | Authorization engine selection |
| `MOA_AUTHZ_OPENFGA_MODEL_ID` | `authz.openfga.model_id` | _empty_ | OpenFGA authorization model ID |
| `MOA_AUTHZ_OPENFGA_PRESHARED_KEY` | `authz.openfga.preshared_key` | _empty_ | Preshared key configured in OpenFGA **(secret)** |
| `MOA_AUTHZ_OPENFGA_STORE_ID` | `authz.openfga.store_id` | _empty_ | OpenFGA store ID |
| `MOA_AUTHZ_OPENFGA_TIMEOUT_MS` | `authz.openfga.timeout_ms` | 2000 | Per-request HTTP timeout in milliseconds |
| `MOA_AUTHZ_OPENFGA_URL` | `authz.openfga.url` | _empty_ | OpenFGA HTTP API base URL |

### `async_authz`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_ASYNC_AUTHZ_DEFAULT_TIMEOUT_SECS` | `async_authz.default_timeout_secs` | 900 | Default approval timeout in seconds |
| `MOA_ASYNC_AUTHZ_PROVIDER` | `async_authz.provider` | builtin | Selected async authorization provider |

### `token_vault`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_TOKEN_VAULT_PROVIDER` | `token_vault.provider` | none | Selected token vault provider |
| `MOA_TOKEN_VAULT_REFRESH_JSON` | `token_vault.refresh` | {} | JSON map of outbound OAuth refresh settings for the self-hosted Postgres vault, keyed by connection name |

### `kms`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_KMS_ALLOW_EPHEMERAL` | `kms.allow_ephemeral` | false | Development/test opt-in permitting a non-durable (ephemeral) provider to back envelope encryption; `false` fails closed at boot |
| `MOA_KMS_PROVIDER` | `kms.provider` | local | Selected key-management provider (`local` for dev/tests, `postgres` for persistent deployments) |
| `MOA_KMS_REQUIRED_GENERATION` | `kms.required_generation` | primary | Generation this pod requires the database to have active before it is compatible and ready |
| `MOA_KMS_ROOT_KEY_DIR` | `kms.root_key_dir` | /var/run/secrets/moa-kms/root-keys | Directory containing base64 root-key files named by generation |

### `compliance`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_LINEAGE_AUDIT_ROOT_SEED_HEX` | `compliance.lineage_audit_root_seed_hex` | _none_ | Optional 32-byte deployment root seed (hex or base64) deriving per-tenant audit-root signing keys |
| `MOA_LINEAGE_AUDIT_SIGNING_KEY_HEX` | `compliance.lineage_audit_signing_key_hex` | _none_ | Private key material used to verify lineage audit roots **(secret)** |
| `MOA_LINEAGE_AUDIT_SIGNING_KEY_ID` | `compliance.lineage_audit_signing_key_id` | moa-lineage-audit-ops | Stable key identifier used for lineage audit-root signatures |
| `MOA_PII_VAULT_SECRET_HEX` | `compliance.pii_vault_secret_hex` | _none_ | Optional secret used to compute PII-vault subject pseudonyms **(secret)** |
| `MOA_PRIVACY_APPROVAL_PUBLIC_KEY_HEX` | `compliance.privacy_approval_public_key_hex` | _none_ | Public key material used to verify signed privacy approval tokens **(secret)** |
| `MOA_PRIVACY_EXPORT_SIGNING_KEY_HEX` | `compliance.privacy_export_signing_key_hex` | _none_ | Private key material used to sign privacy export and lineage DSAR manifests **(secret)** |
| `MOA_PRIVACY_EXPORT_SIGNING_KEY_ID` | `compliance.privacy_export_signing_key_id` | moa-privacy-export-ops | Stable key identifier recorded on privacy export manifests |
| `MOA_REQUIRE_DUAL_CONTROL_FOR_ERASURE` | `compliance.require_dual_control_for_erasure` | false | When true, privacy erasure requires a four-eyes dual-control approval by a second, distinct tenant admin before it may execute |

### `llm_dlp`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_LLM_DLP_TOKENIZE_ENABLED` | `llm_dlp.tokenize_enabled` | false | Tokenize restricted spans in outbound requests before they reach a provider, and detokenize the provider's response inside the trust boundary |

### `audit_security`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_AUDIT_SECURITY_EMIT_AUTHZ_ALLOWS` | `audit_security.emit_authz_allows` | true | Emit allowed authorization decisions in addition to denied decisions |

### `observability`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_OBSERVABILITY_ENABLED` | `observability.enabled` | false | Whether OTLP export is enabled |
| `MOA_OBSERVABILITY_ENVIRONMENT` | `observability.environment` | _none_ | Deployment environment resource attribute |
| `MOA_OBSERVABILITY_LINEAGE_BATCH_MAX_AGE_SECS` | `observability.lineage.batch_max_age_secs` | 2 | Maximum age for a partial worker batch |
| `MOA_OBSERVABILITY_LINEAGE_BATCH_SIZE` | `observability.lineage.batch_size` | 512 | Maximum rows written per worker flush |
| `MOA_OBSERVABILITY_LINEAGE_CHANNEL_CAPACITY` | `observability.lineage.channel_capacity` | 8192 | Bounded hot-path channel capacity |
| `MOA_OBSERVABILITY_LINEAGE_ENABLED` | `observability.lineage.enabled` | false | Whether durable lineage capture is enabled |
| `MOA_OBSERVABILITY_LINEAGE_JOURNAL_PATH` | `observability.lineage.journal_path` | ~/.moa/lineage-journal | Durable fjall journal path |
| `MOA_OBSERVABILITY_LINEAGE_SAMPLE_PGVECTOR_EXPLAIN` | `observability.lineage.sample_pgvector_explain` | 0.01 | Fraction of pgvector queries that run full EXPLAIN ANALYZE |
| `MOA_OBSERVABILITY_OTLP_ENDPOINT` | `observability.otlp_endpoint` | _none_ | Optional OTLP endpoint override |
| `MOA_OBSERVABILITY_OTLP_HEADERS` | `observability.otlp_headers` | {} | Additional OTLP headers for exporter auth and routing |
| `MOA_OBSERVABILITY_OTLP_PROTOCOL` | `observability.otlp_protocol` | grpc | OTLP transport protocol |
| `MOA_OBSERVABILITY_RELEASE` | `observability.release` | _none_ | Application release or version resource attribute |
| `MOA_OBSERVABILITY_SAMPLE_RATE` | `observability.sample_rate` | 0.01 | Trace sampling ratio from 0.0 to 1.0 |
| `MOA_OBSERVABILITY_SERVICE_NAME` | `observability.service_name` | moa | Logical service name for traces |

### `metrics`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_METRICS_ENABLED` | `metrics.enabled` | false | Whether the Prometheus scrape endpoint should be exposed |
| `MOA_METRICS_LISTEN` | `metrics.listen` | 0.0.0.0:9090 | Listener address for the Prometheus scrape endpoint |

### `messaging`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_MESSAGING_EMAIL_FROM` | `messaging.email_from` | _empty_ | Default sender address for outbound email |
| `MOA_MESSAGING_EMAIL_REPLY_TO` | `messaging.email_reply_to` | _none_ | Optional reply-to address for outbound email |
| `MOA_MESSAGING_POSTMARK_BASE_URL` | `messaging.postmark_base_url` | https://api.postmarkapp.com | Base URL for the Postmark email API |
| `MOA_MESSAGING_POSTMARK_MESSAGE_STREAM` | `messaging.postmark_message_stream` | outbound | Default Postmark message stream |
| `MOA_MESSAGING_SLACK_APP_TOKEN` | `messaging.slack_app_token` | _empty_ | Slack app token loaded from runtime configuration **(secret)** |
| `MOA_MESSAGING_SLACK_TOKEN` | `messaging.slack_token` | _empty_ | Slack bot token loaded from runtime configuration |
| `MOA_MESSAGING_TWILIO_BASE_URL` | `messaging.twilio_base_url` | https://api.twilio.com | Base URL for Twilio's REST API |

### `cloud`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_CLOUD_HANDS_ALLOW_LOCAL` | `cloud.hands.allow_local_provider` | false | Development-only opt-in that permits routing hand tools to the local host provider |
| `MOA_CLOUD_HANDS_DAYTONA_API_KEY` | `cloud.hands.daytona_api_key` | _none_ | Daytona API key loaded from runtime configuration **(secret)** |
| `MOA_CLOUD_HANDS_DAYTONA_API_URL` | `cloud.hands.daytona_api_url` | _none_ | Optional Daytona API base URL override |
| `MOA_CLOUD_HANDS_DAYTONA_DEFAULT_IMAGE` | `cloud.hands.daytona_default_image` | _none_ | Optional default image for Daytona sandboxes |
| `MOA_CLOUD_HANDS_DEFAULT_PROVIDER` | `cloud.hands.default_provider` | _none_ | Default hand provider |
| `MOA_CLOUD_HANDS_E2B_API_KEY` | `cloud.hands.e2b_api_key` | _none_ | E2B API key loaded from runtime configuration **(secret)** |
| `MOA_CLOUD_HANDS_E2B_API_URL` | `cloud.hands.e2b_api_url` | _none_ | Optional E2B API base URL override |
| `MOA_CLOUD_HANDS_E2B_DOMAIN` | `cloud.hands.e2b_domain` | _none_ | Optional E2B domain override |
| `MOA_CLOUD_HANDS_E2B_TEMPLATE` | `cloud.hands.e2b_template` | _none_ | Optional default E2B template identifier |
| `MOA_CLOUD_HANDS_FALLBACK_PROVIDERS` | `cloud.hands.fallback_providers` | [] | Ordered fallback cloud providers attempted when the selected cloud hand is unavailable |
| `MOA_CLOUD_MEMORY_DIR` | `cloud.memory_dir` | _none_ | Optional alternate memory root for cloud deployments |

### `permissions`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_PERMISSIONS_ADMIN_REVIEW` | `permissions.admin_review` | [] | Tools that require tenant-admin review |
| `MOA_PERMISSIONS_ALWAYS_DENY` | `permissions.always_deny` | [] | Tools always denied |
| `MOA_PERMISSIONS_DEFAULT_EFFECT` | `permissions.default_effect` | allow | Default effect when neither persisted rules nor tool-specific config match |

### `tool_output`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_TOOL_OUTPUT_HEAD_RATIO` | `tool_output.head_ratio` | 0.4 | Fraction of the truncation budget allocated to the head of the output |
| `MOA_TOOL_OUTPUT_MAX_BASH_LINES` | `tool_output.max_bash_lines` | 200 | Maximum preserved lines for bash output before head+tail truncation |
| `MOA_TOOL_OUTPUT_MAX_REPLAY_CHARS` | `tool_output.max_replay_chars` | 20000 | Maximum characters for replayed tool output |

### `tool_budgets`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_TOOL_BUDGETS_BASH_STDERR` | `tool_budgets.bash_stderr` | 2000 | Approximate token budget for successful `bash` stderr |
| `MOA_TOOL_BUDGETS_BASH_STDOUT` | `tool_budgets.bash_stdout` | 4000 | Approximate token budget for successful `bash` stdout |
| `MOA_TOOL_BUDGETS_DEFAULT` | `tool_budgets.default` | 4000 | Approximate token budget for tools without a dedicated override, including MCP tools |
| `MOA_TOOL_BUDGETS_FILE_OUTLINE` | `tool_budgets.file_outline` | 2000 | Approximate token budget for `file_outline` |
| `MOA_TOOL_BUDGETS_FILE_READ` | `tool_budgets.file_read` | 8000 | Approximate token budget for `file_read` |
| `MOA_TOOL_BUDGETS_FILE_SEARCH` | `tool_budgets.file_search` | 4000 | Approximate token budget for `file_search` |
| `MOA_TOOL_BUDGETS_GREP` | `tool_budgets.grep` | 4000 | Approximate token budget for `grep` |
| `MOA_TOOL_BUDGETS_MEMORY_SEARCH` | `tool_budgets.memory_search` | 3000 | Approximate token budget for `memory_search` |

### `skill_budget`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_SKILL_BUDGET_MAX_MANIFEST_CHARS` | `skill_budget.max_manifest_chars` | _none_ | Maximum characters for the entire skill manifest |
| `MOA_SKILL_BUDGET_MAX_PER_SKILL_CHARS` | `skill_budget.max_per_skill_chars` | 1536 | Maximum characters for one individual skill entry before truncation |
| `MOA_SKILL_BUDGET_SHOW_TOKEN_ESTIMATES` | `skill_budget.show_token_estimates` | true | Whether manifest entries should include estimated token counts |

### `query_rewrite`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_QUERY_REWRITE_CIRCUIT_BREAKER_COOLDOWN_SECS` | `query_rewrite.circuit_breaker_cooldown_secs` | 60 | Circuit-breaker cooldown length in seconds after tripping |
| `MOA_QUERY_REWRITE_CIRCUIT_BREAKER_THRESHOLD` | `query_rewrite.circuit_breaker_threshold` | 0.05 | Circuit-breaker error-rate threshold that disables rewriting |
| `MOA_QUERY_REWRITE_CIRCUIT_BREAKER_WINDOW_SECS` | `query_rewrite.circuit_breaker_window_secs` | 60 | Circuit-breaker sliding window length in seconds |
| `MOA_QUERY_REWRITE_ENABLED` | `query_rewrite.enabled` | true | Whether query rewriting is enabled |
| `MOA_QUERY_REWRITE_MIN_QUERY_TOKENS` | `query_rewrite.min_query_tokens` | 15 | Minimum token count in a single-turn query to trigger rewriting |
| `MOA_QUERY_REWRITE_MODEL` | `query_rewrite.model` | _none_ | Model to use for rewriting |
| `MOA_QUERY_REWRITE_SKIP_SINGLE_TURN` | `query_rewrite.skip_single_turn` | true | Whether to skip rewriting on single-turn conversations below the token threshold |
| `MOA_QUERY_REWRITE_TIMEOUT_MS` | `query_rewrite.timeout_ms` | 5000 | Hard timeout for the rewriter LLM call |

### `resolution`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_RESOLUTION_ENABLED` | `resolution.enabled` | true | Whether automated segment assessment is enabled |
| `MOA_RESOLUTION_IDLE_TIMEOUT_MINUTES` | `resolution.idle_timeout_minutes` | 30 | Idle timeout used for final continuation assessment |
| `MOA_RESOLUTION_REPHRASE_SIMILARITY_THRESHOLD` | `resolution.rephrase_similarity_threshold` | 0.85 | Similarity threshold above which a later user message is treated as a rephrase |
| `MOA_RESOLUTION_STRUCTURAL_MIN_SAMPLES` | `resolution.structural_min_samples` | 20 | Minimum historical sample count before structural baselines are used |
| `MOA_RESOLUTION_WEIGHTS_CONTINUATION` | `resolution.weights.continuation` | 0.25 | Weight assigned to user continuation behavior |
| `MOA_RESOLUTION_WEIGHTS_SELF_ASSESSMENT` | `resolution.weights.self_assessment` | 0.15 | Weight assigned to agent final-response self-assessment |
| `MOA_RESOLUTION_WEIGHTS_STRUCTURAL` | `resolution.weights.structural` | 0.1 | Weight assigned to structural anomaly detection |
| `MOA_RESOLUTION_WEIGHTS_TOOL` | `resolution.weights.tool` | 0.2 | Weight assigned to tool outcome analysis |
| `MOA_RESOLUTION_WEIGHTS_VERIFICATION` | `resolution.weights.verification` | 0.3 | Weight assigned to verification command detection |

### `learning`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_LEARNING_EMBEDDINGS_EXPERIENCE_BATCH_SIZE` | `learning.embeddings.experience_batch_size` | 128 | Maximum number of `experience_records` embedded per backfill tick |
| `MOA_LEARNING_EMBEDDINGS_EXPERIENCE_LOOKBACK_DAYS` | `learning.embeddings.experience_lookback_days` | 30 | Only `experience_records` created within this many days are eligible for embedding backfill |
| `MOA_LEARNING_EMBEDDINGS_SKILL_BATCH_SIZE` | `learning.embeddings.skill_batch_size` | 64 | Maximum number of published Skill artifacts embedded per backfill tick |
| `MOA_LEARNING_RECURRENCE_CLUSTER_SIMILARITY` | `learning.recurrence.cluster_similarity` | 0.85 | Cosine-similarity threshold at which two exact-fingerprint groups merge into one semantic recurrence cluster |
| `MOA_LEARNING_RECURRENCE_LOOKBACK_DAYS` | `learning.recurrence.lookback_days` | 30 | Lookback window, in days, over which recurring experiences are grouped |
| `MOA_LEARNING_RECURRENCE_MAX_CANDIDATE_GROUPS` | `learning.recurrence.max_candidate_groups` | 200 | Upper bound on exact-fingerprint groups loaded per tenant per tick as clustering candidates |
| `MOA_LEARNING_RECURRENCE_MIN_OCCURRENCES` | `learning.recurrence.min_occurrences` | 3 | Minimum resolved/partial experiences sharing one task fingerprint before recurrence dispatches distillation |
| `MOA_LEARNING_RECURRENCE_REJECTION_COOLDOWN_DAYS` | `learning.recurrence.rejection_cooldown_days` | 30 | Suppression window, in days, after a reviewer rejects a fingerprint's candidate |
| `MOA_LEARNING_RECURRENCE_RELAXED_MIN_TOOL_CALLS` | `learning.recurrence.relaxed_min_tool_calls` | 3 | Relaxed per-session tool-call floor applied to the recurrence exemplar |
| `MOA_LEARNING_SEGMENTS_IDLE_GAP_MINUTES` | `learning.segments.idle_gap_minutes` | 30 | Idle gap, in minutes, that starts a new task segment when no LLM boundary signal is present |
| `MOA_LEARNING_SKILLS_IMPROVE_ROUTE_SIMILARITY` | `learning.skills.improve_route_similarity` | 0.8 | Cosine-similarity floor at which filing-time routing improves the nearest existing skill instead of creating a new one |
| `MOA_LEARNING_SKILLS_MIN_TOOL_CALLS` | `learning.skills.min_tool_calls` | 8 | Minimum tool-call count a segment must contain before it is eligible for skill distillation |
| `MOA_LEARNING_SKILLS_PROPOSAL_DEDUP_SIMILARITY` | `learning.skills.proposal_dedup_similarity` | 0.85 | Cosine-similarity floor at which a new distilled experience is deduped into an open proposal as a sibling |

### `budgets`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_BUDGETS_DAILY_TENANT_CENTS` | `budgets.daily_tenant_cents` | 2000 | Maximum daily spend per tenant in cents |

### `clickhouse`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_CLICKHOUSE_DATABASE` | `clickhouse.database` | moa | Target database; created at startup when missing |
| `MOA_CLICKHOUSE_EXPORT_BATCH_ROWS` | `clickhouse.export_batch_rows` | 5000 | Maximum rows pulled from Postgres and inserted into ClickHouse per analytics-export batch |
| `MOA_CLICKHOUSE_EXPORT_POLL_SECS` | `clickhouse.export_poll_secs` | 15 | Poll interval in seconds for the analytics exporter loop; also sets the cursor rewind overlap (`2 × export_poll_secs`) |
| `MOA_CLICKHOUSE_LINEAGE_TTL_DAYS` | `clickhouse.lineage_ttl_days` | 30 | Row TTL in days for `turn_lineage`, mirroring the Postgres/Timescale 30-day retention drop |
| `MOA_CLICKHOUSE_PASSWORD` | `clickhouse.password` | _none_ | Optional password for HTTP basic auth **(secret)** |
| `MOA_CLICKHOUSE_URL` | `clickhouse.url` | _empty_ | HTTP interface endpoint, for example `http://localhost:8123` |
| `MOA_CLICKHOUSE_USER` | `clickhouse.user` | _none_ | Optional user for HTTP basic auth |

### `local`

| Variable | Config path | Default | Description |
|---|---|---|---|
| `MOA_LOCAL_DOCKER_ENABLED` | `local.docker_enabled` | true | Whether local Docker hands are enabled |
| `MOA_LOCAL_MEMORY_DIR` | `local.memory_dir` | ~/.moa/memory | Memory root directory |
| `MOA_LOCAL_SANDBOX_DIR` | `local.sandbox_dir` | ~/.moa/sandbox | Sandbox working directory |
## Special (non-overlay) variables

These `MOA_*` variables are **not** part of the typed overlay — they are read
directly by test lanes, deploy scripts, or Docker Compose. They are allowlisted
by the startup check (in `crates/moa-config/src/env_overlay/mod.rs`) so they do
not trip the unknown-variable audit. They do not affect application config.

### Approved prefixes

| Prefix | Consumed by |
|---|---|
| `MOA_RUN_*` (incl. `MOA_RUN_LIVE_*`) | Gates for live/docker/chaos tests (e.g. `MOA_RUN_LIVE_COHERE_TESTS`, `MOA_RUN_CHAOS_TESTS`); `#[ignore]` lanes read them |
| `MOA_TEST_*` | Test-only config (e.g. Auth0 test-tenant values) |
| `MOA_FIXTURE_*` | Integration-test fixtures (e.g. fixture OpenFGA endpoint) |
| `MOA_PENTEST_*` | Cross-tenant pentest harness knobs |
| `MOA_LOADTEST_*` | k6/loadtest harness knobs (`crates/moa-loadtest`) |
| `MOA_CLEAN_E2E_*` | `scripts/run-clean-e2e.sh` harness (ports, run id, thread count) |
| `MOA_RUSTFS_*` | Local Docker Compose object-store ports |
| `MOA_FGA_*` | OpenFGA bootstrap scripts |
| `MOA_BOOTSTRAP_*` | Tenant/user bootstrap scripts |
| `MOA_EVAL_*` | Eval harness (`crates/moa-eval`) |
| `MOA_TRACE_*` | Tracing/debug sampling toggles read at runtime |
| `MOA_TWILIO_*` | Twilio live-messaging credentials (live tests) |
| `MOA_DAYTONA_*` | Daytona sandbox credentials (compose/live) |
| `MOA_NEON_*` | Neon branching credentials (deploy/live tests) |
| `MOA_OPENFGA_*` | OpenFGA compose/bootstrap vars (distinct from the `MOA_AUTHZ_OPENFGA_*` overlay) |
| `MOA_POSTMARK_*` | Postmark live-email credentials |
| `MOA_E2B_*` | E2B sandbox credentials |
| `MOA_OPENROUTER_*` | OpenRouter credentials (deploy) |
| `MOA_EDGE_*` | Edge binary bind/upstream and exact MCP allowlists (`MOA_EDGE_BIND`, `MOA_EDGE_UPSTREAM`, `MOA_EDGE_MCP_ALLOWED_HOSTS`, `MOA_EDGE_MCP_ALLOWED_ORIGINS`) |
| `MOA_RESTATE_DEPLOYMENT_*` | Restate deploy-registration (`MOA_RESTATE_DEPLOYMENT_HOST`/`_URI`) |

### Approved exact names

| Variable | Consumed by |
|---|---|
| `MOA_CONFIG_ENV_STRICT` | This check's own strictness switch (warn vs fail) |
| `MOA_SKIP_FGA` | Skips OpenFGA bootstrap in local/dev startup |
| `MOA_DEREGISTER_ON_SHUTDOWN` | Orchestrator: deregister Restate deployment on shutdown |
| `MOA_REQUIRE_RESTATE_REGISTRATION_FOR_READINESS` | Orchestrator readiness gate |
| `MOA_ORCHESTRATOR_BIN` / `MOA_ORCHESTRATOR_FEATURES` | e2e harness: orchestrator binary path / cargo features |
| `MOA_MEMORY_AUTO_BOOTSTRAP` | Auto-run memory schema bootstrap on startup |
| `MOA_MEMORY_EXTRACTION_MODEL` / `_TIMEOUT_MS` / `_MAX_FACTS_PER_CHUNK` | Memory fact-extraction overrides read directly |
| `MOA_AUTHZ_DECISION_CACHE_TTL_MS` | Authz decision-cache TTL, read directly (see [Hardcoded tuning knobs](#hardcoded-tuning-knobs)) |
| `MOA_AUTHZ_OPENFGA_STORE_NAME` | OpenFGA store name for bootstrap |
| `MOA_AUDIT_BUCKET` / `MOA_AUDIT_OBJECT_LOCK_MODE` / `MOA_AUDIT_RETENTION_YEARS` | Audit shipper (object-lock WORM) config |
| `MOA_AUTH_HEADER_TRUST` | Trusted auth-header mode toggle |
| `MOA_AUTH0_CLIENT_ID` | Auth0 client id (compose/test) |
| `MOA_LINEAGE_SINK` | Lineage sink selection |
| `MOA_PERSIST_TURN_METRICS` | Persist per-turn metrics rows |
| `MOA_PROVIDERS_OVERRIDE` | Provider-catalog override (tests/tools) |
| `MOA_SCIM_BASE_URL` | SCIM base URL |
| `MOA_TOXIPROXY_URL` | Toxiproxy control URL (chaos tests) |
| `MOA_TURBOPUFFER_LIVE_NEWS_FACTS` | Live Turbopuffer news-facts eval fixture |
| `MOA_DOCKER_SECCOMP_PROFILE` | Docker seccomp profile path for sandbox runs |
| `MOA_VENDOR_NAME` | Vendor label surfaced in metadata |

## Provider concurrency

Concurrency limits for outbound provider calls live under
`providers.concurrency` plus a per-provider cap:

- `MOA_PROVIDERS_CONCURRENCY_DEFAULT_MAX_IN_FLIGHT` (default `16`) bounds any
  provider that sets no cap of its own; `0` = unbounded.
- `MOA_<PROVIDER>_MAX_CONCURRENT_REQUESTS` (e.g. `MOA_OPENAI_MAX_CONCURRENT_REQUESTS`)
  overrides per provider; unset keeps the provider's built-in default.
- `MOA_PROVIDERS_CONCURRENCY_SCOPE` (`local` | `global`) enforces the ceiling
  per process or shared across replicas; `global` uses
  `MOA_PROVIDERS_CONCURRENCY_LEASE_TTL_MS` as the crash backstop.
- `MOA_PROVIDERS_CONCURRENCY_BLOCK_THRESHOLD_MS` (default `2000`) is how long a
  caller waits for a slot before it is reported saturated.

See the [`providers`](#providers) table for exact defaults.

## Operational notes

### Database connection pool sizing

`database.max_connections` (`MOA_DATABASE_MAX_CONNECTIONS`, default `20`) is the
**whole process's** Postgres budget — sessions, authz, graph memory, lineage,
and ingest all share this one pool. The default is dev-appropriate. Size it in
production to the process's worker fan-out (roughly `50–100`), and keep the sum
across replicas under the Postgres server's `max_connections`.

### Metrics endpoint exposure

`metrics.listen` (`MOA_METRICS_LISTEN`) defaults to `0.0.0.0:9090`. That is
correct for a Kubernetes scrape, but it binds all interfaces — restrict access
with a NetworkPolicy (or bind a private interface) so the metrics port is not
reachable from outside the cluster.

### MCP tool permission posture

Non-builtin tools (MCP servers) now default to **admin review** at the tool
descriptor level (`crates/moa-hands/src/core/policy.rs`): a tenant must approve
an MCP tool action unless an operator action-policy rule (or config) grants it.
Builtin hands keep their own per-tool defaults. Operator rules always override
the descriptor default.
MCP tool annotations are treated as untrusted hints by default. A tool becomes
retry-safe only when its exact server config sets `trust_tool_annotations` to
`true`, the negotiated protocol revision is `2025-03-26` or newer, and the
discovered tool declares `idempotentHint=true`; server names never imply trust.

### Hardcoded tuning knobs

A few operationally relevant values are compile-time constants, not overlay
variables. They are listed here as a pointer, not full documentation.

| Value | Default | Where | Override |
|---|---|---|---|
| Authz decision-cache TTL | 2000 ms | `moa-auth/authz` `require.rs` | `MOA_AUTHZ_DECISION_CACHE_TTL_MS` (env) |
| JWKS refresh cooldown | 10 s | `moa-auth/auth0` `jwks_cache.rs` | constant |
| JWKS unknown-`kid` negative TTL | 60 s | `moa-auth/auth0` `jwks_cache.rs` | constant |
| JWKS negative-cache capacity | 1024 | `moa-auth/auth0` `jwks_cache.rs` | constant |
| Authz outbox poller interval | 500 ms | `moa-auth/authz` `poller.rs` | constant |
| Authz outbox poller batch | 64 rows | `moa-auth/authz` `poller.rs` | constant |
| Authz outbox max delivery attempts | 8 | `moa-auth/authz` `poller.rs` | constant |

## How to set them

### Local shell / `.env.local`

```bash
export MOA_DATABASE_URL="postgres://moa_owner:dev@127.0.0.1:10040/moa"
export MOA_MODELS_MAIN="claude-fable-5"
export MOA_ANTHROPIC_API_KEY="sk-ant-..."
```

**Local trap:** the local/dev stack reads a `.env.local` if present. An
**empty or provider-less `.env.local`** leaves no model provider configured and
the stack falls back to the **`mock`** provider — set
`MOA_GENERAL_DEFAULT_PROVIDER` / `MOA_MODELS_MAIN` (or the compose
`MODEL_PROVIDER`) explicitly to use a real model.

### Docker Compose

The compose stack passes a curated set of `MOA_*` variables through to the edge,
orchestrator, PII, audit-shipper, and loadtest services (Postgres/Restate/
OpenFGA endpoints, ports, and the `MOA_RUSTFS_*` / `MOA_CLEAN_E2E_*` families).
Set them in your shell before `docker compose up`, or in a compose `.env` file;
compose substitutes `${MOA_...}` into the service `environment:` blocks.

### Kubernetes

Non-secret config as plain env, secrets from a `Secret`:

```yaml
env:
  - name: MOA_CONFIG_ENV_STRICT
    value: "1"                       # fail startup on an unknown MOA_* var
  - name: MOA_MODELS_MAIN
    value: "claude-fable-5"
  - name: MOA_PROVIDERS_CONCURRENCY_SCOPE
    value: "global"
  - name: MOA_DATABASE_MAX_CONNECTIONS
    value: "80"                      # size to worker fan-out, not the dev default
  - name: MOA_ANTHROPIC_API_KEY
    valueFrom:
      secretKeyRef: { name: moa-provider-secrets, key: anthropic-api-key }
  - name: MOA_DATABASE_URL
    valueFrom:
      secretKeyRef: { name: moa-db, key: url }
```

## Regenerating the tables

The overlay tables are produced from source, not hand-maintained. The
`#[ignore]` dev test `dump_env_var_reference` in
`crates/moa-config/src/env_overlay/mod.rs` walks `EnvOverlay` and
`MoaConfig::default()` and prints `env_var | config_path | default` for every
variable:

```bash
cargo test -p moa-config dump_env_var_reference -- --ignored --nocapture
```

Descriptions come from each config field's doc comment, resolved along the full
config path (`MoaConfig` → section struct → … → leaf field) so a nested field
never inherits a same-named parent field's comment. When adding or renaming an
overlay field, re-run the dump and refresh the affected section table.
