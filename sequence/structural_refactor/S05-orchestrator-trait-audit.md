# S05 — Audit the duplicate `SessionStore` trait suspicion in `moa-orchestrator`

## Scope

**Read-only investigation prompt.** Determine whether `moa-orchestrator/src/services/session_store.rs` (~1,067 LOC) defines a `pub trait SessionStore` that is a duplicate of `moa_core::traits::SessionStore`, an internal facade with intentionally different semantics, or something else. Produce a decision document. Do not modify any code.

## Preconditions

- S01–S04 complete and merged.
- `cargo check --workspace` is green.

## Why this prompt

The audit phase flagged this file as containing a `pub trait` whose name shadows the core trait. Three possibilities:
1. **Genuine duplicate** — should be deleted, downstream code should use `moa_core::traits::SessionStore` directly. → S11 will fix.
2. **Intentional facade** with different signatures (e.g. wraps the core trait with orchestrator-specific concerns like spans, retry, idempotency tokens) — should be renamed to `SessionStoreFacade` or `OrchestratorSessionStore` to disambiguate. → S11 will rename.
3. **Vestigial** — a trait that was once a duplicate but has accumulated divergent methods that callers depend on. Most painful case; needs case-by-case judgment.

S11 will execute the resolution. This prompt's job is to figure out which case applies so S11 can be written confidently.

## Files in scope (read-only)

- `crates/moa-orchestrator/src/services/session_store.rs`
- `crates/moa-core/src/traits/session_store.rs` (or wherever the core trait lives)
- Every call site of either trait (`rg` to enumerate)

## Files explicitly out of scope

- All `.rs` files for *modification*. This prompt produces a markdown document only.

## Step-by-step instructions

1. **Locate both traits.** Find the trait definition in `moa-orchestrator/src/services/session_store.rs`. Find the trait definition in `moa-core` (likely `traits/session_store.rs` or `traits/mod.rs`).

2. **Side-by-side comparison.** Produce a table:
   ```
   | Method | moa_core::SessionStore | moa_orchestrator::services::SessionStore | Notes |
   |--------|------------------------|------------------------------------------|-------|
   | create | async fn create(...) -> Result<Id> | async fn create(...) -> Result<Id, MyErr> | error type differs |
   | ...    | ...                    | ...                                       | ...   |
   ```
   Mark each row as: `IDENTICAL`, `RENAMED`, `WIDENED` (orch has extra params), `NARROWED` (orch has fewer params), `MISSING` (in one but not other), `DIVERGED` (different semantics).

3. **Find all callers** of each trait:
   ```bash
   rg "moa_core::traits::SessionStore" crates/ -l
   rg "moa_orchestrator::services::SessionStore" crates/ -l  
   rg "use crate::services::SessionStore" crates/moa-orchestrator/
   ```
   Build a list of: which crate uses which trait, and whether any code uses both.

4. **Look for the impl pattern.** Specifically:
   - Is there a struct in `moa-orchestrator` that `impl`s **both** traits? (Strong signal of facade pattern.)
   - Is there a struct that implements only the orchestrator trait, with a body that *calls into* a `dyn moa_core::SessionStore` it holds? (Signal of decorator pattern.)
   - Or does the orchestrator trait have impls that don't use the core trait at all? (Signal of genuine duplicate that drifted.)

5. **Read the doc comments.** If there are `//!` or `///` comments explaining the orchestrator's trait, that's the original author's intent. Pay attention.

6. **Produce the decision document** at `struct-pack/S05-decision.md` (the prompt creates this file). Structure:

   ```markdown
   # S05 Decision Document
   
   ## Verdict
   
   One of:
   - DELETE: orchestrator's trait is a true duplicate; remove and migrate callers to moa_core
   - RENAME: orchestrator's trait is a real facade; rename to `OrchestratorSessionStore` (or `SessionStoreFacade`) for clarity
   - MERGE: orchestrator's trait has features that should be promoted into the core trait
   - LEAVE: more complex than expected; needs design discussion before any change
   
   ## Evidence
   
   - Trait definition comparison (the table from step 2)
   - Caller list (from step 3)
   - Impl pattern observed (from step 4)
   - Doc comments excerpted (from step 5)
   
   ## Recommendation for S11
   
   Concrete instructions for S11: which symbols to rename/delete/move, which call sites to update, what the resulting trait surface should look like.
   
   ## Risk assessment
   
   What could go wrong with the recommended action.
   ```

7. **Save the decision document** as `struct-pack/S05-decision.md`. The S11 prompt reads this file as input.

8. **Append a one-line summary** to `REFACTOR_NOTES.md` under `[S05]` so the verdict is searchable.

## Verification

This is a read-only prompt; verification is that the decision document exists and is complete:

```bash
test -f struct-pack/S05-decision.md
wc -l struct-pack/S05-decision.md  # should be at least 30 lines; less than that is suspicious
grep -E "^## Verdict|^## Evidence|^## Recommendation|^## Risk" struct-pack/S05-decision.md  # all four sections present
```

## Acceptance criteria

- [ ] `struct-pack/S05-decision.md` exists with all four sections populated.
- [ ] Verdict is one of: DELETE, RENAME, MERGE, LEAVE.
- [ ] Evidence section includes the method-comparison table.
- [ ] Caller list is complete (every file that uses either trait).
- [ ] Recommendation section gives concrete instructions usable by S11.
- [ ] No `.rs` files were modified in this prompt.
- [ ] `cargo check --workspace` is still green (it should be — no source changes).

## Rollback plan

N/A — read-only.

## Notes for the agent

- **This is an investigation, not a refactor.** Resist the urge to "just fix it while I'm in here." S11 owns the fix. This prompt produces the input S11 needs.
- **If the verdict is LEAVE**, that's a valid outcome. Some duplicates exist for good reasons (versioning, deprecation transitions). Better to leave it and document than to force a merge that's wrong.
- **If the orchestrator trait is *much* larger** (10+ methods that the core trait doesn't have), that's MERGE territory but probably not a single-prompt fix. Recommend "MERGE, but split S11 into S11a (rename for now) + a future S15 (merge methods into core when there's appetite)."
- **The orchestrator may have *multiple* traits in the file** named differently. The audit only flagged one. Skim for others while you're there; mention them in the doc but don't expand scope unless they're also duplicates.
- **Cargo dependency direction matters.** If `moa-core` is a leaf and `moa-orchestrator` depends on it, the resolution must not introduce a cycle. (It shouldn't — moving things *toward* core is safe; moving things *out* of core would be the cycle risk.)
- **Be honest in the verdict.** If the situation is genuinely murky, say LEAVE and explain why. The packs are designed to make uncertainty explicit, not to grind through every ambiguity.
