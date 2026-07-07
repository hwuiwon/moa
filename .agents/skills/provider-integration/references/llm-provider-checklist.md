# LLM Provider Checklist

For implementing `LLMProvider` from `moa-core::traits`. Use this when adding a third-party chat-completion service (OpenAI-compatible, Anthropic-style, Gemini-style, or a new wire format).

## Trait Surface

The trait lives in `crates/moa-core/src/traits/mod.rs` (function `LLMProvider` near line 373 at time of writing — search for the current line if it has moved). At minimum the implementation must produce a `CompletionResponse` from a `CompletionRequest`, support streaming, surface tool calls in a structured form, and report token usage.

Read at least one existing adapter end-to-end before writing your own:

- `crates/moa-providers/src/adapters/anthropic/` for streaming + cache_control + native web search
- `crates/moa-providers/src/adapters/openai_responses/` for the Responses-API envelope
- `crates/moa-providers/src/adapters/gemini/` for Google's wire format
- `crates/moa-providers/src/adapters/scripted/` for the in-memory test double

## Module Layout

The convention in `moa-providers/src/adapters/<name>/`:

- `mod.rs` — exposes the public `MyProvider` type and its constructor; nothing else
- `client.rs` or `api.rs` — `reqwest` client, base URL, retry policy
- `request.rs` — translation from MOA's `CompletionRequest` to the provider's wire shape
- `parse.rs` (or `stream.rs` for streaming-first providers) — translation from provider responses back to MOA types
- `models.rs` — model catalog entries, context windows, output limits
- `pricing.rs` (or a shared catalog) — pricing per million tokens, including cached-input pricing if supported

If a single file is fewer than 200 lines and has only one shape, it is fine to inline. Mirror the existing adapter that is closest to the new wire format.

## Required Behaviors

1. **Streaming.** All real LLM providers stream. Implement the streaming path even if the offline tests start with non-streaming. Use `tokio_stream` or `futures::Stream` to match the trait signature.
2. **Tool call extraction.** Parse provider-native tool calls into MOA's structured `ToolCall { id, name, arguments }`. Tool names use underscores (`memory_remember`), not dotted names — apply the renaming at the boundary.
3. **Token accounting.** Every response must populate `usage.input_tokens`, `usage.output_tokens`, and `usage.cached_input_tokens` if the provider supports prompt caching. Missing fields default to `None`, not `0`; the analytics layer distinguishes the two.
4. **Cost computation.** Cost is derived from `usage` plus the pricing table; the provider does not compute cost itself. Just make sure the pricing entry exists for every model the catalog exposes.
5. **Cancellation.** The HTTP request must respect cancellation. Wrap the `reqwest` future in a `tokio::select!` against the cancellation token from `RequestScope`. Failing to do this causes hung sessions when the user soft-cancels.
6. **Error mapping.** Map provider HTTP errors to MOA's `ProviderError` enum. Distinguish: 401/403 (auth), 429 (rate limit, surface retry-after), 5xx (transient), 4xx other (request shape).

## Model Catalog and Pricing

Every model the provider exposes needs three things:

1. A `ModelId` constant or registry entry.
2. A model catalog entry with `context_window`, `max_output_tokens`, `supports_tools`, `supports_streaming`, `supports_cache_control`, and `supports_web_search` (booleans for capabilities).
3. A pricing entry with `input_per_mtok`, `cached_input_per_mtok` (if applicable), and `output_per_mtok`.

The pricing entries are versioned; do not edit historical entries. Add a new dated entry if the provider changes prices.

## Live Test Pattern

Every LLM provider needs both:

- An offline test in `crates/moa-providers/tests/<name>_offline.rs` using `wiremock` to assert the request body shape and parse a recorded response shape.
- A live test in `crates/moa-providers/tests/<name>_live.rs` with the double-gate:

```rust
#[tokio::test]
#[ignore = "requires MOA_RUN_LIVE_<NAME>_TESTS=1 and <NAME>_API_KEY"]
async fn name_live_completes_simple_prompt() {
    if std::env::var("MOA_RUN_LIVE_<NAME>_TESTS").as_deref() != Ok("1") {
        return;
    }
    let api_key = std::env::var("<NAME>_API_KEY")
        .expect("<NAME>_API_KEY required when MOA_RUN_LIVE_<NAME>_TESTS=1");
    // ...
}
```

The live test should cover: simple prompt, prompt with tool calls, prompt with web search if supported. Do not test free-text response content; assert on usage tokens, tool call structure, and finish reason.

## Wiring Into the Brain

Two registration points:

1. The provider registry (search `LLMProvider` registration in `crates/moa-providers/src/lib.rs` or wherever the registry lives) — adds a new variant.
2. The brain's provider selection logic — usually keyed by `ModelId` prefix. Make sure the new provider's `ModelId` namespace does not collide with an existing one.

## Snapshot Test the Request Body

Use `insta::assert_json_snapshot!` against the serialized request body, with redactions for any non-deterministic fields. This catches regressions in the wire format that no other test catches. Anchor the snapshot to a specific `CompletionRequest` fixture in `moa-test-support`.

## Common Mistakes

- Computing cost inside the adapter instead of returning structured usage.
- Reading `std::env::var("API_KEY")` inside the adapter instead of accepting a `CredentialVault` handle in the constructor.
- Skipping the cached-input-tokens field because the provider's MVP didn't expose it; add it with `Option<u64>` and populate when the provider does support caching.
- Treating 429 as 5xx in the error mapping; rate limits need their own retry behavior with backoff.
- Hardcoding the model ID inside the request builder; pass it through from `CompletionRequest`.
