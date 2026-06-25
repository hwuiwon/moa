# Evidence Checklist

Start here before changing code. The goal is to preserve enough evidence to answer whether the failure lives in brain/pipeline logic, the Restate orchestrator, provider translation, persistence, or analytics.

## Always Capture

- exact failing command, including `--ignored`, `--exact`, and feature flags
- which layer the repro exercises: brain harness (`moa-brain`) or Restate orchestrator (`moa-orchestrator`)
- provider and model
- whether the failure is deterministic, live-only, or restart/recovery-specific
- the session id when one exists
- the final persisted status

## Deterministic Repro Commands

Use the smallest exact test target that still reproduces:

```bash
# brain harness (drives the pipeline directly)
cargo test -p moa-brain --test brain_turn_db -- --test-threads=1

# Restate orchestrator (pick the suite that matches the change)
cargo test -p moa-orchestrator --test session_vo -- --test-threads=1
cargo test -p moa-orchestrator --test tool_executor -- --test-threads=1
cargo test -p moa-orchestrator --test replay_determinism -- --test-threads=1
```

For live or provider lifecycle failures:

```bash
MOA_RUN_LIVE_PROVIDER_TESTS=1 cargo test -p moa-brain --test live_harness -- --ignored --nocapture
```

If a target name is not present in the current `tests/` directory, list the directory and pick the closest current name.

## Session-Level Reads

When a repro yields a session id, collect both the row-level summary and the raw events.

Use the hosted analytics API for the fast operational view:

```bash
curl -X POST "$MOA_EDGE_URL/v1/analytics/session-stats" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  --data '{"session_id":"<session-id>"}'
curl -X POST "$MOA_EDGE_URL/v1/analytics/tool-stats" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  --data '{}'
curl -X POST "$MOA_EDGE_URL/v1/analytics/tenant-stats" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  --data '{"days":30}'
curl -X POST "$MOA_EDGE_URL/v1/analytics/cache-stats" \
  -H "Authorization: Bearer $MOA_API_KEY" \
  -H "Content-Type: application/json" \
  --data '{"days":30}'
```

If you need the raw event log, query the store or use the test harness path already used by the failing test. The key question is whether the expected event was persisted at all.

## What To Preserve From The Event Log

- whether `QueuedMessage` was written
- whether `ApprovalRequested` was written with the expected request id
- whether `ApprovalDecided` appears after the request
- whether `ToolCall` has a matching `ToolResult` or `ToolError`
- whether `BrainResponse` exists for the turn that appears stuck
- whether the final `SessionCompleted`, `SessionFailed`, or cancel-related state change was persisted

## Minimum Artifact Set

- failing command output with `--nocapture` when available
- session id and persisted status
- event sequence around the bad turn
- matching analytics rows when the issue involves counts, cost, or cache hit rate
- trace or span evidence when the issue involves latency, stalls, or missing boundaries

## Escalate To Another Reference

- use `local-vs-restate.md` when the brain harness and Restate disagree
- use `analytics-and-traces.md` when the disagreement is between SQL rollups, session events, and spans
