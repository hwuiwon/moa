# Step 93 — Tiered Model Routing

_Route LLM calls to different models based on task complexity. Frontier model (Sonnet/Opus) for the main agent loop; cheaper model (Haiku) for compaction summarization, memory consolidation, skill distillation, and subagent tasks. Step 88's Tier 3 compactor already has a `summarizer_model` config field — this step generalizes that into a system-wide `ModelRouter`._

---

## 1. What this step is about

Step 88's compactor calls the LLM for Tier 3 summarization. It hardcodes `summarizer_model` from config. But three other subsystems also call LLMs for auxiliary work: memory consolidation (step 13), skill distillation (step 12), and future subagent tasks. Each of these should use a cheaper model.

Claude Sonnet 4.6 costs $3/$15 per MTok (input/output). Claude Haiku 4.5 costs $0.80/$4. For summarization and consolidation — tasks where output quality doesn't need frontier reasoning — Haiku saves 73% on input and 73% on output tokens.

---

## 2. Files to read

- `moa-providers/src/factory.rs` — provider/model creation. This is where the router hooks in.
- `moa-providers/src/models.rs` — model definitions and capabilities.
- `moa-brain/src/pipeline/compactor.rs` — existing `summarizer_model` field.
- `moa-memory/src/consolidation.rs` — memory consolidation LLM calls.
- `moa-skills/src/distiller.rs` (or equivalent) — skill distillation.
- `moa-core/src/config.rs` — model config.

---

## 3. Goal

1. A `ModelRouter` in `moa-providers` maps task types to model configurations.
2. Task types: `MainLoop`, `Summarization`, `Consolidation`, `SkillDistillation`, `Subagent`.
3. Config:
   ```toml
   [models]
   main = "claude-sonnet-4-20250514"
   auxiliary = "claude-haiku-4-5-20251001"
   # auxiliary is used for Summarization, Consolidation, SkillDistillation, Subagent
   ```
4. `ModelRouter::provider_for(task: ModelTask) -> Arc<dyn LLMProvider>` returns the correct provider instance.
5. Step 88's `summarizer_model` config is replaced by `models.auxiliary`.
6. Memory consolidation and skill distillation calls route through `ModelRouter` instead of using the main provider.

---

## 4. Rules

- **Two tiers is enough.** Don't build a 5-tier routing matrix. Main vs auxiliary covers all current needs. A third tier (e.g., for embeddings) already exists in `moa-providers/src/embedding.rs`.
- **Same provider, different model.** Both tiers typically use the same API provider (e.g., Anthropic). The `ModelRouter` creates two `LLMProvider` instances pointing at different model IDs, sharing the same HTTP client and API key.
- **Fallback to main.** If `models.auxiliary` is not configured, all tasks use `models.main`. No failure.
- **Cost tracking per tier.** Step 92's analytic views should be able to distinguish main-loop costs from auxiliary costs. Add a `model_tier` attribute to `Event::BrainResponse` (or equivalent).

---

## 5. Tasks

### 5a. Define `ModelTask` enum

```rust
// moa-core/src/types/provider.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ModelTask {
    MainLoop,
    Summarization,
    Consolidation,
    SkillDistillation,
    Subagent,
}
```

### 5b. `ModelRouter` implementation

```rust
// moa-providers/src/router.rs
pub struct ModelRouter {
    main: Arc<dyn LLMProvider>,
    auxiliary: Option<Arc<dyn LLMProvider>>,
}

impl ModelRouter {
    pub fn from_config(config: &MoaConfig) -> Result<Self> { ... }

    pub fn provider_for(&self, task: ModelTask) -> Arc<dyn LLMProvider> {
        match task {
            ModelTask::MainLoop => self.main.clone(),
            _ => self.auxiliary.as_ref().unwrap_or(&self.main).clone(),
        }
    }
}
```

### 5c. Wire into consumers

- `moa-brain/src/pipeline/compactor.rs`: replace `self.llm` with `self.model_router.provider_for(ModelTask::Summarization)`.
- `moa-memory/src/consolidation.rs`: use `ModelTask::Consolidation`.
- `moa-skills/src/distiller.rs`: use `ModelTask::SkillDistillation`.
- `moa-orchestrator/src/local.rs`: pass `ModelRouter` instead of a single `Arc<dyn LLMProvider>` to subsystems that need both tiers.

### 5d. Add `model_tier` to events

Extend `Event::BrainResponse` with `model_tier: String` (values: `"main"`, `"auxiliary"`). Step 92's `tool_call_analytics` and `session_summary` views can filter/aggregate by tier.

### 5e. Tests

- Unit: `ModelRouter` with auxiliary configured returns different providers for `MainLoop` vs `Summarization`.
- Unit: `ModelRouter` with no auxiliary returns main for all tasks.
- Integration: run a session that triggers Tier 3 compaction; assert the compaction LLM call uses the auxiliary model (check `Event::BrainResponse.model` field).

---

## 6. Deliverables

- [ ] `ModelTask` enum in `moa-core`.
- [ ] `ModelRouter` in `moa-providers/src/router.rs`.
- [ ] `[models]` config section with `main` and `auxiliary`.
- [ ] Compactor, consolidation, and distillation wired through `ModelRouter`.
- [ ] `model_tier` on `Event::BrainResponse`.
- [ ] Tests.

---

## 7. Acceptance criteria

1. A session that triggers Tier 3 compaction uses the auxiliary model for the summarization call and the main model for the user-facing turns.
2. Config without `models.auxiliary` still works (all calls use `models.main`).
3. Step 92's `session_summary` view can aggregate costs by `model_tier`.
4. `cargo test --workspace` green.
