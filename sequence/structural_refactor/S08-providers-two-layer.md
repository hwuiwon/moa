# S08 — Reorganize `moa-providers` into `core/` + `adapters/` two-layer

## Scope

`moa-providers` is the textbook "small core, many vendor adapters" case. Currently flat: `anthropic.rs` (~2,160 LOC), `gemini.rs` (~2,092 LOC), `openai_chat.rs`, `openai_responses.rs` (~1,318), `embedding.rs`, `factory.rs`, `instrumentation.rs`, `retry.rs`, `scripted.rs`, `sse.rs`, etc. Split into `src/core/` (shared plumbing) and `src/adapters/{anthropic,gemini,openai_chat,openai_responses,scripted}/` (per-vendor folders), and break each vendor's giant single file into `mod.rs + request.rs + response.rs + streaming.rs + tools.rs`.

## Preconditions

- S01–S07 complete and merged. **S04 is a hard prerequisite** — the embedding trait now lives in `moa-core`, so `moa-providers` no longer owns it.
- `cargo check --workspace` is green.

## Why this prompt

Each vendor file is ~2k LOC and tangles request types, response types, SSE streaming parser, tool-call translation, and capability tables in one place. The pattern is identical across vendors — same five concerns, different vendors. Two-layer split makes the pattern explicit, lets each vendor evolve independently, and creates a clear seam for adding new providers (the eventual question of "how do I add Mistral" reduces to "copy the openai_chat folder shape").

## Files in scope

```
crates/moa-providers/src/
├── lib.rs                    — module declarations + re-exports
├── core/                     — NEW: shared plumbing
│   ├── mod.rs
│   ├── request.rs            — request types shared across providers
│   ├── response.rs           — response types
│   ├── streaming.rs          — SSE plumbing (was top-level sse.rs)
│   ├── retry.rs              — was top-level retry.rs
│   ├── instrumentation.rs    — was top-level instrumentation.rs
│   ├── factory.rs            — was top-level factory.rs
│   └── schema.rs             — was top-level schema.rs (if exists)
├── adapters/
│   ├── mod.rs
│   ├── anthropic/
│   │   ├── mod.rs            — provider struct + impl LLMProvider
│   │   ├── request.rs        — request type translation
│   │   ├── response.rs       — response parsing
│   │   ├── streaming.rs      — SSE handler
│   │   └── tools.rs          — tool-use translation
│   ├── gemini/  (same shape)
│   ├── openai_chat/  (same shape)
│   ├── openai_responses/  (same shape)
│   └── scripted/             — test-only canned-response provider
│       ├── mod.rs
│       └── ...
└── embedding/                — keep as-is from S04 (impls only; trait is in moa-core)
    ├── mod.rs
    ├── cohere.rs
    ├── gemini.rs
    └── openai.rs
```

## Files explicitly out of scope

- `crates/moa-providers/tests/` — TEST pack handles
- The `LLMProvider` trait (in `moa-core`)
- The `EmbeddingProvider` trait (in `moa-core` after S04)
- Live API tests — must continue to compile but are not run here

## Step-by-step instructions

1. **Create the `core/` and `adapters/` directories.** Move existing top-level files into the right folder:
   - `sse.rs` → `core/streaming.rs`
   - `retry.rs` → `core/retry.rs`
   - `instrumentation.rs` → `core/instrumentation.rs`
   - `factory.rs` → `core/factory.rs`
   - `schema.rs` → `core/schema.rs`
   - `embedding.rs` (and `embedding/` if folder) → already a folder from S04; leave at top level

2. **Identify shared types.** Read each vendor file and find request/response types that are *the same across vendors*:
   - `Message`, `Role`, `ToolCall`, `ToolResult`, `Usage`, `StopReason` likely live in `moa-core` already → use them via `use moa_core::types::*`
   - Provider-internal request shapes (e.g. `AnthropicRequest`, `GeminiContent`) are vendor-specific → stay in the adapter
   - Streaming event types (`StreamingChunk`, `Delta`) — could be shared or vendor-specific. If 80%+ identical across vendors, extract to `core/streaming.rs`. Otherwise leave per-vendor.

3. **Per-vendor split.** For each `adapters/<vendor>/`:
   - **`mod.rs`** — the provider struct (`AnthropicProvider`), `pub fn new`, `impl LLMProvider for AnthropicProvider` block. The `impl LLMProvider` body should mostly be 1-2 line delegations to the sibling modules.
   - **`request.rs`** — `to_<vendor>_request(req: &CompletionRequest) -> VendorRequest`, the vendor's wire-format types, serialization
   - **`response.rs`** — `from_<vendor>_response(raw: VendorResponse) -> CompletionResponse`, the vendor's response types
   - **`streaming.rs`** — `parse_streaming_chunk`, the per-vendor SSE event types, conversion to `core::streaming::CompletionStream`
   - **`tools.rs`** — tool-use translation (vendor's tool-call shape ↔ core's `ToolCall`/`ToolResult`)

4. **`core/streaming.rs` shape.** This file should contain:
   - `CompletionStream` type (alias or wrapper around `Pin<Box<dyn Stream>>`)
   - `StreamingEvent` enum (the unified event type all vendors emit after parsing)
   - SSE-line-parsing helpers usable by every vendor (`parse_sse_line`, `parse_event_data`)
   - **Not** vendor-specific event types — those stay in `adapters/<vendor>/streaming.rs`.

5. **`core/retry.rs`** should keep its current contents — retry policy, backoff, error classification. Vendors call into it.

6. **`core/factory.rs`** is the entry point: `create_provider(config: &ProviderConfig) -> Box<dyn LLMProvider>`. After the split, this match-on-vendor function imports each adapter's `Provider::new`.

7. **`lib.rs` shape after split**:
   ```rust
   //! LLM provider implementations.
   
   mod core;
   mod adapters;
   pub mod embedding;
   
   // Re-export the shared plumbing
   pub use core::factory::create_provider;
   pub use core::retry::{RetryPolicy, RetryConfig};
   pub use core::instrumentation::{InstrumentedProvider, ProviderInstrumentation};
   
   // Re-export provider structs (so existing call sites work)
   pub use adapters::anthropic::AnthropicProvider;
   pub use adapters::gemini::GeminiProvider;
   pub use adapters::openai_chat::OpenAiChatProvider;
   pub use adapters::openai_responses::OpenAiResponsesProvider;
   pub use adapters::scripted::ScriptedProvider;
   ```
   The exact `pub use` set must match what was previously visible at `moa_providers::*`.

8. **The vendor file is the budget.** Target post-split sizes:
   - `mod.rs` ~150–300 LOC (struct + delegating impl)
   - `request.rs` ~300–500 LOC
   - `response.rs` ~300–500 LOC
   - `streaming.rs` ~400–700 LOC
   - `tools.rs` ~200–400 LOC
   
   If any vendor file exceeds 700 LOC after split, split further. If it's under 200 LOC, it didn't need splitting; consolidate.

9. **Run verification.**

10. **Document anything weird** in `REFACTOR_NOTES.md` under `[S08]` — for example, a vendor with an unusual streaming protocol that didn't fit the four-file pattern.

## Verification

```bash
cargo check -p moa-providers --all-targets
cargo clippy -p moa-providers --all-targets -- -D warnings
cargo test -p moa-providers --no-run
cargo check --workspace --all-targets

# Each vendor's tests must still compile:
cargo test -p moa-providers --no-run --test anthropic_provider 2>&1 | head -20
cargo test -p moa-providers --no-run --test openai_provider 2>&1 | head -20
cargo test -p moa-providers --no-run --test gemini_provider 2>&1 | head -20
```

## Acceptance criteria

- [ ] `crates/moa-providers/src/core/` exists with shared plumbing.
- [ ] `crates/moa-providers/src/adapters/{anthropic,gemini,openai_chat,openai_responses,scripted}/` each has `mod.rs`, `request.rs`, `response.rs`, `streaming.rs`, `tools.rs` (some may be empty or omitted if a vendor genuinely doesn't have that concern).
- [ ] No file in `crates/moa-providers/src/` exceeds 700 LOC.
- [ ] `lib.rs` re-exports the previous `pub` surface.
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No downstream crate's source had to change.

## Rollback plan

`git checkout -- crates/moa-providers/src/` and `git clean -fd crates/moa-providers/src/`. The change is contained.

## Notes for the agent

- **Vendors are similar but not identical.** The four-file shape (`request/response/streaming/tools`) is a *target*, not a rule. If Gemini doesn't have a separate streaming concern (because it parses inline), `gemini/streaming.rs` may be empty or omitted. Don't force a split that doesn't exist.
- **`scripted` provider** is test-only. It should follow the same shape but `mod.rs` is probably fine without all four sibling files — it's a fake.
- **The `sse.rs` → `core/streaming.rs` rename**: if `sse.rs` had public re-exports, mirror them. `pub use sse::*;` in `lib.rs` becomes `pub use core::streaming::*;` (or keep both temporarily for transition).
- **Instrumentation wraps providers.** `InstrumentedProvider<P>` likely lives in `core/instrumentation.rs` and wraps any `LLMProvider`. The wrapper itself doesn't move; what moves is *only* its file location.
- **Don't refactor the retry policy.** Retry, backoff, and error classification have subtle production behavior. Only move; don't tune.
- **Don't unify "vendor request types" across vendors.** Anthropic's `Message` is not OpenAI's `Message`. Each vendor's wire format is its own. The unification happens at the `core::request` / `moa-core::types` boundary.
- **Tool-call translation is the trickiest part.** Each vendor expresses tool calls differently. `tools.rs` per vendor handles `to_<vendor>_tool_call` and `from_<vendor>_tool_response`. Don't try to generalize across vendors.
- **Time budget**: 2 sessions. Vendor 1 (anthropic) is the template; vendors 2–4 are pattern application. Scripted is a quick wrapper.
- **Anti-pattern**: do NOT introduce a `Vendor` enum that holds shared provider state. Each provider is a separate struct with its own state. Polymorphism is via the `LLMProvider` trait, not via a sum type.
