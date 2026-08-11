---
name: moa-audit
description: >
  Use this skill for broad MOA repository audits and reviews that need current-state
  investigation, architecture or modularity analysis, diff/debt review, simplification
  recommendations, or a safe path from findings to implementation. It coordinates
  bounded parallel read-only review, durable evidence and planning, explicit
  confirmed/adjusted/refuted findings, and independent final verification. Triggers
  include: "audit the architecture", "review the diff", "find unnecessary code",
  "simplify this", "look for gaps", "is this the right long-term design", "work in
  parallel using subagents", and "make a plan then fix it". Do NOT use it for one
  failing runtime incident (use `runtime-forensics`), a Rust-only implementation
  (use `rust`), a memory-pack step (use `memory-pack`), provider integration (use
  `provider-integration`), individual test authoring (use `test-authoring`), or
  release/regression test selection (use `certify`).
allowed-tools:
  - Read
  - Grep
  - Glob
  - Edit
  - Write
  - Bash(rg:*)
  - Bash(git:*)
  - Bash(cargo:*)
  - Bash(./scripts/codegraph:*)
metadata:
  moa-tags: "audit, architecture, modularity, simplification, debt, parallel-review, planning"
---

# MOA Audit

Use this skill to answer: what is true in the current checkout, what is actually
wrong or unnecessarily complex, and what is the smallest safe next change?

## Route the request

- Use `audit` for broad architecture, modularity, dependency, debt, skill, or
  diff review.
- Use `plan` when the user wants findings turned into an executable action plan.
- Use `remediate` only when the user authorizes fixes; hand implementation to the
  narrow owning skill and return here for integration review.
- Use `current-state` when the user asks what exists now or whether a proposed
  abstraction is needed.

Hand off narrow work instead of duplicating it:

- Rust implementation or refactor: `rust`.
- `sequence/memory-pack` steps: `memory-pack`.
- Provider, MCP, hand, or channel integration: `provider-integration`.
- Runtime/replay/analytics incident: `runtime-forensics`.
- Test design or test additions: `test-authoring`.
- Post-change regression or release validation: `certify`.

## Establish current state

1. Inspect the live files and worktree before relying on an old diff or plan:
   `git status --short --branch`, `git diff --stat`, and focused `rg`/file reads.
   Preserve unrelated dirty or untracked work; never reset broadly.
2. If `.codegraph/` exists, use the repository CodeGraph explorer first for
   ownership and call-path questions, then verify important claims in source.
3. Read the matching architecture document before judging a subsystem. Treat
   `docs/01-architecture-overview.md` and the relevant subsystem document as
   contracts, not optional background.
4. Map the requested surface: owners, callers, persistence, tests, deployment
   or live prerequisites, and the exact files a change would touch.

When reviewing a diff, distinguish staged, unstaged, and untracked changes. When
reviewing a plan, verify every named path and command against the current source;
do not promote stale plan text into a finding.

## Parallel review

For a broad request, use independent read-only reviewers when available. Give
each reviewer one non-overlapping question and the smallest necessary context:

1. Contract/architecture: ownership, invariants, API and doc alignment.
2. Correctness/security: failure modes, authorization, isolation, replay and
   data-loss risks.
3. Reuse/simplicity: dead paths, duplicate state, unnecessary abstractions,
   allocations, and YAGNI opportunities.
4. Verification/operations: tests, live lanes, migrations, deployment,
   observability, and external prerequisites.

Do not let reviewers edit during the audit. Reconcile their reports against
source and each other; missing, timed-out, or contradictory reviewer output is
an unresolved verification gap, not a pass.

## Record evidence and classify findings

For each material finding, record:

- exact file and line or symbol;
- observable failure or unnecessary behavior;
- affected path and blast radius;
- source, test, runtime, or plan evidence;
- smallest fix and its verification;
- classification: `confirmed`, `adjusted`, `refuted`, `deferred`, or
  `external-prereq-missing`.

Separate verified defects from roadmap ideas. Do not call broad grep counts,
style preferences, incomplete worker reports, or unverified provider assumptions
bugs. For deletion or simplification, prove the path is unused or redundant by
checking callers, persistence, tests, docs, manifests, and runtime registration.

## Plan before remediation

For work with three or more steps, keep `task_plan.md`, `findings.md`, and
`progress.md` in the repository root. If the user requests a handoff-ready
implementation plan, also write
`docs/engineering-discipline/plans/YYYY-MM-DD-<feature>.md` with exact files,
dependencies, acceptance criteria, commands, and a final verification task.

The plan must state:

- in-scope and out-of-scope files;
- whether the pass is read-only or authorized to edit;
- disjoint write sets and task dependencies;
- deterministic, database, live, and billed checks separately;
- how unrelated dirty-worktree changes will be preserved.

When breaking changes are explicitly allowed, prefer direct removal and one
canonical path over compatibility aliases, dual reads, or wrapper shims. Keep
the change at the owning seam and do not expand a confirmed fix into speculative
architecture.

## Remediate and validate

When authorized to implement:

1. Convert each confirmed finding into a narrow task with one owning write set.
2. Run independent tasks in parallel only when their files and state are
   disjoint. Validate each task against its acceptance criteria.
3. Re-read the integrated source, inspect the diff, and run an independent final
   pass after all workers finish. Worker completion is not integration completion.
4. Run the smallest deterministic checks first, then the relevant broader gate.
   Keep live/billed checks opt-in and classify missing Docker, Postgres,
   credentials, network, or service prerequisites separately from code failures.
5. Finish with `git diff --check` and a report that names changed files,
   behavior, verification, unresolved gaps, and deferred ideas.

Do not weaken assertions or declare success from compilation alone when the
behavior is observable through a real integration, eval, or live path.

## Report format

Return:

- `Scope and evidence window`
- `Current-state map`
- `Confirmed findings` with severity, path, impact, and fix
- `Adjusted/refuted/deferred findings`
- `Recommended changes` ranked by value and effort
- `Changes made` (or explicitly `read-only`)
- `Verification` and external-prerequisite gaps
- `Open follow-ups`
