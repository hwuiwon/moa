# Step C03 — Migrate orchestrators off the legacy memory store

_`moa-orchestrator-local` (the embedded production runtime) and `moa-orchestrator` (the Restate-backed orchestrator) both currently consume the legacy `MemoryStore`. C03 cuts them over to the graph stack: writes go through `fast_remember` (M11) or `IngestionVO::ingest_turn` (M10), reads go through the hybrid retriever (M15)._

## 1 What this step is about

Orchestrators are the runtime hot path: every session turn flows through them. Their use of `MemoryStore` is the production blast radius for the legacy crate. After this prompt, no orchestrator code path touches `FileMemoryStore` or the `MemoryStore` trait — every memory read/write is graph-shaped.

C01's inventory will tell you exactly which sites need migration. This prompt assumes the typical pattern: orchestrators construct a `MemoryStore` at startup and pass it through `BrainOrchestrator` / session machinery; turns call `MemoryStore::ingest_source` for raw sources and `MemoryStore::search` for retrieval.

## 2 Files to read

- `docs/migrations/moa-memory-inventory.md` — every row tagged with `crates/moa-orchestrator/` or `crates/moa-orchestrator-local/`.
- `crates/moa-orchestrator-local/src/lib.rs` (especially around line 35 where `moa-memory` is imported per the audit).
- `crates/moa-orchestrator/src/lib.rs` (if it has a similar pattern — C01 will say).
- `crates/moa-memory-ingest/src/lib.rs` — `fast_remember` and `IngestionVO::ingest_turn` signatures.
- `crates/moa-brain/src/pipeline/` — hybrid retriever entry point (M15).
- `crates/moa-core/src/traits.rs` — `BrainOrchestrator` trait (the orchestrator's public surface). Note that `ToolContext` carries `memory_store: &dyn MemoryStore` today; that field needs to change in C05 but consumers stop calling its methods here.

## 3 Goal

After C03:

- Both orchestrator crates import `moa-memory-graph`, `moa-memory-ingest`, `moa-brain` instead of `moa-memory`.
- The construction site (where `FileMemoryStore::from_config_with_pool` is called) builds an `AgeGraphStore` + `IngestionVO` + retriever instead.
- Every legacy method call (`search`, `read_page`, `write_page`, `ingest_source`, `list_pages`, `get_index`, `rebuild_search_index`) is replaced. Default replacements:
  - `search` → hybrid retriever.
  - `ingest_source` → `IngestionVO::ingest_turn` (slow path) for raw documents; `fast_remember` (fast path) for short observations from the session loop.
  - `read_page` / `write_page` / `list_pages` / `get_index` → these had no graph equivalent for orchestrators in M01–M19 design. C01 should have flagged them. Default disposition: delete the call (orchestrator wasn't using a wiki-shaped feature, just routing the trait around). If a real read use case exists, reroute to the hybrid retriever or `GraphStore::get_node`.
  - `rebuild_search_index` → delete the call; graph indexes are write-incremental.
- The `ToolContext` field `memory_store: &dyn MemoryStore` is **left in place for now** — C05 deletes it. Orchestrators stop populating it with a `FileMemoryStore`; instead they leave it as a soon-dead field by passing a no-op stub. (C05 will remove it entirely; this is the bridge.)

The build is green at the orchestrator-crate level. Workspace-level build still fails until C04 + C05 land.

## 4 Rules

- **Don't refactor unrelated code.** If the orchestrator has a stale `MemoryStore` field that is no longer used after migration, delete it; but don't restructure the orchestrator beyond what's needed for the cutover.
- **Wire fast vs. slow path correctly.** Short, high-confidence observations from the session loop (e.g., "user said X", "tool returned Y") use `fast_remember`. Bulk document ingest goes through `IngestionVO::ingest_turn`. C01 inventory rows should already classify each call site.
- **No silent behavior changes.** If a legacy call had side effects (e.g., `ingest_source` ran consolidation on the wiki), match the closest graph-side semantics or document the loss in a migration note in the orchestrator's `lib.rs`.
- **Tests follow the code.** Update orchestrator integration tests to use graph fixtures; mark legacy tests `#[ignore]` only with an explanation, never delete silently.

## 5 Tasks

### 5a Update `moa-orchestrator-local/Cargo.toml`

```toml
# remove
moa-memory = { path = "../moa-memory" }

# add (or no-op if already present)
moa-memory-graph = { path = "../moa-memory-graph" }
moa-memory-ingest = { path = "../moa-memory-ingest" }
moa-brain = { path = "../moa-brain" }
```

Same for `moa-orchestrator/Cargo.toml` if applicable.

### 5b Replace the construction site

Find where `FileMemoryStore::from_config_with_pool` is called. Replace with construction of three handles:

```rust
// before
let memory_store = Arc::new(
    FileMemoryStore::from_config_with_pool(&config, pool.clone(), schema).await?,
);

// after
use moa_memory_graph::{AgeGraphStore, GraphStore};
use moa_memory_ingest::IngestionVO;
use moa_brain::pipeline::HybridRetriever;

let graph_store: Arc<dyn GraphStore> = Arc::new(
    AgeGraphStore::from_config_with_pool(&config, pool.clone(), schema).await?,
);
let ingestion_vo = Arc::new(
    IngestionVO::new(graph_store.clone(), embedder.clone(), pii_filter.clone()).await?,
);
let retriever = Arc::new(
    HybridRetriever::new(graph_store.clone(), embedder.clone()).await?,
);
```

(Names and constructor signatures depend on what M07/M11/M15 actually shipped. Adapt to match.)

The orchestrator now carries three handles instead of one. Rename the field on the orchestrator struct accordingly:

```rust
pub struct LocalOrchestrator {
    // before:
    // memory_store: Arc<dyn MemoryStore>,
    // after:
    graph_store: Arc<dyn GraphStore>,
    ingestion: Arc<IngestionVO>,
    retriever: Arc<HybridRetriever>,
    // ...
}
```

### 5c Replace `MemoryStore::search` call sites

Wherever the orchestrator searches memory, swap:

```rust
let hits = self.memory_store.search(query, &scope, limit).await?;
```

with:

```rust
let hits = self.retriever.retrieve(query, &scope, limit).await?;
```

The shape of `hits` differs (legacy returns `Vec<MemorySearchResult>` with `path`, `title`, `snippet`; graph retriever returns ranked node hits). Update downstream consumers in the orchestrator that read fields off the hits — they'll be node-shaped now.

### 5d Replace `ingest_source` call sites

For raw-document ingestion (e.g., the orchestrator pipes a session's RAG attachments into memory):

```rust
self.memory_store.ingest_source(&scope, &name, &content).await?;
```

becomes:

```rust
let turn = SessionTurn::for_attachment(session_id, &name, &content);
self.ingestion.ingest_turn(&scope, turn).await?;
```

For short, in-loop "remember this" calls (e.g., a built-in `remember` tool):

```rust
self.memory_store.write_page(&scope, &path, page).await?;
```

becomes:

```rust
fast_remember(&self.graph_store, &scope, &observation).await?;
```

`fast_remember` is the M11 API; it skips the full Restate VO and writes a single node + embedding inline. Use it only for short, single-fact observations.

### 5e Delete wiki-shaped reads with no graph equivalent

Per C01 decisions, calls to `read_page`, `list_pages`, `get_index` from the orchestrator either:

- delete the call (and any code that consumed its result) if the feature was wiki-only, or
- redirect to the retriever or `GraphStore::get_node` if the user opted to redesign.

Document each non-trivial deletion in a `// MIGRATION:` comment so the diff explains itself.

### 5f Bridge `ToolContext` for now

`crates/moa-core/src/traits.rs` declares:

```rust
pub struct ToolContext<'a> {
    pub session: &'a SessionMeta,
    pub memory_store: &'a dyn MemoryStore,
    // ...
}
```

The orchestrator constructs a `ToolContext` per tool invocation. It currently passes the `FileMemoryStore`. Until C05 removes the field, pass a no-op stub:

```rust
// Bridge: the field is removed in C05. Keep the type-check happy until then.
struct DeadMemoryStoreShim;
#[async_trait]
impl MemoryStore for DeadMemoryStoreShim {
    async fn search(&self, _q: &str, _s: &MemoryScope, _l: usize)
        -> Result<Vec<MemorySearchResult>> {
        Err(MoaError::Unsupported("legacy memory_store removed; see C05".into()))
    }
    // ... every other method returns the same Unsupported error
    async fn read_page(&self, _: &MemoryScope, _: &MemoryPath) -> Result<WikiPage> {
        Err(MoaError::Unsupported("legacy memory_store removed; see C05".into()))
    }
    // ... etc.
}

let ctx = ToolContext {
    session: &session,
    memory_store: &DeadMemoryStoreShim,
    // ...
};
```

This shim lives in the orchestrator crate, not in `moa-core`. It's transient — C05 deletes both the field and the shim.

If C04's update to `moa-skills` reveals that the built-in tools are calling `ctx.memory_store.*`, those tools also need to be migrated in C04. The shim ensures they fail loudly (not silently) if a missed migration calls into it.

### 5g Update orchestrator tests

For each integration test that constructs a `FileMemoryStore` for fixture data, replace with graph fixtures. The graph subsystem's test helpers (added in M03/M04) should expose a `seed_test_graph(pool, scope, fixtures)` helper. Use it.

If a test exists specifically for wiki-shaped behavior (`test_orchestrator_reads_topics_md`, etc.), delete the test — that behavior no longer exists.

### 5h Update `moa-orchestrator` (if applicable)

C01 inventory will say whether `moa-orchestrator` (the non-local one) also imports `moa-memory`. If yes, repeat 5a–5g for that crate. If no, this section is a no-op.

## 6 Deliverables

- Updated Cargo.tomls for `moa-orchestrator-local` (and `moa-orchestrator` if applicable).
- Construction site rewritten to build graph stack handles.
- Every `MemoryStore::*` call site in orchestrators replaced or deleted.
- `DeadMemoryStoreShim` bridge in place for `ToolContext`.
- Orchestrator integration tests updated to graph fixtures.
- `// MIGRATION:` comments documenting any deleted features.

## 7 Acceptance criteria

1. `rg "use moa_memory" crates/moa-orchestrator-local/` and `crates/moa-orchestrator/` return 0 hits.
2. `rg "FileMemoryStore" crates/moa-orchestrator-local/` and `crates/moa-orchestrator/` return 0 hits.
3. `rg "MemoryStore::" crates/moa-orchestrator-local/ crates/moa-orchestrator/` returns only matches inside `DeadMemoryStoreShim` (the bridge).
4. `cargo build -p moa-orchestrator-local` clean.
5. `cargo build -p moa-orchestrator` clean.
6. `cargo test -p moa-orchestrator-local` green.
7. The orchestrator-level integration test (M11 fast-path or M10 slow-path end-to-end test) still passes.

## 8 Tests

```sh
cargo build -p moa-orchestrator-local
cargo build -p moa-orchestrator
cargo test -p moa-orchestrator-local
cargo test -p moa-orchestrator

# End-to-end smoke (against test DB):
cargo test --test e2e_session_ingest -- --nocapture
```

## 9 Cleanup

- Remove unused imports from the orchestrator crates.
- `cargo fmt && cargo clippy -p moa-orchestrator-local -p moa-orchestrator -- -D warnings` clean.
- If the orchestrator had a `memory_store_metrics_*` block of telemetry that referenced wiki-specific counters, swap it for graph-side counters (`moa.memory.graph.write_count`, etc.) or delete the block.

## 10 What's next

**C04 — Skills + tail.** Migrate `moa-skills` (especially `regression.rs` / `run_skill_suite`), plus `moa-eval`, `moa-runtime`, `moa-gateway`, `moa-loadtest` if any have lingering imports.
