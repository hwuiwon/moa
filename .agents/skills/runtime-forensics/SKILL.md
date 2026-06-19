---
name: runtime-forensics
description: >
  Use this skill when diagnosing MOA runtime regressions, approval deadlocks,
  replay or recovery issues, event-log inconsistencies, or analytics/trace
  mismatches, compose/Postgres contamination, or observability source-of-truth
  disputes. It correlates persisted session events, runtime behavior, traces,
  SQL analytics, and local service state so the failure is localized before patching. Triggers
  include: "session stuck in Running", "approval did not resume", "the cache hit
  numbers are wrong", "trace shows X but the session shows Y", "this only fails
  after a worker restart". Do NOT use for selecting which tests to run (use
  `certify`), implementing the fix (use `rust` or `memory-pack`), or authoring
  new tests (use `test-authoring`).
allowed-tools:
  - Bash(cargo:*)
  - Bash(rg:*)
  - Bash(git:*)
  - Bash(psql:*)
  - Read
metadata:
  moa-tags: "debugging, tracing, observability, restate, replay, analytics"
  moa-one-liner: "Runtime forensics workflow for reconstructing MOA failures from events, traces, and analytics"
---

# Runtime Forensics

Use this skill to answer one question: what actually happened in this run, and where did it diverge from the expected lifecycle?

The default stance is:

- reproduce the symptom exactly
- capture durable evidence before editing code
- find the earliest divergence, not the loudest symptom
- separate shared lifecycle bugs from adapter-only bugs

## When To Use

Use this skill when the problem looks like any of the following:

- the brain harness (`moa-brain`) and the Restate orchestrator (`moa-orchestrator`) disagree on session behavior
- approvals stall, resume incorrectly, or skip queued work
- replay, recovery, or restart behavior differs from a fresh run
- session events, runtime events, traces, and final status disagree
- analytics views or cache-hit numbers disagree with the underlying event log
- local Postgres, compose services, fixture data, or global rows contaminate deterministic tests
- a dashboard or audit claim might double-count durable events, spans, and metrics
- tool results exist but the turn never finishes
- a live or provider test fails and you need to prove whether the issue is provider, adapter, or persistence

## Boundary

This skill diagnoses; it does not fix.

Once the fault domain is clear, hand off to:

- `rust` for code changes outside the memory-pack scope
- `memory-pack` for memory-pack step implementation
- `provider-integration` for changes to a provider adapter, model catalog, or credential routing
- `certify` for regression coverage after the fix

This skill also does not own pre-merge test selection. If you arrived here without a confirmed symptom, use `certify` first.

## Modes

- `session`: reconstruct one session end-to-end from persisted events and current status
- `adapter-diff`: compare the same scenario across the brain harness and Restate
- `trace`: inspect latency spans, runtime events, and provider/tool timing
- `analytics`: cross-check triggers, generated columns, views, and materialized views against raw events
- `compose-db`: inspect local Docker Compose/Postgres state, shared fixture contamination, and cleanup needs
- `recovery`: focus on replay, worker restart, or approval resume behavior

## First Map The Symptom

Read only the matching docs before choosing commands:

- `docs/02-brain-orchestration.md` for lifecycle, approvals, and Restate orchestration
- `docs/12-restate-architecture.md` for Restate virtual objects, services, signal flow, and worker behavior
- `docs/05-session-event-log.md` for persisted events, replay, and recovery
- `docs/11-event-replay-runbook.md` for replay-cost and event-fetch instrumentation
- `docs/observability/turn-latency.md` for `session_turn` span interpretation
- `docs/analytics.md` for generated columns, triggers, views, and refresh behavior
- `docs/implementation-caveats.md` when the issue smells adapter-specific

Then load only the relevant reference file:

- `references/evidence-checklist.md` for what to capture first
- `references/local-vs-restate.md` for brain-harness-vs-Restate drift and approval/restart issues
- `references/analytics-and-traces.md` for event-log versus trace versus SQL checks

## Workflow

1. Reproduce with the smallest exact test, API request, or live scenario that still shows the bug.
2. Record the exact command, feature flags, orchestrator type, provider, and environment assumptions.
3. Pull durable evidence first: persisted session status, event log, and analytics rows.
4. If the symptom is adapter drift, run the shared orchestrator contract path before backend-specific tests.
5. Correlate the four planes of truth:
   - persisted events
   - current session status and analytics views
   - runtime events or queue/approval behavior
   - trace spans and latency attributes
6. Identify the earliest point where the bad run differs from the expected lifecycle.
7. Hand off:
   - to `rust` or `memory-pack` for the fix
   - to `certify` for regression coverage after the fix
   - back to `test-authoring` if a new permanent test is needed to prevent recurrence

## Compose/Postgres Contamination Playbook

Use this when tests pass alone but fail after other suites, when global memory rows leak across scenarios, or when local service state is uncertain.

1. Check stack state first with `docker compose ps` and, when needed, `docker ps --filter 'name=moa'`.
2. Record `MOA_DATABASE_URL`, database name, schema assumptions, and whether SQLx offline data or live Postgres is being used.
3. Inspect shared tables that commonly outlive a single test, especially graph-memory indexes such as `moa.node_index` and any global/workspace rows.
4. Prefer isolated schema/database setup or explicit fixture cleanup over weakening assertions.
5. If the stack was started only for diagnosis, stop it with `docker compose down` and verify no MOA containers remain.

## Observability Source Of Truth

Choose one canonical source for each claim before debugging dashboards:

- Durable session state and user-visible lifecycle claims come from persisted events first.
- Runtime events explain transient delivery, queueing, and approval behavior.
- Traces explain timing, span boundaries, and attribution, but do not prove persistence correctness.
- Metrics and dashboards are derived evidence; check their query, aggregation, and refresh path before treating them as authoritative.

Avoid behavioral dashboards that double-count the same action through durable events, brain tool spans, hand tool spans, and runtime metrics.

## Rules

- Persisted session events are the durable source of truth for what happened.
- Runtime events are transient; use them to explain UX behavior, not to override the event log.
- Traces explain timing and span boundaries; they do not prove persistence correctness on their own.
- If analytics disagree with the event log, trust the event log first and then inspect the trigger, generated columns, or refresh path.
- If the brain harness passes and Restate fails, do not assume provider behavior is the cause until persisted events say so.
- If a deterministic test fails only after another suite, suspect shared Postgres or fixture contamination before changing production logic.
- Refresh materialized views before treating them as evidence.
- On macOS dev machines, prefer `PROTOC=/opt/homebrew/bin/protoc` when builds touch protobuf.

## Output Format

Use this structure when reporting results:

- `Symptom`: what failed and where it appeared
- `Repro`: exact command or scenario
- `Earliest Divergence`: the first observable mismatch
- `Evidence`: events, traces, analytics, or status checks that prove it
- `Fault Domain`: shared lifecycle, adapter, provider, persistence, analytics, or observability
- `Next Check`: the smallest verification that should go green after the fix
- `Handoff`: which skill picks up next
