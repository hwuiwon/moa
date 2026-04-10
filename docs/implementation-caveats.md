# Implementation Caveats

Implementation notes and design caveats surfaced while building the current MOA workspace. These are not necessarily bugs, but they are places where the current trait surface or helper behavior is awkward enough to review before later steps build on top of them.

Caveats are grouped by root cause / architectural boundary, not by the crate where the symptom first appears. Fixing the root of a group typically unblocks every caveat in it.

## 1. Memory trait scope & scoped writes

### 1.1 `MemoryStore` trait cannot express scoped reads or writes cleanly

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

### 1.2 `memory_write` now exposes the `MemoryStore` scoped-write gap at the tool layer

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

- This becomes fully fixable when the trait issue from 1.1 is addressed.
- Until then, treat `memory_write` as an update-oriented tool rather than a general create/update API.

## 2. Context pipeline async + typing

### 2.1 `ContextProcessor` being synchronous forces async preloading outside the stages

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

### 2.2 Metadata-key coupling in the pipeline is now part of the design

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

## 3. Local config & path assumptions

### 3.1 `FileMemoryStore::from_config()` assumes `local.memory_dir` has a parent base directory

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

### 3.2 The current file-memory implementation is local-only

Current state:

- `FileMemoryStore` uses the local filesystem for markdown pages.
- FTS uses a local libSQL/SQLite database.

Issue:

- This matches the current milestone, but not the eventual cloud model described in the architecture docs.

Consequence:

- The local design is correct for Step 05, but later cloud work should avoid assuming the same storage topology or write model.

Recommended review:

- Treat the current implementation as the local reference implementation, not the final cloud memory architecture.

### 3.3 The local CLI/TUI still assume the configured local paths are writable

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

## 4. Tool result & registry surfaces in core

### 4.1 `ToolOutput` is shell-oriented, so built-in tools currently flatten rich results into text

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

### 4.2 The default tool registry currently lives in `moa-hands`, not in a shared core surface

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

## 5. Sandbox containerization completeness

### 5.1 Docker-backed local hands still execute file tools on the host-mounted sandbox

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

## 6. Approval policy engine

### 6.1 Persistent approval matching is still string-normalization based

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

### 6.2 Step 07 persists only workspace-scoped approval rules, even though the type system allows more

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

### 6.3 Approval resume logic currently replays the session event log

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

### 6.4 Policy enforcement is intentionally duplicated in the brain and the tool router

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

### 6.5 Persisted approval events do not contain the full rich approval payload

Current state:

- The event log stores `ApprovalRequested` with:
  - `request_id`
  - `tool_name`
  - `input_summary`
  - `risk_level`
- The live TUI approval widget also needs:
  - parsed parameter fields
  - diff previews
  - the exact normalized pattern used for "Always Allow"

Issue:

- That richer approval prompt exists in live `RuntimeEvent::ApprovalRequested`, but not in the persisted `Event::ApprovalRequested`.
- When the TUI reconstructs an older session from the event log alone, it can only rebuild a minimal approval card.

Consequence:

- Switching tabs during the same app session keeps the full rich approval card because the view cache preserves it.
- Rehydrating a waiting-for-approval session from storage alone loses the exact diff/pattern fidelity of the original live prompt.

Recommended review:

- Consider either enriching the stored approval event shape later or storing a separate durable approval payload keyed by `request_id`.

## 7. Approval UI in TUI

### 7.1 The example chat harness is not yet a real approval UI

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

### 7.2 The Step 09 diff viewer is driven by TUI-local approval payloads, not shared core events

Current state:

- The session event log still stores the approval request as:
  - tool name
  - input summary
  - risk level
- The richer diff payload used by the TUI lives only in `moa-tui::runner::ApprovalPrompt`.

Issue:

- This keeps Step 09 scoped correctly to the TUI and avoids changing the stable core event schema.
- It also means the diff preview is a client-side enhancement, not a replayable property of the persisted approval event.

Consequence:

- The TUI can render inline diff previews and a full-screen diff viewer during a live approval.
- A later client that replays old approvals from the event log will not have enough information to reconstruct the same diff UI without re-deriving it from tool input and current filesystem state.

Recommended review:

- Decide later whether rich approval context should remain a client-local concern or be promoted into a shared replayable approval payload. Related: 6.5.

### 7.3 The current diff viewer only has first-class semantics for `file_write`

Current state:

- Step 09 derives diff previews only for `file_write`.
- Other approval types still render structured parameters and risk coloring, but no diff.

Issue:

- This matches the current requirement, which is specifically about file-write approvals and diff previews.
- It does not yet generalize to:
  - multi-file write batches
  - patch-oriented tools
  - future tools that mutate structured state without mapping cleanly to one text file

Consequence:

- The current diff experience is good for the existing built-in file write flow.
- Future write tools will need their own approval preview model instead of assuming the same before/after text-file shape.

Recommended review:

- Introduce a richer "approval preview" abstraction later if additional mutating tools need specialized visualizations.

### 7.4 `e` is still a placeholder, not a real approval-parameter editor

Current state:

- The Step 09 approval widget shows the documented `e` shortcut.
- Pressing it currently surfaces a status message instead of opening an editor.

Issue:

- The shortcut is present so the approval UI matches the spec surface and does not paint the implementation into a different keyboard contract.
- The actual parameter-editing workflow has not been built yet.

Consequence:

- The important approval flow works:
  - allow once
  - deny
  - always allow
  - open diff
- Parameter editing should still be considered unimplemented, not partially complete.

Recommended review:

- Treat `e` as reserved UI space until there is a concrete design for editing, validating, and resubmitting tool inputs safely.

## 8. Streaming runtime vs. trait surface

### 8.1 The Step 08 chat runtime duplicates part of the brain loop to expose streaming

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

### 8.2 Live runtime observation is richer than the `BrainOrchestrator` trait surface

Current state:

- The stable trait still exposes `observe()` for persisted `EventRecord` streaming.
- The TUI and CLI now also rely on a concrete `LocalOrchestrator::observe_runtime()` stream of `RuntimeEvent` values for:
  - incremental assistant text
  - inline tool-card updates
  - rich approval prompts

Issue:

- This keeps the generic orchestrator trait small and storage-oriented.
- It also means the local UI uses a local-only extension API that does not exist on `BrainOrchestrator` yet.

Consequence:

- The local TUI and CLI can preserve the Step 08 streaming UX after moving onto the orchestrator.
- A future remote/daemon client will need either the same runtime stream promoted into a shared trait or a different transport-level observation contract.

Recommended review:

- Decide later whether live UI/runtime events belong in the stable orchestrator trait, in a separate observation trait, or purely in transport-specific adapters.

## 9. Cancellation semantics

### 9.1 Step 08 cancellation is task-abort based, not provider-native cancellation

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

### 9.2 Hard cancel is immediate at the task level, not cooperative inside tools

Current state:

- `HardCancel` aborts the session task from the orchestrator and marks the session cancelled.
- `SoftCancel` is cooperative and stops after the current step.

Issue:

- This gives the correct user-facing distinction for Step 10:
  - soft stop after current work
  - hard stop immediately
- It does not guarantee cleanup inside an already-running external command beyond what Tokio task abortion naturally interrupts.

Consequence:

- The local UX behaves correctly for cancellation semantics at the session level.
- If a future hand backend needs strict process cleanup guarantees, cancellation will need to become hand-aware instead of only task-aware.

Recommended review:

- Revisit hard-cancel semantics when remote hands / containerized hands become first-class, especially for orphan-process cleanup.

### 9.3 Resuming a cancelled session now waits for fresh input instead of auto-continuing old tail work

Current state:

- `resume_session()` no longer auto-runs persisted tail events when the stored session status is `Cancelled`.
- This prevents a soft-stopped tool call from later resuming into an assistant response just because the session was reopened.

Issue:

- The architecture docs describe `resume_session()` as “wake from last event,” but they do not say whether a user-stopped session should continue the interrupted turn or remain stopped until new input arrives.

Consequence:

- Current behavior treats stop as authoritative: reopening or reattaching to a cancelled session leaves it idle.
- The next turn starts only after a new `QueueMessage` or user prompt arrives.
- If later product semantics want “pause and resume the interrupted turn,” this behavior will need to change.

Recommended review:

- Decide explicitly whether `Cancelled` means:
  - permanently stop the interrupted turn until fresh user input, or
  - pause execution and allow `resume_session()` to continue where it left off.

## 10. Session lifecycle & orchestrator gaps

### 10.1 `/clear` and `/model <name>` start a fresh session instead of preserving transcript continuity

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

### 10.2 The Step 10 `LocalOrchestrator` is multi-session in-process, not yet a daemon

Current state:

- `LocalOrchestrator` now keeps multiple session actors alive as Tokio tasks inside the current process.
- The TUI and `moa exec` both talk to that in-process orchestrator.

Issue:

- This satisfies the Step 10 requirement that sessions survive across TUI views and are no longer tied to one foreground turn task.
- It does not yet satisfy the later daemon shape described in `docs/03-communication-layer.md`, where the orchestrator survives closing the TUI entirely.

Consequence:

- Switching UI views or holding multiple sessions in one process works.
- Exiting the current process still stops all local session actors.

Recommended review:

- Introduce the planned local daemon / Unix socket boundary later instead of treating the current in-process orchestrator as the final local architecture.

### 10.3 Local cron scheduling is real, but task execution is still a logging stub

Current state:

- `schedule_cron()` now uses `tokio-cron-scheduler`.
- The scheduled job currently logs the requested job/task identity.

Issue:

- This is enough to validate the orchestrator wiring and return real local cron handles.
- It is not yet connected to memory consolidation, skill improvement, or any other concrete background job implementation.

Consequence:

- The scheduling surface exists and is testable.
- The actual cron work remains to be implemented in later phases.

Recommended review:

- Keep treating local cron as infrastructure plumbing until the first concrete maintenance job is wired through it.

### 10.4 The TUI only keeps one live runtime observer attached at a time

Current state:

- Step 11 added tabbed multi-session viewing in the TUI.
- The UI now keeps exactly one live runtime subscription attached to the currently selected session.
- Background sessions continue running in the `LocalOrchestrator`, and their tab/status metadata is refreshed by polling the session store.

Issue:

- This is the smallest design that satisfies the Step 11 requirement that switching tabs does not kill running sessions.
- It does not provide simultaneous live transcript streaming for every open tab.

Consequence:

- The selected tab shows incremental assistant/tool/approval updates in real time.
- Background tabs keep accurate coarse status, but their transcripts only catch up when the user switches back to them.

Recommended review:

- Decide later whether the TUI should stay single-observer for simplicity or maintain a lightweight live observer per visible session.

### 10.5 Session picker previews currently use an N+1 query pattern

Current state:

- The session picker shows workspace, status, and a last-message preview.
- `ChatRuntime::list_session_previews()` loads:
  - the session summaries
  - then a small recent event slice per session to derive the preview text

Issue:

- This is reasonable for the current local TUI scale.
- It is not an especially efficient listing strategy if the number of sessions grows large.

Consequence:

- The picker is accurate and simple now.
- Large local histories may eventually want denormalized preview fields or a batched query path.

Recommended review:

- Revisit preview derivation once local users have enough sessions for picker latency to matter.

### 10.6 Prompt draft state is global to the TUI, not per session tab

Current state:

- Step 11 introduced multiple session tabs.
- The compose box is still a single shared prompt widget for the whole TUI process.

Issue:

- This keeps the app state much smaller and simpler.
- It means switching tabs does not preserve a separate in-progress draft per session.

Consequence:

- Multi-session chat works cleanly.
- Per-tab draft preservation remains a UX gap for later refinement.

Recommended review:

- Decide later whether draft text should move into per-session UI state once the basic session-management workflow has settled.

### 10.7 Queued prompts are buffered in memory until the current turn boundary

Current state:

- A prompt queued while a session is actively running is no longer written to the event log immediately.
- The orchestrator now buffers queued prompts in memory and flushes them as `QueuedMessage` events only after the current turn finishes.

Issue:

- This fixes the provider-facing conversation ordering bug where a queued user message could be persisted before the in-flight assistant reply, causing the next Anthropic request to end with an assistant message.
- It introduces a small durability gap: if the local process crashes after the user queues a prompt but before the current turn reaches its flush point, that queued prompt is lost.

Consequence:

- Normal queued follow-ups now produce correct event ordering and valid Anthropic request bodies.
- Crash recovery for in-flight queued prompts is weaker than the rest of the session-log-based design until a durable side queue or explicit pending-signal store exists.

Recommended review:

- Decide later whether queued prompts should move into a durable pending-signal table so MOA can keep both correct conversational ordering and crash-safe queue persistence.

## 11. Search ranking

### 11.1 Search result ranking is reasonable but still heuristic

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

## 12. Skills system

### 12.1 Skill metadata now depends on arbitrary frontmatter preservation in `WikiPage`

Current state:

- Step 12 adds real `SKILL.md` parsing with fields such as:
  - `name`
  - `version`
  - `one_liner`
  - `tools_required`
  - `moa.estimated_tokens`
- The shared `WikiPage` type now carries a generic `metadata: HashMap<String, serde_json::Value>`.
- `moa-memory` preserves any non-core YAML frontmatter fields in that map so skill pages can round-trip through the memory layer without losing their skill-specific metadata.

Issue:

- This works well for the current local file-backed memory store.
- It also makes the skills system depend on memory implementations preserving arbitrary frontmatter, not just the typed core wiki fields.

Consequence:

- Stage 4 skill injection, registry loading, and skill self-improvement all work on the current stack.
- A future alternative `MemoryStore` implementation that drops or normalizes unknown frontmatter fields would silently break skill metadata fidelity unless it also preserves `WikiPage.metadata`.

Recommended review:

- Treat `WikiPage.metadata` as part of the required memory contract going forward, or formalize a dedicated typed metadata channel for specialized page types such as skills.

### 12.2 Skill creation is still tied to the concrete `FileMemoryStore`, not the generic `MemoryStore` trait

Current state:

- The read path for skills uses `MemoryStore` through `SkillRegistry`.
- The write path for distillation and self-improvement uses `FileMemoryStore::write_page_in_scope()`.

Issue:

- This is still a direct consequence of the earlier `MemoryStore` trait gap:
  - `search`, `list_pages`, and `get_index` are scoped
  - `read_page`, `write_page`, and `delete_page` are not
- Creating a brand-new workspace-scoped skill page cannot be expressed safely through the current trait alone.

Consequence:

- Post-run skill distillation is implemented in the local orchestrator, which already owns a concrete `Arc<FileMemoryStore>`.
- The generic single-turn brain harness still does not own enough information to persist a new skill through the trait surface alone.

Recommended review:

- If skills are meant to be a first-class cross-runtime feature, the `MemoryStore` trait should eventually grow scoped read/write/delete operations so distillation is not pinned to the local filesystem implementation.

### 12.3 Skill lookup currently assumes skills live in workspace scope only

Current state:

- The registry loads skills from `MemoryScope::Workspace(...)`.
- Skill paths are canonicalized under `skills/{slug}/SKILL.md`.

Issue:

- The current lookup path still uses `MemoryStore::read_page(path)` after listing workspace skill summaries.
- That works as long as the same logical skill path does not also exist in user scope.

Consequence:

- The current implementation matches the documented workspace-oriented skill lifecycle and passes all tests.
- If user-scoped skills are introduced later with the same path layout, the existing read path could become ambiguous.

Recommended review:

- Keep skills workspace-scoped, or solve the broader scoped-read problem before adding user-scoped skills with overlapping logical paths.

### 12.4 MOA now treats the Agent Skills format as the only supported on-disk `SKILL.md` shape

Current state:

- MOA now parses and renders `SKILL.md` files using the Agent Skills layout from `agentskills.io`.
- The top-level skill fields are the spec-style ones such as:
  - `name`
  - `description`
  - optional `compatibility`
  - optional `allowed-tools`
  - optional `metadata`
- MOA-specific bookkeeping such as versioning, success metrics, and token estimates is stored under `metadata` keys prefixed with `moa-`.

Issue:

- The earlier custom MOA-only top-level frontmatter shape is no longer accepted.
- Skills seeded manually on disk must follow the spec-compatible shape or they will fail to load.

Consequence:

- The runtime, tests, and docs now all align on a single external format.
- Skill interoperability with other Agent Skills-aware clients is better.
- Any older local skill drafts that still use the previous MOA-only top-level fields need to be rewritten once.

Recommended review:

- Keep the runtime strict on the external format unless there is a concrete migration need later.
- If a migration path is ever needed, add an explicit one-shot converter rather than reintroducing dual-format parsing in the hot path.

## 13. Simplification pass

### 13.1 Tool approval metadata now has one source of truth in `ToolRouter`

Current state:

- Tool normalization, approval summaries, default policy actions, always-allow patterns, and file diff previews now come from the tool definition metadata in `moa-hands/src/router.rs`.
- `moa-security` now evaluates policy decisions against a normalized `ToolPolicyInput` prepared by the router instead of re-deriving fields from raw JSON input.
- `moa-orchestrator` and `moa-brain` both consume the router-prepared invocation metadata.

Consequence:

- The earlier drift risk between `moa-security/src/policies.rs` and `moa-orchestrator/src/local.rs` is removed.
- Adding a new tool now requires updating one metadata definition instead of several parallel helper tables.

Remaining caveat:

- Any future non-router execution path must also use `ToolRouter::prepare_invocation()` before applying policy or rendering approval UI. Bypassing the router would reintroduce drift immediately.

### 13.2 The buffered harness now rides on the same streamed completion path as the orchestrator

Current state:

- Shared streamed-turn helpers now live in `moa-brain/src/turn.rs`.
- `stream_completion_response()` is used by both the buffered brain harness and the local orchestrator.
- Approval replay scanning (`find_pending_tool_approval`, `find_pending_approval_request`, `find_resolved_pending_tool_approval`) is also centralized there.

Consequence:

- The buffered `run_brain_turn_with_tools()` path no longer has its own separate provider-drain implementation.
- Streaming behavior, cancellation semantics, and approval replay now share one lower-level implementation.

Remaining caveat:

- The local orchestrator still owns queueing, approval waiting, tool-card runtime updates, and stop semantics because those are session-actor concerns rather than pure turn-stream concerns.
- The turn engine is substantially slimmer, but not fully actor-agnostic yet.

### 13.3 The in-memory skills model now matches the on-disk Agent Skills shape

Current state:

- `SkillFrontmatter` now stores only the spec-shaped fields:
  - `name`
  - `description`
  - optional `license`
  - optional `compatibility`
  - optional `allowed-tools`
  - optional `metadata`
- MOA bookkeeping is derived lazily from `metadata` through helper accessors instead of being stored twice.
- `tools_required` is gone as a stored field; callers now use `allowed_tools`.

Consequence:

- The old normalization layer in `moa-skills/src/format.rs` is much smaller.
- The in-memory and on-disk skill representations now line up directly.
- Round-tripping `SKILL.md` is less surprising because the runtime preserves the same top-level shape it parses.

Remaining caveat:

- MOA-specific behavior still depends on `metadata` keys such as `moa-version`, `moa-one-liner`, and `moa-estimated-tokens`.
- Any code outside `moa-skills` that reaches into raw `metadata` directly is now more fragile than code that goes through the helpers in `format.rs`.

## 14. Step 13 memory maintenance

### 14.1 Consolidation is heuristic, not yet LLM-driven

Current state:

- `moa-memory/src/consolidation.rs` now performs deterministic maintenance locally:
  - relative date normalization
  - port-claim contradiction resolution
  - pruning entities with `metadata.entity_exists = false`
  - confidence decay for old unreferenced pages
  - orphan detection
  - `MEMORY.md` regeneration and `_log.md` append

Consequence:

- Step 13 is implemented and testable without network or provider availability.
- The behavior is stable and cheap, which is appropriate for local hourly maintenance.

Remaining caveat:

- Consolidation currently understands only a narrow set of contradiction patterns and stale-page signals.
- The implementation matches the architectural role, but not the full eventual intelligence implied by the spec’s LLM-maintainer sketch.

### 14.2 Branch writes exist as a concrete file-store feature, but the runtime still writes directly to mainline memory

Current state:

- `moa-memory/src/branching.rs` provides:
  - branch-local writes under `.branches/`
  - a JSON change manifest
  - deterministic reconciliation back into the main scope
- The live tool/runtime path still uses direct `write_page_in_scope()` / `memory_write`, not branch-local writes.

Consequence:

- Concurrent-write isolation is implemented at the store level and covered by tests.
- The production runtime does not yet automatically route session-specific memory writes through branch directories.

Remaining caveat:

- The branch/reconcile model is available, but not yet wired into the orchestrator or tools as the default write path.
- If true concurrent cloud writers become a near-term requirement, the router/runtime should switch to branched writes instead of direct mainline writes.

### 14.3 Scheduled consolidation currently runs for workspace scopes only

Current state:

- `LocalOrchestrator` now registers an hourly maintenance job that calls `FileMemoryStore::run_due_consolidations(...)`.
- That scheduler currently derives scopes from session history and only executes workspace consolidations.

Consequence:

- Shared project memory now gets the expected periodic maintenance.
- The cron hook stays aligned with the existing local store model and avoids duplicating the known local user-scope ambiguity.

Remaining caveat:

- User-scope consolidation is not scheduled yet.
- This is intentional while local user memory still shares one physical `memory/` root regardless of user id, but it means personal memory does not yet get automatic dream-cycle maintenance.

## 15. Step 14 Telegram gateway

### 15.1 `PlatformAdapter` is still message-oriented, so Telegram callbacks are normalized into control messages

Current state:

- `moa-gateway/src/telegram.rs` receives Telegram callback queries for approval buttons.
- The core `PlatformAdapter` trait only emits `InboundMessage`; it does not have a direct signal callback surface for `ApprovalDecided`.
- The adapter therefore converts callback payloads into normalized control text such as `/approval allow <request_id>` and forwards that as an `InboundMessage`.

Consequence:

- Step 14 works without widening the core gateway trait surface.
- Approval button payloads are compact, testable, and future routing layers can parse them deterministically.

Remaining caveat:

- Telegram approval callbacks are not yet first-class typed gateway events.
- A future gateway/orchestrator seam may want a richer event type than `InboundMessage.text` for platform-originated control actions.

### 15.2 Outbound Telegram sends still require a reply anchor

Current state:

- `OutboundMessage` does not carry an explicit destination chat or thread.
- The Telegram adapter resolves where to send by using `reply_to` as an anchor into either:
  - a known inbound Telegram message id, or
  - a previously sent synthetic gateway message id.

Consequence:

- Reply-chain session mapping works for the intended session model.
- The adapter can send, edit, and delete multi-part Telegram replies without changing the existing trait.

Remaining caveat:

- The adapter cannot originate a brand-new top-level Telegram conversation without an existing reply anchor.
- If gateway adapters eventually need to proactively start conversations, `OutboundMessage` or the adapter trait should grow an explicit destination field.

### 15.3 Telegram rendering is intentionally conservative right now

Current state:

- `moa-gateway/src/renderer.rs` renders text, tool cards, approvals, status updates, diffs, and code blocks.
- Long messages are split at Telegram’s 4096-character limit and approval buttons stay on the final chunk.
- Rendering currently uses plain text plus fenced blocks instead of full Telegram Markdown/HTML parse-mode formatting.

Consequence:

- The renderer is robust against escaping bugs and message splitting issues.
- Code/diff output stays readable and the adapter passed feature-gated tests quickly.

Remaining caveat:

- Rich Telegram-specific formatting is not fully implemented yet.
- If the bot starts carrying heavier user-facing traffic, the next upgrade should be a proper Telegram-safe formatting layer with escaping and richer inline emphasis.

## 16. Step 15 Slack gateway

### 16.1 Slack approval buttons are still normalized back into control messages

Current state:

- `moa-gateway/src/slack.rs` receives Block Kit button actions over Socket Mode.
- The core `PlatformAdapter` trait still only emits `InboundMessage`.
- The adapter converts approval button clicks into normalized commands such as `/approval deny <request_id>`.

Consequence:

- Step 15 works without widening the gateway trait surface.
- Approval actions from Slack and Telegram now share the same downstream parsing model.

Remaining caveat:

- Slack button actions are not yet first-class typed gateway events.
- If adapters need richer structured callbacks later, `InboundMessage.text` should stop carrying control commands.

### 16.2 Slack outbound routing still depends on an existing reply anchor

Current state:

- `OutboundMessage` still has no explicit Slack destination.
- `moa-gateway/src/slack.rs` resolves channel/thread targets from `reply_to`, using either:
  - a known inbound Slack message timestamp, or
  - a previously sent synthetic gateway message id.

Consequence:

- The intended session model works: one MOA session per Slack thread, with replies and edits anchored correctly.
- Status and event-log messages can be posted and updated in-thread.

Remaining caveat:

- The adapter cannot proactively open a brand-new channel/thread without a prior inbound anchor.
- If outbound-initiated Slack workflows are needed, the platform trait or outbound message model must gain an explicit destination.

### 16.3 Slack rendering is intentionally minimal Block Kit right now

Current state:

- `moa-gateway/src/renderer.rs` now splits Slack output at the 40K text cap and renders approval buttons as Block Kit actions.
- Normal text/code/diff output stays text-first, with Block Kit only added when interactive buttons are needed.
- The adapter uses `chat.update` directly and advertises a 1-second edit interval, but it does not yet coalesce bursts of intermediate status updates into a smarter buffer.

Consequence:

- The renderer is simple, testable, and low-risk for the initial Slack adapter milestone.
- Approval flows work with primary/danger button styling and thread-safe message updates.

Remaining caveat:

- Slack-specific rich layouts are still conservative.
- If Slack becomes a primary surface, the next upgrade should add richer per-event thread rendering and more deliberate edit throttling/coalescing.

## 17. Cross-platform approvals and Discord adapter

### 17.1 The unified approval layer still targets inline-button platforms first

Current state:

- `moa-gateway/src/approval.rs` is now the single source of truth for approval callback encoding and default approval buttons.
- Telegram, Slack, and Discord all consume that same callback format and button set.
- The fallback path degrades to text commands when inline buttons are unavailable.

Consequence:

- Approval rendering is now consistent across all current messaging adapters.
- Platform-specific callback parsing no longer drifts between Telegram, Slack, and Discord.

Remaining caveat:

- The generic gateway surface still has no first-class modal representation.
- `PlatformCapabilities.supports_modals` is informative today, but the unified approval flow still chooses inline buttons when available and text fallback otherwise.

### 17.2 Discord thread mapping is anchored to an inbound message

Current state:

- `moa-gateway/src/discord.rs` auto-creates a thread the first time the adapter responds to a guild message that is not already in a thread.
- Direct messages stay in the DM channel.
- Existing Discord threads are reused when the inbound message already arrived inside a thread.

Consequence:

- The documented “one MOA session per Discord thread” model now works for the normal inbound-driven flow.
- Post-decision edits and follow-up tool/status updates land in the same thread.

Remaining caveat:

- Like Telegram and Slack, the Discord adapter still relies on `reply_to` as its routing anchor.
- It cannot proactively open a new conversation without an inbound message or prior synthetic gateway message id.

## 18. Temporal orchestrator

### 18.1 Temporal approval resume had a real wait-condition bug

Current state:

- `moa-orchestrator/src/temporal.rs` now gates the workflow loop differently while paused for approval.
- When `waiting_for_approval` is true, the workflow waits only for an approval decision or cancel request.
- The manual Temporal dev-server integration test in `moa-orchestrator/tests/temporal_orchestrator.rs` now covers the full approval path end to end.

Consequence:

- `ApprovalRequested` no longer deadlocks the workflow.
- An `ApprovalDecided` signal now wakes the workflow, appends the decision event, resumes the turn, executes the approved tool, and reaches completion.

Remaining caveat:

- This was a real correctness bug in the initial Step 17 implementation, not just a missing test.
- The Temporal path should keep at least one ignored live dev-server integration test because replay-only or unit tests would not have caught this specific bug.

### 18.2 Temporal child workflows are still not true Temporal child workflows

Current state:

- `TemporalOrchestrator::spawn_child_workflow()` currently delegates to `start_session()`.
- That means it creates another top-level session workflow with normal session metadata and workflow id allocation.

Consequence:

- Sub-brain work can still be started as an independent Temporal-backed session.
- The public API surface is usable for now without blocking later cloud work.

Remaining caveat:

- This is not yet using Temporal's actual child-workflow semantics from the spec.
- Parent/child cancellation propagation, parent-close behavior, and Temporal-native child observability are not implemented yet.

### 18.3 Worker lifetime is process-scoped and not gracefully stoppable yet

Current state:

- `TemporalRuntime::connect()` starts a dedicated OS thread that owns a current-thread Tokio runtime and runs the Temporal worker.
- The `JoinHandle` is retained only to keep the thread alive for the life of the orchestrator.

Consequence:

- The worker can poll workflows and activities correctly without violating the SDK's non-`Send` constraints.
- The in-process cloud-mode prototype is workable for tests and local development.

Remaining caveat:

- Dropping `TemporalOrchestrator` does not perform a graceful worker shutdown; it effectively detaches the worker thread until process exit.
- A production-ready Temporal deployment likely needs an explicit worker lifecycle manager instead of the current fire-and-forget thread model.

## 19. Cloud hands and MCP

### 19.1 E2B command execution is coupled to the current sandbox RPC surface

Current state:

- `moa-hands/src/e2b.rs` provisions, pauses, resumes, destroys, and reconnects sandboxes using the documented E2B REST lifecycle endpoints.
- Command execution is then routed through the current sandbox RPC/process surface exposed by the sandbox domain, matching the behavior of the published E2B JS SDK as closely as practical.
- Live validation showed that command execution is not a plain JSON POST. It is a Connect JSON stream:
  - `Content-Type: application/connect+json`
  - `Connect-Protocol-Version: 1`
  - request body encoded as a Connect envelope
  - response body decoded as Connect envelopes with `start`, `data`, `end`, and end-stream frames
- Live validation also showed that `POST /sandboxes/{id}/pause` requires a JSON body, even when it is just `{}`.

Consequence:

- The current Step 18 E2B hand provider works for mocked tests and real E2B Cloud runs.
- MOA can treat E2B as a real `HandProvider` without inventing a second execution abstraction just for microVMs.

Remaining caveat:

- The E2B command execution path is less stable than the local or Daytona hands because it depends on an upstream transport shape that is not as cleanly documented as the lifecycle REST API.
- If E2B changes the sandbox RPC surface, `moa-hands/src/e2b.rs` is the first place likely to need adjustment.

### 19.2 MCP remote auth currently applies only to HTTP/SSE transports

Current state:

- `moa-security/src/mcp_proxy.rs` issues session-scoped opaque tokens and injects real credentials only when a remote MCP call is dispatched.
- `moa-hands/src/router.rs` uses that proxy when executing MCP tools discovered from configured servers.
- `moa-hands/src/mcp.rs` supports stdio and remote JSON-RPC transports, with SSE response parsing for remote endpoints that reply as event streams.

Consequence:

- Remote MCP servers can receive credentials without exposing them to the brain or to serialized tool arguments.
- Session-scoped auth is enforced at the router/proxy seam instead of being embedded in prompt-visible tool definitions.

Remaining caveat:

- Stdio MCP auth is still process/env based; the proxy does not inject credentials into subprocess transports.
- The current credential flow is therefore strongest for HTTP/SSE MCP servers and weaker for stdio servers that need secrets at startup.

### 19.3 MCP tool results are still flattened into shell-shaped output

Current state:

- `moa-hands/src/mcp.rs` flattens MCP `content` arrays into the existing `ToolOutput` shape.
- Text-like results are concatenated into `stdout`; MCP `isError` is mapped onto `stderr` and a non-zero exit code.

Consequence:

- Existing brain, approval, and UI code can consume MCP tool results without a larger cross-crate type migration.
- Step 18 landed with low blast radius outside `moa-hands` and `moa-security`.

Remaining caveat:

- Structured MCP results such as rich JSON objects, binary assets, or multiple typed content blocks lose fidelity when reduced to `ToolOutput`.
- A future first-class structured tool result type would make MCP integrations cleaner across the brain, orchestrator, and UI layers.

### 19.4 Daytona live API behavior diverges from the obvious reading of the docs

Current state:

- `moa-hands/src/daytona.rs` now provisions sandboxes successfully against the real Daytona Cloud API.
- The provider uses the live proxy routes that actually answered during end-to-end validation:
  - `.../toolbox/{sandboxId}/process/execute`
  - `.../toolbox/{sandboxId}/files/download`
  - `.../toolbox/{sandboxId}/files/upload`
  - `.../toolbox/{sandboxId}/files/search`
- File upload uses multipart form data, matching the live toolbox behavior.
- Destroy now treats `409 state change in progress` as a retryable transition instead of a hard failure.

Consequence:

- The Daytona provider now passes both mocked tests and real ignored live integration tests.
- Router-level lazy provisioning, hand reuse, pause/resume, and session isolation all work against the real Daytona service.

Remaining caveat:

- The live API rejected explicit sandbox resource fields on the current create path with `Cannot specify Sandbox resources when using a snapshot`, so `moa-hands/src/daytona.rs` currently relies on Daytona's default sandbox class instead of honoring requested CPU/memory overrides.
- The published docs mix `/toolbox/{sandboxId}/toolbox/...` reference paths with curl examples that omit the second `/toolbox`; the real proxy accepted the shorter form during validation.

### 20.1 Local encrypted vault currently uses a generated local passphrase file

Current state:

- `moa-security/src/vault.rs` stores credentials in `vault.enc` as an age-encrypted JSON document.
- The local passphrase is generated automatically on first use and stored beside the vault as `vault.key` with `0600` permissions on Unix.
- This keeps credentials encrypted at rest and gives MOA a concrete local `CredentialVault` implementation without requiring user setup before the first run.

Consequence:

- Step 19 now has a working encrypted local vault with async `get/set/delete/list` operations and test coverage.
- Existing local components can move off env-only credential handling when the runtime is wired to use `FileVault`.

Remaining caveat:

- This is stronger than plaintext-at-rest, but weaker than OS keychain or external secret-manager storage because the decryption material still lives on the same machine.
- If we later want stricter local secret handling, the swap point is the `CredentialVault` trait, not the rest of the security pipeline.

### 20.2 Local Docker hardening currently disables container network access entirely

Current state:

- `moa-hands/src/local.rs` now starts Docker sandboxes with:
  - read-only root filesystem
  - tmpfs scratch mounts
  - `cap-drop=ALL`
  - `no-new-privileges:true`
  - `pids-limit=256`
  - Docker seccomp active (daemon default unless `MOA_DOCKER_SECCOMP_PROFILE` is set)
- The implementation uses `--network none` as the concrete way to block the cloud metadata endpoint from local Docker sandboxes.

Consequence:

- The new Docker hardening integration test can verify `Seccomp: 2`, `NoNewPrivs: 1`, a read-only root mount, and that `169.254.169.254` is unreachable.
- The local hand now has a real hardened container posture instead of a soft long-lived shell.

Remaining caveat:

- This is stricter than the original spec in one dimension: local Docker sandboxes are offline, not just metadata-blocked.
- If we later need outbound network for local containerized tools, we will need a narrower metadata-blocking mechanism than `--network none`.

### 20.3 Tool-result trust boundaries are explicit, but repeated malicious tool loops are still model-driven

Current state:

- `moa-brain/src/harness.rs` now injects a per-turn canary into tool-enabled requests.
- Tool invocations are blocked if they leak either the exact active canary or any `moa_canary_*` marker.
- Tool outputs are always wrapped in `<untrusted_tool_output>...</untrusted_tool_output>` plus an explicit instruction not to follow embedded instructions.
- Suspicious tool output produces `Warning` events instead of being silently fed back into history.

Consequence:

- The instruction hierarchy is now materially stronger: tool results re-enter Stage 6 as low-authority, explicitly untrusted content.
- Step 19 regression tests now cover both canary leakage and malicious tool-output containment.

Remaining caveat:

- If a model keeps emitting fresh malicious tool calls after seeing the resulting `ToolError`/`Warning`, the retry behavior is still governed by the surrounding turn loop rather than a dedicated security circuit breaker.
- If that becomes a real failure mode, the next seam to tighten is the orchestrator/harness retry policy, not the classifier itself.

### 20.4 OpenAI/OpenRouter now use the Responses API, but MOA tool schemas are still translated provider-side

Current state:

- `moa-providers/src/openai.rs` and `moa-providers/src/openrouter.rs` both call the OpenAI-compatible `/responses` API.
- MOA still stores tool schemas in the existing internal format used across the rest of the repo.
- `moa-providers/src/common.rs` translates those schemas into Responses function tools at request time.
- The translation currently sends `strict: false` because the current MOA schemas include optional properties that are not yet normalized into OpenAI's stricter function-schema shape.

Consequence:

- The default local runtime can now use `openai / gpt-5.4` successfully, including streaming and tool use.
- OpenRouter rides the same Responses-compatible translation layer instead of a separate request shape.

Remaining caveat:

- The provider layer is still compensating for schema mismatches that really belong in the shared tool-definition surface.
- If we later want fully strict OpenAI function schemas, the right fix is to normalize tool schemas once in the registry/core model, not to keep adding provider-specific exceptions.

### 20.5 OpenAI metadata forwarding is intentionally lossy

Current state:

- `moa-providers/src/common.rs` forwards request metadata to the Responses API only when each value fits within OpenAI's metadata size limits.
- Oversized internal metadata values such as serialized `tool_schemas` are now dropped before the request is sent.

Consequence:

- The live `moa exec` path now works with the default OpenAI provider instead of failing with `metadata.* string too long`.
- Provider requests still preserve small diagnostic metadata values when they are useful.

Remaining caveat:

- OpenAI/OpenRouter requests no longer carry the full MOA metadata bag verbatim.
- If any downstream debugging or analytics later depends on large metadata fields, those fields will need a different transport than provider metadata.

### 20.6 Capability coverage is precise for default models and best-effort elsewhere

Current state:

- `moa-providers/src/openai.rs` has explicit capabilities and pricing for the supported GPT-5 family used by MOA defaults.
- `moa-providers/src/openrouter.rs` reuses those mappings for OpenAI-family models and has explicit fallbacks for the currently supported Anthropic families routed through OpenRouter.

Consequence:

- `gpt-5.4` is now the repo default model and reports concrete capabilities.
- Known OpenRouter model families report useful capability data instead of a generic placeholder.

Remaining caveat:

- Coverage is still selective, not exhaustive across every OpenAI/OpenRouter model id.
- Adding new default models should include an explicit capability/pricing update rather than relying on the generic fallback.

### 21.1 The Step 21 TUI memory/settings surfaces are functional, but still intentionally shallow

Current state:

- `moa-tui/src/app.rs` now exposes the documented Step 21 shells for sidebar, memory browser, settings, help, command palette, slash completion, and `@file` completion.
- The memory browser in `moa-tui/src/views/memory.rs` supports page browsing, FTS-backed search, wikilink following, and back/forward history.
- The settings panel in `moa-tui/src/views/settings.rs` persists a focused subset of config values and hot-reloads the provider/model path through the existing runtime seam.

Consequence:

- The TUI now has a real feature-complete shell instead of only chat/session/diff overlays.
- Manual PTY smoke tests confirmed that the command palette, memory browser, and settings overlays all open in the live alternate-screen loop.

Remaining caveat:

- The memory browser does not yet implement destructive delete or external-editor open; those actions currently surface explicit status messages instead of performing the operation.
- Markdown rendering in the memory pane is still lightweight text rendering, not full `pulldown-cmark` rich formatting.
- The settings panel intentionally edits a small high-value subset of config rather than every field in `MoaConfig`.

### 21.2 Prompt completion is intentionally simple

Current state:

- Slash completion and `@file` completion now render above the prompt and accept via `Tab`.
- File completion is ranked by a small in-memory frecency map plus path order.
- Sandbox files are scanned from the configured local sandbox root and refreshed on periodic sidebar refreshes.

Consequence:

- The documented completion flows now exist and are visible in the real TUI.
- The implementation stayed local to `moa-tui` without introducing another completion engine or cursor-aware editor abstraction.

Remaining caveat:

- Completion is prompt-text based, not true cursor-position aware editing inside arbitrary multiline input.
- `@file` completion currently only rewrites the trailing token, so paths with embedded spaces are not handled yet.
- File-frecency is process-local and is not yet persisted across TUI restarts.

### 22.1 The local daemon currently owns one mutable runtime, not one runtime per client

Current state:

- `moa-cli/src/daemon.rs` runs a single `ChatRuntime` behind a Unix socket and exposes control-plane commands plus per-session observation streams.
- TUI clients can attach to persisted sessions through that daemon, and sessions continue running after the TUI exits.
- `status` and `sessions` intentionally read persisted state directly from the local session store instead of forcing everything through daemon RPC.

Consequence:

- The Step 22 daemon flow now works end to end: `moa daemon start`, `moa status`, `moa sessions`, `moa resume`, and `moa daemon stop`.
- Session persistence is durable even when no TUI is attached.

Remaining caveat:

- Workspace/model changes still mutate shared daemon runtime state, so they are global to the daemon process rather than scoped per connected client.
- If local multi-client control becomes important, the daemon should promote those settings into per-session state instead of one shared mutable runtime.
- The daemon currently starts with one default active session because `ChatRuntime` always owns a current session.
- If we want a truly idle daemon with zero sessions until first use, the runtime boundary should support lazy session creation instead of eager bootstrap.

### 22.2 Default terminal logging is intentionally quiet so it does not corrupt the TUI

Current state:

- `moa-core/src/telemetry.rs` now initializes the human-readable tracing layer at `WARN` by default while still wiring OTLP export when configured.
- This suppresses noisy info-level runtime logs from libraries such as the cron scheduler when launching the alternate-screen TUI.

Consequence:

- Interactive `moa`, `moa resume`, and `moa attach` no longer print routine runtime logs into the terminal before the TUI renders.
- The TUI and CLI subcommands remain usable without log noise while observability stays available through OTLP.

Remaining caveat:

- Rich local debug logging now requires an explicit code or config change instead of appearing by default on stderr.
- If we want operator-friendly local debug mode later, it should be an explicit CLI/config switch rather than the default interactive behavior.
