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

## 12. Persistent approval matching is still string-normalization based

Current state:

- Approval rules are stored as `(workspace_id, tool, pattern, action)`.
- Matching is performed against a normalized string representation of the tool input.
- For `bash`, the normalization parses only a single command segment and rejects chained commands for rule matching.

Issue:

- This is intentionally safer than matching the raw shell string, but it is still not a full semantic command policy engine.
- Equivalent shell inputs may normalize differently depending on quoting or wrapper structure.
- Non-shell tools currently rely on tool-specific normalization and summaries rather than a shared typed policy input model.

Consequence:

- The current implementation correctly avoids obvious bypasses like matching `npm test && rm -rf /` against an `npm test*` rule.
- It is also intentionally conservative: some safe inputs may fail to match an existing rule and require approval again.

Recommended review:

- If approval rules become a major surface area, consider promoting tool-specific normalized policy inputs into shared types rather than matching plain strings.

## 13. Step 07 persists only workspace-scoped approval rules, even though the type system allows more

Current state:

- `PolicyScope` includes both `Workspace` and `Global`.
- The current brain and router only create workspace-scoped rules for `AlwaysAllow`.
- Rule lookup includes global rows if they exist, but the current flow does not create or manage them.

Issue:

- The data model is slightly ahead of the product behavior.
- There is no current UX or API distinction between workspace-local and global persistent approvals.

Consequence:

- The implementation satisfies the current step requirement of per-workspace persistence.
- Global scope exists in the types and storage layer but should be treated as reserved for later work, not as a finished feature.

Recommended review:

- Decide whether global approval rules are actually part of the intended product surface before later clients start depending on the enum value.

## 14. Approval resume logic currently replays the session event log

Current state:

- On each turn, the brain scans the session event log to find:
  - unresolved approval requests
  - approval decisions that unblock a pending tool call
  - completed tool executions

Issue:

- This keeps the implementation simple and faithful to the event log, but it derives approval state by replay rather than reading a materialized current-state record.

Consequence:

- The design is correct for the current milestone and small local sessions.
- As sessions grow, repeatedly scanning the full event history on each turn may become a noticeable cost, especially once compaction and longer tool loops arrive.

Recommended review:

- Later options:
  - keep replay as the source of truth but add cheap indexed/materialized session state
  - or teach compaction/state recovery to preserve current approval state explicitly

## 15. Policy enforcement is intentionally duplicated in the brain and the tool router

Current state:

- The brain checks policy first so it can emit `ApprovalRequested` and return `TurnResult::NeedsApproval` before execution.
- The `ToolRouter` also checks policy inside `execute()` as a defense-in-depth guard.

Issue:

- This is the right safety posture for now, but it means policy behavior is expressed in two call paths:
  - pre-execution orchestration in `moa-brain`
  - final dispatch enforcement in `moa-hands`

Consequence:

- If those paths ever diverge, the user-facing event flow and the final execution gate could disagree.
- The current tests cover the main behavior, but the duplication is still structural coupling worth reviewing later.

Recommended review:

- Once the approval flow stabilizes, consider whether policy evaluation should expose one canonical helper/output shape that both layers consume.

## 16. The example chat harness is not yet a real approval UI

Current state:

- The `moa-brain` example harness can drive the live brain/tool loop.
- After Step 07, a turn can now return `TurnResult::NeedsApproval`.

Issue:

- The example harness is still primarily a developer smoke-test harness, not a full approval client.
- It does not yet provide a complete interactive flow for approving or denying pending tool calls.

Consequence:

- Tool-free chats still work as expected.
- Tool-using prompts can now legitimately block on approval without the example offering the full decision UX described in the communication-layer docs.

Recommended review:

- Upgrade the example harness later if it is meant to remain a human-facing debug tool, or keep it intentionally minimal and treat approvals as a TUI/CLI/gateway concern.

## 17. The Step 08 chat runtime duplicates part of the brain loop to expose streaming

Current state:

- `moa-brain::run_brain_turn_with_tools()` is the canonical buffered harness.
- The new Step 08 `ChatRuntime` in `moa-tui` reimplements the compile → stream → tool/approval → continue loop locally.

Issue:

- This duplication exists because the current harness emits only final `BrainResponse` events after collection, while the TUI and `moa exec` need live streamed deltas.
- The duplicated loop is intentionally close to the harness, but it is still a second implementation of overlapping orchestration logic.

Consequence:

- The TUI and exec mode now satisfy the streaming requirement.
- It also means future changes to turn execution need to keep the buffered harness and the streamed runtime in sync unless the architecture converges on one shared streamed primitive.

Recommended review:

- When the orchestrator work settles, consider extracting a shared streamed turn engine instead of maintaining both a buffered and a TUI-specific turn loop.

## 18. Step 08 cancellation is task-abort based, not provider-native cancellation

Current state:

- `Ctrl+C` and `Escape` abort the running turn task in the TUI.
- This immediately returns control to the UI and stops further rendering for that turn.

Issue:

- The cancellation does not currently propagate a provider-native cancel request into Anthropic streaming or a richer session signal into the buffered brain harness.

Consequence:

- The visible user behavior is correct for the current step: generation stops and the UI becomes responsive again.
- The underlying request may still terminate by task abortion rather than a graceful model/provider stop path.

Recommended review:

- Later work should decide whether cancellation belongs at the session/orchestrator layer, the provider layer, or both.

## 19. `/clear` and `/model <name>` start a fresh session instead of preserving transcript continuity

Current state:

- `/clear` clears the visible transcript and creates a new empty session.
- `/model <name>` also starts a new session after switching the default model.

Issue:

- This is the simplest way to keep the UI display consistent with the actual prompt context for a single-session Step 08 client.
- It does mean these commands are implemented as session replacement, not in-place state mutation.

Consequence:

- The visible transcript always matches the real context sent to the model after these commands.
- Old session history is not surfaced in the current TUI because tabs/session switching are later milestones.

Recommended review:

- Revisit once multi-session/tab support lands so `/clear` and model switching can have a clearer relationship to session history and persistence.

## 20. The local CLI/TUI still assume the configured local paths are writable

Current state:

- The Step 08 runtime uses the configured local defaults:
  - `~/.moa/sessions.db`
  - `~/.moa/memory`
  - `~/.moa/sandbox`

Issue:

- This is correct for real local usage.
- In restricted environments such as the current sandbox, those paths may not be writable even when the implementation itself is correct.

Consequence:

- `moa exec` works end to end when the local paths are writable or overridden through config/env.
- In locked-down environments, the runtime needs config overrides (for example `MOA__LOCAL__SESSION_DB`, `MOA__LOCAL__MEMORY_DIR`, `MOA__LOCAL__SANDBOX_DIR`) to run.

Recommended review:

- Decide later whether the binaries should keep failing fast on unwritable local state, or offer a documented temporary-directory fallback for constrained environments.
