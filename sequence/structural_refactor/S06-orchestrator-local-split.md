# S06 — Split `moa-orchestrator-local/src/lib.rs` into modules

## Scope

Cut the 2,202-LOC `crates/moa-orchestrator-local/src/lib.rs` along its existing functional seams into separate module files. **No behavior changes, no signature changes.** This is the easiest mechanical split in the workspace and should be done early as a low-risk warmup.

## Preconditions

- S01–S05 complete and merged.
- `cargo check --workspace` is green.
- `cargo test -p moa-orchestrator-local --no-run` compiles (existing tests don't have to pass live; they just have to compile).

## Why this prompt

`lib.rs` is currently a single 2,202-LOC file containing the entire `LocalOrchestrator` implementation. There's no module structure at all. This is a pure mechanical refactor — no architectural decisions, no DDD vocabulary, just "cut along visible seams." Done early to:
1. Build refactor confidence with a low-risk crate
2. Unblock the eventual TEST pack split of `tests/local_orchestrator.rs` (which is even bigger)
3. Make the file editable in any IDE (2.2k LOC strains rust-analyzer)

## Files in scope

- `crates/moa-orchestrator-local/src/lib.rs` — shrinks to a thin module-declarations + re-exports file
- `crates/moa-orchestrator-local/src/<new-modules>.rs` — new files

## Files explicitly out of scope

- `crates/moa-orchestrator-local/tests/` — TEST pack handles
- Any other crate
- The `LocalOrchestrator` API surface — `pub fn new`, `pub fn start_session`, etc. must remain identical

## Step-by-step instructions

1. **Read `lib.rs` end to end.** Identify the natural sections. Likely:
   - Type definitions (`LocalOrchestrator`, `LocalBrainHandle`, `LocalOrchestratorBuilder`, error types)
   - `BrainOrchestrator` trait impl
   - Brain-loop spawning logic (`spawn_brain_loop`, `brain_loop` fn)
   - Signal routing (queue message, soft cancel, hard cancel, approval)
   - Session lifecycle (start, resume, observe, list)
   - Cron / scheduler integration (consolidation, skill improvement)
   - Helpers (status updates, channel construction, error mapping)
   - Inline `#[cfg(test)] mod tests` block (if any)

2. **Pick a structure.** Two reasonable shapes:

   **Option A (flat)**:
   ```
   crates/moa-orchestrator-local/src/
   ├── lib.rs              — pub mod + re-exports + thin glue
   ├── orchestrator.rs     — LocalOrchestrator struct + impl BrainOrchestrator
   ├── brain_handle.rs     — LocalBrainHandle struct + Drop impl
   ├── brain_loop.rs       — the brain_loop async fn and its turn dispatcher
   ├── signals.rs          — signal channel construction, signal routing
   ├── lifecycle.rs        — start_session, resume_session, list_sessions
   ├── observation.rs      — observe(), event-stream wiring
   ├── scheduler.rs        — cron jobs (consolidation, skill improvement)
   ├── builder.rs          — LocalOrchestratorBuilder
   ├── error.rs            — error types if local-specific
   └── helpers.rs          — small free functions (last resort; don't dump-bucket)
   ```

   **Option B (folder-per-concern)**:
   ```
   src/
   ├── lib.rs
   ├── orchestrator.rs
   ├── brain/{mod.rs, handle.rs, loop.rs, signals.rs}
   ├── lifecycle/{mod.rs, start.rs, resume.rs, list.rs, observe.rs}
   └── scheduler/{mod.rs, consolidation.rs, skills.rs}
   ```

   **Recommendation**: Option A. It's flatter, less ceremonious, fits a 2.2k-LOC crate. Option B is justified if a section becomes 800+ LOC after the split. Start with A; promote to B only if needed.

3. **Move types and impls verbatim.** For each section:
   - Cut the section out of `lib.rs`
   - Paste into the appropriate new file
   - Add minimal `use` declarations to satisfy the compiler
   - Keep `pub`/`pub(crate)` visibilities exactly as-is

4. **`lib.rs` after the split** should look approximately like:
   ```rust
   //! Local orchestrator — runs brains as in-process tokio tasks.
   //! 
   //! Used by the TUI and by the cloud orchestrator's local-fallback mode.
   
   mod orchestrator;
   mod brain_handle;
   mod brain_loop;
   mod signals;
   mod lifecycle;
   mod observation;
   mod scheduler;
   mod builder;
   mod error;
   mod helpers;
   
   pub use orchestrator::LocalOrchestrator;
   pub use builder::LocalOrchestratorBuilder;
   pub use error::{LocalOrchestratorError, /* etc. */};
   // ... whatever was previously pub at lib.rs level
   ```
   The exact `pub use` list must be the union of every `pub` symbol that was previously visible at the `moa_orchestrator_local::*` path.

5. **Inline `mod tests` blocks**: if `lib.rs` had `#[cfg(test)] mod tests { ... }`, decide where each test belongs:
   - Test of an internal helper that uses `pub(crate)` items → move to a `mod tests` block in the file containing that item
   - Test of public API → leave as a `mod tests` block at the bottom of `lib.rs` for now; TEST pack will move it to `tests/`

6. **Watch for cross-module visibility.** The most common compile error after a split: `LocalBrainHandle` was a `pub(crate)` field of `LocalOrchestrator`, and after splitting, the field can no longer access the type unless `LocalBrainHandle` is `pub(crate)` from its new module too. Resolution: keep `pub(crate)` everywhere it was before; tighten visibility in S14 if at all.

7. **Run verification.**

8. **No new types, no new traits, no method renames.** If a method *should* be renamed (because the current name is misleading), document in `REFACTOR_NOTES.md` under `[S06]` and do not rename.

## Verification

```bash
cargo check -p moa-orchestrator-local --all-targets
cargo clippy -p moa-orchestrator-local --all-targets -- -D warnings
cargo test -p moa-orchestrator-local --no-run
cargo check --workspace --all-targets   # downstream still compiles
```

## Acceptance criteria

- [ ] `crates/moa-orchestrator-local/src/lib.rs` is under 200 LOC (it's now just module declarations + re-exports).
- [ ] No file in `crates/moa-orchestrator-local/src/` exceeds 700 LOC. (Slightly higher is fine if a single coherent function genuinely is that long; smaller is better.)
- [ ] Every `pub` symbol that was previously visible at `moa_orchestrator_local::*` is still visible.
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.
- [ ] No downstream crate's source had to change.

## Rollback plan

`git checkout -- crates/moa-orchestrator-local/src/` and `git clean -fd crates/moa-orchestrator-local/src/`. The change is contained to one crate.

## Notes for the agent

- **This is the simplest prompt in the pack.** If you find yourself making architectural decisions, you've drifted from scope. Cut, paste, fix imports, run tests.
- **`use crate::brain_handle::LocalBrainHandle;`** style imports are fine. Don't pre-create a `prelude` module — that's a future optimization.
- **Inline test modules**: a `#[cfg(test)] mod tests { ... }` at the bottom of a source file is idiomatic Rust. Don't move them out into `tests/` in this prompt — TEST pack handles it.
- **Visibility hygiene**: if a `pub` item was *only* re-exported by `lib.rs` and never used externally, it stays `pub` for now. Visibility tightening is S14 territory.
- **If there's a sub-module already** (e.g. `crates/moa-orchestrator-local/src/some_existing_mod.rs`), don't disturb it. Work around it.
- **Imports get noisy after a split.** Rust-analyzer's "Add missing import" code action handles most of it. Run `cargo fmt --all` at the end to normalize.
- **Time budget**: ~1 session, possibly 1.5 if the file's structure is more tangled than expected.
- **Anti-pattern**: don't introduce new traits "for testability" in this prompt. Mocking / testability changes belong in the TEST pack.
