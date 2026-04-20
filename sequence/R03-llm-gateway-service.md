# R03 — `LLMGateway` Service

## Purpose

Ship the `LLMGateway` Service: a Restate Service that wraps `moa-providers` and provides journaled, retry-safe LLM calls to all upstream handlers. Every brain turn will call into this service rather than hitting provider SDKs directly. This prompt also introduces the pattern for wrapping external HTTP calls in `ctx.run()`.

End state: `moa-orchestrator` exposes `LLMGateway::complete` that accepts a `CompletionRequest`, dispatches to the configured provider (Anthropic, OpenAI, OpenRouter), emits token usage and cost events, and returns a `CompletionResponse`. Unit tests mock the provider; integration test makes a real call if an API key is available.

## Prerequisites

- R01, R02 complete.
- `moa-providers` crate exists with `LLMProvider` trait and at least one implementation (Anthropic recommended).
- At least one of `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `OPENROUTER_API_KEY` available for integration testing.

## Read before starting

- `docs/12-restate-architecture.md` — "Core Restate concepts", section on `ctx.run()`
- `docs/02-brain-orchestration.md` — existing brain loop LLM call site (for context)
- `moa-providers/src/lib.rs` — existing `LLMProvider` trait and provider implementations
- `moa-core/src/types.rs` — `CompletionRequest`, `CompletionResponse`, `ModelCapabilities`

## Steps

### 1. Understand what "journaled" means for LLM calls

A Restate handler can be invoked, partially execute, crash, and be retried. Every `ctx.run("name", || async { ... })` block:
- On first execution: runs the closure, stores the result in the journal.
- On retry/replay: skips the closure, returns the journaled result directly.

For LLM calls, this means: if a handler crashes *after* the LLM returned but *before* the handler finished, the retry does not re-call the LLM. It returns the cached result. This is essential — you do not want to pay for or re-generate an LLM response.

### 2. Define the Service trait

`moa-orchestrator/src/services/llm_gateway.rs`:

```rust
use restate_sdk::prelude::*;
use moa_core::types::*;  // CompletionRequest, CompletionResponse, etc.

#[restate_sdk::service]
pub trait LLMGateway {
    async fn complete(
        ctx: Context<'_>,
        req: CompletionRequest,
    ) -> Result<CompletionResponse, HandlerError>;

    /// Streaming variant: returns a handle that the caller polls for chunks.
    /// Streaming state lives in memory on the gateway pod; handle is valid for 5 minutes.
    async fn stream_complete(
        ctx: Context<'_>,
        req: CompletionRequest,
    ) -> Result<CompletionStreamHandle, HandlerError>;

    /// Poll a stream handle for the next chunk.
    async fn poll_stream(
        ctx: Context<'_>,
        handle: CompletionStreamHandle,
    ) -> Result<StreamPoll, HandlerError>;
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletionStreamHandle {
    pub id: uuid::Uuid,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum StreamPoll {
    Chunk { text: String, partial_tokens: usize },
    Done { full_response: CompletionResponse },
    Error { message: String },
}
```

### 3. Provider dispatch

```rust
use std::sync::Arc;
use moa_providers::{AnthropicProvider, OpenAIProvider, OpenRouterProvider, LLMProvider};

pub struct LLMGatewayImpl {
    pub providers: Arc<ProviderRegistry>,
}

pub struct ProviderRegistry {
    anthropic: Option<Arc<AnthropicProvider>>,
    openai: Option<Arc<OpenAIProvider>>,
    openrouter: Option<Arc<OpenRouterProvider>>,
}

impl ProviderRegistry {
    pub fn from_env() -> Self {
        Self {
            anthropic: std::env::var("ANTHROPIC_API_KEY").ok()
                .map(|key| Arc::new(AnthropicProvider::new(key))),
            openai: std::env::var("OPENAI_API_KEY").ok()
                .map(|key| Arc::new(OpenAIProvider::new(key))),
            openrouter: std::env::var("OPENROUTER_API_KEY").ok()
                .map(|key| Arc::new(OpenRouterProvider::new(key))),
        }
    }

    pub fn resolve(&self, model: &str) -> Result<Arc<dyn LLMProvider>, HandlerError> {
        if model.starts_with("claude-") {
            self.anthropic.clone().map(|p| p as Arc<dyn LLMProvider>)
                .ok_or_else(|| HandlerError::from("Anthropic provider not configured"))
        } else if model.starts_with("gpt-") || model.starts_with("o1-") || model.starts_with("o3-") {
            self.openai.clone().map(|p| p as Arc<dyn LLMProvider>)
                .ok_or_else(|| HandlerError::from("OpenAI provider not configured"))
        } else {
            // Fallback to OpenRouter for any model name it recognizes
            self.openrouter.clone().map(|p| p as Arc<dyn LLMProvider>)
                .ok_or_else(|| HandlerError::from(format!("No provider for model: {}", model)))
        }
    }
}
```

### 4. Implement `complete`

```rust
impl LLMGateway for LLMGatewayImpl {
    async fn complete(
        ctx: Context<'_>,
        req: CompletionRequest,
    ) -> Result<CompletionResponse, HandlerError> {
        let providers = get_providers_from_ctx(&ctx);
        let provider = providers.resolve(&req.model)?;

        // The LLM call itself must be inside ctx.run() so it's journaled.
        // Retries on handler-level failures do not re-hit the provider.
        let req_clone = req.clone();
        let response = ctx.run("llm_complete", || async move {
            provider.complete(req_clone).await
                .map_err(|e| HandlerError::from(format!("LLM call failed: {}", e)))
        })
        .await?;

        // Emit cost/token usage event — fire-and-forget to SessionStore if session_id present.
        if let Some(session_id) = req.session_id {
            let evt = moa_core::types::SessionEvent::BrainResponse {
                text: response.text.clone(),
                model: response.model.clone(),
                input_tokens: response.input_tokens,
                output_tokens: response.output_tokens,
                cost_cents: compute_cost_cents(&response),
                duration_ms: response.duration_ms,
            };

            // Send (not call) — fire-and-forget, don't block on event persistence.
            ctx.service_client::<SessionStoreClient>()
                .append_event(session_id, evt)
                .send();
        }

        Ok(response)
    }

    async fn stream_complete(
        ctx: Context<'_>,
        req: CompletionRequest,
    ) -> Result<CompletionStreamHandle, HandlerError> {
        // For R03, streaming is stub-only. Brain loop uses non-streaming `complete`.
        // Real streaming wiring lands in a follow-up after the basic path works.
        Err(HandlerError::from("stream_complete not yet implemented"))
    }

    async fn poll_stream(
        ctx: Context<'_>,
        _handle: CompletionStreamHandle,
    ) -> Result<StreamPoll, HandlerError> {
        Err(HandlerError::from("poll_stream not yet implemented"))
    }
}

fn compute_cost_cents(response: &CompletionResponse) -> u32 {
    // Per-model pricing table lookup. For R03, hard-code a default; R11 adds
    // dynamic pricing config.
    let (input_price, output_price) = match response.model.as_str() {
        m if m.starts_with("claude-opus") => (15.0, 75.0),      // per MTok
        m if m.starts_with("claude-sonnet") => (3.0, 15.0),
        m if m.starts_with("claude-haiku") => (0.25, 1.25),
        m if m.starts_with("gpt-4o") => (2.5, 10.0),
        _ => (1.0, 2.0),
    };
    let input_cost = (response.input_tokens as f64 / 1_000_000.0) * input_price;
    let output_cost = (response.output_tokens as f64 / 1_000_000.0) * output_price;
    ((input_cost + output_cost) * 100.0) as u32
}
```

### 5. Retry policy

Configure retry policy for `LLMGateway::complete` in the Restate deployment registration. The SDK reads this from handler attributes or service config:

```rust
// Handler-level config — exact syntax per restate-sdk 0.8 docs
#[restate_sdk::service]
pub trait LLMGateway {
    #[retry(max_attempts = 5, initial_interval_ms = 1000, backoff_multiplier = 2.0)]
    async fn complete(ctx: Context<'_>, req: CompletionRequest)
        -> Result<CompletionResponse, HandlerError>;
    // ...
}
```

If the SDK version doesn't support per-handler retry attributes yet, configure via admin API at deployment registration. R10 covers the full config-as-code approach.

### 6. Wire into main

```rust
// main.rs
let providers = Arc::new(ProviderRegistry::from_env());
let _ = PROVIDERS.set(providers.clone());

HttpServer::new(
    Endpoint::builder()
        .bind(services::health::HealthImpl.serve())
        .bind(services::session_store::SessionStoreImpl { pool: pool.clone() }.serve())
        .bind(services::llm_gateway::LLMGatewayImpl { providers }.serve())
        .build(),
)
.listen_and_serve(...)
.await
```

### 7. Unit tests

`moa-orchestrator/tests/llm_gateway.rs`:

- `resolve_provider_for_claude_model` — registry returns Anthropic for `claude-sonnet-4-20250514`
- `resolve_provider_for_gpt_model` — registry returns OpenAI for `gpt-4o`
- `resolve_provider_unknown_model_falls_back_to_openrouter` — if OpenRouter key present
- `compute_cost_cents_sonnet` — known input/output tokens → expected cost
- `complete_propagates_provider_error` — with a mock provider that errors, handler returns HandlerError

Use `mockall` or a hand-rolled mock `LLMProvider` trait object for these.

### 8. Integration test

`moa-orchestrator/tests/integration/llm_gateway_e2e.rs`:

- Only runs if `ANTHROPIC_API_KEY` is set (skip with `#[ignore]` otherwise).
- Register service with restate-server.
- Call `LLMGateway/complete` with a simple "What is 2+2?" prompt.
- Assert response has non-empty text, input_tokens > 0, output_tokens > 0.
- Verify `BrainResponse` event appeared in Postgres (if session_id supplied).

## Files to create or modify

- `moa-orchestrator/src/services/llm_gateway.rs` — new
- `moa-orchestrator/src/services/mod.rs` — add `pub mod llm_gateway;`
- `moa-orchestrator/src/main.rs` — wire service
- `moa-orchestrator/Cargo.toml` — add `moa-providers` dep
- `moa-orchestrator/tests/llm_gateway.rs` — unit tests
- `moa-orchestrator/tests/integration/llm_gateway_e2e.rs` — integration test
- `moa-providers/src/lib.rs` — no change required if trait is stable; verify `LLMProvider::complete` signature matches the gateway's expectations

## Acceptance criteria

- [ ] `cargo build -p moa-orchestrator` succeeds.
- [ ] `cargo test -p moa-orchestrator llm_gateway` passes (unit tests).
- [ ] `cargo test -p moa-orchestrator --test llm_gateway_e2e -- --ignored` passes if `ANTHROPIC_API_KEY` is set.
- [ ] `restate invocation call 'LLMGateway/complete'` with `{"model": "claude-sonnet-4-5", "messages": [...]}` returns a `CompletionResponse` with non-zero tokens.
- [ ] When `session_id` is supplied in the request, a `BrainResponse` event is persisted to Postgres.
- [ ] Killing `moa-orchestrator` mid-call and restarting: Restate replays the journal, the LLM is *not* re-called (verify via provider request logs or billing telemetry).

## Notes

- **Do not put LLM pricing into `tracing` spans as metrics**: the cost is computed per-response in the gateway and stored in the `BrainResponse` event. Metrics for billing live in Postgres queries, not Prometheus. Metrics in Grafana reflect *rates* and *latencies*, not dollars-of-record. (R11 details this.)
- **Prompt caching is not yet wired**: `cache_control` breakpoints on the request come from the context pipeline (R06). Gateway passes them through to providers as-is.
- **Streaming deferred**: the `stream_complete` / `poll_stream` handlers are stubs. Real streaming requires in-memory stream state on the gateway pod plus a polling protocol; address after R06 ships the non-streaming path.
- **Multi-provider failover not yet implemented**: a 429 on Anthropic should not automatically failover to OpenAI mid-session (breaks prompt caching, tokenizer shifts). Failover within a provider family (e.g., Sonnet → Opus) is future work. For R03, propagate errors up; Restate's retry policy handles transient failures.
- **Rate limiting**: not in R03. Phase 3 introduces the TensorZero-cored gateway with DRR scheduling. For R03, assume a single tenant and rely on provider-side rate limits.

## What R04 expects

- `LLMGateway::complete` callable from any Restate handler.
- Provider dispatch works for at least Anthropic.
- Cost computation happens at the gateway.
- Event emission to SessionStore happens as a `send()` (fire-and-forget).
