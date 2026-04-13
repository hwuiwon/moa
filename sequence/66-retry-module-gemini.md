# Step 66 — Centralized Retry Module + Replace OpenRouter with Gemini Provider

_Extract shared retry/backoff logic into a reusable module. Remove the OpenRouter provider. Add a Google Gemini provider. Three providers total: Anthropic, OpenAI, Google._

---

## 1. What this step is about

Two related changes:

**Part A:** Retry logic is currently scattered across providers — each has its own `send_with_retry` or inline retry loop with different backoff strategies. Extract a shared `RetryPolicy` module in `moa-providers/src/retry.rs` that all providers use.

**Part B:** Remove the OpenRouter provider entirely. Replace it with a Google Gemini provider using the Gemini REST API (`generativelanguage.googleapis.com/v1beta`). The final provider set is: **Anthropic, OpenAI, Google** — three providers, no routing aggregator.

---

## 2. Files to read

For Part A (retry):
- **`moa-providers/src/common.rs`** — Contains `send_with_retry` function. Read it to understand the current retry pattern.
- **`moa-providers/src/anthropic.rs`** — How the Anthropic provider handles retries and errors.
- **`moa-providers/src/openai.rs`** — How the OpenAI provider handles retries.

For Part B (Gemini):
- **`moa-providers/src/anthropic.rs`** — The most complete provider. Use as a structural template for the Gemini provider. Match the pattern: struct with config, `from_config`/`from_env` constructors, `LLMProvider` impl with streaming via SSE.
- **`moa-providers/src/openrouter.rs`** — **DELETE THIS FILE.** Read it first to understand what OpenRouter-specific logic exists so you don't accidentally leave dangling references.
- **`moa-providers/src/factory.rs`** — Provider selection/construction. Replace `openrouter` references with `google`.
- **`moa-providers/src/lib.rs`** — Module declarations and re-exports.
- **`moa-core/src/config.rs`** — Provider config structs. Add `GoogleProviderConfig`.
- **`moa-core/src/types/model.rs`** (after Step 61) or **`moa-core/src/types.rs`** — `ToolCallFormat` enum. Add a `Gemini` variant.
- **Gemini API reference** — See the research report attached to this project for complete API details including endpoint structure, request/response format, SSE streaming with `?alt=sse`, function calling, Google Search grounding, and thought signatures.

---

## 3. Goal

After this step:
1. A shared `RetryPolicy` struct in `moa-providers/src/retry.rs` handles exponential backoff with jitter for all providers
2. `moa-providers/src/openrouter.rs` does not exist
3. `moa-providers/src/gemini.rs` exists and implements `LLMProvider` for Google's Gemini API
4. `factory.rs` routes `"google"` to `GeminiProvider`, and `"openrouter"` is rejected with a helpful error
5. Config supports `[providers.google]` with `api_key_env`
6. Model inference recognizes `gemini-` prefixed models
7. Gemini provider supports: streaming via SSE, function calling, Google Search grounding as a native tool
8. The retry module is used by all three providers

---

## 4. Rules

- **Use `reqwest` + `serde` for Gemini.** No third-party Gemini SDK. Match the pattern of the Anthropic provider (raw HTTP + SSE parsing).
- **Gemini API uses `v1beta`**, endpoint: `https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse`
- **Authentication via header**: `x-goog-api-key: {API_KEY}`
- **Gemini roles are `user` and `model`** (not `assistant`). System instructions go in the top-level `systemInstruction` field, NOT in `contents[]`.
- **Gemini uses parts-based content**: `contents[].parts[]` where each part is `{"text": "..."}`, `{"functionCall": {...}}`, or `{"functionResponse": {...}}`.
- **SSE streaming**: Each `data:` line is a complete `GenerateContentResponse` JSON object. No `[DONE]` sentinel — the stream ends after the final chunk with `finishReason`.
- **Function calling**: Functions declared in `tools[].functionDeclarations[]`. Responses sent back as `functionResponse` parts in a `user` turn with matching `id`.
- **Google Search grounding**: Add `{"google_search": {}}` to the `tools` array (note: snake_case key, not camelCase).
- **Token usage**: `usageMetadata` in the response. Complete breakdown only in the final streaming chunk.
- **Rate limit errors**: HTTP 429 with `RESOURCE_EXHAUSTED` status. No `Retry-After` header — use exponential backoff with jitter.
- **For Gemini 2.5 models**: Use `thinkingConfig.thinkingBudget` (integer). For Gemini 3 models: Use `thinkingConfig.thinkingLevel` (enum). The `reasoning_effort` config maps to these differently per model family.
- **Gemini 3 thought signatures**: Response parts include `thoughtSignature` (base64). These MUST be echoed back in multi-turn conversations. Store them on `ToolInvocation` or `CompletionContent` and replay them in the context pipeline.

---

## 5. Tasks

### Part A: Centralized Retry Module

#### 5a. Create `moa-providers/src/retry.rs`

```rust
use std::time::Duration;
use moa_core::{MoaError, Result};
use reqwest::StatusCode;

pub struct RetryPolicy {
    pub max_retries: usize,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub backoff_factor: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            backoff_factor: 2.0,
        }
    }
}

impl RetryPolicy {
    /// Sends an HTTP request with exponential backoff + jitter on retryable errors.
    pub async fn send<F, Fut>(&self, build_request: F) -> Result<reqwest::Response>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = Result<reqwest::RequestBuilder>>,
    {
        // Implementation: loop up to max_retries
        // On 429, 500, 503: sleep with jittered delay, then retry
        // On 429 with Retry-After header: use that value
        // On non-retryable errors: return immediately
        // On success but non-2xx: check if retryable status
        todo!()
    }

    fn delay_for_attempt(&self, attempt: usize) -> Duration {
        let base = self.initial_delay.as_secs_f64() * self.backoff_factor.powi(attempt as i32);
        let capped = base.min(self.max_delay.as_secs_f64());
        // Add jitter: 50-100% of computed delay
        let jitter = capped * (0.5 + rand::random::<f64>() * 0.5);
        Duration::from_secs_f64(jitter)
    }

    fn is_retryable(status: StatusCode) -> bool {
        matches!(status, StatusCode::TOO_MANY_REQUESTS | StatusCode::INTERNAL_SERVER_ERROR | StatusCode::SERVICE_UNAVAILABLE | StatusCode::BAD_GATEWAY | StatusCode::GATEWAY_TIMEOUT)
    }
}
```

#### 5b. Refactor existing providers to use `RetryPolicy`

Replace `send_with_retry` in `common.rs` (and any inline retry logic in `anthropic.rs`, `openai.rs`) with calls to `RetryPolicy::send()`. Each provider holds a `RetryPolicy` field (configurable via `with_max_retries`).

### Part B: Remove OpenRouter, Add Gemini

#### 5c. Delete `moa-providers/src/openrouter.rs`

#### 5d. Create `moa-providers/src/gemini.rs`

Structure (following the Anthropic provider pattern):

```rust
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
    default_model: String,
    default_reasoning_effort: String,
    default_capabilities: ModelCapabilities,
    retry_policy: RetryPolicy,
    web_search_enabled: bool,
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>, default_model: impl Into<String>) -> Result<Self> { ... }
    pub fn from_config(config: &MoaConfig) -> Result<Self> { ... }
    pub fn from_config_with_model(config: &MoaConfig, model: impl Into<String>) -> Result<Self> { ... }
    pub fn from_env(default_model: impl Into<String>) -> Result<Self> { ... }
}

#[async_trait]
impl LLMProvider for GeminiProvider {
    fn name(&self) -> &str { "google" }
    fn capabilities(&self) -> ModelCapabilities { ... }
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionStream> { ... }
}
```

Key implementation details for `complete()`:
1. Convert `CompletionRequest.messages` to Gemini format: extract system messages → `systemInstruction`, map `User`/`Assistant` → `user`/`model` roles, convert tool results to `functionResponse` parts
2. Build request body with `contents`, `systemInstruction`, `generationConfig`, `tools` (function declarations + optional `google_search`), `toolConfig`
3. POST to `https://generativelanguage.googleapis.com/v1beta/models/{model}:streamGenerateContent?alt=sse`
4. Parse SSE stream: extract text deltas from `candidates[0].content.parts`, detect `functionCall` parts, read `usageMetadata` from final chunk
5. Map to `CompletionContent::Text` / `CompletionContent::ToolCall` / `CompletionContent::ProviderToolResult`

#### 5e. Add model capabilities for Gemini

```rust
fn capabilities_for_model(model: &str) -> ModelCapabilities {
    match model {
        m if m.starts_with("gemini-3.1-pro") => ModelCapabilities {
            model_id: model.to_string(),
            context_window: 1_000_000,
            max_output: 64_000,
            supports_tools: true,
            supports_vision: true,
            supports_prefix_caching: true,
            cache_ttl: None,
            tool_call_format: ToolCallFormat::Gemini,
            pricing: TokenPricing {
                input_per_mtok: 2.0,
                output_per_mtok: 12.0,
                cached_input_per_mtok: Some(0.2),
            },
            native_tools: google_search_tool(),
        },
        m if m.starts_with("gemini-3-flash") || m.starts_with("gemini-3.1-flash") => ModelCapabilities {
            context_window: 1_000_000,
            max_output: 64_000,
            pricing: TokenPricing { input_per_mtok: 0.5, output_per_mtok: 3.0, cached_input_per_mtok: Some(0.05) },
            ..pro_base(model)
        },
        m if m.starts_with("gemini-2.5-pro") => ModelCapabilities {
            context_window: 1_000_000,
            max_output: 65_000,
            pricing: TokenPricing { input_per_mtok: 1.25, output_per_mtok: 10.0, cached_input_per_mtok: Some(0.125) },
            ..pro_base(model)
        },
        m if m.starts_with("gemini-2.5-flash") => ModelCapabilities {
            context_window: 1_000_000,
            max_output: 65_000,
            pricing: TokenPricing { input_per_mtok: 0.3, output_per_mtok: 2.5, cached_input_per_mtok: Some(0.03) },
            ..pro_base(model)
        },
        _ => /* fallback */ ...
    }
}
```

#### 5f. Update `factory.rs`

- Replace `PROVIDER_OPENROUTER` with `PROVIDER_GOOGLE`
- Add `"google"` arm in `build_provider_from_selection`
- Update `infer_provider_name` to recognize `gemini-` prefixed models → `"google"`
- Remove `normalize_openrouter_model` and all OpenRouter-specific logic
- Add helpful error message: `"openrouter is no longer supported; use anthropic, openai, or google"`

#### 5g. Update `moa-core/src/config.rs`

Replace `OpenRouterProviderConfig` with `GoogleProviderConfig`:
```rust
pub struct GoogleProviderConfig {
    pub api_key_env: String,  // default: "GOOGLE_API_KEY"
}
```

Update `ProvidersConfig` to have `pub google: GoogleProviderConfig` instead of `pub openrouter: OpenRouterProviderConfig`.

Config file format:
```toml
[providers.google]
api_key_env = "GOOGLE_API_KEY"
```

#### 5h. Update `ToolCallFormat` enum

Add `Gemini` variant to distinguish Gemini's `functionCall`/`functionResponse` format from Anthropic's and OpenAI's.

#### 5i. Update `lib.rs`

```rust
pub mod gemini;  // replaces pub mod openrouter;
pub use gemini::GeminiProvider;  // replaces pub use openrouter::OpenRouterProvider;
```

#### 5j. Update `moa-providers/src/instrumentation.rs`

If there are any OpenRouter-specific instrumentation paths, update them for Google.

#### 5k. Update Tauri model list

In `src-tauri/src/commands.rs`, update `list_model_options` to replace OpenRouter models with Gemini models:
```rust
ModelOptionDto {
    value: "gemini-2.5-flash".to_string(),
    label: "Gemini 2.5 Flash".to_string(),
    provider: "google".to_string(),
},
ModelOptionDto {
    value: "gemini-2.5-pro".to_string(),
    label: "Gemini 2.5 Pro".to_string(),
    provider: "google".to_string(),
},
```

#### 5l. Update environment variables

In docs and config:
```bash
# Replace:
OPENROUTER_API_KEY=sk-or-...
# With:
GOOGLE_API_KEY=AIza...
```

---

## 6. Deliverables

### Part A
- [ ] `moa-providers/src/retry.rs` — Shared `RetryPolicy` with exponential backoff + jitter
- [ ] `moa-providers/src/common.rs` — Updated to use `RetryPolicy`
- [ ] `moa-providers/src/anthropic.rs` — Uses `RetryPolicy`
- [ ] `moa-providers/src/openai.rs` — Uses `RetryPolicy`

### Part B
- [ ] `moa-providers/src/openrouter.rs` — **DELETED**
- [ ] `moa-providers/src/gemini.rs` — Google Gemini provider
- [ ] `moa-providers/src/factory.rs` — Updated: `google` replaces `openrouter`
- [ ] `moa-providers/src/lib.rs` — Updated module declarations
- [ ] `moa-core/src/config.rs` — `GoogleProviderConfig` replaces `OpenRouterProviderConfig`
- [ ] `moa-core/src/types/model.rs` or `types.rs` — `ToolCallFormat::Gemini` variant
- [ ] `src-tauri/src/commands.rs` — Updated model list

---

## 7. Acceptance criteria

1. `cargo build --workspace` compiles with zero errors.
2. `cargo test --workspace` passes (OpenRouter tests are removed, Gemini unit tests added).
3. `openrouter.rs` does not exist anywhere in the repo.
4. No references to `openrouter` or `OpenRouter` remain in any source file (except possibly migration notes).
5. Setting `default_provider = "google"` and `default_model = "gemini-2.5-flash"` with a valid `GOOGLE_API_KEY` → agent streams responses correctly.
6. Gemini function calling works: agent can call MOA tools via Gemini's `functionCall`/`functionResponse` protocol.
7. Gemini Google Search grounding works: asking "what's the latest news" triggers grounding and shows search results.
8. All three providers use `RetryPolicy` for HTTP requests.
9. Rate-limited requests (429) are retried with exponential backoff + jitter.
10. `grep -r "send_with_retry" moa-providers/src/` finds only the shared implementation in `retry.rs` (or `common.rs` if the function stays there).