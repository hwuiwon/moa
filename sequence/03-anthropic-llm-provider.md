# Step 03: Anthropic LLM Provider

## What this step is about
Implementing the `LLMProvider` trait for Anthropic's Claude API with streaming support.

## Files to read
- `docs/01-architecture-overview.md` — `LLMProvider` trait, `ModelCapabilities`, `TokenPricing`
- `docs/07-context-pipeline.md` — How the provider's capabilities inform the pipeline
- `docs/10-technology-stack.md` — Crates: `reqwest`, `eventsource-stream`

## Goal
Call Claude's Messages API with streaming, tool definitions, and proper error handling. Return a `CompletionStream` that yields content blocks (text + tool calls) as they arrive.

## Rules
- Use `reqwest` with SSE streaming — no official Anthropic Rust SDK exists
- Support models: `claude-sonnet-4-6`, `claude-opus-4-6`
- API key read from `ANTHROPIC_API_KEY` env var (via config)
- Implement `ModelCapabilities` accurately (context windows, pricing, caching support)
- Handle rate limits (429) with exponential backoff (3 retries)
- Parse SSE `message_start`, `content_block_start`, `content_block_delta`, `content_block_stop`, `message_delta`, `message_stop` events

## Tasks
1. **`moa-providers/src/anthropic.rs`**: `AnthropicProvider` struct implementing `LLMProvider`
2. **`moa-providers/src/common.rs`**: Shared HTTP client setup, SSE parsing, retry logic
3. **`CompletionRequest` → Anthropic API format** conversion (messages, tools, system prompt)
4. **SSE stream → `CompletionStream`** conversion (parse deltas into `ContentBlock::Text` and `ContentBlock::ToolCall`)
5. **Error handling**: Map HTTP errors, JSON parse errors, rate limits to `MoaError`

## Deliverables
```
moa-providers/src/
├── lib.rs
├── anthropic.rs     # AnthropicProvider
└── common.rs        # Shared HTTP/SSE utilities
```

## Acceptance criteria
1. Can send a simple message and receive a streamed text response
2. Can send a message with tool definitions and receive tool_use blocks
3. Rate limit retry works (mock 429 response)
4. `ModelCapabilities` returns correct context window sizes
5. Streaming tokens arrive incrementally (not buffered to completion)

## Tests
- Unit test: `CompletionRequest` serializes to correct Anthropic API JSON format
- Unit test: Parse a recorded SSE stream into `ContentBlock` sequence
- Integration test (requires API key, skip in CI): Send "What is 2+2?" and verify response contains "4"
- Unit test: Rate limit retry logic with mock HTTP client

```bash
cargo test -p moa-providers
# Integration test (needs ANTHROPIC_API_KEY):
ANTHROPIC_API_KEY=sk-ant-... cargo test -p moa-providers -- --ignored
```

---

