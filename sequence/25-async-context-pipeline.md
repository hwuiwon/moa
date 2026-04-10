# Step 25: Async Context Pipeline + Typed Preload

## What this step is about
The `ContextProcessor` trait's `process()` method is synchronous, but stages 4, 5, and 6 need async I/O (skill loading, memory search, session event loading). The current workaround preloads data into `WorkingContext.metadata` as stringly-typed JSON values before running those stages. This works but is fragile: the keys are magic strings, the payloads are untyped `serde_json::Value`, and stage logic is split between the preload code in `pipeline/mod.rs` and the formatting code in each stage module.

This step makes `ContextProcessor::process()` async and removes the preload indirection.

## Files to read
- `moa-core/src/traits.rs` — `ContextProcessor` trait (sync `process()`)
- `moa-core/src/types.rs` — `WorkingContext`, `ProcessorOutput`
- `moa-brain/src/pipeline/mod.rs` — pipeline runner with preload logic, `HISTORY_EVENTS_METADATA_KEY`, `preload_memory_stage_data()`
- `moa-brain/src/pipeline/memory.rs` — `MemoryRetriever`, `MEMORY_STAGE_DATA_METADATA_KEY`, `PreloadedMemoryStageData`
- `moa-brain/src/pipeline/history.rs` — `HistoryCompiler`, reads from metadata
- `moa-brain/src/pipeline/skills.rs` — `SkillInjector`, `SKILLS_STAGE_DATA_METADATA_KEY`
- `moa-brain/src/pipeline/identity.rs` — `IdentityProcessor` (pure sync, no I/O)
- `moa-brain/src/pipeline/instructions.rs` — `InstructionProcessor` (pure sync)
- `moa-brain/src/pipeline/tools.rs` — `ToolDefinitionProcessor` (pure sync)
- `moa-brain/src/pipeline/cache.rs` — `CacheOptimizer` (pure sync)

## Goal
Every pipeline stage has a uniform `async fn process()` signature. Stages that need I/O do it directly inside their `process()` method. No more metadata-key preloading. Pure-sync stages simply don't `.await` anything — the async overhead (~243ns per call) is negligible compared to LLM latency.

## Rules
- Change `ContextProcessor::process` to an async method: `async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput>`
- Use `#[async_trait]` on `ContextProcessor` (consistent with every other trait in `moa-core`).
- Each stage that currently depends on a preloaded metadata key must be refactored to receive its dependencies through constructor injection (e.g., `Arc<dyn SessionStore>`, `Arc<dyn MemoryStore>`) and do its own I/O in `process()`.
- Pure-sync stages (identity, instructions, tools, cache) keep their current logic unchanged — just add `async` to the signature.
- Remove `HISTORY_EVENTS_METADATA_KEY`, `MEMORY_STAGE_DATA_METADATA_KEY`, `SKILLS_STAGE_DATA_METADATA_KEY` and all preload logic from `pipeline/mod.rs`.
- Remove `PreloadedMemoryStageData` struct from `memory.rs` (or make it stage-internal if the stage still uses it as an internal detail).
- The `WorkingContext.metadata` field should remain available for legitimate cross-stage communication (e.g., stage 7 reading cache metrics set by earlier stages), but it should NOT be used for the pipeline runner to feed data into stages.

## Tasks

### 1. Update `ContextProcessor` trait in `moa-core/src/traits.rs`
```rust
#[async_trait]
pub trait ContextProcessor: Send + Sync {
    fn name(&self) -> &str;
    fn stage(&self) -> u8;
    async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput>;
}
```

### 2. Give async stages their own dependencies via constructor injection

**`HistoryCompiler`** — currently reads events from `ctx.metadata`. After: holds `Arc<dyn SessionStore>`, loads events in `process()`.
```rust
pub struct HistoryCompiler {
    session_store: Arc<dyn SessionStore>,
}
```

**`MemoryRetriever`** — currently reads preloaded `PreloadedMemoryStageData` from metadata. After: holds `Arc<dyn MemoryStore>`, does search + read in `process()`.
```rust
pub struct MemoryRetriever {
    memory_store: Arc<dyn MemoryStore>,
}
```

**`SkillInjector`** — currently reads preloaded skill metadata from metadata. After: holds `Arc<SkillRegistry>`, calls `list_for_pipeline()` in `process()`.
```rust
pub struct SkillInjector {
    skill_registry: Arc<SkillRegistry>,
}
```

### 3. Simplify the pipeline runner (`pipeline/mod.rs`)
Remove:
- `HISTORY_EVENTS_METADATA_KEY` constant
- All `if stage.stage() == N && !ctx.metadata.contains_key(...)` preload blocks
- `preload_memory_stage_data()` function
- `load_history_events()` helper (or move into `HistoryCompiler` as a private method)

The runner becomes a simple loop:
```rust
for stage in &self.stages {
    let started_at = Instant::now();
    let mut output = stage.process(ctx).await?;
    output.duration = started_at.elapsed();
    // ... logging and report collection unchanged
}
```

### 4. Update `build_default_pipeline` and `build_default_pipeline_with_tools`
These constructors now need to pass `session_store` and `memory_store` to the stages that need them:
```rust
vec![
    Box::new(IdentityProcessor),
    Box::new(InstructionProcessor::from_config(config)),
    Box::new(ToolDefinitionProcessor::new(tool_schemas)),
    Box::new(SkillInjector::new(skill_registry.clone())),
    Box::new(MemoryRetriever::new(memory_store.clone())),
    Box::new(HistoryCompiler::new(session_store.clone())),
    Box::new(CacheOptimizer),
]
```

### 5. Update pure-sync stages
Add `#[async_trait]` and `async` to `process()` in:
- `identity.rs`
- `instructions.rs`
- `tools.rs`
- `cache.rs`

No logic changes needed — they just become async functions that happen not to `.await`.

### 6. Update all tests
- `pipeline/mod.rs` tests — `TestStage` must implement the async `process()`
- `pipeline/identity.rs` tests
- `pipeline/instructions.rs` tests
- `pipeline/tools.rs` tests
- `pipeline/memory.rs` tests
- `pipeline/history.rs` tests
- `pipeline/skills.rs` tests
- `pipeline/cache.rs` tests
- `moa-brain/tests/brain_turn.rs` — if it builds a pipeline

## Deliverables
```
moa-core/src/traits.rs                # Async ContextProcessor trait
moa-brain/src/pipeline/mod.rs         # Simplified runner, no preload
moa-brain/src/pipeline/memory.rs      # Self-sufficient async stage
moa-brain/src/pipeline/history.rs     # Self-sufficient async stage
moa-brain/src/pipeline/skills.rs      # Self-sufficient async stage
moa-brain/src/pipeline/identity.rs    # Async signature, no logic change
moa-brain/src/pipeline/instructions.rs # Async signature, no logic change
moa-brain/src/pipeline/tools.rs       # Async signature, no logic change
moa-brain/src/pipeline/cache.rs       # Async signature, no logic change
```

## Acceptance criteria
1. `ContextProcessor::process` is async across the entire codebase.
2. No `METADATA_KEY` constants remain for runner-to-stage data passing.
3. No preload blocks in the pipeline runner — each stage fetches what it needs.
4. `MemoryRetriever` holds and uses `Arc<dyn MemoryStore>` directly.
5. `HistoryCompiler` holds and uses `Arc<dyn SessionStore>` directly.
6. `SkillInjector` holds and uses `Arc<SkillRegistry>` directly.
7. Pure-sync stages compile and pass tests with the async signature.
8. Pipeline integration test still produces correct stage ordering.
9. All existing tests pass.

## Tests

**Unit tests per stage (existing tests updated):**
- `IdentityProcessor` appends identity block (now async, same assertions)
- `InstructionProcessor` appends workspace/user instructions (same)
- `ToolDefinitionProcessor` sets tool schemas (same)
- `SkillInjector` injects skill metadata — now with mock `SkillRegistry` passed at construction
- `MemoryRetriever` searches and includes relevant pages — now with mock `MemoryStore` at construction, no metadata pre-seeding
- `HistoryCompiler` compiles events — now with mock `SessionStore` at construction, no metadata pre-seeding
- `CacheOptimizer` verifies prefix ordering (same)

**Pipeline integration test:**
- Build pipeline with 3+ stages, run, verify ordering and reports (existing test, updated for async)

**Regression test:**
- Full brain turn: build pipeline → compile context → call mock LLM → verify context includes memory and history. This validates the end-to-end flow still works after removing preloads.

```bash
cargo test -p moa-core
cargo test -p moa-brain
```
