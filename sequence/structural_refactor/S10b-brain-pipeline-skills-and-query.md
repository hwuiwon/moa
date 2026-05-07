# S10b — Split `pipeline/skills.rs` (1,048 LOC) and `pipeline/query_rewrite.rs` (1,137 LOC)

## Scope

Split two pipeline-stage files in `moa-brain` that are each over 1,000 LOC. **No behavior changes.**

## Preconditions

- S10a complete and merged.
- `cargo check --workspace` is green.

## Why this prompt

`pipeline/skills.rs` (1,048 LOC) is the SkillInjector context-pipeline stage — it lists skills as metadata (Tier 1 of progressive disclosure), handles Tier 2 loading on activation, and integrates with the skill registry. `pipeline/query_rewrite.rs` (1,137 LOC) is a stage that rewrites the user's query before retrieval based on conversation context. Both are at the same level of the pipeline and follow similar shapes, so they share a prompt.

## Files in scope

- `crates/moa-brain/src/pipeline/skills.rs` → split to `pipeline/skills/`
- `crates/moa-brain/src/pipeline/query_rewrite.rs` → split to `pipeline/query_rewrite/`
- `crates/moa-brain/src/pipeline/mod.rs` → declarations stay, no other changes

## Files explicitly out of scope

- Other `pipeline/*.rs` files
- The skill registry itself (in `moa-skills`)
- `moa-brain/tests/`

## Step-by-step instructions

### Part A — `skills.rs`

1. Read end-to-end. Identify sections:
   - The `SkillInjector` struct + `impl ContextProcessor`
   - Skill metadata formatting (Tier 1: ~100 tokens/skill metadata into context)
   - Skill activation logic (when does the brain say "use this skill" and trigger Tier 2 load)
   - Skill body loading (Tier 2: full `SKILL.md` body)
   - Cache-breakpoint marking (skills are at Stage 4; this is where the stable prefix ends)

2. Target structure:
   ```
   pipeline/skills/
   ├── mod.rs              — SkillInjector + impl ContextProcessor
   ├── tier1_metadata.rs   — list/format skill metadata for stable prefix
   ├── tier2_loading.rs    — full skill body load on activation
   ├── activation.rs       — detect "this turn needs skill X" signals
   └── cache_break.rs      — cache-breakpoint marking specific to skills stage
   ```

3. Move types verbatim, same delegation pattern as S10a.

### Part B — `query_rewrite.rs`

4. Read end-to-end. Identify sections:
   - The `QueryRewriteProcessor` struct + impl
   - Trigger detection (when does the user's message warrant a rewrite?)
   - Rewrite prompt construction (the system prompt that asks the LLM "rewrite this user query as a search query")
   - LLM call wrapping (probably uses a small/cheap model, separate from the main provider)
   - Result post-processing (how the rewritten query gets attached to the working context)

5. Target structure:
   ```
   pipeline/query_rewrite/
   ├── mod.rs              — QueryRewriteProcessor + impl ContextProcessor
   ├── triggers.rs         — when to rewrite (heuristics, signals)
   ├── prompt.rs           — rewrite-prompt construction
   ├── llm_call.rs         — calling out to a small LLM for the rewrite
   └── postprocess.rs      — how the rewritten query attaches to context
   ```

6. Move types verbatim.

### Both

7. Inline tests follow the same per-concern moving rule as S10a.

8. Run verification.

9. Document anomalies in `REFACTOR_NOTES.md` under `[S10b]`.

## Verification

```bash
cargo check -p moa-brain --all-targets
cargo clippy -p moa-brain --all-targets -- -D warnings
cargo test -p moa-brain --no-run
cargo check --workspace --all-targets
```

## Acceptance criteria

- [ ] `crates/moa-brain/src/pipeline/skills.rs` no longer exists; replaced by folder.
- [ ] `crates/moa-brain/src/pipeline/query_rewrite.rs` no longer exists; replaced by folder.
- [ ] No file in either folder exceeds 600 LOC.
- [ ] Public surface unchanged.
- [ ] `cargo check --workspace --all-targets` passes.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes.

## Rollback plan

`git checkout -- crates/moa-brain/src/pipeline/skills{.rs,/} crates/moa-brain/src/pipeline/query_rewrite{.rs,/}` and `git clean -fd crates/moa-brain/src/pipeline/{skills,query_rewrite}`.

## Notes for the agent

- **`SkillInjector` marks the cache breakpoint.** Stage 4 (skills) is the last "stable" stage in the pipeline. The `mark_cache_breakpoint()` call must remain in the right place after the split — this is a load-bearing detail for KV-cache hit rate.
- **Query rewriter likely uses a separate provider** (cheap/fast model). That's a config option, not a structural concern. Don't refactor the provider selection.
- **Triggers may be coupled to specific user-message patterns.** Don't try to generalize. Move the heuristics verbatim.
- **Time budget**: 1 session for both files together.
- **Anti-pattern**: do not unify the two stages even though they share shape. They have different inputs, different downstream consumers, and different tuning. Keep them as separate folders.
