---
name: moa-certify
description: >
  Use this when validating MOA changes before merge or release, especially for
  orchestrators, providers, approvals, session lifecycle, persistence, event
  schemas, memory/context pipeline, or skill/eval infrastructure. It selects
  the right deterministic and live test matrix, enforces shared orchestrator
  contract coverage, and helps localize regressions before shipping.
compatibility: Rust 2024 MOA workspace with cargo; Temporal CLI optional; live provider env vars optional
allowed-tools:
  - Bash(cargo:*)
  - Bash(rg:*)
  - Bash(git:*)
  - Read
metadata:
  moa-tags: "validation, regression, release, orchestrator, provider, temporal"
  moa-one-liner: "Certification workflow for MOA changes, with deterministic gates first and live checks where needed"
---

# MOA Certify

Use this skill to answer one question: did this change break anything important, and if it did, where?

The default stance is:

- deterministic suites first
- live/provider checks second
- shared orchestrator contract before adapter-specific behavior
- smallest matrix that still covers the risk

## When To Use

Use this skill when a change touches any of the following:

- Local or Temporal orchestrator behavior
- session lifecycle, approvals, queued messages, cancellation, replay, or recovery
- provider request/response parsing, model catalogs, pricing, caching, tool calls, or web search
- session store, event schema, analytics, migrations, or generated aggregates
- memory/context pipeline behavior
- skills distillation, eval wiring, or skill regression suites
- anything being prepared for merge or release that needs a regression gate

## Boundary

This skill owns validation strategy and failure localization. It does not own implementation planning for memory-pack sequence steps; use `moa-memory-pack` for those changes, then return here for certification.

## Modes

- `quick`: changed crate plus nearest deterministic suite
- `certify`: deterministic matrix for the affected surface
- `release`: `certify` plus live/provider and ignored/manual flows when prerequisites exist
- `triage`: failure localization and artifact collection

## First Map The Change

Read only the matching docs before choosing commands:

- `docs/02-brain-orchestration.md` for orchestrators, approvals, or Temporal
- `docs/05-session-event-log.md` for events, replay, persistence, analytics, or compaction
- `docs/07-context-pipeline.md` for prompt layout, cache planning, or memory injection
- `docs/09-skills-and-learning.md` for skill distillation, improvement, or eval

Then load only the relevant reference file:

- `references/certification-matrix.md` for what to run
- `references/failure-triage.md` for how to localize a failure
- `references/new-orchestrator.md` when adding a backend

## Workflow

1. Identify the change surface and choose `quick`, `certify`, `release`, or `triage`.
2. Run baseline hygiene first: formatting, then clippy on the touched crates or the workspace gate.
3. Run the smallest deterministic matrix that still covers the changed surface.
4. If orchestrator behavior changed, always run the shared contract path before backend-only tests.
5. If provider request shape, approval flow, or orchestrator live behavior changed and credentials exist, run the live matrices.
6. If anything fails, switch to `triage` mode and classify the failure by layer before patching.
7. End with a short certification summary:
   - scope
   - commands run
   - pass/fail by layer
   - gaps not covered
   - ship / do-not-ship recommendation

## Live And Billed Test Discipline

Live tests that call paid APIs or external infrastructure must require two gates:

- the Rust test is marked `#[ignore = "..."]`
- an explicit opt-in env flag is set for that provider or service

Known live opt-in flags:

| Surface | Required opt-in | Credential / prerequisite |
|---|---|---|
| Cohere Embed/Rerank | `MOA_RUN_LIVE_COHERE_TESTS=1` | `COHERE_API_KEY` or `MOA_COHERE_API_KEY` |
| Provider matrix | test-specific ignored run | provider env vars such as `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or Google credentials |
| PII sidecar | ignored test run | `docker compose up -d moa-pii-service`, optional `MOA_PII_SERVICE_URL` |
| Local live provider roundtrip | ignored test run | selected provider credential and model env |

When a user provides a temporary key, do not write it to a file or include it in command text. Prefer a TTY `read -rs` prompt or stdin injection, export it only inside that short-lived shell, and avoid echoing it in output.

Compile ignored live tests without opt-in when changing their code, then run the explicit live path only when the user asks for live validation or the release mode requires it.

## Rules

- Do not treat Local green as Temporal green.
- Do not duplicate lifecycle coverage across adapters when the shared contract can express it.
- Prefer exact test targets over broad ignored-test sweeps.
- If live provider credentials are available, do not ship provider request-shape changes without at least one live check.
- Do not run billed live tests merely because `--ignored` was requested; require the matching opt-in flag too.
- If a new orchestrator is added, make it implement the shared contract harness before writing large adapter-specific e2e tests.
- On this machine, prefer `PROTOC=/opt/homebrew/bin/protoc` if the default `protoc` is invalid.

## Output Format

Use this structure when reporting results:

- `Scope`: what changed
- `Deterministic`: what passed and failed
- `Live`: what passed and failed
- `Fault Domain`: shared lifecycle, adapter, provider, persistence, tooling, or observability
- `Release Risk`: low, medium, or high with one sentence
