# Step 63 — Split `harness.rs` + Move Async Stream Types Out of `moa-core`

_Break the 66KB brain harness into focused modules. Relocate `CompletionStream` and `EventStream` from `moa-core` to the crates that own them._

---

## 1. What this step is about

Two related cleanups:

**Part A:** `moa-brain/src/harness.rs` at 66KB is the core execution engine jammed into one file. It handles streaming, tool dispatch, approval flow, and turn lifecycle all in one place.

**Part B:** `moa-core/src/types/completion.rs` (after Step 61) contains `CompletionStream` which wraps `tokio::task::JoinHandle`, `mpsc::Receiver`, and `CancellationToken`. `moa-core/src/types/events_stream.rs` contains `EventStream` which wraps `broadcast::Receiver`. A "core types" crate should be plain data — these async runtime wrappers belong in the crates that create and consume them.

---

## 2. Files to read

For Part A:
- **`moa-brain/src/harness.rs`** — The 66KB file to split.
- **`moa-brain/src/lib.rs`** — Current re-exports.
- **`moa-brain/src/turn.rs`** — Related turn engine code. Understand the boundary between harness and turn.

For Part B:
- **`moa-core/src/types/completion.rs`** (from Step 61) — Contains `CompletionStream`.
- **`moa-core/src/types/events_stream.rs`** (from Step 61) — Contains `EventStream`.
- **`moa-providers/src/anthropic.rs`** — Creates `CompletionStream` instances.
- **`moa-providers/src/gemini.rs`** — Creates `CompletionStream` instances.
- **`moa-providers/src/common.rs`** — May create or consume `CompletionStream`.
- **`moa-core/src/traits.rs`** — `LLMProvider` trait returns `CompletionStream`. This is the key dependency.

---

## 3. Goal

After this step:

**Part A:**
1. `moa-brain/src/harness.rs` is replaced by `moa-brain/src/harness/` module directory
2. Each module file is 200-500 lines
3. The public API (`run_brain_turn`, `run_streamed_turn`, etc.) is unchanged

**Part B:**
1. `CompletionStream` moves from `moa-core` to `moa-providers`
2. `EventStream` moves from `moa-core` to `moa-session` (or stays in `moa-core` with a `#[cfg(feature)]` gate — see decision below)
3. `moa-core` no longer depends on `tokio::sync::broadcast` or `tokio::task::JoinHandle` for these types

---

## 4. Rules

- **Public API changes are allowed for Part B** but must be mechanical (move, not redesign).
- **`CompletionStream` move:** The `LLMProvider` trait in `moa-core/src/traits.rs` returns `CompletionStream`. This creates a circular dependency problem: `moa-core` defines the trait, `moa-providers` implements it, but if `CompletionStream` lives in `moa-providers`, `moa-core` can't reference it. **Solution:** Keep `CompletionStream` in `moa-core` but behind a feature flag `stream` that gates the tokio dependencies, OR accept the tokio dependency in core (it's already there for other reasons). **Evaluate which approach is simpler during implementation.** If tokio is already an unconditional dependency of moa-core, then the move is not worth the complexity — just leave `CompletionStream` in `moa-core` and document why. The key judgment call: **don't create a feature flag just to move one type if tokio is already pulled in unconditionally.**
- **For Part A, no changes to method signatures.** Move code, don't rewrite it.

---

## 5. Tasks

### Part A: Split `harness.rs`

#### 5a. Create `moa-brain/src/harness/` directory

Analyze the 66KB file and identify logical boundaries. Expected split:

```
moa-brain/src/harness/
├── mod.rs              # Public API: run_brain_turn, run_streamed_turn, etc. + re-exports
├── streaming.rs        # SSE/stream consumption, delta accumulation, token counting
├── tool_dispatch.rs    # Tool call loop: parse tool calls, dispatch to ToolRouter, collect results
├── approval_flow.rs    # Approval request construction, decision handling, rule storage
└── context_build.rs    # Context compilation orchestration (calls the pipeline stages)
```

The exact split depends on the logical boundaries in the file. Read the file carefully and split at natural function-group boundaries. The principle: each file handles one concern, functions in the same file call each other frequently, functions in different files interact through well-defined parameters.

#### 5b. Update `moa-brain/src/lib.rs`

Update the module declaration and re-exports:
```rust
pub mod harness;
// re-exports stay the same
pub use harness::{StreamedTurnResult, TurnResult, run_brain_turn, ...};
```

### Part B: Evaluate and potentially move async types

#### 5c. Check moa-core's tokio dependency

```bash
grep "tokio" moa-core/Cargo.toml
```

If tokio is already an unconditional dependency of moa-core (which is likely since `CompletionStream` and `EventStream` are there), then **moving them out creates more complexity than it solves**. In that case:

- Add a doc comment to `CompletionStream` and `EventStream` explaining why they live in core despite wrapping async primitives: `/// NOTE: This type wraps async runtime primitives (JoinHandle, mpsc) and ideally belongs in moa-providers. It lives in moa-core because the LLMProvider trait (also in core) must reference it.`
- **Skip the move.** Document the architectural debt and move on.

If tokio is only pulled in because of these two types (unlikely), then move them and adjust the trait to use a boxed future or generic return type.

---

## 6. Deliverables

### Part A
- [ ] `moa-brain/src/harness.rs` — **DELETED** (replaced by directory)
- [ ] `moa-brain/src/harness/mod.rs` — Public API + re-exports
- [ ] `moa-brain/src/harness/streaming.rs` — Stream handling
- [ ] `moa-brain/src/harness/tool_dispatch.rs` — Tool execution loop
- [ ] `moa-brain/src/harness/approval_flow.rs` — Approval logic
- [ ] `moa-brain/src/harness/context_build.rs` — Context compilation
- [ ] `moa-brain/src/lib.rs` — Updated module declaration

### Part B
- [ ] Doc comments added to `CompletionStream` and `EventStream` explaining placement (if not moved)
- [ ] OR: types moved to destination crates with trait adjustments (if tokio is not already a core dep)

---

## 7. Acceptance criteria

1. `cargo build --workspace` compiles with zero errors.
2. `cargo test --workspace` passes.
3. `moa-brain/src/harness.rs` single file does not exist.
4. No file in `moa-brain/src/harness/` exceeds 500 lines.
5. `moa-brain` public API unchanged — all re-exports in `lib.rs` still resolve.
6. `CompletionStream` and `EventStream` have doc comments explaining their placement.
