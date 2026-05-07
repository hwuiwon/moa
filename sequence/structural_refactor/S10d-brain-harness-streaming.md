# S10d — Split `harness/streaming.rs` (1,211 LOC) and other harness/* over 600 LOC

## Scope

Split harness streaming and other harness files that exceed 600 LOC. **No behavior changes.**

## Preconditions

- S10a + S10b + S10c complete and merged.
- `cargo check --workspace` is green.

## Why this prompt

`harness/streaming.rs` is the streaming-completion driver: it consumes the LLM provider's `CompletionStream`, dispatches text deltas + tool-call deltas, handles cancellation, and emits stream events to observers. Companion files: `harness/approval_flow.rs` (~740 LOC), `harness/tool_dispatch.rs` (~670 LOC), and the top-level `turn.rs` (~714 LOC). All four sit at the harness boundary and exhibit the same "single async function with too many concerns" smell.

## Files in scope

- `crates/moa-brain/src/harness/streaming.rs` → split to `harness/streaming/`
- `crates/moa-brain/src/harness/approval_flow.rs` → split to `harness/approval_flow/`
- `crates/moa-brain/src/harness/tool_dispatch.rs` → split to `harness/tool_dispatch/` (only if it benefits; may be left)
- `crates/moa-brain/src/turn.rs` → split to `turn/` if it's tangled

## Files explicitly out of scope

- `harness/mod.rs` and other small harness files
- The brain harness loop itself (in `lib.rs` or `harness/mod.rs`) — its top-level orchestration stays
- Pipeline files (handled in S10a/b/c)

## Step-by-step instructions

### Part A — `harness/streaming.rs`

1. Read end-to-end. Identify sections:
   - Top-level `run_streaming_turn` (or similar) async function
   - Stream-event consumer loop
   - Text-delta accumulation (incremental message building)
   - Tool-call-delta accumulation
   - Cancellation handling (soft cancel mid-stream, hard cancel)
   - Observer dispatch (sending to event subscribers)
   - Token counting / cost tracking

2. Target structure:
   ```
   harness/streaming/
   ├── mod.rs              — top-level driver fn (run_streaming_turn)
   ├── consumer.rs         — the stream-consumption loop
   ├── text_accumulator.rs — text-delta merging into ContextMessage
   ├── tool_accumulator.rs — tool-call-delta merging into ToolCall
   ├── cancellation.rs     — soft/hard cancel handling
   └── observer.rs         — event dispatch to subscribers
   ```

### Part B — `harness/approval_flow.rs`

3. Read end-to-end. Identify sections:
   - Approval request emission
   - Waiting for approval signal
   - Decision processing (AllowOnce / AlwaysAllow / Deny)
   - "Always allow" rule storage (interacts with `moa-security`)
   - Post-decision event emission

4. Target structure:
   ```
   harness/approval_flow/
   ├── mod.rs           — top-level approval flow
   ├── request.rs       — approval request emission
   ├── decision.rs      — decision processing + dispatch
   ├── rule_storage.rs  — "Always Allow" rule write to moa-security
   └── post_decision.rs — event emission after decision
   ```

### Part C — `harness/tool_dispatch.rs`

5. ~670 LOC is borderline. **Decide based on content**:
   - If it's a single coherent dispatcher with helpers: leave as-is, do not split.
   - If it bundles dispatch logic + result post-processing + error handling: split.

   Default: leave alone unless the read reveals a clear seam.

### Part D — `turn.rs`

6. ~714 LOC. The top-level "one turn of the brain loop" function. Likely sections:
   - Turn entry / context loading
   - Pipeline invocation
   - LLM call dispatch (streaming vs non-streaming)
   - Tool-call dispatch (delegates to `harness/tool_dispatch`)
   - Approval flow dispatch (delegates to `harness/approval_flow`)
   - Turn-result classification (Continue / Complete / NeedsApproval / Error)

7. If `turn.rs` is mostly *delegating*, keep it as a single file but make sure the delegations are clean. If it has substantial inline logic for any of the above, split into:
   ```
   turn/
   ├── mod.rs        — run_turn function
   ├── entry.rs      — turn entry / context loading
   ├── llm_call.rs   — LLM provider invocation (streaming vs not)
   ├── tools.rs      — tool-call dispatch
   ├── result.rs     — TurnResult classification
   └── error.rs      — error handling
   ```

   Default: split only if `turn.rs` is genuinely tangled. Pure orchestrators are *better* as single files.

### All parts

8. Run verification.

9. Document any structural surprises in `REFACTOR_NOTES.md` under `[S10d]`.

## Verification

```bash
cargo check -p moa-brain --all-targets
cargo clippy -p moa-brain --all-targets -- -D warnings
cargo test -p moa-brain --no-run
cargo check --workspace --all-targets
```

## Acceptance criteria

- [ ] `harness/streaming.rs` no longer exists; replaced by folder.
- [ ] `harness/approval_flow.rs` no longer exists; replaced by folder.
- [ ] `tool_dispatch.rs` either split or explicitly left as-is (documented decision).
- [ ] `turn.rs` either split or explicitly left as-is.
- [ ] No file in `harness/` exceeds 700 LOC after the prompt.
- [ ] `cargo check --workspace --all-targets` passes.

## Rollback plan

`git checkout -- crates/moa-brain/src/harness/ crates/moa-brain/src/turn{.rs,/}`.

## Notes for the agent

- **Streaming has subtle async ordering.** The text accumulator and tool accumulator must agree on when a "tool call started" event triggers a flush of pending text. Don't restructure the merge logic; only relocate.
- **Cancellation interacts with both stream consumption and tool dispatch.** If a hard cancel arrives mid-tool-call, the tool may need to be aborted; if mid-stream, the LLM connection must be closed cleanly. Keep this logic intact.
- **Approval flow may block the harness.** When `NeedsApproval` is emitted, the harness suspends until the user signals back. This is by design (Temporal-style indefinite wait). The split must preserve this — don't add timeouts.
- **`turn.rs` orchestration order matters.** Pipeline runs first, then LLM, then tool-or-approval-or-end. Don't reorder.
- **The boundary between `harness/` and `pipeline/`**: pipeline produces a compiled context; harness uses that context to call the LLM. Don't blur the line.
- **Observer dispatch is async fan-out.** Each observer receives the same event. Make sure broadcast semantics are preserved (no observer should affect another).
- **Time budget**: 1.5–2 sessions. Streaming + approval flow are non-trivial.
- **Anti-pattern**: do not introduce a `StreamHandler` trait abstracting over text/tool deltas. The current concrete handling is fine.
