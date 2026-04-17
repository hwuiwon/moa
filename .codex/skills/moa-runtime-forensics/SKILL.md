---
name: moa-runtime-forensics
description: >
  Use this when diagnosing MOA runtime regressions, Local-versus-Temporal drift,
  approval deadlocks, replay/recovery issues, event-log inconsistencies, or
  analytics/trace mismatches. It helps correlate persisted session events,
  runtime behavior, traces, and SQL analytics so the failure is localized before
  patching.
compatibility: Rust 2024 MOA workspace with cargo; Postgres-backed session store; Temporal CLI optional; live provider env vars optional
allowed-tools:
  - Bash(cargo:*)
  - Bash(rg:*)
  - Bash(git:*)
  - Bash(psql:*)
  - Bash(temporal:*)
  - Read
metadata:
  moa-tags: "debugging, tracing, observability, temporal, replay, analytics"
  moa-one-liner: "Runtime forensics workflow for reconstructing MOA failures from events, traces, and analytics"
---

# MOA Runtime Forensics

Use this skill to answer one question: what actually happened in this run, and
where did it diverge from the expected lifecycle?

The default stance is:

- reproduce the symptom exactly
- capture durable evidence before editing code
- find the earliest divergence, not the loudest symptom
- separate shared lifecycle bugs from adapter-only bugs

## When To Use

Use this skill when the problem looks like any of the following:

- Local and Temporal disagree on session behavior
- approvals stall, resume incorrectly, or skip queued work
- replay, recovery, or restart behavior differs from a fresh run
- session events, runtime events, traces, and final status disagree
- analytics views or cache-hit numbers disagree with the underlying event log
- tool results exist but the turn never finishes
- a live/provider test fails and you need to prove whether the issue is provider, adapter, or persistence

## Modes

- `session`: reconstruct one session end-to-end from persisted events and current status
- `adapter-diff`: compare the same scenario across Local and Temporal
- `trace`: inspect latency spans, runtime events, and provider/tool timing
- `analytics`: cross-check triggers, generated columns, views, and materialized views against raw events
- `recovery`: focus on replay, worker restart, or approval resume behavior

## First Map The Symptom

Read only the matching docs before choosing commands:

- `docs/02-brain-orchestration.md` for lifecycle, approvals, Local, or Temporal
- `docs/05-session-event-log.md` for persisted events, replay, and recovery
- `docs/11-event-replay-runbook.md` for replay-cost and event-fetch instrumentation
- `docs/observability/turn-latency.md` for `session_turn` span interpretation
- `docs/analytics.md` for generated columns, triggers, views, and refresh behavior
- `docs/implementation-caveats.md` when the issue smells Temporal-specific

Then load only the relevant reference file:

- `references/evidence-checklist.md` for what to capture first
- `references/local-vs-temporal.md` for adapter drift and approval/restart issues
- `references/analytics-and-traces.md` for event-log versus trace versus SQL checks

## Workflow

1. Reproduce with the smallest exact test, CLI command, or live scenario that still shows the bug.
2. Record the exact command, feature flags, orchestrator type, provider, and environment assumptions.
3. Pull durable evidence first: persisted session status, event log, and analytics rows.
4. If the symptom is adapter drift, run the shared orchestrator contract path before backend-specific tests.
5. Correlate the four planes of truth:
   - persisted events
   - current session status and analytics views
   - runtime events or queue/approval behavior
   - trace spans and latency attributes
6. Identify the earliest point where the bad run differs from the expected lifecycle.
7. Patch only after the fault domain is clear, then rerun the minimal repro and hand off to `moa-certify` for regression coverage.

## Rules

- Persisted session events are the durable source of truth for what happened.
- Runtime events are transient; use them to explain UX behavior, not to override the event log.
- Traces explain timing and span boundaries; they do not prove persistence correctness on their own.
- If analytics disagree with the event log, trust the event log first and then inspect the trigger, generated columns, or refresh path.
- If Local passes and Temporal fails, do not assume provider behavior is the cause until the shared contract and persisted events say so.
- Refresh materialized views before treating them as evidence.
- On this machine, prefer `PROTOC=/opt/homebrew/bin/protoc` when Temporal or cloud-feature builds touch protobuf.

## Output Format

Use this structure when reporting results:

- `Symptom`: what failed and where it appeared
- `Repro`: exact command or scenario
- `Earliest Divergence`: the first observable mismatch
- `Evidence`: events, traces, analytics, or status checks that prove it
- `Fault Domain`: shared lifecycle, adapter, provider, persistence, analytics, or observability
- `Next Check`: the smallest verification that should go green after the fix
