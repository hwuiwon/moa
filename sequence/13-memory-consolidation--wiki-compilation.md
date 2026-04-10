# Step 13: Memory Consolidation + Wiki Compilation

## What this step is about
The consolidation "dream" cycle that cleans up memory, and the wiki compilation pipeline that integrates new sources into the knowledge base.

## Files to read
- `docs/04-memory-architecture.md` — Consolidation process, ingest pipeline, git-branch concurrent writes, _log.md

## Goal
Memory stays healthy over time. Stale entries are pruned, contradictions resolved, dates normalized. New sources are compiled into wiki pages with cross-references. Multiple brains can write concurrently without conflicts.

## Tasks
1. **`moa-memory/src/consolidation.rs`**: `run_consolidation()` — temporal normalization, contradiction resolution, stale pruning, dedup, orphan detection, confidence decay, index maintenance. Triggered when ≥3 sessions AND ≥24h since last.
2. **`moa-memory/src/ingest.rs`**: `ingest_source()` — read source, generate summary page, update entity/topic/decision pages, flag contradictions, update index and log.
3. **`moa-memory/src/branching.rs`**: Git-branch concurrent writes. Each brain writes to a branch directory. `reconcile_branches()` uses LLM to merge, resolve conflicts, clean up branches.
4. **Register consolidation as a cron job** in the orchestrator (every hour, check conditions).
5. **Update `moa-memory/src/index.rs`**: _log.md append-only change tracking.

## Deliverables
`moa-memory/src/consolidation.rs`, `moa-memory/src/ingest.rs`, `moa-memory/src/branching.rs`, updated orchestrator cron.

## Acceptance criteria
1. Consolidation normalizes relative dates to absolute
2. Consolidation resolves contradictions between pages
3. Consolidation prunes pages about non-existent entities
4. MEMORY.md stays under 200 lines after consolidation
5. Source ingestion creates summary + updates related pages
6. Branch writes isolate concurrent modifications
7. Reconciliation merges branches correctly

## Tests
- Integration test: Write pages with relative dates → consolidate → verify absolute dates
- Integration test: Write contradictory pages → consolidate → verify resolution
- Integration test: Two branches write same page → reconcile → verify merged content
- Integration test: Ingest a source → verify summary page created and related pages updated

---

