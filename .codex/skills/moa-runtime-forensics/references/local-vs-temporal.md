# Local Versus Temporal

Use this when the same user-visible flow passes on `LocalOrchestrator` and fails
on `TemporalOrchestrator`, or when Temporal-only approval, restart, or worker
recovery behavior looks wrong.

## First Principle

Do not start by diffing the adapters line-by-line. Start by proving whether the
shared lifecycle contract is intact on both sides.

The shared harness lives in:

- `moa-orchestrator/tests/support/orchestrator_contract.rs`

The two main adapter suites live in:

- `moa-orchestrator/tests/local_orchestrator.rs`
- `moa-orchestrator/tests/temporal_orchestrator.rs`

## Classification Flow

1. Run the nearest shared-lifecycle assertions or exact adapter tests on both backends.
2. Compare persisted session events for the same scenario, not just stdout or test assertions.
3. Find the first missing or reordered lifecycle edge:
   - blank session should wait for the first message
   - queued messages should remain FIFO
   - approval should persist, pause, resume, then continue
   - cancel should stop cleanly without inventing extra turns
4. Only after that should you inspect adapter mechanics such as Temporal wait conditions, signal wiring, or worker lifecycle.

## Strong Signals

- If Local and Temporal both fail the same shared contract assertion, the bug is probably in shared lifecycle logic or the common harness.
- If Local passes and Temporal fails before the expected persisted event exists, the bug is probably in Temporal workflow control flow, signal delivery, or activity boundaries.
- If both persist the same events but UI/runtime behavior differs, the bug is probably in runtime-event translation or observation plumbing.
- If only restart or worker-recovery tests fail, focus on replay and workflow-resume semantics rather than normal turn execution.

## Temporal-Specific Places To Inspect

- `moa-orchestrator/src/temporal.rs`
- `docs/implementation-caveats.md`
- `moa-orchestrator/examples/temporal_worker_helper.rs`

Known caveats already documented:

- approval resume had a real wait-condition bug in the Temporal loop
- child workflows are still modeled as top-level workflows
- worker lifetime is process-scoped and not gracefully stoppable

These caveats do not prove the current bug, but they tell you where Temporal
drift has already happened before.

## Exact Test Targets Worth Using

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --test local_orchestrator -- --test-threads=1
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator -- --test-threads=1
```

Manual Temporal-only recovery and live tests:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator temporal_orchestrator_runs_workflow_and_unblocks_on_approval -- --ignored --exact --nocapture
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator temporal_orchestrator_recovers_after_worker_process_restart -- --ignored --exact --nocapture
MOA_RUN_LIVE_PROVIDER_TESTS=1 PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator temporal_live_providers_complete_tool_approval_roundtrip_when_available -- --ignored --exact --nocapture
```

## What Good Evidence Looks Like

- a Local event sequence and a Temporal event sequence for the same scenario
- the first point where Temporal stops, duplicates, or skips lifecycle progress
- any matching approval request id, queued message text, or tool id needed to prove ordering
- if available, matching `session_turn` or `tool_execution` spans that show whether the turn stalled before or after persistence
