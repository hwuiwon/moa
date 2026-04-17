# Evidence Checklist

Start here before changing code. The goal is to preserve enough evidence to
answer whether the failure lives in shared lifecycle logic, an orchestrator
adapter, provider translation, persistence, or analytics.

## Always Capture

- exact failing command, including `--ignored`, `--exact`, and feature flags
- orchestrator type: `Local` or `Temporal`
- provider and model
- whether the failure is deterministic, live-only, or restart/recovery-specific
- the session id when one exists
- the final persisted status

## Deterministic Repro Commands

Use the smallest exact test target that still reproduces:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --test local_orchestrator -- --test-threads=1
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator -- --test-threads=1
```

For live/provider lifecycle failures:

```bash
MOA_RUN_LIVE_PROVIDER_TESTS=1 PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --test live_provider_roundtrip live_providers_complete_tool_approval_roundtrip_when_available -- --ignored --exact --nocapture
MOA_RUN_LIVE_PROVIDER_TESTS=1 PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --features temporal --test temporal_orchestrator temporal_live_providers_complete_tool_approval_roundtrip_when_available -- --ignored --exact --nocapture
```

For trace and latency evidence:

```bash
PROTOC=/opt/homebrew/bin/protoc cargo test -p moa-orchestrator --test live_observability live_observability_audit_tracks_cache_replay_and_latency -- --ignored --exact --nocapture
```

## Session-Level Reads

When a repro yields a session id, collect both the row-level summary and the raw
events.

Use the CLI for the fast operational view:

```bash
cargo run -p moa-cli -- session stats <session-id>
cargo run -p moa-cli -- tool stats
cargo run -p moa-cli -- workspace stats --days 30
cargo run -p moa-cli -- cache stats --days 30
```

If you need the raw event log, prefer querying the store or test harness path
already used by the failing test. The key question is whether the expected event
was persisted at all.

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

- use `local-vs-temporal.md` when the two orchestrators disagree
- use `analytics-and-traces.md` when the disagreement is between SQL rollups, session events, and spans
