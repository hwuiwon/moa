# Lineage Linkage Implementation Plan

> **Worker note:** Execute this plan task-by-task. Each step uses checkbox
> (`- [ ]`) syntax for progress tracking.

**Goal:** Link every generated answer to the durable turn, persisted answer
message, compiled context chunks, and underlying source objects/messages that
influenced it.

**Architecture:** Use the existing `LineageHandle` and Postgres lineage sink.
Thread the Restate workflow `turn_id` into context compilation, add structured
source references to compiled context lineage, and emit generation/citation
lineage in the production Restate LLM path. Keep persistence best-effort and
measure lineage overhead instead of blocking the user-visible turn.

**Tech Stack:** Rust 2024, Restate workflows/services, `moa-core` DTOs,
`moa-brain` context pipeline, `moa-orchestrator` production turn loop,
`moa-lineage-*`, Postgres migrations.

**Work Scope:**
- **In scope:** production turn-id propagation, structured context source refs,
  production generation/citation emission, retrieval sidecar `turn_id`,
  focused tests, docs, and metrics for lineage overhead.
- **Out of scope:** replacing the lexical-overlap citation verifier with a real
  NLI provider, changing graph-memory ranking quality logic, cold-storage DSAR
  design, and broad eval baseline refreshes.

**Verification Strategy:**
- **Level:** unit plus focused crate integration/build checks
- **Command:** `cargo fmt --all && cargo check -p moa-brain --features eval-harness --tests --locked && cargo check -p moa-orchestrator --tests --locked && cargo clippy -p moa-brain --features eval-harness --tests --locked -- -D warnings && cargo clippy -p moa-orchestrator --tests --locked -- -D warnings && git diff --check`
- **What it validates:** public DTO call sites compile, lineage helper behavior
  is pinned, production Restate path can emit lineage records, and formatting
  plus clippy remain clean.

---

## Task 1: Thread Durable Turn IDs

**Dependencies:** None
**Files:**
- Modify: `crates/moa-orchestrator/src/brain_bridge.rs`
- Modify: `crates/moa-orchestrator/src/workflows/turn_execution.rs`
- Modify: `crates/moa-brain/src/pipeline/memory.rs`

**Acceptance Criteria:**
- [ ] `prepare_turn_request` receives the workflow turn id and inserts
  `_moa.turn_id` before the context pipeline runs.
- [ ] Graph-memory `RetrievalLineage.turn_id` uses the durable turn id in the
  production Restate path.
- [ ] Existing harness behavior still inserts `_moa.turn_id`.

## Task 2: Add Structured Context Source References

**Dependencies:** Task 1
**Files:**
- Modify: `crates/moa-core/src/types/context.rs`
- Modify: `crates/moa-brain/src/pipeline/history/conversion.rs`
- Modify: `crates/moa-brain/src/pipeline/memory.rs`
- Modify: `crates/moa-lineage/core/src/records.rs`
- Modify: `crates/moa-brain/src/harness/streaming/lineage.rs`

**Acceptance Criteria:**
- [ ] `ContextMessage` can carry optional source metadata without breaking
  provider request serialization.
- [ ] History messages retain event id and sequence number.
- [ ] Tool messages retain tool call/result identifiers as structured source
  refs.
- [ ] Memory-inserted messages retain graph node UIDs as structured source
  refs.
- [ ] `ContextChunk` records source kind, source uid, event sequence, and tool
  id where known.

## Task 3: Emit Production Generation And Citation Lineage

**Dependencies:** Tasks 1-2
**Files:**
- Modify: `crates/moa-brain/src/harness/streaming/lineage.rs`
- Modify: `crates/moa-brain/src/harness/streaming/mod.rs`
- Modify: `crates/moa-brain/src/lib.rs`
- Modify: `crates/moa-orchestrator/src/workflows/turn_execution.rs`
- Modify: `crates/moa-orchestrator/src/services/llm_gateway.rs`

**Acceptance Criteria:**
- [ ] Shared lineage helpers can be called from both harness and production.
- [ ] Production Restate turns emit `GenerationLineage` and `CitationLineage`
  for the same durable `turn_id` used by retrieval/context lineage.
- [ ] The generation/citation record includes a structured link to the persisted
  `BrainResponse` event id and sequence number when available.
- [ ] Tool-use turns and non-text responses do not panic or block on citation
  verification.

## Task 4: Link Retrieval Sidecar To Turn IDs

**Dependencies:** Task 1
**Files:**
- Create: `crates/moa-migrations/migrations/postgres/V000302__retrieval_lineage_turn_id.sql`
- Modify: `crates/moa-migrations/migrations/postgres/V000001__session_baseline.sql`
- Modify: `crates/moa-brain/src/retrieval/hybrid.rs`
- Modify: `crates/moa-brain/src/retrieval/legs.rs`
- Modify: `crates/moa-brain/src/pipeline/memory.rs`

**Acceptance Criteria:**
- [ ] New retrieval lineage rows store nullable `turn_id`.
- [ ] Existing rows without `turn_id` remain readable.
- [ ] Fire-and-forget retrieval sidecar writes still do not block normal
  retrieval.

## Task 5: Add Tests

**Dependencies:** Tasks 1-4
**Files:**
- Modify or create focused tests near the changed modules.

**Acceptance Criteria:**
- [ ] Unit test proves context chunks preserve event and tool source refs.
- [ ] Unit test proves memory chunks preserve graph source refs and citations
  point to those refs.
- [ ] Unit or integration test proves production request compilation uses the
  workflow turn id.
- [ ] Existing eval lineage test remains green.
- [ ] Mutation check is performed on at least one linkage assertion.

## Task 6: Metrics And Docs

**Dependencies:** Tasks 1-5
**Files:**
- Modify: `docs/02-brain-orchestration.md`
- Modify: `docs/04-memory-architecture.md`
- Modify: `docs/05-session-event-log.md`
- Modify relevant metrics code near lineage emission.

**Acceptance Criteria:**
- [ ] Docs describe durable turn-id linkage and source refs.
- [ ] Metrics record citation verifier duration and candidate source count.
- [ ] Performance notes explicitly state the sink is best-effort and bounded.

## Task 7 (Final): End-To-End Verification

**Dependencies:** All preceding tasks
**Files:** None

- [ ] Run the Verification Strategy command.
- [ ] Run any focused DB test needed for the retrieval-lineage migration when
  local Postgres is available.
- [ ] Run `git diff --check`.
- [ ] Verify every success criterion from this plan.
