# Implementation Caveats

Implementation notes and design caveats surfaced while building the current MOA workspace. These are not necessarily bugs, but they are places where the current trait surface or helper behavior is awkward enough to review before later steps build on top of them.

## 1. `MemoryStore` trait cannot express scoped reads or writes cleanly

Current trait:

```rust
async fn read_page(&self, path: &MemoryPath) -> Result<WikiPage>;
async fn write_page(&self, path: &MemoryPath, page: WikiPage) -> Result<()>;
async fn delete_page(&self, path: &MemoryPath) -> Result<()>;
```

Issue:

- `search`, `list_pages`, `get_index`, and `rebuild_search_index` all take `MemoryScope`.
- `read_page`, `write_page`, and `delete_page` do not.
- The same logical path can validly exist in both scopes, for example `topics/preferences.md` in user memory and workspace memory.

Consequence:

- The trait does not let an implementation know which scope the caller intended.
- The current `FileMemoryStore` works around this by exposing explicit scoped helpers:
  - `read_page_in_scope`
  - `write_page_in_scope`
  - `delete_page_in_scope`
- The trait-level methods only work when the path resolves to exactly one scope. If the path exists in both scopes, they return an ambiguity error.

Recommended review:

- Consider changing the trait to one of these shapes:
  - `read_page(&self, scope: MemoryScope, path: &MemoryPath)`
  - `read_page(&self, reference: ScopedMemoryPath)`
- The same change should apply to `write_page` and `delete_page`.

## 2. `ContextProcessor` being synchronous forces async preloading outside the stages

Current trait:

```rust
pub trait ContextProcessor: Send + Sync {
    fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput>;
}
```

Issue:

- Stage 5 memory retrieval needs async I/O.
- Stage 6 history loading also needs async I/O.
- Because `process()` is synchronous, the stage itself cannot call async storage APIs.

Consequence:

- The pipeline runner now has to preload async data into `WorkingContext.metadata` before running those stages.
- This works, but it spreads stage behavior across two places:
  - preload logic in `moa-brain/src/pipeline/mod.rs`
  - formatting logic in the individual stage module

Recommended review:

- Consider changing `ContextProcessor::process()` to `async fn process(...)`.
- If the project wants to keep synchronous processors for simplicity, then the preload pattern should probably be formalized instead of being ad hoc metadata keys.

## 3. Metadata-key coupling in the pipeline is now part of the design

Current Stage 5 and Stage 6 depend on internal metadata keys:

- `moa.pipeline.memory_stage_data`
- `moa.pipeline.history_events`

Issue:

- These keys are stringly typed.
- There is no typed contract between the pipeline runner and the processors beyond serde round-tripping through `Value`.

Consequence:

- Refactors can break a stage silently if the key or payload shape changes.
- The approach is serviceable for now, but it is fragile as more stages start preloading external state.

Recommended review:

- Consider a typed `PipelinePreload` struct on `WorkingContext` instead of raw metadata for internal runner-to-stage coordination.

## 4. `FileMemoryStore::from_config()` assumes `local.memory_dir` has a parent base directory

Current behavior:

- `FileMemoryStore::from_config()` derives the MOA base directory from the parent of `local.memory_dir`.
- With the current config defaults, that works because `local.memory_dir` is `~/.moa/memory`.

Issue:

- This assumption is implicit rather than expressed in config shape.
- A custom `local.memory_dir` without the expected layout could make the derived workspace roots surprising.

Consequence:

- The user memory root and workspace memory root are coupled to the derived base dir rather than configured independently.

Recommended review:

- Either keep this as a documented convention, or add explicit config fields for:
  - user memory root
  - workspace memory root
  - search DB path

## 5. Search result ranking is reasonable but still heuristic

Current ranking in the FTS query boosts:

- recent pages
- high-confidence pages
- high-reference-count pages

Issue:

- This is not yet validated against real memory usage patterns.
- The weighting is implementation judgment, not something explicitly tuned in the spec.

Consequence:

- Search works and tests pass, but result ordering may want adjustment once real memory accumulates.

Recommended review:

- Revisit ranking once Step 05+ usage produces realistic memory corpora.

## 6. `graphify.watch._rebuild_code()` is currently stale against `graphify.detect.detect()`

Issue:

- The documented helper command:

```bash
python3 -c "from graphify.watch import _rebuild_code; from pathlib import Path; _rebuild_code(Path('.'))"
```

  currently fails because `_rebuild_code()` expects an older `detect()` return shape.

Consequence:

- Graph refresh still works, but only via a manual rebuild path.
- This is workflow friction rather than an application bug.

Recommended review:

- Update the `graphify.watch` helper to read `detected["files"]["code"]` instead of indexing by `FileType.CODE`.

## 7. The current file-memory implementation is local-only

Current state:

- `FileMemoryStore` uses the local filesystem for markdown pages.
- FTS uses a local libSQL/SQLite database.

Issue:

- This matches the current milestone, but not the eventual cloud model described in the architecture docs.

Consequence:

- The local design is correct for Step 05, but later cloud work should avoid assuming the same storage topology or write model.

Recommended review:

- Treat the current implementation as the local reference implementation, not the final cloud memory architecture.

## 8. `ToolOutput` is shell-oriented, so built-in tools currently flatten rich results into text

Current shared type:

```rust
pub struct ToolOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration: Duration,
}
```

Issue:

- This shape is a good fit for `bash`, but a less natural fit for higher-level tools like:
  - `memory_search`
  - `memory_write`
  - future MCP-backed tools
- Those tools often want to return structured data, snippets, or references rather than a process-style stdout/stderr split.

Consequence:

- The current built-in tool implementations serialize their meaningful result into `stdout`.
- The brain then records that flattened string in `Event::ToolResult`.
- This is workable, but it loses structure that may matter later for richer UI rendering or better tool-result conditioning.

Recommended review:

- Consider a richer cross-crate tool result shape later, for example:
  - a human-readable summary string
  - an optional structured JSON payload
  - optional stdout/stderr fields only for process-backed tools

## 9. `memory_write` now exposes the `MemoryStore` scoped-write gap at the tool layer

Current state:

- The built-in `memory_write` tool exists and is registered in the default loadout.
- It calls into the shared `MemoryStore` trait.

Issue:

- Because `MemoryStore::write_page()` still lacks `MemoryScope`, the tool cannot generically create a new page in a caller-selected scope.
- The tool can only safely update an existing page that already resolves uniquely through the current trait surface.

Consequence:

- `memory_write` is intentionally limited for now.
- It succeeds for existing uniquely resolvable pages.
- It returns a tool error when the target page does not already exist or is ambiguous across scopes.

Recommended review:

- This becomes fully fixable when the trait issue from caveat 1 is addressed.
- Until then, treat `memory_write` as an update-oriented tool rather than a general create/update API.

## 10. Docker-backed local hands still execute file tools on the host-mounted sandbox

Current state:

- `LocalHandProvider` provisions a Docker container when `SandboxTier::Container` is requested and Docker is available.
- `bash` runs inside that container.

Issue:

- The current `HandHandle::Docker` only carries a `container_id`.
- The file tools need both:
  - the sandbox filesystem path
  - deterministic path validation
- To keep the implementation simple without changing the shared hand handle shape, `file_read`, `file_write`, and `file_search` currently execute against the mounted sandbox directory on the host even when a Docker hand exists.

Consequence:

- Docker-backed local hands are only partially containerized in Step 06.
- Command execution is containerized.
- File tools are still sandboxed, but the sandboxing is host-path-based rather than `docker exec` based.

Recommended review:

- Later options:
  - enrich the hand handle / runtime state so Docker file tools can execute inside the container cleanly
  - or make the distinction explicit in the design and keep file tools host-side by policy

## 11. The default tool registry currently lives in `moa-hands`, not in a shared core surface

Current state:

- `ToolRegistry`, `ToolDefinition`, and the built-in tool handler abstraction were added in `moa-hands`.

Issue:

- The architecture docs clearly describe a tool registry, but the current stable shared core traits do not define one.
- That means:
  - stage 3 tool schemas come from `moa-hands`
  - the brain harness depends on the concrete `ToolRouter`
  - there is not yet a crate-agnostic registry interface in `moa-core`

Consequence:

- This is acceptable for the local Step 06 milestone.
- It is a place to review before cloud hands, MCP discovery, and policy engines grow more complex.

Recommended review:

- Once the registry shape stabilizes, consider promoting the shared registry-facing types or traits into `moa-core`.
