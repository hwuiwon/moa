---
name: moa-memory-pack
description: >
  Use this when implementing or reviewing sequence/memory-pack steps such as
  M01-M30 graph memory, RLS, AGE, pgvector, ingestion, retrieval, privacy,
  and migration cleanup. It keeps memory-pack work hard-break, docs-first,
  flat-workspace aware, and validated without overlapping with general Rust
  coding or certification skills.
compatibility: Rust 2024 MOA workspace with flat top-level crate directories
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

# MOA Memory Pack

Use this skill for implementing the `sequence/memory-pack` prompts. It owns the step-by-step graph-memory migration workflow. Use `moa-rust` for general Rust code quality rules and `moa-certify` for validation strategy.

## Boundary

Use this skill for:

- `M01`-style `MemoryScope` and graph-memory type changes
- Postgres/RLS/AGE/pgvector/changelog migrations
- `moa-memory-graph`, `moa-memory-vector`, `moa-memory-pii`, and `moa-memory-ingest` sequence work
- hybrid retrieval, query planning, read-time cache, and memory-pack cleanup
- translating memory-pack prompt paths and acceptance criteria into this repo

Do not use this skill for:

- generic Rust refactors outside memory-pack scope; use `moa-rust`
- release certification or live-test matrix selection; use `moa-certify`
- runtime incident diagnosis; use `moa-runtime-forensics`

## Required Orientation

1. Read `AGENTS.md`.
2. If `graphify-out/GRAPH_REPORT.md` exists, skim it before broad exploration.
3. Read the doc file that matches the step:
   - memory architecture: `docs/04-memory-architecture.md`
   - orchestration and Restate: `docs/02-brain-orchestration.md`
   - event log and Postgres persistence: `docs/05-session-event-log.md`
   - context/retrieval pipeline: `docs/07-context-pipeline.md`
   - security/privacy/RLS implications: `docs/08-security.md`
   - skills graph work: `docs/09-skills-and-learning.md`
4. Inspect existing code before editing. Prefer local patterns over the prompt's sketch when they differ.

## Path Translation

The memory-pack prompts often say `crates/<name>/...`. This repo uses flat top-level crate directories:

- `crates/moa-core/...` means `moa-core/...`
- `crates/moa-brain/...` means `moa-brain/...`
- `crates/moa-memory-graph/...` means `moa-memory-graph/...`

Search exact top-level crates first. Avoid failing broad searches against a non-existent `crates/` directory.

## Implementation Rules

- No backwards compatibility unless the prompt explicitly requests it.
- Delete obsolete wiki/vector/tool paths when the step says cleanup; do not leave shims to preserve old behavior.
- Do not introduce deprecated aliases, tuple-variant compatibility, or old JSON parsing shims for hard-break steps.
- User-scoped memory is always workspace-bound.
- Tool names use underscores, not dotted names.
- Prefer compact SQL helpers/templates over duplicated policy blocks when the user asks to clean up SQL.
- For RLS work, use `FORCE ROW LEVEL SECURITY`; app paths must not use `BYPASSRLS`.
- For scoped Postgres code, use `ScopedConn`/`ScopeContext` and set GUCs inside the transaction.
- For AGE/Cypher work, do not format user input into Cypher strings.
- For live/billed providers, add permanent ignored tests with explicit env opt-in flags.

## Execution Sequence

1. Map the prompt's deliverables to actual files.
2. Run targeted `rg` searches for affected symbols and match sites.
3. Read the current implementation around each match before editing.
4. Make the smallest hard-break implementation that satisfies the step.
5. Add focused deterministic tests in the owning crate.
6. If adding live/billed behavior, add ignored tests gated by an explicit env flag.
7. Run:
   - `cargo fmt --all`
   - focused tests for the changed crate
   - `cargo clippy -p <crate> --all-targets --all-features --locked -- -D warnings`
   - `cargo build --workspace` when public APIs or shared crates changed
   - `git diff --check`
8. Use `moa-certify` when choosing broader release/live validation.

## Reporting

In the final response, include:

- files changed
- the memory-pack behavior landed
- deterministic tests run
- live tests run or intentionally skipped
- any prompt acceptance criteria not covered, with a concrete reason
