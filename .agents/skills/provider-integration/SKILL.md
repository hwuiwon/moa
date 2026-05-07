---
name: provider-integration
description: >
  Use this skill when adding or modifying a provider implementation in the MOA
  workspace: a new `LLMProvider` (third-party chat model), `EmbeddingProvider`,
  `HandProvider` (sandbox or runtime backend), MCP server, or `PlatformAdapter`
  (Telegram, Slack, Discord, or future platforms). It covers the required trait
  surface, model catalog and pricing wiring, credential vault entries, gateway
  routing, observability hooks, feature-flag gating, and the live-test pattern
  with `MOA_RUN_LIVE_*_TESTS` flags. Triggers include: "add support for the X
  model", "integrate Y as an embedding provider", "add a Discord adapter", "wire
  up an MCP server", "support a new sandbox runtime". Do NOT use for general
  Rust mechanics (use `rust`), release-time validation (use `certify`), or
  memory-pack steps that touch embeddings (use `memory-pack`).
compatibility: Rust 2024 MOA workspace with `moa-core` traits, `moa-providers`, `moa-hands`, `moa-gateway`, and `moa-security`
allowed-tools:
  - Read
  - Grep
  - Glob
  - Edit
  - Write
  - Bash(rg:*)
  - Bash(cargo:*)
  - Bash(git:*)
metadata:
  moa-tags: "providers, llm, embeddings, hands, mcp, platform-adapters, integrations"
  moa-one-liner: "Workflow for adding LLM, embedding, hand, MCP, and platform-adapter provider implementations"
---

# Provider Integration

Use this skill when wiring a new external service or backend into MOA. Five trait families share this pattern: `LLMProvider`, `EmbeddingProvider`, `HandProvider`, `PlatformAdapter`, and MCP servers (which compose into `HandProvider` via the MCP router).

## Boundary

Use this skill for:

- adding a new LLM provider (third-party chat model API)
- adding a new embedding or rerank provider
- adding a new hand provider (sandbox: Docker, Daytona, E2B, future runtimes)
- adding a new MCP server integration
- adding a new platform adapter (Telegram, Slack, Discord, future chat platforms)
- modifying any of the above to support new models, sandbox kinds, or platform features

Do not use this skill for:

- general Rust quality review; use `rust`
- selecting which tests to run before release; use `certify`
- diagnosing a failing live provider test; use `runtime-forensics`
- memory-pack steps that happen to touch embeddings; use `memory-pack`
- authoring tests as a first-class task; use `test-authoring` (this skill prescribes the live-test pattern but defers test-authoring discipline to that skill)

## Choose the Provider Type

Decide before reading the trait references:

| Want to integrate | Trait | Crate | Reference |
|---|---|---|---|
| LLM (chat completion, streaming, tool calls) | `LLMProvider` | `moa-providers/src/adapters/<name>/` | [llm-provider-checklist.md](references/llm-provider-checklist.md) |
| Embeddings or rerank | `EmbeddingProvider` | `moa-providers/src/embedding.rs` + `moa-memory/vector/` | [embedding-provider-checklist.md](references/embedding-provider-checklist.md) |
| Sandbox (run untrusted code/commands) | `HandProvider` | `moa-hands/src/adapters/<name>/` | [hand-provider-checklist.md](references/hand-provider-checklist.md) |
| MCP server (tool-bundle exposed via Model Context Protocol) | `HandProvider` via MCP router | `moa-hands/src/adapters/mcp/` | [hand-provider-checklist.md](references/hand-provider-checklist.md) |
| Chat platform (deliver messages, render approvals) | `PlatformAdapter` | `moa-gateway/src/<name>.rs` | [platform-adapter-checklist.md](references/platform-adapter-checklist.md) |

The trait definitions live in `crates/moa-core/src/traits/`. Read the trait first, then the relevant checklist.

## Required Surfaces (All Provider Types)

Every provider implementation touches at least these surfaces:

1. **Trait implementation** — `impl LLMProvider for MyProvider { ... }` etc.
2. **Feature flag** — gate the provider behind a workspace feature flag if it is optional. Don't pull in the dependency on default builds unless the provider is core (only Anthropic/OpenAI/Gemini are core LLMs today).
3. **Credentials** — register the credential keys with `moa-security`'s `CredentialVault`. Read [credentials-and-vault.md](references/credentials-and-vault.md) before deciding where to look up a secret.
4. **Routing wiring** — the gateway, brain, or hand router needs to know the new provider exists. This is usually a one-line addition to a registration table.
5. **Observability** — emit `tracing` spans at the same boundaries as existing providers. See `crates/moa-providers/src/adapters/anthropic/` for the canonical span layout.
6. **Tests** — at minimum a deterministic offline test (wiremock-based for HTTP providers) and a live test gated by an env flag. Hand off to `test-authoring` for the assertion patterns.

LLM providers also need: a model catalog entry and a pricing entry. Embedding providers also need: a halfvec dimension declaration. Hand providers also need: a sandbox-kind tag for the tool router. Platform adapters also need: a renderer for approval prompts.

## Workflow

1. Decide the provider type (see table above) and load the matching reference checklist.
2. Read at least one existing implementation of the same type as a template:
   - LLM: `crates/moa-providers/src/adapters/anthropic/`
   - Embedding: existing OpenAI / Cohere implementations
   - Hand: `crates/moa-hands/src/adapters/local/` for the simplest, `e2b/` for an HTTP-backed sandbox
   - Platform: `crates/moa-gateway/src/telegram.rs`
   - MCP: `crates/moa-hands/src/adapters/mcp/`
3. Implement the trait. Mirror the existing adapter's module layout: a `mod.rs` that exposes the public type, an `api.rs` or `client.rs` for HTTP wire shapes, a `parse.rs` for response parsing, and a `tests/` adjacent to the adapter or in `crates/<crate>/tests/<name>_offline.rs`.
4. Wire credentials through `moa-security`. Do not read environment variables directly from inside the provider; the vault is the indirection.
5. Wire feature flags in the workspace `Cargo.toml`. Optional providers go behind named features; core providers stay default.
6. Register the provider in the routing table: model catalog for LLMs, sandbox kind for hands, platform name for adapters.
7. Write the offline test (wiremock for HTTP providers, scripted fakes for in-memory ones).
8. Write the live test with `#[ignore = "requires MOA_RUN_LIVE_<NAME>_TESTS=1 and <CREDENTIAL>"]` and the matching env-flag check. Reuse an existing flag when the surface is the same; only add a new flag for a new credential family.
9. Update the model catalog and pricing fixtures (LLMs only). Add the model ID, context window, output limit, pricing per million tokens (input, cached input if applicable, output).
10. Hand off to `certify` for the validation matrix.

## Rules

- A provider that calls a paid API must have a live test, and the live test must follow the double-gate pattern (`#[ignore]` + env flag).
- A provider must not read environment variables directly; secrets come from `CredentialVault`.
- A provider's HTTP client must respect cancellation via `tokio::select!` against the cancellation token in the request scope. Long-running calls that ignore cancellation cause approval-flow deadlocks.
- A new LLM provider must include a pricing entry in the same PR as the adapter; missing pricing breaks cost analytics silently.
- A new embedding provider's vector dimension must match the database column type. If the column is `halfvec(1536)`, the provider must produce 1536-dim vectors at the appropriate precision.
- A new hand provider that supports `BuiltInTool` must list every supported tool explicitly. Wildcard support is not allowed; the security model depends on knowing the supported set.
- A platform adapter must implement the approval-render API; a platform that cannot render approvals is not a complete adapter.

## Output Format

When reporting on a new provider integration, include:

- `Provider type`: LLM / Embedding / Hand / MCP / Platform
- `Files changed`: list, grouped by crate
- `Feature flag`: name (or `default` if core)
- `Credentials added`: list of vault keys
- `Routing wired`: where in the registration table
- `Tests`: offline test names, live test names, env flags
- `Model catalog / pricing` (LLM only): model IDs and prices added
- `Verification`: deterministic test result and live test result if run
