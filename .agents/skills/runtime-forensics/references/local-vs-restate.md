# Brain Harness vs Restate

Use this when the same user-visible flow passes in the in-process brain harness (`moa-brain` streamed-turn tests) but fails under the Restate orchestrator (`moa-orchestrator`), or when Restate-only approval, restart, or worker-recovery behavior looks wrong.

The former `moa-orchestrator-local` crate was removed (PRs #186/#196). Restate is now the only orchestrator backend, so "adapter drift" means the brain pipeline behaves one way when driven directly and another way when driven through Restate workflows.

## First Principle

Do not start by diffing the Restate workflow line-by-line. Start by proving whether the shared turn logic in `moa-brain` is intact when driven directly.

The brain harness suites live in:

- `crates/moa-brain/tests/brain_turn_db.rs` (buffered turn lifecycle, approvals, cancellation)
- `crates/moa-brain/tests/brain_turn_cache_replay_db_memory.rs` (cache + replay accounting)

The Restate suites live in:

- `crates/moa-orchestrator/tests/` (multiple files: `session_vo.rs`, `session_store_db.rs`, `tool_executor.rs`, `llm_gateway.rs`, `ingestion_service_e2e.rs`, `workspace.rs`, `integration_service_e2e.rs`, `replay_determinism.rs`, `sub_agent_delegation.rs`, `session_turn_lifecycle_service_e2e.rs`)

## Classification Flow

1. Run the nearest brain-harness assertions and the matching Restate suite.
2. Compare persisted session events for the same scenario, not just stdout or test assertions.
3. Find the first missing or reordered lifecycle edge:
   - blank session should wait for the first message
   - queued messages should remain FIFO
   - approval should persist, pause, resume, then continue
   - cancel should stop cleanly without inventing extra turns
4. Only after that should you inspect Restate mechanics such as signal wiring, virtual-object state, or worker lifecycle.

## Strong Signals

- If the brain harness fails the same lifecycle assertion, the bug is probably in shared brain/pipeline logic, not Restate.
- If the brain harness passes and Restate fails before the expected persisted event exists, the bug is probably in Restate workflow control flow, signal delivery, or virtual-object boundaries.
- If both persist the same events but UI/runtime behavior differs, the bug is probably in runtime-event translation or observation plumbing.
- If only restart or worker-recovery tests fail, focus on Restate's replay and workflow-resume semantics rather than normal turn execution.

## Restate-Specific Places To Inspect

- `crates/moa-orchestrator/src/services/` - Restate service handlers
- `crates/moa-orchestrator/src/objects/` - virtual-object state machines
- `crates/moa-orchestrator/src/workflows/` - workflow definitions (TurnExecution, SubAgentTurnExecution, Consolidate)
- `crates/moa-orchestrator/src/turn/` - turn-level orchestration
- `crates/moa-orchestrator/src/brain_bridge.rs` - the bridge that compiles one turn via the brain pipeline
- `docs/12-restate-architecture.md` - the Restate architecture deep dive
- `docs/implementation-caveats.md` - already-known caveats

## Exact Test Targets Worth Using

```bash
# brain harness (drives the pipeline directly)
cargo test -p moa-brain --test brain_turn_db -- --test-threads=1

# Restate, scoped to the surface that changed
cargo test -p moa-orchestrator --test session_vo -- --test-threads=1
cargo test -p moa-orchestrator --test replay_determinism -- --test-threads=1
cargo test -p moa-orchestrator --test tool_executor -- --test-threads=1
```

If a target does not exist, list `crates/moa-brain/tests/` or `crates/moa-orchestrator/tests/` to find the actual current names.

## What Good Evidence Looks Like

- a brain-harness event sequence and a Restate event sequence for the same scenario
- the first point where Restate stops, duplicates, or skips lifecycle progress
- any matching approval request id, queued message text, or tool id needed to prove ordering
- if available, matching `session_turn` or `tool_execution` spans that show whether the turn stalled before or after persistence

## Determinism Risk

Restate replays workflow code on resume. If the workflow code is non-deterministic (reads system clock outside of `ctx.run`, generates UUIDs without seeding, iterates a `HashMap` where order matters), replay produces a different durable step sequence than the first run. This shows up as silent divergence after a worker restart. When investigating restart or recovery issues, audit the workflow code for these specific patterns before suspecting infrastructure.
