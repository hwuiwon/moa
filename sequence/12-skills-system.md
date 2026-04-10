# Step 12: Skills System

## What this step is about
Parsing Agent Skills format, maintaining a skill registry, auto-distilling skills from successful runs, and self-improving skills during use.

## Files to read
- `docs/09-skills-and-learning.md` — SKILL.md format, distillation logic, self-improvement, registry, lifecycle

## Goal
After a complex multi-step run (≥5 tool calls), the agent can distill the session into a reusable skill. On future runs, the pipeline injects relevant skill metadata. Skills self-improve when the agent finds a better approach.

## Tasks
1. **`moa-skills/src/format.rs`**: Parse SKILL.md (YAML frontmatter + markdown body). Validate required fields.
2. **`moa-skills/src/registry.rs`**: `SkillRegistry` — load skills from memory store, list for pipeline, load full body on demand.
3. **`moa-skills/src/distiller.rs`**: `maybe_distill_skill()` — called after successful runs. Generates SKILL.md from session events via LLM call.
4. **`moa-skills/src/improver.rs`**: `maybe_improve_skill()` — compares current execution with existing skill, updates if better.
5. **Update `moa-brain/src/pipeline/skills.rs`** (Stage 4): Load skill metadata from registry, inject into context.
6. **Update `moa-brain/src/harness.rs`**: After session completes, call `maybe_distill_skill()`.

## Deliverables
```
moa-skills/src/
├── lib.rs
├── format.rs
├── registry.rs
├── distiller.rs
└── improver.rs
```

## Acceptance criteria
1. Can parse a SKILL.md file with correct frontmatter extraction
2. Registry lists skills with metadata (name, tags, estimated_tokens)
3. After a 7-tool-call session, a new skill is created in workspace memory
4. Pipeline Stage 4 shows skill metadata in context
5. Using a skill and finding a better approach updates the skill

## Tests
- Unit test: Parse valid SKILL.md → correct fields
- Unit test: Parse invalid SKILL.md → appropriate error
- Integration test: Mock a 7-tool-call session → verify skill distilled to memory
- Integration test: Load skills into pipeline → verify they appear in context

```bash
cargo test -p moa-skills
```

---

