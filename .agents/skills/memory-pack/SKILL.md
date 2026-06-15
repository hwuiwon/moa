---
name: memory-pack
description: >
  Use this skill when implementing or reviewing `sequence/memory-pack` steps such
  as M01-M30 graph memory, RLS, AGE, pgvector, ingestion, retrieval, privacy, and
  migration cleanup in the MOA workspace. It owns the step-by-step graph-memory
  migration workflow and the rules around RLS, ScopedConn, Cypher safety, and
  hard-break migrations. Triggers include: "implement M07", "graph-memory step",
  "ingest the new schema", "fix the RLS policy", "AGE Cypher pattern", "pgvector
  migration". Do NOT use for eval baseline refreshes, query-rewrite policy or
  gating research, retrieval ranking scorecards, live memory-eval lanes, general
  Rust refactors outside memory-pack scope (use `rust`), release certification
  (use `certify`), runtime incident diagnosis (use `runtime-forensics`), or test
  authoring (use `test-authoring`).
allowed-tools:
  - Read
  - Grep
  - Glob
  - Edit
  - Write
  - Bash(rg:*)
  - Bash(cargo:*)
  - Bash(git:*)
metadata:
  moa-tags: "memory-pack, graph-memory, migrations, retrieval, ingestion, rls, pgvector, age"
  moa-one-liner: "Implementation workflow for sequence/memory-pack graph-memory steps"
---

# Memory Pack

Use this skill for implementing the `sequence/memory-pack` prompts. It owns the step-by-step graph-memory migration workflow.

## Boundary

Use this skill for:

- `M01`-style `MemoryScope` and graph-memory type changes
- Postgres / RLS / AGE / pgvector / changelog migrations under `crates/moa-memory/`
- `moa-memory/graph`, `moa-memory/vector`, `moa-memory/pii`, and `moa-memory/ingest` sequence work
- hybrid retrieval, query planning, read-time cache, and cleanup only when the task is a memory-pack implementation step or direct graph-memory internals change
- translating memory-pack prompt paths and acceptance criteria into this repo

Do not use this skill for:

- memory-retrieval eval baseline refreshes, memory-eval reports, budget gates, or live-lane accounting; use `certify` for validation until a dedicated memory-eval skill exists
- query-rewrite policy research, gating decisions, prompt-cache ordering, or retrieval scorecard tuning unless the implementation task explicitly changes memory-pack internals
- generic Rust refactors outside memory-pack scope; use `rust`
- release certification or live-test matrix selection; use `certify`
- runtime incident diagnosis; use `runtime-forensics`
- adding a new provider implementation; use `provider-integration`
- authoring tests; use `test-authoring`

## Required Orientation

1. Read `AGENTS.md`.
2. Read the doc file that matches the step:
   - memory architecture: `docs/04-memory-architecture.md`
   - orchestration and Restate: `docs/02-brain-orchestration.md`
   - event log and Postgres persistence: `docs/05-session-event-log.md`
   - context and retrieval pipeline: `docs/07-context-pipeline.md`
   - security, privacy, RLS implications: `docs/08-security.md`
   - skills graph work: `docs/09-skills-and-learning.md`
3. Inspect existing code before editing. Prefer local patterns over the prompt's sketch when they differ.

## Path Translation

Memory-pack prompts may say `crates/<name>/...`. Use the actual workspace path:

- `crates/moa-core/...`
- `crates/moa-brain/...`
- `crates/moa-memory/graph/...`
- `crates/moa-memory/vector/...`
- `crates/moa-memory/pii/...`
- `crates/moa-memory/ingest/...`

Search exact top-level crates first. Avoid failing broad searches against a non-existent `crates/` layout that does not match this repo.

## Implementation Rules (Quick Reference)

The dense rules are in references, loaded only when relevant:

- [references/rls-and-scoped-conn.md](references/rls-and-scoped-conn.md) for RLS, `ScopedConn`, and scoped GUC handling
- [references/age-cypher-patterns.md](references/age-cypher-patterns.md) for AGE Cypher safety, parameter binding, and projection helpers
- [references/migration-rules.md](references/migration-rules.md) for hard-break vs compatibility, deprecation policy, and SQL helper conventions

The five rules to keep in mind without loading a reference:

- No backwards compatibility unless the prompt explicitly requests it.
- User-scoped memory is always workspace-bound.
- Tool names use underscores, not dotted names.
- For RLS work, use `FORCE ROW LEVEL SECURITY`; app paths must not use `BYPASSRLS`.
- For AGE Cypher work, do not format user input into Cypher strings.

## Execution Sequence

1. Map the prompt's deliverables to actual files.
2. Run targeted `rg` searches for affected symbols and match sites.
3. Read the current implementation around each match before editing.
4. Make the smallest hard-break implementation that satisfies the step.
5. Add focused deterministic tests in the owning crate.
6. If adding live or billed behavior, add ignored tests gated by an explicit env flag.
7. Run:
   - `cargo fmt --all`
   - focused tests for the changed crate
   - `cargo clippy -p <crate> --all-targets --all-features --locked -- -D warnings`
   - `cargo build --workspace` when public APIs or shared crates changed
   - `git diff --check`
8. Hand off to `certify` when choosing broader release or live validation.

## Reporting

In the final response, include:

- files changed
- the memory-pack behavior landed
- deterministic tests run
- live tests run or intentionally skipped
- any prompt acceptance criteria not covered, with a concrete reason
