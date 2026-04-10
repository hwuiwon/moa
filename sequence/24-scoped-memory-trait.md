# Step 24: Scoped MemoryStore Trait + memory_write Fix

## What this step is about
The `MemoryStore` trait has an inconsistency: `search`, `list_pages`, `get_index`, and `rebuild_search_index` all take `MemoryScope`, but `read_page`, `write_page`, and `delete_page` do not. This causes ambiguity when the same logical path exists in both user and workspace scopes, and prevents the `memory_write` tool from creating new pages in a caller-selected scope.

This step fixes the trait by adding `MemoryScope` to the remaining three methods, updates all implementations and callers, and unlocks `memory_write` as a full create-or-update tool.

## Files to read
- `moa-core/src/traits.rs` — current `MemoryStore` trait definition
- `moa-core/src/types.rs` — `MemoryPath`, `MemoryScope`, `WikiPage`
- `moa-memory/src/lib.rs` — `FileMemoryStore` implementation, including `read_page_in_scope`, `write_page_in_scope`, `delete_page_in_scope` workarounds
- `moa-hands/src/tools/memory.rs` — `MemoryReadTool`, `MemoryWriteTool`, `MemorySearchTool`
- `moa-brain/src/pipeline/memory.rs` — Stage 5 memory retriever (calls `read_page`)
- `moa-brain/src/pipeline/mod.rs` — pipeline preload (calls `read_page`)
- `moa-memory/tests/memory_store.rs` — existing memory tests
- `moa-skills/src/lib.rs` — SkillRegistry calls to memory store

## Goal
Every `MemoryStore` method that operates on a page takes `MemoryScope`, eliminating ambiguity. `memory_write` can create new pages in a specified scope. The `_in_scope` workaround methods on `FileMemoryStore` become unnecessary.

## Rules
- The trait change must be **scope-per-method** (add `scope: MemoryScope` to `read_page`, `write_page`, `delete_page`), not a scoped-handle pattern. Rationale: the scoped-handle pattern is architecturally cleaner but would require restructuring every callsite and mock in the codebase right now. The scope-per-method approach is the minimal correct fix that aligns all methods consistently.
- Do NOT break the existing public API on `FileMemoryStore` until the trait methods fully replace the `_in_scope` helpers.
- All existing tests must pass after the migration.
- `memory_write` must support creating new pages (not just updating existing ones) when a `scope` is provided.
- When `memory_write` is called without a `scope` and the page doesn't exist, return a clear tool error explaining that scope is required for new pages.

## Tasks

### 1. Update `MemoryStore` trait in `moa-core/src/traits.rs`
Change the three methods:
```rust
async fn read_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<WikiPage>;
async fn write_page(&self, scope: MemoryScope, path: &MemoryPath, page: WikiPage) -> Result<()>;
async fn delete_page(&self, scope: MemoryScope, path: &MemoryPath) -> Result<()>;
```

### 2. Update `FileMemoryStore` in `moa-memory/src/lib.rs`
- Update the trait impl methods to use the scope parameter directly (they should now just delegate to the existing `_in_scope` logic).
- Keep `read_page_in_scope`, `write_page_in_scope`, `delete_page_in_scope` as deprecated pub methods that forward to the trait methods. Remove only after all downstream callers are migrated.

### 3. Update all callers of `read_page`, `write_page`, `delete_page`
Grep all callers across the workspace. Each needs a scope. Key locations:
- `moa-brain/src/pipeline/mod.rs` — `preload_memory_stage_data` reads pages after search; the search result already tells us which scope the page was found in. Thread that scope through.
- `moa-brain/src/pipeline/memory.rs` — if it calls `read_page` directly
- `moa-hands/src/tools/memory.rs` — `MemoryReadTool` and `MemoryWriteTool`
- `moa-skills/src/lib.rs` — `SkillRegistry` reads skill pages
- `moa-memory/src/consolidation.rs` — consolidation reads/writes pages
- `moa-memory/src/ingest.rs` — ingest reads/writes pages
- `moa-memory/src/branching.rs` — branch reconciliation reads/writes pages

For callers where the scope is not immediately obvious (like `MemoryReadTool`), add a scope resolution strategy: try workspace first, then user, or require the caller to specify. For `MemoryReadTool`, default to workspace scope but accept an optional `scope` parameter in the tool schema.

### 4. Upgrade `memory_write` to support page creation
- When `scope` is provided in the tool input: use it directly; create the page if it doesn't exist.
- When `scope` is NOT provided: try to read the existing page (workspace first, then user). If found, update in the scope where it was found. If not found, return a tool error asking the agent to specify a scope.
- Add `scope` as a required-for-creation parameter in the tool's description text so the LLM knows when to provide it.

### 5. Update `MemorySearchResult` to carry scope
`MemorySearchResult` should include a `scope` field (or `scope_label: String`) so that when a search result is used to read the full page, the caller knows which scope to pass. Check if this is already present; if not, add it.

### 6. Update all mocks and tests
- Update `MockMemoryStore` in `moa-brain/src/pipeline/mod.rs` tests
- Update `MockMemoryStore` in any other test files
- Update `moa-memory/tests/memory_store.rs`
- Update `moa-hands/tests/local_tools.rs` if it tests memory tools

## Deliverables
```
moa-core/src/traits.rs          # Updated MemoryStore trait
moa-core/src/types.rs           # MemorySearchResult with scope (if needed)
moa-memory/src/lib.rs           # Updated FileMemoryStore impl
moa-hands/src/tools/memory.rs   # Updated tools with scope support
moa-brain/src/pipeline/mod.rs   # Updated preload callers
moa-brain/src/pipeline/memory.rs # Updated stage 5 callers
moa-skills/src/lib.rs           # Updated skill registry callers
moa-memory/src/consolidation.rs # Updated consolidation callers
moa-memory/src/ingest.rs        # Updated ingest callers
moa-memory/src/branching.rs     # Updated branching callers
```

## Acceptance criteria
1. `MemoryStore::read_page`, `write_page`, `delete_page` all take `MemoryScope` as a parameter.
2. `memory_write` can create a new page when `scope` is provided.
3. `memory_write` returns a clear error when `scope` is omitted and the page doesn't exist.
4. `memory_read` accepts an optional `scope` parameter; defaults to workspace, falls back to user.
5. `FileMemoryStore`'s `_in_scope` helpers are deprecated or removed.
6. All existing tests pass.
7. No ambiguity errors from the trait-level methods — scope is always explicit.

## Tests

**Unit tests in `moa-memory`:**
- `read_page` with explicit user scope returns user-scoped page
- `read_page` with explicit workspace scope returns workspace-scoped page
- `write_page` creates a new page in the specified scope
- `write_page` updates an existing page in the specified scope
- `delete_page` deletes only from the specified scope
- Same logical path in both scopes: each scope returns its own page

**Unit tests in `moa-hands` (memory tools):**
- `memory_write` with `scope: "workspace"` creates a new page → success
- `memory_write` without `scope` on an existing page → updates it
- `memory_write` without `scope` on a non-existent page → tool error with helpful message
- `memory_read` without `scope` → falls back across scopes
- `memory_read` with explicit `scope` → reads only from that scope

**Integration (pipeline):**
- Pipeline stage 5 still loads memory correctly after the trait change
- Brain turn with memory_write tool call creates a page in workspace scope

```bash
cargo test -p moa-core
cargo test -p moa-memory
cargo test -p moa-hands
cargo test -p moa-brain
cargo test -p moa-skills
```
