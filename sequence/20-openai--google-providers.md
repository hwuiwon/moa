# Step 20: OpenAI + Google Providers

## What this step is about
Adding `LLMProvider` implementations for OpenAI and Google.

## Files to read
- `docs/01-architecture-overview.md` — `LLMProvider` trait
- `docs/07-context-pipeline.md` — Provider-specific cache behavior

## Goal
Users can switch between Anthropic, OpenAI, and Google models via config or `/model` command. All three support streaming, tool use, and accurate capability reporting.

## Tasks
1. **`moa-providers/src/openai.rs`**: `OpenAIProvider` using `async-openai` crate. Chat completions with streaming and tool_choice.
2. **`moa-providers/src/gemini.rs`**: `GeminiProvider` — Google Gemini REST API with SSE streaming and function calling.
3. **Model capabilities**: Accurate context windows, pricing, and cache behavior for each provider's models.
4. **Update brain harness**: Provider selection from config. `/model` slash command to switch.

## Deliverables
`moa-providers/src/openai.rs`, `moa-providers/src/gemini.rs`

## Acceptance criteria
1. OpenAI: Chat completion with streaming works
2. OpenAI: Tool use works (parallel tool calls)
3. Google: Streaming and tool use work through Gemini's API
4. Model switching via config and `/model` command
5. Capabilities correctly report context windows and pricing

## Tests
- Unit test: Request format matches OpenAI API spec
- Unit test: SSE parsing handles OpenAI's format (different from Anthropic's)
- Integration test (with API key): Send message, verify response, test tool use

---
