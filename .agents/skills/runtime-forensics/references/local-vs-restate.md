# Local vs Restate

Use this when the same user-visible flow passes on `moa-orchestrator-local` and fails on `moa-orchestrator` (Restate), or when Restate-only approval, restart, or worker-recovery behavior looks wrong.

## First Principle

Do not start by diffing the adapters line-by-line. Start by proving whether the shared lifecycle contract is intact on both sides.

The shared harness lives in:

- `crates/moa-orchestrator-local/tests/support/orchestrator_contract.rs`

The two main suites live in:

- `crates/moa-orchestrator-local/tests/local_orchestrator.rs`
- `crates/moa-orchestrator/tests/` (multiple files: `consolidate.rs`, `session_vo.rs`, `session_store.rs`, `tool_executor.rs`, `llm_gateway.rs`, `ingestion_e2e.rs`, `workspace.rs`, `integration.rs`)

## Classification Flow

1. Run the nearest shared-lifecycle assertions or exact adapter tests on both backends.
2. Compare persisted session events for the same scenario, not just stdout or test assertions.
3. Find the first missing or reordered lifecycle edge:
   - blank session should wait for the first message
   - queued messages should remain FIFO
   - approval should persist, pause, resume, then continue
   - cancel should stop cleanly without inventing extra turns
4. Only after that should you inspect adapter mechanics such as Restate signal wiring, virtual-object state, or worker lifecycle.

## Strong Signals

- If both orchestrators fail the same shared contract assertion, the bug is probably in shared lifecycle logic or the brain harness.
- If the local orchestrator passes and Restate fails before the expected persisted event exists, the bug is probably in Restate workflow control flow, signal delivery, or virtual-object boundaries.
- If both persist the same events but UI/runtime behavior differs, the bug is probably in runtime-event translation or observation plumbing.
- If only restart or worker-recovery tests fail, focus on Restate's replay and workflow-resume semantics rather than normal turn execution.

## Restate-Specific Places To Inspect

- `crates/moa-orchestrator/src/services/` - Restate service handlers
- `crates/moa-orchestrator/src/objects/` - virtual-object state machines
- `crates/moa-orchestrator/src/workflows/` - workflow definitions (Consolidate, IntentDiscovery)
- `crates/moa-orchestrator/src/turn/` - turn-level orchestration
- `crates/moa-orchestrator/src/restate_register.rs` - registration of services and objects
- `docs/12-restate-architecture.md` - the Restate architecture deep dive
- `docs/implementation-caveats.md` - already-known caveats

## Exact Test Targets Worth Using

```bash
# local
cargo test -p moa-orchestrator-local --test local_orchestrator -- --test-threads=1

# Restate, scoped to the surface that changed
cargo test -p moa-orchestrator --test session_vo -- --test-threads=1
cargo test -p moa-orchestrator --test consolidate -- --test-threads=1
cargo test -p moa-orchestrator --test tool_executor -- --test-threads=1
```

Live local approval roundtrip, when that path is implicated:

```bash
MOA_RUN_LIVE_PROVIDER_TESTS=1 cargo test -p moa-orchestrator-local --test live_provider_roundtrip -- --ignored --nocapture
```

If a target does not exist, list `crates/moa-orchestrator-local/tests/` or `crates/moa-orchestrator/tests/` to find the actual current names.

## What Good Evidence Looks Like

- a local event sequence and a Restate event sequence for the same scenario
- the first point where Restate stops, duplicates, or skips lifecycle progress
- any matching approval request id, queued message text, or tool id needed to prove ordering
- if available, matching `session_turn` or `tool_execution` spans that show whether the turn stalled before or after persistence

## Determinism Risk

Restate replays workflow code on resume. If the workflow code is non-deterministic (reads system clock outside of `ctx.run`, generates UUIDs without seeding, iterates a `HashMap` where order matters), replay produces a different durable step sequence than the first run. This shows up as silent divergence after a worker restart. When investigating restart or recovery issues, audit the workflow code for these specific patterns before suspecting infrastructure.
