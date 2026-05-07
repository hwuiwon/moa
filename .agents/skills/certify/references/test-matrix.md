# Test Matrix

This file is the command map for `certify`. Run the smallest section that still covers the changed surface. Crate paths are based on the workspace verification at the time of writing; if a target does not exist, do not invent one — read `crates/<name>/tests/` to find the current name.

## Prerequisites

- On macOS dev machines, prefer `PROTOC=/opt/homebrew/bin/protoc` when running `cargo` commands that need protobuf tooling.
- Restate cloud or self-hosted runtime is required only for Restate live tests; deterministic Restate tests typically run in-process.
- Live provider checks require the relevant API keys in the environment.
- Set `MOA_RUN_LIVE_PROVIDER_TESTS=1` for the live local-orchestrator matrix.
- Set `MOA_RUN_LIVE_COHERE_TESTS=1` for live Cohere embed/rerank tests.

## Baseline Hygiene

For any Rust change:

```bash
cargo fmt --all
cargo clippy -p <touched-crate> --all-targets --all-features --locked -- -D warnings
```

For a pre-release gate or wide cross-crate change:

```bash
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

## Orchestrator, Approval, Lifecycle, Replay

Two orchestrator backends share a contract harness in `crates/moa-orchestrator-local/tests/support/`. Verify the local orchestrator first; the Restate workflows live in `crates/moa-orchestrator/`.

Local orchestrator deterministic suite:

```bash
cargo test -p moa-orchestrator-local --test local_orchestrator -- --test-threads=1
```

Restate orchestrator deterministic suites (consolidate workflow, session VO, ingestion, tool executor, llm gateway):

```bash
cargo test -p moa-orchestrator --test consolidate -- --test-threads=1
cargo test -p moa-orchestrator --test session_vo -- --test-threads=1
cargo test -p moa-orchestrator --test session_store -- --test-threads=1
cargo test -p moa-orchestrator --test tool_executor -- --test-threads=1
cargo test -p moa-orchestrator --test llm_gateway -- --test-threads=1
cargo test -p moa-orchestrator --test ingestion_e2e -- --test-threads=1
cargo test -p moa-orchestrator --test workspace -- --test-threads=1
cargo test -p moa-orchestrator --test integration -- --test-threads=1
```

Live orchestrator approval roundtrip (local):

```bash
MOA_RUN_LIVE_PROVIDER_TESTS=1 cargo test -p moa-orchestrator-local --test live_provider_roundtrip -- --ignored --nocapture
```

Observability audit when traces, cache metrics, or session timing changed:

```bash
cargo test -p moa-orchestrator-local --test live_observability -- --ignored --nocapture
```

Prometheus-metrics surface:

```bash
cargo test -p moa-orchestrator-local --test prometheus_metrics
```

## Providers, Models, Pricing, Tool Parsing, Web Search

Deterministic:

```bash
cargo test -p moa-providers --lib
```

Live provider matrix (requires keys):

```bash
cargo test -p moa-providers --test live_provider_matrix -- --ignored --nocapture
```

Per-provider live smoke when narrowing a failure:

```bash
cargo test -p moa-providers --test anthropic_live -- --ignored --nocapture
cargo test -p moa-providers --test openai_live -- --ignored --nocapture
cargo test -p moa-providers --test gemini_live -- --ignored --nocapture
```

If you do not see one of these test files, list `crates/moa-providers/tests/` and use the actual name.

## Session Store, Event Schema, Analytics, Persistence

Deterministic:

```bash
cargo test -p moa-session --tests
cargo test -p moa-brain --tests
```

If the change affects session-derived analytics or event accounting, also rerun the orchestrator deterministic suites because they exercise persisted session state through real flows.

## Memory and Context Pipeline

Deterministic:

```bash
cargo test -p moa-brain --test brain_turn -- --test-threads=1
cargo test -p moa-brain --test stable_prefix -- --test-threads=1
cargo test -p moa-memory --tests
```

Live cache or live harness verification when prompt layout or cache planning changed:

```bash
cargo test -p moa-brain --test live_harness -- --ignored --nocapture
cargo test -p moa-brain --test live_cache_audit -- --ignored --nocapture
```

If those targets do not exist, list `crates/moa-brain/tests/` and use the names that do.

## Hands and MCP

Deterministic:

```bash
cargo test -p moa-hands --tests
```

When sandbox provider behavior changed, scope to the affected adapter:

```bash
cargo test -p moa-hands --test local_provider
cargo test -p moa-hands --test daytona_provider
cargo test -p moa-hands --test e2b_provider
cargo test -p moa-hands --test mcp
```

## Skills and Eval Infrastructure

Deterministic:

```bash
cargo test -p moa-skills --tests
cargo test -p moa-eval --tests -- --test-threads=1
```

If a workspace skill or skill regression suite changed:

```bash
cargo run -p moa-cli -- eval skill <skill-name> --ci
```

## Gateway

Deterministic, with feature flags as needed:

```bash
cargo test -p moa-gateway --tests
cargo test -p moa-gateway --tests --features telegram
cargo test -p moa-gateway --tests --features slack
cargo test -p moa-gateway --tests --features discord
```

## Suggested Release Gate

Use this when the change spans orchestrators, providers, or persistence:

1. `cargo fmt --all`
2. workspace `clippy`
3. `moa-providers --lib`
4. `moa-session --tests`
5. `moa-orchestrator-local --test local_orchestrator`
6. `moa-orchestrator` deterministic suites that match the change
7. live provider matrix if provider/envs are available
8. live local orchestrator approval roundtrip if approval/tool flow changed
