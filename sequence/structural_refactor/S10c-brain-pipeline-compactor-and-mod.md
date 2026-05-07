# S10c — Split `pipeline/compactor.rs` (910 LOC) and clean up `pipeline/mod.rs` (680 LOC)

## Scope

Split the compaction stage and clean up the pipeline aggregator file. **No behavior changes.**

## Preconditions

- S10a + S10b complete and merged.
- `cargo check --workspace` is green.

## Why this prompt

`pipeline/compactor.rs` is the dedicated compaction stage that decides when to trigger a checkpoint event in the session log (separate from `history.rs`'s in-memory compaction during the same turn). It's 910 LOC — under the 1k threshold but still big enough to deserve a folder. `pipeline/mod.rs` is 680 LOC and likely contains: the pipeline builder, stage ordering, `WorkingContext` helpers, and possibly the `ProcessorOutput` impl. Cleaning it up makes the pipeline structure scannable.

## Files in scope

- `crates/moa-brain/src/pipeline/compactor.rs` → split to `pipeline/compactor/`
- `crates/moa-brain/src/pipeline/mod.rs` → trimmed and reorganized

## Files explicitly out of scope

- Already-split pipeline files (`history/`, `skills/`, `query_rewrite/`) from S10a/b
- `pipeline/identity.rs`, `pipeline/instruction.rs`, `pipeline/tool_definition.rs` (probably small files; leave alone)
- The `ContextProcessor` trait in `moa-core`

## Step-by-step instructions

### Part A — `compactor.rs`

1. Read end-to-end. Identify sections:
   - `CompactorProcessor` struct + `impl ContextProcessor`
   - Trigger conditions (event count, token usage thresholds)
   - Memory flush logic (give brain a chance to save before compacting)
   - Checkpoint summary generation (LLM call to summarize)
   - Checkpoint event emission

2. Target structure:
   ```
   pipeline/compactor/
   ├── mod.rs           — struct + impl
   ├── triggers.rs      — when to compact
   ├── flush.rs         — memory flush before compaction
   ├── summarize.rs     — LLM-driven summary generation
   └── emit.rs          — checkpoint event emission to session log
   ```

### Part B — `pipeline/mod.rs` cleanup

3. Read `pipeline/mod.rs`. Identify content:
   - Module declarations (`pub mod identity; pub mod instruction; ...`)
   - The pipeline builder (`build_pipeline()` function)
   - The `WorkingContext` type — *if it lives here*, evaluate moving to `moa-core` (likely better home for context type) or to a sibling `pipeline/context.rs`
   - The `ProcessorOutput` type — same evaluation
   - Free helper functions used by multiple processors

4. Decisions:
   - **`WorkingContext`**: if used by `ContextProcessor` trait (which is in `moa-core`), it must be in `moa-core` already or moved there. Verify. If it's in `moa-brain` but `moa-core` references it, the trait must be using `&mut WorkingContext` via a re-import — confusing. Prefer: `WorkingContext` lives in `moa-core::context` alongside the trait.
   - **`ProcessorOutput`**: same rule. Probably in `moa-core` already.
   - If either type *is* defined in `pipeline/mod.rs`, move to `moa-core::context` in this prompt. Update all imports.
   - **`build_pipeline` function**: stays in `mod.rs` or moves to `pipeline/builder.rs` if it's >100 LOC.

5. Target structure for `pipeline/`:
   ```
   pipeline/
   ├── mod.rs              — module declarations + build_pipeline (thin)
   ├── builder.rs          — build_pipeline if it's >100 LOC
   ├── identity.rs         — Stage 1 (unchanged unless >300 LOC)
   ├── instruction.rs      — Stage 2 (unchanged)
   ├── tool_definition.rs  — Stage 3 (unchanged)
   ├── skills/             — Stage 4 (from S10b)
   ├── memory_retriever.rs — Stage 5 (probably small; leave)
   ├── history/            — Stage 6 (from S10a)
   ├── compactor/          — companion stage (from this prompt)
   ├── query_rewrite/      — auxiliary stage (from S10b)
   └── cache_optimizer.rs  — Stage 7 (probably small; leave)
   ```

6. Run verification.

7. Document anomalies in `REFACTOR_NOTES.md` under `[S10c]` — especially if `WorkingContext` or `ProcessorOutput` had to move to `moa-core`.

## Verification

```bash
cargo check -p moa-brain --all-targets
cargo clippy -p moa-brain --all-targets -- -D warnings
cargo test -p moa-brain --no-run
cargo check -p moa-core --all-targets   # if WorkingContext moved
cargo check --workspace --all-targets
```

## Acceptance criteria

- [ ] `pipeline/compactor.rs` no longer exists; replaced by folder.
- [ ] `pipeline/mod.rs` is under 300 LOC.
- [ ] Module declarations are present for every pipeline stage.
- [ ] If `WorkingContext` / `ProcessorOutput` moved to `moa-core`, they're at `moa_core::context::*` and existing call sites still work.
- [ ] No file in `pipeline/` exceeds 600 LOC.
- [ ] `cargo check --workspace --all-targets` passes.

## Rollback plan

`git checkout -- crates/moa-brain/src/pipeline/ crates/moa-core/src/` (since this prompt may have touched core).

## Notes for the agent

- **The pipeline order is load-bearing** for KV-cache hit rate. The 7-stage order (Identity → Instruction → ToolDefinition → Skills → MemoryRetriever → History → CacheOptimizer) is documented architecture. Compactor and QueryRewrite are *auxiliary* stages — they don't sit in the 7-stage main flow but are invoked at specific points. Keep the structure crystal clear in `mod.rs`.
- **`build_pipeline` may instantiate processors with config**. Each processor's `new` takes config; `build_pipeline` just orchestrates. Don't change the construction logic.
- **`WorkingContext` mutation ordering matters.** Each processor reads/mutates the context in turn. Don't change the call order.
- **If `WorkingContext` lives in `moa-brain`**, that's a smell — the `ContextProcessor` trait is in `moa-core` and takes `&mut WorkingContext`, which means `moa-core` must already know about `WorkingContext`. Either it's already in `moa-core` (and `moa-brain` re-exports), or there's a generic parameter. Check before moving.
- **Time budget**: 1 session.
- **Anti-pattern**: do not introduce a `Pipeline` struct that owns the processors. The current pattern (a `Vec<Box<dyn ContextProcessor>>` returned by `build_pipeline`) is fine.
