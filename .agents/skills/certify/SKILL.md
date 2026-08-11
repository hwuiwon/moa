---
name: certify
description: >
  Use this skill when validating MOA changes before merge or release, especially for
  orchestrators (Restate orchestrator or direct brain harness), providers, approvals, session lifecycle, persistence,
  event schemas, memory and context pipeline, or skill/eval infrastructure. It selects
  the right deterministic and live test matrix, enforces shared orchestrator contract
  coverage, and helps localize regressions before shipping. Triggers include: "validate
  this change", "is this ready to merge", "run the right tests for X", "release-gate
  these changes", "verify this task", "refresh memory retrieval baseline", "compare
  retrieval quality/cost/latency", "validate query rewrite gating", and "live auth
  e2e". Do NOT use for memory-pack step implementation (use `memory-pack`),
  diagnosing a failing test (use `runtime-forensics`), authoring new tests (use
  `test-authoring`), or general Rust review (use `rust`).
allowed-tools:
  - Bash(cargo:*)
  - Bash(rg:*)
  - Bash(git:*)
  - Read
metadata:
  moa-tags: "validation, regression, release, orchestrator, provider, restate"
---

# Certify

Use this skill to answer one question: did this change break anything important, and if it did, where?

The default stance is:

- deterministic suites first
- live and provider checks second
- shared orchestrator contract before adapter-specific behavior
- smallest matrix that still covers the risk

## When To Use

Use this skill when a change touches any of the following:

- Restate orchestrator (`moa-orchestrator`) behavior
- session lifecycle, approvals, queued messages, cancellation, replay, or recovery
- provider request/response parsing, model catalogs, pricing, caching, tool calls, or web search
- session store, event schema, analytics, migrations, or generated aggregates
- memory or context pipeline behavior
- memory-retrieval baselines, query-rewrite gating, retrieval quality/cost/latency comparisons, or live memory-eval lanes
- skills distillation, eval wiring, or skill regression suites
- anything being prepared for merge or release that needs a regression gate

## Boundary

This skill owns validation strategy and failure localization at the test-matrix level.

It does not own:

- implementation planning for memory-pack sequence steps; use `memory-pack`, then return here
- authoring or extending individual tests; use `test-authoring`
- diagnosing why a specific failing test fails; hand off to `runtime-forensics` after triage
- general Rust quality review; use `rust`
- broad repository, architecture, modularity, debt, or diff audits; use `moa-audit`

## Modes

- `quick`: changed crate plus the nearest deterministic suite
- `task-validation`: validate a specific task or plan step, separating task acceptance from unrelated dirty-worktree or broader suite failures
- `certify`: deterministic matrix for the affected surface
- `release`: `certify` plus live and provider checks when prerequisites exist
- `triage`: failure localization and artifact collection before handoff to `runtime-forensics`

## First Map The Change

Read only the matching docs before choosing commands:

- `docs/02-brain-orchestration.md` for orchestrators or approvals
- `docs/12-restate-architecture.md` for Restate virtual objects, services, or workflow flow
- `docs/05-session-event-log.md` for events, replay, persistence, analytics, or compaction
- `docs/07-context-pipeline.md` for prompt layout, cache planning, or memory injection
- `docs/09-skills-and-learning.md` for skill distillation, improvement, or eval
- `docs/16-evaluation.md` for eval runners, score targets, memory-retrieval reports, and budget gates

Then load only the relevant reference file:

- `references/test-matrix.md` for what to run
- `references/failure-triage.md` for how to localize a failure before handoff
- `references/memory-eval-validation.md` for memory-retrieval baselines, paired compares, query-rewrite gating validation, and live-lane artifacts

## Workflow

1. Identify the change surface and choose `quick`, `certify`, `release`, or `triage`.
2. Run baseline hygiene first: formatting, then clippy on the touched crates or the workspace gate.
3. Run the smallest deterministic matrix that still covers the changed surface.
4. If orchestrator behavior changed, run the shared contract path before adapter-specific tests.
5. If provider request shape, approval flow, or live behavior changed and credentials exist, run the live matrices.
6. If anything fails, switch to `triage` mode, classify the failure by layer, and hand off to `runtime-forensics` for deep diagnosis if it cannot be localized in two passes.
7. End with a short certification summary:
   - scope
   - validation status
   - commands run
   - pass/fail by layer
   - gaps not covered
   - ship / do-not-ship recommendation

## Task Validation Statuses

When validating one plan task or worker output, report one of these statuses instead of a generic pass/fail:

- `task-pass`: task acceptance criteria and focused checks passed.
- `task-pass-but-suite-dirty`: task acceptance passed, but broader checks failed for a pre-existing, unrelated, or dirty-worktree reason.
- `acceptance-failure`: the requested behavior, artifact, or task-local check is missing or wrong.
- `focused-test-failure`: the nearest deterministic test for the changed surface failed.
- `release-gate-failure`: focused task checks passed, but a broader required matrix failed.
- `external-prereq-missing`: required Docker, Postgres, Restate, credentials, network, or live service prerequisite is absent.

Do not mark a task failed solely because an unrelated workspace gate is red. Preserve the distinction and recommend the next smallest check.

## Live and Billed Test Discipline

Live tests that call paid APIs or external infrastructure must require two gates:

- the Rust test is marked `#[ignore = "..."]`
- an explicit opt-in env flag is set for that provider or service

Known live opt-in flags:

| Surface | Required opt-in | Credential / prerequisite |
|---|---|---|
| Cohere Embed/Rerank | `MOA_RUN_LIVE_COHERE_TESTS=1` | `COHERE_API_KEY` or `MOA_COHERE_API_KEY` |
| Local orchestrator live providers | `MOA_RUN_LIVE_PROVIDER_TESTS=1` | provider env vars such as `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, or Google credentials |
| Provider matrix | test-specific ignored run | provider env vars |
| PII sidecar | ignored test run | `docker compose up -d moa-pii-service`, optional `MOA_PII_SERVICE_URL` |

When a user provides a temporary key, do not write it to a file or include it in command text. Prefer a TTY `read -rs` prompt or stdin injection, export it only inside that short-lived shell, and avoid echoing it in output.

Compile ignored live tests without opt-in when changing their code. Run the explicit live path only when the user asks for live validation or the release mode requires it.

## Rules

- Do not treat brain-harness green as Restate green; durable workflow behavior needs orchestrator suites.
- Prefer exact test targets over broad ignored-test sweeps.
- If live provider credentials are available, do not ship provider request-shape changes without at least one live check.
- Do not run billed live tests merely because `--ignored` was requested; require the matching opt-in flag too.
- If a new orchestrator backend is added, make it implement the shared contract harness before writing large adapter-specific e2e tests.
- On macOS dev machines, prefer `PROTOC=/opt/homebrew/bin/protoc` if the default `protoc` is invalid.

## Output Format

Use this structure when reporting results:

- `Scope`: what changed
- `Deterministic`: what passed and failed
- `Live`: what passed and failed
- `Fault Domain`: shared lifecycle, adapter, provider, persistence, tooling, or observability
- `Release Risk`: low, medium, or high with one sentence
