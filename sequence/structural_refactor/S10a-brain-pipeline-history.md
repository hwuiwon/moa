# S10a — Split `moa-brain/src/pipeline/history.rs` (2,647 LOC)

## Scope

Break the largest single file in `moa-brain` into a `pipeline/history/` folder organized by concern. **No behavior changes, no algorithmic changes, no rename of public types.**

## Preconditions

- S01–S09 complete and merged.
- `cargo check --workspace` is green.
- This is the first of five `moa-brain` sub-prompts. It must complete before S10b.

## Why this prompt

`pipeline/history.rs` is 2,647 LOC and is **the single largest file in the workspace** by LOC. It bundles: history retrieval from the session log, compaction, pruning/eviction policy, message-level serialization for the LLM context, checkpoint summarization, and the inline test module. Splitting by concern is the same recipe as previous prompts, but the file is large enough that it's worth its own session.

## Files in scope

- `crates/moa-brain/src/pipeline/history.rs` → deleted
- `crates/moa-brain/src/pipeline/history/` → new folder
- `crates/moa-brain/src/pipeline/mod.rs` → keep `pub mod history;` line; nothing else changes

## Files explicitly out of scope

- Every other `pipeline/*.rs` file. They get their own prompts.
- Every file in `harness/` — also separate prompts.
- The `ContextProcessor` trait (in `moa-core`)
- `moa-brain/tests/` — TEST pack handles

## Step-by-step instructions

1. **Read `pipeline/history.rs` end to end.** Identify natural sections:
   - Top-level processor struct (likely `HistoryProcessor`) + `impl ContextProcessor for HistoryProcessor`
   - Event-to-message conversion (turning `Event::UserMessage`, `Event::BrainResponse`, `Event::ToolCall`, `Event::ToolResult`, etc. into `ContextMessage`)
   - Compaction policy (when to summarize, what to drop)
   - Checkpoint integration (loading a previous checkpoint, applying it as the starting point of the compiled history)
   - Error preservation (errors are always kept verbatim — explicit special-case)
   - Token budgeting (how much budget this stage gets, how it's distributed)
   - Inline `#[cfg(test)] mod tests` block

2. **Target structure**:
   ```
   crates/moa-brain/src/pipeline/history/
   ├── mod.rs              — HistoryProcessor struct + impl ContextProcessor; thin glue
   ├── conversion.rs       — Event → ContextMessage (the largest sub-section)
   ├── compaction.rs       — when/how to compact, summary triggers
   ├── checkpoint.rs       — checkpoint loading + application
   ├── budgeting.rs        — token budget allocation logic
   ├── errors.rs           — error-preservation rules (small file, but distinct concern)
   └── prune.rs            — eviction / oldest-first dropping when budget exceeded
   ```
   Adjust if actual content is shaped differently.

3. **Move types verbatim.** For each section:
   - Cut from old file
   - Paste into new sub-file
   - Use `pub(super)` for items that are needed only by sibling modules (most internals)
   - Keep `pub` for items that were previously visible at `moa_brain::pipeline::history::*`
   - Mark methods on `HistoryProcessor` that distribute across files using the impl-block delegation pattern from S07

4. **`mod.rs` content** approximates:
   ```rust
   //! History compilation stage of the context pipeline.
   //!
   //! Stage 6 of 7 — turns the session event log into a sequence of
   //! ContextMessages, applying compaction, checkpoint loading, and
   //! error preservation.
   
   mod conversion;
   mod compaction;
   mod checkpoint;
   mod budgeting;
   mod errors;
   mod prune;
   
   use moa_core::traits::ContextProcessor;
   use moa_core::context::WorkingContext;
   // ...
   
   pub struct HistoryProcessor {
       // fields
   }
   
   impl HistoryProcessor {
       pub fn new(/* ... */) -> Self { /* ... */ }
   }
   
   impl ContextProcessor for HistoryProcessor {
       fn name(&self) -> &str { "history" }
       fn stage(&self) -> u8 { 6 }
       
       fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
           let events = self.load_events(ctx)?;
           let checkpoint = checkpoint::find_last_checkpoint(&events);
           let budget = budgeting::compute_budget(ctx);
           let messages = conversion::events_to_messages(&events, &budget)?;
           let pruned = prune::apply_budget(messages, budget)?;
           let with_errors = errors::preserve_errors(&events, pruned)?;
           ctx.extend_messages(with_errors);
           Ok(ProcessorOutput::default())
       }
   }
   ```
   The actual flow may differ; this is illustrative of the *delegating* style — `process` becomes a thin sequence of named submodule calls.

5. **Inline tests**: each test in the original `mod tests` block likely tests one concern. Move each test to a `mod tests` block in the relevant sub-file (test of compaction → `compaction.rs`'s `mod tests`; test of error preservation → `errors.rs`'s `mod tests`). Tests that span multiple sub-modules stay in `mod.rs`'s `mod tests`.

6. **Run verification.**

7. **Document anything unexpected** in `REFACTOR_NOTES.md` under `[S10a]`.

## Verification

```bash
cargo check -p moa-brain --all-targets
cargo clippy -p moa-brain --all-targets -- -D warnings
cargo test -p moa-brain --no-run
cargo test -p moa-brain --lib history  # run unit tests inline if any are quick
cargo check --workspace --all-targets
```

## Acceptance criteria

- [ ] `crates/moa-brain/src/pipeline/history.rs` no longer exists.
- [ ] `crates/moa-brain/src/pipeline/history/mod.rs` exists.
- [ ] No file in `pipeline/history/` exceeds 700 LOC.
- [ ] Public surface (`HistoryProcessor` and any other previously-pub items) is unchanged.
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No downstream crate's source had to change.

## Rollback plan

`git checkout -- crates/moa-brain/src/pipeline/history.rs crates/moa-brain/src/pipeline/history/` and `git clean -fd crates/moa-brain/src/pipeline/history`.

## Notes for the agent

- **The `HistoryProcessor` `process` function is the single most important method in this file.** Read it carefully; it dictates the order of sub-module calls. The split should preserve that order exactly.
- **Token budgeting interacts with multiple concerns.** Budget is computed once, then consumed by conversion, compaction, and prune. The pattern is "compute up front, pass down" — keep it.
- **Don't modify the compaction *policy*.** When to compact, what threshold, how the summary is generated — these are tuned numbers. Move; don't tune.
- **Checkpoint loading may involve deserialization** that's tightly coupled to `moa-session` event types. Keep that coupling in `checkpoint.rs`; don't try to abstract.
- **Inline test moves**: if a test calls a `pub(super)` item that's now in a different sibling module, the test has to follow the item. That's the rule.
- **Time budget**: 1.5 sessions. The file is large; don't rush.
- **Anti-pattern**: do not introduce a `HistoryStrategy` trait or similar to "make it pluggable." That's a future design discussion. Move only.
