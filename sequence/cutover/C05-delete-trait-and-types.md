# Step C05 — Delete the `MemoryStore` trait and wiki types from `moa-core`

_With every consumer migrated by C02–C04, the `MemoryStore` trait and the wiki types it depended on (`WikiPage`, `MemoryPath`, `PageType`, `IngestReport`, `MemorySearchMode`, `MemorySearchResult`, `PageSummary`, `ConfidenceLevel`) have zero in-workspace use sites outside of `moa-memory` itself. C05 deletes them from `moa-core`, removes the `memory_store` field from `ToolContext`, and deletes the `DeadMemoryStoreShim` bridge introduced in C03._

## 1 What this step is about

`moa-core` is the type-stable foundation crate. It carried wiki types because every consumer needed them through the `MemoryStore` trait. Now that the trait is unused, the types come out too. This eliminates the last in-workspace reference to wiki shapes outside `moa-memory` itself, which makes C06's deletion of the legacy crate trivial.

This step also formalizes `ToolContext`. C03 left a `DeadMemoryStoreShim` bridging the old field. C05 removes the field entirely and adds graph-stack handles in its place.

## 2 Files to read

- `crates/moa-core/src/traits.rs` (the `MemoryStore` trait, `ToolContext` struct).
- `crates/moa-core/src/types/` (every file holding a wiki type).
- `crates/moa-core/src/lib.rs` (re-exports).
- `crates/moa-memory/src/lib.rs` — only because it imports types from `moa-core`. After this step, `moa-memory` will fail to compile (good — that's the lever C06 uses).
- C03's `DeadMemoryStoreShim` location (orchestrator crates).
- Built-in tool implementations that read `ctx.memory_store` (now `ctx.graph_store` after this step).

## 3 Goal

After this step:

- `crates/moa-core/src/traits.rs` no longer defines `MemoryStore`.
- `crates/moa-core/src/types/` no longer holds `WikiPage`, `MemoryPath`, `PageType`, `IngestReport`, `MemorySearchMode`, `MemorySearchResult`, `PageSummary`, `ConfidenceLevel`. Each is deleted.
- `crates/moa-core/src/lib.rs` re-exports are pruned to match.
- `ToolContext` carries graph-stack handles instead of a `memory_store: &dyn MemoryStore` field.
- `DeadMemoryStoreShim` is deleted from the orchestrator crates.
- `crates/moa-memory/src/lib.rs` now fails to compile because its imports from `moa-core` are gone. **This is intended.** C06 deletes the crate entirely.
- `cargo build --workspace --exclude moa-memory` is clean.

## 4 Rules

- **Re-audit before deleting.** Run `rg` for each type one more time to confirm zero in-workspace use sites outside `moa-memory`. If any remain, fix the consumer before deleting the type.
- **`MoaError` variants stay.** If `MoaError` has a variant tied to wiki errors (e.g., `WikiPageNotFound`), leave it — it costs nothing and avoids a noisy churn. C06 can revisit if desired.
- **No "marker" stub types.** Don't leave a hollow `MemoryStore` trait or empty `WikiPage` struct as a placeholder. Delete cleanly.
- **One commit per type group is fine.** This prompt can be split into 5a/5b/5c commits if useful for review; the acceptance criteria apply to the cumulative end state.

## 5 Tasks

### 5a Pre-deletion re-audit

```sh
# Trait
rg "MemoryStore" crates/ --type rust | grep -v "crates/moa-memory/" | grep -v "DeadMemoryStoreShim"

# Wiki types
for ty in WikiPage MemoryPath PageType IngestReport MemorySearchMode MemorySearchResult PageSummary ConfidenceLevel; do
    echo "=== $ty ==="
    rg "$ty" crates/ --type rust | grep -v "crates/moa-memory/" | head -20
done
```

Expected: only `moa-memory` and `DeadMemoryStoreShim` references remain. If any other consumer pops up, **stop and migrate that consumer first** (loop back into C02/C03/C04 as a follow-up commit).

### 5b Update `ToolContext`

In `crates/moa-core/src/traits.rs`:

```rust
// before
pub struct ToolContext<'a> {
    pub session: &'a SessionMeta,
    pub memory_store: &'a dyn MemoryStore,
    pub session_store: Option<&'a dyn SessionStore>,
    pub cancel_token: Option<&'a CancellationToken>,
}

// after
pub struct ToolContext<'a> {
    pub session: &'a SessionMeta,
    pub graph_store: &'a dyn moa_memory_graph::GraphStore,
    pub retriever: &'a moa_brain::pipeline::HybridRetriever,
    pub ingestion: &'a moa_memory_ingest::IngestionVO,
    pub session_store: Option<&'a dyn SessionStore>,
    pub cancel_token: Option<&'a CancellationToken>,
}
```

⚠ **Caution about a circular dependency.** `moa-core` is a foundation crate; `moa-memory-graph`, `moa-memory-ingest`, `moa-brain` all depend on it. If `moa-core` tries to import them, you get a cycle.

Two ways to avoid the cycle:

**Option A** (recommended): make `ToolContext` generic over the graph stack. Define traits in `moa-core` for the minimum surface tools need, and put the concrete handles in those traits.

```rust
// in moa-core
pub trait GraphReadHandle: Send + Sync { /* small interface — get_node, neighbors */ }
pub trait RetrievalHandle: Send + Sync { /* retrieve(query, scope, limit) */ }
pub trait IngestHandle: Send + Sync { /* ingest_turn(scope, turn) */ }

pub struct ToolContext<'a> {
    pub session: &'a SessionMeta,
    pub graph: &'a dyn GraphReadHandle,
    pub retriever: &'a dyn RetrievalHandle,
    pub ingestion: &'a dyn IngestHandle,
    // ...
}
```

Then `moa-memory-graph` impls `GraphReadHandle` for `AgeGraphStore`; `moa-brain` impls `RetrievalHandle` for `HybridRetriever`; `moa-memory-ingest` impls `IngestHandle` for `IngestionVO`. No circular dep.

**Option B**: move `ToolContext` and `BuiltInTool` out of `moa-core` and into a new `moa-tools` crate that depends on the graph stack. More disruptive; only do this if Option A's trait surface gets unwieldy.

Recommend Option A.

### 5c Delete the `MemoryStore` trait

In `crates/moa-core/src/traits.rs`, delete:

```rust
#[async_trait]
pub trait MemoryStore: Send + Sync { ... }
```

And every method declaration inside it (search, search_with_mode, read_page, write_page, delete_page, list_pages, get_index, ingest_source, rebuild_search_index).

### 5d Delete wiki types

For each type, delete its definition and any helper functions that operated on it:

- `crates/moa-core/src/types/wiki_page.rs` (or wherever `WikiPage` lives) — delete file or stripped to empty.
- `crates/moa-core/src/types/memory_path.rs` — delete.
- `crates/moa-core/src/types/page_type.rs` — delete.
- `crates/moa-core/src/types/ingest_report.rs` — delete.
- `crates/moa-core/src/types/memory_search.rs` — delete (`MemorySearchMode`, `MemorySearchResult`).
- `crates/moa-core/src/types/page_summary.rs` — delete.
- `crates/moa-core/src/types/confidence_level.rs` — delete.

(Exact filenames depend on the existing layout. Use `rg "pub struct WikiPage|pub enum PageType"` etc. to locate.)

### 5e Prune `lib.rs` re-exports

Open `crates/moa-core/src/lib.rs` and remove every `pub use ... { ..., WikiPage, MemoryPath, ... }`. Same for the type module's `mod.rs` if applicable.

### 5f Delete `DeadMemoryStoreShim`

In `crates/moa-orchestrator-local/` (and `moa-orchestrator/` if applicable), find and delete the `DeadMemoryStoreShim` struct and impl introduced in C03. The `ToolContext` construction site updates to populate the new fields:

```rust
// before
let ctx = ToolContext {
    session: &session,
    memory_store: &DeadMemoryStoreShim,
    // ...
};

// after
let ctx = ToolContext {
    session: &session,
    graph: &*self.graph_store,
    retriever: &*self.retriever,
    ingestion: &*self.ingestion,
    // ...
};
```

### 5g Update built-in tools

Tools migrated in C04 referenced their own struct-field handles. With the new `ToolContext`, they can read `ctx.graph` / `ctx.retriever` / `ctx.ingestion` directly. Refactor each tool to use the context:

```rust
// after C04
pub struct MemorySearchTool {
    retriever: Arc<HybridRetriever>,
}

// after C05
pub struct MemorySearchTool;

impl BuiltInTool for MemorySearchTool {
    async fn execute(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<ToolOutput> {
        let hits = ctx.retriever.retrieve(...).await?;
        ...
    }
}
```

This is a simplification — the tools no longer need to be constructed with handles. They become unit structs again.

### 5h Confirm `moa-memory` is the only failing crate

```sh
cargo build --workspace --exclude moa-memory
```

Must be clean. If any other crate fails, you missed a consumer in C02/C03/C04 — find and migrate it.

```sh
cargo build --workspace 2>&1 | tail -50
```

Should fail with errors localized to `crates/moa-memory/src/`. That's the green light for C06.

## 6 Deliverables

- `MemoryStore` trait deleted from `moa-core/src/traits.rs`.
- 8 wiki type files deleted from `moa-core/src/types/`.
- `moa-core/src/lib.rs` re-exports pruned.
- `ToolContext` reshaped with graph-stack handles via thin `moa-core` traits (Option A).
- `DeadMemoryStoreShim` deleted from orchestrator crates.
- Built-in tools simplified to unit structs reading from context.
- `cargo build --workspace --exclude moa-memory` clean.

## 7 Acceptance criteria

1. `rg "trait MemoryStore" crates/` returns 0 hits.
2. `rg "WikiPage|MemoryPath|PageType|IngestReport|MemorySearchMode|MemorySearchResult|PageSummary|ConfidenceLevel" crates/ --type rust | grep -v "crates/moa-memory/"` returns 0 hits.
3. `rg "DeadMemoryStoreShim" crates/` returns 0 hits.
4. `cargo build --workspace --exclude moa-memory` is clean.
5. `cargo build --workspace` fails **only** with errors in `crates/moa-memory/`.
6. `cargo test --workspace --exclude moa-memory` green.
7. `ToolContext` no longer has a `memory_store` field.

## 8 Tests

```sh
cargo build --workspace --exclude moa-memory
cargo test --workspace --exclude moa-memory

# Confirm the only failures are in moa-memory:
cargo build --workspace 2>&1 | grep "^error" | grep -v "crates/moa-memory/" | wc -l   # expect 0
```

## 9 Cleanup

- `cargo fmt && cargo clippy --workspace --exclude moa-memory -- -D warnings` clean.
- Audit doc imports — any markdown in `docs/` that referenced `moa-core::WikiPage` etc. should be updated or deleted.
- `architecture.md` removed mentions of the wiki types.

## 10 What's next

**C06 — Delete `moa-memory` crate.** With nothing depending on it, the legacy crate is safe to remove from the workspace.
