# Test Matrix

This file is the command map for `certify`. Run the smallest section that still covers the changed surface. Crate paths are based on the workspace verification at the time of writing; if a target does not exist, do not invent one — read `crates/<name>/tests/` to find the current name.

## Prerequisites

- On macOS dev machines, prefer `PROTOC=/opt/homebrew/bin/protoc` when running `cargo` commands that need protobuf tooling.
- Restate cloud or self-hosted runtime is required only for Restate live tests; deterministic Restate tests typically run in-process.
- Live provider checks require the relevant API keys in the environment.
- Set `MOA_RUN_LIVE_PROVIDER_TESTS=1` for live provider round-trip tests.
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

The Restate orchestrator (`crates/moa-orchestrator/`) is the only orchestrator backend; the former `moa-orchestrator-local` crate was removed (PRs #186/#196).

Restate orchestrator deterministic suites (session VO, session store, tool executor, llm gateway, ingestion, workspace, replay):

```bash
cargo test -p moa-orchestrator --test session_vo -- --test-threads=1
cargo test -p moa-orchestrator --test session_store_db -- --test-threads=1
cargo test -p moa-orchestrator --test tool_executor -- --test-threads=1
cargo test -p moa-orchestrator --test llm_gateway -- --test-threads=1
cargo test -p moa-orchestrator --test ingestion_service_e2e -- --test-threads=1
cargo test -p moa-orchestrator --test workspace -- --test-threads=1
cargo test -p moa-orchestrator --test integration_service_e2e -- --test-threads=1
cargo test -p moa-orchestrator --test replay_determinism -- --test-threads=1
cargo test -p moa-orchestrator --test worker_delegation -- --test-threads=1
```

If a target does not exist, list `crates/moa-orchestrator/tests/` and use the actual name.

## Providers, Models, Pricing, Tool Parsing, Web Search

Deterministic:

```bash
cargo test -p moa-providers --lib
```

Live provider matrix (requires keys):

```bash
cargo test -p moa-providers --test provider_matrix_live -- --ignored --nocapture
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
cargo test -p moa-brain --test brain_turn_db -- --test-threads=1
cargo test -p moa-brain --test stable_prefix_db_memory -- --test-threads=1
cargo test -p moa-memory --tests
```

Live cache or live harness verification when prompt layout or cache planning changed:

```bash
cargo test -p moa-brain --test harness_live -- --ignored --nocapture
cargo test -p moa-brain --test cache_audit_live -- --ignored --nocapture
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
curl -X POST "$MOA_EDGE_URL/v1/evals/run" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  --data @skill-eval-request.json
```

## Gateway

Deterministic, with feature flags as needed:

```bash
cargo test -p moa-gateway --tests
cargo test -p moa-gateway --tests --features slack
```

## Suggested Release Gate

Use this when the change spans orchestrators, providers, or persistence:

1. `cargo fmt --all`
2. workspace `clippy`
3. `moa-providers --lib`
4. `moa-session --tests`
5. `moa-orchestrator` deterministic suites that match the change
6. live provider matrix if provider/envs are available
