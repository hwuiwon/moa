# Step 04: Brain Harness + Context Pipeline

## What this step is about
The brain harness loop (single turn: wake → compile context → call LLM → process response → emit events) and all 7 context pipeline stages.

## Files to read
- `docs/02-brain-orchestration.md` — Brain loop pseudocode, `brain_turn` activity
- `docs/07-context-pipeline.md` — All 7 stages with implementations
- `docs/01-architecture-overview.md` — `ContextProcessor` trait, `WorkingContext`

## Goal
A `run_brain_turn()` function that loads session events, compiles context through 7 pipeline stages, calls the LLM, processes the response (text and tool calls), and emits events. At this stage, tool calls are emitted as events but NOT executed (no hands yet).

## Rules
- The brain harness must be completely trait-based — it takes `Arc<dyn SessionStore>`, `Arc<dyn LLMProvider>`, etc.
- Pipeline stages execute in order 1-7, each receiving `&mut WorkingContext`
- Stages 1-4 (Identity, Instructions, Tools, Skills) form the stable prefix — mark a cache breakpoint after stage 4
- Stage 5 (MemoryRetriever) is a no-op stub at this step (memory not implemented yet)
- Stage 6 (HistoryCompiler) loads events and formats them as conversation messages
- Stage 7 (CacheOptimizer) verifies prefix ordering and logs cache efficiency
- Every stage must emit a `ProcessorOutput` with token counts

## Tasks
1. **`moa-brain/src/harness.rs`**: `run_brain_turn()` function
2. **`moa-brain/src/pipeline/mod.rs`**: Pipeline runner that chains processors
3. **`moa-brain/src/pipeline/identity.rs`**: Stage 1 — static system prompt
4. **`moa-brain/src/pipeline/instructions.rs`**: Stage 2 — workspace/user instructions (reads from config)
5. **`moa-brain/src/pipeline/tools.rs`**: Stage 3 — serializes tool schemas (empty list for now)
6. **`moa-brain/src/pipeline/skills.rs`**: Stage 4 — stub (returns empty)
7. **`moa-brain/src/pipeline/memory.rs`**: Stage 5 — stub (returns empty)
8. **`moa-brain/src/pipeline/history.rs`**: Stage 6 — loads session events, formats as messages
9. **`moa-brain/src/pipeline/cache.rs`**: Stage 7 — verifies ordering, logs metrics
10. **`moa-brain/src/compaction.rs`**: Stub for context compaction (full implementation in Step 13)

## Deliverables
```
moa-brain/src/
├── lib.rs
├── harness.rs
├── compaction.rs
└── pipeline/
    ├── mod.rs
    ├── identity.rs
    ├── instructions.rs
    ├── tools.rs
    ├── skills.rs
    ├── memory.rs
    ├── history.rs
    └── cache.rs
```

## Acceptance criteria
1. `run_brain_turn()` can process a session with one UserMessage event and produce a BrainResponse
2. Pipeline stages execute in order, each logging its output
3. HistoryCompiler correctly formats events as user/assistant messages
4. Cache breakpoint is placed after stage 4
5. Token counting is approximate but consistent

## Tests
- Unit test: Each pipeline stage processes a `WorkingContext` and returns valid `ProcessorOutput`
- Unit test: HistoryCompiler formats a sequence of UserMessage + BrainResponse events into alternating user/assistant messages
- Integration test: `run_brain_turn()` with a mock LLM provider that returns fixed text → verify correct events emitted to session store
- Unit test: Pipeline runner executes stages in order (verify via stage names in output)

```bash
cargo test -p moa-brain
```

---

