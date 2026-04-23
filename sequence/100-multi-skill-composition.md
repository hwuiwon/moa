# 100 — Multi-Skill Composition with Budget-Aware Manifest

## Purpose

Upgrade the `SkillInjector` pipeline stage to support 20+ skills with correct multi-skill composition. Currently the injector loads all skill metadata into the system prompt. This works at small scale but breaks cache hit rates and overflows context budgets at 20+ skills. This prompt implements: budgeted skill manifest emission, stable alphabetical sorting for cache preservation, a meta-instruction for conflict resolution when multiple skills match, and a `load_skill` tool call pattern for progressive disclosure.

End state: SkillInjector emits a budget-capped, alphabetically-sorted skill manifest. Skills are selected by a relevance-weighted ranking but emitted in deterministic order for cache stability. Multiple matching skills are composed by the LLM at runtime.

## Prerequisites

- Sequence 84 (`static-prefix-relocation`) complete — cache breakpoint placement is correct.
- `moa-brain/src/pipeline/skills.rs` exists and is functional.
- `moa-skills/src/registry.rs` and `moa-skills/src/format.rs` are working.
- `moa-hands/src/tools/` has a `memory_read` tool that can load full skill bodies.

## Read before starting

```
cat moa-brain/src/pipeline/skills.rs
cat moa-brain/src/pipeline/mod.rs
cat moa-skills/src/registry.rs
cat moa-skills/src/format.rs
cat moa-brain/src/pipeline/cache.rs
```

## Architecture

### Current behavior

The `SkillInjector` loads skill summaries from memory, formats them as a list in a `<available_skills>` block, and appends to the system prompt. No budget cap, no sorting guarantee, no conflict guidance.

### Target behavior

1. **Budget cap**: Total skill manifest capped at `max(context_window * 0.01, 8000)` characters. Per-skill entry capped at 1,536 characters.
2. **Stable alphabetical sort**: Skills emitted alphabetically by name, never by relevance score. This ensures identical prefix across turns → cache hits.
3. **Overflow handling**: When total exceeds budget, emit top-N that fit. Selection priority: `0.3 * keyword_overlap(query, description) + 0.5 * use_count_normalized + 0.2 * recency_score`. But the *emitted* set is always re-sorted alphabetically.
4. **Conflict meta-instruction**: Prepend to skill manifest: "When multiple skills apply, prefer the one whose trigger conditions most specifically match the current task. Skills can be composed — use multiple if the task requires steps from different skills."
5. **Progressive disclosure**: Full skill bodies are NOT in the system prompt. The brain loads them via `memory_read` tool calls. The manifest only contains: name (≤64 chars), one-liner description (≤256 chars), tags, `estimated_tokens`.

### Skill manifest format

```
<available_skills>
When multiple skills apply, prefer the one whose trigger conditions most
specifically match the current task. Skills can be composed — use multiple
if the task requires steps from different skills.
To activate a skill, call memory_read with the skill path.

- auth-jwt-refresh: Handle JWT refresh token rotation flows [tags: auth, security] (est. 1200 tok)
- deploy-to-fly: Deploy applications to Fly.io staging/production [tags: deployment, fly] (est. 800 tok)
- migrate-postgres: Run and verify Postgres schema migrations [tags: database, migration] (est: 1500 tok)
</available_skills>
```

## Steps

### 1. Add skill budget config to `MoaConfig`

In `moa-core/src/config.rs`, add to the pipeline config section:

```rust
pub struct SkillBudgetConfig {
    /// Maximum characters for the entire skill manifest.
    /// Default: max(context_window * 0.01, 8000)
    pub max_manifest_chars: Option<usize>,
    /// Maximum characters per individual skill entry.
    pub max_per_skill_chars: usize,  // default: 1536
    /// Whether to include estimated token counts in manifest entries.
    pub show_token_estimates: bool,  // default: true
}
```

### 2. Add scoring infrastructure to `SkillInjector`

In `moa-brain/src/pipeline/skills.rs`, add a `SkillRanker` that computes a score for each skill given the current query context:

```rust
struct RankedSkill {
    metadata: SkillMetadata,
    score: f64,
    manifest_entry: String,  // pre-formatted, truncated to per-skill cap
}

fn rank_skills(
    skills: &[SkillMetadata],
    query_keywords: &[String],
) -> Vec<RankedSkill> {
    // Score = 0.3 * keyword_overlap + 0.5 * normalized_use_count + 0.2 * recency
    // Sort by score descending for selection
    // Then re-sort selected set alphabetically by name for emission
}
```

### 3. Implement budget-aware emission in `SkillInjector::process`

Rewrite the `process` method:

```rust
async fn process(&self, ctx: &mut WorkingContext) -> Result<ProcessorOutput> {
    let skills = self.load_skill_metadata(ctx).await?;
    if skills.is_empty() {
        return Ok(ProcessorOutput::default());
    }

    let budget = self.compute_budget(ctx.model_capabilities.context_window);
    let query_keywords = extract_keywords_from_pending(ctx);
    let ranked = rank_skills(&skills, &query_keywords);

    // Select top-N that fit within budget
    let mut selected = Vec::new();
    let mut chars_used = CONFLICT_META_INSTRUCTION.len() + MANIFEST_HEADER.len();
    for skill in &ranked {
        if chars_used + skill.manifest_entry.len() > budget.max_manifest_chars {
            break;
        }
        chars_used += skill.manifest_entry.len();
        selected.push(skill);
    }

    // Re-sort alphabetically for cache stability
    selected.sort_by(|a, b| a.metadata.name.cmp(&b.metadata.name));

    // Format and emit
    let manifest = format_skill_manifest(&selected);
    ctx.append_system(manifest);

    Ok(ProcessorOutput {
        tokens_added: estimate_tokens_for_chars(chars_used),
        items_included: selected.iter().map(|s| s.metadata.name.clone()).collect(),
        items_excluded: ranked.iter()
            .filter(|r| !selected.iter().any(|s| s.metadata.name == r.metadata.name))
            .map(|r| r.metadata.name.clone()).collect(),
        ..Default::default()
    })
}
```

### 4. Add `ProcessorOutput` fields for skill overflow tracking

The `ProcessorOutput` should report `items_excluded` with reasons so observability can track when skills are being dropped. If skills are being consistently dropped, it's a signal to either increase the budget or add category-layer filtering.

### 5. Update eval setup to pass skill budget config

In `moa-eval/src/setup.rs`, ensure `build_pipeline` passes the skill budget config from `MoaConfig` to the `SkillInjector` constructor.

### 6. Tests

- Unit: 5 skills, all fit within budget → all emitted alphabetically
- Unit: 30 skills, budget allows 15 → top-15 by score emitted, re-sorted alphabetically
- Unit: skill with description > 1536 chars → truncated with "..." suffix
- Unit: identical query on two calls → identical manifest output (cache stability)
- Unit: different query → same set selected (if scores don't change) → still identical output
- Unit: empty skills → no `<available_skills>` block emitted
- Integration: pipeline produces correct cache breakpoint with skills manifest before it

## Files to create or modify

- `moa-core/src/config.rs` — add `SkillBudgetConfig`
- `moa-brain/src/pipeline/skills.rs` — rewrite with budget, ranking, sorting
- `moa-eval/src/setup.rs` — pass skill budget config
- `moa-brain/src/pipeline/mod.rs` — if `SkillInjector` constructor changes

## Acceptance criteria

- [ ] `cargo build` succeeds.
- [ ] With 30 skills configured, manifest stays within budget.
- [ ] Manifest entries are always alphabetically sorted regardless of ranking scores.
- [ ] Two consecutive pipeline runs with different user messages produce identical skill manifest (cache stability).
- [ ] `ProcessorOutput` reports both included and excluded skills.
- [ ] Eval tests still pass (`cargo test -p moa-eval`).
- [ ] Per-skill entries truncated at 1,536 chars.

## Notes

- **Do NOT sort by relevance in the emitted output.** Relevance determines *selection* (which skills make the cut), but the emitted order must be alphabetical for cache prefix stability. This is the most important invariant.
- **Do NOT put full skill bodies in the system prompt.** Bodies load via `memory_read` tool calls. The manifest is metadata only.
- For future: if skill count exceeds ~200, add a category layer (per AnyTool paper). Defer until we actually have that many skills.